#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

pub const ENTRY_POINT: u32 = 0;

use merkle_trees::MerkleTreeCapVarLength;
use prover::cs::definitions::TimestampScalar;
use prover::cs::utils::split_timestamp;
use prover::tracers::delegation::DelegationWitness;
use prover::tracers::main_cycle_optimized::CycleData;
use prover::tracers::oracles::delegation_oracle::DelegationCircuitOracle;
use prover::tracers::oracles::main_risc_v_circuit::MainRiscVOracle;
use setups::prover::definitions::OPTIMAL_FOLDING_PROPERTIES;
use setups::prover::fft::*;
use setups::prover::field::*;
use setups::prover::merkle_trees::DefaultTreeConstructor;
use setups::prover::merkle_trees::MerkleTreeConstructor;
use setups::prover::risc_v_simulator::abstractions::non_determinism::*;
use setups::prover::risc_v_simulator::cycle::MachineConfig;
use setups::prover::transcript::Seed;
use setups::prover::*;
use std::collections::HashMap;
use worker::Worker;

pub use setups;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FinalRegisterValue {
    /// 程序结束时该寄存器的值
    pub value: u32,
    /// 该寄存器最后一次被访问的timestamp
    pub last_access_timestamp: TimestampScalar,
}

/// 1. 创建VM memory
/// 2. 把bytecode写进ROM区域
/// 3. 创建RISC-V VM初始状态
/// 4. 创建GPUFriendlyTracer
/// 5. 按chunk调用state.run_cycles
/// 6. 收集CycleData、最终寄存器状态、RAM touched words、delegation witnesses
pub fn run_till_end_for_gpu_for_machine_config<
    ND: NonDeterminismCSRSource<VectorMemoryImplWithRom>,
    C: MachineConfig,
    A: GoodAllocator,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    num_cycles_upper_bound: usize,
    trace_size: usize,
    binary: &[u32],
    non_determinism: &mut ND,
    delegation_factories: HashMap<u16, Box<dyn Fn() -> DelegationWitness<A>>>,
    worker: &Worker,
) -> (
    u32,                                     //final_pc
    Vec<CycleData<C, A>>,                    //main RISC-V每个chunk的cycle记录
    HashMap<u16, Vec<DelegationWitness<A>>>, //每种delegation type对应的delegation witness列表
    Vec<FinalRegisterValue>,                 //32个寄存器最终值和最后访问timestamp
    Vec<Vec<(u32, (TimestampScalar, u32))>>, // 被访问过的RAM word，记录地址、最后访问timestamp、最终值 lazy iniy/teardown data - all unique words touched, sorted ascending, but not in one vector
) {
    use crate::cs::one_row_compiler::timestamp_from_chunk_cycle_and_sequence;
    use prover::tracers::main_cycle_optimized::DelegationTracingData;
    use prover::tracers::main_cycle_optimized::GPUFriendlyTracer;
    use prover::tracers::main_cycle_optimized::RamTracingData;
    use setups::prover::risc_v_simulator::cycle::state_new::RiscV32StateForUnrolledProver;
    use setups::prover::risc_v_simulator::delegations::DelegationsCSRProcessor;

    // STARK trace的基本形状要求，FFT/LDE/FRI通常要求domain size是2的幂。Airbender后续会把trace当作多项式在大小为trace_size的domain上插值和扩展，所以这里先保证形状合法。
    assert!(trace_size.is_power_of_two());
    // 2MB
    let rom_address_space_bound = 1usize << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS);

    // 创建一块1GB VM memory；其中前rom_address_space_bound字节被视为ROM区域。源码注释也写了use 1 GB RAM。
    let mut memory = VectorMemoryImplWithRom::new_for_byte_size(1 << 30, rom_address_space_bound); // use 1 GB RAM
    for (idx, insn) in binary.iter().enumerate() {
        // 模拟器运行用的内存镜像 VectorMemoryImplWithRom
        // 把binary写入ROM
        memory.populate(ENTRY_POINT + idx as u32 * 4, *insn);
    }

    let cycles_per_chunk = trace_size - 1;
    let num_cycles_upper_bound = num_cycles_upper_bound.next_multiple_of(cycles_per_chunk);
    let num_circuits_upper_bound = num_cycles_upper_bound / cycles_per_chunk;

    // 创建初始VM状态，所有寄存器 = 0，pc = ENTRY_POINT = 0
    let mut state = RiscV32StateForUnrolledProver::<C>::initial(ENTRY_POINT);

    // 创建“访问历史记账本”，记录每个寄存器/RAM word最后一次被访问的timestamp，保存哪些RAM word被访问过
    // 所有寄存器 last timestamp = 0
    // 所有RAM word last timestamp = 0
    // 所有RAM touched bit = 0
    let bookkeeping_aux_data =
        RamTracingData::<true>::new_for_ram_size_and_rom_bound(1 << 30, rom_address_space_bound); // use 1 GB RAM
    let delegation_tracer = DelegationTracingData {
        all_per_type_logs: HashMap::new(),
        delegation_witness_factories: delegation_factories,
        current_per_type_logs: HashMap::new(),
        num_traced_registers: 0,
        mem_reads_offset: 0,
        mem_writes_offset: 0,
    };

    // important - in our memory implementation first access in every chunk is timestamped as (trace_size * circuit_idx) + 4,
    // so we take care of it

    // CSR是RISC-V的control and status register。Airbender里某些CSR写入会触发delegation。
    // 如果guest写入某个特殊CSR，VM层会真正执行对应delegation逻辑，同时tracer记录delegation witness。
    let mut custom_csr_processor = DelegationsCSRProcessor;

    let initial_ts = timestamp_from_chunk_cycle_and_sequence(0, cycles_per_chunk, 0);
    // 举例：
    // trace_size = 8
    // cycles_per_chunk = 7
    // GPUFriendlyTracer:
    //   chunk_size = 7
    //   trace_chunk = CycleData capacity 8
    //   num_cycles_chunk_size = 7
    //   current_timestamp = 4
    let mut tracer = GPUFriendlyTracer::<_, _, true, true, true>::new(
        initial_ts,
        bookkeeping_aux_data,
        delegation_tracer,
        cycles_per_chunk,
        num_circuits_upper_bound,
    );

    let mut end_reached = false; //程序是否执行到了结束条件
    let mut circuits_needed = 0; //实际用了多少个main circuit chunk

    let now = std::time::Instant::now();

    for chunk_idx in 0..num_circuits_upper_bound {
        circuits_needed = chunk_idx + 1;
        //         为什么chunk0不调用prepare_for_next_chunk？
        // 因为tracer刚new出来时已经创建了第一个trace_chunk，并且current_timestamp已经是chunk0起始timestamp。
        // 从chunk1开始，才要把上一块trace_chunk存入traced_chunks，并创建新的trace_chunk。
        if chunk_idx != 0 {
            let timestamp = timestamp_from_chunk_cycle_and_sequence(0, cycles_per_chunk, chunk_idx);
            tracer.prepare_for_next_chunk(timestamp);
        }

        // 真正执行guest
        let finished = state.run_cycles(
            &mut memory,               //VM执行时从这里取指、load、store
            &mut tracer, //VM每次读寄存器、读RAM、写RAM、读non-determinism，都通过tracer记录
            non_determinism, //外部输入源
            &mut custom_csr_processor, //处理特殊CSR和delegation
            cycles_per_chunk, //这一块最多执行多少个cycle
        );

        if finished {
            println!("Ended at address 0x{:08x}", state.observable.pc);
            println!("Took {} circuits to finish execution", circuits_needed);
            end_reached = true;
            break;
        };
    }

    // 如果程序在上限内没结束，就panic。这和前面的num_instances_upper_bound有关。
    // 举例：
    // trace_size = 8
    // cycles_per_chunk = 7
    // num_circuits_upper_bound = 2
    // 最多跑14 cycles。
    // 如果程序第20个cycle才结束，那么这里：end_reached = false
    // 所以外部调用者必须给足num_instances_upper_bound。
    assert!(end_reached, "end of the execution was never reached");
    // GPUFriendlyTracer会记录这一行
    let GPUFriendlyTracer {
        bookkeeping_aux_data,
        trace_chunk,
        traced_chunks,
        delegation_tracer,
        ..
    } = tracer;

    // put latest chunk manually in traced ones
    // 为什么要手动push最后一个chunk？
    // 因为prepare_for_next_chunk只在进入下一个chunk前调用。最后一个chunk执行完后，没有“下一个chunk”触发prepare_for_next_chunk，所以要手动把当前trace_chunk放进traced_chunks。
    let mut traced_chunks = traced_chunks;
    traced_chunks.push(trace_chunk);
    assert_eq!(traced_chunks.len(), circuits_needed);

    let elapsed = now.elapsed();
    let cycles_upper_bound = circuits_needed * cycles_per_chunk;
    let speed = (cycles_upper_bound as f64) / elapsed.as_secs_f64() / 1_000_000f64;
    println!(
        "Simulator running speed with witness tracing is {} MHz: ran {} cycles over {:?}",
        speed, cycles_upper_bound, elapsed
    );

    // 把tracer里的memory bookkeeping拆出来。
    let RamTracingData {
        register_last_live_timestamps,
        ram_words_last_live_timestamps,
        access_bitmask,
        ..
    } = bookkeeping_aux_data;

    // now we can co-join touched memory cells, their final values and timestamps

    // get_final_ram_state会把ROM区域清零，只保留RAM最终状态。
    let memory_final_state = memory.get_final_ram_state(); // 最终RAM内容
    let memory_state_ref = &memory_final_state;
    let ram_words_last_live_timestamps_ref = &ram_words_last_live_timestamps; //每个RAM word最后一次访问timestamp

    // parallel collect
    // first we will walk over access_bitmask and collect subparts

    // 把“运行过程中被访问过的RAM word”找出来，并收集它们的最终状态：物理地址、最后访问timestamp、最终value
    // 这些数据后面用于lazy init / teardown。
    // 证明系统需要知道某个RAM word一开始是什么状态，执行结束后是什么状态，最后一次访问发生在什么时候。
    let mut chunks: Vec<Vec<(u32, (TimestampScalar, u32))>> =
        vec![vec![].clone(); worker.get_num_cores()];
    // 为什么是Vec<Vec<...>>？
    // 因为这段代码并行扫描access_bitmask。每个线程负责一部分bitmask。为了避免多个线程同时push同一个Vec导致锁竞争，所以给每个线程一个独立Vec。

    // 把chunks这个Vec切成一个可变slice。
    let mut dst = &mut chunks[..];
    // 把access_bitmask.len()这么多工作量，按照worker的线程数切分。
    // 为什么要并行: RAM很大。access_bitmask也可能很大。扫描所有bit是一个纯CPU任务
    worker.scope(access_bitmask.len(), |scope, geometry| {
        // 你从哪里开始，处理多少个元素
        for thread_idx in 0..geometry.len() {
            // 这里的chunk不是VM trace chunk。它只是“并行任务切片”。
            let chunk_size = geometry.get_chunk_size(thread_idx); //给当前线程的起点
            let chunk_start = geometry.get_chunk_start_pos(thread_idx); //给当前线程的长度，源码用它们构造range
            let range = chunk_start..(chunk_start + chunk_size); // 这个线程负责扫描access_bitmask[range]

            // 从dst里切出第一个Vec，给当前线程用。剩下的继续留给后面的线程。split_at_mut是Rust里安全地把一个可变slice切成互不重叠两部分的标准方式。
            // 安全地从dst里拿出当前线程专用的一个Vec：
            // el[0]: 当前线程写入的Vec
            // rest: 剩下线程将要使用的Vec列表
            let (el, rest) = dst.split_at_mut(1);
            dst = rest;
            let src = &access_bitmask[range];

            // access_bitmask记录哪些RAM word被访问过。
            // 如果某个bit是1，就说明对应word被访问过。
            // 收集这个word的物理地址、最后访问timestamp、最终value
            // 这就是后面chunk_lazy_init_and_teardown要处理的原始数据。
            Worker::smart_spawn(scope, thread_idx == geometry.len() - 1, move |_| {
                // 所以el[0]才是当前线程真正要写入的那个Vec。
                // 可以理解为el = 当前线程的输出Vec
                let el = &mut el[0];
                // 这里的word是access_bitmask里的一个usize。
                // 一个usize包含很多bit。每个bit代表一个RAM word是否被访问过。
                // 假设64位机器：word = access_bitmask[k]
                // 它包含：
                // bit0  -> RAM word k*64 + 0
                // bit1  -> RAM word k*64 + 1
                // bit2  -> RAM word k*64 + 2
                // ...
                // bit63 -> RAM word k*64 + 63
                for (idx, word) in src.iter().enumerate() {
                    for bit_idx in 0..usize::BITS {
                        // 算当前bit对应的是第几个RAM word。
                        // chunk_start + idx: 当前正在看的access_bitmask全局下标
                        // * usize::BITS: 每个access_bitmask元素管理多少个RAM word
                        // + bit_idx: 当前bit对应其中第几个RAM word
                        let word_idx =
                            (chunk_start + idx) * (usize::BITS as usize) + (bit_idx as usize);
                        let phys_address = word_idx << 2; //因为一个word是4字节

                        // 检查当前bit是不是1是1，表示对应RAM word被访问过。是0，就跳过。
                        let word_is_used = *word & (1 << bit_idx) > 0;
                        if word_is_used {
                            let word_value = memory_state_ref[word_idx];
                            let last_timestamp: TimestampScalar =
                                ram_words_last_live_timestamps_ref[word_idx];
                            // 这个RAM word的物理字节地址，这个RAM word最后一次被访问的timestamp，这个RAM word执行结束后的最终值
                            el.push((phys_address as u32, (last_timestamp, word_value)));
                            //如果RAM[1008]被访问过，最终值77，最后timestamp25，那么push：(1008, (25, 77))
                        }
                    }
                }
            });
        }
    });

    // 这里为什么只收集被访问过的RAM word？因为后面的lazy init / teardown只需要处理被访问过的内存单元。

    let mut registers_final_states = Vec::with_capacity(32);
    // 生成32个FinalRegisterValue
    for register_idx in 0..32 {
        let last_timestamp = register_last_live_timestamps[register_idx];
        let register_state = FinalRegisterValue {
            value: state.observable.registers[register_idx],
            last_access_timestamp: last_timestamp,
        };
        registers_final_states.push(register_state);
    }

    let DelegationTracingData {
        all_per_type_logs,
        current_per_type_logs,
        ..
    } = delegation_tracer;

    // 为什么会有“当前还没flush”？
    // 因为delegation witness通常有容量限制。某种delegation请求收集满以后，会把当前witness移到all_per_type_logs，再创建一个新的current witness。程序结束时，最后一个current witness可能没满，但里面已经有有效数据，所以要手动放进all_per_type_logs。
    //     把当前还没flush的delegation witness也放入all_per_type_logs。
    // 空的delegation witness不放。
    let mut all_per_type_logs = all_per_type_logs;
    for (delegation_type, current_data) in current_per_type_logs.into_iter() {
        // We decide whether we do or not do delegation by comparing length, so we do NOT pad here.
        // GPU also benefits from little less transfer, and pads for another convantion by itself

        // let mut current_data = current_data;
        // current_data.pad();

        if current_data.is_empty() == false {
            all_per_type_logs
                .entry(delegation_type)
                .or_insert(vec![])
                .push(current_data);
        }
    }

    assert_eq!(circuits_needed, traced_chunks.len());

    (
        state.observable.pc,
        traced_chunks,
        all_per_type_logs,
        registers_final_states,
        chunks,
    )
}

pub fn run_till_end_for_machine_config_without_tracing<
    ND: NonDeterminismCSRSource<VectorMemoryImplWithRom>,
    C: MachineConfig,
    A: GoodAllocator,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    num_cycles_upper_bound: usize,
    trace_size: usize,
    binary: &[u32],
    non_determinism: &mut ND,
) -> (u32, [u32; 32]) {
    use setups::prover::risc_v_simulator::cycle::state_new::RiscV32StateForUnrolledProver;
    use setups::prover::risc_v_simulator::delegations::DelegationsCSRProcessor;

    assert!(trace_size.is_power_of_two());
    let rom_address_space_bound = 1usize << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS);

    let mut memory = VectorMemoryImplWithRom::new_for_byte_size(1 << 30, rom_address_space_bound); // use 1 GB RAM
    for (idx, insn) in binary.iter().enumerate() {
        memory.populate(ENTRY_POINT + idx as u32 * 4, *insn);
    }

    let cycles_per_chunk = trace_size - 1;
    let num_cycles_upper_bound = num_cycles_upper_bound.next_multiple_of(cycles_per_chunk);
    let num_circuits_upper_bound = num_cycles_upper_bound / cycles_per_chunk;

    let mut state = RiscV32StateForUnrolledProver::<C>::initial(ENTRY_POINT);

    let num_cycles_in_chunk = trace_size - 1;
    // important - in our memory implementation first access in every chunk is timestamped as (trace_size * circuit_idx) + 4,
    // so we take care of it

    let mut custom_csr_processor = DelegationsCSRProcessor;

    let mut end_reached = false;
    let mut circuits_needed = 0;

    let now = std::time::Instant::now();

    for chunk_idx in 0..num_circuits_upper_bound {
        circuits_needed = chunk_idx + 1;

        let finished = state.run_cycles(
            &mut memory,
            &mut (),
            non_determinism,
            &mut custom_csr_processor,
            num_cycles_in_chunk,
        );

        if finished {
            println!("Ended at address 0x{:08x}", state.observable.pc);
            println!("Took {} circuits to finish execution", circuits_needed);
            end_reached = true;
            break;
        };
    }

    assert!(end_reached, "end of the execution was never reached");

    let elapsed = now.elapsed();
    let cycles_upper_bound = circuits_needed * num_cycles_in_chunk;
    let speed = (cycles_upper_bound as f64) / elapsed.as_secs_f64() / 1_000_000f64;
    println!(
        "Simulator running speed without witness tracing is {} MHz: ran {} cycles over {:?}",
        speed, cycles_upper_bound, elapsed
    );

    (state.observable.pc, state.observable.registers)
}

pub fn commit_memory_tree_for_riscv_circuit_using_gpu_tracer<C: MachineConfig, A: GoodAllocator>(
    compiled_machine: &setups::prover::cs::one_row_compiler::CompiledCircuitArtifact<
        Mersenne31Field,
    >,
    witness_chunk: &CycleData<C>,
    inits_and_teardowns: &ShuffleRamSetupAndTeardown,
    _circuit_sequence: usize,
    twiddles: &Twiddles<Mersenne31Complex, A>,
    lde_precomputations: &LdePrecomputations<A>,
    worker: &Worker,
) -> (Vec<MerkleTreeCapVarLength>, WitnessEvaluationAuxData) {
    let lde_factor = lde_precomputations.lde_factor;

    use setups::prover::prover_stages::stage1::compute_wide_ldes;
    let trace_len = witness_chunk.num_cycles_chunk_size + 1;
    assert!(trace_len.is_power_of_two());

    let optimal_folding = OPTIMAL_FOLDING_PROPERTIES[trace_len.trailing_zeros() as usize];

    let num_cycles_in_chunk = trace_len - 1;
    let now = std::time::Instant::now();

    let oracle = MainRiscVOracle {
        cycle_data: witness_chunk,
    };

    let memory_chunk = evaluate_memory_witness(
        compiled_machine,
        num_cycles_in_chunk,
        &oracle,
        &inits_and_teardowns.lazy_init_data,
        &worker,
        A::default(),
    );
    println!(
        "Materializing memory trace for {} cycles took {:?}",
        num_cycles_in_chunk,
        now.elapsed()
    );

    let MemoryOnlyWitnessEvaluationData {
        aux_data,
        memory_trace,
    } = memory_chunk;
    // now we should commit to it
    let width = memory_trace.width();
    let mut memory_trace = memory_trace;
    adjust_to_zero_c0_var_length(&mut memory_trace, 0..width, worker);

    let memory_ldes = compute_wide_ldes(
        memory_trace,
        twiddles,
        lde_precomputations,
        0,
        lde_factor,
        worker,
    );
    assert_eq!(memory_ldes.len(), lde_factor);

    // now form a tree
    let subtree_cap_size = (1 << optimal_folding.total_caps_size_log2) / lde_factor;
    assert!(subtree_cap_size > 0);

    let mut memory_subtrees = Vec::with_capacity(lde_factor);
    let now = std::time::Instant::now();
    for domain in memory_ldes.iter() {
        let memory_tree = DefaultTreeConstructor::construct_for_coset(
            &domain.trace,
            subtree_cap_size,
            true,
            worker,
        );
        memory_subtrees.push(memory_tree);
    }

    let dump_fn = |caps: &[DefaultTreeConstructor]| {
        let mut result = Vec::with_capacity(caps.len());
        for el in caps.iter() {
            result.push(el.get_cap());
        }

        result
    };

    let caps = dump_fn(&memory_subtrees);
    println!("Memory witness commitment took {:?}", now.elapsed());

    (caps, aux_data)
}

pub fn commit_memory_tree_for_delegation_circuit_with_gpu_tracer<A: GoodAllocator>(
    compiled_machine: &setups::prover::cs::one_row_compiler::CompiledCircuitArtifact<
        Mersenne31Field,
    >,
    witness_chunk: &DelegationWitness,
    twiddles: &Twiddles<Mersenne31Complex, A>,
    lde_precomputations: &LdePrecomputations<A>,
    lde_factor: usize,
    _tree_cap_size: usize,
    worker: &Worker,
) -> (Vec<MerkleTreeCapVarLength>, u32) {
    use setups::prover::prover_stages::stage1::compute_wide_ldes;

    let trace_len = witness_chunk.num_requests + 1;

    assert!(trace_len.is_power_of_two());
    let optimal_folding = OPTIMAL_FOLDING_PROPERTIES[trace_len.trailing_zeros() as usize];

    let num_cycles_in_chunk = trace_len - 1;
    let now = std::time::Instant::now();
    let oracle = DelegationCircuitOracle {
        cycle_data: witness_chunk,
    };
    let memory_chunk = evaluate_delegation_memory_witness(
        compiled_machine,
        num_cycles_in_chunk,
        &oracle,
        &worker,
        A::default(),
    );
    println!(
        "Materializing delegation type {} memory trace for {} cycles took {:?}",
        witness_chunk.delegation_type,
        num_cycles_in_chunk,
        now.elapsed()
    );

    let DelegationMemoryOnlyWitnessEvaluationData { memory_trace } = memory_chunk;
    // now we should commit to it
    let width = memory_trace.width();
    let mut memory_trace = memory_trace;
    adjust_to_zero_c0_var_length(&mut memory_trace, 0..width, worker);

    let memory_ldes = compute_wide_ldes(
        memory_trace,
        twiddles,
        lde_precomputations,
        0,
        lde_factor,
        worker,
    );
    assert_eq!(memory_ldes.len(), lde_factor);

    // now form a tree
    let subtree_cap_size = (1 << optimal_folding.total_caps_size_log2) / lde_factor;
    assert!(subtree_cap_size > 0);

    let mut memory_subtrees = Vec::with_capacity(lde_factor);
    let now = std::time::Instant::now();
    for domain in memory_ldes.iter() {
        let memory_tree = DefaultTreeConstructor::construct_for_coset(
            &domain.trace,
            subtree_cap_size,
            true,
            worker,
        );
        memory_subtrees.push(memory_tree);
    }

    let dump_fn = |caps: &[DefaultTreeConstructor]| {
        let mut result = Vec::with_capacity(caps.len());
        for el in caps.iter() {
            result.push(el.get_cap());
        }

        result
    };

    let caps = dump_fn(&memory_subtrees);
    println!("Memory witness commitment took {:?}", now.elapsed());

    (caps, witness_chunk.delegation_type as u32)
}

fn flatten_merkle_caps(trees: &[MerkleTreeCapVarLength]) -> Vec<u32> {
    let mut result = vec![];
    for subtree in trees.iter() {
        for cap_element in subtree.cap.iter() {
            result.extend_from_slice(cap_element);
        }
    }

    result
}

/// We need to draw a common challenge based on all the values that will contribute to the memory permutation grand product, and
/// delegation argument set equality
pub fn fs_transform_for_memory_and_delegation_arguments(
    main_circuit_setup_cap: &[MerkleTreeCapVarLength],
    final_register_values: &[FinalRegisterValue],
    risc_v_circuit_merkle_tree_caps: &[Vec<MerkleTreeCapVarLength>],
    delegation_circuits_merkle_tree_caps: &[(u32, Vec<Vec<MerkleTreeCapVarLength>>)],
) -> Seed {
    use transcript::blake2s_u32::BLAKE2S_BLOCK_SIZE_U32_WORDS;

    let mut memory_trace_transcript = transcript::Blake2sBufferingTranscript::new();

    // commit all registers
    let mut register_values_and_timestamps = Vec::with_capacity(32 + 32 * 2);
    for register in final_register_values.iter() {
        register_values_and_timestamps.push(register.value);
        let (low, high) = split_timestamp(register.last_access_timestamp);
        register_values_and_timestamps.push(low);
        register_values_and_timestamps.push(high);
    }

    memory_trace_transcript.absorb(&register_values_and_timestamps);

    // then commit setup of the main circuit, as it contains partial timestamps
    {
        let caps = flatten_merkle_caps(&main_circuit_setup_cap);
        memory_trace_transcript.absorb(&caps);
    }

    // then we commit all main RISC-V circuits. Note that we have a special contribution into it from circuit sequence index (as it's a part of
    // write timestamps), but we will not commit to it here as the verifier MUST check that 1) first such sequence is 0 2) every next sequence is previous + 1.
    // This way we only need to commit to the order here
    for caps in risc_v_circuit_merkle_tree_caps.iter() {
        let caps = flatten_merkle_caps(&caps);
        memory_trace_transcript.absorb(&caps);
    }

    assert_eq!(
        memory_trace_transcript.get_current_buffer_offset(),
        BLAKE2S_BLOCK_SIZE_U32_WORDS
    );

    // then for delegation circuits: delegation type contributes to the delegation argument's expressions, and as we have a variable number of them
    // we will always commit a tuple of delegation type + caps. This way the order is not too important, but we adhere to convention that
    // those should be batched and sorted

    assert!(delegation_circuits_merkle_tree_caps.is_sorted_by(|a, b| a.0 < b.0));
    for (delegation_type, caps) in delegation_circuits_merkle_tree_caps.iter() {
        if caps.len() > 0 {
            let mut buffer = [0u32; BLAKE2S_BLOCK_SIZE_U32_WORDS];
            buffer[0] = *delegation_type;
            memory_trace_transcript.absorb(&buffer);
        }
        for caps in caps.iter() {
            let caps = flatten_merkle_caps(&caps);
            memory_trace_transcript.absorb(&caps);
        }

        assert_eq!(
            memory_trace_transcript.get_current_buffer_offset(),
            BLAKE2S_BLOCK_SIZE_U32_WORDS
        );
    }
    let memory_challenges_seed = memory_trace_transcript.finalize();

    memory_challenges_seed
}

pub fn run_and_split_for_gpu<
    ND: NonDeterminismCSRSource<VectorMemoryImplWithRom>,
    C: MachineConfig,
    A: GoodAllocator,
>(
    num_cycles_upper_bound: usize,
    domain_size: usize,
    binary: &[u32],
    non_determinism: &mut ND,
    delegation_factories: HashMap<u16, Box<dyn Fn() -> DelegationWitness<A>>>,
    worker: &Worker,
) -> (
    u32,
    Vec<CycleData<C, A>>,
    HashMap<u16, Vec<DelegationWitness<A>>>,
    Vec<FinalRegisterValue>,
    Vec<Vec<(u32, (TimestampScalar, u32))>>,
) {
    // 检查几种machine使用同一个ROM地址空间高位参数。
    // 这里检查三种machine的ROM参数相同，是为了让不同machine配置共享同一套VM memory/ROM边界假设。
    // 否则你用default machine执行出来的ROM地址空间，可能和reduced machine setup里假设的不一致。
    assert_eq!(
        setups::risc_v_cycles::ROM_ADDRESS_SPACE_SECOND_WORD_BITS,
        setups::reduced_risc_v_machine::ROM_ADDRESS_SPACE_SECOND_WORD_BITS
    );
    assert_eq!(
        setups::risc_v_cycles::ROM_ADDRESS_SPACE_SECOND_WORD_BITS,
        setups::final_reduced_risc_v_machine::ROM_ADDRESS_SPACE_SECOND_WORD_BITS
    );

    let (
        final_pc,
        main_circuit_traces,
        delegation_traces,
        register_final_values,
        lazy_init_teardown_data,
    ) = run_till_end_for_gpu_for_machine_config::<
        ND,
        C,
        A,
        { setups::risc_v_cycles::ROM_ADDRESS_SPACE_SECOND_WORD_BITS },
    >(
        num_cycles_upper_bound,
        domain_size,
        binary,
        non_determinism,
        delegation_factories,
        worker,
    );

    (
        final_pc,
        main_circuit_traces,
        delegation_traces,
        register_final_values,
        lazy_init_teardown_data,
    )
}

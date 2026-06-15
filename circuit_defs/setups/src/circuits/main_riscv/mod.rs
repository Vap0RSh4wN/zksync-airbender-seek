use super::*;

/// 标准 main RISC-V machine 的 circuit setup 构造入口（IMStandardIsaConfig）。
///
/// 输入：
/// - bytecode: &[u32]：待证明程序的 ROM（RISC-V 机器码，按 32-bit word）；
/// - worker: &Worker：并行工具（lookup 表、FFT/LDE、setup precomputations 等）。
///
/// 输出：MainCircuitPrecomputations<IMStandardIsaConfig, A, B>，含约束 artifact、
/// table driver、twiddles、LDE、setup commitment 预计算及 GPU witness tracer 函数指针。
pub fn get_main_riscv_circuit_setup<A: GoodAllocator, B: GoodAllocator>(
    bytecode: &[u32],
    worker: &Worker,
) -> MainCircuitPrecomputations<IMStandardIsaConfig, A, B> {
    // 从机器配置取出允许的 delegation CSR 集合；编译电路与 CSR lookup 表均需此白名单
    //（例如 BLAKE2、BigInt 对应不同 delegation type id）。
    let delegation_csrs = IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS;

    // 编译 main RISC-V machine，得到约束布局、trace 长度、setup layout、quotient 相关等。
    let machine: cs::one_row_compiler::CompiledCircuitArtifact<Mersenne31Field> =
        ::risc_v_cycles::get_machine(bytecode, delegation_csrs);
    // ROM 表依赖 bytecode，CSR/delegation 表依赖 delegation_csrs，生成 lookup table driver。
    let table_driver = ::risc_v_cycles::get_table_driver(bytecode, delegation_csrs);

    // 为 main circuit 的 trace domain（DOMAIN_SIZE，如 2^22）预计算 FFT twiddles，供后续 LDE/proving 复用。
    let twiddles: Twiddles<_, A> = Twiddles::new(::risc_v_cycles::DOMAIN_SIZE, &worker);
    // Low-degree extension 预计算：domain 大小、LDE 放大倍数、source cosets 配置。
    let lde_precomputations = LdePrecomputations::new(
        ::risc_v_cycles::DOMAIN_SIZE,
        ::risc_v_cycles::LDE_FACTOR,
        ::risc_v_cycles::LDE_SOURCE_COSETS,
        &worker,
    );
    // 将固定 lookup 表、trace 长度、machine.setup_layout、twiddles、LDE 等组织为 setup 预计算对象。
    // 许多列在 prover 运行程序前即固定（ROM/decoder/range 等 setup trace），可承诺、可打开、可复用。
    let setup =
        SetupPrecomputations::<DEFAULT_TRACE_PADDING_MULTIPLE, A, DefaultTreeConstructor>::from_tables_and_trace_len(
            &table_driver,
            ::risc_v_cycles::DOMAIN_SIZE,
            &machine.setup_layout,
            &twiddles,
            &lde_precomputations,
            ::risc_v_cycles::LDE_FACTOR,
            ::risc_v_cycles::TREE_CAP_SIZE,
            &worker,
        );

    MainCircuitPrecomputations {
        compiled_circuit: machine,
        table_driver,
        twiddles,
        lde_precomputations,
        setup,
        // 函数指针：证明时用 MainRiscVOracle 将 RISC-V 执行轨迹写入 witness trace（非此处生成 witness）。
        witness_eval_fn_for_gpu_tracer: ::risc_v_cycles::witness_eval_fn_for_gpu_tracer,
    }
}

use one_row_compiler::LookupInput;

use super::*;
use crate::devices::diffs::PC_INC_STEP;
use crate::tables::*;

pub fn assert_no_unimp<F: PrimeField, C: Circuit<F>>(_cs: &mut C, _next_opcode: Register<F>) {
    todo!();

    // let term_low = Term::from(next_opcode.0[0]) - Term::<F>::from(UNIMP_OPCODE_LOW as u64);
    // let term_high = Term::from(next_opcode.0[1]) - Term::<F>::from(UNIMP_OPCODE_HIGH as u64);
    // // we never want them to simultaneously be 0, so we can make a variable and assert it's not zero
    // let inversion_witness_0 = cs.add_variable();
    // let t0 = cs.add_variable_from_constraint(
    //     Constraint::from(1) - term_low * Term::from(inversion_witness_0),
    // );
    // let inversion_witness_1 = cs.add_variable();
    // let t1 = cs.add_variable_from_constraint(
    //     Constraint::from(1) - term_high * Term::from(inversion_witness_1),
    // );
    // cs.add_constraint(Term::from(t0) * Term::from(t1));

    // let value_fn = |input: WitnessGenSource<'_, F>,
    //                 mut output: WitnessGenDest<'_, F>,
    //                 constants: &[F],
    //                 table_driver: &TableDriver<F>,
    //                 table_type: TableType| {
    //     debug_assert!(constants.is_empty());
    //     let mut opcode_low: F = input[0];
    //     opcode_low.sub_assign(&F::from_u64_unchecked(UNIMP_OPCODE_LOW as u64));
    //     let mut opcode_high: F = input[1];
    //     opcode_high.sub_assign(&F::from_u64_unchecked(UNIMP_OPCODE_HIGH as u64));
    //     let inv0 = opcode_low.inverse().unwrap_or(F::ZERO);
    //     let inv1 = opcode_high.inverse().unwrap_or(F::ZERO);
    //     output[0] = inv0;
    //     output[1] = inv1;
    // };
    // cs.set_values(
    //     &[
    //         next_opcode.0[0].get_variable(),
    //         next_opcode.0[1].get_variable(),
    //     ],
    //     &[inversion_witness_0, inversion_witness_1],
    //     &[],
    //     TableType::ZeroEntry,
    //     value_fn,
    // );
}

pub fn calculate_pc_next_no_overflows<F: PrimeField, CS: Circuit<F>>(
    circuit: &mut CS,
    pc: Register<F>,
) -> Register<F> {
    // Input invariant: PC % 4 == 0, preserved as:
    // - initial PC is valid % 4
    // - jumps and branches check for alignments

    // strategy:
    // - allocate lower part of addition result and ensure that it is 16 bits
    // - do not allocate carry and make sure that (pc_low + 4 - result) >> 16 is boolean
    // - compute new high as pc_high + ((pc_low + 4 - result) >> 16)
    // - make sure that new high is not equal to 2^16

    let pc_next_low = circuit.add_variable();
    circuit.require_invariant(
        pc_next_low,
        Invariant::RangeChecked {
            width: LIMB_WIDTH as u32,
        },
    );

    let pc_t = pc.get_terms();
    let mut carry_constraint = Constraint::empty();
    carry_constraint += pc_t[0].clone();
    carry_constraint += Term::from(PC_INC_STEP);
    carry_constraint -= Term::from(pc_next_low);
    carry_constraint.scale(F::from_u64_unchecked(1 << 16).inverse().unwrap());

    // ensure boolean
    let mut t = carry_constraint.clone();
    t -= Term::from(1u64);
    circuit.add_constraint(carry_constraint.clone() * t);

    let mut pc_high_constraint = carry_constraint;
    pc_high_constraint += pc_t[1].clone();
    // we will evaluate witness below all at once
    let pc_next_high = circuit
        .add_variable_from_constraint_allow_explicit_linear_without_witness_evaluation(
            pc_high_constraint,
        );
    // ensure that it is not equal to 2^16
    let inversion_witness = circuit.add_variable();
    circuit.add_constraint(
        (Term::from(inversion_witness) * (Term::from(pc_next_high) - Term::from(1u64 << 16)))
            - Term::from(1u64),
    );

    let pc_next = Register([Num::Var(pc_next_low), Num::Var(pc_next_high)]);

    // NOTE: we should try to set values before setting constraint as much as possible
    // setting values for overflow flags

    let pc_vars = [pc.0[0].get_variable(), pc.0[1].get_variable()];
    let pc_next_vars = [pc_next.0[0].get_variable(), pc_next.0[1].get_variable()];

    let value_fn = move |placer: &mut CS::WitnessPlacer| {
        use crate::cs::witness_placer::*;

        let pc_inc_step =
            <CS::WitnessPlacer as WitnessTypeSet<F>>::U32::constant(PC_INC_STEP as u32);
        let pc = placer.get_u32_from_u16_parts(pc_vars);
        let (pc_next, _of) = pc.overflowing_add(&pc_inc_step);
        placer.assign_u32_from_u16_parts(pc_next_vars, &pc_next);

        let pc_high = pc_next.shr(16);
        let mut pc_high = <CS::WitnessPlacer as WitnessTypeSet<F>>::Field::from_integer(pc_high);
        let shift = <CS::WitnessPlacer as WitnessTypeSet<F>>::Field::constant(
            F::from_u64_unchecked(1u64 << 16),
        );
        pc_high.sub_assign(&shift);
        let inversion_witness_value = pc_high.inverse();
        placer.assign_field(inversion_witness, &inversion_witness_value);
    };

    circuit.set_values(value_fn);

    pc_next
}

// pub fn read_from_mem<F: PrimeField, C: Circuit<F>>(
//     cs: &mut C,
//     _addr: Register<F>,
//     opt_ctx: &mut OptimizationContext<F, C>,
//     exec_flag: Boolean,
// ) -> RegisterDecomposition<F> {
//     let mem_slot = Register::new_from_placeholder::<C>(cs, Placeholder::MemSlot);
//     let res = RegisterDecomposition::split_reg_with_opt_ctx(cs, mem_slot, opt_ctx, exec_flag);
//     res
// }

#[allow(deprecated)]
pub fn read_from_shuffle_ram_or_bytecode_with_ctx<F: PrimeField, C: Circuit<F>>(
    cs: &mut C,
    local_timestamp_in_cycle: usize,
    address_aligned_low: Constraint<F>,
    address_aligned_high: Num<F>,
    opt_ctx: &mut OptimizationContext<F, C>,
    exec_flag: Boolean,
) -> (RegisterDecomposition<F>, ShuffleRamMemQuery, Variable) {
    let (mem_value, query, is_ram_range) =
        read_from_shuffle_ram_or_bytecode_no_decomposition_with_ctx(
            cs,
            local_timestamp_in_cycle,
            address_aligned_low,
            address_aligned_high,
            opt_ctx,
            exec_flag,
        );

    let res = RegisterDecomposition::split_reg_with_opt_ctx(cs, mem_value, opt_ctx, exec_flag);

    (res, query, is_ram_range)
}

pub(crate) fn read_from_shuffle_ram_or_bytecode_no_decomposition_with_ctx<
    F: PrimeField,
    C: Circuit<F>,
>(
    cs: &mut C,
    local_timestamp_in_cycle: usize,
    address_aligned_low: Constraint<F>,
    address_aligned_high: Num<F>,
    optimization_context: &mut OptimizationContext<F, C>,
    exec_flag: Boolean,
) -> (Register<F>, ShuffleRamMemQuery, Variable) {
    // NOTE: all lookup actions here are conditional, so we should not accume that boolean is so,
    // and should not use special operations like Boolean::and where witness generation is specialized.

    // This is ok even for masking into x0 read/write for query as we are globally predicated by memory operations flags,
    // so if it's not a memory operation it'll be overwritten during merge of memory queries

    let [is_ram_range, address_high_bits_for_rom] = optimization_context.append_lookup_relation(
        cs,
        &[address_aligned_high.get_variable()],
        TableType::RomAddressSpaceSeparator.to_num(),
        exec_flag,
    );
    // this one is also aligned
    let rom_address = address_aligned_low.clone()
        + Term::from((F::from_u64_unchecked(1 << 16), address_high_bits_for_rom));

    let [rom_value_low, rom_value_high] = optimization_context
        .append_lookup_relation_from_linear_terms(
            cs,
            &[rom_address],
            TableType::RomRead.to_num(),
            exec_flag,
        );

    // no range check is needed here, as our RAM is consistent by itself - our writes(!) are range-checked,
    // so any reads will have to be range-checked
    let ram_result = Register::new_unchecked_from_placeholder(cs, Placeholder::MemSlot);
    // If it is not RAM query, we should mask is as x0 register access,
    // with a corresponding value

    let ram_result_masked_low = cs.add_variable_from_constraint(
        Term::from(is_ram_range) * Term::from(ram_result.0[0].get_variable()),
    );
    let ram_result_masked_high = cs.add_variable_from_constraint(
        Term::from(is_ram_range) * Term::from(ram_result.0[1].get_variable()),
    );

    let ram_address_masked_low =
        cs.add_variable_from_constraint(address_aligned_low * Term::from(is_ram_range));
    let ram_address_masked_high = cs.add_variable_from_constraint(
        Term::from(is_ram_range) * Term::from(address_aligned_high.get_variable()),
    );

    let is_register = cs.add_variable_from_constraint_allow_explicit_linear(
        Term::from(1) - Term::from(is_ram_range),
    );

    let query_type = ShuffleRamQueryType::RegisterOrRam {
        is_register: Boolean::Is(is_register),
        address: [ram_address_masked_low, ram_address_masked_high],
    };

    let query = ShuffleRamMemQuery {
        query_type,
        local_timestamp_in_cycle,
        read_value: [ram_result_masked_low, ram_result_masked_high],
        write_value: [ram_result_masked_low, ram_result_masked_high],
    };

    // and here we have to quasy-choose between value from ROM and RAM queries, and in the path we take
    // we also know that value is range-checked, otherwise it is not important
    let result_low = cs.add_variable_from_constraint(
        Term::from(is_ram_range) * Term::from(ram_result_masked_low)
            + (Term::from(1) - Term::from(is_ram_range)) * Term::from(rom_value_low),
    );
    let result_high = cs.add_variable_from_constraint(
        Term::from(is_ram_range) * Term::from(ram_result_masked_high)
            + (Term::from(1) - Term::from(is_ram_range)) * Term::from(rom_value_high),
    );

    let result = Register([Num::Var(result_low), Num::Var(result_high)]);

    (result, query, is_ram_range)
}

/// 用pc从RomRead表读取当前指令。
///
/// 输入pc是一个32-bit寄存器值，但field不能安全承载任意u32 opcode，
/// 所以RomRead表把opcode拆成两个16-bit limb返回。
/// 这个函数先用RomAddressSpaceSeparator处理pc高位，再用RomRead查出opcode_low16和opcode_high16。
pub(crate) fn read_opcode_from_rom<
    F: PrimeField,
    C: Circuit<F>,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    cs: &mut C,
    pc: Register<F>,
) -> Register<F> {
    // 先确保ROM地址宽度不超过field安全范围。
    // ROM 地址由 pc 高位拆分出的 rom_address_low 与 pc 低位组合。
    // 组合后的位数不能超过 field 安全承载范围。main RISC-V 的 ROM_ADDRESS_SPACE_SECOND_WORD_BITS 在 risc_v_cycles 里配置，这里只保证地址编码不会溢出 field。
    assert!(16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS <= F::CHAR_BITS - 1);

    // 第一个lookup读取pc高16位对应的ROM地址辅助信息：
    // is_ram_range 表示当前高位是否落在RAM区，
    // rom_address_low 表示组合ROM地址需要的高位部分。
    // get_variables_from_lookup_constrained 做两件事：向 lookup_storage 登记一条 RomAddressSpaceSeparator 查询；分配输出变量。
    // 第三章 create_table_driver_into_cs 已把这张表注册进 table_driver；lookup argument 后续证明查询行在 setup trace 的固定表里。
    // ADD 行 pc_high=0 时，表输出 is_ram_range=0、rom_address_low=0。
    let [is_ram_range, rom_address_low] = cs.get_variables_from_lookup_constrained(
        &[LookupInput::from(pc.0[1].get_variable())],
        TableType::RomAddressSpaceSeparator,
    );
    // instruction fetch只能从ROM读，所以这里强制is_ram_range = 0。
    // Constraint::from(is_ram_range) 是次数为 1 的约束，要求 is_ram_range = 0。
    // main machine 配置 USE_ROM_FOR_BYTECODE=true，取指不能从 RAM 读指令。若 witness 填 is_ram_range=1，电路不满足。
    // 这条约束进入 constraint_storage
    cs.add_constraint_allow_explicit_linear(Constraint::<F>::from(is_ram_range));
    // 把pc低16位和rom_address_low重新拼成完整ROM地址：
    // rom_address = pc_low + 2^16 * rom_address_low
    // ADD 行：pc_low=0，rom_address_low=0，故 rom_address=0。是两个 limb 变量在约束下的值。
    let rom_address_constraint = Term::from(pc.0[0].get_variable())
        + Term::from((F::from_u64_unchecked(1 << 16), rom_address_low));

    // 第二个lookup在RomRead表里查出opcode的两个16-bit limb。
    // 输入：上一行拼出的 rom_address_constraint（线性表达式，不是裸整数）。
    // 输出：opcode 的两个 16-bit limb，变量 low 和 high。这里只分配 low、high 两个空 Variable，并把一条 lookup 记进 lookup_storage。
    // 同时用 set_values 把一段计算登记进 witness_graph：「将来 witness 运行时，先算 rom_address，再查 RomRead 表，把结果写进 low/high」。
    let [low, high] = cs.get_variables_from_lookup_constrained(
        &[LookupInput::from(rom_address_constraint)],
        TableType::RomRead,
    );

    // 返回值仍然是Register形状，[low16, high16]。
    let result = Register([Num::Var(low), Num::Var(high)]);

    result
}

#[allow(dead_code)]
pub(crate) fn get_register_op_as_shuffle_ram<F: PrimeField, C: Circuit<F>>(
    cs: &mut C,
    reg_encoding: Num<F>,
    bytecode_is_in_rom_only: bool,
    is_first: bool,
) -> (Register<F>, ShuffleRamMemQuery) {
    // NOTE: since we use a value from read set, it means we do not need range check
    let (mut local_timestamp_in_cycle, placeholder) = if is_first {
        (0, Placeholder::FirstRegMem)
    } else {
        (1, Placeholder::SecondRegMem)
    };
    if bytecode_is_in_rom_only == false {
        local_timestamp_in_cycle += 1;
    }
    // no range check is needed here, as our RAM is consistent by itself - our writes(!) are range-checked,
    // so any reads will have to be range-checked
    let value = Register::new_unchecked_from_placeholder::<C>(cs, placeholder);

    // registers live in their separate address space
    let query = form_mem_op_for_register_only(local_timestamp_in_cycle, reg_encoding, value, value);

    (value, query)
}

#[allow(dead_code)]
pub(crate) fn get_rs1_as_shuffle_ram<F: PrimeField, C: Circuit<F>>(
    cs: &mut C,
    reg_encoding: Num<F>,
    bytecode_is_in_rom_only: bool,
) -> (Register<F>, ShuffleRamMemQuery) {
    // NOTE: since we use a value from read set, it means we do not need range check
    let (mut local_timestamp_in_cycle, placeholder) = (0, Placeholder::FirstRegMem);
    if bytecode_is_in_rom_only == false {
        local_timestamp_in_cycle += 1;
    }

    // no range check is needed here, as our RAM is consistent by itself - our writes(!) are range-checked,
    // so any reads will have to be range-checked
    let value = Register::new_unchecked_from_placeholder::<C>(cs, placeholder);

    // registers live in their separate address space
    let query = form_mem_op_for_register_only(local_timestamp_in_cycle, reg_encoding, value, value);

    (value, query)
}

#[allow(dead_code)]
pub(crate) struct RS2ShuffleRamQueryCandidate<F: PrimeField> {
    pub(crate) rs2: Constraint<F>,
    pub(crate) local_timestamp_in_cycle: usize,
    pub(crate) read_value: [Variable; REGISTER_SIZE],
}

#[allow(dead_code)]
pub(crate) fn prepare_rs2_op_as_shuffle_ram<F: PrimeField, C: Circuit<F>>(
    cs: &mut C,
    rs2_constraint: Constraint<F>,
    bytecode_is_in_rom_only: bool,
) -> (Register<F>, RS2ShuffleRamQueryCandidate<F>) {
    // NOTE: since we use a value from read set, it means we do not need range check
    let (mut local_timestamp_in_cycle, placeholder) = (1, Placeholder::SecondRegMem);
    if bytecode_is_in_rom_only == false {
        local_timestamp_in_cycle += 1;
    }

    // no range check is needed here, as our RAM is consistent by itself - our writes(!) are range-checked,
    // so any reads will have to be range-checked
    let value = Register::new_unchecked_from_placeholder::<C>(cs, placeholder);

    // here we should manually form temporary holder
    let query = RS2ShuffleRamQueryCandidate {
        rs2: rs2_constraint,
        local_timestamp_in_cycle,
        read_value: value.0.map(|el| el.get_variable()),
    };

    (value, query)
}

#[allow(dead_code)]
pub(crate) fn update_register_op_as_shuffle_ram<F: PrimeField, C: Circuit<F>>(
    cs: &mut C,
    local_timestamp_in_cycle: usize,
    reg_encoding: Num<F>,
    reg_value: Register<F>,
    execute_register_update: Boolean,
    memory_store_query_to_merge: ShuffleRamMemQuery,
    execute_memory_store: Boolean,
) -> ShuffleRamMemQuery {
    assert_eq!(
        local_timestamp_in_cycle,
        memory_store_query_to_merge.local_timestamp_in_cycle
    );

    // if we will not update register and do not execute memory store, then
    // we still want to model it as reading x0 (and writing back hardcoded 0)

    let reg_is_zero = cs.is_zero(reg_encoding);
    // we ALWAYS write to register (with maybe modified value), unless we write to RAM

    // But if we do NOT need to execute register update, OR if dst register is x0, then we must mask a value
    let mask_value_to_zero = Boolean::or(&execute_register_update.toggle(), &reg_is_zero, cs);
    // if we write to x0, then we will write 0
    let reg_write_value = reg_value.mask(cs, mask_value_to_zero.toggle());

    // no range check is needed here, as our RAM is consistent by itself - our writes(!) are range-checked,
    // so any reads will have to be range-checked
    let reg_read_value =
        Register::new_unchecked_from_placeholder(cs, Placeholder::WriteRdReadSetWitness);

    // registers live in their separate address space, so we just choose here, and default to 0
    // - no register update, no store -> address 0
    // - register update, no store -> reg index in low
    // - store, no register update -> RAM address

    let ShuffleRamQueryType::RegisterOrRam {
        is_register: _,
        address: ram_query_address,
    } = memory_store_query_to_merge.query_type
    else {
        panic!("we expect query to merge to be RAM")
    };

    let addr_low = cs.add_variable_from_constraint(
        (Term::from(reg_encoding) * Term::from(execute_register_update)) // register
        + (Term::from(ram_query_address[0]) * Term::from(execute_memory_store)), // RAM
    );
    let addr_high = cs.add_variable_from_constraint(
        Term::from(ram_query_address[1]) * Term::from(execute_memory_store),
    );

    let is_register = Boolean::choose(
        cs,
        &execute_memory_store,
        &Boolean::Constant(false),
        &Boolean::Constant(true),
    );
    let query_type = ShuffleRamQueryType::RegisterOrRam {
        is_register,
        address: [addr_low, addr_high],
    };

    let mut query = memory_store_query_to_merge;
    query.query_type = query_type;
    query.read_value = std::array::from_fn(|i| {
        cs.choose(
            execute_memory_store,
            Num::Var(query.read_value[i]),
            reg_read_value.0[i],
        )
        .get_variable()
    });
    query.write_value = std::array::from_fn(|i| {
        cs.choose(
            execute_memory_store,
            Num::Var(query.write_value[i]),
            reg_write_value.0[i],
        )
        .get_variable()
    });

    query
}

/// 把「读某个寄存器编号」封装成标准 `ShuffleRamMemQuery`
pub fn form_mem_op_for_register_only<F: PrimeField>(
    local_timestamp_in_cycle: usize,
    reg_idx: Num<F>,
    read_value: Register<F>,
    write_value: Register<F>,
) -> ShuffleRamMemQuery {
    ShuffleRamMemQuery {
        // `query_type::RegisterOnly`明确这是寄存器访问，不是 RAM
        query_type: ShuffleRamQueryType::RegisterOnly {
            register_index: reg_idx.get_variable(), //`raw_decoder_output.rs1` 的 Variable，witness 为 1（x1 编号）
        },
        local_timestamp_in_cycle, //`RS1_LOAD_LOCAL_TIMESTAMP`，本行第 0 次访问
        // 同一组 limb 变量；读寄存器时写回值等于读值，表示「未修改寄存器内容」
        read_value: read_value.0.map(|el| el.get_variable()),
        write_value: write_value.0.map(|el| el.get_variable()),
    }
}

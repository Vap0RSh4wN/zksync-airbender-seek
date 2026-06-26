use super::*;
use crate::machine::machine_configurations::state_transition_parts::*;

/// 写出一行main machine状态转移。
///
/// 这个函数把一个RISC-V cycle拆成固定顺序：
/// 读取初始状态 -> 用pc查ROM -> decode -> 预分配三个memory query槽位
/// -> 各opcode family产生候选结果 -> OptimizationContext统一写约束
/// -> writeback选择最终rd和next_pc -> 返回final_state。
pub(crate) fn optimized_base_isa_state_transition<
    F: PrimeField,
    CS: Circuit<F>,
    const ASSUME_TRUSTED_CODE: bool,
    const OUTPUT_EXACT_EXCEPTIONS: bool,
    const PERFORM_DELEGATION: bool,
    const SUPPORT_SIGNED_MUL_DIV: bool,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    cs: &mut CS,
    decode_table_splitting: [usize; 2],
    boolean_keys: DecoderOutputExtraKeysHolder,
) -> (
    MinimalStateRegistersInMemory<F>,
    MinimalStateRegistersInMemory<F>,
) {
    // 初始化本行CPU状态。对main machine来说，最关键的跨行状态是pc，从这个状态里取出pc变量。
    let initial_state = MinimalStateRegistersInMemory::<F>::initialize(cs);

    // now apply decoding and all the opcodes.
    // Note that we use custom decoder here

    // 取当前行pc，后面先用它读ROM，再根据opcode决定本行执行哪类指令。
    // 对 ADD 第一行，witness 阶段会把 pc_low=0、pc_high=0 填进去。上一行 final_state.pc 与本行 initial_state.pc 通过 state linkage 相连；
    // 第一行没有上一行，public input 或边界条件决定起始 pc。
    let pc = *initial_state.get_pc();

    // pc.0[0] 是低 16 位 limb。require_invariant 把一条 range check 请求记入 BasicAssembly，
    // finalize 后进入 CircuitOutput.range_check_expressions，后续编译成 range 表 lookup。
    // 源码注释说明：decoder 读 ROM 时会处理 pc 高半部分；这里先约束低半位落在 16-bit 范围内。ADD 行 pc=0，pc_low=0 满足 range check。
    // 给pc低16位加range check，pc高半部分会在decoder读取ROM时参与拆分并完成范围约束，所以这里先处理低半部分。
    cs.require_invariant(
        pc.0[0].get_variable(),
        Invariant::RangeChecked {
            width: LIMB_WIDTH as u32,
        },
    );

    // TODO: because PCs are part of the state and are linked from the previous row, then by recursion they are range checked,
    // and we may consider to remove this extra range check completely

    // TODO: Reading opcode from ROM here checks that PC % 4 == 0, so we can skip checks for PC % 4 == 0 in jump and branch instructions
    // 读ROM、decode opcode，并预分配本行固定的三个memory query槽位。
    // memory_queries: 三个预分配 query
    // src1: x1 的寄存器值变量
    // src2: x2 的寄存器值变量
    // flags_source: 可查询 ADD_OP_KEY
    // opcode_format_bits: r_insn=1
    let (memory_queries, src1, src2, raw_decoder_output, flags_source, opcode_types_bits) =
        optimized_decode_and_preallocate_mem_queries_for_bytecode_in_rom::<
            F,
            CS,
            ASSUME_TRUSTED_CODE,
            PERFORM_DELEGATION,
            ROM_ADDRESS_SPACE_SECOND_WORD_BITS,
        >(cs, pc, decode_table_splitting, boolean_keys);

    // 接下来，
    // 1. 每个 opcode family 登记「若本行是我，则应满足的关系」（多数先进 OptimizationContext，不立刻 add_constraint）。
    // 2. 全部登记完后 enforce_all，再 writeback 选最终 rd/pc。
    // 一行 CPU 电路会同时跑 AddOp、SubOp、LoadOp……但只有 decoder 标为 1 的那个 family 的 exec_flag 会让约束生效。

    // now with PC considered range-checked we can compute next PC without overflows
    // 默认next_pc = pc + 4。branch、jump等opcode family后面可以返回自定义pc候选值。
    let next_pc = calculate_pc_next_no_overflows(cs, pc);

    // OptimizationContext先收集候选算术关系、lookup关系和is_zero关系，稍后统一落成Constraint。
    // 暂存 ADD/SUB/MUL 等关系的缓冲区；4.13 的 enforce_all 才真正写入 Circuit
    // 作用：若每个 AddOp/LoadOp 各自 add_constraint，会重复分配 carry、range check 变量。
    // OptimizationContext 把同类关系攒在一起，enforce_all 时按 exec_flag 加权合并，再写一组约束——既省列数，又保持「一行只生效一个 family」。
    let mut opt_ctx = OptimizationContext::<F, CS>::new();

    // parse_reg：把 Register 拆成带符号信息的结构，供 branch 等指令使用；ADD 主要用里面的 limb 值。
    let src1 = RegisterDecompositionWithSign::parse_reg(cs, src1);
    let src2 = RegisterDecompositionWithSign::parse_reg(cs, src2);

    // OptimizedDecoderOutput 是 decoder 刚产出的原始解码结果，贴近 instruction encoding。
    // BasicDecodingResultWithSigns 是 交给各个 opcode family 使用的执行输入容器，贴近后续 AddOp/LoadOp/StoreOp 的消费方式。

    // decoder输出整理成各opcode family统一读取的 decoder 输出形状（pc、src1、src2、imm 等）。
    let decoder_output = BasicDecodingResultWithSigns {
        pc_next: next_pc,
        src1,
        src2,
        rs2_index: raw_decoder_output.rs2.clone(),
        imm: raw_decoder_output.imm,
        funct3: raw_decoder_output.funct3,
        funct12: raw_decoder_output.funct12,
    };

    cs.set_log(&opt_ctx, "DECODER");
    // 每个opcode family返回一份CommonDiffs（候选 rd、候选 pc、exec_flag），里面保存exec_flag、rd候选值和pc候选值。
    // writeback 后面从这里选最终结果。
    let mut application_results = Vec::<CommonDiffs<F>>::with_capacity(32);

    // ADD 行里 `AddOp::apply` 返回：
    // exec_flag   = is_add
    // rd_value    = [(16, is_add)]
    // new_pc_value = Default
    // 随后 `application_results: Vec<CommonDiffs<F>>` 收集所有 family 的返回值。writeback 后面做两件事：
    // 1. `select_final_rd_value` 从全部 `rd_value` 候选里选真正写回的那个；
    // 2. `select_final_pc_value` 从 `Default` 和 `Custom` 里选真正的 next_pc。
    let application_result = AddOp::apply::<_, ASSUME_TRUSTED_CODE, OUTPUT_EXACT_EXCEPTIONS>(
        cs,
        &initial_state,
        &decoder_output,
        &flags_source,
        &mut opt_ctx,
    );
    application_results.push(application_result);
    cs.set_log(&opt_ctx, "ADD");

    let application_result = SubOp::apply::<_, ASSUME_TRUSTED_CODE, OUTPUT_EXACT_EXCEPTIONS>(
        cs,
        &initial_state,
        &decoder_output,
        &flags_source,
        &mut opt_ctx,
    );
    application_results.push(application_result);
    cs.set_log(&opt_ctx, "SUB");

    let application_result = LuiOp::apply::<_, ASSUME_TRUSTED_CODE, OUTPUT_EXACT_EXCEPTIONS>(
        cs,
        &initial_state,
        &decoder_output,
        &flags_source,
        &mut opt_ctx,
    );
    application_results.push(application_result);
    cs.set_log(&opt_ctx, "LUI");

    let application_result = AuiPc::apply::<_, ASSUME_TRUSTED_CODE, OUTPUT_EXACT_EXCEPTIONS>(
        cs,
        &initial_state,
        &decoder_output,
        &flags_source,
        &mut opt_ctx,
    );
    application_results.push(application_result);
    cs.set_log(&opt_ctx, "AUIPC");

    let application_result = BinaryOp::apply::<_, ASSUME_TRUSTED_CODE, OUTPUT_EXACT_EXCEPTIONS>(
        cs,
        &initial_state,
        &decoder_output,
        &flags_source,
        &mut opt_ctx,
    );
    application_results.push(application_result);
    cs.set_log(&opt_ctx, "BINARY");

    let application_result =
        MulOp::<SUPPORT_SIGNED_MUL_DIV>::apply::<_, ASSUME_TRUSTED_CODE, OUTPUT_EXACT_EXCEPTIONS>(
            cs,
            &initial_state,
            &decoder_output,
            &flags_source,
            &mut opt_ctx,
        );
    application_results.push(application_result);
    cs.set_log(&opt_ctx, "MUL");

    let application_result = DivRemOp::<SUPPORT_SIGNED_MUL_DIV>::apply::<
        _,
        ASSUME_TRUSTED_CODE,
        OUTPUT_EXACT_EXCEPTIONS,
    >(
        cs,
        &initial_state,
        &decoder_output,
        &flags_source,
        &mut opt_ctx,
    );
    application_results.push(application_result);
    cs.set_log(&opt_ctx, "DIVREM");

    let application_result =
        ConditionalOp::<true>::apply::<_, ASSUME_TRUSTED_CODE, OUTPUT_EXACT_EXCEPTIONS>(
            cs,
            &initial_state,
            &decoder_output,
            &flags_source,
            &mut opt_ctx,
        );
    application_results.push(application_result);
    cs.set_log(&opt_ctx, "CONDITIONAL");

    let application_result =
        ShiftOp::<true, false>::apply::<_, ASSUME_TRUSTED_CODE, OUTPUT_EXACT_EXCEPTIONS>(
            cs,
            &initial_state,
            &decoder_output,
            &flags_source,
            &mut opt_ctx,
        );
    application_results.push(application_result);
    cs.set_log(&opt_ctx, "SHIFT_SRA_ROT");

    let application_result = JumpOp::apply::<_, ASSUME_TRUSTED_CODE, OUTPUT_EXACT_EXCEPTIONS>(
        cs,
        &initial_state,
        &decoder_output,
        &flags_source,
        &mut opt_ctx,
    );
    application_results.push(application_result);
    cs.set_log(&opt_ctx, "JUMP");

    // 三个固定槽位：
    // slot 0 读取rs1
    // slot 1 读取rs2或LOAD用RAM read
    // slot 2 写回rd或STORE用RAM write
    let [rs1_query, mut rs2_or_mem_load_query, mut rd_or_mem_store_query] = memory_queries;

    let application_result = LoadOp::<true, true>::spec_apply::<
        _,
        _,
        _,
        _,
        _,
        _,
        ASSUME_TRUSTED_CODE,
        OUTPUT_EXACT_EXCEPTIONS,
    >(
        cs,
        &initial_state,
        &decoder_output,
        &flags_source,
        &mut rs2_or_mem_load_query,
        &mut opt_ctx,
    );
    application_results.push(application_result);
    cs.set_log(&opt_ctx, "LOAD");

    let application_result = StoreOp::<true>::spec_apply::<
        _,
        _,
        _,
        _,
        _,
        _,
        ASSUME_TRUSTED_CODE,
        OUTPUT_EXACT_EXCEPTIONS,
    >(
        cs,
        &initial_state,
        &decoder_output,
        &flags_source,
        &mut rd_or_mem_store_query,
        &mut opt_ctx,
    );
    application_results.push(application_result);
    cs.set_log(&opt_ctx, "STORE");

    if PERFORM_DELEGATION == false {
        // CSR operation must be hand implemented for most of the machines, even though we can declare support of it in the opcode
        let application_result = apply_non_determinism_csr_only_assuming_no_unimp::<
            _,
            _,
            _,
            _,
            _,
            _,
            false,
            false,
            false,
            ASSUME_TRUSTED_CODE,
            OUTPUT_EXACT_EXCEPTIONS,
        >(
            cs,
            &initial_state,
            &decoder_output,
            &flags_source,
            &mut opt_ctx,
        );
        application_results.push(application_result);
    } else {
        let application_result = apply_csr_with_delegation::<
            _,
            _,
            _,
            _,
            _,
            _,
            false,
            false,
            false,
            ASSUME_TRUSTED_CODE,
            OUTPUT_EXACT_EXCEPTIONS,
        >(
            cs,
            &initial_state,
            &decoder_output,
            &flags_source,
            &mut opt_ctx,
        );
        application_results.push(application_result);
    };
    cs.set_log(&opt_ctx, "CSR");

    // 所有opcode family都登记完候选关系后，再统一把它们变成Constraint和lookup。
    opt_ctx.enforce_all(cs);
    // drop(opt_ctx);
    cs.set_log(&opt_ctx, "OPT_CONTEXT");

    // 现在开始合并状态：从所有候选rd和pc里选出当前行真正生效的那个结果。

    assert!(OUTPUT_EXACT_EXCEPTIONS == false);

    let final_state = writeback_no_exception_with_opcodes_in_rom::<
        F,
        CS,
        ASSUME_TRUSTED_CODE,
        PERFORM_DELEGATION,
    >(
        cs,
        opcode_types_bits,
        raw_decoder_output.rd,
        rs1_query,
        rs2_or_mem_load_query,
        rd_or_mem_store_query,
        application_results,
        next_pc,
        &opt_ctx,
    );

    (initial_state, final_state)
}

use super::*;
use crate::machine::{
    decoder::decode_optimized_must_handle_csr::OptimizedDecoderOutput,
    ops::{RD_STORE_LOCAL_TIMESTAMP, RS1_LOAD_LOCAL_TIMESTAMP, RS2_LOAD_LOCAL_TIMESTAMP},
};

/// 读取opcode、执行decoder，并预分配本行固定的三个shuffle RAM query槽位。
///
/// 返回值顺序：
/// [slot0, slot1, slot2], src1, src2, raw_decoder_output, flags_source, opcode_format_bits
///
/// slot0固定表示rs1读取；
/// slot1固定表示rs2读取，或者LOAD使用的RAM读取；
/// slot2固定表示rd写回，或者STORE使用的RAM写入。
pub(crate) fn optimized_decode_and_preallocate_mem_queries_for_bytecode_in_rom<
    F: PrimeField,
    CS: Circuit<F>,
    const ASSUME_TRUSTED_CODE: bool,
    const PERFORM_DELEGATION: bool,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    cs: &mut CS,
    pc: Register<F>,
    decode_table_splitting: [usize; 2],
    boolean_keys: DecoderOutputExtraKeysHolder,
) -> (
    [ShuffleRamMemQuery; 3],
    Register<F>,
    Register<F>,
    OptimizedDecoderOutput<F>,
    BasicFlagsSource,
    [Boolean; NUM_INSTRUCTION_TYPES_IN_DECODE_BITS],
) {
    // 第一步：根据pc从RomRead表取出当前指令编码。
    let next_opcode = read_opcode_from_rom::<F, CS, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(cs, pc);
    // 此时，CircuitOutput 新增:
    // lookups: 上述两条
    // constraints: is_ram_range = 0
    // range_check: pc_low 的 16-bit range
    // 以ADD为例子，此时 CPU row 只知道「当前 pc 处 instruction 的 32-bit 位模式」。更准确地说，它已经拿到了足以唯一确定 rs1/rs2/rd/opcode/funct3/funct7 的完整 32-bit 编码，
    // 但这些信息还只是隐含在位模式里，尚未被 decoder 拆成显式的 rs1=1、rs2=2、rd=5、ADD_OP_KEY=1 等变量。那是 4.9 decoder 的工作。

    // 这里有一件小事 -如果我们使用不匹配 CSR 索引的 CSR 处理，那么我们必须在这里处理 UNIMP 指令，即 csrrw x0, Cycle, x0
    // ROM 也用 UNIMP 填充

    if ASSUME_TRUSTED_CODE {
        if PERFORM_DELEGATION {
            // 对应的路径中会有CSR索引的mtaching，并且我们不支持“cycle”csr，所以我们会失败

            // 什么都不做
        } else {
            // assert_no_unimp 对 UNIMP 编码加约束，防止 ROM padding 区的无效指令被当成合法执行
            assert_no_unimp(cs, next_opcode);
        }
    } else {
        unimplemented!()
    }

    if let Some(opcode) = next_opcode.get_value_unsigned(cs) {
        println!("Opcode = 0x{:08x}", opcode);
    }

    use crate::machine::decoder::decode_optimized_must_handle_csr::*;
    use crate::machine::decoder::DecoderInput;

    // 第二步：把opcode送入decoder，next_opcode包装成DecoderInput，得到寄存器索引、立即数、指令格式位和major family flags。
    // instruction 就是 4.8 得到的 next_opcode，形状仍是 Register<F> = [low16, high16]。
    let decoder_input = DecoderInput {
        instruction: next_opcode,
    };
    // decode 返回四个对象，含义如下。
    // **invalid_opcode**（Boolean）：若 instruction 不在 machine 支持的 opcode 集合里，这个 flag 为 1。trusted code 下后面会强制它等于 0。
    // **raw_decoder_output**：字段容器，ADD 行里最重要的是：
    // - rs1：Constraint 形状，解码出源寄存器编号 1（即 x1）
    // - rs2：编号 2（x2）
    // - rd：编号 5（x5）
    // - imm、funct3、funct12：ADD 行也有，但 ADD 不用 imm 当 src2
    // **opcode_format_bits**：六个互斥 Boolean，表示 R/I/S/B/U/J。ADD 是 R-type，故 r_insn=1，其余为 0。
    // **other_bits**：major opcode family 的布尔位集合。AddOp 在编译期通过 define_decoder_subspace 声明 ADD 和 ADDI 都归入 ADD_OP_KEY；decode 后 ADD_OP_KEY 对应位为 1。
    let (invalid_opcode, raw_decoder_output, opcode_format_bits, other_bits) =
        OptimizedDecoder::decode::<F, CS>(&decoder_input, cs, decode_table_splitting);

    if ASSUME_TRUSTED_CODE {
        // 信任代码配置下，代码把invalid_opcode作为线性约束加入Circuit。
        // trusted code表示Airbender假设正在证明的guest program落在受支持的执行路径里。
        // 源码里遇到invalid opcode时，不会进入一个完整exception handler分支，而是把`invalid_opcode = 0`作为约束加入Circuit。
        // 若实际opcode无效，witness无法满足这条约束。
        cs.add_constraint_allow_explicit_linear_prevent_optimizations(Constraint::<F>::from(
            invalid_opcode,
        ));
    } else {
        unimplemented!()
    }

    // 把decoder的布尔输出包装成后续opcode family能查询的flag source，后面AddOp、LoadOp、StoreOp等都从这里取exec_flag。

    // boolean_keys 在 describe_state_transition 入口由 Machine::all_decoder_keys() 提供，是编译期登记的 family 名字列表。
    // other_bits 是本行 decode 出的布尔位。合在一起后，AddOp 可以写：
    // boolean_set.get_major_flag(ADD_OP_KEY)  // 4.12 里叫 exec_flag
    // 对 ADD 行，exec_flag 在 witness 里应为 1，表示「本行启用 ADD family 的候选关系」。
    let flags_source = BasicFlagsSource::new(boolean_keys, other_bits);

    // ### 为什么固定三个槽位
    // 真实 RISC-V 一行指令可能读 0~2 个寄存器、读/写 RAM。Airbender 把「一行最多三次访问」固定成三个槽位，换来统一 trace 形状：
    // slot 0：几乎总是读 rs1（寄存器）
    // slot 1：读 rs2，或 LOAD 时读 RAM
    // slot 2：写 rd，或 STORE 时写 RAM
    // ADD 只用寄存器；LW 把 slot1 改成 RAM 读；SW 把 slot2 改成 RAM 写。compiler 始终看到三个 shuffle_ram_queries。
    // ### ADD / LW / SW 三槽位对照
    // ADD x5,x1,x2：

    // slot 0: 读寄存器 1 → witness: 7
    // slot 1: 读寄存器 2 → witness: 9
    // slot 2: 写寄存器 5，旧 100 → 新 16

    // LW x5, 0(x10)：

    // slot 0: 读 x10（基址）
    // slot 1: RAM 读，address = x10 + imm
    // slot 2: 写 x5

    // SW x6, 0(x10)：

    // slot 0: 读 x10
    // slot 1: 读 x6（store value）
    // slot 2: RAM 写

    let mut memory_queries = vec![];

    // Register 形状，后面作为 src1
    let rs1_value = {
        // slot 0固定表示rs1寄存器读取。
        let (local_timestamp_in_cycle, placeholder) = (
            // RS1_LOAD_LOCAL_TIMESTAMP：本行内第一次访问的时间戳常数（具体数值在 ops 模块定义，读源码时搜这个名字即可）。
            RS1_LOAD_LOCAL_TIMESTAMP,
            // 给「slot0 读到的值」占一个 Variable 位置；witness 阶段填对应值
            Placeholder::ShuffleRamReadValue(0),
        );

        // no range check is needed here, as our RAM is consistent by itself - our writes(!) are range-checked,
        // so any reads will have to be range-checked
        // 分配 Variable，登记 placeholder 映射；不在此处单独 range check（读值一致性由 memory argument 连接历史写）。

        // new_unchecked_from_placeholder 里的 unchecked 不是跳过安全检查。
        // 它表示当前位置先不在本地添加 range check，因为这些值来自 register/RAM 的读集合，后续 memory argument 会把它们和之前写入过、已经 range checked 的值连接起来。
        let value = Register::new_unchecked_from_placeholder(cs, placeholder);

        // registers live in their separate address space
        // 构造 RegisterOnly 类型 query；地址是寄存器编号，不是 RAM 地址。
        let query = form_mem_op_for_register_only(
            local_timestamp_in_cycle,
            // decoder 给出的 rs1 编号 Constraint
            raw_decoder_output.rs1.clone(),
            // value 同时作为 read 和 write：表示「只读寄存器，不把新值写回去」。
            value,
            value,
        );
        memory_queries.push(query);

        value
    };

    // slot 1固定表示“rs2读取或LOAD的RAM读取”。
    // 先按寄存器读取形状分配，LoadOp后面可以把它改写成RAM read。
    let rs2_value_if_register = {
        // NOTE: since we use a value from read set, it means we do not need range check
        let (local_timestamp_in_cycle, placeholder) = (
            RS2_LOAD_LOCAL_TIMESTAMP,
            Placeholder::ShuffleRamReadValue(1),
        );

        // no range check is needed here, as our RAM is consistent by itself - our writes(!) are range-checked,
        // so any reads will have to be range-checked
        let value = Register::new_unchecked_from_placeholder(cs, placeholder);
        // read_address 来自 Placeholder::ShuffleRamAddress(1)：构造电路时 address 变量还是空的；
        // writeback 或 LoadOp 会把「寄存器 2」或「RAM 地址」约束进去。ADD 行后续 writeback 会把 slot1 的 address 绑到 rs2 编号。
        let read_address =
            Register::new_unchecked_from_placeholder(cs, Placeholder::ShuffleRamAddress(1));

        let query = ShuffleRamMemQuery {
            // 使用 RegisterOrRam 而非 RegisterOnly：保留 is_register 开关。ADD 行 is_register 编译期常量为 true，表示按寄存器编号解释 address。
            query_type: ShuffleRamQueryType::RegisterOrRam {
                is_register: Boolean::Constant(true),
                address: read_address.0.map(|el| el.get_variable()),
            },
            local_timestamp_in_cycle,
            read_value: value.0.map(|el| el.get_variable()),
            write_value: value.0.map(|el| el.get_variable()),
        };
        memory_queries.push(query);

        value
    };

    // slot 2固定表示“rd写回或STORE的RAM写入”。
    // 这里先按寄存器写回形状分配，StoreOp或writeback后面会把地址和值约束到正确结果。
    {
        let local_timestamp_in_cycle = RD_STORE_LOCAL_TIMESTAMP;
        // no range check is needed here, as our RAM is consistent by itself - our writes(!) are range-checked,
        // so any reads will have to be range-checked
        let read_value =
            Register::new_unchecked_from_placeholder(cs, Placeholder::ShuffleRamReadValue(2));
        // Also unchecked, as it would be constrained in STORE opcode, or at the end of the cycle
        let write_value =
            Register::new_unchecked_from_placeholder(cs, Placeholder::ShuffleRamWriteValue(2));

        let read_address =
            Register::new_unchecked_from_placeholder(cs, Placeholder::ShuffleRamAddress(2));

        let query = ShuffleRamMemQuery {
            query_type: ShuffleRamQueryType::RegisterOrRam {
                is_register: Boolean::Constant(true),
                address: read_address.0.map(|el| el.get_variable()),
            },
            local_timestamp_in_cycle,
            read_value: read_value.0.map(|el| el.get_variable()),
            write_value: write_value.0.map(|el| el.get_variable()),
        };
        memory_queries.push(query);
    }

    // opcode_format_bits 解构出六个格式 flag；decoder 保证互斥，同一行只有一个为 1。
    let [r_insn, i_insn, s_insn, b_insn, _u_insn, _j_insn] = opcode_format_bits;

    // src1 直接等于 rs1_value（slot0 读出的 x1）
    let src1 = rs1_value;

    // src2按指令格式选择来源：
    // R型取rs2寄存器值；
    // I型取立即数；
    // S型和B型也使用rs2寄存器值。
    // src2 = r_insn * rs2_reg + i_insn * imm + s_insn * rs2_reg + b_insn * rs2_reg
    let src2 = Register::choose_from_orthogonal_variants(
        cs,
        &[r_insn, i_insn, s_insn, b_insn],
        &[
            rs2_value_if_register,
            raw_decoder_output.imm,
            rs2_value_if_register,
            rs2_value_if_register,
        ],
    );

    (
        // memory_queries: 三个预分配 query
        memory_queries.try_into().unwrap(),
        // x1 的寄存器值变量
        src1,
        // x2 的寄存器值变量
        src2,
        raw_decoder_output,
        // 可查询 ADD_OP_KEY
        flags_source,
        // r_insn=1
        opcode_format_bits,
    )
}

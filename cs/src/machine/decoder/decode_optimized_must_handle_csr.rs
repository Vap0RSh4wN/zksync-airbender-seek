//! Optimized RISC-V instruction decoder
//!
//! This decoder extracts instruction fields and opcode-format information while
//! avoiding creation of unnecessary explicit variables. Whenever possible it
//! carries values as linear constraints and only materializes variables that
//! must exist for circuit logic. This is especially useful when CSR
//! handling is performed later, so we can defer checks until the CSR stage and
//! keep the decode phase cheap.
//!
//! High level overview:
//! - Split the 32-bit instruction into chunks used by different formats.
//! - Assign witnesses for small chunks which are range-checked with fixed tables.
//! - Build rs1, rs2, rd as explicit value or linear constraints.
//! - Construct funct7 and funct12.
//! - Perform a table lookup over [opcode | funct3 | funct7] to a bitmask
//!   of instruction format and variant flags R/I/S/B/U/J.
//! - Build a sign-extended immediate from dependent chunks.
//!
use one_row_compiler::LookupInput;

use super::*;
use crate::devices::risc_v_types::NUM_INSTRUCTION_TYPES;

// An optimization of basic decode for the case when CSR is explicitly matched later on. We try to drag values that are
// not needed as explicit variables all the way to output

pub const NUM_INSTRUCTION_TYPES_IN_DECODE_BITS: usize = NUM_INSTRUCTION_TYPES;

pub struct OptimizedDecoder;

/// 为什么 rs1 是 Num<F>，而 rs2 / rd 是 Constraint<F>？
/// rd 后面 writeback 时会和别的东西一起并入显式变量，所以现在先不分配变量
/// rs2 同理，先拖着线性表达式即可
/// 但 rs1 在后面要直接作为 slot0 的寄存器读地址使用，因此这里先 materialize 成了变量
/// 所以这不是语义差别，而是电路工程优化差别。
pub struct OptimizedDecoderOutput<F: PrimeField> {
    /// 第一源寄存器的编号，是一个5-bit字段，它的值1就表示寄存器x1，2就表示x2
    pub rs1: Num<F>,
    /// 第二源寄存器编号。
    pub rs2: Constraint<F>, // linear constraint
    /// 目标寄存器编号
    pub rd: Constraint<F>, // linear constraint
    // 立即数（immediate），已经被 decoder 组装成一个 32-bit 值。
    // 类型是 Register<F>，表示它不是一个小字段，而是一个完整的 32-bit 数，内部用两个 16-bit limb 表示。
    // 对不同指令：
    // ADD：不用 imm，但 decoder 仍然会构造一个形状统一的 imm
    // ADDI x5, x1, 3：imm = 3
    // LW x5, 8(x10)：imm = 8
    // B/J/U：imm 则按各自格式重组并符号扩展
    // 源码里 imm 的构造是 decoder 的一个重点，因为不同指令格式的立即数字段分散在不同 bit 位置，要重新拼起来
    pub imm: Register<F>,
    /// 3-bit 功能字段。
    /// 它通常和 opcode / funct7 一起决定具体是哪条指令。
    /// 对 ADD：
    /// funct3 = 000
    /// 举例：
    /// funct3=000 在某些 opcode 下可能表示 ADD/ADDI
    /// funct3=010 在 load/store 里可能表示 word/half 等不同变体
    /// branch 也常靠 funct3 区分 beq/bne/blt/...
    /// 所以 funct3 是“子操作码”。
    pub funct3: Num<F>,
    /// 7-bit 功能字段。
    /// 对 ADD x5, x1, x2：
    /// funct7 = 0000000
    /// 对 SUB x5, x1, x2：
    /// funct7 = 0100000
    /// 所以 funct7 在 ADD/SUB 这种共用同一大类 opcode 的场景里特别重要。
    pub funct7: Constraint<F>, // linear constraint
    /// 12-bit 功能字段。
    /// 这个字段主要是给 SYSTEM / CSR / ECALL / EBREAK 这类指令用的。
    pub funct12: Constraint<F>, // linear constraint
}

impl OptimizedDecoder {
    /// Decode a 32-bit instruction into fields, immediate, and opcode format flags.
    /// Returns:
    /// - is_invalid a boolean flag from the opcode lookup that marks an invalid encoding.
    /// - OptimizedDecoderOutput the structured decode result (rs1/rs2/rd/imm/funct fields).
    /// - [Boolean; NUM_INSTRUCTION_TYPES]: orthogonal format flags in order [R,I,S,B,U,J].
    /// - Vec<Boolean>: extra variant bits returned by the opcode table.
    pub fn decode<F: PrimeField, CS: Circuit<F>>(
        inputs: &DecoderInput<F>,
        circuit: &mut CS,
        splitting: [usize; 2],
    ) -> (
        Boolean,
        OptimizedDecoderOutput<F>,
        [Boolean; NUM_INSTRUCTION_TYPES],
        Vec<Boolean>,
    ) {
        // instruction set of variables: low: [15:0], high: [31:16]
        // the most shredded instruction type is B-type (with additional splitting of rs_2, required for J-type):
        // all other instruction types can be constructed from
        // chunks of split instruction are:
        // opcode [6:0], imm11: [7], imm[4-1]: [11:8], func3: [14:12], rs1: [19:15],
        // rs2_low: [20], rs2_high: [24:21], imm[10-5]: [30:25], imm12: [31]
        // rs1 crosses the border of register, so we need to additionally split it as
        // rs1_low: [15], rs1_high: [16-19]

        // NOTE: we DO range check opcode (7 bits) so we can later on use a single table lookup to get all our opcode properties

        let opcode = Num::Var(circuit.add_variable());
        // imm11 will be replaced as quadratic constraint over difference
        let imm4_1 = Num::Var(circuit.add_variable());
        let funct3 = Num::Var(circuit.add_variable());
        // RISC-V 的字段跨越 low16/high16 边界。rs1 位于 bit 19..15，其中 bit 15 在 low16，bit 16..19 在 high16，因此源码拆成 rs1_low 和 rs1_high
        let rs1_low = circuit.add_boolean_variable();
        let rs1_high = Num::Var(circuit.add_variable());
        // rs2_low will be replaced as quadratic constraint over difference
        // let rs2_low = circuit.add_boolean_variable();
        // rs2 的最低位在 high16 的 bit 4，其余四位在 high16 的 bit 5..8。源码没有给 rs2_low 单独分配变量，而是稍后用线性约束从 high16 里推出来。
        let rs2_high = Num::Var(circuit.add_variable());
        let imm10_5 = Num::Var(circuit.add_variable());
        let sign_bit = circuit.add_boolean_variable();

        // here we will have to write value-fn manually

        let input = inputs.instruction.0.map(|x| x.get_variable());

        let opcode_var = opcode.get_variable();
        let imm4_1_var = imm4_1.get_variable();
        let funct3_var = funct3.get_variable();
        let rs1_low_var = rs1_low.get_variable().unwrap();

        let rs1_high_var = rs1_high.get_variable();
        let rs2_high_var = rs2_high.get_variable();
        let imm10_5_var = imm10_5.get_variable();
        let sign_bit_var = sign_bit.get_variable().unwrap();

        // Assign witnesses by slicing low/high 16-bit halves of the instruction.
        // We take care to only materialize the small chunks we need, leaving
        // imm11, rs2_low to be reconstructed as linear constraints.
        let value_fn = move |placer: &mut CS::WitnessPlacer| {
            use crate::cs::witness_placer::*;

            let mut low_word = placer.get_u16(input[0]);
            let mut high_word = placer.get_u16(input[1]);

            let opcode = low_word.get_lowest_bits(7);
            // skip imm11
            low_word = low_word.shr(8);
            let imm4_1 = low_word.get_lowest_bits(4);
            low_word = low_word.shr(4);
            let funct3 = low_word.get_lowest_bits(3);
            low_word = low_word.shr(3);
            let rs1_low = low_word.get_bit(0);

            let rs1_high = high_word.get_lowest_bits(4);
            // skip rs2_low
            high_word = high_word.shr(5);
            let rs2_high = high_word.get_lowest_bits(4);
            high_word = high_word.shr(4);
            let imm10_5 = high_word.get_lowest_bits(6);
            high_word = high_word.shr(6);
            let sign_bit = high_word.get_bit(0);

            placer.assign_u16(opcode_var, &opcode);
            placer.assign_u16(imm4_1_var, &imm4_1);
            placer.assign_u16(funct3_var, &funct3);
            placer.assign_mask(rs1_low_var, &rs1_low);

            placer.assign_u16(rs1_high_var, &rs1_high);
            placer.assign_u16(rs2_high_var, &rs2_high);
            placer.assign_u16(imm10_5_var, &imm10_5);
            placer.assign_mask(sign_bit_var, &sign_bit);
        };

        circuit.set_values(value_fn);

        // range check decomposition pieces
        // 两个固定表检查小字段宽度

        // 保证imm4_1, rs1_high, rs2_high 都在 0..15
        circuit.enforce_lookup_tuple_for_fixed_table(
            &[
                imm4_1.get_variable(),
                rs1_high.get_variable(),
                rs2_high.get_variable(),
            ]
            .map(|el| LookupInput::from(el)),
            TableType::QuickDecodeDecompositionCheck4x4x4,
            false,
        );

        // 保证opcode 在 0..127
        // funct3 在 0..7
        // imm10_5 在 0..63
        circuit.enforce_lookup_tuple_for_fixed_table(
            &[
                opcode.get_variable(),
                funct3.get_variable(),
                imm10_5.get_variable(),
            ]
            .map(|el| LookupInput::from(el)),
            TableType::QuickDecodeDecompositionCheck7x3x6,
            false,
        );

        // insn_low <=> opcode [6:0], imm11: [7], imm[4-1]: [11:8], func3: [14:12], rs1_low: [15],
        let [low_insn, high_insn] = inputs.instruction.get_terms();

        // 源码从 low16 反推 imm11
        // low16 =
        //     opcode
        //     + 2^7  * imm11
        //     + 2^8  * imm4_1
        //     + 2^12 * funct3
        //     + 2^15 * rs1_low
        let mut imm11_constraint = {
            low_insn
                - Term::from(opcode)
                - Term::from(1 << 8) * Term::from(imm4_1)
                - Term::from(1 << 12) * Term::from(funct3)
                - Term::from(rs1_low) * Term::from(1 << 15)
        };
        // imm11_constraint 就是把等式移项后除以 2^7 得到的 bit。
        imm11_constraint.scale(F::from_u64_unchecked(1 << 7).inverse().unwrap());
        // imm11 * (imm11 - 1) = 0，布尔约束只允许 imm11 为 0 或 1。
        circuit
            .add_constraint(imm11_constraint.clone() * (imm11_constraint.clone() - Term::from(1)));

        // insn_high <=> rs1_high: [19:16], rs2: [24:20], imm[10-5]: [30:25], imm12: [31]

        // high16 也按同样方式重构：
        // high16 =
        //   rs1_high
        //   + 2^4  * rs2_low
        //   + 2^5  * rs2_high
        //   + 2^9  * imm10_5
        //   + 2^15 * sign_bit
        let mut rs2_low_constraint = {
            high_insn
                - Term::from(rs1_high)
                - Term::from(rs2_high) * Term::from(1 << 5)
                - Term::from(imm10_5) * Term::from(1 << 9)
                - Term::from(sign_bit) * Term::from(1 << 15)
        };
        // 通过移项除以 2^4 得到 rs2_low_constraint，并加布尔约束rs2_low * (rs2_low - 1) = 0
        rs2_low_constraint.scale(F::from_u64_unchecked(1 << 4).inverse().unwrap());
        circuit.add_constraint(
            rs2_low_constraint.clone() * (rs2_low_constraint.clone() - Term::from(1)),
        );

        // imm11 and rs2_low constraint are linear
        assert_eq!(imm11_constraint.degree(), 1);
        assert_eq!(rs2_low_constraint.degree(), 1);

        // We do NOT need rd as variable, because it'll be merged in the write into explicit variable,
        // so we can drag it along as linear constraint

        // same for rs2, but not for rs1

        // 把小字段拼成 decoder 输出字段。这里的字段名仍然复用了立即数命名。对 R-type，这些 bit 的语义是 funct7；对 I/S/B/J/U，它们又被立即数构造逻辑使用。
        let rs1 = circuit.add_variable_from_constraint_allow_explicit_linear(
            Term::from(rs1_high) * Term::from(1 << 1) + Term::from(rs1_low),
        );
        let rs2_constraint = Term::from(rs2_high) * Term::from(1 << 1) + rs2_low_constraint.clone();
        // rd 这里容易看错。imm4_1 是 low16 的 bit 8..11；对 R-type，rd 的 bit 1..4 正好位于这四个 bit。
        // imm11 是 low16 的 bit 7；对 R-type，它正好是 rd 的 bit 0。
        // 因此：rd = imm11 + 2 * imm4_1
        let rd_constraint = Term::from(imm4_1) * Term::from(1 << 1) + imm11_constraint.clone();

        // funct_7 = sign_bit[1] | imm_10-5[6]
        // funct7 则由 high16 的 bit 9..15 组成：funct7 = imm10_5 + 2^6 * sign_bit
        let funct7_constraint = Term::from(sign_bit) * Term::from(1 << 6) + Term::from(imm10_5);

        // now we can feed [opcode || funct_3 || funct 7] (all are range checked, so concatenation IS allowed)
        // to get basic bitmask that will tell whether the opcode is valid or not, and provide aux properties
        // like belonging to opcode family, etc
        let (
            is_invalid,
            [r_insn, i_insn, s_insn, b_insn, u_insn, j_insn],
            opcode_type_and_variant_bits,
        ) = Self::opcode_lookup::<F, CS>(
            opcode,
            funct3,
            funct7_constraint.clone(),
            circuit,
            splitting,
        );

        // now we need to construct the right constant from different constant chunks
        // the actual constant is dependent on the opcode type:
        // -------------------------------------------------------------------------------------------------------|
        // |       chunk5[31-16]    |   chunk4[15-12]   | chunk3[11] | chunk2[10-5] | chunk1[4-1] | chunk0[0] |   |
        // |========================|===================|============|==============|=============|===========|===|
        // |         sign_bit       |    sign_bit       |  sign_bit  |   imm[10-5]  |   rs2_high  |  rs2_low  | I |
        // |------------------------|-------------------|------------|--------------|-------------|-----------|---|
        // |         sign_bit       |    sign_bit       |  sign_bit  |   imm[10-5]  |   imm4_1    |   imm11   | S |
        // |------------------------|-------------------|------------|--------------|-------------|-----------|---|
        // |         sign_bit       |    sign_bit       |   imm11    |   imm[10-5]  |   imm4_1    |     0     | B |
        // |------------------------|-------------------|------------|--------------|-------------|-----------|---|
        // |         insn_high      | rs1_low || funct3 |      0     |      0       |      0      |     0     | U |
        // |------------------------|-------------------|------------|--------------|-------------|-----------|---|
        // |  sign_bit || rs1_high  | rs1_low || funct3 |  rs2_low   |   imm[10-5]  |   rs2_high  |     0     | J |
        // |========================|===================|============|==============|=============|===========|===|
        // hence:
        // chunk0 = i_insn * rs2_low +  s_insn * imm11
        // chunk1 = (i_insn + j_insn) * rs2_high + (s_insn + b_insn) * imm4_1
        // chunk2 = (1 - u_insn) * imm10_5
        // chunk3 = (i_insn + s_insn) * sign_bit + b_insn * imm11 + j_insn * rs2_low
        // chunk4 = (i_insn + s_insn + b_insn) * sign_bit * 0b1111 + (u_insn + j_insn) * (rs1_low << 3 + funct3)
        // chunk5 = {
        //      j_insn * (sign_bit * 0xfff0 + rs1_high) + u_insn * insn_high +
        //      (1 - j_insn - u_insn) * sign_bit * 0xffff
        // }

        // chunks 0..4 are used for linear constraint later on to form imm_low
        let chunks_defining_constraints: [Constraint<F>; 5] = [
            // 0
            Term::from(i_insn) * rs2_low_constraint.clone()
                + Term::from(s_insn) * imm11_constraint.clone(),
            // 1
            (Term::from(i_insn) + Term::from(j_insn)) * Term::from(rs2_high)
                + (Term::from(s_insn) + Term::from(b_insn)) * Term::from(imm4_1),
            // 2
            (Term::from(1) - Term::from(u_insn)) * Term::from(imm10_5),
            // 3
            (Term::from(i_insn) + Term::from(s_insn)) * Term::from(sign_bit)
                + Term::from(b_insn) * imm11_constraint
                + Term::from(j_insn) * rs2_low_constraint.clone(),
            // 4
            (Term::from(i_insn) + Term::from(s_insn) + Term::from(b_insn))
                * Term::from(sign_bit)
                * Term::from(0b1111u64)
                + (Term::from(u_insn) + Term::from(j_insn))
                    * (Term::from(rs1_low) * Term::from(1 << 3) + (Term::from(funct3))),
        ];

        let [chunk0, chunk1, chunk2, chunk3, chunk4] = chunks_defining_constraints;

        let imm_low = Num::Var(circuit.add_variable_from_constraint(
            chunk0
                + chunk1 * Term::from(1 << 1)
                + chunk2 * Term::from(1 << 5)
                + chunk3 * Term::from(1 << 11)
                + chunk4 * Term::from(1 << 12),
        ));

        // chunk 5 is just higher part of the immediate
        // This encodes sign-extension for all formats. For U format we take insn_high entirely.
        let imm_high = Num::Var(circuit.add_variable_from_constraint(
            Term::from(j_insn) * (Term::from(sign_bit) * Term::from(0xfff0) + Term::from(rs1_high))
                + Term::from(u_insn) * Term::from(inputs.instruction.0[1])
                + (Term::from(1) - Term::from(j_insn) - Term::from(u_insn))
                    * Term::from(sign_bit)
                    * Term::from(0xffff),
        ));

        let imm = Register([imm_low, imm_high]);

        // funct_12 is used only by:
        // SYSTEM CSR - there we can use single table lookup to validate if 12-bit index is valid and trap (along with R/W info if we want)
        // SYSTEM ECALL/EBREAK - again, we can check validity in there, because if it's not a valid 12-bit index we will trap anyway, but with different code

        // funct_12 = sign_bit[1] | imm_10-5[6] | rs2_high[4] | rs2_low[1]
        // funct12 用在 SYSTEM、CSR、ECALL、EBREAK 这类指令。源码把 funct7 和 rs2 拼成 12-bit
        // 普通 ADD 不使用 funct12。CSR 指令会用它表示 CSR index 或 system 指令的 12-bit 编码。
        let funct12_constraint =
            rs2_constraint.clone() + (funct7_constraint.clone() * Term::from(1 << 5));

        let decoder_output = OptimizedDecoderOutput {
            rs1: Num::Var(rs1), //rs1 是 Num，因为 slot0 寄存器读取马上需要一个显式变量作为地址。
            // rs2、rd、funct7、funct12 保持 Constraint，是因为后续代码可以把这些线性表达式直接放进约束或再 materialize，减少不必要的变量。
            rs2: rs2_constraint,
            rd: rd_constraint,
            funct3,
            funct7: funct7_constraint,
            funct12: funct12_constraint,
            imm,
        };

        (
            is_invalid,
            decoder_output,
            [r_insn, i_insn, s_insn, b_insn, u_insn, j_insn],
            opcode_type_and_variant_bits,
        )
    }

    #[track_caller]
    /// Perform a table lookup for [opcode | funct3 | funct7] and expose the resulting bitmask as boolean variables.
    /// The first bit indicates invalid opcode, the next NUM_INSTRUCTION_TYPES bits are the orthogonal format flags [R,I,S,B,U,J],
    /// and the remaining bits are variant selected by splitting.
    /// The splitting is realized by connecting two linear constraints to fixed-table lookups that return two 16-bit masks.
    /// We then expose the requested number of bits from those masks as booleans.
    fn opcode_lookup<F: PrimeField, CS: Circuit<F>>(
        opcode: Num<F>,
        funct3: Num<F>,
        funct7: Constraint<F>,
        circuit: &mut CS,
        splitting: [usize; 2],
    ) -> (
        Boolean,
        [Boolean; NUM_INSTRUCTION_TYPES_IN_DECODE_BITS],
        Vec<Boolean>,
    ) {
        // 先把 opcode、funct3、funct7 拼成一个线性表达式
        // table_input =
        //     opcode
        //     + 2^7  * funct3
        //     + 2^10 * funct7
        let table_input_constraint = Constraint::empty()
            + Term::from(opcode)
            + Term::from(funct3) * Term::from(1 << 7)
            + (funct7 * Term::from(1 << (7 + 3)));

        // here we will merge bit decomposition AND splitting, by putting linear constraints into everything

        // 分配所有 decoder 输出 bit。每个 bit 是 Boolean，`add_boolean_variable` 会让后续电路带上：
        // bit * (bit - 1) = 0
        // splitting_constraint_0 和 splitting_constraint_1 是把这些 Boolean 重新拼回两个整数：
        // splitting_constraint_0 = bit0 + 2*bit1 + 4*bit2 + ...
        // splitting_constraint_1 = 后半段 bitmask 的同样拼法
        // 这两个表达式稍后会作为 OpTypeBitmask lookup 的两个输出列。也就是说，lookup 约束检查的是：
        // (table_input, splitting_constraint_0, splitting_constraint_1)
        // 这三项必须出现在 OpTypeBitmask 固定表里。
        let mut all_bits = Vec::with_capacity(splitting[0] + splitting[1]);

        let mut splitting_constraint_0 = Constraint::<F>::empty();
        for i in 0..splitting[0] {
            let bit = circuit.add_boolean_variable();
            splitting_constraint_0 = splitting_constraint_0 + Term::from(1 << i) * bit.get_terms();
            all_bits.push(bit);
        }

        let mut splitting_constraint_1 = Constraint::<F>::empty();
        for i in 0..splitting[1] {
            let bit = circuit.add_boolean_variable();
            splitting_constraint_1 = splitting_constraint_1 + Term::from(1 << i) * bit.get_terms();
            all_bits.push(bit);
        }

        {
            let (quadratic, linear_terms, constant_coeff) =
                table_input_constraint.clone().split_max_quadratic();
            assert!(quadratic.is_empty());
            assert_eq!(constant_coeff, F::ZERO);

            // not push the splitting data

            const NUM_INPUT_VARS: usize = 4;
            assert_eq!(linear_terms.len(), NUM_INPUT_VARS);

            let outputs: Vec<_> = all_bits
                .iter()
                .map(|el| el.get_variable().unwrap())
                .collect();

            // 这段只定义 witness 怎样填值。prover 生成 witness 时，它先用 opcode/funct3/funct7 算出 table_input，
            // 再查 table_driver 里的 OpTypeBitmask 表，得到两个 bitmask word，最后逐 bit 写入 all_bits 对应的 Boolean 变量。
            let value_fn = move |placer: &mut CS::WitnessPlacer| {
                use crate::cs::witness_placer::*;

                // Step 1: 用 witness 里已有的 opcode/funct3/funct7 算出 table_input 的数值
                let mut result = <CS::WitnessPlacer as WitnessTypeSet<F>>::Field::constant(F::ZERO);

                // 即OpTypeBitmask表的key = opcode + 128*funct3 + 1024*funct7
                for (coeff, var) in linear_terms.iter() {
                    let coeff = <CS::WitnessPlacer as WitnessTypeSet<F>>::Field::constant(*coeff);
                    let value = placer.get_field(*var);
                    result.add_assign_product(&coeff, &value);
                }

                let [splitting_0, splitting_1] = splitting;

                assert!(splitting_0 <= F::CHAR_BITS - 1);
                assert!(splitting_1 <= F::CHAR_BITS - 1);

                // Step 2: 在 TableDriver 的 OpTypeBitmask 表里查 key
                let table_id = <CS::WitnessPlacer as WitnessTypeSet<F>>::U16::constant(
                    TableType::OpTypeBitmask.to_table_id() as u16,
                );
                // 得到两个 field 元素，各承载 bitmask 的一半
                let [bitmask_0, bitmask_1] = placer.lookup::<1, 2>(&[result], &table_id);
                let bitmask_0 = bitmask_0.as_integer();
                let bitmask_1 = bitmask_1.as_integer();

                // Step 3: 把 bitmask_0 的每一位写到 all_bits 前 splitting[0] 个变量
                for i in 0..splitting_0 {
                    let bit = bitmask_0.get_bit(i as u32);
                    // outputs[0]=is_invalid, [1]=r_insn ...
                    placer.assign_mask(outputs[i], &bit);
                }

                // Step 4: 把 bitmask_1 的每一位写到 all_bits 后半段
                for i in 0..splitting_1 {
                    let bit = bitmask_1.get_bit(i as u32);
                    placer.assign_mask(outputs[splitting_0 + i], &bit);
                }
            };

            circuit.set_values(value_fn);
        }

        circuit.enforce_lookup_tuple_for_fixed_table(
            &[
                LookupInput::from(table_input_constraint),
                LookupInput::from(splitting_constraint_0),
                LookupInput::from(splitting_constraint_1),
            ],
            TableType::OpTypeBitmask,
            true, // we used lookup in the query above, so we record a relation,
                  // but do not need to auto-generate multiplicity counting function
        );

        assert!(all_bits.len() >= 1 + NUM_INSTRUCTION_TYPES);

        let is_invalid = all_bits[0];

        let format_bits: [Boolean; NUM_INSTRUCTION_TYPES_IN_DECODE_BITS] =
            all_bits[1..][..NUM_INSTRUCTION_TYPES].try_into().unwrap();
        let other_bits = all_bits[1..][NUM_INSTRUCTION_TYPES_IN_DECODE_BITS..].to_vec();

        (is_invalid, format_bits, other_bits)
    }

    /// Choose source operands based on the decoded instruction format and return a
    /// linear flag indicating whether the destination register should be
    /// written in the current instruction.
    pub fn select_src1_and_src2_values<F: PrimeField, C: Circuit<F>>(
        cs: &mut C,
        opcode_format_bits: &[Boolean; NUM_INSTRUCTION_TYPES],
        rs1_value: Register<F>,
        decoded_imm: Register<F>,
        rs2_value: Register<F>,
    ) -> (Register<F>, Register<F>, Constraint<F>) {
        let [r_insn, i_insn, s_insn, b_insn, u_insn, j_insn] = *opcode_format_bits;
        // R, I, S, B instruction formats use RS1 value as the first operand,
        // otherwise we do not need to put anything anything there - U can access IMM from the decoder directly,
        // same as J format

        // So we do NOT select src1, and assume that opcodes that do not need to use it will not access it
        let src1 = rs1_value;

        // R, S and B use RS2 value as second operand, otherwise - I format supplies immediate
        // We do R/I mixing here to save on register value decomposition for instructions
        // such as ADD/ADDI or XOR/XORI
        let src2 = Register::choose_from_orthogonal_variants(
            cs,
            &[r_insn, i_insn, s_insn, b_insn],
            &[rs2_value, decoded_imm, rs2_value, rs2_value],
        );

        // opcode formats are orthogonal flags, so a boolean to update RD is just a linear combination
        let update_rd = Constraint::from(r_insn.get_variable().unwrap())
            + Constraint::from(i_insn.get_variable().unwrap())
            + Constraint::from(j_insn.get_variable().unwrap())
            + Constraint::from(u_insn.get_variable().unwrap());

        (src1, src2, update_rd)
    }
}

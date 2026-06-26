use super::*;

pub const ADD_OP_KEY: DecoderMajorInstructionFamilyKey =
    DecoderMajorInstructionFamilyKey("ADD/ADDI");
pub const SUB_OP_KEY: DecoderMajorInstructionFamilyKey =
    DecoderMajorInstructionFamilyKey("SUB/SUBI");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// ADD和ADDI共用的opcode family。
///
/// decoder已经在更早阶段把ADD和ADDI统一到ADD_OP_KEY下，
/// 并把第二个操作数整理成src2：
/// ADD时src2来自rs2；
/// ADDI时src2来自立即数。
pub struct AddOp;

impl DecodableMachineOp for AddOp {
    /// 因此，当表生成器遇到：
    /// opcode = OPERATION_OP
    /// funct3 = 000
    /// funct7 = 0000000
    /// 它把这一行标成：
    /// valid
    /// format = RType
    /// major family = ADD_OP_KEY
    /// 若没有任何 supported opcode 匹配某个 key，produce_decoder_table_stub 保留 basic_invalid_bitmask；opcode_lookup 返回的 is_invalid 就会是 1。
    fn define_decoder_subspace(
        &self,
        opcode: u8,
        func3: u8,
        func7: u8,
    ) -> Result<
        (
            InstructionType,
            DecoderMajorInstructionFamilyKey,
            &'static [DecoderInstructionVariantsKey],
        ),
        (),
    > {
        let params = match (opcode, func3, func7) {
            (OPERATION_OP, 0b000, 0b000_0000) => {
                // ADD
                (InstructionType::RType, ADD_OP_KEY, &[][..])
            }
            (OPERATION_OP_IMM, 0b000, _) => {
                // ADDI
                (InstructionType::IType, ADD_OP_KEY, &[][..])
            }

            _ => return Err(()),
        };

        Ok(params)
    }
}

impl<
        F: PrimeField,
        ST: BaseMachineState<F>,
        RS: RegisterValueSource<F>,
        DE: DecoderOutputSource<F, RS>,
        BS: IndexableBooleanSet,
    > MachineOp<F, ST, RS, DE, BS> for AddOp
{
    /// 把ADD语义登记进OptimizationContext，并返回候选rd值。
    ///
    /// 这里不直接生成最终Constraint，而是先调用append_add_relation把关系暂存起来，
    /// 稍后由opt_ctx.enforce_all统一落成低16位和高16位约束。
    fn apply<
        CS: Circuit<F>,
        const ASSUME_TRUSTED_CODE: bool,
        const OUTPUT_EXACT_EXCEPTIONS: bool,
    >(
        cs: &mut CS,
        _machine_state: &ST,
        inputs: &DE,
        boolean_set: &BS,
        opt_ctx: &mut OptimizationContext<F, CS>,
    ) -> CommonDiffs<F> {
        // reset_indexers：本 family 若多次用 opt_ctx，先重置内部索引（AddOp 只登记一条加法）。
        opt_ctx.reset_indexers();
        // 只有当前行decoder确定属于ADD/ADDI family时，这些关系才会生效。
        // decoder 在 4.9 已判定本行是否为 ADD/ADDI；ADD 行 is_add=1。
        let exec_flag = boolean_set.get_major_flag(ADD_OP_KEY);

        // decoder已经把ADD和ADDI都统一成src1 + src2的形式。
        // 来自 4.10/4.11 的寄存器 limb 变量
        let src1 = inputs.get_rs1_or_equivalent().get_register();
        let src2 = inputs.get_rs2_or_equivalent().get_register();

        // 把加法关系登记给OptimizationContext，返回结果寄存器和溢出flag。
        // append_add_relation：登记 a+b=res，分配 res 的两个新 Variable。此时还没有 cs.add_constraint。
        let (res, _of_flag) = opt_ctx.append_add_relation(src1, src2, exec_flag, cs);

        if exec_flag.get_value(cs).unwrap_or(false) {
            println!("ADD");
            dbg!(src1.get_value_signed(cs));
            dbg!(src2.get_value_signed(cs));
            dbg!(res.get_value_signed(cs));
        }

        // CommonDiffs里保存的是“候选rd值”，writeback阶段再决定它是否成为最终写回结果。
        let returned_value = [
            Constraint::<F>::from(res.0[0].get_variable()),
            Constraint::<F>::from(res.0[1].get_variable()),
        ];

        CommonDiffs {
            exec_flag,
            trapped: None,
            trap_reason: None,
            rd_value: vec![(returned_value, exec_flag)], //把 res 的两个 limb 包成 Constraint，和 exec_flag 配对，供 writeback 选择。
            new_pc_value: NextPcValue::Default, //new_pc_value::Default：ADD 不改 pc，默认 pc+4
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubOp;

impl DecodableMachineOp for SubOp {
    fn define_decoder_subspace(
        &self,
        opcode: u8,
        func3: u8,
        func7: u8,
    ) -> Result<
        (
            InstructionType,
            DecoderMajorInstructionFamilyKey,
            &'static [DecoderInstructionVariantsKey],
        ),
        (),
    > {
        let params = match (opcode, func3, func7) {
            (OPERATION_OP, 0b000, 0b010_0000) => {
                // SUB
                (InstructionType::RType, SUB_OP_KEY, &[][..])
            }
            _ => return Err(()),
        };

        Ok(params)
    }
}

impl<
        F: PrimeField,
        ST: BaseMachineState<F>,
        RS: RegisterValueSource<F>,
        DE: DecoderOutputSource<F, RS>,
        BS: IndexableBooleanSet,
    > MachineOp<F, ST, RS, DE, BS> for SubOp
{
    fn apply<
        CS: Circuit<F>,
        const ASSUME_TRUSTED_CODE: bool,
        const OUTPUT_EXACT_EXCEPTIONS: bool,
    >(
        cs: &mut CS,
        _machine_state: &ST,
        inputs: &DE,
        boolean_set: &BS,
        opt_ctx: &mut OptimizationContext<F, CS>,
    ) -> CommonDiffs<F> {
        opt_ctx.reset_indexers();
        let exec_flag = boolean_set.get_major_flag(SUB_OP_KEY);

        let src1 = inputs.get_rs1_or_equivalent().get_register();
        let src2 = inputs.get_rs2_or_equivalent().get_register();

        let (res, _uf_flag) = opt_ctx.append_sub_relation(src1, src2, exec_flag, cs);

        if exec_flag.get_value(cs).unwrap_or(false) {
            println!("SUB");
            dbg!(src1.get_value_signed(cs));
            dbg!(src2.get_value_signed(cs));
            dbg!(res.get_value_signed(cs));
        }

        let returned_value = [
            Constraint::<F>::from(res.0[0].get_variable()),
            Constraint::<F>::from(res.0[1].get_variable()),
        ];

        CommonDiffs {
            exec_flag,
            trapped: None,
            trap_reason: None,
            rd_value: vec![(returned_value, exec_flag)],
            new_pc_value: NextPcValue::Default,
        }
    }
}

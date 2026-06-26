use super::ops::*;
use super::*;
// use crate::machine::machine_configurations::full_isa_no_exceptions::basic_state_transition::base_isa_state_transition;
use crate::machine::machine_configurations::full_isa_no_exceptions::optimized_state_transition::optimized_base_isa_state_transition;
use crate::machine::machine_configurations::minimal_state::MinimalStateRegistersInMemory;

type ST<F> = MinimalStateRegistersInMemory<F>;
type BS = BasicFlagsSource;

type RS<F> = RegisterDecompositionWithSign<F>;
type DE<F> = BasicDecodingResultWithSigns<F>;

#[derive(Clone, Copy, Debug, Default)]
/// full ISA加delegation CSR calls，不包含exception handling logic。
/// 证明的是“正常执行路径”。非法opcode、未对齐访问等不会通过异常分支被证明成合法执行；更直接地说，这类行为会导致约束无法满足或程序不在支持范围里。
pub struct FullIsaMachineWithDelegationNoExceptionHandling;

impl<F: PrimeField> Machine<F> for FullIsaMachineWithDelegationNoExceptionHandling {
    const ASSUME_TRUSTED_CODE: bool = true;
    const OUTPUT_EXACT_EXCEPTIONS: bool = false;
    const USE_ROM_FOR_BYTECODE: bool = true;

    type State = MinimalStateRegistersInMemory<F>;

    fn all_supported_opcodes() -> Vec<Box<dyn DecodableMachineOp>> {
        vec![
            Box::new(AddOp),
            Box::new(SubOp),
            Box::new(LuiOp),
            Box::new(AuiPc),
            Box::new(BinaryOp),
            Box::new(MulOp::<true>),
            Box::new(DivRemOp::<true>),
            Box::new(ConditionalOp::<true>),
            Box::new(ShiftOp::<true, false>),
            Box::new(JumpOp),
            Box::new(LoadOp::<true, true>),
            Box::new(StoreOp::<true>),
            Box::new(CsrOp::<false, false, false>),
        ]
    }

    /// 汇总这台machine在compile/setup阶段需要注册的固定表。
    ///
    /// 每个 `MachineOp` 自己声明它依赖的 `TableType`，
    /// 这里把当前配置里启用的所有 opcode family 的表需求做并集。
    ///
    /// 注意这里返回的是“机器级依赖”：
    /// - 不是执行顺序；
    /// - 不是某一行一定会访问的表；
    /// - 而是后续 `create_table_driver*()` 必须提前 materialize 的 lookup/helper tables。
    fn define_used_tables() -> BTreeSet<TableType> {
        // `BTreeSet` 做两件事：
        // 1. 去重：多个opcode family可能共用同一张表；
        // 2. 稳定顺序：后续构建table driver时遍历顺序固定。
        let mut set = BTreeSet::new();

        // 这些基础算术/立即数操作目前不额外声明 lookup 表。
        // 仍然保留统一写法，让这里的机器配置和 `all_supported_opcodes()` 一一对应。
        set.extend(<AddOp as MachineOp<F, ST<F>, RS<F>, DE<F>, BS>>::define_used_tables());
        set.extend(<SubOp as MachineOp<F, ST<F>, RS<F>, DE<F>, BS>>::define_used_tables());
        set.extend(<LuiOp as MachineOp<F, ST<F>, RS<F>, DE<F>, BS>>::define_used_tables());
        set.extend(<AuiPc as MachineOp<F, ST<F>, RS<F>, DE<F>, BS>>::define_used_tables());

        // 位运算会引入 And / Or / Xor 等标准表。
        set.extend(<BinaryOp as MachineOp<F, ST<F>, RS<F>, DE<F>, BS>>::define_used_tables());

        // 有符号乘除在当前实现里不额外声明标准 lookup 表，
        // 但仍然通过统一入口保留机器配置的完整性。
        set.extend(<MulOp<true> as MachineOp<F, ST<F>, RS<F>, DE<F>, BS>>::define_used_tables());
        set.extend(<DivRemOp<true> as MachineOp<F, ST<F>, RS<F>, DE<F>, BS>>::define_used_tables());

        // 条件分支、移位、跳转会用到条件解析/offset 清理/shift 实现等辅助表。
        set.extend(<ConditionalOp<true> as MachineOp<
            F,
            ST<F>,
            RS<F>,
            DE<F>,
            BS,
        >>::define_used_tables());
        set.extend(<ShiftOp<true, false> as MachineOp<
            F,
            ST<F>,
            RS<F>,
            DE<F>,
            BS,
        >>::define_used_tables());
        set.extend(<JumpOp as MachineOp<F, ST<F>, RS<F>, DE<F>, BS>>::define_used_tables());

        // load / store 会引入地址低位分解、符号扩展、字节写回贡献等内存相关表。
        set.extend(<LoadOp<true, true> as MachineOp<
            F,
            ST<F>,
            RS<F>,
            DE<F>,
            BS,
        >>::define_used_tables());
        set.extend(<StoreOp<true> as MachineOp<F, ST<F>, RS<F>, DE<F>, BS>>::define_used_tables());

        // CSR delegation 路径会访问 `SpecialCSRProperties`，
        // 但这张表当前由外层单独提供，而不是在这里通过 `used_tables` 自动materialize。
        // 因此先保留注释，避免和外层手工注入的CSR表来源冲突。
        // set.extend(<CsrOp::<false, false> as MachineOp::<F, ST<F>, RS<F>, DE<F>, BS>>::define_used_tables());

        set
    }

    /// main RISC-V machine的状态转移入口。
    ///
    /// 这里先准备decoder表划分和布尔key，
    /// 再转入optimized_base_isa_state_transition去实际写出一行CPU状态转移。
    fn describe_state_transition<CS: Circuit<F>, const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize>(
        cs: &mut CS,
    ) -> (Self::State, Self::State)
    where
        [(); { <Self as Machine<F>>::ASSUME_TRUSTED_CODE } as usize]:,
        [(); { <Self as Machine<F>>::OUTPUT_EXACT_EXCEPTIONS } as usize]:,
    {
        // decoder表会把instruction编码拆成格式位、major family flag和其他辅助位。
        let (splitting, _) = <Self as Machine<F>>::produce_decoder_table_stub();
        let boolean_keys = <Self as Machine<F>>::all_decoder_keys();

        // 注意：它内部有硬编码的 ISA mods，因此要么需要传播更多配置，要么必须使用另一种形式的函数

        optimized_base_isa_state_transition::<
            F,
            CS,
            { <Self as Machine<F>>::ASSUME_TRUSTED_CODE },
            { <Self as Machine<F>>::OUTPUT_EXACT_EXCEPTIONS },
            true,
            true,
            ROM_ADDRESS_SPACE_SECOND_WORD_BITS,
        >(
            cs,
            // <Self::State as BaseMachineState<F>>::opcodes_are_in_rom(),
            splitting,
            boolean_keys,
        )
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::utils::serialize_to_file;
    use field::Mersenne31Field;

    const SECOND_WORD_BITS: usize = 4;

    #[test]
    fn compile_full_machine_with_delegation() {
        let machine = FullIsaMachineWithDelegationNoExceptionHandling;
        let rom_table = create_table_for_rom_image::<_, SECOND_WORD_BITS>(
            &[],
            TableType::RomRead.to_table_id(),
        );
        let csr_table = create_csr_table_for_delegation(
            true,
            &[1991],
            TableType::SpecialCSRProperties.to_table_id(),
        );

        let compiled =
            default_compile_machine::<_, SECOND_WORD_BITS>(machine, rom_table, Some(csr_table), 20);
        serialize_to_file(&compiled, "full_machine_with_delegation_layout.json");
    }

    #[test]
    fn full_machine_with_delegation_get_witness_graph() {
        let machine = FullIsaMachineWithDelegationNoExceptionHandling;

        let ssa_forms = dump_ssa_witness_eval_form::<Mersenne31Field, _, SECOND_WORD_BITS>(machine);
        serialize_to_file(&ssa_forms, "full_machine_with_delegation_ssa.json");
    }
}

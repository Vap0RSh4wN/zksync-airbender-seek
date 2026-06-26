use super::scalar_witness_type_set::ScalarWitnessTypeSet;
use super::*;
use crate::cs::oracle::Oracle;
use crate::definitions::Variable;
use crate::tables::TableDriver;
use field::PrimeField;

use super::WitnessPlacer;

///CSDebugWitnessEvaluator是WitnessPlacer的一种实现。它用于调试。它会在电路构造时立刻执行set_values登记的计算，维护一份变量编号到字段值的数组。
pub struct CSDebugWitnessEvaluator<F: PrimeField> {
    ///保存变量值
    pub(crate) values: Vec<F>,
    ///oracle提供placeholder对应的外部witness
    pub oracle: Option<Box<dyn Oracle<F>>>,
    ///table_driver用于查lookup表
    pub(crate) table_driver: TableDriver<F>,
}

impl<F: PrimeField> CSDebugWitnessEvaluator<F> {
    /// 创建空values、空oracle、空TableDriver
    pub fn new() -> Self {
        Self {
            values: Vec::new(),
            oracle: None,
            table_driver: TableDriver::new(),
        }
    }

    pub fn new_with_oracle<O: Oracle<F> + 'static>(oracle: O) -> Self {
        let mut new = Self::new();
        new.oracle = Some(Box::new(oracle));

        new
    }

    pub fn get_value(&self, variable: Variable) -> Option<F> {
        if variable.is_placeholder() {
            panic!("variable is placeholder");
        }
        let idx = variable.0 as usize;
        if idx >= self.values.len() {
            None
        } else {
            Some(self.values[idx])
        }
    }

    pub fn evaluate(&mut self, node: &impl WitnessResolutionDescription<F, Self>) {
        node.evaluate(self);
    }

    pub fn resolve_placeholder(
        &mut self,
        variable: Variable,
        placeholder: Placeholder,
        subindex: usize,
    ) {
        if variable.is_placeholder() {
            panic!("variable is placeholder");
        }
        if let Some(oracle) = self.oracle.as_ref() {
            let value = oracle.get_witness_from_placeholder(placeholder, subindex, 0);
            let idx = variable.0 as usize;
            if idx >= self.values.len() {
                self.values.resize(idx + 1, F::ZERO);
            }
            self.values[idx] = value;
        }
    }
}

impl<F: PrimeField> WitnessTypeSet<F> for CSDebugWitnessEvaluator<F> {
    const CAN_BRANCH: bool = <ScalarWitnessTypeSet<F, true> as WitnessTypeSet<F>>::CAN_BRANCH;
    const MERGE_LOOKUP_AND_MULTIPLICITY_COUNT: bool = true;

    type Mask = <ScalarWitnessTypeSet<F, true> as WitnessTypeSet<F>>::Mask;
    type Field = <ScalarWitnessTypeSet<F, true> as WitnessTypeSet<F>>::Field;
    type I32 = <ScalarWitnessTypeSet<F, true> as WitnessTypeSet<F>>::I32;
    type U32 = <ScalarWitnessTypeSet<F, true> as WitnessTypeSet<F>>::U32;
    type U16 = <ScalarWitnessTypeSet<F, true> as WitnessTypeSet<F>>::U16;
    type U8 = <ScalarWitnessTypeSet<F, true> as WitnessTypeSet<F>>::U8;

    #[inline(always)]
    fn branch(mask: &Self::Mask) -> bool {
        *mask
    }
}

impl<F: PrimeField> WitnessPlacer<F> for CSDebugWitnessEvaluator<F> {
    fn record_resolver(&mut self, resolver: impl WitnessResolutionDescription<F, Self>) {
        resolver.evaluate(self);
    }

    fn get_oracle_field(&mut self, placeholder: Placeholder, subindex: usize) -> Self::Field {
        if let Some(oracle) = self.oracle.as_ref() {
            oracle.get_witness_from_placeholder(placeholder, subindex, 0)
        } else {
            F::ZERO
        }
    }

    fn get_oracle_u32(&mut self, placeholder: Placeholder) -> Self::U32 {
        if let Some(oracle) = self.oracle.as_ref() {
            oracle.get_u32_witness_from_placeholder(placeholder, 0)
        } else {
            0
        }
    }

    fn get_oracle_u16(&mut self, placeholder: Placeholder) -> Self::U16 {
        if let Some(oracle) = self.oracle.as_ref() {
            oracle.get_u16_witness_from_placeholder(placeholder, 0)
        } else {
            0
        }
    }

    fn get_oracle_u8(&mut self, placeholder: Placeholder) -> Self::U8 {
        if let Some(oracle) = self.oracle.as_ref() {
            oracle.get_u8_witness_from_placeholder(placeholder, 0)
        } else {
            0
        }
    }

    fn get_oracle_boolean(&mut self, placeholder: Placeholder) -> Self::Mask {
        if let Some(oracle) = self.oracle.as_ref() {
            oracle.get_boolean_witness_from_placeholder(placeholder, 0)
        } else {
            false
        }
    }

    #[track_caller]
    /// 输入是一个 Variable，输出是这个变量当前对应self.values的field 值。注意这里用的是 &mut self，因为它可能会改 self.values 的长度。
    /// 可以把 values 想成一张按变量编号排好的 witness 表：
    /// variable id:  0   1   2   3   4   5
    /// values:      v0  v1  v2  v3  v4  v5
    /// 如果现在来读 Variable(10)，但数组只有 6 个元素，这个函数不会报“没赋值”，而是会先把数组补成：
    /// values: [v0, v1, v2, v3, v4, v5, 0, 0, 0, 0, 0]
    /// 然后返回 values[10] = 0。
    /// 所以它更像：
    /// “确保这个变量槽位存在，然后取它的当前值”
    /// 而不是“严格检查这个变量是否已经被赋值”
    fn get_field(&mut self, variable: Variable) -> Self::Field {
        // 先拒绝 placeholder。
        // 原因是 placeholder 不是从 values[idx] 里取，它应该走 get_oracle_field() 这条路径。
        // 也就是说，这里在强制区分两类数据源：
        // 普通变量：从本地 values 读；
        // 占位输入：从 oracle 读。
        if variable.is_placeholder() {
            panic!("variable is placeholder");
        }
        let idx = variable.0 as usize;
        if idx >= self.values.len() {
            self.values.resize(idx + 1, F::ZERO);
        }
        self.values[idx]
    }

    #[inline(always)]
    fn get_boolean(&mut self, variable: Variable) -> Self::Mask {
        self.get_field(variable).as_boolean()
    }

    #[inline(always)]
    fn get_u16(&mut self, variable: Variable) -> Self::U16 {
        self.get_field(variable).as_u64_reduced() as u16
    }

    #[inline(always)]
    fn get_u8(&mut self, variable: Variable) -> Self::U8 {
        self.get_field(variable).as_u64_reduced() as u8
    }

    #[inline(always)]
    fn assign_mask(&mut self, variable: Variable, value: &Self::Mask) {
        if variable.is_placeholder() {
            panic!("variable is placeholder");
        }
        let idx = variable.0 as usize;
        // TODO 这里每次都要resize，效率很低。
        if idx >= self.values.len() {
            self.values.resize(idx + 1, F::ZERO);
        }
        self.values[idx] = F::from_boolean(*value);
    }

    #[inline(always)]
    fn assign_field(&mut self, variable: Variable, value: &Self::Field) {
        if variable.is_placeholder() {
            panic!("variable is placeholder");
        }
        let idx = variable.0 as usize;
        if idx >= self.values.len() {
            self.values.resize(idx + 1, F::ZERO);
        }
        self.values[idx] = *value;
    }

    #[inline(always)]
    fn assign_u16(&mut self, variable: Variable, value: &Self::U16) {
        self.assign_field(variable, &F::from_u64_unchecked(*value as u64));
    }

    #[inline(always)]
    fn assign_u8(&mut self, variable: Variable, value: &Self::U8) {
        self.assign_field(variable, &F::from_u64_unchecked(*value as u64));
    }

    #[inline(always)]
    fn conditionally_assign_mask(
        &mut self,
        variable: Variable,
        mask: &Self::Mask,
        value: &Self::Mask,
    ) {
        if *mask {
            self.assign_mask(variable, value);
        }
    }

    #[inline(always)]
    fn conditionally_assign_field(
        &mut self,
        variable: Variable,
        mask: &Self::Mask,
        value: &Self::Field,
    ) {
        if *mask {
            self.assign_field(variable, value);
        }
    }

    #[inline(always)]
    fn conditionally_assign_u16(
        &mut self,
        variable: Variable,
        mask: &Self::Mask,
        value: &Self::U16,
    ) {
        if *mask {
            self.assign_u16(variable, value);
        }
    }

    #[inline(always)]
    fn conditionally_assign_u8(&mut self, variable: Variable, mask: &Self::Mask, value: &Self::U8) {
        if *mask {
            self.assign_u8(variable, value);
        }
    }

    #[inline(always)]
    // 这就是它为什么必须有表。只要debug evaluator要执行lookup witness计算，它就需要table_driver里已经有那张表。
    fn lookup<const M: usize, const N: usize>(
        &mut self,
        inputs: &[Self::Field; M],
        table_id: &Self::U16,
    ) -> [Self::Field; N] {
        self.table_driver
            .lookup_values::<N>(inputs, *table_id as u32)
    }

    #[inline(always)]
    fn maybe_lookup<const M: usize, const N: usize>(
        &mut self,
        inputs: &[Self::Field; M],
        table_id: &Self::U16,
        mask: &Self::Mask,
    ) -> [Self::Field; N] {
        if *mask {
            self.lookup(inputs, table_id)
        } else {
            [F::ZERO; N]
        }
    }

    #[inline(always)]
    fn lookup_enforce<const M: usize>(&mut self, inputs: &[Self::Field; M], table_id: &Self::U16) {
        let _ = self
            .table_driver
            .enforce_values_and_get_absolute_index(inputs, *table_id as u32);
    }
}

pub fn witness_early_branch_if_possible<
    F: PrimeField,
    W: WitnessPlacer<F>,
    T: WitnessResolutionDescription<F, W>,
>(
    branch_mask: W::Mask,
    placer: &mut W,
    node: &T,
) {
    if W::CAN_BRANCH {
        if W::branch(&branch_mask) {
            node.evaluate(placer);
        }
    } else {
        // we should use conditional assignment anyway
        node.evaluate(placer);
    }
}

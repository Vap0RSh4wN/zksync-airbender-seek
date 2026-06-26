use super::spec_selection::*;
use super::witness_placer::WitnessPlacer;
use super::witness_placer::WitnessResolutionDescription;
use crate::cs::placeholder::*;
use crate::cs::utils::collapse_max_quadratic_constraint_into;
use crate::cs::witness_placer::cs_debug_evaluator::witness_early_branch_if_possible;
use crate::cs::witness_placer::WitnessComputationalField;
use crate::cs::witness_placer::WitnessComputationalInteger;
use crate::cs::witness_placer::WitnessTypeSet;
use crate::definitions::*;
use crate::devices::optimization_context::OptimizationContext;
use crate::one_row_compiler::LookupInput;
use crate::tables::LookupWrapper;
use crate::types::Register;
use crate::{
    constraint::*,
    tables::TableDriver,
    types::{Boolean, Num},
};
use field::PrimeField;
use std::collections::HashMap;

pub const DEFAULT_SOURCE_DEST_CAPACITY: usize = 4;
#[cfg(feature = "debug_logs")]
pub const ENABLE_LOGGING: bool = true;
#[cfg(not(feature = "debug_logs"))]
pub const ENABLE_LOGGING: bool = false;

#[non_exhaustive]
pub enum Invariant {
    Boolean,
    RangeChecked { width: u32 },
    Substituted((Placeholder, usize)),
}

#[derive(Debug, Clone, Copy)]
pub enum ShuffleRamQueryType {
    RegisterOnly {
        register_index: Variable,
    },
    /// is_register = 1:
    ///   address 解释为寄存器编号
    ///   read_value 来自 register file
    /// is_register = 0:
    ///   address 解释为 RAM 地址
    ///   read_value 来自 RAM
    RegisterOrRam {
        is_register: Boolean,
        address: [Variable; REGISTER_SIZE],
    },
}
impl ShuffleRamQueryType {
    pub fn get_address<F: PrimeField, CS: Circuit<F>>(&self, cs: &CS) -> Option<u32> {
        match *self {
            Self::RegisterOnly { .. } => None,
            Self::RegisterOrRam {
                is_register,
                address,
            } => {
                let addr =
                    cs.get_value(address[0])
                        .zip_with(cs.get_value(address[1]), |low, high| {
                            (low.as_u64_reduced() | (high.as_u64_reduced() << 16))
                                .try_into()
                                .unwrap()
                        });
                let flag = cs
                    .get_value(is_register.get_variable().unwrap())
                    .filter(|&b| b == F::ZERO);
                flag.and(addr)
            }
        }
    }
    pub fn get_register_id<F: PrimeField, CS: Circuit<F>>(&self, cs: &CS) -> Option<u8> {
        match *self {
            Self::RegisterOnly { register_index } => cs
                .get_value(register_index)
                .map(|f| f.as_u64_reduced().try_into().unwrap()),
            Self::RegisterOrRam {
                is_register,
                address,
            } => {
                let flag = cs
                    .get_value(is_register.get_variable().unwrap())
                    .filter(|&b| b == F::ONE);
                flag.and_then(|_| {
                    cs.get_value(address[0])
                        .zip_with(cs.get_value(address[1]), |low, high| {
                            (low.as_u64_reduced() | (high.as_u64_reduced() << 16))
                                .try_into()
                                .unwrap()
                        })
                })
            }
        }
    }
}

// Prover would have to substitute global timestamp here
// but itself, and ensure that eventually global read timestamp
// is < global write timestamp + local offset
#[derive(Debug, Clone, Copy)]
/// 一次访问在电路里记成一条 query
/// 寄存器读通常 read_value = write_value（读 x1 时「写回」仍是 x1，表示未改寄存器内容）。寄存器写时 read_value 是旧值，write_value 是新值。例如ADD 写 x5：read 100，write 16。
pub struct ShuffleRamMemQuery {
    /// 这次是纯寄存器访问，还是「寄存器或 RAM」二选一
    pub query_type: ShuffleRamQueryType,
    /// 本行内的第几次访问（0、1、2…），用于排序
    pub local_timestamp_in_cycle: usize,
    /// 读到的旧值两个 limb
    pub read_value: [Variable; REGISTER_SIZE],
    /// 写入的新值两个 limb
    pub write_value: [Variable; REGISTER_SIZE],
}

impl ShuffleRamMemQuery {
    pub fn is_readonly(&self) -> bool {
        if self.read_value == self.write_value {
            true
        } else {
            for (a, b) in self.read_value.iter().zip(self.write_value.iter()) {
                assert!(a != b);
            }

            false
        }
    }
    pub fn get_write_value<F: PrimeField, CS: Circuit<F>>(&self, cs: &CS) -> u32 {
        cs.get_value(self.write_value[0])
            .zip_with(cs.get_value(self.write_value[1]), |low, high| {
                (low.as_u64_reduced() | (high.as_u64_reduced() << 16))
                    .try_into()
                    .unwrap()
            })
            .unwrap()
    }
    pub fn get_read_value<F: PrimeField, CS: Circuit<F>>(&self, cs: &CS) -> u32 {
        cs.get_value(self.read_value[0])
            .zip_with(cs.get_value(self.read_value[1]), |low, high| {
                (low.as_u64_reduced() | (high.as_u64_reduced() << 16))
                    .try_into()
                    .unwrap()
            })
            .unwrap()
    }
}

#[derive(Clone, Debug)]
pub struct LookupQuery<F: PrimeField> {
    pub row: [LookupInput<F>; COMMON_TABLE_WIDTH],
    pub table: LookupQueryTableType,
}

#[derive(Clone, Copy, Debug)]
pub enum LookupQueryTableType {
    Variable(Variable),
    Constant(TableType),
}

pub struct LinkedVariablesPair {
    pub initial_var: Variable,
    pub final_var: Variable,
}

#[derive(Clone, Debug)]
pub struct RangeCheckQuery<F: PrimeField> {
    pub input: LookupInput<F>,
    pub width: usize,
}

impl<F: PrimeField> RangeCheckQuery<F> {
    pub fn new(variable: Variable, width: usize) -> Self {
        RangeCheckQuery {
            input: LookupInput::from(variable),
            width,
        }
    }

    pub fn new_for_input(input: LookupInput<F>, width: usize) -> Self {
        RangeCheckQuery { input, width }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DelegatedComputationRequest {
    pub execute: Variable,            // assumed boolean
    pub degegation_type: Variable,    // abstract index
    pub memory_offset_high: Variable, // 16 bit variable
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DelegatedProcessingData {
    pub execute: Variable,            // assumed boolean
    pub memory_offset_high: Variable, // 16 bit variable
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BatchedMemoryAccessType {
    Read {
        read_value: [Variable; REGISTER_SIZE],
    },
    Write {
        read_value: [Variable; REGISTER_SIZE],
        write_value: [Variable; REGISTER_SIZE],
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RegisterAccessRequest {
    pub register_index: u32,
    pub register_write: bool,
    pub indirects_alignment_log2: u32,
    pub indirect_accesses: Vec<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RegisterAccessType {
    Read {
        read_value: [Variable; REGISTER_SIZE],
    },
    Write {
        read_value: [Variable; REGISTER_SIZE],
        write_value: [Variable; REGISTER_SIZE],
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IndirectAccessType {
    Read {
        read_value: [Variable; REGISTER_SIZE],
    },
    Write {
        read_value: [Variable; REGISTER_SIZE],
        write_value: [Variable; REGISTER_SIZE],
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RegisterAndIndirectAccesses {
    pub register_index: u32,
    pub register_access: RegisterAccessType,
    pub indirects_alignment_log2: u32,
    pub indirect_accesses: Vec<IndirectAccessType>,
}

/// 是compiler边界前的“约束草稿”。它还没有把变量排成trace列，只是用Variable编号描述所有东西。
pub struct CircuitOutput<F: PrimeField> {
    /// state_input和state_output保存跨行状态变量。main RISC-V通常最核心的是pc状态。它们告诉后面的compiler：这一行的结束状态要和下一行的开始状态连接。
    pub state_input: Vec<Variable>,
    /// state_input和state_output保存跨行状态变量。main RISC-V通常最核心的是pc状态。它们告诉后面的compiler：这一行的结束状态要和下一行的开始状态连接。
    pub state_output: Vec<Variable>,
    /// table_driver保存当前CircuitOutput已经注册好的固定表信息。
    /// 第三章里setup trace正是根据这些表内容和setup layout生成的。
    pub table_driver: TableDriver<F>,
    /// num_of_variables是电路构造阶段一共分配了多少个Variable。
    /// 这仍然是“符号编号空间”的大小，不是最终trace列数。
    pub num_of_variables: usize,
    /// constraints保存普通多项式约束。比如某个变量必须等于两个变量相加，某个flag必须满足布尔性，某个candidate relation必须为0。这里的Constraint<F>还基于Variable，不是最终列地址
    pub constraints: Vec<(Constraint<F>, bool)>,
    /// lookups保存普通lookup查询。LookupQuery里有一行LookupInput和一个表类型，表类型可以是常量表，也可以由变量决定。源码中LookupQuery和LookupQueryTableType定义在Circuit文件中。
    pub lookups: Vec<LookupQuery<F>>,
    /// shuffle_ram_queries保存RAM/register统一memory argument查询。ShuffleRamQueryType有两类：RegisterOnly和RegisterOrRam。RegisterOrRam带一个is_register布尔值和address limbs；当is_register=1时解释为寄存器访问，当is_register=0时解释为RAM访问。源码里ShuffleRamQueryType和ShuffleRamMemQuery定义在circuit.rs。
    pub shuffle_ram_queries: Vec<ShuffleRamMemQuery>,
    /// delegated_computation_requests保存main circuit向delegation circuit发出的请求。比如某行CSR触发BLAKE2 delegation，就会产生一个request。源码里的DelegatedComputationRequest包含execute、degegation_type和memory_offset_high。
    pub delegated_computation_requests: Vec<DelegatedComputationRequest>,
    pub degegated_request_to_process: Option<DelegatedProcessingData>,
    pub batched_memory_accesses: Vec<BatchedMemoryAccessType>,
    pub register_and_indirect_memory_accesses: Vec<RegisterAndIndirectAccesses>,
    pub linked_variables: Vec<LinkedVariablesPair>,
    /// range_check_expressions保存range check请求。比如某个表达式需要证明落在16-bit范围内，就会生成range check query。
    pub range_check_expressions: Vec<RangeCheckQuery<F>>,
    /// boolean_vars保存必须为0/1的变量。后面compiler会为它们生成布尔约束或相关布局。
    pub boolean_vars: Vec<Variable>,
    /// substitutions保存placeholder到变量的映射。这个后面witness generation和生成代码会用到，比如“某个公开输入位置”或“某个特殊变量”要找到对应Variable。
    pub substitutions: HashMap<(Placeholder, usize), Variable>,
}

impl<F: PrimeField> CircuitOutput<F> {
    pub fn get_variable_by_placeholder(
        &self,
        placeholder: Placeholder,
        subindex: usize,
    ) -> Variable {
        self.substitutions
            .get(&(placeholder, subindex))
            .cloned()
            .unwrap()
    }
}

pub trait Circuit<F: PrimeField>: Sized {
    type WitnessPlacer: WitnessPlacer<F>;

    fn new() -> Self;
    fn add_variable(&mut self) -> Variable;
    fn set_values(&mut self, node: impl WitnessResolutionDescription<F, Self::WitnessPlacer>);
    fn get_value(&self, _var: Variable) -> Option<F> {
        None
    }
    fn add_constant_variable(&mut self, fr: F) -> Variable;
    fn add_constraint(&mut self, constraint: Constraint<F>);
    fn add_constraint_allow_explicit_linear(&mut self, constraint: Constraint<F>);
    fn add_constraint_allow_explicit_linear_prevent_optimizations(
        &mut self,
        constraint: Constraint<F>,
    );
    // fn add_lookup(&mut self, query: LookupQuery<F>);

    fn add_shuffle_ram_query(&mut self, query: ShuffleRamMemQuery);

    fn add_delegation_request(&mut self, request: DelegatedComputationRequest);
    fn process_delegation_request(&mut self) -> Boolean;
    fn create_batched_memory_accesses(
        &mut self,
        is_writable: &[bool],
    ) -> Vec<BatchedMemoryAccessType>;
    fn create_register_and_indirect_memory_accesses(
        &mut self,
        request: RegisterAccessRequest,
    ) -> RegisterAndIndirectAccesses;

    fn require_invariant(&mut self, variable: Variable, invariant: Invariant);
    fn link_variables(&mut self, initial_variable: Variable, final_variable: Variable);
    fn finalize(self) -> (CircuitOutput<F>, Option<Self::WitnessPlacer>);

    fn materialize_table(&mut self, table_type: TableType);
    fn add_table_with_content(&mut self, table_type: TableType, table: LookupWrapper<F>);

    #[track_caller]
    fn add_boolean_variable(&mut self) -> Boolean {
        let new_var = self.add_variable();
        self.require_invariant(new_var, Invariant::Boolean);
        Boolean::Is(new_var)
    }

    #[track_caller]
    fn add_variable_with_range_check(&mut self, width: u32) -> Num<F> {
        assert!(
            width as usize == SMALL_RANGE_CHECK_TABLE_WIDTH
                || width as usize == LARGE_RANGE_CHECK_TABLE_WIDTH
        );
        let new_var = self.add_variable();
        self.require_invariant(new_var, Invariant::RangeChecked { width });
        Num::Var(new_var)
    }

    #[track_caller]
    fn add_variable_from_constraint(&mut self, mut constraint: Constraint<F>) -> Variable {
        assert!(constraint.is_empty() == false);
        assert!(constraint.terms.iter().all(|x| x.is_constant()) == false);
        constraint.normalize();
        let new_var = self.add_variable();
        collapse_max_quadratic_constraint_into(self, constraint.clone(), new_var);

        constraint -= new_var.into();
        self.add_constraint(constraint);

        new_var
    }

    #[track_caller]
    fn add_variable_from_constraint_without_witness_evaluation(
        &mut self,
        mut constraint: Constraint<F>,
    ) -> Variable {
        assert!(constraint.is_empty() == false);
        assert!(constraint.terms.iter().all(|x| x.is_constant()) == false);
        constraint.normalize();
        let new_var = self.add_variable();
        constraint -= new_var.into();
        self.add_constraint(constraint);

        new_var
    }

    #[track_caller]
    fn add_variable_from_constraint_allow_explicit_linear(
        &mut self,
        mut constraint: Constraint<F>,
    ) -> Variable {
        assert!(constraint.is_empty() == false);
        assert!(constraint.terms.iter().all(|x| x.is_constant()) == false);
        constraint.normalize();
        let new_var = self.add_variable();
        collapse_max_quadratic_constraint_into(self, constraint.clone(), new_var);

        constraint -= new_var.into();
        self.add_constraint_allow_explicit_linear(constraint);

        new_var
    }

    #[track_caller]
    fn add_variable_from_constraint_allow_explicit_linear_without_witness_evaluation(
        &mut self,
        mut constraint: Constraint<F>,
    ) -> Variable {
        assert!(constraint.is_empty() == false);
        assert!(constraint.terms.iter().all(|x| x.is_constant()) == false);
        constraint.normalize();
        let new_var = self.add_variable();
        constraint -= new_var.into();
        self.add_constraint_allow_explicit_linear(constraint);

        new_var
    }

    #[track_caller]
    fn choose(&mut self, flag: Boolean, if_true_val: Num<F>, if_false_val: Num<F>) -> Num<F> {
        match (if_true_val, if_false_val) {
            (Num::Var(a), Num::Var(b)) => {
                if a == b {
                    return if_true_val.clone();
                }
                match flag {
                    Boolean::Constant(flag) => {
                        if flag {
                            if_true_val
                        } else {
                            if_false_val
                        }
                    }
                    Boolean::Is(cond) => {
                        // if_true_val = a, if_false_val = b
                        // new_var = flag * a + (1 - flag) * b = flag * (a - b) + b
                        let mut cnstr: Constraint<F> =
                            { Term::from(cond) * (Term::from(a) - Term::from(b)) + Term::from(b) };
                        let new_var = self.add_variable();
                        cnstr -= Term::from(new_var);

                        let value_fn = move |placer: &mut Self::WitnessPlacer| {
                            let mask = placer.get_boolean(cond);
                            let selection_result = WitnessComputationalField::select(
                                &mask,
                                &placer.get_field(a),
                                &placer.get_field(b),
                            );
                            placer.assign_field(new_var, &selection_result);
                        };
                        self.set_values(value_fn);

                        self.add_constraint(cnstr);
                        Num::Var(new_var)
                    }

                    Boolean::Not(_cond) => {
                        unreachable!()

                        // // new_var = flag * b + (1 - flag) * a = flag * (b - a) + a
                        // let cnstr: Constraint<F> =
                        //     { Term::from(cond) * (Term::from(b) - Term::from(a)) + Term::from(a) };
                        // let new_var = self.add_variable_from_constraint(cnstr);
                        // Num::Var(new_var)
                    }
                }
            }
            (Num::Var(a), Num::Constant(constant)) => {
                match flag {
                    Boolean::Constant(flag) => {
                        if flag {
                            return Num::Var(a.clone());
                        } else {
                            return Num::Constant(constant);
                        }
                    }
                    Boolean::Is(cond) => {
                        // new_var = flag * a + (1 - flag) * constant = flag * (if_true - constant) + constant
                        let mut cnstr: Constraint<F> = {
                            Term::from(cond) * (Term::from(a) - Term::from_field(constant))
                                + Term::from_field(constant)
                        };
                        let new_var = self.add_variable();
                        cnstr -= Term::from(new_var);

                        let value_fn = move |placer: &mut Self::WitnessPlacer| {
                            let mask = placer.get_boolean(cond);
                            let b = WitnessComputationalField::constant(constant);
                            let selection_result =
                                WitnessComputationalField::select(&mask, &placer.get_field(a), &b);
                            placer.assign_field(new_var, &selection_result);
                        };
                        self.set_values(value_fn);

                        self.add_constraint(cnstr);
                        Num::Var(new_var)
                    }

                    Boolean::Not(cond) => {
                        // new_var = flag * constant + (1 - flag) * a = flag * (constant - a) + a
                        let mut cnstr: Constraint<F> = {
                            Term::from(cond) * (Term::from_field(constant) - Term::from(a))
                                + Term::from(a)
                        };
                        let new_var = self.add_variable();
                        cnstr -= Term::from(new_var);

                        let value_fn = move |placer: &mut Self::WitnessPlacer| {
                            let mask = placer.get_boolean(cond);
                            let b = WitnessComputationalField::constant(constant);
                            let selection_result =
                                WitnessComputationalField::select(&mask, &b, &placer.get_field(a));
                            placer.assign_field(new_var, &selection_result);
                        };
                        self.set_values(value_fn);

                        self.add_constraint(cnstr);
                        Num::Var(new_var)
                    }
                }
            }

            (Num::Constant(..), Num::Var(..)) => {
                self.choose(flag.toggle(), if_false_val, if_true_val)
            }
            (Num::Constant(a), Num::Constant(b)) => {
                if a == b {
                    return Num::Constant(a);
                }
                match flag {
                    Boolean::Constant(flag) => {
                        let result_value = if flag { a } else { b };

                        Num::Constant(result_value)
                    }
                    Boolean::Is(cond) => {
                        // a * condition + b*(1-condition) = c ->
                        // (a - b) *condition - c + b = 0
                        let mut cnstr: Constraint<F> = {
                            Term::from(cond) * (Term::from_field(a) - Term::from_field(b))
                                + Term::from_field(b)
                        };
                        let new_var = self.add_variable();
                        cnstr -= Term::from(new_var);

                        let value_fn = move |placer: &mut Self::WitnessPlacer| {
                            let mask = placer.get_boolean(cond);
                            let a = WitnessComputationalField::constant(a);
                            let b = WitnessComputationalField::constant(b);
                            let selection_result = WitnessComputationalField::select(&mask, &a, &b);
                            placer.assign_field(new_var, &selection_result);
                        };
                        self.set_values(value_fn);

                        self.add_constraint_allow_explicit_linear(cnstr);
                        Num::Var(new_var)
                    }
                    Boolean::Not(cond) => {
                        // b * condition + a*(1-condition) = c ->
                        // (b - a) * condition - c + a = 0
                        let mut cnstr: Constraint<F> = {
                            Term::from(cond) * (Term::from_field(b) - Term::from_field(a))
                                + Term::from_field(a)
                        };
                        let new_var = self.add_variable();
                        cnstr -= Term::from(new_var);

                        let value_fn = move |placer: &mut Self::WitnessPlacer| {
                            let mask = placer.get_boolean(cond);
                            let a = WitnessComputationalField::constant(a);
                            let b = WitnessComputationalField::constant(b);
                            let selection_result = WitnessComputationalField::select(&mask, &b, &a);
                            placer.assign_field(new_var, &selection_result);
                        };
                        self.set_values(value_fn);

                        self.add_constraint_allow_explicit_linear(cnstr);
                        Num::Var(new_var)
                    }
                }
            }
        }
    }

    #[track_caller]
    fn choose_from_orthogonal_variants(
        &mut self,
        flags: &[Boolean],
        variants: &[Num<F>],
    ) -> Num<F> {
        assert!(flags.len() > 0);
        assert_eq!(flags.len(), variants.len());
        return spec_choose_from_orthogonal_variants(self, flags, variants);
    }

    #[track_caller]
    fn choose_from_orthogonal_variants_for_linear_terms(
        &mut self,
        flags: &[Boolean],
        variants: &[Constraint<F>],
    ) -> Num<F> {
        assert!(flags.len() > 0);
        assert_eq!(flags.len(), variants.len());

        return spec_choose_from_orthogonal_variants_for_linear_terms(self, flags, variants);
    }

    fn is_zero(&mut self, var: Num<F>) -> Boolean {
        self.equals_to(var, Num::Constant(F::ZERO))
    }

    // Special zero-check for register, that consists of range-checked limbs, so we can just
    // check that their sum is 0
    fn is_zero_reg(&mut self, reg: Register<F>) -> Boolean {
        let is_zero_flag = self.add_variable();
        self.is_zero_reg_explicit(reg, is_zero_flag); // would be much nicer to use not_zero_flag directly
        Boolean::Is(is_zero_flag)
    }

    fn is_zero_reg_explicit(&mut self, reg: Register<F>, is_zero_flag: Variable) {
        match reg.0 {
            [Num::Var(low), Num::Var(high)] => {
                let inv = self.add_variable();

                let value_fn = move |placer: &mut Self::WitnessPlacer| {
                    let low = placer.get_field(low);
                    let high = placer.get_field(high);
                    let mut sum = low;
                    sum.add_assign(&high);
                    let inv_value = sum.inverse_or_zero();
                    let zflag_value = sum.is_zero();
                    placer.assign_field(inv, &inv_value);
                    placer.assign_mask(is_zero_flag, &zflag_value);
                };
                self.set_values(value_fn);

                let not_zero_flag = Constraint::from(1) - Term::from(is_zero_flag);
                self.add_constraint(
                    Constraint::from(inv) * (Term::from(low) + Term::from(high))
                        - not_zero_flag.clone(),
                );
                self.add_constraint(
                    (Constraint::from(1) - not_zero_flag) * (Term::from(low) + Term::from(high)),
                );
            }
            _ => unreachable!(),
        }
    }

    fn equals_to(&mut self, a: Num<F>, b: Num<F>) -> Boolean {
        match (a, b) {
            (Num::Var(a), Num::Var(b)) => {
                // (var - var2) * zero_flag = 0;
                // (var - var2) * var_inv = 1 - zero_flag;
                let var_inv = self.add_variable();
                let zero_flag = self.add_boolean_variable();
                let zero_flag_var = zero_flag.get_variable().unwrap();

                let value_fn = move |placer: &mut Self::WitnessPlacer| {
                    let mut a = placer.get_field(a);
                    let b = placer.get_field(b);
                    a.sub_assign(&b);
                    let is_zero = a.is_zero();
                    let inverse_witness = a.inverse_or_zero();
                    placer.assign_mask(zero_flag_var, &is_zero);
                    placer.assign_field(var_inv, &inverse_witness);
                };
                self.set_values(value_fn);
                self.add_constraint((Term::from(a) - Term::from(b)) * Term::from(zero_flag));
                self.add_constraint(
                    (Term::from(a) - Term::from(b)) * Term::from(var_inv) + Term::from(zero_flag)
                        - Term::from(1),
                );

                zero_flag
            }
            (Num::Var(a), Num::Constant(b)) => {
                // (var - cnst) * zero_flag = 0;
                // (var - cnst) * var_inv = 1 - zero_flag;
                let var_inv = self.add_variable();
                let zero_flag = self.add_boolean_variable();
                let zero_flag_var = zero_flag.get_variable().unwrap();

                let value_fn = move |placer: &mut Self::WitnessPlacer| {
                    let mut a = placer.get_field(a);
                    let b = WitnessComputationalField::constant(b);
                    a.sub_assign(&b);
                    let is_zero = a.is_zero();
                    let inverse_witness = a.inverse_or_zero();
                    placer.assign_mask(zero_flag_var, &is_zero);
                    placer.assign_field(var_inv, &inverse_witness);
                };
                self.set_values(value_fn);
                self.add_constraint((Term::from(a) - Term::from_field(b)) * Term::from(zero_flag));
                self.add_constraint(
                    (Term::from(a) - Term::from_field(b)) * Term::from(var_inv)
                        + Term::from(zero_flag)
                        - Term::from(1),
                );

                zero_flag
            }
            (Num::Constant(a), Num::Var(b)) => {
                // (var - cnst) * zero_flag = 0;
                // (var - cnst) * var_inv = 1 - zero_flag;
                let var_inv = self.add_variable();
                let zero_flag = self.add_boolean_variable();
                let zero_flag_var = zero_flag.get_variable().unwrap();

                let value_fn = move |placer: &mut Self::WitnessPlacer| {
                    let b = placer.get_field(b);
                    let mut a = <Self::WitnessPlacer as WitnessTypeSet<F>>::Field::constant(a);
                    a.sub_assign(&b);
                    let is_zero = a.is_zero();
                    let inverse_witness = a.inverse_or_zero();
                    placer.assign_mask(zero_flag_var, &is_zero);
                    placer.assign_field(var_inv, &inverse_witness);
                };
                self.set_values(value_fn);
                self.add_constraint((Term::from_field(a) - Term::from(b)) * Term::from(zero_flag));
                self.add_constraint(
                    (Term::from_field(a) - Term::from(b)) * Term::from(var_inv)
                        + Term::from(zero_flag)
                        - Term::from(1),
                );

                zero_flag
            }
            (Num::Constant(a), Num::Constant(b)) => {
                let is_equal = a == b;
                Boolean::Constant(is_equal)
            }
        }
    }

    #[track_caller]
    fn peek_lookup_value_unconstrained<const M: usize, const N: usize>(
        &mut self,
        inputs: &[LookupInput<F>; M],
        table_type: TableType,
        exec_flag: Boolean,
    ) -> [Variable; N] {
        assert_eq!(M + N, COMMON_TABLE_WIDTH);
        assert!(M > 0);

        // here we should do the same trick as with "add variable from constraint",
        // so that we can have a universal witness generation function, but provide via constraints
        // a description of the relation

        let output_variables: [Variable; N] = std::array::from_fn(|_| self.add_variable());
        let inputs = inputs.clone();
        let exec_flag = exec_flag.get_variable().unwrap();

        let inner_evaluator = move |placer: &mut Self::WitnessPlacer| {
            let mask = placer.get_boolean(exec_flag);
            if table_type == TableType::ZeroEntry {
                let zero = WitnessComputationalField::constant(F::ZERO);
                for var in output_variables.iter() {
                    placer.conditionally_assign_field(*var, &mask, &zero);
                }
                return;
            }
            let input_values: [_; M] = std::array::from_fn(|i| inputs[i].evaluate(placer));
            let table_id = <Self::WitnessPlacer as WitnessTypeSet<F>>::U16::constant(
                table_type.to_table_id() as u16,
            );
            let output_values = placer.lookup::<M, N>(&input_values, &table_id);
            for (var, value) in output_variables.iter().zip(output_values.iter()) {
                placer.conditionally_assign_field(*var, &mask, value);
            }
        };

        let value_fn = move |placer: &mut Self::WitnessPlacer| {
            let mask = placer.get_boolean(exec_flag);
            witness_early_branch_if_possible(mask.clone(), placer, &inner_evaluator);
        };

        self.set_values(value_fn);

        output_variables
    }

    #[track_caller]
    fn peek_lookup_value_unconstrained_ext<const M: usize, const N: usize>(
        &mut self,
        inputs: &[LookupInput<F>; M],
        outputs: &[Variable; N],
        table: Num<F>,
        exec_flag: Boolean,
    ) {
        assert!(inputs.len() > 0);

        let output_variables: [Variable; N] = outputs.clone();
        let inputs = inputs.clone();
        let exec_flag = exec_flag.get_variable().unwrap();

        let inner_evaluator = move |placer: &mut Self::WitnessPlacer| {
            let table_id = match table {
                Num::Constant(con) => <Self::WitnessPlacer as WitnessTypeSet<F>>::U16::constant(
                    con.as_u64_reduced() as u16,
                ),
                Num::Var(var) => placer.get_u16(var),
            };
            let mask = placer.get_boolean(exec_flag);
            let input_values: [_; M] = std::array::from_fn(|i| inputs[i].evaluate(placer));
            let output_values = placer.maybe_lookup::<M, N>(&input_values, &table_id, &mask);
            for (var, value) in output_variables.iter().zip(output_values.iter()) {
                placer.conditionally_assign_field(*var, &mask, value);
            }
        };

        let value_fn = move |placer: &mut Self::WitnessPlacer| {
            let mask = placer.get_boolean(exec_flag);
            witness_early_branch_if_possible(mask.clone(), placer, &inner_evaluator);
        };

        self.set_values(value_fn);
    }

    fn enforce_lookup_tuple_for_fixed_table<const M: usize>(
        &mut self,
        inputs: &[LookupInput<F>; M],
        table_type: TableType,
        skip_generating_multiplicity_counting_function: bool,
    );

    fn enforce_lookup_tuple_for_variable_table<const M: usize>(
        &mut self,
        inputs: &[LookupInput<F>; M],
        table_type: Variable,
    );

    #[track_caller]
    /// 向 lookup_storage 登记一条 RomAddressSpaceSeparator 查询；分配输出变量
    fn get_variables_from_lookup_constrained<const M: usize, const N: usize>(
        &mut self,
        inputs: &[LookupInput<F>; M],
        table_type: TableType,
    ) -> [Variable; N];

    fn set_log(&mut self, opt_ctx: &OptimizationContext<F, Self>, name: &'static str);
    fn view_log(&self, name: &'static str);
    fn is_satisfied(&mut self) -> bool;
}

impl<F: PrimeField> LookupInput<F> {
    pub fn evaluate<W: WitnessPlacer<F>>(&self, placer: &mut W) -> W::Field {
        match self {
            LookupInput::Variable(variable) => placer.get_field(*variable),
            LookupInput::Expression {
                linear_terms,
                constant_coeff,
            } => {
                let mut result: W::Field = WitnessComputationalField::constant(*constant_coeff);
                for (c, a) in linear_terms.iter() {
                    result.add_assign_product(
                        &WitnessComputationalField::constant(*c),
                        &placer.get_field(*a),
                    );
                }

                result
            }
        }
    }
}

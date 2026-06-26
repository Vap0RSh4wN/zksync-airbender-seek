use std::collections::BTreeMap;

use super::*;
use crate::devices::aux_data::PcWrapper;

#[derive(Clone, Copy, Debug)]
/// 对当前阅读目标来说，最重要的是pc。寄存器文件和RAM不直接作为普通state字段保存，而是通过shuffle RAM query表达读写。
/// 也就是说，相邻两行之间的pc用state linkage连接，寄存器和RAM的一致性由memory argument证明。
/// Register<F>表示一个32-bit寄存器值，通常用两个16-bit limb表示。这里pc也是一个Register<F>：
/// pc = [pc_low16, pc_high16]
/// 例如：
/// pc = 0x0001_0020
/// pc_low16  = 0x0020
/// pc_high16 = 0x0001
/// 所以MinimalStateRegistersInMemory可以理解成表示跨行状态只保留最小CPU状态：
/// MinimalStateRegistersInMemory {
/// pc: [pc_low16, pc_high16]
/// }
/// 它没有regs: [Register; 32]，也没有memory字段。
pub struct MinimalStateRegistersInMemory<F: PrimeField> {
    pub pc: Register<F>,
}

impl<F: PrimeField> AbstractMachineState<F> for MinimalStateRegistersInMemory<F> {
    fn set_size() -> usize {
        Register::<F>::set_size()
    }

    fn append_into_variables_set(&self, dst: &mut Vec<Variable>) {
        self.pc.append_into_variables_set(dst);
    }
}

impl<F: PrimeField> MinimalStateRegistersInMemory<F> {
    /// 通过 PcWrapper::initialize 向 cs 申请 pc 的两个 Variable：pc_low 和 pc_high。此时还没有具体数值 0，只是占住跨行状态槽位。
    pub fn initialize<CS: Circuit<F>>(circuit: &mut CS) -> Self {
        // this will link to witness inputs
        let pc = PcWrapper::<F>::initialize(circuit);

        Self { pc: pc.pc }
    }
}

impl<F: PrimeField> BaseMachineState<F> for MinimalStateRegistersInMemory<F> {
    fn opcodes_are_in_rom() -> bool {
        true
    }

    fn get_pc(&self) -> &Register<F> {
        &self.pc
    }
    fn get_pc_mut(&mut self) -> &mut Register<F> {
        &mut self.pc
    }

    fn csr_use_props() -> CSRUseProperties {
        CSRUseProperties {
            standard_csrs: vec![],
            allow_non_determinism_csr: true,
            support_mstatus: false,
        }
    }

    fn all_csrs(&self) -> BTreeMap<u16, Register<F>> {
        BTreeMap::new()
    }
}

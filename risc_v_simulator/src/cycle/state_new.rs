// This simulator follows a paradigm of the unrolled cycle circuits

use std::collections::HashMap;
use std::hint::unreachable_unchecked;

pub use super::decoder_utils::*;
pub use super::utils::*;
use crate::cycle::state::RiscV32ObservableState;
use crate::mmu::NoMMU;
use crate::sim::RiscV32Machine;
use crate::sim::SimulatorConfig;
use crate::utils::{sign_extend, sign_extend_16, sign_extend_8, zero_extend_16, zero_extend_8};

use crate::abstractions::csr_processor::NoExtraCSRs;
use crate::abstractions::memory::{AccessType, MemorySource};
use crate::abstractions::non_determinism::NonDeterminismCSRSource;
use crate::abstractions::tracer::Tracer;
use crate::cycle::state::report_opcode;
use crate::cycle::state::MARKER_CSR;
use crate::cycle::state::NON_DETERMINISM_CSR;
use crate::cycle::state::NUM_REGISTERS;
#[cfg(feature = "opcode_stats")]
use crate::cycle::state::OPCODES_COUNTER;
#[cfg(feature = "cycle_marker")]
use crate::cycle::state::{CycleMarker, Mark, CYCLE_MARKER};
use crate::cycle::status_registers::TrapReason;
use crate::cycle::IMStandardIsaConfig;
use crate::cycle::MachineConfig;
use crate::mmu::MMUImplementation;

use crate::cycle::opcode_formats::*;

pub trait DelegationCSRProcessor: 'static + Clone + std::fmt::Debug {
    fn process_write<
        M: MemorySource,
        TR: Tracer<C>,
        ND: NonDeterminismCSRSource<M>,
        C: MachineConfig,
    >(
        &mut self,
        state: &mut RiscV32StateForUnrolledProver<C>,
        csr_index: u16,
        memory_source: &mut M,
        non_determinism_source: &mut ND,
        tracer: &mut TR,
    );
}

impl DelegationCSRProcessor for NoExtraCSRs {
    #[inline(always)]
    fn process_write<
        M: MemorySource,
        TR: Tracer<C>,
        ND: NonDeterminismCSRSource<M>,
        C: MachineConfig,
    >(
        &mut self,
        _state: &mut RiscV32StateForUnrolledProver<C>,
        csr_index: u16,
        _memory_source: &mut M,
        _non_determinism_source: &mut ND,
        _tracer: &mut TR,
    ) {
        panic!("Unsupported CSR index {}", csr_index);
    }
}

impl DelegationCSRProcessor for crate::delegations::DelegationsCSRProcessor {
    #[inline(always)]
    fn process_write<
        M: MemorySource,
        TR: Tracer<C>,
        ND: NonDeterminismCSRSource<M>,
        C: MachineConfig,
    >(
        &mut self,
        state: &mut RiscV32StateForUnrolledProver<C>,
        csr_index: u16,
        memory_source: &mut M,
        _non_determinism_source: &mut ND,
        tracer: &mut TR,
    ) {
        use crate::delegations::unrolled::blake2_round_function_with_compression_mode::*;
        use crate::delegations::unrolled::u256_ops_with_control::*;

        match csr_index as u32 {
            BLAKE2_ROUND_FUNCTION_WITH_EXTENDED_CONTROL_ACCESS_ID => {
                blake2_round_function_with_extended_control_over_unrolled_state(
                    state,
                    memory_source,
                    tracer,
                );
            }
            U256_OPS_WITH_CONTROL_ACCESS_ID => {
                u256_ops_with_control_impl_over_unrolled_state(state, memory_source, tracer);
            }
            csr => {
                panic!("Unsupported CSR = 0x{:04x}", csr);
            }
        }
    }
}

pub(crate) struct Riscv32MachineProverUnrolled<
    MS: MemorySource,
    TR: Tracer<C>,
    ND: NonDeterminismCSRSource<MS>,
    CSR: DelegationCSRProcessor,
    C: MachineConfig,
> {
    pub(crate) state: RiscV32StateForUnrolledProver<C>,
    pub(crate) memory_source: MS,
    pub(crate) memory_tracer: TR,
    pub(crate) non_determinism_source: ND,
    pub(crate) csr_processor: CSR,
}

impl<MS, TR, ND, CSR, C> Riscv32MachineProverUnrolled<MS, TR, ND, CSR, C>
where
    MS: MemorySource,
    TR: Tracer<C>,
    ND: NonDeterminismCSRSource<MS>,
    CSR: DelegationCSRProcessor,
    C: MachineConfig,
{
    pub fn new(
        config: &SimulatorConfig,
        memory_source: MS,
        memory_tracer: TR,
        non_determinism_source: ND,
        csr_processor: CSR,
    ) -> Self {
        let state = RiscV32StateForUnrolledProver::initial(config.entry_point);
        Self {
            state,
            memory_source,
            memory_tracer,
            non_determinism_source,
            csr_processor,
        }
    }
}

impl<MS, TR, ND, CSR, C> RiscV32Machine<ND, MS, TR, NoMMU, C>
    for Riscv32MachineProverUnrolled<MS, TR, ND, CSR, C>
where
    MS: MemorySource,
    TR: Tracer<C>,
    ND: NonDeterminismCSRSource<MS>,
    CSR: DelegationCSRProcessor,
    C: MachineConfig,
{
    fn cycle(&mut self) {
        self.state.cycle(
            &mut self.memory_source,
            &mut self.memory_tracer,
            &mut self.non_determinism_source,
            &mut self.csr_processor,
        );
    }

    fn state(&self) -> &super::state::RiscV32ObservableState {
        &self.state.observable
    }

    // fn deconstruct(self) -> (super::state::RiscV32ObservableState, MS, ND, TR) {
    //     let Riscv32MachineProverUnrolled {
    //         state,
    //         memory_source,
    //         memory_tracer,
    //         non_determinism_source,
    //         csr_processor
    //     } = self;
    //
    //     (
    //         state.state,
    //         memory_source,
    //         non_determinism_source,
    //         memory_tracer
    //     )
    // }

    fn collect_stacktrace(
        &mut self,
        symbol_info: &crate::sim::diag::SymbolInfo,
        dwarf_cache: &mut crate::sim::diag::DwarfCache,
        cycle: usize,
    ) -> crate::sim::diag::StacktraceCollectionResult {
        crate::sim::diag::collect_stacktrace(
            symbol_info,
            dwarf_cache,
            &self.state.observable,
            &mut self.memory_source,
            &mut self.memory_tracer,
            &mut NoMMU::default(),
            cycle,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
/// 真正的RISC-V VM状态
pub struct RiscV32StateForUnrolledProver<Config: MachineConfig = IMStandardIsaConfig> {
    /// 保存寄存器数组和pc。源码初始化时创建32个0寄存器和初始pc。
    pub observable: RiscV32ObservableState,
    // pub registers: [u32; NUM_REGISTERS],
    // pub pc: u32,
    /// 用于处理特殊CSR写入触发的delegation逻辑。比如Blake2、U256这类被委托出去的计算。
    /// 源码里process_write会根据CSR编号进入Blake2或U256 delegation实现。
    _marker: std::marker::PhantomData<Config>,
}

impl<Config: MachineConfig> RiscV32StateForUnrolledProver<Config> {
    pub fn initial(initial_pc: u32) -> Self {
        // we should start in machine mode, the rest is not important and can be by default
        let registers = [0u32; NUM_REGISTERS];
        let pc = initial_pc;

        #[cfg(feature = "opcode_stats")]
        OPCODES_COUNTER.with_borrow_mut(|el| el.clear());

        Self {
            observable: RiscV32ObservableState { registers, pc },
            // registers,
            // pc,
            _marker: std::marker::PhantomData,
        }
    }

    #[must_use]
    #[inline(always)]
    pub fn get_register(&self, reg_idx: u32) -> u32 {
        unsafe {
            core::hint::assert_unchecked(reg_idx < 32);
        }
        let res = unsafe { *self.observable.registers.get_unchecked(reg_idx as usize) };

        res
    }

    #[inline(always)]
    /// set_register会返回旧值，同时把value写入寄存器编号reg_idx对应的寄存器。
    pub fn set_register(&mut self, reg_idx: u32, mut value: u32) -> u32 {
        unsafe {
            core::hint::assert_unchecked(reg_idx < 32);
        }
        if reg_idx == 0 {
            value = 0;
        }
        unsafe {
            let dst = self
                .observable
                .registers
                .get_unchecked_mut(reg_idx as usize);
            let existing = *dst;
            *dst = value;

            existing
        }
    }

    #[inline(always)]
    fn add_marker(&self) {
        #[cfg(feature = "cycle_marker")]
        CYCLE_MARKER.with_borrow_mut(|cm| cm.add_marker())
    }

    #[inline(always)]
    fn add_delegation(id: u32) {
        #[cfg(feature = "cycle_marker")]
        CYCLE_MARKER.with_borrow_mut(|cm| cm.add_delegation(id))
    }

    #[inline(always)]
    fn count_new_cycle_for_markers(&self) {
        #[cfg(feature = "cycle_marker")]
        CYCLE_MARKER.with_borrow_mut(|cm| cm.incr_cycle_counter())
    }

    #[inline(always)]
    /// 读取当前pc对应的instruction
    fn decoder_step<M: MemorySource, TR: Tracer<Config>>(
        &mut self,
        memory_source: &mut M,
        tracer: &mut TR,
    ) -> u32 {
        let opcode = opcode_read(self.observable.pc, memory_source);

        opcode
    }

    /// 真正执行guest。比如当前指令是：ADD x3, x1, x2
    /// 假设执行前：
    /// pc = 0
    /// x1 = 7
    /// x2 = 5
    /// x3 = 100
    /// VM执行后：
    /// x3 = 12
    /// pc = 4
    pub fn run_cycles<
        M: MemorySource,
        TR: Tracer<Config>,
        ND: NonDeterminismCSRSource<M>,
        CSR: DelegationCSRProcessor,
    >(
        &mut self,
        memory_source: &mut M,
        tracer: &mut TR,
        non_determinism_source: &mut ND,
        csr_processor: &mut CSR,
        num_cycles: usize,
    ) -> bool {
        let mut finished_execution = false;
        // 每次调用self.cycle(...)，执行一个VM cycle。
        // 注意它即使某个cycle返回finished，也会继续跑完本chunk剩余cycles，只是把finished_execution置为true。
        // 为什么不立刻break？因为每个main circuit chunk需要固定长度：cycles_per_chunk = trace_size - 1
        // 即使程序中途结束，这个chunk也要填满，后面才容易生成固定形状的witness trace。
        for _cycle in 0..num_cycles {
            if self.cycle(memory_source, tracer, non_determinism_source, csr_processor) {
                finished_execution = true;
            }
        }

        finished_execution
    }

    #[inline(always)]
    /// 1. tracer.at_cycle_start_ext
    /// 2. 从pc读取opcode
    /// 3. decode rd/rs1/rs2/op/funct3/funct7
    /// 4. pc默认加4
    /// 5. 读取rs1、rs2
    /// 6. match opcode执行具体指令
    /// 7. load/store/CSR/delegation/tracer记录
    /// 8. tracer.at_cycle_end_ext
    pub fn cycle<
        M: MemorySource,
        TR: Tracer<Config>,
        ND: NonDeterminismCSRSource<M>,
        CSR: DelegationCSRProcessor,
    >(
        &mut self,                       //当前VM状态，里面有pc和32个寄存器
        memory_source: &mut M,           //VM内存，负责取指、load、store
        tracer: &mut TR,                 //记录本cycle发生的访问，最终形成CycleData
        non_determinism_source: &mut ND, //外部输入源
        csr_processor: &mut CSR,         //处理特殊CSR和delegation
    ) -> bool {
        // 它先通知tracer：“新cycle开始了”。然后从当前pc读opcode，再把RISC-V instruction拆字段。

        tracer.at_cycle_start_ext(&*self);

        // 这里opcode变量名容易误导。它其实是完整32-bit instruction，不只是低7位opcode。
        // 读取当前pc对应的instruction
        let opcode = self.decoder_step(memory_source, tracer);

        // bits  0..6   opcode
        // bits  7..11  rd
        // bits 12..14  funct3
        // bits 15..19  rs1
        // bits 20..24  rs2 或 immediate的一部分
        // bits 25..31  funct7 或 immediate的一部分
        // 源码变量叫formal_rs1、formal_rs2。formal意思是“按固定bit位置形式上解析出来的字段”。它不一定语义上真的是rs2。
        let rd = get_rd_bits(opcode);
        let formal_rs1 = get_formal_rs1_bits(opcode);
        let formal_rs2 = get_formal_rs2_bits(opcode);
        let op = get_opcode_bits(opcode);
        let funct3 = funct3_bits(opcode);
        let funct7 = funct7_bits(opcode);

        // rs1/rs2/rd都是5-bit，所以范围是0..31
        // funct3是3-bit，所以范围是0..7
        // 这里用assert_unchecked是性能优化。它告诉编译器这些条件一定成立，不做普通运行时检查。
        unsafe {
            core::hint::assert_unchecked(formal_rs1 < 32);
            core::hint::assert_unchecked(formal_rs2 < 32);
            core::hint::assert_unchecked(rd < 32);
            core::hint::assert_unchecked(funct3 < 8);
        }
        // 先保存旧pc，然后默认把VM状态里的pc加4
        let pc = self.observable.pc;
        self.observable.pc = self.observable.pc.wrapping_add(4);

        // 无论什么指令，它都会按formal_rs1读一次寄存器，并记录slot0。
        let rs1_value = self.get_register(formal_rs1 as u32);
        tracer.trace_rs1_read(formal_rs1 as u32, rs1_value);

        // 如果不是LOAD，VM先把formal_rs2当作寄存器读，并记录slot1。
        // 如果是LOAD，不读rs2。因为LOAD的slot1要留给RAM read。
        // 为什么LOAD不读rs2？RISC-V的LW x3, 8(x1)格式是I-type：
        // rd = x3
        // rs1 = x1
        // imm = 8
        // 没有rs2。instruction里的bits 20..31是immediate，不是rs2。
        // 所以：
        // ADD:
        //   slot0 = read rs1
        //   slot1 = read rs2
        //   slot2 = write rd
        // LW:
        //   slot0 = read rs1
        //   slot1 = read RAM
        //   slot2 = write rd
        let rs2_value = if op != OPCODE_LOAD {
            let rs2_value = self.get_register(formal_rs2 as u32);
            tracer.trace_rs2_read(formal_rs2 as u32, rs2_value);

            rs2_value
        } else {
            0
        };

        let rd = rd as u32;

        match op {
            OPCODE_LUI => {
                // U format
                report_opcode("LUI");
                let imm = UTypeOpcode::imm(opcode);
                let rd_value = imm;

                let rd_old_value = self.set_register(rd, rd_value);
                tracer.trace_rd_write(rd, rd_old_value, rd_value);
            }
            OPCODE_AUIPC => {
                // U format
                report_opcode("AUIPC");
                let imm = UTypeOpcode::imm(opcode);
                let rd_value = pc.wrapping_add(imm);

                let rd_old_value = self.set_register(rd, rd_value);
                tracer.trace_rd_write(rd, rd_old_value, rd_value);
            }
            OPCODE_JAL => {
                report_opcode("JAL");
                // J format
                let mut imm: u32 = JTypeOpcode::imm(opcode);
                sign_extend(&mut imm, 21);
                let rd_value = self.observable.pc; // already incremented by 4
                let jmp_addr = pc.wrapping_add(imm); // this one is at this cycle

                if jmp_addr & 0x3 != 0 {
                    // unaligned PC
                    panic!("Unaligned jump address 0x{:08x}", jmp_addr);
                } else {
                    self.observable.pc = jmp_addr;
                }

                let rd_old_value = self.set_register(rd, rd_value);
                tracer.trace_rd_write(rd, rd_old_value, rd_value);
            }
            OPCODE_JALR => {
                report_opcode("JALR");
                // I format
                let mut imm: u32 = ITypeOpcode::imm(opcode);
                // quasi sign extend
                sign_extend(&mut imm, 12);
                let rd_value = self.observable.pc; // already incremented by 4
                                                   //  The target address is obtained by adding the 12-bit signed I-immediate
                                                   // to the register rs1, then setting the least-significant bit of the result to zero
                let jmp_addr = (rs1_value.wrapping_add(imm) & !0x1);

                if jmp_addr & 0x3 != 0 {
                    // unaligned PC
                    panic!("Unaligned jump address 0x{:08x}", jmp_addr);
                } else {
                    self.observable.pc = jmp_addr;
                }

                let rd_old_value = self.set_register(rd, rd_value);
                tracer.trace_rd_write(rd, rd_old_value, rd_value);
            }
            OPCODE_BRANCH => {
                report_opcode("BRANCH");
                // B format
                let mut imm = BTypeOpcode::imm(opcode);
                sign_extend(&mut imm, 13);
                let jmp_addr = pc.wrapping_add(imm);

                let should_jump = match funct3 {
                    0 => rs1_value == rs2_value,
                    1 => rs1_value != rs2_value,
                    4 => (rs1_value as i32) < (rs2_value as i32),
                    5 => (rs1_value as i32) >= (rs2_value as i32),
                    6 => rs1_value < rs2_value,
                    7 => rs1_value >= rs2_value,
                    _ => {
                        panic!("Unknown opcode 0x{:08x}", opcode);
                    }
                };

                if should_jump {
                    if jmp_addr & 0x3 != 0 {
                        // unaligned PC
                        panic!("Unaligned jump address 0x{:08x}", jmp_addr);
                    } else {
                        self.observable.pc = jmp_addr;
                    }
                }

                // BRANCH doesn't write to RD, and must be masked as-is it did access x0
                let rd = 0;
                let rd_old_value = self.get_register(rd);
                tracer.trace_rd_write(rd, rd_old_value, 0);
            }
            OP_IMM_SUBMASK => {
                let operand_1 = rs1_value;
                let mut imm = ITypeOpcode::imm(opcode);
                sign_extend(&mut imm, 12);
                let operand_2 = imm;
                let rd_value = match funct3 {
                    0b000 => {
                        report_opcode("ADD");
                        operand_1.wrapping_add(operand_2)
                    }
                    0b001 if funct7 == SLL_FUNCT7 => {
                        report_opcode("SLL");
                        // shift is encoded in lowest 5 bits
                        operand_1 << (operand_2 & 0x1f)
                    }
                    0b101 if funct7 == SRL_FUNCT7 => {
                        report_opcode("SRL");
                        // shift is encoded in lowest 5 bits
                        operand_1 >> (operand_2 & 0x1f)
                    }
                    0b101 if funct7 == SRA_FUNCT7 => {
                        report_opcode("SRA");
                        // Arithmetic shift right
                        // shift is encoded in lowest 5 bits

                        if Config::SUPPORT_SRA {
                            ((operand_1 as i32) >> (operand_2 & 0x1f)) as u32
                        } else {
                            panic!("Unknown opcode 0x{:08x}", opcode);
                        }
                    }
                    0b101 if funct7 == ROT_FUNCT7 => {
                        report_opcode("ROR");
                        // Arithmetic shift right
                        // shift is encoded in lowest 5 bits

                        if Config::SUPPORT_ROT {
                            operand_1.rotate_right(operand_2 & 0x1f)
                        } else {
                            panic!("Unknown opcode 0x{:08x}", opcode);
                        }
                    }
                    0b010 => {
                        report_opcode("SLT");
                        // Store less than
                        ((operand_1 as i32) < (operand_2 as i32)) as u32
                    }
                    0b011 => {
                        report_opcode("SLTU");
                        // Store less than unsigned
                        (operand_1 < operand_2) as u32
                    }
                    0b100 => {
                        report_opcode("XOR");
                        // XOR
                        operand_1 ^ operand_2
                    }
                    0b110 => {
                        report_opcode("OR");
                        // OR
                        operand_1 | operand_2
                    }
                    0b111 => {
                        report_opcode("AND");
                        // AND
                        operand_1 & operand_2
                    }
                    _ => {
                        panic!("Unknown opcode 0x{:08x}", opcode);
                    }
                };

                let rd_old_value = self.set_register(rd, rd_value);
                tracer.trace_rd_write(rd, rd_old_value, rd_value);
            }
            OP_SUBMASK => {
                let operand_1 = rs1_value;
                let operand_2 = rs2_value;

                if funct7 == M_EXT_FUNCT7 {
                    // Multiplication extension
                    let rd_value = match funct3 {
                        0b000 => {
                            report_opcode("MUL");
                            // MUL
                            if Config::SUPPORT_MUL {
                                (operand_1 as i32).wrapping_mul(operand_2 as i32) as u32
                            } else {
                                panic!("Unknown opcode 0x{:08x}", opcode);
                            }
                        }
                        0b001 => {
                            report_opcode("MULH");
                            // MULH
                            if Config::SUPPORT_MUL && Config::SUPPORT_SIGNED_MUL {
                                (((operand_1 as i32) as i64)
                                    .wrapping_mul((operand_2 as i32) as i64)
                                    >> 32) as u32
                            } else {
                                panic!("Unknown opcode 0x{:08x}", opcode);
                            }
                        }
                        0b010 => {
                            report_opcode("MULSU");
                            // MULHSU
                            if Config::SUPPORT_MUL && Config::SUPPORT_SIGNED_MUL {
                                (((operand_1 as i32) as i64)
                                    .wrapping_mul(((operand_2 as u32) as u64) as i64)
                                    >> 32) as u32
                            } else {
                                panic!("Unknown opcode 0x{:08x}", opcode);
                            }
                        }
                        0b011 => {
                            report_opcode("MULHU");
                            // MULHU
                            if Config::SUPPORT_MUL {
                                ((operand_1 as u64).wrapping_mul(operand_2 as u64) >> 32) as u32
                            } else {
                                panic!("Unknown opcode 0x{:08x}", opcode);
                            }
                        }
                        0b100 => {
                            report_opcode("DIV");
                            // DIV
                            if Config::SUPPORT_DIV && Config::SUPPORT_SIGNED_DIV {
                                if operand_2 == 0 {
                                    -1i32 as u32
                                } else {
                                    if operand_1 as i32 == i32::MIN && operand_2 as i32 == -1 {
                                        operand_1
                                    } else {
                                        ((operand_1 as i32) / (operand_2 as i32)) as u32
                                    }
                                }
                            } else {
                                panic!("Unknown opcode 0x{:08x}", opcode);
                            }
                        }
                        0b101 => {
                            report_opcode("DIVU");
                            // DIVU
                            if Config::SUPPORT_DIV {
                                if operand_2 == 0 {
                                    0xffffffff
                                } else {
                                    operand_1 / operand_2
                                }
                            } else {
                                panic!("Unknown opcode 0x{:08x}", opcode);
                            }
                        }
                        0b110 => {
                            report_opcode("REM");
                            // REM
                            if Config::SUPPORT_DIV && Config::SUPPORT_SIGNED_DIV {
                                if operand_2 == 0 {
                                    operand_1
                                } else {
                                    if operand_1 as i32 == i32::MIN && operand_2 as i32 == -1 {
                                        0u32
                                    } else {
                                        ((operand_1 as i32) % (operand_2 as i32)) as u32
                                    }
                                }
                            } else {
                                panic!("Unknown opcode 0x{:08x}", opcode);
                            }
                        }
                        0b111 => {
                            report_opcode("REMU");
                            // REMU
                            if Config::SUPPORT_DIV {
                                if operand_2 == 0 {
                                    operand_1
                                } else {
                                    operand_1 % operand_2
                                }
                            } else {
                                panic!("Unknown opcode 0x{:08x}", opcode);
                            }
                        }
                        _ => unsafe { unreachable_unchecked() },
                    };

                    let rd_old_value = self.set_register(rd, rd_value);
                    tracer.trace_rd_write(rd, rd_old_value, rd_value);
                } else {
                    // basic set
                    let rd_value = match funct3 {
                        0b000 if funct7 == 0 => {
                            report_opcode("ADD");
                            operand_1.wrapping_add(operand_2)
                        }
                        0b000 if funct7 == SUB_FUNCT7 => {
                            report_opcode("SUB");
                            operand_1.wrapping_sub(operand_2)
                        }
                        0b001 if funct7 == SLL_FUNCT7 => {
                            report_opcode("SLL");
                            // shift is encoded in lowest 5 bits
                            operand_1 << (operand_2 & 0x1f)
                        }
                        0b001 if funct7 == ROT_FUNCT7 => {
                            report_opcode("ROL");
                            // Arithmetic shift right
                            // shift is encoded in lowest 5 bits

                            if Config::SUPPORT_ROT {
                                operand_1.rotate_left(operand_2 & 0x1f)
                            } else {
                                panic!("Unknown opcode 0x{:08x}", opcode);
                            }
                        }
                        0b101 if funct7 == SRL_FUNCT7 => {
                            report_opcode("SRL");
                            // shift is encoded in lowest 5 bits
                            operand_1 >> (operand_2 & 0x1f)
                        }
                        0b101 if funct7 == SRA_FUNCT7 => {
                            report_opcode("SRA");
                            // Arithmetic shift right
                            // shift is encoded in lowest 5 bits

                            if Config::SUPPORT_SRA {
                                ((operand_1 as i32) >> (operand_2 & 0x1f)) as u32
                            } else {
                                panic!("Unknown opcode 0x{:08x}", opcode);
                            }
                        }
                        0b101 if funct7 == ROT_FUNCT7 => {
                            report_opcode("ROR");
                            // Arithmetic shift right
                            // shift is encoded in lowest 5 bits

                            if Config::SUPPORT_ROT {
                                operand_1.rotate_right(operand_2 & 0x1f)
                            } else {
                                panic!("Unknown opcode 0x{:08x}", opcode);
                            }
                        }
                        0b010 => {
                            report_opcode("SLT");
                            // Store less than
                            ((operand_1 as i32) < (operand_2 as i32)) as u32
                        }
                        0b011 => {
                            report_opcode("SLTU");
                            // Store less than unsigned
                            (operand_1 < operand_2) as u32
                        }
                        0b100 => {
                            report_opcode("XOR");
                            // XOR
                            operand_1 ^ operand_2
                        }
                        0b110 => {
                            report_opcode("OR");
                            // OR
                            operand_1 | operand_2
                        }
                        0b111 => {
                            report_opcode("AND");
                            // AND
                            operand_1 & operand_2
                        }
                        _ => {
                            panic!("Unknown opcode 0x{:08x}", opcode);
                        }
                    };

                    let rd_old_value = self.set_register(rd, rd_value);
                    tracer.trace_rd_write(rd, rd_old_value, rd_value);
                }
            }
            OPCODE_LOAD => {
                // 从指令里解析I-type immediate
                let mut imm = ITypeOpcode::imm(opcode);
                // LOAD的立即数是12-bit有符号数，所以要符号扩展。比如imm=-4在12-bit里编码成0xffc，符号扩展后变成32-bit的-4
                sign_extend(&mut imm, 12);
                // 计算最终内存地址：load_address = rs1_value + imm，用wrapping_add表示按u32溢出回绕。比如超过0xffffffff就绕回低位。
                let load_address = rs1_value.wrapping_add(imm);

                // funct3决定是哪一种LOAD：
                // funct3 = 000:
                // LB，load byte，有符号1字节

                // funct3 = 001:
                // LH，load halfword，有符号2字节

                // funct3 = 010:
                // LW，load word，4字节

                // funct3 = 100:
                // LBU，load byte unsigned，无符号1字节

                // funct3 = 101:
                // LHU，load halfword unsigned，无符号2字节
                match funct3 {
                    a @ 0 | a @ 1 | a @ 2 | a @ 4 | a @ 5 => {
                        // 这一步决定要从内存读几个字节
                        let num_bytes = match a {
                            0 | 4 => 1, // LB / LBU: 读1字节
                            1 | 5 => 2, // LH / LHU: 读2字节
                            2 => 4,     // LW: 读4字节
                            _ => unsafe { unreachable_unchecked() },
                        };
                        // Memory implementation should handle read in full. For now we only use one
                        // that doesn't step over 4 byte boundary ever, meaning even though formal address is not 4 byte aligned,
                        // loads of u8/u16/u32 are still "aligned"

                        // aligned_ram_read_value:
                        //   对齐到4字节边界后，读出来的完整32-bit word。给trace/memory argument用，证明读了哪个4字节RAM word
                        // ram_read_value:
                        //   根据load_address和num_bytes截出来的真实load结果。给VM语义用，作为真正写入rd的数据基础
                        let (aligned_ram_read_value, ram_read_value) =
                            mem_read::<M, Config>(memory_source, load_address as u64, num_bytes);
                        // 记录slot1的RAM read，把地址向下对齐到4字节边界，从 aligned address 读了一个完整RAM word
                        tracer.trace_ram_read((load_address & !0x3) as u64, aligned_ram_read_value);
                        let rd_value = if Config::SUPPORT_SIGNED_LOAD {
                            // now depending on the type of load we extend it
                            // 开始计算最终写进rd的值
                            // 因为寄存器是32-bit。但是LB只读1字节，LH只读2字节。读出来的数据要变成32-bit才能写进寄存器。
                            match a {
                                0 => {
                                    report_opcode("LB");
                                    sign_extend_8(ram_read_value)
                                }
                                1 => {
                                    report_opcode("LH");
                                    sign_extend_16(ram_read_value)
                                }
                                2 => {
                                    report_opcode("LW");
                                    ram_read_value
                                }
                                4 => {
                                    report_opcode("LBU");
                                    zero_extend_8(ram_read_value)
                                }
                                5 => {
                                    report_opcode("LHU");
                                    zero_extend_16(ram_read_value)
                                }
                                _ => unsafe { unreachable_unchecked() },
                            }
                        } else {
                            // now depending on the type of load we extend it
                            match a {
                                0 | 1 => {
                                    panic!("Sign extension not enabled for LOAD");
                                }
                                2 => {
                                    report_opcode("LW");
                                    ram_read_value
                                }
                                4 => {
                                    report_opcode("LBU");
                                    zero_extend_8(ram_read_value)
                                }
                                5 => {
                                    report_opcode("LHU");
                                    zero_extend_16(ram_read_value)
                                }
                                _ => unsafe { unreachable_unchecked() },
                            }
                        };

                        let rd_old_value = self.set_register(rd, rd_value);
                        tracer.trace_rd_write(rd, rd_old_value, rd_value);
                    }
                    _ => {
                        panic!("Unknown opcode 0x{:08x}", opcode);
                    }
                }
            }
            OPCODE_STORE => {
                // STORE
                let mut imm = STypeOpcode::imm(opcode);
                sign_extend(&mut imm, 12);

                let store_address = rs1_value.wrapping_add(imm);

                // store operand rs2

                // access memory
                match funct3 {
                    a @ 0 | a @ 1 | a @ 2 => {
                        let store_length = 1 << a;

                        #[cfg(feature = "opcode_stats")]
                        {
                            match store_length {
                                1 => {
                                    report_opcode("SB");
                                }
                                2 => {
                                    report_opcode("SH");
                                }
                                4 => {
                                    report_opcode("SW");
                                }
                                _ => unsafe { core::hint::unreachable_unchecked() },
                            }
                        }

                        // memory handles the write in full, whether it's aligned or not, or whatever
                        let (aligned_ram_old_value, aligned_ram_write_value) = mem_write::<M, Config>(
                            memory_source,
                            store_address as u64,
                            rs2_value,
                            store_length,
                        );
                        tracer.trace_ram_read_write(
                            (store_address & !0x3) as u64,
                            aligned_ram_old_value,
                            aligned_ram_write_value,
                        );
                    }
                    _ => {
                        panic!("Unknown opcode 0x{:08x}", opcode);
                    }
                }
            }
            OPCODE_SYSTEM => {
                // various control instructions, we implement only a subset
                const ZICSR_MASK: u8 = 0x3;
                const ZIMOP_MASK: u8 = 0x4;
                const ZIMOP_FUNCT3: u8 = 0b100;

                // if funct3 & ZIMOP_MASK == ZIMOP_MASK {
                if funct3 == ZIMOP_FUNCT3 {
                    // MOP，是SYSTEM分支里的一类特殊算术/field操作。
                    const MOP_FUNCT7_TEST: u8 = 0b1000001u8;
                    if Config::SUPPORT_MOPS && funct7 & MOP_FUNCT7_TEST == MOP_FUNCT7_TEST {
                        report_opcode("MOP");

                        use field::{Field, Mersenne31Field};

                        let mop_number = ((funct7 & 0b110) >> 1) | ((funct7 & 0b100000) >> 5);
                        let operand_1 = rs1_value;
                        let operand_2 = rs2_value;
                        let mut operand_1 = Mersenne31Field::from_nonreduced_u32(operand_1);
                        let operand_2 = Mersenne31Field::from_nonreduced_u32(operand_2);
                        match mop_number {
                            0 => {
                                operand_1.add_assign(&operand_2);
                            }
                            1 => {
                                operand_1.sub_assign(&operand_2);
                            }
                            2 => {
                                operand_1.mul_assign(&operand_2);
                            }
                            _ => {
                                panic!("Unknown opcode 0x{:08x}", opcode);
                            }
                        }
                        let rd_value = operand_1.to_reduced_u32();
                        let rd_old_value = self.set_register(rd, rd_value);
                        tracer.trace_rd_write(rd, rd_old_value, rd_value);
                    }
                } else if funct3 & ZICSR_MASK != 0 {
                    // CSR指令是I-type格式，bits 31..20通常叫csr字段。
                    // We do not support standard CSRs yet
                    assert!(Config::SUPPORT_STANDARD_CSRS == false);
                    assert!(Config::SUPPORT_ONLY_CSRRW);

                    let csr_number = ITypeOpcode::imm(opcode); // 要访问哪个特殊CSR
                    let mut rd_value = 0;
                    let mut delegation_type = 0u16;

                    // read
                    // 如果是NON_DETERMINISM_CSR，从non_determinism_source读一个值，并调用trace_non_determinism_read；
                    // 如果是MARKER_CSR，read阶段不做事；
                    // 如果是delegation CSR，只检查该machine配置允许这个CSR。
                    match csr_number {
                        NON_DETERMINISM_CSR => {
                            // 访问外部输入CSR
                            // to improve oracle usability we can try to avoid read
                            // if we intend to write, so check oracle config
                            rd_value = if ND::SHOULD_MOCK_READS_BEFORE_WRITES {
                                // all our oracle accesses are implemented via CSRRW
                                // with either rd == 0 or rs1 == 0, so if we have
                                // rd == 0 here it's just a read
                                if rd == 0 {
                                    // we consider main intention to be write into CSR,
                                    // so do NOT perform `read()`
                                    0
                                } else {
                                    // it's actually intended to read
                                    non_determinism_source.read()
                                }
                            } else {
                                non_determinism_source.read()
                            };
                            tracer.trace_non_determinism_read(rd_value);
                        }
                        MARKER_CSR => { // 访问marker CSR
                             // Do nothing here, we do the work in the write case
                        }
                        delegation_csr => {
                            // 触发某个delegation
                            // read阶段只是检查：这个machine配置是否允许这个delegation CSR。
                            // 这里的delegation可以先理解成：把某些复杂计算交给专门的delegation电路/子证明逻辑处理。
                            // 例如Blake2、U256这类计算，如果全放在main RISC-V电路里逐条指令证明，可能很重。于是Airbender允许程序通过特殊CSR触发delegation：
                            // main VM:
                            //   记录“这里请求了一次Blake2 delegation”
                            // delegation circuit:
                            //   单独证明这次Blake2计算是对的
                            // we can ignore this pass - it will be resolved below in write section
                            debug_assert!(Config::ALLOWED_DELEGATION_CSRS.contains(&delegation_csr), "Machine {:?} is not configured to support CSR number {} at pc 0x{:08x}", Config::default(), delegation_csr, pc);
                        }
                    }

                    if funct3 != 0b001 {
                        //源码检查只支持CSRRW
                        // not CSRRW
                        panic!("Unknown opcode 0x{:08x}", opcode);
                    }

                    // now write into CSR. We do not use written value,
                    // but some delegations depend on formal write event

                    match csr_number {
                        NON_DETERMINISM_CSR => {
                            delegation_type = NON_DETERMINISM_CSR as u16;
                            if ND::SHOULD_IGNORE_WRITES_AFTER_READS {
                                // if we have rs1 == 0 then we should ignore write into CSR,
                                // as our main intension was to read

                                // index of rs1
                                if formal_rs1 == 0 {
                                    // do nothing
                                } else {
                                    non_determinism_source
                                        .write_with_memory_access(&*memory_source, rs1_value);
                                }
                                // 为什么还传memory_source？因为某些外部输入源可能需要看当前VM内存状态，或者根据内存访问来处理写入。
                            } else {
                                non_determinism_source
                                    .write_with_memory_access(&*memory_source, rs1_value);
                            }
                            // non-determinism write 不额外写main trace字段。
                            tracer.trace_non_determinism_write(rs1_value);
                        }
                        // 更新VM内部marker。
                        // 它没有产生普通寄存器/RAM访问，也不返回有意义的CSR值。
                        // 后面会要求rd_value = 0。
                        MARKER_CSR => self.add_marker(),
                        delegation_csr => {
                            debug_assert!(
                                Config::ALLOWED_DELEGATION_CSRS.contains(&delegation_csr),
                                "Machine {:?} is not configured to support CSR number {}",
                                Config::default(),
                                delegation_csr
                            );
                            Self::add_delegation(delegation_csr);
                            csr_processor.process_write(
                                self,
                                delegation_csr as u16,
                                memory_source,
                                non_determinism_source,
                                tracer,
                            );
                            delegation_type = delegation_csr as u16;
                        }
                    }

                    // 如果不是NON_DETERMINISM_CSR，那么rd_value必须是0。
                    if delegation_type != NON_DETERMINISM_CSR as u16 {
                        assert_eq!(rd_value, 0);
                    }

                    let rd_old_value = self.set_register(rd, rd_value);
                    tracer.trace_rd_write(rd, rd_old_value, rd_value);
                } else {
                    panic!("Unknown opcode 0x{:08x}", opcode);
                }
            }
            _ => {
                panic!("Unknown opcode 0x{:08x}", opcode);
            }
        }

        self.count_new_cycle_for_markers();

        // 当前cycle结束；
        // timestamp前进到下一cycle；
        // 当前chunk已记录cycle数量+1。
        tracer.at_cycle_end_ext(&*self);
        // 下个cycle的slot timestamps会是：
        // current_timestamp + 0
        // current_timestamp + 1
        // current_timestamp + 2
        // current_timestamp + 3

        // 如果执行后pc等于执行前pc，则认为finished。某些结束约定可能会让pc不前进或跳回自身
        // 例如JAL/JALR跳转，或者某些结束约定可能会让pc不前进或跳回自身
        self.observable.pc == pc
    }
}

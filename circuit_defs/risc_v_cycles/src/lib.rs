#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

use crate::machine::machine_configurations::full_isa_with_delegation_no_exceptions::FullIsaMachineWithDelegationNoExceptionHandling;
use prover::cs::*;
use prover::fft::GoodAllocator;
use prover::field::Mersenne31Field;
use prover::risc_v_simulator::cycle::{IMStandardIsaConfig, MachineConfig};
use prover::tracers::oracles::main_risc_v_circuit::MainRiscVOracle;
use prover::*;

/// trace domain大小。你可以先把它理解成main RISC-V circuit这一张大表的高度上限
pub const DOMAIN_SIZE: usize = 1 << 22;
// 是一个main circuit instance最多承载的RISC-V cycle数。
// 为什么少1？后面读SetupLayout和SetupPrecomputations会看到，很多setup编码都使用trace_len - 1作为容量，最后一行会被留出来做边界或协议处理；
// 源码里SetupLayout::layout_for_lookup_size和SetupPrecomputations::get_main_domain_trace都用trace_len - 1作为表内容编码容量。
pub const NUM_CYCLES: usize = DOMAIN_SIZE - 1;
pub const LDE_FACTOR: usize = 2;
pub const LDE_SOURCE_COSETS: &[usize] = &[0, 1];
pub const TREE_CAP_SIZE: usize = 32;
/// 表示程序ROM固定上界是2MB。由于bytecode按u32存储，所以ROM_WORDS = 2^19。也就是说，get_machine看到的bytecode长度必须是2^19个u32。
pub const MAX_ROM_SIZE: usize = 1 << 21; // bytes
pub const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize = (MAX_ROM_SIZE.trailing_zeros() - 16) as usize;

pub const ALLOWED_DELEGATION_CSRS: &[u32] =
    prover::risc_v_simulator::cycle::IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS;

fn serialize_to_file<T: serde::Serialize>(el: &T, filename: &str) {
    let mut dst = std::fs::File::create(filename).unwrap();
    serde_json::to_writer_pretty(&mut dst, el).unwrap();
}

pub type Machine = FullIsaMachineWithDelegationNoExceptionHandling;

pub fn formal_machine_for_compilation() -> Machine {
    FullIsaMachineWithDelegationNoExceptionHandling
}

pub fn get_machine(
    bytecode: &[u32],
    delegation_csrs: &[u32],
) -> one_row_compiler::CompiledCircuitArtifact<field::Mersenne31Field> {
    get_machine_for_rom_bound::<ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(bytecode, delegation_csrs)
}

// ROM_ADDRESS_SPACE_SECOND_WORD_BITS表示ROM地址高位部分的宽度
pub fn get_machine_for_rom_bound<const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize>(
    bytecode: &[u32],
    delegation_csrs: &[u32],
) -> one_row_compiler::CompiledCircuitArtifact<field::Mersenne31Field> {
    // bytecode必须是2^19个u32。
    // 为什么要这么严格？
    // 因为Airbender把bytecode放进固定大小ROM表。这个ROM表是setup的一部分，要被承诺。证明系统希望布局稳定，不希望每个程序都导致一张任意大小的ROM表。
    // 所以get_padded_binary必须先把程序pad到固定上界。换句话说：
    // 原始app.bin可能很短。
    // 进入get_machine前，它已经被补成固定大小ROM image。
    assert_eq!(
        bytecode.len(),
        (1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS)) / 4
    );
    use crate::machine::machine_configurations::create_csr_table_for_delegation;
    use prover::cs::machine::machine_configurations::create_table_for_rom_image;
    use prover::cs::tables::TableType;

    // `FullIsaMachineWithDelegationNoExceptionHandling`是full ISA加delegation CSR calls，不包含exception handling logic
    // 和传统CPU不同。普通CPU遇到非法指令、未对齐访问、权限错误，可以trap。这里的main circuit没有异常处理路径。
    // 程序如果做了不被支持的行为，通常就是约束无法满足，证明失败。
    let machine = FullIsaMachineWithDelegationNoExceptionHandling;

    // create_table_for_rom_image把当前要证明的程序bytecode变成ROM lookup table。后面main circuit每个cycle根据pc查ROM，证明当前instruction来自这份bytecode。
    // Airbender证明RISC-V执行时，每个cycle都会有一个pc。电路必须证明：
    // 当前pc对应的instruction，确实来自当前程序bytecode。
    // 这件事通过ROM lookup完成。
    let rom_table = create_table_for_rom_image::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(
        &bytecode,
        TableType::RomRead.to_table_id(),
    );
    // 这个表用于约束哪些CSR值是合法delegation调用。官方文档说delegation circuits通过专用CSR值被RISC-V程序调用，每个precompile有唯一`DELEGATION_TYPE_ID`，必须和程序写入的CSR值匹配。
    let csr_table = create_csr_table_for_delegation(
        true,
        delegation_csrs,
        TableType::SpecialCSRProperties.to_table_id(),
    );

    let compiled_machine = default_compile_machine::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(
        machine,
        rom_table,
        Some(csr_table),
        DOMAIN_SIZE.trailing_zeros() as usize, //DOMAIN_SIZE.trailing_zeros() = 22
    );

    compiled_machine
}

/// Produce a RISC-V machine table driver taking into account the bytecode we want to prove and allowed
/// delegation implementations
/// 单独构造TableDriver。
/// 输出lookup table内容集合。用于main circuit每个cycle根据pc查ROM，证明当前instruction来自这份bytecode。
pub fn get_table_driver(
    bytecode: &[u32],
    delegation_csrs: &[u32],
) -> prover::cs::tables::TableDriver<Mersenne31Field> {
    get_table_driver_for_rom_bound::<ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(bytecode, delegation_csrs)
}

/// 1. assert bytecode长度符合ROM上界。
/// 2. 创建FullIsaMachineWithDelegationNoExceptionHandling。
/// 3. 调用create_table_driver(machine)，生成通用表。
/// 4. 用当前bytecode生成RomRead表，加入TableDriver。
/// 5. 用当前delegation_csrs生成SpecialCSRProperties表，加入TableDriver。
/// 6. 返回TableDriver。
pub fn get_table_driver_for_rom_bound<const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize>(
    bytecode: &[u32],
    delegation_csrs: &[u32],
) -> prover::cs::tables::TableDriver<Mersenne31Field> {
    assert_eq!(
        bytecode.len(),
        (1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS)) / 4
    );

    use crate::machine::machine_configurations::create_csr_table_for_delegation;
    use prover::cs::machine::machine_configurations::create_table_driver;
    use prover::cs::machine::machine_configurations::create_table_for_rom_image;
    use prover::cs::tables::LookupWrapper;
    use prover::cs::tables::TableType;

    let machine = FullIsaMachineWithDelegationNoExceptionHandling;
    let mut table_driver = create_table_driver::<_, _, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(machine);
    let rom_table = create_table_for_rom_image::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(
        &bytecode,
        TableType::RomRead.to_table_id(),
    );
    table_driver.add_table_with_content(TableType::RomRead, LookupWrapper::Dimensional3(rom_table));
    let csr_table = create_csr_table_for_delegation(
        true,
        delegation_csrs,
        TableType::SpecialCSRProperties.to_table_id(),
    );
    table_driver.add_table_with_content(
        TableType::SpecialCSRProperties,
        LookupWrapper::Dimensional3(csr_table),
    );

    table_driver
}

mod sealed {
    use crate::Mersenne31Field;
    use prover::cs::cs::placeholder::Placeholder;
    use prover::cs::cs::witness_placer::*;
    use prover::witness_proxy::WitnessProxy;

    include!("../generated/witness_generation_fn.rs");
}

/// 给定执行oracle / witness proxy，把具体witness值填进对应变量。
pub fn witness_eval_fn_for_gpu_tracer<'a, 'b>(
    proxy: &'_ mut SimpleWitnessProxy<
        'a,
        MainRiscVOracle<'b, IMStandardIsaConfig, impl GoodAllocator>,
    >,
) {
    use prover::cs::cs::witness_placer::scalar_witness_type_set::ScalarWitnessTypeSet;

    let fn_ptr = sealed::evaluate_witness_fn::<
        ScalarWitnessTypeSet<Mersenne31Field, true>,
        SimpleWitnessProxy<'a, MainRiscVOracle<'b, IMStandardIsaConfig, _>>,
    >;
    (fn_ptr)(proxy);
}

/// 用于刷新verifier layout和quotient source。
/// 使用dummy bytecode生成compiled machine，然后写出：
/// generated/layout
/// generated/circuit_layout.rs
/// generated/quotient.rs
pub fn generate_artifacts() {
    use std::io::Write;

    // particular bytecode doesn't matter here, we only need length, that is anyway padded to upped bound
    // 先用全零dummy bytecode填满ROM大小
    let dummy_bytecode = vec![0u32; MAX_ROM_SIZE / 4];

    let compiled_machine = get_machine(&dummy_bytecode, ALLOWED_DELEGATION_CSRS);
    serialize_to_file(&compiled_machine, "generated/layout");

    let compiled_machine = get_machine(&dummy_bytecode, ALLOWED_DELEGATION_CSRS);
    // 生成layout和quotient代码
    let (layout, quotient) = verifier_generator::generate_for_description(compiled_machine);

    let mut dst = std::fs::File::create("generated/circuit_layout.rs").unwrap();
    dst.write_all(&layout.as_bytes()).unwrap();

    let mut dst = std::fs::File::create("generated/quotient.rs").unwrap();
    dst.write_all(&quotient.as_bytes()).unwrap();
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn generate() {
        generate_artifacts();
    }
}

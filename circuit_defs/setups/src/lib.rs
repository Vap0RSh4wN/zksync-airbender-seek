#![feature(allocator_api)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

use std::alloc::Global;
use std::collections::HashMap;

use cs::machine::machine_configurations::pad_bytecode;
use cs::tables::TableDriver;
use definitions::MerkleTreeCap;
use merkle_trees::DefaultTreeConstructor;
use prover::fft::*;
use prover::field::*;
use prover::prover_stages::SetupPrecomputations;
use prover::tracers::delegation::bigint_with_control_factory_fn;
use prover::tracers::delegation::blake2_with_control_factory_fn;
use prover::tracers::oracles::delegation_oracle::DelegationCircuitOracle;
use prover::tracers::oracles::main_risc_v_circuit::MainRiscVOracle;
use prover::DEFAULT_TRACE_PADDING_MULTIPLE;
use prover::*;
use risc_v_simulator::cycle::IMStandardIsaConfig;
use risc_v_simulator::cycle::IMWithoutSignedMulDivIsaConfig;
use risc_v_simulator::cycle::IWithoutByteAccessIsaConfig;
use risc_v_simulator::cycle::IWithoutByteAccessIsaConfigWithDelegation;
use risc_v_simulator::cycle::MachineConfig;
use worker::Worker;

pub use bigint_with_control;
pub use blake2_with_compression;
pub use final_reduced_risc_v_machine;
pub use machine_without_signed_mul_div;
pub use prover;
pub use reduced_risc_v_log_23_machine;
pub use reduced_risc_v_machine;
pub use risc_v_cycles;

pub mod circuits;
pub use self::circuits::*;

pub fn pad_bytecode_for_proving(bytecode: &mut Vec<u32>) {
    pad_bytecode::<{ risc_v_cycles::MAX_ROM_SIZE as u32 }>(bytecode);
}

pub fn is_default_machine_configuration<C: MachineConfig>() -> bool {
    std::any::TypeId::of::<C>() == std::any::TypeId::of::<IMStandardIsaConfig>()
}

pub fn is_reduced_machine_configuration<C: MachineConfig>() -> bool {
    std::any::TypeId::of::<C>()
        == std::any::TypeId::of::<IWithoutByteAccessIsaConfigWithDelegation>()
}

pub fn is_machine_without_signed_mul_div_configuration<C: MachineConfig>() -> bool {
    std::any::TypeId::of::<C>() == std::any::TypeId::of::<IMWithoutSignedMulDivIsaConfig>()
}

pub fn is_final_reduced_machine_configuration<C: MachineConfig>() -> bool {
    std::any::TypeId::of::<C>() == std::any::TypeId::of::<IWithoutByteAccessIsaConfig>()
}

pub fn delegation_factories_for_machine<C: MachineConfig, A: GoodAllocator>(
) -> HashMap<u16, Box<dyn Fn() -> prover::tracers::delegation::DelegationWitness<A>>> {
    if is_default_machine_configuration::<C>()
        || is_machine_without_signed_mul_div_configuration::<C>()
    {
        // blake and bigint
        HashMap::from_iter(
            [
                (
                    blake2_with_compression::DELEGATION_TYPE_ID as u16,
                    Box::new(|| {
                        blake2_with_control_factory_fn(
                            blake2_with_compression::DELEGATION_TYPE_ID as u16,
                            blake2_with_compression::NUM_DELEGATION_CYCLES,
                            A::default(),
                        )
                    })
                        as Box<dyn Fn() -> prover::tracers::delegation::DelegationWitness<A>>,
                ),
                (
                    bigint_with_control::DELEGATION_TYPE_ID as u16,
                    Box::new(|| {
                        bigint_with_control_factory_fn(
                            bigint_with_control::DELEGATION_TYPE_ID as u16,
                            bigint_with_control::NUM_DELEGATION_CYCLES,
                            A::default(),
                        )
                    })
                        as Box<dyn Fn() -> prover::tracers::delegation::DelegationWitness<A>>,
                ),
            ]
            .into_iter(),
        )
    } else if is_reduced_machine_configuration::<C>() {
        // only blake
        HashMap::from_iter(
            [(
                blake2_with_compression::DELEGATION_TYPE_ID as u16,
                Box::new(|| {
                    blake2_with_control_factory_fn(
                        blake2_with_compression::DELEGATION_TYPE_ID as u16,
                        blake2_with_compression::NUM_DELEGATION_CYCLES,
                        A::default(),
                    )
                })
                    as Box<dyn Fn() -> prover::tracers::delegation::DelegationWitness<A>>,
            )]
            .into_iter(),
        )
    } else if is_final_reduced_machine_configuration::<C>() {
        HashMap::new() // no delegations
    } else {
        panic!("unknown machine configuration {:?}", C::default())
    }
}

/// Main RISC-V（或 reduced 等）电路的完整预计算包，供证明与 witness 生成使用。
///
/// 除约束系统外还包含 lookup 表驱动、FFT/LDE、setup commitment 预计算等。
/// setup.setup.ldes[0].trace 为 row-major 的 setup trace，可经 create_circuit_setup 转为 evaluations。
pub struct MainCircuitPrecomputations<C: MachineConfig, A: GoodAllocator, B: GoodAllocator = Global>
{
    /// 经 one_row_compiler 编译后的约束系统 artifact（trace 列布局、setup layout、quotient 等）。
    /// 描述约束长什么样。例如这一列和那一列要满足加法关系，某个lookup要查RomRead表。
    pub compiled_circuit: cs::one_row_compiler::CompiledCircuitArtifact<Mersenne31Field>,
    /// Lookup 表驱动：ROM、decoder、range、CSR/delegation 等表的统一访问入口。
    pub table_driver: TableDriver<Mersenne31Field>,
    /// FFT twiddles（复域 Mersenne31Complex），供 LDE 与证明后端使用。
    pub twiddles: Twiddles<Mersenne31Complex, A>,
    /// Low-degree extension 相关预计算（domain、coset 等）。
    pub lde_precomputations: LdePrecomputations<A>,
    /// 由固定表与 setup layout 生成的 setup 预计算；内含 ldes[0].trace 等 setup trace 数据。
    /// 根据tables和trace length生成的setup commitment / trees / LDE trace
    pub setup: SetupPrecomputations<DEFAULT_TRACE_PADDING_MULTIPLE, A, DefaultTreeConstructor>,
    /// GPU tracer 用：给定 MainRiscVOracle，通过 SimpleWitnessProxy 将 witness 写入 trace。
    pub witness_eval_fn_for_gpu_tracer: fn(&mut SimpleWitnessProxy<'_, MainRiscVOracle<'_, C, B>>),
}

/// 单个 delegation circuit（如 BLAKE2、BigInt）的完整预计算包；结构上与 main 类似但无 MachineConfig 泛型。
pub struct DelegationCircuitPrecomputations<A: GoodAllocator, B: GoodAllocator = Global> {
    pub trace_len: usize,
    pub lde_factor: usize,
    pub tree_cap_size: usize,
    /// Delegation 处理器描述：编译后的 delegation 约束、trace 长度、表等。
    pub compiled_circuit: DelegationProcessorDescription,
    pub twiddles: Twiddles<Mersenne31Complex, A>,
    pub lde_precomputations: LdePrecomputations<A>,
    pub setup: SetupPrecomputations<DEFAULT_TRACE_PADDING_MULTIPLE, A, DefaultTreeConstructor>,
    pub witness_eval_fn_for_gpu_tracer:
        fn(&mut SimpleWitnessProxy<'_, DelegationCircuitOracle<'_, B>>),
}

pub fn get_delegation_compiled_circuits_for_machine_type<C: MachineConfig>(
) -> Vec<(u32, DelegationProcessorDescription)> {
    if is_default_machine_configuration::<C>() {
        get_delegation_compiled_circuits_for_default_machine()
    } else if is_reduced_machine_configuration::<C>() {
        get_delegation_compiled_circuits_for_reduced_machine()
    } else if is_final_reduced_machine_configuration::<C>() {
        vec![]
    } else if is_machine_without_signed_mul_div_configuration::<C>() {
        get_delegation_compiled_circuits_for_machine_without_signed_mul_div_configuration()
    } else {
        panic!("unknown machine configuration {:?}", C::default())
    }
}

pub fn get_delegation_compiled_circuits_for_default_machine(
) -> Vec<(u32, DelegationProcessorDescription)> {
    let mut machines = vec![];
    machines.push((
        blake2_with_compression::DELEGATION_TYPE_ID as u32,
        blake2_with_compression::get_delegation_circuit(),
    ));
    machines.push((
        bigint_with_control::DELEGATION_TYPE_ID,
        bigint_with_control::get_delegation_circuit(),
    ));

    assert_eq!(
        machines.len(),
        IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS.len()
    );
    for i in 0..machines.len() {
        assert_eq!(
            machines[i].0,
            IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS[i]
        );
    }

    machines
}

pub fn get_delegation_compiled_circuits_for_reduced_machine(
) -> Vec<(u32, DelegationProcessorDescription)> {
    let mut machines = vec![];
    machines.push((
        blake2_with_compression::DELEGATION_TYPE_ID as u32,
        blake2_with_compression::get_delegation_circuit(),
    ));

    assert_eq!(
        machines.len(),
        IWithoutByteAccessIsaConfigWithDelegation::ALLOWED_DELEGATION_CSRS.len()
    );
    for i in 0..machines.len() {
        assert_eq!(
            machines[i].0,
            IWithoutByteAccessIsaConfigWithDelegation::ALLOWED_DELEGATION_CSRS[i]
        );
    }

    machines
}

/// 生成当前启用的全部 delegation circuit setup 列表。
///
/// 返回 [(delegation_type_id, DelegationCircuitPrecomputations)]。
/// delegation_type_id 须与 RISC-V 程序通过 CSR 调用 delegation 时写入的值一致。
/// 当前包含：BLAKE2 compression、BigInt with control（不依赖具体程序 bytecode）。
pub fn all_delegation_circuits_precomputations<A: GoodAllocator, B: GoodAllocator>(
    worker: &Worker,
) -> Vec<(u32, DelegationCircuitPrecomputations<A, B>)> {
    vec![
        (
            blake2_with_compression::DELEGATION_TYPE_ID,
            get_blake2_with_compression_circuit_setup(worker),
        ),
        (
            bigint_with_control::DELEGATION_TYPE_ID,
            get_bigint_with_control_circuit_setup(worker),
        ),
        // (
        //     blake2_single_round::DELEGATION_TYPE_ID,
        //     get_blake2_single_round_circuit_setup(worker),
        // ),
        // (
        //     poseidon2_compression_with_witness::DELEGATION_TYPE_ID,
        //     get_poseidon2_compress_with_witness_circuit_setup(worker),
        // ),
    ]
}

pub fn get_delegation_compiled_circuits_for_machine_without_signed_mul_div_configuration(
) -> Vec<(u32, DelegationProcessorDescription)> {
    let mut machines = vec![];
    machines.push((
        blake2_with_compression::DELEGATION_TYPE_ID as u32,
        blake2_with_compression::get_delegation_circuit(),
    ));
    machines.push((
        bigint_with_control::DELEGATION_TYPE_ID,
        bigint_with_control::get_delegation_circuit(),
    ));

    assert_eq!(
        machines.len(),
        IMWithoutSignedMulDivIsaConfig::ALLOWED_DELEGATION_CSRS.len()
    );
    for i in 0..machines.len() {
        assert_eq!(
            machines[i].0,
            IMWithoutSignedMulDivIsaConfig::ALLOWED_DELEGATION_CSRS[i]
        );
    }

    machines
}

pub mod all_parameters {
    use verifier_common::prover::definitions::MerkleTreeCap;
    include!("../generated/all_delegation_circuits_params.rs");
}

pub const CAP_SIZE: usize = 64;
pub const NUM_COSETS: usize = 2;

pub fn generate_artifacts() -> String {
    use prover::cap_holder::array_to_tokens;
    use prover::merkle_trees::MerkleTreeConstructor;
    use quote::quote;

    let worker = prover::worker::Worker::new();
    let all_circuits = all_delegation_circuits_precomputations::<Global, Global>(&worker);
    let mut streams = Vec::with_capacity(all_circuits.len());
    for (delegation_type, prec) in all_circuits.into_iter() {
        let delegation_type = delegation_type as u32;
        let num_delegation_requests = prec.trace_len as u32;
        let setup = DefaultTreeConstructor::dump_caps(&prec.setup.trees);
        let setup: [MerkleTreeCap<CAP_SIZE>; NUM_COSETS] = setup
            .into_iter()
            .map(|el| MerkleTreeCap {
                cap: el.cap.try_into().unwrap(),
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let caps_stream = array_to_tokens(&setup);
        let t = quote! {
            (
                #delegation_type,
                #num_delegation_requests,
                #caps_stream
            )
        };
        streams.push(t);
    }

    use quote::TokenStreamExt;

    let mut full_stream = proc_macro2::TokenStream::new();
    full_stream.append_separated(
        streams.into_iter().map(|el| {
            quote! { #el }
        }),
        quote! {,},
    );

    let cap_size = CAP_SIZE;
    let num_cosets = NUM_COSETS;

    let description = quote! {
        pub const ALL_DELEGATION_CIRCUITS_PARAMS: &[(u32, u32, [MerkleTreeCap<#cap_size>; #num_cosets])] = & [#full_stream];
    }.to_string();

    description
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn generate_all() {
        let description = generate_artifacts();

        let mut dst = std::fs::File::create("generated/all_delegation_circuits_params.rs").unwrap();
        use std::io::Write;
        dst.write_all(&description.as_bytes()).unwrap();
    }
}

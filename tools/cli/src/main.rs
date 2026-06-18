//! Airbender CLI 可执行入口（cargo run -p cli -- 子命令）。
//!
//! 本文件只负责解析命令行参数，并把证明、验证、运行等具体工作分发给 cli_lib 下的模块。
//! 它本身不直接编译约束系统，也不调用 get_main_riscv_circuit_setup。
//!
//! 第二章主线（从 prove 到 main RISC-V setup）在此处的分界是：
//! CLI argv -> Cli::parse() -> Commands::Prove
//!   -> fetch_input_data(input)
//!   -> create_proofs(...)   // 见 prover_utils.rs
//!
//! tools/cli/src/setup.rs 中的 SetupCache 是旁支缓存封装，当前 Commands::Prove 主路径不经过它。

#![feature(allocator_api)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

use base64::Engine;
use blake2s_u32::Blake2sState;
use clap::{Parser, Subcommand};
// 证明相关逻辑集中在 prover_utils.rs；main.rs 只导入并调用，不内联实现证明。
// create_proofs：CLI prove 的第一层封装，加载 binary、计算 instance 数、选择 CPU/GPU 路径。
// create_final_proofs_from_program_proof：ProveFinal 子命令用。
// generate_oracle_data_from_metadata / serialize_to_file / u32_from_hex_string：验证与序列化辅助。
// ProvingLimit / DEFAULT_CYCLES：递归停止条件与默认 cycle 上限（32_000_000）。
use cli_lib::prover_utils::{
    create_final_proofs_from_program_proof, create_proofs, generate_oracle_data_from_metadata,
    serialize_to_file, u32_from_hex_string, ProvingLimit, DEFAULT_CYCLES,
};

use cli_lib::vk::generate_vk;
// Machine：决定 create_proofs_internal 进入 Standard / Reduced / ReducedLog23 哪条分支。
// Standard 用于 base proving（full ISA main RISC-V）；Reduced 系列主要用于递归层。
use execution_utils::{
    generate_constants_for_binary, Machine, ProgramProof, RecursionStrategy,
    VerifierCircuitsIdentifiers,
};
use reqwest::blocking::Client;
use serde_json::Value;
use std::path::Path;
use std::{fs, io::Write, iter};

use prover::{
    merkle_trees::{MerkleTreeCapVarLength, MerkleTreeConstructor},
    prover_stages::Proof,
    risc_v_simulator::{
        abstractions::non_determinism::QuasiUARTSource,
        cycle::{
            IMStandardIsaConfig, IWithoutByteAccessIsaConfig,
            IWithoutByteAccessIsaConfigWithDelegation,
        },
        runner::run_simple_with_entry_point_and_non_determimism_source_for_config,
        sim::SimulatorConfig,
    },
};

fn deserialize_from_file<T: serde::de::DeserializeOwned>(filename: &str) -> T {
    let src = std::fs::File::open(filename).unwrap();
    serde_json::from_reader(src).unwrap()
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Debug, clap::ValueEnum, Parser)]
enum InputType {
    Hex,
    ProverInputJson,
}
impl Default for InputType {
    fn default() -> Self {
        InputType::Hex
    }
}

#[derive(Clone, Debug, Parser, Default)]
struct InputConfig {
    /// 从本地文件读取非确定输入（与 input_rpc 二选一）。
    #[arg(long)]
    input_file: Option<String>,

    /// 输入编码方式：Hex 为连续 hex 字符串；ProverInputJson 从 JSON 的 prover_input 字段读取（仅配合 input_file）。
    #[arg(long, value_enum, default_value = "hex")]
    input_type: InputType,

    /// 从 JSON-RPC URL 拉取输入（需同时设置 input_batch）。
    #[arg(long)]
    input_rpc: Option<String>,
    /// 配合 input_rpc 使用的批次号，用于拼接下载 URL。
    #[arg(long)]
    input_batch: Option<u64>,
}

#[derive(Subcommand)]
enum Commands {
    /// 证明一条 RISC-V 程序执行轨迹（第二章主线入口）。
    ///
    /// CLI 在此处分界：解析参数后调用 create_proofs，不直接编译约束系统。
    Prove {
        /// 待证明的 RISC-V binary 文件路径；后续会 pad 成固定 ROM 大小的 Vec<u32>。
        #[arg(short, long)]
        bin: String,
        /// 非确定输入：可来自文件或 RPC（hex 或 JSON prover_input），供 guest 通过 oracle 读取。
        #[clap(flatten)]
        input: InputConfig,
        /// 证明产物输出目录。
        #[arg(long, default_value = "output")]
        output_dir: String,

        /// 最终 program proof 的文件名（用于 until=final-proof 等场景）。
        #[arg(long, default_value = "final_program_proof.json")]
        final_proof_name: String,

        /// 机器类型，默认 standard，对应 Machine::Standard 与 main RISC-V base proving。
        #[arg(long, value_enum, default_value = "standard")]
        machine: Machine,
        /// 递归证明时传入的上一段 metadata 路径；base proving 通常不传。
        #[arg(long)]
        prev_metadata: Option<String>,
        /// 最多执行的 RISC-V cycle 数，默认 DEFAULT_CYCLES（32_000_000）。
        #[arg(long)]
        cycles: Option<usize>,

        /// 若设置，在 base proving 后继续跑递归，直到指定阶段（FinalRecursion / FinalProof / Snark）。
        #[arg(long)]
        until: Option<ProvingLimit>,
        /// 递归层使用的机器策略（如 use-reduced-log23-machine）。
        #[arg(long, value_enum, default_value = "use-reduced-log23-machine")]
        mode: RecursionStrategy,

        /// 中间证明与 metadata 的临时目录（递归路径可选）。
        #[arg(long)]
        tmp_dir: Option<String>,
        /// 为 true 时走 GPU proving，会绕开 CPU 路径里对 get_main_riscv_circuit_setup 的直接调用。
        #[arg(long)]
        gpu: bool,
    },
    /// Run the 'final' step of proving (for example on the output from ZKSmith)
    ProveFinal {
        // Either load data from the input file or from RPC
        #[clap(flatten)]
        input: InputConfig,
        #[arg(long, default_value = "output")]
        output_dir: String,
        #[arg(long, value_enum, default_value = "use-reduced-log23-machine")]
        mode: RecursionStrategy,
        /// If true, use GPU for proving.
        #[arg(long)]
        gpu: bool,
    },
    /// Verifies a single proof.
    Verify {
        /// Path to proof file.
        #[arg(short, long)]
        proof: String,
    },
    /// Verifies whole run (potentially multiple proofs)
    VerifyAll {
        #[arg(short, long)]
        metadata: Option<String>,

        #[arg(short, long)]
        program_proof: Option<String>,
    },
    Run {
        #[arg(short, long)]
        bin: String,
        // Either load data from the input file or from RPC
        #[clap(flatten)]
        input: InputConfig,
        /// Number of riscV cycles to run. 32_000_000 if not set.
        #[arg(long)]
        cycles: Option<usize>,
        /// If present - compare the register values with results.
        #[arg(long, num_args = 1.., value_delimiter = ',')]
        expected_results: Option<Vec<u32>>,

        #[arg(long, value_enum, default_value = "standard")]
        machine: Machine,
    },

    /// Generates verification key hash, for a given binary.
    /// This way you can compare it with the one inside the proof, to make sure that
    /// the proof is really checking the execution of a given code.
    GenerateVk {
        #[arg(short, long)]
        bin: String,
        #[arg(long)]
        machine: Option<Machine>,
        #[arg(long)]
        output: Option<String>,
    },

    Flatten {
        #[arg(long)]
        input_file: String,
        #[arg(long)]
        output_file: String,
    },
    FlattenAll {
        #[arg(long)]
        input_metadata: String,
        #[arg(long)]
        output_file: String,
    },
    /// Combines two proofs into a single one.
    /// This is used to combine the proof from the previous block with the current one.
    /// Both proofs must have the same recursion chain hash.
    FlattenTwo {
        #[arg(long)]
        first_metadata: String,
        #[arg(long)]
        second_metadata: String,
        #[arg(long)]
        output_file: String,
    },
    /// Generate End params and AUX values for a given binary and verification path.
    // These can be considered quasi 'verification' keys - as they tie the final proof
    // to the original bytecode (and verifications).
    GenerateConstants {
        #[arg(short, long)]
        bin: String,
        /// If true, use the universal verifier (used by the cli tool).
        /// If false, use separate verifiers.
        #[arg(long)]
        universal_verifier: bool,
        /// If true recompute all the verification keys.
        /// If false, use the ones from the vk.json files.
        #[arg(long)]
        recompute: bool,
        #[arg(long, value_enum, default_value = "use-reduced-log23-machine")]
        mode: RecursionStrategy,
    },
}

fn fetch_data_from_json_rpc(url: &str) -> Result<Option<String>, reqwest::Error> {
    let client = Client::new();

    let response = client.post(url).send()?.json::<Value>()?;

    match &response["result"] {
        Value::String(data) => {
            let tmp_data = data.strip_prefix("0x").unwrap_or(&data);
            Ok(Some(tmp_data.to_string()))
        }
        _ => Ok(None),
    }
}

fn fetch_data_from_url(url: &str) -> Result<Option<String>, reqwest::Error> {
    let client = Client::new();
    let response = client.get(url).send()?.text()?;
    Ok(Some(response))
}

// 读取 Prove / Run 的外部输入，返回 Option<Vec<u32>>；无输入源时返回 None。
// 下游：create_proofs 中变为 non_determinism_data，再进入 QuasiUARTSource.oracle。
fn fetch_input_data(input: &InputConfig) -> Result<Option<Vec<u32>>, reqwest::Error> {
    // 按 input_file 或 input_rpc 选择数据来源，并带上对应的 input_type。
    let (data, input_type) = if let Some(input_file) = &input.input_file {
        (
            Some(fs::read_to_string(input_file).unwrap().trim().to_string()),
            input.input_type.clone(),
        )
    // RPC 路径固定按 ProverInputJson 解析。
    } else if let Some(url) = &input.input_rpc {
        (fetch_data_from_json_rpc(&url)?, InputType::ProverInputJson)
    } else {
        return Ok(None);
    };

    match input_type {
        // Hex：每 8 个十六进制字符解析为一个 u32。
        InputType::Hex => Ok(data.map(|d| u32_from_hex_string(&d))),
        InputType::ProverInputJson => {
            if let Some(data) = data {
                let json: Value = serde_json::from_str(&data).expect("Failed to parse JSON");
                // JSON 中 prover_input 字段，通常为 base64 编码的字节串。
                let prover_input = json["prover_input"].as_str().unwrap_or_default();

                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&prover_input)
                    .expect("Failed to decode base64 input");

                // 按 4 字节小端切成 u32，与 guest 读 oracle 时的字序一致。
                let prover_input: Vec<u32> = decoded
                    .chunks_exact(4)
                    .map(|chunk| u32::from_le_bytes(chunk.try_into().unwrap()))
                    .collect();
                Ok(Some(prover_input))
            } else {
                Ok(None)
            }
        }
    }
}

fn fetch_final_input_json(input: &InputConfig) -> Result<Option<String>, reqwest::Error> {
    if let Some(input_file) = &input.input_file {
        Ok(Some(
            fs::read_to_string(input_file).unwrap().trim().to_string(),
        ))
    } else if let Some(url) = &input.input_rpc {
        let batch = input
            .input_batch
            .expect("input_batch must be set if input_rpc is set");
        fetch_data_from_url(format!("{}/downloads/{}", url, batch).as_str())
    } else {
        Ok(None)
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .format_module_path(false)
        .format_target(false)
        .init();
    let cli = Cli::parse();
    match &cli.command {
        Commands::Prove {
            bin,             // 要证明的RISC-V binary
            input,           // 非确定输入，可以来自文件或RPC
            output_dir,
            final_proof_name,
            machine,         // 选择机器类型，默认是standard
            prev_metadata,
            cycles,          // 控制最多跑多少RISC-V cycles
            until,           // 主要和递归证明相关
            mode,            // 主要和递归证明相关
            tmp_dir,         // 主要和递归证明相关
            gpu,             // 决定走CPU还是GPU proving路径
        } => {
            // 第二章 2.2：把 CLI input 转成 Vec<u32> 非确定输入。
            let input_data = fetch_input_data(input).expect("Failed to fetch");
            // 第二章 2.3：进入 prove 第一层封装；不直接调用 get_main_riscv_circuit_setup。
            create_proofs(
                bin,
                output_dir,
                final_proof_name,
                input_data,
                prev_metadata,
                machine,
                cycles,
                until,
                *mode,
                tmp_dir,
                gpu.clone(),
            );
        }
        Commands::ProveFinal {
            input,
            output_dir,
            mode,
            gpu,
        } => {
            let input = fetch_final_input_json(input).expect("Failed to fetch");

            let input_program_proof: ProgramProof = serde_json::from_str(&input.unwrap())
                .expect("Failed to parse input_hex into ProgramProof");

            let program_proof =
                create_final_proofs_from_program_proof(input_program_proof, *mode, *gpu);

            serialize_to_file(
                &program_proof,
                &Path::new(output_dir).join("final_program_proof.json"),
            );
        }
        Commands::Verify { proof } => {
            #[cfg(feature = "include_verifiers")]
            verify_proof(proof);
            #[cfg(not(feature = "include_verifiers"))]
            {
                let _ = proof;
                panic!("Not enabled - please compile with `include_verifiers` feature.")
            }
        }
        Commands::VerifyAll {
            metadata,
            program_proof,
        } => {
            #[cfg(feature = "include_verifiers")]
            {
                if let Some(metadata) = metadata {
                    verify_all(metadata);
                } else if let Some(program_proof) = program_proof {
                    verify_all_program_proof(program_proof);
                } else {
                    panic!("Please either provide --metadata or --program_proof");
                }
            }
            #[cfg(not(feature = "include_verifiers"))]
            {
                let _ = metadata;
                let _ = program_proof;
                panic!("Not enabled - please compile with `include_verifiers` feature.")
            }
        }
        Commands::Run {//run 只验证程序语义，不生成 proof，不调用 get_main_riscv_circuit_setup。
            bin,
            cycles,
            input,
            expected_results,
            machine,
        } => {
            let input_data = fetch_input_data(input).expect("Failed to fetch");

            run_binary(bin, cycles, input_data, expected_results, machine);
        }
        Commands::GenerateVk {
            bin,
            machine,
            output,
        } => generate_vk(bin, machine, output),
        Commands::Flatten {
            input_file,
            output_file,
        } => flatten_file(input_file, output_file),
        Commands::FlattenAll {
            input_metadata,
            output_file,
        } => flatten_all(input_metadata, output_file),
        Commands::FlattenTwo {
            first_metadata,
            second_metadata,
            output_file,
        } => flatten_two(first_metadata, second_metadata, output_file),
        Commands::GenerateConstants {
            bin,
            universal_verifier,
            recompute,
            mode,
        } => {
            let base_layer_bin = std::fs::read(bin).expect("Failed to read base layer binary file");

            let (end_params, aux_values) = generate_constants_for_binary(
                &base_layer_bin,
                *mode,
                *universal_verifier,
                *recompute,
            );

            println!("End params: {:?}", end_params);
            println!("Aux values: {:?}", aux_values);
        }
    }
}

/// Computes a single hash for multiple trees.
pub fn merkle_trees_to_hash<T: MerkleTreeConstructor>(trees: &Vec<T>) -> String {
    let caps = trees.iter().map(|x| x.get_cap()).collect::<Vec<_>>();
    merkle_caps_to_hash(&caps)
}

/// Computes a single hash for multiple tree caps.
pub fn merkle_caps_to_hash(caps: &Vec<MerkleTreeCapVarLength>) -> String {
    let mut all_leaves = vec![];
    for cap in caps {
        all_leaves.append(&mut cap.cap.clone());
    }
    let mut hasher = Blake2sState::new();
    for entry in all_leaves {
        let mut result = [0u32; 16];
        // yes, this is very lazy - as we just copy 8 uint32, and the remaining 8 are zero.
        result[..8].copy_from_slice(&entry);
        hasher.absorb::<true>(&result);
    }
    let empty = [0u32; 16];
    let mut dst = [0u32; 8];
    hasher.absorb_final_block::<true>(&empty, 0, &mut dst);

    dst.iter()
        .map(|value| format!("{:08x}", value))
        .collect::<Vec<_>>()
        .join("")
}

#[derive(Debug)]
pub enum CircuitType {
    RiscV,
    RiscVReduced,
    DelegatedExtendedBlake,
}

pub fn proof_name_to_circuit_type(file_name: &str) -> CircuitType {
    if file_name.starts_with("delegation_proof_1991_") {
        CircuitType::DelegatedExtendedBlake
    } else if file_name.starts_with("proof_") {
        CircuitType::RiscV
    } else if file_name.starts_with("reduced_proof_") {
        CircuitType::RiscVReduced
    } else {
        panic!("Failed to map file {} to a proof type.", file_name);
    }
}

#[cfg(feature = "include_verifiers")]
fn verify_proof(proof_path: &String) {
    use cli_lib::prover_utils::get_end_params_output_suffix_from_proof;

    println!("Verifying proof from {}", proof_path);
    let proof: Proof = deserialize_from_file(proof_path);

    let end_params_output = get_end_params_output_suffix_from_proof(&proof);
    println!("Final params hash: {:?}", end_params_output);

    let verification_key = merkle_caps_to_hash(&proof.setup_tree_caps);
    println!("Proof verification key is {}", verification_key);

    let circuit_type = proof_name_to_circuit_type(
        std::path::Path::new(proof_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
    );

    println!("Circuit type detected as {:?}", circuit_type);

    let shuffle_ram_inits_and_teardowns: bool = match circuit_type {
        CircuitType::RiscV => true,
        CircuitType::RiscVReduced => true,
        CircuitType::DelegatedExtendedBlake => false,
    };

    let mut oracle_data = vec![];

    oracle_data.extend(
        verifier_common::proof_flattener::flatten_proof_for_skeleton(
            &proof,
            shuffle_ram_inits_and_teardowns,
        ),
    );
    for query in proof.queries.iter() {
        oracle_data.extend(verifier_common::proof_flattener::flatten_query(query));
    }

    let it = oracle_data.into_iter();

    verifier_common::prover::nd_source_std::set_iterator(it);

    match circuit_type {
        CircuitType::RiscV => unsafe {
            risc_v_cycles_verifier::verify(
                std::mem::MaybeUninit::uninit().assume_init_mut(),
                &mut verifier_common::ProofPublicInputs::uninit(),
            )
        },
        CircuitType::RiscVReduced => unsafe {
            reduced_risc_v_machine_verifier::verify(
                std::mem::MaybeUninit::uninit().assume_init_mut(),
                &mut verifier_common::ProofPublicInputs::uninit(),
            )
        },
        CircuitType::DelegatedExtendedBlake => {
            unsafe {
                blake2_with_compression_verifier::verify(
                    std::mem::MaybeUninit::uninit().assume_init_mut(),
                    &mut verifier_common::ProofPublicInputs::uninit(),
                )
            };
        }
    }
    println!("PROOF IS VALID");
}

fn flatten_file(input_file: &String, output_file: &String) {
    let proof: Proof = deserialize_from_file(input_file);
    //let compiled_circuit: CompiledCircuitArtifact<Mersenne31Field> =
    //        deserialize_from_file("../../prover/delegation_layout");
    let shuffle_ram_inits_and_teardowns = true;

    let mut data = vec![VerifierCircuitsIdentifiers::RiscV as u32];
    // FIXME: this should detect the type of the proof.
    data.extend(
        verifier_common::proof_flattener::flatten_proof_for_skeleton(
            &proof,
            shuffle_ram_inits_and_teardowns,
        ),
    );

    for query in proof.queries.iter() {
        data.extend(verifier_common::proof_flattener::flatten_query(query));
    }

    u32_to_file(output_file, &data);

    let foo = u32_from_file(output_file);
    assert_eq!(foo, data);
}

fn flatten_all(input_metadata: &String, output_file: &String) {
    let (metadata, mut oracle) = generate_oracle_data_from_metadata(input_metadata);

    if metadata.basic_proof_count > 0 {
        oracle.insert(0, VerifierCircuitsIdentifiers::BaseLayer as u32);
    } else if metadata.reduced_proof_count > 0 {
        oracle.insert(0, VerifierCircuitsIdentifiers::RecursionLayer as u32);
    } else {
        panic!("No proofs");
    };

    u32_to_file(output_file, &oracle);
}

fn flatten_two(first_metadata: &String, second_metadata: &String, output_file: &String) {
    let (metadata, mut oracle) = generate_oracle_data_from_metadata(first_metadata);
    let (metadata2, oracle2) = generate_oracle_data_from_metadata(second_metadata);

    oracle.extend(oracle2);
    assert!(metadata.reduced_proof_count > 0);
    assert!(metadata2.reduced_proof_count > 0);

    oracle.insert(
        0,
        VerifierCircuitsIdentifiers::CombinedRecursionLayers as u32,
    );

    u32_to_file(output_file, &oracle);
}

#[cfg(feature = "include_verifiers")]
fn verify_all(metadata_path: &String) {
    let (metadata, oracle_data) = generate_oracle_data_from_metadata(metadata_path);
    let it = oracle_data.into_iter();

    verifier_common::prover::nd_source_std::set_iterator(it);

    if metadata.basic_proof_count > 0 {
        assert_eq!(metadata.reduced_proof_count, 0);
        let output = full_statement_verifier::verify_base_layer();
        println!("Output is: {:?}", output);
    } else if metadata.reduced_proof_count > 0 {
        println!("Running continue recursive");
        assert!(metadata.reduced_proof_count > 0);
        let output = full_statement_verifier::verify_recursion_layer();
        println!("Output is: {:?}", output);
    } else if metadata.reduced_log_23_proof_count > 0 {
        todo!("not implemented yet");
    } else {
        panic!("No proofs");
    };
    assert!(
        verifier_common::prover::nd_source_std::try_read_word().is_none(),
        "Expected that all words from CSR were consumed"
    );
}

#[cfg(feature = "include_verifiers")]
fn verify_all_program_proof(program_proof_path: &String) {
    use execution_utils::generate_oracle_data_from_metadata_and_proof_list;

    let input_program_proof: ProgramProof = deserialize_from_file(&program_proof_path);
    //serde_json::from_str(&input.unwrap()).expect("Failed to parse input_hex into ProgramProof");
    let (metadata, proof_list) = input_program_proof.to_metadata_and_proof_list();

    let oracle_data = generate_oracle_data_from_metadata_and_proof_list(&metadata, &proof_list);
    let it = oracle_data.into_iter();

    verifier_common::prover::nd_source_std::set_iterator(it);

    // Assume that program proof has only recursion proofs.
    println!("Running continue recursive");
    assert!(metadata.reduced_proof_count > 0);
    let output = full_statement_verifier::verify_recursion_layer();
    println!("Output is: {:?}", output);

    assert!(
        verifier_common::prover::nd_source_std::try_read_word().is_none(),
        "Expected that all words from CSR were consumed"
    );
}

fn u32_from_file(input_file: &String) -> Vec<u32> {
    let hex_string = fs::read_to_string(input_file).unwrap().trim().to_string();

    u32_from_hex_string(&hex_string)
}

fn u32_to_file(output_file: &String, numbers: &[u32]) {
    // Open the file for writing
    let mut file = fs::File::create(output_file).expect("Failed to create file");

    // Write each u32 as an 8-character hexadecimal string without newlines
    for &num in numbers {
        write!(file, "{:08X}", num).expect("Failed to write to file");
    }

    println!("Successfully wrote to file: {}", output_file);
}

// 运行RISC-V binary，并返回最终寄存器值和是否执行到结束点
// 只验证程序语义，不生成 proof，不调用 get_main_riscv_circuit_setup。
fn run_binary(
    bin_path: &String,
    cycles: &Option<usize>,
    input_data: Option<Vec<u32>>,
    expected_results: &Option<Vec<u32>>,
    machine: &Machine,
) {
    // SimulatorConfig {
    //     bin: Path(app.bin),
    //     cycles: DEFAULT_CYCLES = 32_000_000,
    //     entry_point: 0,
    //   }
    let config = SimulatorConfig {
        bin: prover::risc_v_simulator::sim::BinarySource::Path(bin_path.into()),
        cycles: cycles.unwrap_or(DEFAULT_CYCLES),
        entry_point: 0,
        diagnostics: None,
    };
    // 因为 input_data = None，所以 oracle 队列为空
    let mut non_determinism_source = QuasiUARTSource::default();
    if let Some(input_data) = input_data {
        for entry in input_data {
            non_determinism_source.oracle.push_back(entry);
        }
    }

    // 输出：
    // registers: 模拟器最终寄存器
    // reached_end: 是否执行到 zksync_os_finish_success / 结束点
    let (registers, reached_end) = match machine {
        Machine::Standard => {
            let result = run_simple_with_entry_point_and_non_determimism_source_for_config::<
                _,
                IMStandardIsaConfig,
            >(config, non_determinism_source);

            (result.state.registers, result.reached_end)
        }
        Machine::Reduced => {
            let result = run_simple_with_entry_point_and_non_determimism_source_for_config::<
                _,
                IWithoutByteAccessIsaConfigWithDelegation,
            >(config, non_determinism_source);

            (result.state.registers, result.reached_end)
        }
        Machine::ReducedLog23 => {
            let result = run_simple_with_entry_point_and_non_determimism_source_for_config::<
                _,
                IWithoutByteAccessIsaConfigWithDelegation,
            >(config, non_determinism_source);

            (result.state.registers, result.reached_end)
        }
        Machine::ReducedFinal => {
            let result = run_simple_with_entry_point_and_non_determimism_source_for_config::<
                _,
                IWithoutByteAccessIsaConfig,
            >(config, non_determinism_source);

            (result.state.registers, result.reached_end)
        }
    };

    if !reached_end {
        println!("WARNING: execution did not finish; most likely ran out of cycles!");
    }

    // our convention is to return 32 bytes placed into registers x10-x17

    let result = registers[10..26]
        .iter()
        .map(|x| format!("{}", x))
        .collect::<Vec<_>>()
        .join(", ");
    println!("Result: {}", result);
    if let Some(expected_results) = expected_results {
        // let's compare registers to expected results.
        // expected results can be shorter - so pad with zeros.
        for (i, (a, b)) in registers[10..18]
            .iter()
            .zip(expected_results.iter().chain(iter::repeat(&0)))
            .enumerate()
        {
            if a != b {
                panic!(
                    "Expected results differ on {}: register: {} expected: {}",
                    i, a, b
                );
            }
        }
    }
}

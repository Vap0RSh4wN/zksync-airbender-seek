use std::{collections::BTreeMap, path::Path};

use trace_and_split::FinalRegisterValue;
use verifier_common::{
    blake2s_u32::BLAKE2S_DIGEST_SIZE_U32_WORDS, cs::utils::split_timestamp,
    prover::prover_stages::Proof,
};

// Structures to serialize / deserialize airbender proofs.
// ProgramProof = ProofMetadata + ProofList.
//
// For large programs, there can be 100s of proofs, so serialization via ProgramProof might be slow.
// That's why the alternative is to serialize ProofMetadata into one file, and put proofs into separate files.

/// This struct contains the proof data for a single program execution.
/// It has both metadata and proofs themselves.
#[derive(Clone, Debug, Hash, serde::Serialize, serde::Deserialize)]
pub struct ProgramProof {
    pub base_layer_proofs: Vec<Proof>,
    pub delegation_proofs: BTreeMap<u32, Vec<Proof>>,
    pub register_final_values: Vec<FinalRegisterValue>,
    pub end_params: [u32; 8],
    pub recursion_chain_preimage: Option<[u32; 16]>,
    pub recursion_chain_hash: Option<[u32; 8]>,
}

/// 一组证明的元数据摘要，不包含 STARK proof 本体（proof 字节在 ProofList 或分散的 json 文件中）。
///
/// 与 ProofList 配对使用：metadata.json 记数量和绑定参数，proof_N.json 等存具体证明。
/// 大程序可能有上百个 proof，拆开后避免把所有 proof 塞进一个 JSON。全塞进一个 JSON 会很慢，所以用 ProofMetadata 描述「有哪些、各几个」，再用 ProofList::load_from_directory 按计数逐个加载文件。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct ProofMetadata {
    /// main RISC-V machine（Standard / base layer）产生的 basic proof 个数。
    /// 对应磁盘文件名 proof_0.json、proof_1.json 等；load_from_directory 按此计数加载。
    pub basic_proof_count: usize,
    /// reduced RISC-V machine（递归第一层常用）产生的 proof 个数。
    /// 对应 reduced_proof_0.json 等；与 basic 互斥出现在同一轮 metadata 的情况较常见。
    pub reduced_proof_count: usize,
    /// reduced log23 machine（更小 trace domain，递归第二层常用）产生的 proof 个数。
    /// 对应 reduced_log_23_proof_0.json 等。
    pub reduced_log_23_proof_count: usize,

    /// 已废弃：旧版 final layer 计数；反序列化时兼容 JSON 字段名 final_proof_count，新代码应恒为 0。
    #[serde(alias = "final_proof_count")]
    pub deprecated_final_proof_count: usize,

    /// 各 delegation circuit 的 proof 数量列表：[(delegation_type_id, count), ...]。
    /// delegation_type_id 与 CSR 写入值一致（如 BLAKE2、BigInt）；文件名为 delegation_proof_{id}_{i}.json。
    pub delegation_proof_count: Vec<(u32, usize)>,
    /// 执行结束时的 32 个通用寄存器终态（含 value 与 last_access_timestamp，供 memory argument 与 verifier 输入）。
    pub register_values: Vec<FinalRegisterValue>,
    /// 当前被证明程序的执行绑定摘要：Blake2s(final_pc || setup_tree_caps)，共 8 个 u32。
    /// 由最后一个 proof 的 public_inputs（终态 PC）与 setup_tree_caps 哈希得到；可视为该 bytecode 的 quasi verification key 片段。
    pub end_params: [u32; 8],
    /// 递归链哈希：对 prev_end_params_output  preimage 做 Blake2s 的结果（8 个 u32）；仅调试与链式编码用。
    /// 与 ProgramProof.recursion_chain_hash 对应；base layer 无上一段链时通常为 None。
    pub prev_end_params_output_hash: Option<[u32; BLAKE2S_DIGEST_SIZE_U32_WORDS]>,
    /// 上一递归层传入的链式参数 preimage（16 个 u32）：前 8 字可为前序链哈希，后 8 字常为上一段 end_params。
    /// 与 ProgramProof.recursion_chain_preimage 对应；create_prev_metadata 把它与 end_params 传给下一段证明。
    pub prev_end_params_output: Option<[u32; 16]>,
}

/// This struct contains just the proofs.
pub struct ProofList {
    pub basic_proofs: Vec<Proof>,
    pub reduced_proofs: Vec<Proof>,
    pub reduced_log_23_proofs: Vec<Proof>,
    pub delegation_proofs: Vec<(u32, Vec<Proof>)>,
}

impl ProgramProof {
    pub fn get_num_delegation_proofs_for_type(&self, delegation_type: u32) -> u32 {
        if let Some(proofs) = self.delegation_proofs.get(&delegation_type) {
            proofs.len() as u32
        } else {
            0
        }
    }

    pub fn flatten_for_delegation_circuits_set(
        &self,
        allowed_delegation_circuits: &[u32],
    ) -> Vec<u32> {
        let mut responses = Vec::with_capacity(32 + 32 * 2);

        assert_eq!(self.register_final_values.len(), 32);
        // registers
        for final_values in self.register_final_values.iter() {
            responses.push(final_values.value);
            let (low, high) = split_timestamp(final_values.last_access_timestamp);
            responses.push(low);
            responses.push(high);
        }

        // basic ones
        responses.push(self.base_layer_proofs.len() as u32);
        for proof in self.base_layer_proofs.iter() {
            let t = verifier_common::proof_flattener::flatten_full_proof(proof, true);
            responses.extend(t);
        }
        // then for every allowed delegation circuit
        for delegation_type in allowed_delegation_circuits.iter() {
            if let Some(proofs) = self.delegation_proofs.get(&delegation_type) {
                responses.push(proofs.len() as u32);
                for proof in proofs.iter() {
                    let t = verifier_common::proof_flattener::flatten_full_proof(proof, false);
                    responses.extend(t);
                }
            } else {
                responses.push(0);
            }
        }

        if let Some(preimage) = self.recursion_chain_preimage {
            responses.extend(preimage);
        }

        // check that we didn't have unexpected ones
        for t in self.delegation_proofs.keys() {
            assert!(
                allowed_delegation_circuits.contains(t),
                "allowed set of delegation circuits {:?} doesn't contain circuit type {}",
                allowed_delegation_circuits,
                t
            );
        }

        responses
    }

    pub fn from_proof_list_and_metadata(
        proof_list: &ProofList,
        proof_metadata: &ProofMetadata,
    ) -> ProgramProof {
        // program proof doesn't distinguish between final, reduced & basic proofs.
        let mut base_layer_proofs = vec![];
        base_layer_proofs.extend_from_slice(&proof_list.basic_proofs);
        base_layer_proofs.extend_from_slice(&proof_list.reduced_log_23_proofs);
        base_layer_proofs.extend_from_slice(&proof_list.reduced_proofs);

        ProgramProof {
            base_layer_proofs,
            delegation_proofs: proof_list.delegation_proofs.clone().into_iter().collect(),
            register_final_values: proof_metadata.register_values.clone(),
            end_params: proof_metadata.end_params,
            recursion_chain_preimage: proof_metadata.prev_end_params_output,
            recursion_chain_hash: proof_metadata.prev_end_params_output_hash,
        }
    }
    pub fn to_metadata_and_proof_list(self) -> (ProofMetadata, ProofList) {
        let reduced_proof_count = self.base_layer_proofs.len();
        let proof_list = ProofList {
            basic_proofs: vec![],
            // Here we're guessing - as ProgramProof doesn't distinguish between basic and reduced proofs.
            reduced_proofs: self.base_layer_proofs,
            reduced_log_23_proofs: vec![],
            delegation_proofs: self.delegation_proofs.clone().into_iter().collect(),
        };

        let proof_metadata = ProofMetadata {
            basic_proof_count: 0,
            reduced_proof_count,
            reduced_log_23_proof_count: 0,
            deprecated_final_proof_count: 0,
            delegation_proof_count: vec![],
            register_values: self.register_final_values,
            end_params: self.end_params,
            prev_end_params_output_hash: self.recursion_chain_hash,
            prev_end_params_output: self.recursion_chain_preimage,
        };
        (proof_metadata, proof_list)
    }
}

impl ProofMetadata {
    pub fn total_proofs(&self) -> usize {
        self.basic_proof_count
            + self.reduced_proof_count
            + self.reduced_log_23_proof_count
            + self
                .delegation_proof_count
                .iter()
                .map(|(_, v)| *v)
                .sum::<usize>()
    }
    pub fn create_prev_metadata(&self) -> ([u32; 8], Option<[u32; 16]>) {
        (self.end_params, self.prev_end_params_output)
    }
}

impl ProofList {
    pub fn write_to_directory(&self, output_dir: &Path) {
        println!("Writing proofs to {:?}", output_dir);

        for (i, proof) in self.basic_proofs.iter().enumerate() {
            serialize_to_file(
                proof,
                &Path::new(output_dir).join(&format!("proof_{}.json", i)),
            );
        }
        for (i, proof) in self.reduced_proofs.iter().enumerate() {
            serialize_to_file(
                proof,
                &Path::new(output_dir).join(&format!("reduced_proof_{}.json", i)),
            );
        }
        for (i, proof) in self.reduced_log_23_proofs.iter().enumerate() {
            serialize_to_file(
                proof,
                &Path::new(output_dir).join(&format!("reduced_log_23_proof_{}.json", i)),
            );
        }
        for (delegation_type, proofs) in self.delegation_proofs.iter() {
            for (i, proof) in proofs.iter().enumerate() {
                serialize_to_file(
                    proof,
                    &Path::new(output_dir)
                        .join(&format!("delegation_proof_{}_{}.json", delegation_type, i)),
                );
            }
        }
    }

    pub fn load_from_directory(input_dir: &String, metadata: &ProofMetadata) -> Self {
        let mut basic_proofs = vec![];
        for i in 0..metadata.basic_proof_count {
            let proof_path = Path::new(input_dir).join(format!("proof_{}.json", i));
            let proof: Proof = deserialize_from_file(proof_path.to_str().unwrap());
            basic_proofs.push(proof);
        }

        let mut reduced_proofs = vec![];
        for i in 0..metadata.reduced_proof_count {
            let proof_path = Path::new(input_dir).join(format!("reduced_proof_{}.json", i));
            let proof: Proof = deserialize_from_file(proof_path.to_str().unwrap());
            reduced_proofs.push(proof);
        }

        let mut reduced_log_23_proofs = vec![];
        for i in 0..metadata.reduced_log_23_proof_count {
            let proof_path = Path::new(input_dir).join(format!("reduced_log_23_proof_{}.json", i));
            let proof: Proof = deserialize_from_file(proof_path.to_str().unwrap());
            reduced_log_23_proofs.push(proof);
        }

        let mut delegation_proofs = vec![];
        for (delegation_type, count) in metadata.delegation_proof_count.iter() {
            let mut proofs = vec![];
            for i in 0..*count {
                let proof_path = Path::new(input_dir)
                    .join(format!("delegation_proof_{}_{}.json", delegation_type, i));
                let proof: Proof = deserialize_from_file(proof_path.to_str().unwrap());
                proofs.push(proof);
            }
            delegation_proofs.push((*delegation_type, proofs));
        }

        Self {
            basic_proofs,
            reduced_proofs,
            reduced_log_23_proofs,
            delegation_proofs,
        }
    }

    pub fn get_last_proof(&self) -> &Proof {
        self.basic_proofs.last().unwrap_or_else(|| {
            self.reduced_log_23_proofs.last().unwrap_or_else(|| {
                self.reduced_proofs
                    .last()
                    .expect("Neither main proof nor reduced proof is present")
            })
        })
    }
}

fn deserialize_from_file<T: serde::de::DeserializeOwned>(filename: &str) -> T {
    let src = std::fs::File::open(filename).expect(&format!("{filename}"));
    serde_json::from_reader(src).unwrap()
}
pub fn serialize_to_file<T: serde::Serialize>(el: &T, filename: &Path) {
    let mut dst = std::fs::File::create(filename).unwrap();
    serde_json::to_writer_pretty(&mut dst, el).unwrap();
}

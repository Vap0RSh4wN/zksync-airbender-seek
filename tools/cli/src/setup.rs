//! CLI 侧的 setup 缓存层：连接 main RISC-V、reduced RISC-V 与 delegation circuits。
//!
//! 本文件不是 Clap 命令入口（命令解析在 main.rs），而是为证明或生成 VK 等流程
//! 提供「按 bytecode 复用 circuit setup」的缓存工具。
//!
//! 给定 bytecode 时的处理流程：
//!   计算 bytecode hash
//!     -> 若缓存中已有对应 setup，直接复用
//!     -> 若没有，调用 setups::get_main_riscv_circuit_setup 或 reduced setup
//!     -> 将 setup trace 转成 setup evaluations
//!     -> 写入 HashMap 缓存
//!
//! Delegation circuits 类似：首次需要时生成全部 delegation precomputations 及 evaluations，
//! 后续通过 Arc::clone 共享，不按 bytecode 分 key。
//!
//! SetupCache 结构概览：
//!   main_circuit_setup        key: hash(bytecode)
//!   reduced_circuit_setup     key: hash(bytecode)
//!   delegations               全局一份（不依赖 bytecode）
//!   delegation_evals          全局一份

// GoodAllocator：FFT/LDE、setup 预计算等大对象使用的分配器泛型约束。
// 本文件不直接做 FFT，但缓存的 setup 对象内部包含 FFT/LDE 相关预计算。
use prover::{
    fft::GoodAllocator,
    // Mersenne31Field（F）：Airbender 基础域元素类型；setup evaluations 即 Vec<Mersenne31Field, B>。
    field::Mersenne31Field,
    // IMStandardIsaConfig：标准 main RISC-V 机器配置。
    // IWithoutByteAccessIsaConfigWithDelegation：reduced machine，不支持 byte access，但支持 delegation。
    risc_v_simulator::cycle::{IMStandardIsaConfig, IWithoutByteAccessIsaConfigWithDelegation},
};
// create_circuit_setup：不编译约束系统，仅把 row-major 的 setup trace 转置为 evaluation 向量。
use prover_examples::create_circuit_setup;
// MainCircuitPrecomputations / DelegationCircuitPrecomputations：已构造好的「可用于证明的电路预计算包」，
// 含 compiled circuit、table driver、twiddles、LDE、setup commitment 预计算、witness tracer 函数指针等。
use setups::{DelegationCircuitPrecomputations, MainCircuitPrecomputations};
use std::collections::HashMap;
// Hash / Hasher / DefaultHasher：为 bytecode 计算进程内缓存 key（u64），非密码学承诺。
use std::hash::{Hash, Hasher};
// Arc：共享大对象，避免 clone 时复制 MainCircuitPrecomputations 或 evaluation 向量本体。
use std::{collections::hash_map::DefaultHasher, sync::Arc};

/// Setup 缓存容器。
///
/// 泛型参数：
/// - A: GoodAllocator：进入 MainCircuitPrecomputations / DelegationCircuitPrecomputations 内部
///   （twiddles、LDE、setup precomputations 等）。
/// - B: GoodAllocator：用于 Vec<Mersenne31Field, B>，即 setup evaluations 数组的分配器；可与 A 相同或不同。
#[derive(Default)]
pub struct SetupCache<A: GoodAllocator, B: GoodAllocator> {
    /// 标准 main RISC-V machine 的 setup 缓存。
    ///
    /// - key：u64，来自 DefaultHasher 对 bytecode 的 hash（仅工程缓存 key，非证明安全边界）。
    /// - value 二元组：
    ///   1. Arc<MainCircuitPrecomputations<IMStandardIsaConfig, A, B>>：完整 setup 包；
    ///   2. Arc<Vec<Mersenne31Field, B>>：由 setup.setup.ldes[0].trace 经 create_circuit_setup
    ///      转置得到的 setup evaluations（按列排列的一维向量）。
    pub main_circuit_setup: HashMap<
        u64,
        (
            Arc<MainCircuitPrecomputations<IMStandardIsaConfig, A, B>>,
            Arc<Vec<Mersenne31Field, B>>,
        ),
    >,
    /// Reduced RISC-V machine 的 setup 缓存；形状同 main_circuit_setup，机器配置为
    /// IWithoutByteAccessIsaConfigWithDelegation（常用于递归层等较小约束系统）。
    pub reduced_circuit_setup: HashMap<
        u64,
        (
            Arc<MainCircuitPrecomputations<IWithoutByteAccessIsaConfigWithDelegation, A, B>>,
            Arc<Vec<Mersenne31Field, B>>,
        ),
    >,
    /// 所有 delegation circuit 的预计算对象列表：[(delegation_type_id, DelegationCircuitPrecomputations)]。
    /// 不按 bytecode hash：BLAKE2、BigInt 等 delegation 电路固定，不随某程序 ROM 变化。
    pub delegations: Arc<Vec<(u32, DelegationCircuitPrecomputations<A, B>)>>,
    /// 与 delegations 平行的 setup evaluations：[(delegation_type_id, setup_evaluation_vector)]。
    pub delegation_evals: Arc<Vec<(u32, Arc<Vec<Mersenne31Field, B>>)>>,
}

impl<A: GoodAllocator, B: GoodAllocator> SetupCache<A, B> {
    /// 获取或创建标准 main RISC-V circuit 的 setup（带缓存）；旁支路径，Commands::Prove 主流程不经过此处。
    pub fn get_or_create_main_circuit(
        &mut self,
        bytecode: &Vec<u32>,
    ) -> &(
        Arc<MainCircuitPrecomputations<IMStandardIsaConfig, A, B>>,
        Arc<Vec<Mersenne31Field, B>>,
    ) {
        // bytecode hash 作缓存 key（工程用途，非密码学绑定）。
        let mut hasher = DefaultHasher::new();
        bytecode.hash(&mut hasher);
        let hash = hasher.finish();

        // 缓存命中直接返回；未命中则编译 setup 并写入 HashMap。
        self.main_circuit_setup.entry(hash).or_insert_with(|| {
            let worker = worker::Worker::new_with_num_threads(8);
            let setup = setups::get_main_riscv_circuit_setup(&bytecode, &worker);
            // setup trace 转置为 evaluation 向量，供部分快速查询路径复用。
            let eval = create_circuit_setup(&setup.setup.ldes[0].trace);
            (Arc::new(setup), Arc::new(eval))
        })
    }

    /// 获取或创建 reduced RISC-V circuit 的 setup（与 get_or_create_main_circuit 平行）。
    pub fn get_or_create_reduced_circuit(
        &mut self,
        bytecode: &Vec<u32>,
    ) -> &(
        Arc<MainCircuitPrecomputations<IWithoutByteAccessIsaConfigWithDelegation, A, B>>,
        Arc<Vec<Mersenne31Field, B>>,
    ) {
        let mut hasher = DefaultHasher::new();
        bytecode.hash(&mut hasher);
        let hash = hasher.finish();

        self.reduced_circuit_setup.entry(hash).or_insert_with(|| {
            let worker = worker::Worker::new_with_num_threads(8);
            let setup = setups::get_reduced_riscv_circuit_setup(&bytecode, &worker);
            let eval = create_circuit_setup(&setup.setup.ldes[0].trace);
            (Arc::new(setup), Arc::new(eval))
        })
    }

    /// 获取或创建全部 delegation circuit 的 setup 与 evaluations（全局只构造一次）。
    ///
    /// 无 bytecode 参数：delegation 电路（如 BLAKE2 compression、BigInt with control）固定，
    /// 某程序是否调用 BLAKE2 不改变 BLAKE2 delegation circuit 本身的 setup。
    ///
    /// 返回 (Arc<delegations>, Arc<delegation_evals>)，仅为引用计数 clone，不复制底层大数据。
    pub fn get_or_create_delegations(
        &mut self,
    ) -> (
        Arc<Vec<(u32, DelegationCircuitPrecomputations<A, B>)>>,
        Arc<Vec<(u32, Arc<Vec<Mersenne31Field, B>>)>>,
    ) {
        if self.delegations.is_empty() {
            let worker = worker::Worker::new_with_num_threads(8);
            // 生成 BLAKE2、BigInt 等 delegation circuit 的完整 precomputations 列表。
            self.delegations = Arc::new(setups::all_delegation_circuits_precomputations(&worker));
            let mut delegation_evals = Vec::new();
            // 为每个 delegation 的 setup.setup.ldes[0].trace 生成与 main 相同的 evaluation 布局。
            for (circuit, setup) in self.delegations.iter() {
                let eval = create_circuit_setup(&setup.setup.ldes[0].trace);
                // circuit 为 delegation_type_id（u32），须与程序写入的 CSR 值匹配。
                delegation_evals.push((circuit.clone(), Arc::new(eval)));
            }
            self.delegation_evals = Arc::new(delegation_evals);
        }
        (self.delegations.clone(), self.delegation_evals.clone())
    }
}

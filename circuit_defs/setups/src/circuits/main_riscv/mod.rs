use super::*;

/// 根据当前bytecode和standard ISA delegation CSR白名单，函数编译main RISC-V约束系统，构造ROM/CSR lookup tables，
/// 准备FFT/LDE/setup commitment相关数据，并把这些对象打包给prover,  供 prove_image_execution 使用。
/// A: GoodAllocator和B: GoodAllocator是内存分配器类型参数。
/// Airbender大量使用大数组、FFT/LDE buffer、trace buffer和GPU/CPU不同内存布局，所以很多预计算对象都参数化在allocator上。
/// CPU路径里调用的是：::<Global, Global>，也就是普通全局allocator。
/// bytecode: &[u32]是已经padding好的RISC-V program ROM。原始ELF bytes经过load_binary_from_path和get_padded_binary后，变成按4字节小端排列的u32数组。
/// worker: &Worker用于并行预计算。后面Twiddles::new、LdePrecomputations::new和SetupPrecomputations::from_tables_and_trace_len都会用它。
pub fn get_main_riscv_circuit_setup<A: GoodAllocator, B: GoodAllocator>(
    bytecode: &[u32],
    worker: &Worker,
) -> MainCircuitPrecomputations<IMStandardIsaConfig, A, B> {
    // 从机器配置取出允许的 delegation CSR 集合；编译电路与 CSR lookup 表均需此白名单（例如 BLAKE2、BigInt 对应不同 delegation type id）。
    // 它决定main RISC-V machine允许哪些CSR触发delegation。risc_v_cycles/src/lib.rs也把ALLOWED_DELEGATION_CSRS导出为IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS。
    // main machine只允许白名单里的CSR触发precompile。后面会生成CSR properties table，证明当前CSR调用属于允许的delegation集合。
    let delegation_csrs = IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS;

    // 编译 main RISC-V machine，得到约束布局、trace 长度、setup layout、quotient 相关等。
    let machine: cs::one_row_compiler::CompiledCircuitArtifact<Mersenne31Field> =
        ::risc_v_cycles::get_machine(bytecode, delegation_csrs);
    // ROM 表依赖 bytecode，CSR/delegation 表依赖 delegation_csrs，生成 lookup table driver。
    // `table_driver`只构造lookup tables，不编译全部machine。
    // get_table_driver会创建很多表。除了program-specific的ROM和CSR表，还有机器通用表，保存真实表内容。后面SetupPrecomputations要用它把表内容写进setup trace。
    let table_driver = ::risc_v_cycles::get_table_driver(bytecode, delegation_csrs);

    // 为 main circuit 的 trace domain（DOMAIN_SIZE，如 2^22）预计算 FFT twiddles，供后续 LDE/proving 复用。
    // twiddles是FFT需要的预计算旋转因子。Airbender后端需要把trace多项式做LDE和commitment；setup阶段先准备这些FFT辅助数据。
    let twiddles: Twiddles<_, A> = Twiddles::new(::risc_v_cycles::DOMAIN_SIZE, &worker);
    // Low-degree extension 预计算：domain 大小、LDE 放大倍数、source cosets 配置。
    // 原始trace domain大小是 H。为了做低度测试和commitment，后端会在更大的domain上评价这些多项式。
    // LDE_FACTOR=2 表示扩展到大约 2H 的评价域。
    let lde_precomputations = LdePrecomputations::new(
        ::risc_v_cycles::DOMAIN_SIZE,
        ::risc_v_cycles::LDE_FACTOR,
        ::risc_v_cycles::LDE_SOURCE_COSETS,
        &worker,
    );
    // 将固定 lookup 表、trace 长度、machine.setup_layout、twiddles、LDE 等组织为 setup 预计算对象。
    // 许多列在 prover 运行程序前即固定（ROM/decoder/range 等 setup trace），可承诺、可打开、可复用。

    // SetupPrecomputations::from_tables_and_trace_len从table_driver、trace长度、machine.setup_layout、twiddles、LDE预计算和Merkle cap size生成setup precomputations。
    // machine.setup_layout来自CompiledCircuitArtifact。编译后的circuit不仅包含约束，还包含setup trace布局。
    // SetupPrecomputations::from_tables_and_trace_len用这个布局和table contents生成setup阶段需要的trace、LDE和Merkle tree。
    // 把所有固定表和固定列做成一张setup trace，然后对这张setup trace做承诺。
    let setup =
        SetupPrecomputations::<DEFAULT_TRACE_PADDING_MULTIPLE, A, DefaultTreeConstructor>::from_tables_and_trace_len(
            &table_driver,
            // DOMAIN_SIZE告诉我们setup trace有多少行
            ::risc_v_cycles::DOMAIN_SIZE,
            &machine.setup_layout,
            &twiddles,
            &lde_precomputations,
            ::risc_v_cycles::LDE_FACTOR,
            ::risc_v_cycles::TREE_CAP_SIZE,
            &worker,
        );

    MainCircuitPrecomputations {
        compiled_circuit: machine,
        table_driver,
        twiddles,
        lde_precomputations,
        setup,
        // 函数指针：证明时用 MainRiscVOracle 将 RISC-V 执行轨迹写入 witness trace（非此处生成 witness）。
        witness_eval_fn_for_gpu_tracer: ::risc_v_cycles::witness_eval_fn_for_gpu_tracer,
    }
}

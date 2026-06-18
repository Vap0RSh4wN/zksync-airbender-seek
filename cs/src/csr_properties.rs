use crate::{machine::NON_DETERMINISM_CSR, tables::*};
use field::PrimeField;

/// 先从CSR说起。CSR是RISC-V里的“control and status register”。普通RISC-V程序可以通过CSR指令读写某些特殊寄存器。在Airbender里，有些特殊CSR被用作“扩展入口”。
/// 这张表要回答的问题是：
/// 某个CSR编号，是否是Airbender main circuit支持的特殊CSR？
/// 如果支持，它是不是delegation CSR？
/// 所以这张表不是保存程序代码的。程序代码在RomRead表里。CSR表保存的是“哪些CSR编号有特殊含义”。
/// main circuit里会遇到CSR相关指令。比如程序里执行某个CSR读写：
/// CSRRW / CSRRS / CSRRC / CSRRWI / ...
/// 这时电路要判断这个CSR是不是允许的特殊CSR。如果它是non-determinism CSR，就允许从非确定输入源读数据。如果它是delegation CSR，就允许发起delegation request。不是白名单里的CSR，不能随便当特殊入口使用。
/// 这里和ROM表不同的是：CSR表里的值都很小。CSR编号是12-bit，flag是0/1，因此放进Mersenne31Field没有问题。
/// 表形状是：
/// key:
/// csr_index
/// value:
/// is_supported
/// is_allowed_for_delegation
/// 完整行是：
/// [csr_index, is_supported, is_allowed_for_delegation]
pub fn create_special_csr_properties_table<F: PrimeField>(
    id: u32,
    support_non_determinism_csr: bool,
    supported_delegations: &[u32],
) -> LookupTable<F, 3> {
    // CSR编号必须小于2^12。这是因为RISC-V instruction里的CSR字段本来就是12-bit。
    for el in supported_delegations.iter() {
        assert!(*el < (1 << 12));
    }
    // key覆盖一个连续的2^12范围。也就是：
    // 0, 1, 2, ..., 4095
    // 每个key都是一个CSR编号。
    // 为什么覆盖全部4096个CSR，而不是只列出支持的CSR？
    // 因为这样查询任意CSR时都能得到一个确定答案：
    // 这个CSR支持吗？
    // 这个CSR是delegation吗？
    // 如果只保存白名单里的CSR，那么查一个不支持的CSR时就会“查不到”。但这张表更像一个属性表，对每个CSR编号都给出属性：
    // 支持 -> 1
    // 不支持 -> 0
    // 是否delegation -> 1/0
    // 这对电路更方便。
    let keys = key_for_continuous_log2_range(12);
    let supported_delegations = supported_delegations.to_vec();
    const TABLE_NAME: &'static str = "Special CSR properties";
    // 这里的1表示num_key_columns = 1。由于总宽度是3，所以value列数量是2。LookupTable构建时会把前num_key_columns列作为key，剩下列作为value。
    LookupTable::<F, 3>::create_table_from_key_and_key_generation_closure(
        &keys,
        TABLE_NAME.to_string(),
        1,
        move |key| {
            let input = key[0].as_u64_reduced();
            assert!(input < (1u64 << 12));
            // 第一步，拿CSR编号,这一行表在描述哪个CSR：
            let csr_index = input as u32;
            // 第二步，判断它是不是non-determinism CSR

            // 在模拟器状态代码里，NON_DETERMINISM_CSR定义为：pub const NON_DETERMINISM_CSR: u32 = 0x7c0;
            // 也就是说，CSR编号0x7c0被Airbender用作non-determinism输入入口。
            // 它的作用可以先理解成：guest程序通过这个CSR从外部输入源读数据。
            // 比如dynamic_fibonacci这种程序，n不是写死在程序binary里的，而是从输入里传进来。执行时，程序可能通过某个CSR读取这个非确定输入。这里的“非确定”不是说乱给，而是说它不是由程序bytecode固定决定的，而是prover提供的私有或外部输入。
            /// 所以：
            /// allow_non_determinism = true:
            /// CSR 0x7c0 被认为是支持的特殊CSR。
            /// allow_non_determinism = false:
            /// CSR 0x7c0 不被这张表标记为支持。
            let is_nondeterminism_csr = csr_index == NON_DETERMINISM_CSR as u32;
            // 第三步，判断它是不是delegation白名单里的CSR
            let is_allowed_for_delegation = supported_delegations.contains(&csr_index);
            // 第四步，检查一个CSR不能同时是non-determinism CSR和delegation CSR，0x7c0不能同时出现在delegation白名单里
            assert!(is_nondeterminism_csr & is_allowed_for_delegation == false);
            // 第五步，如果它是non-determinism CSR，并且当前配置允许non-determinism，那么支持。或者，如果它在delegation白名单里，那么支持。否则不支持。
            let is_supported =
                (is_nondeterminism_csr & support_non_determinism_csr) | is_allowed_for_delegation;
            // 第六步，输出value：
            // value[0] = is_supported
            // value[1] = is_allowed_for_delegation
            let result = [
                F::from_u64_unchecked(is_supported as u64),
                F::from_u64_unchecked(is_allowed_for_delegation as u64),
                F::ZERO,
            ];

            (input as usize, result)
        },
        Some(first_key_index_gen_fn::<F, 3>),
        id,
    )
}

use crate::cs::{
    cs_reference::BasicAssembly, witness_placer::graph_description::WitnessGraphCreator,
};

use super::*;
use rayon::prelude::*;

pub mod full_isa_no_exceptions;
pub mod full_isa_with_delegation_no_exceptions;
pub mod full_isa_with_delegation_no_exceptions_no_signed_mul_div;
pub mod minimal_no_exceptions;
pub mod minimal_no_exceptions_with_delegation;
pub mod minimal_state;
pub mod state_transition_parts;

#[derive(Clone, Debug)]
pub struct BasicFlagsSource {
    keys: DecoderOutputExtraKeysHolder,
    values: Vec<Boolean>,
}

impl BasicFlagsSource {
    pub fn new(keys: DecoderOutputExtraKeysHolder, values: Vec<Boolean>) -> Self {
        assert_eq!(keys.num_major_keys() + keys.max_minor_keys(), values.len());

        Self { keys, values }
    }
}

impl IndexableBooleanSet for BasicFlagsSource {
    #[track_caller]
    fn get_major_flag(&self, major: DecoderMajorInstructionFamilyKey) -> Boolean {
        let major_index = self.keys.get_major_index(&major);
        self.values[major_index]
    }

    #[track_caller]
    fn get_minor_flag(
        &self,
        major: DecoderMajorInstructionFamilyKey,
        minor: DecoderInstructionVariantsKey,
    ) -> Boolean {
        let (_major_index, minor_index) = self.keys.get_index_set(&major, &minor);
        let offset = self.keys.num_major_keys();
        self.values[offset..][minor_index]
    }
}

#[allow(deprecated)]
#[derive(Clone, Debug)]
pub struct BasicDecodingResultWithoutSigns<F: PrimeField> {
    pub pc_next: Register<F>,
    pub src1: RegisterDecomposition<F>,
    pub src2: RegisterDecomposition<F>,
    pub rs2_index: Constraint<F>,
    pub imm: Register<F>,
    pub funct3: Num<F>,
    pub funct12: Constraint<F>,
}

#[allow(deprecated)]
impl<F: PrimeField> DecoderOutputSource<F, RegisterDecomposition<F>>
    for BasicDecodingResultWithoutSigns<F>
{
    fn get_pc_next(&self) -> Register<F> {
        self.pc_next
    }
    fn funct3(&self) -> Num<F> {
        self.funct3
    }
    fn get_rs2_index(&self) -> Constraint<F> {
        self.rs2_index.clone()
    }
    fn funct12(&self) -> Constraint<F> {
        self.funct12.clone()
    }
    fn get_imm(&self) -> Register<F> {
        self.imm
    }
    fn get_rs1_or_equivalent(&self) -> RegisterDecomposition<F> {
        self.src1
    }
    fn get_rs2_or_equivalent(&self) -> RegisterDecomposition<F> {
        self.src2
    }
}

#[derive(Clone, Debug)]
pub struct BasicDecodingResultWithSigns<F: PrimeField> {
    pub pc_next: Register<F>,
    pub src1: RegisterDecompositionWithSign<F>,
    pub src2: RegisterDecompositionWithSign<F>,
    pub imm: Register<F>,
    pub rs2_index: Constraint<F>,
    pub funct3: Num<F>,
    pub funct12: Constraint<F>,
}

impl<F: PrimeField> DecoderOutputSource<F, RegisterDecompositionWithSign<F>>
    for BasicDecodingResultWithSigns<F>
{
    fn get_pc_next(&self) -> Register<F> {
        self.pc_next
    }
    fn funct3(&self) -> Num<F> {
        self.funct3
    }
    fn get_rs2_index(&self) -> Constraint<F> {
        self.rs2_index.clone()
    }
    fn funct12(&self) -> Constraint<F> {
        self.funct12.clone()
    }
    fn get_imm(&self) -> Register<F> {
        self.imm
    }
    fn get_rs1_or_equivalent(&self) -> RegisterDecompositionWithSign<F> {
        self.src1.clone()
    }
    fn get_rs2_or_equivalent(&self) -> RegisterDecompositionWithSign<F> {
        self.src2.clone()
    }
}

pub fn pad_bytecode<const ROM_ADDRESS_SPACE_BOUND: u32>(bytecode: &mut Vec<u32>) {
    assert!(ROM_ADDRESS_SPACE_BOUND.is_power_of_two());
    assert!(bytecode.len() as u32 <= ROM_ADDRESS_SPACE_BOUND / 4);
    bytecode.resize((ROM_ADDRESS_SPACE_BOUND / 4) as usize, UNIMP_OPCODE);
}

/// Creating a table with ROM (program) data.
/// The table will have a constant size (ROM_ADDRESS_SPACE_BOUND / 4), and look like this:
/// (0, image bytes 0..2, image bytes 2..4)
/// (4, image bytes 4..6, image bytes 6..8)
// We have to do this his way, as our prime field is a little bit smaller than 32 bits.
// All the entries larger than the image will be filled with UNIMP_OPCODE.

/// 返回LookupTable<F, 3>，表示每行有3个field元素。源码注释明确写了ROM表的样子：第一列是地址，后两列是对应4字节instruction的低16位和高16位；注释里还说明这样拆是因为prime field略小于32 bits。
/// ROM_BITS:
/// ROM地址空间大小参数。
/// ROM总字节数 = 2^(16 + ROM_BITS)。
/// image:
/// padded bytecode，按u32存储。
/// image[i]表示pc = 4*i处的instruction word。
/// id:
/// 这张表的TableType编号。
/// 对main RISC-V ROM表来说是TableType::RomRead.to_table_id()。
/// pc=0: ADD x5, x1, x2
/// pc=4: SW  x5, 0(x10)
/// pc=8: LW  x6, 0(x10)
/// 对应关系是：
/// image[0] = ADD x5, x1, x2 这条指令的32-bit机器码
/// image[1] = SW  x5, 0(x10) 这条指令的32-bit机器码
/// image[2] = LW  x6, 0(x10) 这条指令的32-bit机器码
pub fn create_table_for_rom_image<
    F: PrimeField,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    image: &[u32],
    id: u32,
) -> LookupTable<F, 3> {
    //LookupTable<F, 3>表示每行宽度为3个field element的lookup table。RomRead表使用1个key列和2个value列：key: pc value: opcode_low16 opcode_high16
    assert!(ROM_ADDRESS_SPACE_SECOND_WORD_BITS > 0);

    // 这里的image.len()是u32个数，乘4才是字节数。它要求程序字节数不能超过ROM上界。
    assert!(
        image.len() * 4 <= 1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS),
        "ROM size can be at most {} bytes ({} words), but input is {} words",
        1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS),
        (1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS)) / 4,
        image.len()
    );
    // 为什么减2？因为ROM地址按字节计数，但每条instruction是4字节对齐。地址空间有2^(16+k)
    // 个字节，按4字节一行，就有：2^(16+k−2)行。默认k=5，所以行数是2^19。
    let keys_len = 1usize << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS - 2);
    let mut keys = Vec::with_capacity(keys_len);
    (0..keys_len)
        .into_par_iter()
        .map(|i| {
            let mut key = [F::ZERO; 3];
            let address = i * 4;
            key[0] = F::from_u64_unchecked(address as u64);
            key
        })
        .collect_into_vec(&mut keys);

    assert_eq!(keys.len(), keys_len);
    const TABLE_NAME: &'static str = "ROM table";
    let image = image.to_vec();
    // 如果index落在程序image里，就取真实instruction；如果超出image，就填UNIMP_OPCODE。
    // 在当前主路径里，bytecode已经pad满，所以大多数情况下image.len()等于keys_len；但这个函数本身也支持未完全填满的image，用UNIMP补齐。
    LookupTable::<F, 3>::create_table_from_key_and_key_generation_closure(
        &keys,
        TABLE_NAME.to_string(),
        1,
        move |key| {
            // 第一条保证pc没有超出ROM；第二条保证pc是4字节对齐的instruction地址。
            let pc = key[0].as_u64_reduced();
            assert!(
                pc < 1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS) as u64,
                "PC = {} is too large for ROM bound {} bytes",
                pc,
                1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS)
            );
            assert!(pc % 4 == 0, "PC = {} is not aligned", pc);
            let index = (pc as usize) / 4;
            let opcode = if index < image.len() {
                let opcode = image[index];

                opcode
            } else {
                // UNIMP opcodes,如果index超过image长度，就填UNIMP_OPCODE。不过在main path里，bytecode已经pad到固定ROM大小，所以一般不会越界。
                UNIMP_OPCODE
            };
            // 再把32-bit opcode拆成两个16-bit
            let low = opcode as u16;
            let high = (opcode >> 16) as u16;

            let mut result = [F::ZERO; 3];
            result[0] = F::from_u64_unchecked(low as u64);
            result[1] = F::from_u64_unchecked(high as u64);

            ((pc / 4) as usize, result)
        },
        // 给定pc，快速知道它在ROM表的第几行。比如pc=12，对应index=3。后面witness或lookup multiplicity路径需要查表行号时可以直接用。
        Some(|keys| {
            let pc = keys[0].as_u64_reduced();
            assert!(
                pc < 1u64 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS),
                "PC = {} is too large for ROM bound {}",
                pc,
                1u64 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS)
            );
            assert!(pc % 4 == 0, "PC = {} is not aligned", pc);
            let index = (pc / 4) as usize;

            index
        }),
        id,
    )
}

/// 当guest程序通过CSR请求delegation时，main circuit要证明这个CSR id在允许集合里。比如BLAKE2 delegation和BigInt delegation都有自己的type id；Standard machine允许它们，Reduced machine可能只允许其中一部分，Final reduced machine可能不允许delegation。
pub fn create_csr_table_for_delegation<F: PrimeField>(
    allow_non_determinism: bool,
    allowed_delegation_csrs: &[u32],
    id: u32,
) -> LookupTable<F, 3> {
    use crate::csr_properties::create_special_csr_properties_table;
    create_special_csr_properties_table(id, allow_non_determinism, allowed_delegation_csrs)
}

// Use this function if you need CS-detached table driver, e.g. in proving or setup
// 返回一个独立TableDriver给setup/prover使用
/// 这个函数用于需要“CS-detached table driver”的地方，比如proving或setup。也就是说，它生成的是独立固定表集合，不依附于正在构造的Circuit。
/// create_table_driver(machine)生成的是“机器通用表集合”。随后get_table_driver_for_rom_bound再补入program-specific的RomRead和SpecialCSRProperties。
pub fn create_table_driver<
    F: PrimeField,
    M: Machine<F>,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    machine: M,
) -> TableDriver<F> {
    // 机器类型自己声明需要哪些表。接着有几个assert，禁止machine自己声明一些特殊表，比如ZeroEntry、OpTypeBitmask、CsrBitmask、RangeCheckSmall。这些表由通用逻辑统一加入，不能由machine重复声明。源码里这些assert在函数开头。
    // 表示这台machine声明自己会用哪些表
    let used_tables = M::define_used_tables();
    assert!(
        used_tables.contains(&TableType::ZeroEntry) == false,
        "machine must not define zero entry table as used"
    );
    assert!(
        used_tables.contains(&TableType::OpTypeBitmask) == false,
        "machine must not define decoder table"
    );
    assert!(
        used_tables.contains(&TableType::CsrBitmask) == false,
        "machine must not define CSR support table"
    );
    assert!(
        used_tables.contains(&TableType::RangeCheckSmall) == false,
        "machine must not define 8-bit range check table"
    );
    // 允许machine提供额外表内容，但会检查它们不要和used_tables重复。
    let extra_tables = machine.define_additional_tables();
    for (table, _) in extra_tables.iter() {
        assert!(used_tables.contains(table) == false);
    }
    let mut table_driver = TableDriver::new();

    // 对used_tables逐个materialize_table。materialize_table表示“这个表是标准表，可以由TableType自动生成”。
    // 源码里循环调用table_driver.materialize_table(table)。
    for table in used_tables.into_iter() {
        table_driver.materialize_table(table);
    }

    for (table, content) in extra_tables.into_iter() {
        table_driver.add_table_with_content(table, content);
    }
    // 手动materialize一些通用表
    table_driver.materialize_table(TableType::And);
    table_driver.materialize_table(TableType::ZeroEntry);
    table_driver.materialize_table(TableType::QuickDecodeDecompositionCheck4x4x4);
    table_driver.materialize_table(TableType::QuickDecodeDecompositionCheck7x3x6);
    table_driver.materialize_table(TableType::U16GetSignAndHighByte);
    table_driver.materialize_table(TableType::RangeCheckSmall);

    // decoder表是后面instruction decode的重要固定表。它帮助把instruction编码分解成opcode flags。源码里OpTypeBitmask表就是在这里加入的。
    // 把instruction的某些bit分解成opcode family和具体variant flags。
    let decoder_table = M::create_decoder_table(TableType::OpTypeBitmask.to_table_id());
    table_driver.add_table_with_content(
        TableType::OpTypeBitmask,
        LookupWrapper::Dimensional3(decoder_table),
    );

    // let csr_support_table = M::create_csr_support_table(TableType::CsrBitmask.to_table_id());
    // table_driver.add_table_with_content(
    //     TableType::CsrBitmask,
    //     LookupWrapper::Dimensional3(csr_support_table),
    // );

    // 第七步，如果machine使用ROM存bytecode，就加入RomAddressSpaceSeparator表
    // 这张表是ROM地址空间相关的辅助表。真正的当前程序RomRead表在get_table_driver_for_rom_bound外层手动加入，
    // 因为它依赖当前bytecode。源码里这段在create_table_driver末尾。
    if M::USE_ROM_FOR_BYTECODE {
        // manual call here, to later on easily control address bits
        let id = TableType::RomAddressSpaceSeparator.to_table_id();
        use crate::tables::create_rom_separator_table;
        let table = LookupWrapper::Dimensional3(create_rom_separator_table::<
            F,
            ROM_ADDRESS_SPACE_SECOND_WORD_BITS,
        >(id));
        table_driver.add_table_with_content(TableType::RomAddressSpaceSeparator, table);
    }

    table_driver
}

// 把表注册进Circuit：给compile_machine使用。
pub fn create_table_driver_into_cs<
    F: PrimeField,
    CS: Circuit<F>,
    M: Machine<F>,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    cs: &mut CS,
    machine: M,
) {
    // materialize all tables
    let used_tables = M::define_used_tables();

    // 断言machine不能自己声明几张特殊表：ZeroEntry、OpTypeBitmask、CsrBitmask、RangeCheckSmall。这些表由框架统一管理。
    // 为什么要禁止machine自己定义这些表？
    // 因为这些是系统级表：
    // ZeroEntry:固定零项表。
    // OpTypeBitmask:decoder表，框架统一从支持的opcode集合生成。
    // RangeCheckSmall:小范围检查表，框架统一生成。
    // CsrBitmask:CSR支持表，当前代码里相关部分暂时注释/保留。
    // 如果每个machine自己随意定义这些表，后面generic lookup setup和table id就会混乱。
    assert!(
        used_tables.contains(&TableType::ZeroEntry) == false,
        "machine must not define zero entry table as used"
    );
    assert!(
        used_tables.contains(&TableType::OpTypeBitmask) == false,
        "machine must not define decoder table"
    );
    assert!(
        used_tables.contains(&TableType::CsrBitmask) == false,
        "machine must not define CSR support table"
    );
    assert!(
        used_tables.contains(&TableType::RangeCheckSmall) == false,
        "machine must not define 8-bit range check table"
    );

    // 允许machine提供额外表。源码里会检查extra table不能和used_tables重复
    let extra_tables = machine.define_additional_tables();
    for (table, _) in extra_tables.iter() {
        assert!(used_tables.contains(table) == false);
    }

    // 对used_tables逐个注册
    // Circuit trait要求实现materialize_table方法；BasicAssembly的实现是调用内部table_driver.materialize_table(table_type)
    for table in used_tables.into_iter() {
        cs.materialize_table(table);
    }

    for (table, content) in extra_tables.into_iter() {
        cs.add_table_with_content(table, content);
    }

    // 这些表分别服务bit操作、zero项、快速decoder分解、16-bit符号/高字节提取、小range check等。源码中这些表是在create_table_driver_into_cs里统一注册的。
    // table_driver.materialize_table(table_type)会立刻把这个TableType对应的固定表生成出来，并放进TableDriver里。只是它会用一个全局cache避免重复生成同一张通用表。
    // 但要加一句限定：它只能materialize那些可以由TableType::generate_table()通用生成的表。像RomRead、SpecialCSRProperties、OpTypeBitmask、RomAddressSpaceSeparator这种需要程序bytecode、CSR白名单、machine-specific decoder、ROM参数的表，不是靠普通materialize_table生成，而是手动add_table_with_content。
    cs.materialize_table(TableType::And);
    cs.materialize_table(TableType::ZeroEntry);
    cs.materialize_table(TableType::QuickDecodeDecompositionCheck4x4x4);
    cs.materialize_table(TableType::QuickDecodeDecompositionCheck7x3x6);
    cs.materialize_table(TableType::U16GetSignAndHighByte);
    cs.materialize_table(TableType::RangeCheckSmall);

    // 创建decoder表，Machine::create_decoder_table会遍历RISC-V instruction encoding中的opcode、funct3、funct7组合，
    // 把它们映射到instruction format、major key、minor key等bitmask信息。
    // 源码里create_decoder_table调用produce_decoder_table_stub，后者遍历u7 x u3 x u7的编码空间。
    let decoder_table = M::create_decoder_table(TableType::OpTypeBitmask.to_table_id());
    cs.add_table_with_content(
        TableType::OpTypeBitmask,
        LookupWrapper::Dimensional3(decoder_table),
    );

    // 如果机器使用ROM bytecode：这加入的是RomAddressSpaceSeparator，不是RomRead。RomRead需要当前bytecode内容，
    // 而compile_machine没有bytecode参数，所以不可能在这里加入RomRead。
    // 源码确实只在这里加入ROM地址空间辅助表；真正RomRead表在default_compile_machine里追加。
    // 这一行执行完之后，cs内部已经有机器通用表了，但还没有当前程序ROM内容。
    if M::USE_ROM_FOR_BYTECODE {
        // manual call here, to later on easily control address bits
        let id = TableType::RomAddressSpaceSeparator.to_table_id();
        use crate::tables::create_rom_separator_table;
        let table = LookupWrapper::Dimensional3(create_rom_separator_table::<
            F,
            ROM_ADDRESS_SPACE_SECOND_WORD_BITS,
        >(id));
        cs.add_table_with_content(TableType::RomAddressSpaceSeparator, table);
    }
}

/// 真正把machine写进constraint system的地方：
/// 创建一个空Circuit收集器； 注册机器使用的固定表；
/// 让Machine描述一行RISC-V状态转移； 把这行状态转移产生的变量、约束、lookup、memory query收集成CircuitOutput；
/// 标出跨行状态输入和输出。
pub fn compile_machine<
    F: PrimeField,
    C: Circuit<F>,
    M: Machine<F>,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    machine: M,
) -> CircuitOutput<F>
where
    [(); { <M as Machine<F>>::ASSUME_TRUSTED_CODE } as usize]:,
    [(); { <M as Machine<F>>::OUTPUT_EXACT_EXCEPTIONS } as usize]:,
{
    // 创建一个空的constraint system收集器。这里C在default_compile_machine中被指定为：BasicAssembly<Mersenne31Field>
    // BasicAssembly::new()初始化了一堆空容器：变量计数从0开始，constraint storage为空，lookup storage为空，
    // shuffle RAM query为空，boolean variables为空，rangechecked expressions为空，placeholder映射为空，
    // linkage queries为空，table driver为空，delegation request为空，witness graph为空。源码里BasicAssembly字段和new()初始化逻辑都能看到。
    let mut cs = C::new();

    // 这一步把通用lookup表注册进cs。注意它不是生成独立的TableDriver返回，而是直接写进Circuit对象。
    // 注意它和get_table_driver很像，但目标不同：get_table_driver返回独立的TableDriver对象；create_table_driver_into_cs把表注册到正在构造的Circuit里。
    // 源码里可以看到它会cs.materialize_table(...)，还会把decoder表、ROM地址分隔表等内容加入CS。
    create_table_driver_into_cs::<F, C, M, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(&mut cs, machine);

    // 是main RISC-V machine定义“一行怎么执行”的核心。它会往Circuit里添加：
    // 变量
    // 普通约束
    // ROM lookup
    // decoder lookup
    // range check
    // shuffle RAM query
    // delegation request
    // state input / state output

    // 返回：initial_state，final_state

    // 这两个表示一行状态转移的入口状态和出口状态。对main RISC-V来说，最核心的跨行状态通常是pc相关状态。也就是：

    // 这一行开始时pc是多少，这一行结束后下一行pc是多少，表示这一行开始和结束时需要跨行连接的状态变量。
    let (initial_state, final_state) =
        M::describe_state_transition::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(&mut cs);

    // 把状态对象里的变量收集出来。后面state_input和state_output用于相邻行连接。
    let mut initial_state_vars = vec![];
    initial_state.append_into_variables_set(&mut initial_state_vars);

    let mut final_state_vars = vec![];
    final_state.append_into_variables_set(&mut final_state_vars);

    // 根据state_input和state_output生成跨行连接关系，也就是：
    // row i 的 final_state = row i+1 的 initial_state
    // 这就是trace能表示连续执行的原因。
    // 直观上：
    // 第0行结束后的pc
    //   必须等于
    // 第1行开始前的pc

    // 第1行结束后的pc
    //   必须等于
    // 第2行开始前的pc

    // 如果没有这一步，每一行都可能是孤立的，prover可以伪造一堆彼此无关的RISC-V单步执行。

    // 把BasicAssembly里收集到的所有东西变成CircuitOutput。

    let (mut output, _) = cs.finalize();
    output.state_input = initial_state_vars;
    output.state_output = final_state_vars;

    output
}

pub fn dump_wintess_graph<
    F: PrimeField,
    M: Machine<F>,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    _machine: M,
) -> WitnessGraphCreator<F>
where
    [(); { <M as Machine<F>>::ASSUME_TRUSTED_CODE } as usize]:,
    [(); { <M as Machine<F>>::OUTPUT_EXACT_EXCEPTIONS } as usize]:,
{
    let mut cs = BasicAssembly::<F, WitnessGraphCreator<F>>::new();
    cs.witness_placer = Some(WitnessGraphCreator::<F>::new());
    let _ = M::describe_state_transition::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(&mut cs);
    let (_, witness_placer) = cs.finalize();

    witness_placer.unwrap()
}

pub fn dump_ssa_witness_eval_form<
    F: PrimeField,
    M: Machine<F>,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    machine: M,
) -> Vec<Vec<crate::cs::witness_placer::graph_description::RawExpression<F>>>
where
    [(); { <M as Machine<F>>::ASSUME_TRUSTED_CODE } as usize]:,
    [(); { <M as Machine<F>>::OUTPUT_EXACT_EXCEPTIONS } as usize]:,
{
    let graph = dump_wintess_graph::<_, _, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(machine);
    let (_resolution_order, ssa_forms) = graph.compute_resolution_order();

    ssa_forms
}

#[cfg(test)]
mod tests {
    use field::Mersenne31Field;

    use super::*;

    #[test]
    fn rom_table_test() {
        let image = [100_000, 200_000, 0];
        let table = create_table_for_rom_image::<Mersenne31Field, 16>(&image, 15);

        // Now table should have entries:
        // 0 -- 0x86a0 0x1
        // 4 -- 0xd40 0x3
        // 8 -- 0x0 0x0
        // 12 -- 0xc0001073  (UNIMP)
        assert_eq!(
            table.lookup_value::<2>(&[Mersenne31Field::new(0)]),
            [Mersenne31Field::new(0x86a0), Mersenne31Field::new(0x1)]
        );
        assert_eq!(
            table.lookup_value::<2>(&[Mersenne31Field::new(4)]),
            [Mersenne31Field::new(0xd40), Mersenne31Field::new(0x3)]
        );
        assert_eq!(
            table.lookup_value::<2>(&[Mersenne31Field::new(8)]),
            [Mersenne31Field::new(0x0), Mersenne31Field::new(0x0)]
        );
        assert_eq!(
            table.lookup_value::<2>(&[Mersenne31Field::new(12)]),
            [Mersenne31Field::new(0x1073), Mersenne31Field::new(0xc000)]
        );
    }
}

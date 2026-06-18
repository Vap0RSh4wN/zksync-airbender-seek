下面我把这章重写成“能跟着源码走”的版本。核心变化是：每遇到一个函数，就继续往下钻到它真正做了什么；但这一章仍然守住边界——我们读到“compiler边界”和“setup固定输入”这一层为止，opcode gadget内部的ADD/LW/SW怎么写约束放到第4章，因为这里如果直接展开opcode语义，会把setup -> compiler -> layout -> setup trace这条主线打散。以下内容覆盖并重塑你贴的第3章草稿。

第3章 从setup返回对象进入compiler边界

第二章已经确认，Airbender的CPU标准证明入口会走到这里：

tools/cli/src/prover_utils.rs
  create_proofs_internal
    Machine::Standard + CPU
      -> setups::get_main_riscv_circuit_setup(binary, worker)
      -> setups::all_delegation_circuits_precomputations(worker)
      -> prover_examples::prove_image_execution(...)

所以这一章从get_main_riscv_circuit_setup开始。这个函数要解决的问题很具体：给定已经padding好的RISC-V bytecode，准备main RISC-V prover开工之前必须持有的一整包固定材料。它不执行guest程序，不生成寄存器值，不生成RAM访问trace，也不生成proof。它做的是setup：把程序ROM、delegation CSR白名单、机器约束布局、固定lookup表、setup trace commitment、FFT/LDE预计算这些东西组织成一个返回对象，交给后面的prove_image_execution使用。

这一章的源码阅读顺序是：

circuit_defs/setups/src/circuits/main_riscv/mod.rs
  get_main_riscv_circuit_setup

circuit_defs/setups/src/lib.rs
  MainCircuitPrecomputations

circuit_defs/risc_v_cycles/src/lib.rs
  get_machine
  get_table_driver

cs/src/machine/machine_configurations/mod.rs
  create_table_for_rom_image
  create_csr_table_for_delegation
  create_table_driver
  create_table_driver_into_cs
  compile_machine

cs/src/lib.rs
  default_compile_machine

cs/src/cs/circuit.rs
  CircuitOutput
  Circuit trait

cs/src/one_row_compiler/compile_layout.rs
  compile_output_for_chunked_memory_argument
  compile_inner

cs/src/definitions/setup_tree.rs
  SetupLayout

prover/src/prover_stages/mod.rs
  SetupPrecomputations::from_tables_and_trace_len
  get_main_domain_trace

先给一个全局图，后面逐个函数下钻：

padded bytecode + worker
  |
  v
get_main_riscv_circuit_setup
  |
  +-- delegation_csrs
  |
  +-- get_machine(bytecode, delegation_csrs)
  |     |
  |     +-- create_table_for_rom_image
  |     +-- create_csr_table_for_delegation
  |     +-- default_compile_machine
  |           |
  |           +-- compile_machine
  |           |     |
  |           |     +-- create_table_driver_into_cs
  |           |     +-- M::describe_state_transition
  |           |     +-- cs.finalize -> CircuitOutput
  |           |
  |           +-- OneRowCompiler::compile_output_for_chunked_memory_argument
  |                 |
  |                 +-- CircuitOutput -> CompiledCircuitArtifact
  |
  +-- get_table_driver(bytecode, delegation_csrs)
  |     |
  |     +-- create_table_driver
  |     +-- add RomRead table
  |     +-- add SpecialCSRProperties table
  |
  +-- Twiddles::new
  |
  +-- LdePrecomputations::new
  |
  +-- SetupPrecomputations::from_tables_and_trace_len
        |
        +-- TableDriver.dump_tables
        +-- setup trace
        +-- LDE
        +-- Merkle trees
3.1 先把本章涉及的数据分清楚

get_main_riscv_circuit_setup的输入只有两个：

bytecode: &[u32]
worker: &Worker

这里的bytecode不是原始app.bin字节。前面CLI路径已经调用load_binary_from_path和get_padded_binary，先把app.bin按4字节小端切成Vec<u32>，再pad到main ROM要求的固定长度。进入这一章时，bytecode已经是“可以直接做RomRead表”的程序镜像。

main RISC-V circuit使用这些常量：

H = DOMAIN_SIZE = 2^22
N = NUM_CYCLES = H - 1
ρ = LDE_FACTOR = 2
ROM_BYTES = MAX_ROM_SIZE = 2^21
ROM_WORDS = ROM_BYTES / 4 = 2^19

这些常量来自circuit_defs/risc_v_cycles/src/lib.rs：DOMAIN_SIZE是1 << 22，NUM_CYCLES是DOMAIN_SIZE - 1，LDE_FACTOR是2，MAX_ROM_SIZE是1 << 21字节。

先解释它们各自代表什么。

H是trace domain大小。你可以先把它理解成main RISC-V circuit这一张大表的高度上限。N = H - 1是一个main circuit instance最多承载的RISC-V cycle数。为什么少1？后面读SetupLayout和SetupPrecomputations会看到，很多setup编码都使用trace_len - 1作为容量，最后一行会被留出来做边界或协议处理；源码里SetupLayout::layout_for_lookup_size和SetupPrecomputations::get_main_domain_trace都用trace_len - 1作为表内容编码容量。

ROM_BYTES = 2^21表示程序ROM固定上界是2MB。由于bytecode按u32存储，所以ROM_WORDS = 2^19。也就是说，get_machine看到的bytecode长度必须是2^19个u32。

这一章一共涉及四类数据。理解这四类数据，比死记结构体重要。

第一类是program-specific fixed data。它由当前程序决定。最典型的是RomRead表：当前bytecode里每个pc位置对应哪条instruction。另一个是SpecialCSRProperties表：当前标准机器允许哪些delegation CSR。这些东西和程序或机器配置有关，但在一次证明里是固定的，不是prover随便填的witness。

第二类是constraint description。它描述这台RISC-V机器每一行应该满足什么规则。比如一行如果decode成ADD，那么rd = rs1 + rs2；如果decode成LW，那么要产生RAM read query；如果执行CSR delegation，那么CSR id要合法。这类规则会先写成Variable、Constraint、LookupQuery、ShuffleRamMemQuery，再被OneRowCompiler编译成CompiledCircuitArtifact。

第三类是setup precomputations。固定lookup表不能只是存在内存里，后端证明还需要它们被写成setup trace、做LDE、构造Merkle tree。SetupPrecomputations保存的就是固定setup trace的LDE结果和Merkle trees。

第四类是witness-time input。比如basic_fibonacci执行后寄存器里是什么，dynamic_fibonacci从输入里读到的n是多少，某个cycle读了哪个RAM地址，某条ADD的rs1/rs2/rd具体值是多少。这些都不在get_main_riscv_circuit_setup里生成。它们属于后面的VM执行和witness generation。

用一句话压缩本章边界：

setup准备“固定证明环境”；witness generation准备“这次执行的具体事实”。
3.2 get_main_riscv_circuit_setup：入口函数逐行读

源码位置：

circuit_defs/setups/src/circuits/main_riscv/mod.rs

函数签名是：

pub fn get_main_riscv_circuit_setup<A: GoodAllocator, B: GoodAllocator>(
    bytecode: &[u32],
    worker: &Worker,
) -> MainCircuitPrecomputations<IMStandardIsaConfig, A, B> {
    ...
}

A和B是allocator类型参数。Airbender会分配很大的trace、FFT buffer、LDE buffer和Merkle tree输入，所以这些对象不是简单地默认用普通Vec就完事，而是通过allocator参数控制内存来源。CPU主路径里调用的是::<Global, Global>，也就是普通全局分配器。

函数体非常短，但每一行都连接到一大块系统：

let delegation_csrs = IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS;

这行拿到标准ISA配置允许的delegation CSR白名单。main RISC-V machine可以通过特殊CSR调用delegation circuit，比如BLAKE2或BigInt，但不是任意CSR都允许。这个白名单后面会被写入SpecialCSRProperties表。

let machine: cs::one_row_compiler::CompiledCircuitArtifact<Mersenne31Field> =
    ::risc_v_cycles::get_machine(bytecode, delegation_csrs);

这行生成编译后的main RISC-V约束artifact。变量名叫machine，但类型已经是CompiledCircuitArtifact，所以它不是原始machine对象，而是“机器约束编译后的产物”。源码里这一行明确标注了返回类型。

let table_driver = ::risc_v_cycles::get_table_driver(bytecode, delegation_csrs);

这行生成lookup表集合。注意，get_machine内部也会创建ROM表和CSR表，但那是为了把这些表交给compiler参与布局；get_table_driver返回的是后续setup和witness路径要直接使用的表内容集合。

let twiddles: Twiddles<_, A> = Twiddles::new(::risc_v_cycles::DOMAIN_SIZE, &worker);

这行生成FFT预计算数据。它和RISC-V语义没有直接关系，是后端把trace当作多项式处理时需要的工具。

let lde_precomputations = LdePrecomputations::new(
    ::risc_v_cycles::DOMAIN_SIZE,
    ::risc_v_cycles::LDE_FACTOR,
    ::risc_v_cycles::LDE_SOURCE_COSETS,
    &worker,
);

这行生成LDE预计算。LDE是low-degree extension，后端为了证明trace对应低度多项式，会把主domain上的多项式评价扩展到更大的domain上。main RISC-V这里LDE_FACTOR = 2。

let setup =
    SetupPrecomputations::from_tables_and_trace_len(
        &table_driver,
        ::risc_v_cycles::DOMAIN_SIZE,
        &machine.setup_layout,
        &twiddles,
        &lde_precomputations,
        ::risc_v_cycles::LDE_FACTOR,
        ::risc_v_cycles::TREE_CAP_SIZE,
        &worker,
    );

这行把固定表内容写进setup trace，并对setup trace做LDE和Merkle commitment。它需要table_driver提供表内容，也需要machine.setup_layout告诉它setup列应该怎样排。源码里SetupPrecomputations::from_tables_and_trace_len接收的参数正是这些。

最后打包返回：

MainCircuitPrecomputations {
    compiled_circuit: machine,
    table_driver,
    twiddles,
    lde_precomputations,
    setup,
    witness_eval_fn_for_gpu_tracer: ::risc_v_cycles::witness_eval_fn_for_gpu_tracer,
}

这个返回对象就是后面prove_image_execution要消费的main circuit固定输入包。源码里可以看到这些字段按原样返回。

3.3 MainCircuitPrecomputations：返回对象逐字段读

MainCircuitPrecomputations定义在circuit_defs/setups/src/lib.rs：

pub struct MainCircuitPrecomputations<C: MachineConfig, A: GoodAllocator, B: GoodAllocator = Global>
{
    pub compiled_circuit: cs::one_row_compiler::CompiledCircuitArtifact<Mersenne31Field>,
    pub table_driver: TableDriver<Mersenne31Field>,
    pub twiddles: Twiddles<Mersenne31Complex, A>,
    pub lde_precomputations: LdePrecomputations<A>,
    pub setup: SetupPrecomputations<DEFAULT_TRACE_PADDING_MULTIPLE, A, DefaultTreeConstructor>,
    pub witness_eval_fn_for_gpu_tracer: fn(&mut SimpleWitnessProxy<'_, MainRiscVOracle<'_, C, B>>),
}

源码结构和字段可以直接看到。

逐字段解释：

compiled_circuit是编译后的约束artifact。后面prover要靠它知道trace列怎么排、memory列怎么排、setup列怎么排、约束有哪些、public input在哪些边界行、各种lookup/memory argument怎么布局。它是规则和布局，不是当前执行的值。

table_driver是所有lookup table的内容集合。它里面有ROM表、CSR表、decoder表、range表、bit operation表等。setup阶段用它生成固定setup trace；witness evaluator也可能用它按key查value或查行号。

twiddles是FFT预计算。证明后端要把trace evaluations转换到多项式相关domain，会反复用到这些旋转因子。

lde_precomputations是LDE预计算。后端做低度扩展和FRI相关步骤时用。

setup是固定setup trace的LDE和Merkle tree。最终proof里的setup_tree_caps来自这里。Proof结构体里确实有setup_tree_caps字段，说明setup tree cap是proof的一部分。

witness_eval_fn_for_gpu_tracer是witness生成函数指针。名字里有GPU tracer，但本质上它代表“给定执行oracle之后，如何把witness写进列”的函数入口。它不在setup阶段执行，只是被打包带走。

用例子讲：basic_fibonacci/app.bin和dynamic_fibonacci/app.bin如果binary不同，它们的ROM表和setup tree会不同；如果同一个binary只是输入不同，比如输入n不同，那么get_main_riscv_circuit_setup的输出不应该因为输入n变化而变化。输入n属于后面的QuasiUARTSource和witness execution路径，不属于setup固定输入。

3.4 下钻get_machine：从bytecode到CompiledCircuitArtifact

源码位置：

circuit_defs/risc_v_cycles/src/lib.rs

get_main_riscv_circuit_setup调用的是：

risc_v_cycles::get_machine(bytecode, delegation_csrs)

get_machine本身很薄：

pub fn get_machine(
    bytecode: &[u32],
    delegation_csrs: &[u32],
) -> one_row_compiler::CompiledCircuitArtifact<field::Mersenne31Field> {
    get_machine_for_rom_bound::<ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(bytecode, delegation_csrs)
}

也就是说，真正做事的是get_machine_for_rom_bound。源码里get_machine和get_machine_for_rom_bound就在risc_v_cycles/src/lib.rs中。

3.4.1 ROM_ADDRESS_SPACE_SECOND_WORD_BITS为什么是5

risc_v_cycles定义：

MAX_ROM_SIZE = 1 << 21
ROM_ADDRESS_SPACE_SECOND_WORD_BITS = MAX_ROM_SIZE.trailing_zeros() - 16

MAX_ROM_SIZE = 2^21，所以trailing_zeros()是21，减16得到5。这个常量表示ROM地址空间在某种拆分表示里，除了低16位以外，还需要5个高位。它最终影响ROM表长度。

get_machine_for_rom_bound第一步检查：

assert_eq!(
    bytecode.len(),
    (1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS)) / 4
);

代入5：

bytecode.len()=
4
2
21
	​

=2
19

所以bytecode必须正好是2^19个u32。如果没有经过get_padded_binary，这个assert会失败。这个检查保证ROM表大小、setup layout、lookup table容量都是固定的。

3.4.2 创建machine类型

接着：

let machine = FullIsaMachineWithDelegationNoExceptionHandling;

这个名字拆开读：

FullIsa:
  支持比较完整的RV32I + M指令面。

WithDelegation:
  支持通过CSR触发delegation circuits，例如BLAKE2和BigInt。

NoExceptionHandling:
  trusted code模型，不展开trap/exception处理。

因此，这个main machine证明的是“正常执行路径”。非法opcode、未对齐访问等不会通过异常分支被证明成合法执行；更直接地说，这类行为会导致约束无法满足或程序不在支持范围里。

3.4.3 创建RomRead表

然后：

let rom_table = create_table_for_rom_image::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(
    &bytecode,
    TableType::RomRead.to_table_id(),
);

这一步把当前程序写成ROM lookup table。ROM表后面用于证明：每个cycle里当前pc对应的instruction确实来自这份bytecode。

先看函数本身。

3.5 create_table_for_rom_image：逐行读ROM表怎么生成

源码位置：

cs/src/machine/machine_configurations/mod.rs

函数签名：

pub fn create_table_for_rom_image<
    F: PrimeField,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    image: &[u32],
    id: u32,
) -> LookupTable<F, 3>

它返回LookupTable<F, 3>，表示每行有3个field元素。源码注释明确写了ROM表的样子：第一列是地址，后两列是对应4字节instruction的低16位和高16位；注释里还说明这样拆是因为prime field略小于32 bits。

第一段检查ROM bound：

assert!(
    image.len() * 4 <= 1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS),
    ...
);

这里的image.len()是u32个数，乘4才是字节数。它要求程序字节数不能超过ROM上界。

接着计算：

let keys_len = 1usize << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS - 2);

为什么减2？因为ROM地址按字节计数，但每条instruction是4字节对齐。地址空间有2
16+k
个字节，按4字节一行，就有：

2
16+k−2

行。默认k=5，所以行数是：

2
16+5−2
=2
19

源码里随后创建keys，每个key只填第一列：

let address = i * 4;
key[0] = F::from_u64_unchecked(address as u64);

也就是ROM lookup的key是pc地址，地址依次是0、4、8、12……

然后调用：

LookupTable::<F, 3>::create_table_from_key_and_key_generation_closure(...)

这个closure接收key，也就是某个pc地址，然后生成ROM表的value。

closure内部先取pc：

let pc = key[0].as_u64_reduced();

然后检查两件事：

pc < ROM bound
pc % 4 == 0

第一条保证pc没有超出ROM；第二条保证pc是4字节对齐的instruction地址。源码里这两个assert都在closure里。

接着算index：

let index = (pc as usize) / 4;

然后取opcode：

let opcode = if index < image.len() {
    image[index]
} else {
    UNIMP_OPCODE
};

如果index落在程序image里，就取真实instruction；如果超出image，就填UNIMP_OPCODE。在当前主路径里，bytecode已经pad满，所以大多数情况下image.len()等于keys_len；但这个函数本身也支持未完全填满的image，用UNIMP补齐。

再把32-bit opcode拆成两个16-bit：

let low = opcode as u16;
let high = (opcode >> 16) as u16;

写入result：

result[0] = F::from_u64_unchecked(low as u64);
result[1] = F::from_u64_unchecked(high as u64);

这里有一个细节：LookupTable<F, 3>的key也是3宽，value也是3宽的内部表示，但这张ROM表有1个key列和2个value列。调用create_table_from_key_and_key_generation_closure时传了1，表示前1列是key；closure返回的value有效部分是低16和高16。源码里这一段对应low/high/result。

最后还有一个快速index lookup closure：

Some(|keys| {
    let pc = keys[0].as_u64_reduced();
    ...
    let index = (pc / 4) as usize;
    index
})

它的作用是：给定pc，快速知道它在ROM表的第几行。比如pc=12，对应index=3。后面witness或lookup multiplicity路径需要查表行号时可以直接用。

源码里还自带了一个rom_table_test。测试用image [100_000, 200_000, 0]，检查pc=0、4、8、12时的lookup结果。其中pc=12已经超出image，于是读到UNIMP_OPCODE拆成的低16和高16。这个测试非常适合确认ROM表拆分逻辑。

用一个手算例子看：

instruction = 0x00b50533
pc = 4

拆成：

low16  = 0x0533
high16 = 0x00b5

ROM lookup行可以理解为：

key:   [4]
value: [0x0533, 0x00b5]

证明时，某一行如果pc witness是4，RomRead lookup会强迫instruction limbs必须等于这张表里的低16和高16。这样prover不能把pc=4处的指令偷偷换成别的opcode。

3.6 create_csr_table_for_delegation：把delegation白名单变成表

get_machine_for_rom_bound接着创建CSR表：

let csr_table = create_csr_table_for_delegation(
    true,
    delegation_csrs,
    TableType::SpecialCSRProperties.to_table_id(),
);

函数本身很薄：

pub fn create_csr_table_for_delegation<F: PrimeField>(
    allow_non_determinism: bool,
    allowed_delegation_csrs: &[u32],
    id: u32,
) -> LookupTable<F, 3> {
    use crate::csr_properties::create_special_csr_properties_table;
    create_special_csr_properties_table(id, allow_non_determinism, allowed_delegation_csrs)
}

也就是说，真实表内容生成在create_special_csr_properties_table里；这一章先只读到边界：main setup把IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS交给CSR properties table生成函数。源码里create_csr_table_for_delegation确实只是转发。

这张表的目的很明确：当guest程序通过CSR请求delegation时，main circuit要证明这个CSR id在允许集合里。比如BLAKE2 delegation和BigInt delegation都有自己的type id；Standard machine允许它们，Reduced machine可能只允许其中一部分，Final reduced machine可能不允许delegation。

所以CSR表的读法是：

SpecialCSRProperties不是执行BLAKE2的表。
它只是证明“这个CSR调用是被main machine允许的delegation入口”。

真正BLAKE2或BigInt内部计算由对应delegation circuit证明。

3.7 default_compile_machine：把Machine写进BasicAssembly，再交给OneRowCompiler

get_machine_for_rom_bound最后调用：

default_compile_machine::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(
    machine,
    rom_table,
    Some(csr_table),
    DOMAIN_SIZE.trailing_zeros() as usize,
)

源码里DOMAIN_SIZE.trailing_zeros()就是22，因为DOMAIN_SIZE = 2^22。

现在下钻default_compile_machine，源码位置：

cs/src/lib.rs

签名是：

pub fn default_compile_machine<
    M: crate::machine::Machine<::field::Mersenne31Field>,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    machine: M,
    bytecode_table: LookupTable<Mersenne31Field, 3>,
    csr_table: Option<LookupTable<Mersenne31Field, 3>>,
    trace_len_log2: usize,
) -> CompiledCircuitArtifact<Mersenne31Field>

它接收四个东西：machine定义、RomRead表、可选CSR表、trace长度log2。源码签名和返回值在cs/src/lib.rs中。

函数第一步：

let mut cs_output = compile_machine::<
    Mersenne31Field,
    BasicAssembly<Mersenne31Field>,
    M,
    ROM_ADDRESS_SPACE_SECOND_WORD_BITS,
>(machine);

这一步把Machine写入BasicAssembly，得到CircuitOutput。你可以先把BasicAssembly理解为“约束记录器”。Machine代码调用它来申请变量、加约束、加lookup、加memory query，最后finalize成CircuitOutput。

函数第二步把RomRead表塞进cs_output.table_driver：

cs_output.table_driver.add_table_with_content(
    TableType::RomRead,
    LookupWrapper::Dimensional3(bytecode_table),
);

第三步如果有CSR表，也塞进去：

if let Some(csr_table) = csr_table {
    cs_output.table_driver.add_table_with_content(
        TableType::SpecialCSRProperties,
        LookupWrapper::Dimensional3(csr_table),
    );
}

这两步很关键。compile_machine会注册通用表、decoder表等，但当前bytecode生成的ROM表和当前CSR白名单生成的CSR表是program-specific的，所以在default_compile_machine这一层补进去。源码里这几行可以直接看到。

最后：

let compiler = OneRowCompiler::default();
let compiler_output =
    compiler.compile_output_for_chunked_memory_argument(cs_output, trace_len_log2);

这一步把CircuitOutput编译成CompiledCircuitArtifact。源码里default_compile_machine最后返回compiler_output。

所以default_compile_machine的完整流程是：

Machine
  |
  v
compile_machine
  |
  v
CircuitOutput
  |
  +-- add RomRead table
  +-- add SpecialCSRProperties table
  |
  v
OneRowCompiler
  |
  v
CompiledCircuitArtifact
3.8 compile_machine：Machine第一次真正写出“单行状态转移”

源码位置：

cs/src/machine/machine_configurations/mod.rs

compile_machine代码不长：

let mut cs = C::new();

create_table_driver_into_cs::<F, C, M, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(&mut cs, machine);

let (initial_state, final_state) =
    M::describe_state_transition::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(&mut cs);

let mut initial_state_vars = vec![];
initial_state.append_into_variables_set(&mut initial_state_vars);

let mut final_state_vars = vec![];
final_state.append_into_variables_set(&mut final_state_vars);

let (mut output, _) = cs.finalize();
output.state_input = initial_state_vars;
output.state_output = final_state_vars;

output

源码对应这几步：创建CS、把表注册进CS、调用M::describe_state_transition、收集initial/final state变量、finalize成CircuitOutput。

逐行解释。

let mut cs = C::new();

这里的C在当前调用里是BasicAssembly<Mersenne31Field>。它是Circuit trait的实现。Circuit trait提供了加变量、加约束、加memory query、加delegation request、注册表、finalize等接口。

create_table_driver_into_cs(&mut cs, machine);

这一步把通用lookup表注册到cs内部。注意它和get_table_driver很像，但目标不同：get_table_driver返回独立的TableDriver对象；create_table_driver_into_cs把表注册到正在构造的Circuit里。源码里可以看到它会cs.materialize_table(...)，还会把decoder表、ROM地址分隔表等内容加入CS。

let (initial_state, final_state) =
    M::describe_state_transition::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(&mut cs);

这是机器语义进入约束系统的核心点。M是FullIsaMachineWithDelegationNoExceptionHandling。这个方法会描述“一行RISC-V执行”如何从初始状态变成最终状态。这里会涉及pc、ROM fetch、decoder、register/RAM query、opcode selection、delegation request等。第4章和第5章会深入这里，因为opcode gadget和Term/Constraint都从这个函数链条里出现。

这一章先读它的接口意义：describe_state_transition不是执行某个具体程序，而是向cs写入约束模板。它返回initial_state和final_state，表示这一行开始和结束时需要跨行连接的状态变量。

initial_state.append_into_variables_set(&mut initial_state_vars);
final_state.append_into_variables_set(&mut final_state_vars);

这两行把state里的变量收集成Vec。后面state_input和state_output会用于相邻行连接。比如上一行的final pc要等于下一行的initial pc。

let (mut output, _) = cs.finalize();
output.state_input = initial_state_vars;
output.state_output = final_state_vars;

finalize把BasicAssembly里积累的变量、约束、lookup、memory query、delegation request等内容吐出来，形成CircuitOutput。然后把刚才收集的state变量填进CircuitOutput。

3.9 CircuitOutput：Machine写出的“未排版约束草稿”

CircuitOutput定义在cs/src/cs/circuit.rs。它包含很多字段：

pub struct CircuitOutput<F: PrimeField> {
    pub state_input: Vec<Variable>,
    pub state_output: Vec<Variable>,
    pub table_driver: TableDriver<F>,
    pub num_of_variables: usize,
    pub constraints: Vec<(Constraint<F>, bool)>,
    pub lookups: Vec<LookupQuery<F>>,
    pub shuffle_ram_queries: Vec<ShuffleRamMemQuery>,
    pub delegated_computation_requests: Vec<DelegatedComputationRequest>,
    pub degegated_request_to_process: Option<DelegatedProcessingData>,
    pub batched_memory_accesses: Vec<BatchedMemoryAccessType>,
    pub register_and_indirect_memory_accesses: Vec<RegisterAndIndirectAccesses>,
    pub linked_variables: Vec<LinkedVariablesPair>,
    pub range_check_expressions: Vec<RangeCheckQuery<F>>,
    pub boolean_vars: Vec<Variable>,
    pub substitutions: HashMap<(Placeholder, usize), Variable>,
}

源码里字段列表可以看到。

这个结构非常重要。它是compiler边界前的“约束草稿”。它还没有把变量排成trace列，只是用Variable编号描述所有东西。

逐类解释。

state_input和state_output保存跨行状态变量。main RISC-V通常最核心的是pc状态。它们告诉后面的compiler：这一行的结束状态要和下一行的开始状态连接。

constraints保存普通多项式约束。比如某个变量必须等于两个变量相加，某个flag必须满足布尔性，某个candidate relation必须为0。这里的Constraint<F>还基于Variable，不是最终列地址。

lookups保存普通lookup查询。LookupQuery里有一行LookupInput和一个表类型，表类型可以是常量表，也可以由变量决定。源码中LookupQuery和LookupQueryTableType定义在Circuit文件中。

shuffle_ram_queries保存RAM/register统一memory argument查询。ShuffleRamQueryType有两类：RegisterOnly和RegisterOrRam。RegisterOrRam带一个is_register布尔值和address limbs；当is_register=1时解释为寄存器访问，当is_register=0时解释为RAM访问。源码里ShuffleRamQueryType和ShuffleRamMemQuery定义在circuit.rs。

这点对理解Airbender和SP1差异很关键。寄存器不是单独32个跨行状态列，而是被编码成memory argument里的特殊地址空间。比如：

read register x1:
  is_register = 1
  address = 1

read RAM[0x1000]:
  is_register = 0
  address = 0x1000

delegated_computation_requests保存main circuit向delegation circuit发出的请求。比如某行CSR触发BLAKE2 delegation，就会产生一个request。源码里的DelegatedComputationRequest包含execute、degegation_type和memory_offset_high。

range_check_expressions保存range check请求。比如某个表达式需要证明落在16-bit范围内，就会生成range check query。

boolean_vars保存必须为0/1的变量。后面compiler会为它们生成布尔约束或相关布局。

substitutions保存placeholder到变量的映射。这个后面witness generation和生成代码会用到，比如“某个公开输入位置”或“某个特殊变量”要找到对应Variable。

此时还没有ColumnAddress。你可以把CircuitOutput理解成：

Machine说：
  我需要这些变量。
  我需要这些约束。
  我需要这些lookup。
  我需要这些RAM/register query。
  我需要这些delegation request。
但我还没决定它们在trace矩阵的第几列。
3.10 OneRowCompiler：从Variable世界进入ColumnAddress世界

default_compile_machine拿到CircuitOutput后，调用：

let compiler = OneRowCompiler::default();
let compiler_output =
    compiler.compile_output_for_chunked_memory_argument(cs_output, trace_len_log2);

compile_output_for_chunked_memory_argument在cs/src/one_row_compiler/compile_layout.rs里。它只是调用：

Self::compile_inner::<false>(self, circuit_output, trace_len_log2)

其中false表示这不是delegation circuit，而是main chunked memory argument路径。源码里两个入口分别是compile_output_for_chunked_memory_argument和compile_to_evaluate_delegations。

compile_inner开头的注释非常重要，源码直接写了它的任务：

// - place variables in particular grid places
// - select whether they go into witness subtree or memory subtree
// - normalize constraints to address particular columns insteap of variable indexes
// - try to apply some heuristrics

这四句就是compiler边界。

逐句翻译。

第一，place variables in particular grid places。CircuitOutput里只有Variable(0)、Variable(1)这种编号；真正prover需要的是“第几列”。compiler会决定每个变量落到哪个区域、哪个列offset。

第二，select whether they go into witness subtree or memory subtree。Airbender的trace不是单一平面。变量可能属于普通witness列，也可能属于memory argument相关列，还可能属于setup列。compiler要把它们分区域。

第三，normalize constraints to address particular columns instead of variable indexes。约束原来写成Variable表达式；compiler要把它改写成ColumnAddress表达式。后面prover/verifier评价约束时，不会再按Variable找值，而是直接从witness row、memory row、setup row里按列地址读值。

第四，try to apply some heuristics。compiler还会做一些布局优化和变量放置策略。这个以后读性能细节时再展开。

compile_inner一开始会把CircuitOutput拆开：

let CircuitOutput {
    state_input,
    state_output,
    table_driver,
    num_of_variables,
    constraints,
    lookups,
    shuffle_ram_queries,
    linked_variables,
    range_check_expressions,
    boolean_vars,
    substitutions,
    delegated_computation_requests,
    degegated_request_to_process,
    batched_memory_accesses,
    register_and_indirect_memory_accesses,
} = circuit_output;

源码里就是这个结构解构。

然后它先做分支检查：

if FOR_DELEGATION { ... } else { ... }

对main RISC-V路径，FOR_DELEGATION=false，源码要求：

assert_eq!(shuffle_ram_queries.len(), 3);
assert!(linked_variables.is_empty());
assert!(degegated_request_to_process.is_none());
assert!(batched_memory_accesses.is_empty());
assert!(register_and_indirect_memory_accesses.is_empty());

这说明main RISC-V每行在这一层预期有3个shuffle RAM queries。直观上可以对应典型RISC-V一行的几类访问槽位，比如读寄存器、读寄存器/内存、写寄存器/内存。具体每个query如何对应不同opcode，后面读state transition时再展开。源码中这个main路径检查在else分支。

接着它算：

let trace_len = 1usize << trace_len_log2;
let total_tables_size = table_driver.total_tables_len;
let lookup_table_encoding_capacity = trace_len - 1;

这就连接到前面的H和N。如果trace_len_log2=22，那么：

trace_len = H = 2^22
lookup_table_encoding_capacity = H - 1

为什么table encoding capacity是trace_len - 1？因为setup trace最后一行不用于普通表内容。源码里compile_inner用它计算generic lookup setup需要多少列组。

然后它创建setup_layout：

let need_timestamps = !FOR_DELEGATION;
let setup_layout =
    SetupLayout::layout_for_lookup_size(total_tables_size, trace_len, need_timestamps);

main路径FOR_DELEGATION=false，所以need_timestamps=true。也就是说main RISC-V setup需要timestamp相关setup columns。源码里这一步在compile_inner中。

这就进入下一个对象：SetupLayout。

3.11 SetupLayout：固定列怎么排

SetupLayout定义在cs/src/definitions/setup_tree.rs：

pub struct SetupLayout {
    pub timestamp_setup_columns: ColumnSet<NUM_TIMESTAMP_COLUMNS_FOR_RAM>,
    pub range_check_16_setup_column: ColumnSet<1>,
    pub timestamp_range_check_setup_column: ColumnSet<1>,
    pub generic_lookup_setup_columns: ColumnSet<NUM_COLUMNS_FOR_COMMON_TABLE_WIDTH_SETUP>,
    pub total_width: usize,
}

源码里字段只有这几类。

解释每一类：

timestamp_setup_columns用于shuffle RAM timestamp相关固定列。main RISC-V需要，因为它有RAM/register memory argument。

range_check_16_setup_column是16-bit range check固定列。很多RISC-V值会拆成16-bit limb，所以需要固定范围表。

timestamp_range_check_setup_column是timestamp相关范围检查固定列。

generic_lookup_setup_columns用于所有普通lookup表的统一编码。ROM表、decoder表、CSR表、其他通用表最终都会通过TableDriver.dump_tables()拼成统一格式，写入这里。

total_width是setup trace总列宽。

layout_for_lookup_size做的事情很具体：

let encoding_capacity = trace_len - 1;
let mut num_required_setup_tuples = lookups_total_table_len / encoding_capacity;
if lookups_total_table_len % encoding_capacity != 0 {
    num_required_setup_tuples += 1;
}

也就是说，所有generic lookup表总共有lookups_total_table_len行。每组generic lookup setup columns最多放trace_len - 1行。如果放不下，就多开一组columns。源码里就是这样计算。

然后它从offset=0开始依次布局：

timestamp_setup_columns
range_check_16_setup_column
timestamp_range_check_setup_column
generic_lookup_setup_columns

最后得到total_width。

所以setup_layout回答的是：

固定setup trace有多少列？
每一类固定列从第几列开始？
generic lookup表需要多少组4列？

这就是为什么SetupPrecomputations需要machine.setup_layout：固定表内容怎么写入setup trace，必须按compiler给出的布局来。

3.12 CompiledCircuitArtifact：OneRowCompiler的最终产物

compile_inner最后会生成CompiledCircuitArtifact。你原文列出的字段很重要，我们这里用“消费者视角”解释。

CompiledCircuitArtifact包含：

witness_layout
memory_layout
setup_layout
stage_2_layout
degree_2_constraints
degree_1_constraints
state_linkage_constraints
public_inputs
variable_mapping
scratch_space_size_for_witness_gen
lazy_init_address_aux_vars
memory_queries_timestamp_comparison_aux_vars
batched_memory_access_timestamp_comparison_aux_vars
register_and_indirect_access_timestamp_comparison_aux_vars
trace_len
table_offsets
total_tables_size

虽然我们没有在这里完整贴源码定义，但它位于cs/src/one_row_compiler/mod.rs，并被get_main_riscv_circuit_setup返回对象中的compiled_circuit字段持有。OneRowCompiler相关代码还定义了如何用compiled degree constraints在row上读取witness/memory/setup数据进行评价。

逐类解释：

witness_layout告诉witness trace怎么排。后面witness evaluator根据执行轨迹，把pc、opcode flags、寄存器值、RAM访问值等写到对应列。

memory_layout告诉shuffle RAM相关列怎么排。因为register和RAM访问都走memory argument，所以这部分很重要。

setup_layout就是刚才讲的固定列布局。后面SetupPrecomputations会用它写setup trace。

stage_2_layout服务lookup和memory argument后续阶段。Airbender prover不是只有stage 1 witness，它还有stage 2 lookup/memory argument相关数据。

degree_1_constraints和degree_2_constraints是编译后的普通约束。它们已经不再用Variable，而是用ColumnAddress。后端评价时直接从row里按地址取值。源码里CompiledDegree1Constraint和CompiledDegree2Constraint都有evaluate_at_row...方法，接收witness_row、memory_row，某些版本还接收setup_row。这说明编译后的约束已经面向具体列布局。

state_linkage_constraints保存相邻行状态连接。比如上一行的final pc要接到下一行的initial pc。具体连接哪些变量由state_input和state_output经过compiler处理产生。

public_inputs告诉proof哪些边界位置的列值是公开输入。比如最终pc、某些end params相关值会进入外部验证。

variable_mapping保存原始Variable -> ColumnAddress的映射。witness generation需要它，因为Machine写约束时产生的是Variable，真正填trace时必须知道Variable对应哪一列。

trace_len就是H = 2^22。

table_offsets和total_tables_size服务lookup表拼接后的偏移。因为TableDriver把很多表拼到generic setup里，后面需要知道某张表从拼接后的哪个offset开始。

这一步之后，约束系统从“抽象变量世界”进入“列布局世界”。这就是本章标题里的compiler边界。

3.13 get_table_driver：独立生成表内容

现在回到get_main_riscv_circuit_setup里的另一条线：

let table_driver = ::risc_v_cycles::get_table_driver(bytecode, delegation_csrs);

get_table_driver同样在circuit_defs/risc_v_cycles/src/lib.rs，它转发到get_table_driver_for_rom_bound。源码中可以看到它也检查bytecode长度，然后构造TableDriver。

它的流程是：

1. assert bytecode长度符合ROM上界。
2. 创建FullIsaMachineWithDelegationNoExceptionHandling。
3. 调用create_table_driver(machine)，生成通用表。
4. 用当前bytecode生成RomRead表，加入TableDriver。
5. 用当前delegation_csrs生成SpecialCSRProperties表，加入TableDriver。
6. 返回TableDriver。

源码里get_table_driver_for_rom_bound创建machine、调用create_table_driver、添加RomRead表、添加SpecialCSRProperties表。

现在继续下钻create_table_driver。

3.14 create_table_driver：通用表、decoder表、ROM辅助表

源码位置：

cs/src/machine/machine_configurations/mod.rs

函数签名：

pub fn create_table_driver<
    F: PrimeField,
    M: Machine<F>,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    machine: M,
) -> TableDriver<F>

源码注释说：这个函数用于需要“CS-detached table driver”的地方，比如proving或setup。也就是说，它生成的是独立表集合，不依附于正在构造的Circuit。

第一步：

let used_tables = M::define_used_tables();

机器类型自己声明需要哪些表。接着有几个assert，禁止machine自己声明一些特殊表，比如ZeroEntry、OpTypeBitmask、CsrBitmask、RangeCheckSmall。这些表由通用逻辑统一加入，不能由machine重复声明。源码里这些assert在函数开头。

第二步：

let extra_tables = machine.define_additional_tables();

这允许machine提供额外表内容，但会检查它们不要和used_tables重复。

第三步创建空TableDriver：

let mut table_driver = TableDriver::new();

然后对used_tables逐个materialize_table。materialize_table表示“这个表是标准表，可以由TableType自动生成”。源码里循环调用table_driver.materialize_table(table)。

第四步把extra_tables加进去：

for (table, content) in extra_tables.into_iter() {
    table_driver.add_table_with_content(table, content);
}

第五步手动materialize一些通用表：

table_driver.materialize_table(TableType::And);
table_driver.materialize_table(TableType::ZeroEntry);
table_driver.materialize_table(TableType::QuickDecodeDecompositionCheck4x4x4);
table_driver.materialize_table(TableType::QuickDecodeDecompositionCheck7x3x6);
table_driver.materialize_table(TableType::U16GetSignAndHighByte);
table_driver.materialize_table(TableType::RangeCheckSmall);

这些表覆盖bit operations、zero entry、quick decode decomposition、16-bit高字节/符号辅助、小范围检查等。源码里这些表被固定加入。

第六步创建decoder表：

let decoder_table = M::create_decoder_table(TableType::OpTypeBitmask.to_table_id());
table_driver.add_table_with_content(
    TableType::OpTypeBitmask,
    LookupWrapper::Dimensional3(decoder_table),
);

decoder表是后面instruction decode的重要固定表。它帮助把instruction编码分解成opcode flags。源码里OpTypeBitmask表就是在这里加入的。

第七步，如果machine使用ROM存bytecode，就加入RomAddressSpaceSeparator表：

if M::USE_ROM_FOR_BYTECODE {
    let id = TableType::RomAddressSpaceSeparator.to_table_id();
    let table = LookupWrapper::Dimensional3(create_rom_separator_table::<...>(id));
    table_driver.add_table_with_content(TableType::RomAddressSpaceSeparator, table);
}

这张表不是RomRead表本身，而是ROM地址空间相关的辅助表。真正的当前程序RomRead表在get_table_driver_for_rom_bound外层手动加入，因为它依赖当前bytecode。源码里这段在create_table_driver末尾。

因此，create_table_driver(machine)生成的是“机器通用表集合”。随后get_table_driver_for_rom_bound再补入program-specific的RomRead和SpecialCSRProperties。

3.15 create_table_driver_into_cs：同样的表注册到Circuit里

compile_machine调用的是：

create_table_driver_into_cs(&mut cs, machine);

这个函数和create_table_driver几乎平行，但目标不是独立TableDriver，而是Circuit。它调用：

cs.materialize_table(...)
cs.add_table_with_content(...)

而不是：

table_driver.materialize_table(...)
table_driver.add_table_with_content(...)

源码中可以看到两者逻辑高度相似：注册used tables、extra tables、通用表、decoder表、ROM地址辅助表。

为什么要有两套？

create_table_driver:
  给setup/prover使用，生成独立TableDriver对象。

create_table_driver_into_cs:
  给compiler使用，把表信息注册到正在构造的Circuit里。

这也解释了为什么你会看到get_machine和get_table_driver都创建表。Airbender在compiler层和setup/prover层都需要知道表，但用途不同。

3.16 TableDriver、LookupTable、LookupWrapper先怎么理解

你原文里写了LookupTable、LookupWrapper、TableDriver字段。这里换成更容易读的版本。

LookupTable<F, N>是一张宽度为N的表。比如RomRead是LookupTable<F, 3>。它的核心内容可以分成两类：

data:
  表的所有行，顺序保存。

lookup_data / quick_value_lookup_fn:
  给定key，快速查value。

content_data / quick_index_lookup_fn:
  给定完整行，快速查行号。

为什么既要能查value，又要能查index？

因为不同阶段需求不同。witness evaluator可能知道pc，需要查ROM value；lookup argument可能需要知道某个表项在哪个全局表偏移位置，从而更新multiplicity或生成查询。

LookupWrapper是因为不同表宽度不同。有的表宽度1，有的2，有的3。Rust类型上LookupTable<F, 1>和LookupTable<F, 3>不是同一个类型，所以用枚举包起来：

Uninitialized
Dimensional1(...)
Dimensional2(...)
Dimensional3(...)

TableDriver则是一整个表集合。它用TableType作为槽位，把所有表放在一起。后面dump_tables会把这些表统一拼成Vec<[F; 4]>：前3列是表行内容，第4列是table id。这样setup trace可以用统一宽度编码所有generic lookup表。

这个设计可以用一句话记住：

LookupTable是一张表；LookupWrapper让不同宽度的表能放进同一个容器；TableDriver是所有表的总目录。
3.17 SetupPrecomputations：把固定表写成setup trace并承诺

回到get_main_riscv_circuit_setup：

let setup =
    SetupPrecomputations::from_tables_and_trace_len(
        &table_driver,
        DOMAIN_SIZE,
        &machine.setup_layout,
        &twiddles,
        &lde_precomputations,
        LDE_FACTOR,
        TREE_CAP_SIZE,
        worker,
    );

现在可以真正看懂这行了。

输入分三类。

第一类是固定表内容：

table_driver

它提供RomRead、CSR、decoder、range等表的真实行。

第二类是布局：

machine.setup_layout
DOMAIN_SIZE

setup_layout告诉我们setup trace有多少列、每类固定列放在哪里；DOMAIN_SIZE告诉我们setup trace有多少行。

第三类是后端预计算：

twiddles
lde_precomputations
LDE_FACTOR
TREE_CAP_SIZE
worker

它们用于LDE和Merkle tree。

SetupPrecomputations结构体只有两个字段：

pub struct SetupPrecomputations<const N: usize, A: GoodAllocator, T: MerkleTreeConstructor> {
    pub ldes: Vec<CosetBoundTracePart<N, A>>,
    pub trees: Vec<T>,
}

源码定义显示它保存setup trace的LDE结果和Merkle trees。

from_tables_and_trace_len执行四步。

第一步，生成主domain上的setup trace：

let mut main_domain_trace =
    Self::get_main_domain_trace(table_driver, trace_len, setup_layout, worker);

第二步，调整最后一行：

adjust_to_zero_c0_var_length(&mut main_domain_trace, 0..setup_layout.total_width, worker);

源码注释说不使用setup的最后一行，并且必须调整到c0 == 0。这和前面trace_len - 1作为表编码容量相呼应。

第三步，做LDE：

let ldes = compute_wide_ldes(
    main_domain_trace,
    twiddles,
    lde_precomputations,
    0,
    lde_factor,
    worker,
);

第四步，为每个LDE coset构造Merkle tree：

for domain in ldes.iter() {
    let tree = T::construct_for_coset(&domain.trace, subtree_cap_size, true, worker);
    trees.push(tree);
}

源码里这四步连续出现。

3.18 get_main_domain_trace：setup trace每一行写什么

get_main_domain_trace是真正把TableDriver写入setup trace的地方。

先创建一张全零表：

let main_domain_trace =
    RowMajorTrace::new_zeroed_for_size(trace_len, setup_layout.total_width, A::default());

它的高度是trace_len = H，宽度是setup_layout.total_width。源码里就是这样创建。

然后计算每组generic lookup列能放多少表行：

let table_encoding_capacity_per_tuple = trace_len - 1;

如果所有表总长度超过这个容量，就需要多组generic lookup columns。源码中会根据table_driver.total_tables_len计算num_table_subsets，并要求它等于setup_layout.generic_lookup_setup_columns.num_elements()。

接着：

let all_generic_tables = table_driver.dump_tables();

这一步把所有lookup表拼成统一格式的表行。每一行可以理解为：

[table_col_0, table_col_1, table_col_2, table_id]

源码随后assert拼接行数等于table_driver.total_tables_len。

然后创建两个固定range表：

range_check_16_table = 0..2^16
timestamp_range_check_table = 0..2^TIMESTAMP_COLUMNS_NUM_BITS

源码里这两个表是直接按整数范围生成field元素。

然后把generic tables按trace_len - 1切块：

let generic_tables_chunks: Vec<_> = all_generic_tables
    .chunks(table_encoding_capacity_per_tuple)
    .collect();

每个chunk写到一组generic_lookup_setup_columns里。源码里也检查chunk数量和setup layout里的元素数量一致。

最后进入worker并行写每一行。对每个absolute_row_idx：

如果当前行号小于2^16，写入16-bit range table：

trace_view_row[setup_layout.range_check_16_setup_column.start()] =
    range_check_16_table_content_ref[absolute_row_idx];

如果当前行号小于timestamp range表长度，写timestamp range table：

trace_view_row[setup_layout.timestamp_range_check_setup_column.start()] =
    timestamp_range_check_table_content_ref[absolute_row_idx];

对每个generic table chunk，如果当前行号还在chunk范围内，就取出一行table row，并写入对应的generic lookup columns：

let table_row = encoding_chunk[absolute_row_idx];
let range = setup_layout.generic_lookup_setup_columns.get_range(tuple_idx);
trace_view_row[range].copy_from_slice(&table_row);

如果setup layout里有timestamp setup columns，还写timestamp：

let timestamp = (absolute_row_idx as u64) + 1;
let timestamp_shifted = timestamp << NUM_EMPTY_BITS_FOR_RAM_TIMESTAMP;
...
trace_view_row[timestamp_setup_start] = timestamp_low;
trace_view_row[timestamp_setup_start + 1] = timestamp_high;

这些逻辑都在get_main_domain_trace的行循环里。

这段代码告诉我们setup trace到底装了什么：

setup trace row i:
  可能包含range_check_16_table[i]
  可能包含timestamp_range_check_table[i]
  可能包含若干generic lookup table rows
  可能包含timestamp setup value

再强调一次：setup trace不包含guest执行中的x5=16、RAM[0x1000]=16、当前行is_add=1这些东西。它只包含固定表和固定辅助列。

3.19 Twiddles和LDE：这章只读到用途边界

Twiddles::new和LdePrecomputations::new属于后端预计算。它们在get_main_riscv_circuit_setup里被创建，是因为setup trace和后续witness trace都需要做FFT/LDE。这里不展开FRI，但要知道它们服务于下面这条链：

trace values
  -> 多项式评价
  -> LDE扩展域评价
  -> Merkle commitment
  -> 后端低度测试 / FRI

在本章里，它们不是RISC-V语义，不是compiler语义，也不是witness。它们是“证明后端数学工具”的预计算。

3.20 get_machine和get_table_driver为什么看起来重复

现在可以回答一个常见困惑：为什么get_machine和get_table_driver都创建RomRead和CSR表？

因为二者输出服务不同边界。

get_machine:
  目标是CompiledCircuitArtifact。
  也就是约束规则、列布局、setup layout、public input位置、memory layout。
  它需要知道ROM/CSR表存在，才能编译lookup布局。

get_table_driver:
  目标是TableDriver。
  也就是真实lookup表内容。
  它要给SetupPrecomputations写setup trace，也可能给witness路径查表使用。

可以用一个具体ADD例子统一理解。

假设bytecode第0行是：

pc = 0x0000
instruction = ADD x5, x1, x2

TableDriver里会有RomRead表行：

[0x0000, opcode_low16, opcode_high16, RomRead_table_id]

CompiledCircuitArtifact里会有约束布局：

这一行根据pc查RomRead。
RomRead输出进入decoder。
decoder产生is_add flag。
当is_add = 1时，ADD关系必须成立。

执行时的witness trace里才会有：

rs1_value = 7
rs2_value = 9
rd_value = 16
is_add = 1

setup trace、compiled circuit、witness trace各管一层：

TableDriver / setup trace:
  固定表内容。

CompiledCircuitArtifact:
  规则和列布局。

witness trace:
  当前执行的具体值。
3.21 prove_image_execution如何消费这些对象

create_proofs_internal拿到main_circuit_precomputations后，会和delegation precomputations一起传给：

prover_examples::prove_image_execution(
    num_instances,
    &binary,
    non_determinism_source,
    &main_circuit_precomputations,
    &delegation_precomputations,
    &worker,
)

源码中这就是CPU Standard分支的返回路径。

在后续prover里，这些字段大致这样用：

compiled_circuit:
  决定witness layout、memory layout、setup layout、degree constraints、public inputs。

table_driver:
  提供ROM/CSR/decoder/range等表内容。

setup:
  提供固定setup trace的LDE和Merkle trees。
  proof里的setup_tree_caps来自这里。

twiddles / lde_precomputations:
  支持后续trace和setup trace的FFT/LDE。

witness_eval_fn_for_gpu_tracer:
  执行trace进入witness列时使用。

用basic_fibonacci和dynamic_fibonacci区分：

basic_fibonacci/app.bin:
  影响bytecode。
  影响RomRead表。
  影响setup tree。

dynamic_fibonacci/input.txt:
  不影响RomRead表。
  不影响setup tree。
  它进入QuasiUARTSource，影响VM执行和witness trace。

所以，如果同一个binary用不同输入运行，get_main_riscv_circuit_setup生成的固定部分不变；后面的witness和proof会变。

3.22 本章读完后的对象地图

到这里，这章不是宏观略读，而是已经把每个函数下钻到它的直接底层职责了。最终对象关系如下：

get_main_riscv_circuit_setup
  输入:
    padded bytecode
    worker
  输出:
    MainCircuitPrecomputations

MainCircuitPrecomputations
  compiled_circuit:
    OneRowCompiler输出的CompiledCircuitArtifact。
  table_driver:
    所有lookup表内容。
  twiddles:
    FFT预计算。
  lde_precomputations:
    LDE预计算。
  setup:
    setup trace的LDE和Merkle trees。
  witness_eval_fn_for_gpu_tracer:
    witness填充函数指针。

get_machine
  输入:
    bytecode
    delegation_csrs
  下钻:
    get_machine_for_rom_bound
      assert bytecode length
      create FullIsaMachineWithDelegationNoExceptionHandling
      create RomRead table
      create SpecialCSRProperties table
      default_compile_machine
  输出:
    CompiledCircuitArtifact

create_table_for_rom_image
  输入:
    image: &[u32]
    table id
  输出:
    LookupTable<F, 3>
  行语义:
    key = pc
    value = opcode_low16, opcode_high16

create_csr_table_for_delegation
  输入:
    allowed_delegation_csrs
  输出:
    SpecialCSRProperties lookup table

default_compile_machine
  下钻:
    compile_machine -> CircuitOutput
    add RomRead table
    add SpecialCSRProperties table
    OneRowCompiler::compile_output_for_chunked_memory_argument
  输出:
    CompiledCircuitArtifact

compile_machine
  下钻:
    create_table_driver_into_cs
    M::describe_state_transition
    cs.finalize
  输出:
    CircuitOutput

CircuitOutput
  仍然使用Variable编号。
  包含constraints、lookups、shuffle_ram_queries、delegation requests、boolean vars、state input/output。

OneRowCompiler
  把Variable世界编译到ColumnAddress世界。
  输出witness_layout、memory_layout、setup_layout、degree constraints、public inputs等。

SetupLayout
  描述setup trace固定列布局。
  根据total lookup table size和trace_len计算generic lookup setup columns数量。

SetupPrecomputations::from_tables_and_trace_len
  下钻:
    get_main_domain_trace
    adjust last row
    compute_wide_ldes
    construct Merkle trees
  输出:
    ldes
    trees
3.23 本章最终理解

这一章可以用一句话结束：

get_main_riscv_circuit_setup把“程序固定内容”和“机器约束规则”编译成prover能消费的固定材料包；真正执行程序、生成寄存器/RAM witness，是后面的prove_image_execution和witness generation路径负责的。

下一章可以继续从default_compile_machine -> compile_machine -> M::describe_state_transition往下读。那里会第一次进入真正的RISC-V一行语义：pc如何进入ROM lookup，decoder如何产生opcode flags，register/RAM query如何组织，OptimizationContext如何收集各个opcode候选关系，最后约束怎样落到Term和Constraint。



对，这个函数之前只讲了“它会生成CSR表”，但没有把表的形状、输入、输出、每一列含义讲清楚。我们现在专门读它。

函数在：

```text id="sarz20"
cs/src/machine/machine_configurations/mod.rs
```

源码是：

```rust id="xclcyp"
pub fn create_csr_table_for_delegation<F: PrimeField>(
    allow_non_determinism: bool,
    allowed_delegation_csrs: &[u32],
    id: u32,
) -> LookupTable<F, 3> {
    use crate::csr_properties::create_special_csr_properties_table;
    create_special_csr_properties_table(id, allow_non_determinism, allowed_delegation_csrs)
}
```

它本身只是一个薄封装，真正建表逻辑在`create_special_csr_properties_table`。`create_csr_table_for_delegation`把三个输入原样传给它：`id`、`allow_non_determinism`、`allowed_delegation_csrs`。

## 1. 这张表到底解决什么问题

先从CSR说起。CSR是RISC-V里的“control and status register”。普通RISC-V程序可以通过CSR指令读写某些特殊寄存器。在Airbender里，有些特殊CSR被用作“扩展入口”。

这张表要回答的问题是：

```text id="kbxjm4"
某个CSR编号，是否是Airbender main circuit支持的特殊CSR？
如果支持，它是不是delegation CSR？
```

所以这张表不是保存程序代码的。程序代码在`RomRead`表里。CSR表保存的是“哪些CSR编号有特殊含义”。

main circuit里会遇到CSR相关指令。比如程序里执行某个CSR读写：

```text id="bmwi58"
CSRRW / CSRRS / CSRRC / CSRRWI / ...
```

这时电路要判断这个CSR是不是允许的特殊CSR。如果它是non-determinism CSR，就允许从非确定输入源读数据。如果它是delegation CSR，就允许发起delegation request。不是白名单里的CSR，不能随便当特殊入口使用。

## 2. 调用位置：Standard main circuit里传了什么

在main RISC-V路径里，`get_machine_for_rom_bound`这样调用它：

```rust id="fbeso7"
let csr_table = create_csr_table_for_delegation(
    true,
    delegation_csrs,
    TableType::SpecialCSRProperties.to_table_id(),
);
```

也就是：

```text id="1hhyue"
allow_non_determinism = true
allowed_delegation_csrs = delegation_csrs
id = TableType::SpecialCSRProperties.to_table_id()
```

`delegation_csrs`来自main machine配置里的`ALLOWED_DELEGATION_CSRS`，`risc_v_cycles`里也把它暴露出来；`get_machine_for_rom_bound`把这张CSR表和ROM表一起交给`default_compile_machine`。

所以这张表是main circuit setup的一部分。后面`default_compile_machine`会把它以`TableType::SpecialCSRProperties`加入`CircuitOutput.table_driver`。

## 3. 输入一：`F: PrimeField`

```rust id="90jaaz"
F: PrimeField
```

和ROM表一样，`F`表示这张lookup table里的元素使用哪个有限域。

main RISC-V里实际是：

```text id="i3t6ay"
Mersenne31Field
```

这张CSR表最终返回：

```rust id="2v1gud"
LookupTable<F, 3>
```

说明每一行有3个field element。

所有普通整数，比如CSR编号、flag 0/1，都会被转成`F`里的元素：

```rust id="3sy0fj"
F::from_u64_unchecked(...)
```

这里和ROM表不同的是：CSR表里的值都很小。CSR编号是12-bit，flag是0/1，因此放进`Mersenne31Field`没有问题。

## 4. 输入二：`allow_non_determinism: bool`

```rust id="74nag4"
allow_non_determinism: bool
```

这个参数控制是否支持一个特殊CSR：

```rust id="rwmocx"
NON_DETERMINISM_CSR
```

在模拟器状态代码里，`NON_DETERMINISM_CSR`定义为：

```rust id="csym21"
pub const NON_DETERMINISM_CSR: u32 = 0x7c0;
```

也就是说，CSR编号`0x7c0`被Airbender用作non-determinism输入入口。

它的作用可以先理解成：guest程序通过这个CSR从外部输入源读数据。

比如`dynamic_fibonacci`这种程序，`n`不是写死在程序binary里的，而是从输入里传进来。执行时，程序可能通过某个CSR读取这个非确定输入。这里的“非确定”不是说乱给，而是说它不是由程序bytecode固定决定的，而是prover提供的私有或外部输入。

所以：

```text id="3vrndi"
allow_non_determinism = true:
  CSR 0x7c0 被认为是支持的特殊CSR。

allow_non_determinism = false:
  CSR 0x7c0 不被这张表标记为支持。
```

在main RISC-V setup里传的是`true`，所以Standard main circuit支持non-determinism CSR。

## 5. 输入三：`allowed_delegation_csrs: &[u32]`

```rust id="5y3u4r"
allowed_delegation_csrs: &[u32]
```

这是delegation CSR白名单。

它的含义是：

```text id="6l76g8"
哪些CSR编号可以触发delegation circuit。
```

比如概念上可以想象：

```text id="w3jk2a"
allowed_delegation_csrs = [
  CSR_FOR_BLAKE2,
  CSR_FOR_BIGINT,
  ...
]
```

真实常量你可以后面再追`IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS`，但这一层先理解它的语义：这个slice里的每个`u32`都是一个允许的delegation入口编号。

`create_special_csr_properties_table`会先检查：

```rust id="xqtk97"
for el in supported_delegations.iter() {
    assert!(*el < (1 << 12));
}
```

CSR编号必须小于`2^12`。这是因为RISC-V instruction里的CSR字段本来就是12-bit。

所以这张表覆盖的key空间是：

```text id="z2aggl"
0..4095
```

也就是所有12-bit CSR编号。

## 6. 输入四：`id: u32`

```rust id="7gbxwm"
id: u32
```

这个`id`是lookup table的表编号。

调用处传的是：

```rust id="0xjffi"
TableType::SpecialCSRProperties.to_table_id()
```

也就是说，这张表的身份是：

```text id="397cqe"
SpecialCSRProperties
```

为什么需要`id`？

因为Airbender后面会把很多lookup table拼成一个generic lookup setup区域。比如：

```text id="3iyjvk"
RomRead
SpecialCSRProperties
OpTypeBitmask
RangeCheckSmall
And
ZeroEntry
...
```

为了防止不同表的数据混在一起，dump成generic setup rows时需要带上`table_id`。因此`id`不是CSR编号，而是“这整张表是哪一种表”的编号。

## 7. 真正建表函数：`create_special_csr_properties_table`

实际逻辑在：

```rust id="k9mgt7"
pub fn create_special_csr_properties_table<F: PrimeField>(
    id: u32,
    support_non_determinism_csr: bool,
    supported_delegations: &[u32],
) -> LookupTable<F, 3>
```

它返回的也是：

```rust id="85jp17"
LookupTable<F, 3>
```

这张表每一行宽度为3。它使用1个key列，2个value列。

源码里调用：

```rust id="io47jo"
LookupTable::<F, 3>::create_table_from_key_and_key_generation_closure(
    &keys,
    TABLE_NAME.to_string(),
    1,
    move |key| { ... },
    Some(first_key_index_gen_fn::<F, 3>),
    id,
)
```

这里的`1`表示`num_key_columns = 1`。由于总宽度是3，所以value列数量是2。`LookupTable`构建时会把前`num_key_columns`列作为key，剩下列作为value。

所以表形状是：

```text id="f2ye5o"
key:
  csr_index

value:
  is_supported
  is_allowed_for_delegation
```

完整行是：

```text id="hwmc3b"
[csr_index, is_supported, is_allowed_for_delegation]
```

## 8. keys：为什么是所有12-bit CSR

源码：

```rust id="e29knu"
let keys = key_for_continuous_log2_range(12);
```

这表示key覆盖一个连续的`2^12`范围。也就是：

```text id="kgiqea"
0, 1, 2, ..., 4095
```

每个key都是一个CSR编号。

为什么覆盖全部4096个CSR，而不是只列出支持的CSR？

因为这样查询任意CSR时都能得到一个确定答案：

```text id="5nfu65"
这个CSR支持吗？
这个CSR是delegation吗？
```

如果只保存白名单里的CSR，那么查一个不支持的CSR时就会“查不到”。但这张表更像一个属性表，对每个CSR编号都给出属性：

```text id="k2qljv"
支持 -> 1
不支持 -> 0
是否delegation -> 1/0
```

这对电路更方便。

## 9. 每一行怎么生成

核心closure是：

```rust id="a4hjik"
move |key| {
    let input = key[0].as_u64_reduced();
    assert!(input < (1u64 << 12));
    let csr_index = input as u32;
    let is_nondeterminism_csr = csr_index == NON_DETERMINISM_CSR as u32;
    let is_allowed_for_delegation = supported_delegations.contains(&csr_index);
    assert!(is_nondeterminism_csr & is_allowed_for_delegation == false);
    let is_supported =
        (is_nondeterminism_csr & support_non_determinism_csr) | is_allowed_for_delegation;

    let result = [
        F::from_u64_unchecked(is_supported as u64),
        F::from_u64_unchecked(is_allowed_for_delegation as u64),
        F::ZERO,
    ];

    (input as usize, result)
}
```

这里一步步解释。

第一步，拿CSR编号：

```text id="ww44hp"
csr_index = key[0]
```

也就是这一行表在描述哪个CSR。

第二步，判断它是不是non-determinism CSR：

```text id="goi0kj"
is_nondeterminism_csr = csr_index == 0x7c0
```

第三步，判断它是不是delegation白名单里的CSR：

```text id="8m9pei"
is_allowed_for_delegation = supported_delegations.contains(csr_index)
```

第四步，检查一个CSR不能同时是non-determinism CSR和delegation CSR：

```rust id="radc7n"
assert!(is_nondeterminism_csr & is_allowed_for_delegation == false);
```

也就是说：

```text id="w068ri"
0x7c0不能同时出现在delegation白名单里。
```

第五步，算`is_supported`：

```rust id="hh6dr5"
let is_supported =
    (is_nondeterminism_csr & support_non_determinism_csr) | is_allowed_for_delegation;
```

换成普通话：

```text id="9c0lqf"
如果它是non-determinism CSR，并且当前配置允许non-determinism，那么支持。
或者，如果它在delegation白名单里，那么支持。
否则不支持。
```

第六步，输出value：

```text id="n363ua"
value[0] = is_supported
value[1] = is_allowed_for_delegation
```

由于`LookupTable<F, 3>`总宽度是3，而且key列占1列，所以只有前两个result值会作为value列使用。第三个`F::ZERO`是padding。`LookupTable`的通用实现会把key列和value列拼成完整行；它根据`num_key_columns`决定value列数量。

所以这张表的真实语义是：

```text id="mmcby8"
SpecialCSRProperties(csr_index)
  -> (is_supported, is_allowed_for_delegation)
```

## 10. 表格例子

假设：

```text id="yro8la"
support_non_determinism_csr = true
NON_DETERMINISM_CSR = 0x7c0
supported_delegations = [0x801, 0x802]
```

那么表中关键行会是：

```text id="6mr6zo"
csr_index   is_supported   is_allowed_for_delegation

0x000       0              0
0x7bf       0              0
0x7c0       1              0
0x801       1              1
0x802       1              1
0x803       0              0
```

解释：

```text id="pgghg8"
0x7c0:
  是non-determinism CSR。
  因为support_non_determinism_csr = true，所以is_supported = 1。
  它不是delegation，所以is_allowed_for_delegation = 0。

0x801:
  在delegation白名单里。
  所以is_supported = 1，is_allowed_for_delegation = 1。

0x000:
  既不是non-determinism CSR，也不在delegation白名单。
  所以两个flag都是0。
```

如果`support_non_determinism_csr = false`，那么`0x7c0`这一行会变成：

```text id="ljpxlb"
0x7c0 -> [0, 0]
```

但delegation白名单里的CSR仍然是：

```text id="cme3b2"
0x801 -> [1, 1]
```

## 11. 这张表在约束里怎么用

当main circuit遇到CSR相关指令时，指令里会带一个12-bit CSR编号。电路可以查：

```text id="jrkmnb"
SpecialCSRProperties(csr_index)
  -> is_supported, is_allowed_for_delegation
```

然后根据结果走不同逻辑。

如果是普通不支持的CSR：

```text id="sy3rrx"
is_supported = 0
is_allowed_for_delegation = 0
```

它不能被当作Airbender特殊入口使用。

如果是non-determinism CSR：

```text id="kx45eh"
is_supported = 1
is_allowed_for_delegation = 0
```

它表示这是一个受支持的特殊CSR，但不是delegation。典型用途是从non-determinism输入源读数据。

如果是delegation CSR：

```text id="aurlsa"
is_supported = 1
is_allowed_for_delegation = 1
```

它表示这条CSR操作可以触发delegation request。后面main circuit会产生`DelegatedComputationRequest`，再由对应delegation circuit证明具体计算。

## 12. 和RomRead表对比

你可以把两张表放在一起理解。

`RomRead`表：

```text id="ldlr86"
key:
  pc

value:
  opcode_low16
  opcode_high16

作用:
  证明某个pc对应的instruction来自当前bytecode。
```

`SpecialCSRProperties`表：

```text id="z14o76"
key:
  csr_index

value:
  is_supported
  is_allowed_for_delegation

作用:
  证明某个CSR编号是不是Airbender支持的特殊CSR，以及是不是delegation入口。
```

所以：

```text id="o67bpl"
RomRead管“程序里有什么指令”。
SpecialCSRProperties管“CSR编号有什么特殊含义”。
```

如果程序里有一条CSR指令，那么流程大概是：

```text id="90lpp9"
1. pc查RomRead，拿到instruction。
2. decoder发现这是一条CSR类指令，并取出csr_index。
3. csr_index查SpecialCSRProperties。
4. 根据is_supported / is_allowed_for_delegation决定约束逻辑。
```

## 13. 为什么这张表是LookupTable<F, 3>

这里再强调一次`LookupTable<F, 3>`的`3`。

CSR表的逻辑行是：

```text id="flpm96"
[csr_index, is_supported, is_allowed_for_delegation]
```

其中：

```text id="v2pj8l"
csr_index:
  key列

is_supported:
  value列1

is_allowed_for_delegation:
  value列2
```

所以总宽度刚好是3。

构造函数里传`num_key_columns = 1`，于是`LookupTable`知道：

```text id="0zdf80"
前1列是key。
后2列是value。
```

这和ROM表的形状类似：

```text id="amzxr8"
RomRead:
  [pc, opcode_low16, opcode_high16]

SpecialCSRProperties:
  [csr_index, is_supported, is_allowed_for_delegation]
```

两张表都是`LookupTable<F, 3>`，但语义完全不同。

## 14. 最终一句话

`create_csr_table_for_delegation`的作用是：

```text id="jxu4g8"
根据“是否允许non-determinism CSR”和“允许delegation的CSR白名单”，生成一张覆盖所有12-bit CSR编号的SpecialCSRProperties表。
```

这张表让main RISC-V circuit可以在遇到CSR指令时查出：

```text id="1kpfen"
这个CSR是否是Airbender支持的特殊CSR；
它是否是delegation入口。
```

如果把`create_table_for_rom_image`理解为：

```text id="k57x95"
pc -> instruction
```

那么`create_csr_table_for_delegation`就可以理解为：

```text id="b0bgta"
csr_index -> (is_supported, is_delegation)
```

## 第三章 从bytecode到main RISC-V电路固定环境

这一章正式读`get_main_riscv_circuit_setup`背后的函数链路。先给结论：

`get_main_riscv_circuit_setup`不是执行RISC-V程序，也不是生成witness，更不是生成proof。它做的是证明前的固定准备工作：把当前bytecode变成ROM表，把delegation CSR白名单变成CSR表，把main RISC-V machine编译成列布局和约束布局，再把固定表写成setup trace并承诺。

这条链路可以这样看：

```text
bytecode
  |
  +-- get_machine
  |     -> CompiledCircuitArtifact
  |
  +-- get_table_driver
  |     -> TableDriver
  |
  +-- SetupPrecomputations::from_tables_and_trace_len
        -> setup LDEs + setup Merkle trees
```

换一种更直观的说法：

```text
get_machine:
  生成“这台机器的规则书”。

get_table_driver:
  生成“这台机器要查的所有固定表”。

SetupPrecomputations:
  把这些固定表写成setup trace，并做承诺。
```

如果把Airbender prover想成一个考场，那么：

```text
CompiledCircuitArtifact:
  试卷规则。告诉系统每道题怎么判分。

TableDriver:
  附录资料。保存ROM表、CSR表、decoder表、range表等真实内容。

SetupPrecomputations:
  把附录资料固定下来，做成承诺，后面proof必须和这些固定资料一致。

witness trace:
  考生的答卷。也就是某次执行中寄存器、RAM、opcode flag等实际取值。
```

本章只读固定环境。ADD、LW、SW这些指令的具体约束怎么写，从下一章开始进入`Machine::describe_state_transition`和`cs/src/machine/...`继续展开。

### 3.1 先记住三种数据：规则、表、执行值

Airbender里很容易把三类东西混在一起。先拆开。

第一类是规则，也就是约束系统。

例如ADD的规则是：

```text
rd = rs1 + rs2
```

这不是某一次执行的值，而是一条通用规则。无论x1、x2具体是多少，只要当前行是ADD，这条关系就必须成立。

第二类是固定表。

例如ROM表里保存：

```text
pc = 0x0000 -> instruction = ADD x5, x1, x2
pc = 0x0004 -> instruction = SW x5, 0(x10)
pc = 0x0008 -> instruction = LW x6, 0(x10)
```

这些内容由当前程序bytecode决定。换了程序，ROM表就变。

第三类是执行值，也就是witness。

例如某次执行时：

```text
x1 = 7
x2 = 9
x5 = 16
RAM[0x1000] = 16
```

这些值不是setup阶段生成的，而是执行程序后才知道。

所以一条ADD在证明里其实会被三类东西共同描述：

```text
ROM表：
  当前pc处的instruction确实是ADD x5, x1, x2。

约束规则：
  如果当前instruction是ADD，那么rd = rs1 + rs2。

witness：
  rs1 = 7，rs2 = 9，rd = 16。
```

三者合起来，证明系统才能确认：当前程序在这一行确实执行了一条合法ADD，而且结果正确。

### 3.2 get_machine：把RISC-V machine编译成规则书

代码位置：

```text
circuit_defs/risc_v_cycles/src/lib.rs
```

`get_main_riscv_circuit_setup`里第一条主线是：

```rust
let machine: cs::one_row_compiler::CompiledCircuitArtifact<Mersenne31Field> =
    risc_v_cycles::get_machine(bytecode, delegation_csrs);
```

这里的`machine`变量名字有点容易误导。它已经不是原始的“机器配置对象”，而是编译后的：

```text
CompiledCircuitArtifact<Mersenne31Field>
```

可以把它理解成一份已经排好版的约束系统说明书。它告诉后端：

```text
哪些变量放在witness列；
哪些变量放在memory列；
哪些固定列放在setup列；
有哪些一次约束；
有哪些二次约束；
public input在哪些位置；
state input和state output怎么连接；
lookup和memory argument的布局是什么。
```

`get_machine`本身很短：

```rust
pub fn get_machine(
    bytecode: &[u32],
    delegation_csrs: &[u32],
) -> CompiledCircuitArtifact<Mersenne31Field> {
    get_machine_for_rom_bound::<ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(bytecode, delegation_csrs)
}
```

也就是它继续跳到`get_machine_for_rom_bound`。

### 3.3 get_machine_for_rom_bound：先检查ROM大小

进入`get_machine_for_rom_bound`后，第一件事是检查bytecode长度：

```rust
assert_eq!(
    bytecode.len(),
    (1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS)) / 4
);
```

这里要解释一下为什么有这个检查。

Airbender的main RISC-V circuit有固定ROM上界：

```text
MAX_ROM_SIZE = 2^21 bytes
```

bytecode进入电路前会被切成`u32`数组。一个`u32`是4字节，所以ROM里一共有：

```text
ROM_WORDS = 2^21 / 4 = 2^19
```

个word。

因此`bytecode.len()`必须等于`2^19`。如果原始程序没有这么长，就用`UNIMP_OPCODE`之类的内容padding到固定大小。

为什么不能让ROM表按程序长度变化？

因为setup trace、lookup表布局、Merkle commitment都希望有稳定形状。固定ROM大小能让main circuit结构稳定。当前程序的真实长度可以短，但进入证明系统的ROM image要是固定大小。

可以这样理解：

```text
app.bin:
  真实程序，可能只有几KB。

padded bytecode:
  证明系统看到的ROM image，固定为2^19个u32。

RomRead表:
  按这个固定ROM image生成。
```

### 3.4 get_machine_for_rom_bound：选择FullIsaMachineWithDelegationNoExceptionHandling

检查完bytecode长度后，代码创建：

```rust
let machine = FullIsaMachineWithDelegationNoExceptionHandling;
```

这个名字很长，但其实已经把机器性质讲清楚了。

```text
FullIsa:
  支持比较完整的RV32I + M指令集合。

WithDelegation:
  支持通过CSR调用delegation circuit，例如BLAKE2、BigInt。

NoExceptionHandling:
  不处理异常路径。默认程序是trusted code。
```

这里的“没有异常处理”很重要。普通CPU遇到非法指令、未对齐访问、trap等情况，会进入异常处理流程。但这个main circuit不证明这些异常流程。程序如果做了不被支持的行为，通常就是约束无法满足，proof生成不了。

所以这台machine关注的是正常执行路径：

```text
ROM fetch
opcode decode
寄存器/RAM访问
指令语义
pc更新
delegation请求
```

不是完整操作系统意义上的CPU。

### 3.5 get_machine_for_rom_bound：创建RomRead表

接下来是：

```rust
let rom_table = create_table_for_rom_image::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(
    &bytecode,
    TableType::RomRead.to_table_id(),
);
```

这一步把当前程序bytecode写成一张ROM lookup表。

它的意义是：每个cycle里，电路知道当前`pc`，但必须证明`pc`对应的instruction确实来自当前程序。这个证明通过`RomRead` lookup完成。

ROM表每行大概是：

```text
pc      opcode_low16    opcode_high16
0       ...
4       ...
8       ...
12      ...
```

为什么opcode要拆成低16位和高16位？

Airbender使用的主域是`Mersenne31Field`，模数是`2^31 - 1`。一个32-bit opcode最大可能接近`2^32 - 1`，不能直接安全塞进一个field element。拆成两个16-bit limb以后，每个limb都小于`2^16`，可以安全放进域元素。

例如某条指令机器码是：

```text
0x00b50533
```

拆成：

```text
low16  = 0x0533
high16 = 0x00b5
```

如果这条指令在`pc = 4`的位置，ROM表会包含：

```text
[4, 0x0533, 0x00b5]
```

后面main circuit会做：

```text
RomRead(pc) -> (opcode_low16, opcode_high16)
```

然后decoder继续根据这两个16-bit值判断它到底是ADD、LW、SW、JAL还是别的指令。

### 3.6 create_table_for_rom_image逐段读

现在进入函数本身：

```rust
pub fn create_table_for_rom_image<
    F: PrimeField,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    image: &[u32],
    id: u32,
) -> LookupTable<F, 3>
```

返回值是：

```text
LookupTable<F, 3>
```

这里的`3`表示表宽是3个field element。

对于ROM表来说，这3个位置是：

```text
key:
  pc

value:
  opcode_low16
  opcode_high16
```

所以可以理解成：

```text
RomRead: pc -> (opcode_low16, opcode_high16)
```

函数先检查ROM上界：

```rust
assert!(
    image.len() * 4 <= 1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS)
);
```

意思是：`image`按字节算不能超过ROM容量。

接着计算：

```rust
let keys_len = 1usize << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS - 2);
```

为什么要减2？

因为ROM容量按字节算，但一条RISC-V instruction是4字节对齐的word。地址空间有`2^(16+k)`个字节，就有：

```text
2^(16+k) / 4 = 2^(16+k-2)
```

个instruction slot。

然后它并行生成所有key：

```rust
let address = i * 4;
key[0] = F::from_u64_unchecked(address as u64);
```

也就是说，第`i`行的key是：

```text
pc = i * 4
```

后面通过closure生成value：

```rust
let pc = key[0].as_u64_reduced();
assert!(pc % 4 == 0);
let index = (pc as usize) / 4;
let opcode = if index < image.len() {
    image[index]
} else {
    UNIMP_OPCODE
};
let low = opcode as u16;
let high = (opcode >> 16) as u16;
```

这段逻辑很直观：

```text
1. 从key拿pc。
2. 检查pc必须4字节对齐。
3. index = pc / 4。
4. 找到image[index]这个u32 opcode。
5. 拆成low16和high16。
```

如果index超过image长度，就填`UNIMP_OPCODE`。不过在main path里，bytecode已经pad到固定ROM大小，所以一般不会越界。

函数最后返回一个`LookupTable<F, 3>`。这个表既可以用key查value，也可以用完整行查index。

源码里的测试也很好地说明了拆分方式。例如`image = [100000, 200000, 0]`时，测试检查：

```text
pc=0 -> [0x86a0, 0x1]
pc=4 -> [0x0d40, 0x3]
pc=8 -> [0x0, 0x0]
pc=12 -> UNIMP拆成[0x1073, 0xc000]
```

这正好说明ROM表的value确实是`opcode_low16, opcode_high16`。

### 3.7 get_machine_for_rom_bound：创建CSR delegation表

接下来是：

```rust
let csr_table = create_csr_table_for_delegation(
    true,
    delegation_csrs,
    TableType::SpecialCSRProperties.to_table_id(),
);
```

这个表服务delegation。

普通ADD、LW、SW这类指令由main RISC-V circuit直接约束。但是BLAKE2、BigInt这种复杂计算，如果全部展开成普通RISC-V指令，会很贵。Airbender允许程序通过特殊CSR调用delegation circuit。

所以main circuit需要检查：

```text
当前CSR id是不是允许的delegation入口？
```

`create_csr_table_for_delegation`就是把白名单`delegation_csrs`做成`SpecialCSRProperties` lookup表。

函数本身很短：

```rust
pub fn create_csr_table_for_delegation<F: PrimeField>(
    allow_non_determinism: bool,
    allowed_delegation_csrs: &[u32],
    id: u32,
) -> LookupTable<F, 3> {
    use crate::csr_properties::create_special_csr_properties_table;
    create_special_csr_properties_table(id, allow_non_determinism, allowed_delegation_csrs)
}
```

这里先不用进入`create_special_csr_properties_table`。目前只需要知道：这张表告诉main machine哪些special CSR是合法的。

如果程序没有用delegation，这张表仍然可以存在。因为Standard machine支持delegation，只是某次执行没有触发它。

### 3.8 get_machine_for_rom_bound：default_compile_machine

最后`get_machine_for_rom_bound`调用：

```rust
let compiled_machine =
    default_compile_machine::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(
        machine,
        rom_table,
        Some(csr_table),
        DOMAIN_SIZE.trailing_zeros() as usize,
    );
```

这一步才是真正从“机器描述”变成“编译后电路artifact”。

输入有四个：

```text
machine:
  FullIsaMachineWithDelegationNoExceptionHandling

rom_table:
  当前程序bytecode生成的RomRead表

csr_table:
  当前允许delegation CSR生成的SpecialCSRProperties表

trace_len_log2:
  log2(DOMAIN_SIZE)，main RISC-V里是22
```

输出是：

```text
CompiledCircuitArtifact
```

这一步可以理解成“编译器”：

```text
抽象RISC-V machine
  |
  v
BasicAssembly记录变量、约束、lookup、memory query
  |
  v
CircuitOutput
  |
  v
OneRowCompiler分配列布局
  |
  v
CompiledCircuitArtifact
```

下面开始读这条编译路径。

### 3.9 default_compile_machine：进入BasicAssembly

代码位置：

```text
cs/src/lib.rs
```

函数签名：

```rust
pub fn default_compile_machine<
    M: Machine<Mersenne31Field>,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    machine: M,
    bytecode_table: LookupTable<Mersenne31Field, 3>,
    csr_table: Option<LookupTable<Mersenne31Field, 3>>,
    trace_len_log2: usize,
) -> CompiledCircuitArtifact<Mersenne31Field>
```

它先调用：

```rust
let mut cs_output = compile_machine::<
    Mersenne31Field,
    BasicAssembly<Mersenne31Field>,
    M,
    ROM_ADDRESS_SPACE_SECOND_WORD_BITS,
>(machine);
```

这一步含义是：

```text
用BasicAssembly作为Circuit实现，运行machine的describe_state_transition。
```

`BasicAssembly`可以理解成一个“约束收集器”。Machine代码执行时，不是在真的执行RISC-V程序，而是在不断向`BasicAssembly`说：

```text
我要一个变量；
我要这个变量是boolean；
我要添加一条约束；
我要添加一个lookup；
我要添加一个shuffle RAM query；
我要注册一张表；
```

最后`BasicAssembly::finalize()`会把这些东西收集成`CircuitOutput`。

然后`default_compile_machine`把ROM表加进`cs_output.table_driver`：

```rust
cs_output.table_driver.add_table_with_content(
    TableType::RomRead,
    LookupWrapper::Dimensional3(bytecode_table),
);
```

如果有CSR表，也加进去：

```rust
cs_output.table_driver.add_table_with_content(
    TableType::SpecialCSRProperties,
    LookupWrapper::Dimensional3(csr_table),
);
```

注意这里：`compile_machine`本身会注册很多通用表，但program-specific的ROM表和CSR表是在`default_compile_machine`这里补进`CircuitOutput.table_driver`的。

最后：

```rust
let compiler = OneRowCompiler::default();
let compiler_output =
    compiler.compile_output_for_chunked_memory_argument(cs_output, trace_len_log2);
```

也就是把`CircuitOutput`送进`OneRowCompiler`，得到最终`CompiledCircuitArtifact`。

### 3.10 compile_machine：Machine真正写出一行RISC-V状态转移

代码位置：

```text
cs/src/machine/machine_configurations/mod.rs
```

函数签名：

```rust
pub fn compile_machine<
    F: PrimeField,
    C: Circuit<F>,
    M: Machine<F>,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    machine: M,
) -> CircuitOutput<F>
```

这一层是真正把machine写进constraint system的地方。

代码主流程是：

```rust
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
```

我们逐段理解。

第一步：

```rust
let mut cs = C::new();
```

创建一个空的constraint system收集器。这里`C`在`default_compile_machine`中被指定为：

```text
BasicAssembly<Mersenne31Field>
```

第二步：

```rust
create_table_driver_into_cs(&mut cs, machine);
```

这一步把通用lookup表注册进`cs`。注意它不是生成独立的`TableDriver`返回，而是直接写进`Circuit`对象。

第三步，也是最重要的一步：

```rust
M::describe_state_transition(&mut cs)
```

这里才是main RISC-V machine定义“一行怎么执行”的核心。它会往`cs`里添加：

```text
变量
普通约束
ROM lookup
decoder lookup
range check
shuffle RAM query
delegation request
state input / state output
```

返回：

```text
initial_state
final_state
```

这两个表示一行状态转移的入口状态和出口状态。对main RISC-V来说，最核心的跨行状态通常是pc相关状态。也就是：

```text
这一行开始时pc是多少；
这一行结束后下一行pc是多少。
```

第四步：

```rust
initial_state.append_into_variables_set(...)
final_state.append_into_variables_set(...)
```

把状态对象里的变量收集出来。后面`state_input`和`state_output`用于相邻行连接。

第五步：

```rust
let (mut output, _) = cs.finalize();
```

把`BasicAssembly`里收集到的所有东西变成`CircuitOutput`。

最后：

```rust
output.state_input = initial_state_vars;
output.state_output = final_state_vars;
```

把初始状态和最终状态变量写回`CircuitOutput`。

因此，`compile_machine`可以总结成：

```text
创建一个空Circuit收集器；
注册机器使用的固定表；
让Machine描述一行RISC-V状态转移；
把这行状态转移产生的变量、约束、lookup、memory query收集成CircuitOutput；
标出跨行状态输入和输出。
```

### 3.11 create_table_driver_into_cs 和 create_table_driver 的区别

你列了两个函数：

```text
create_table_driver
create_table_driver_into_cs
```

它们非常像，但作用位置不同。

`create_table_driver(machine)`返回一个独立`TableDriver`：

```text
给setup/prover使用。
```

`create_table_driver_into_cs(cs, machine)`把表注册进`Circuit`：

```text
给compile_machine使用。
```

两者都做类似的事情：

```text
1. 读取Machine定义的used_tables。
2. materialize这些表。
3. 加入machine自定义extra_tables。
4. materialize And、ZeroEntry、QuickDecode表、U16GetSignAndHighByte、RangeCheckSmall。
5. 创建decoder table并加入OpTypeBitmask。
6. 如果使用ROM bytecode，加入RomAddressSpaceSeparator表。
```

但是program-specific的`RomRead`和`SpecialCSRProperties`不在这里加入。

原因是：

```text
create_table_driver / create_table_driver_into_cs:
  负责机器通用表。

get_machine / get_table_driver:
  再把当前bytecode生成的RomRead表、当前delegation白名单生成的CSR表补进去。
```

这点很重要。

通用表和当前程序无关，例如range表、decoder表。ROM表和当前程序有关，所以必须拿到bytecode后才能生成。

### 3.12 CircuitOutput：Machine写完约束后的“毛坯房”

`compile_machine`最终返回`CircuitOutput`。这个对象还不是最终布局好的电路，而是一个中间产物。

可以把它理解成：

```text
Machine已经把规则都说完了，
但是变量还没有真正排到trace列里。
```

`CircuitOutput`包含很多字段：

```rust
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
```

逐类解释。

`state_input`和`state_output`：

```text
跨行状态。
比如当前行开始的pc和当前行结束的pc。
后面compiler会把上一行state_output和下一行state_input连接起来。
```

`table_driver`：

```text
这套Circuit里用到的表。
compile_machine期间会注册通用表；default_compile_machine还会补RomRead和SpecialCSRProperties。
```

`num_of_variables`：

```text
Machine描述一行状态转移时创建了多少变量。
这些变量现在只有编号，还没有被放到具体列。
```

`constraints`：

```text
普通多项式约束。
比如某个boolean变量b要满足b*(b-1)=0。
ADD行可能会产生rd - rs1 - rs2 = 0一类约束。
```

`lookups`：

```text
普通lookup查询。
例如ROM read、decoder、range check等。
```

`shuffle_ram_queries`：

```text
register/RAM统一memory argument的查询。
```

Airbender把register和RAM放到统一memory argument里。`ShuffleRamQueryType`里有两种形式：

```text
RegisterOnly:
  只访问寄存器。

RegisterOrRam:
  用is_register区分访问寄存器还是RAM。
```

这能解释为什么main RISC-V circuit不把32个register都显式放在状态里。寄存器读写会变成memory query。

`delegated_computation_requests`：

```text
main circuit向delegation circuit发出的请求。
例如当前行触发了BLAKE2 delegation。
```

`range_check_expressions`：

```text
需要做range check的表达式。
```

`boolean_vars`：

```text
所有要求为0/1的变量。
```

`substitutions`：

```text
Placeholder到Variable的映射。
后面witness generation和代码生成会用。
```

所以`CircuitOutput`是一个很关键的中间状态：

```text
RISC-V语义已经被写成变量、约束、lookup、memory query；
但它们还没有被排成最终trace列。
```

### 3.13 OneRowCompiler：把毛坯房变成可证明的列布局

`default_compile_machine`拿到`CircuitOutput`后，调用：

```rust
compiler.compile_output_for_chunked_memory_argument(cs_output, trace_len_log2)
```

源码中`compile_output_for_chunked_memory_argument`只是进入：

```rust
Self::compile_inner::<false>(self, circuit_output, trace_len_log2)
```

这里`false`表示不是delegation circuit，而是main circuit路径。

`compile_inner`开头有一段注释，直接说明了它的职责：

```text
- place variables in particular grid places
- select whether they go into witness subtree or memory subtree
- normalize constraints to address particular columns instead of variable indexes
- try to apply some heuristics
```

翻译成更容易懂的话：

```text
1. 给每个Variable分配一个真实列位置。
2. 决定它属于witness区域、memory区域，还是setup区域。
3. 把Constraint里的Variable编号换成ColumnAddress。
4. 做一些布局优化。
```

这一步非常重要。因为`CircuitOutput`里的约束长这样：

```text
Variable(12) + Variable(34) - Variable(56) = 0
```

但是后端prover评价trace时，需要知道：

```text
witness第10列 + memory第3列 - witness第15列 = 0
```

所以OneRowCompiler做的是“变量编号到列地址”的翻译。

### 3.14 compile_inner对main circuit的基本检查

在`compile_inner`里，它先把`CircuitOutput`拆开：

```rust
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
```

然后区分delegation circuit和main circuit。

对main circuit，也就是`FOR_DELEGATION = false`，它检查：

```rust
assert_eq!(shuffle_ram_queries.len(), 3);
assert!(linked_variables.is_empty());
assert!(degegated_request_to_process.is_none());
assert!(batched_memory_accesses.is_empty());
assert!(register_and_indirect_memory_accesses.is_empty());
```

这里能看出main RISC-V每个cycle固定有3个shuffle RAM query。

这和RISC-V指令很贴近。绝大多数指令可以抽象成：

```text
读取rs1
读取rs2或读取/写入RAM
写入rd或写入RAM
```

例如ADD：

```text
query 1: read x1
query 2: read x2
query 3: write x5
```

LW：

```text
query 1: read x10
query 2: read RAM[address]
query 3: write x6
```

SW：

```text
query 1: read x10
query 2: read x5
query 3: write RAM[address]
```

不同opcode对这3个query的解释不同，但main circuit布局固定为3个shuffle RAM query。这是Airbender main machine设计中非常关键的一点。

### 3.15 compile_inner创建SetupLayout

接下来：

```rust
let trace_len = 1usize << trace_len_log2;
let total_tables_size = table_driver.total_tables_len;
let lookup_table_encoding_capacity = trace_len - 1;
```

`trace_len`就是：

```text
H = 2^22
```

`total_tables_size`是所有lookup表拼起来后的总行数。`lookup_table_encoding_capacity = trace_len - 1`，说明每组setup lookup列最多编码`H - 1`行表内容。

为什么是`H - 1`？

因为setup trace最后一行不用于普通表内容。后面`SetupPrecomputations`里也有注释：last row不用，并且要调整成`c0 == 0`。

然后：

```rust
let setup_layout =
    SetupLayout::layout_for_lookup_size(total_tables_size, trace_len, need_timestamps);
```

这一步根据表总长度和trace长度决定setup列怎么排。

可以理解成：

```text
TableDriver告诉我一共有多少表行。
trace_len告诉我每列组最多能装多少行。
SetupLayout决定需要多少组generic lookup setup columns。
```

假设所有表一共需要装`3H`行，而每组能装`H-1`行，那就需要大约3组generic lookup setup columns。

### 3.16 CompiledCircuitArtifact：最终编译结果

`OneRowCompiler`最终返回：

```text
CompiledCircuitArtifact
```

它的字段包括：

```rust
pub struct CompiledCircuitArtifact<F: PrimeField> {
    pub witness_layout: WitnessSubtree<F>,
    pub memory_layout: MemorySubtree,
    pub setup_layout: SetupLayout,
    pub stage_2_layout: LookupAndMemoryArgumentLayout,
    pub degree_2_constraints: Vec<CompiledDegree2Constraint<F>>,
    pub degree_1_constraints: Vec<CompiledDegree1Constraint<F>>,
    pub state_linkage_constraints: Vec<(ColumnAddress, ColumnAddress)>,
    pub public_inputs: Vec<(BoundaryConstraintLocation, ColumnAddress)>,
    pub variable_mapping: BTreeMap<Variable, ColumnAddress>,
    pub scratch_space_size_for_witness_gen: usize,
    pub lazy_init_address_aux_vars: Option<ShuffleRamAuxComparisonSet>,
    pub memory_queries_timestamp_comparison_aux_vars: Vec<ColumnAddress>,
    pub batched_memory_access_timestamp_comparison_aux_vars: BatchedRamTimestampComparisonAuxVars,
    pub register_and_indirect_access_timestamp_comparison_aux_vars:
        RegisterAndIndirectAccessTimestampComparisonAuxVars,
    pub trace_len: usize,
    pub table_offsets: Vec<u32>,
    pub total_tables_size: usize,
}
```

这就是最终“规则书”。

字段可以按用途分成几组。

布局类：

```text
witness_layout:
  witness列怎么排。

memory_layout:
  shuffle RAM / memory argument相关列怎么排。

setup_layout:
  fixed setup列怎么排。

stage_2_layout:
  lookup和memory argument第二阶段怎么排。
```

约束类：

```text
degree_1_constraints:
  一次约束。

degree_2_constraints:
  二次约束。
```

状态和公开输入：

```text
state_linkage_constraints:
  相邻行状态连接，例如上一行输出pc等于下一行输入pc。

public_inputs:
  哪些边界位置是公开输入。
```

变量映射和witness辅助：

```text
variable_mapping:
  原始Variable -> ColumnAddress。

scratch_space_size_for_witness_gen:
  witness生成时需要的临时空间。

memory_queries_timestamp_comparison_aux_vars:
  memory timestamp比较相关辅助列。
```

表信息：

```text
trace_len:
  H = 2^22。

table_offsets:
  每张lookup表在拼接总表里的偏移。

total_tables_size:
  所有lookup表总行数。
```

这一步之后，Airbender后端已经知道如何评价这套main RISC-V电路。

但是注意：它仍然没有当前程序运行出来的witness值。它只是知道如果有witness trace，应该怎么检查。

### 3.17 get_table_driver：生成独立的表内容容器

现在回到另一条线：

```rust
let table_driver = risc_v_cycles::get_table_driver(bytecode, delegation_csrs);
```

`get_table_driver`会调用`get_table_driver_for_rom_bound`。

它也会检查bytecode长度，然后：

```rust
let machine = FullIsaMachineWithDelegationNoExceptionHandling;
let mut table_driver = create_table_driver(machine);
let rom_table = create_table_for_rom_image(...);
table_driver.add_table_with_content(TableType::RomRead, LookupWrapper::Dimensional3(rom_table));
let csr_table = create_csr_table_for_delegation(...);
table_driver.add_table_with_content(
    TableType::SpecialCSRProperties,
    LookupWrapper::Dimensional3(csr_table),
);
```

这和`get_machine`看起来重复，但目的不同。

`get_machine`要生成`CompiledCircuitArtifact`。它关心“约束怎么布局”。

`get_table_driver`要生成独立的`TableDriver`。它关心“表内容是什么”。

后面`SetupPrecomputations::from_tables_and_trace_len`需要真实表内容，所以它接收的是这个`table_driver`。

### 3.18 create_table_driver：机器通用表

`create_table_driver(machine)`会先问machine：

```rust
let used_tables = M::define_used_tables();
```

这表示这台machine声明自己会用哪些表。

然后它逐个materialize：

```rust
for table in used_tables {
    table_driver.materialize_table(table);
}
```

接着加入extra tables：

```rust
let extra_tables = machine.define_additional_tables();
for (table, content) in extra_tables {
    table_driver.add_table_with_content(table, content);
}
```

然后它强制materialize几张通用表：

```rust
TableType::And
TableType::ZeroEntry
TableType::QuickDecodeDecompositionCheck4x4x4
TableType::QuickDecodeDecompositionCheck7x3x6
TableType::U16GetSignAndHighByte
TableType::RangeCheckSmall
```

这些表服务decoder、bit operations、range check等常见约束。

再创建decoder表：

```rust
let decoder_table = M::create_decoder_table(TableType::OpTypeBitmask.to_table_id());
table_driver.add_table_with_content(
    TableType::OpTypeBitmask,
    LookupWrapper::Dimensional3(decoder_table),
);
```

decoder表用于把instruction的某些bit分解成opcode family和具体variant flags。后面读opcode decoding时会重点回来。

如果machine使用ROM bytecode，还加入：

```rust
TableType::RomAddressSpaceSeparator
```

这张表和ROM地址空间拆分有关。

所以`create_table_driver`负责的是“机器通用表”。当前程序的`RomRead`表和当前CSR白名单的`SpecialCSRProperties`表是在`get_table_driver_for_rom_bound`里额外加入的。

### 3.19 SetupPrecomputations：把固定表写成setup trace

现在进入最后一步：

```rust
SetupPrecomputations::from_tables_and_trace_len(
    &table_driver,
    DOMAIN_SIZE,
    &machine.setup_layout,
    &twiddles,
    &lde_precomputations,
    LDE_FACTOR,
    TREE_CAP_SIZE,
    worker,
)
```

`SetupPrecomputations`本身很简单：

```rust
pub struct SetupPrecomputations<const N: usize, A: GoodAllocator, T: MerkleTreeConstructor> {
    pub ldes: Vec<CosetBoundTracePart<N, A>>,
    pub trees: Vec<T>,
}
```

也就是说它保存两类东西：

```text
ldes:
  setup trace在LDE domain上的评价。

trees:
  对这些LDE trace构造的Merkle trees。
```

`from_tables_and_trace_len`主流程是：

```text
1. 检查trace_len是2的幂。
2. 根据trace_len和LDE_FACTOR算Merkle cap大小。
3. 调用get_main_domain_trace生成setup trace。
4. 调整最后一行，使c0相关值为0。
5. 对setup trace做LDE。
6. 对每个LDE coset构造Merkle tree。
7. 返回SetupPrecomputations { ldes, trees }。
```

### 3.20 get_main_domain_trace：setup trace里到底写了什么

`get_main_domain_trace`创建一张全零的row-major trace：

```rust
RowMajorTrace::new_zeroed_for_size(trace_len, setup_layout.total_width, A::default())
```

也就是：

```text
trace_len行
setup_layout.total_width列
```

然后它准备几类固定表内容。

第一类是generic lookup tables：

```rust
let all_generic_tables = table_driver.dump_tables();
```

`dump_tables()`会把所有lookup表拼在一起。每一行是宽度4：

```text
[col0, col1, col2, table_id]
```

这就是为什么ROM表原本是3列，进入generic setup后会多一列`table_id`。否则不同表的相同行值会混淆。

第二类是16-bit range table：

```rust
range_check_16_table = 0..2^16
```

第三类是timestamp range table：

```rust
timestamp_range_check_table = 0..2^TIMESTAMP_COLUMNS_NUM_BITS
```

然后它按行填setup trace。

对每个`absolute_row_idx`：

如果还在16-bit range表范围内：

```text
setup trace的range_check_16列 = absolute_row_idx
```

如果还在timestamp range表范围内：

```text
setup trace的timestamp_range_check列 = absolute_row_idx
```

对于generic lookup tables：

```rust
for (tuple_idx, encoding_chunk) in all_generic_tables_ref.iter().enumerate() {
    if absolute_row_idx < encoding_chunk.len() {
        let table_row = encoding_chunk[absolute_row_idx];
        let range = setup_layout.generic_lookup_setup_columns.get_range(tuple_idx);
        trace_view_row[range].copy_from_slice(&table_row);
    }
}
```

意思是：把拼好的lookup表内容分块写进`generic_lookup_setup_columns`。

每一块最多写：

```text
trace_len - 1
```

行，因为最后一行不用。

如果需要timestamp setup columns，还写入：

```text
timestamp_low
timestamp_high
```

这些timestamp列服务shuffle RAM argument。

### 3.21 setup trace和witness trace再次区分

现在可以非常明确地区分：

```text
setup trace:
  ROM表
  CSR表
  decoder表
  range表
  timestamp固定列
  这些都和当前执行输入无关

witness trace:
  当前cycle的pc
  当前instruction decode出来的flags
  当前寄存器读写值
  当前RAM读写值
  当前是否触发delegation
```

例如ADD x5,x1,x2：

setup里有：

```text
pc=0对应的instruction编码
decoder表
range表
```

witness里有：

```text
x1=7
x2=9
x5=16
is_add=1
```

proof要同时绑定二者：

```text
setup tree保证固定表没有被改；
witness tree保证执行轨迹被承诺；
约束检查保证witness和setup表之间关系正确。
```

### 3.22 本章函数链路总图

最后把你列的所有函数串起来：

```text
circuit_defs/risc_v_cycles/src/lib.rs

get_machine(bytecode, delegation_csrs)
  |
  v
get_machine_for_rom_bound(...)
  |
  +-- 检查bytecode长度
  |
  +-- create_table_for_rom_image(...)
  |     -> RomRead表
  |
  +-- create_csr_table_for_delegation(...)
  |     -> SpecialCSRProperties表
  |
  +-- default_compile_machine(...)
        |
        v

cs/src/lib.rs

default_compile_machine(machine, rom_table, csr_table, trace_len_log2)
  |
  +-- compile_machine::<BasicAssembly>(machine)
  |     |
  |     v
  |   CircuitOutput
  |
  +-- 把RomRead表加入CircuitOutput.table_driver
  |
  +-- 把SpecialCSRProperties表加入CircuitOutput.table_driver
  |
  +-- OneRowCompiler::compile_output_for_chunked_memory_argument(...)
        |
        v

cs/src/one_row_compiler/compile_layout.rs

compile_output_for_chunked_memory_argument(circuit_output, trace_len_log2)
  |
  +-- 给Variable分配ColumnAddress
  +-- 生成witness_layout
  +-- 生成memory_layout
  +-- 生成setup_layout
  +-- 编译degree_1 / degree_2 constraints
  +-- 生成lookup和memory argument布局
        |
        v

cs/src/one_row_compiler/mod.rs

CompiledCircuitArtifact
  |
  +-- witness_layout
  +-- memory_layout
  +-- setup_layout
  +-- stage_2_layout
  +-- degree constraints
  +-- public inputs
  +-- variable_mapping
  +-- table_offsets
  +-- trace_len


circuit_defs/risc_v_cycles/src/lib.rs

get_table_driver(bytecode, delegation_csrs)
  |
  +-- create_table_driver(machine)
  |     -> 通用表、decoder表、range表等
  |
  +-- create_table_for_rom_image(...)
  |     -> RomRead表
  |
  +-- create_csr_table_for_delegation(...)
        -> SpecialCSRProperties表


prover/src/prover_stages/mod.rs

SetupPrecomputations::from_tables_and_trace_len(
  table_driver,
  trace_len,
  setup_layout,
  twiddles,
  lde_precomputations,
  ...
)
  |
  +-- get_main_domain_trace(...)
  |     -> 把TableDriver内容写入setup trace
  |
  +-- adjust_to_zero_c0_var_length(...)
  |
  +-- compute_wide_ldes(...)
  |
  +-- construct Merkle trees
        |
        v
SetupPrecomputations { ldes, trees }
```

### 3.23 本章最终理解

这一章的主线不是“Airbender怎么执行程序”，而是“Airbender怎么准备一套能证明程序执行的固定环境”。

最关键的分工是：

```text
get_machine:
  从machine配置、ROM表、CSR表生成CompiledCircuitArtifact。
  它回答：这套电路的规则和布局是什么？

get_table_driver:
  生成所有固定lookup表内容。
  它回答：这套电路要查的表里具体有什么？

compile_machine:
  让Machine写出一行RISC-V状态转移，得到CircuitOutput。
  它回答：一行RISC-V执行会产生哪些变量、约束、lookup、memory query？

OneRowCompiler:
  把CircuitOutput里的变量编号编译成真实列布局。
  它回答：这些变量和约束具体落在哪些trace列？

CompiledCircuitArtifact:
  编译后的规则书。
  它回答：prover/verifier如何按列检查这套电路？

SetupPrecomputations:
  把固定表写入setup trace，做LDE和Merkle tree。
  它回答：固定表如何被承诺，并在proof里绑定？
```

下一章就可以进入`compile_machine`最核心的一行：

```rust
M::describe_state_transition::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(&mut cs)
```

也就是：`FullIsaMachineWithDelegationNoExceptionHandling`到底怎样描述一行RISC-V执行。

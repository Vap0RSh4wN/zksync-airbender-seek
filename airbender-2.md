我先说明抓取情况：我抓到了真实源码。你给的仓库 URL 里 `mainn` 应该是笔误，我按 `main` 分支读取。`tools/cli/src/setup.rs` 当前只有 92 行，GitHub 返回的 blob sha 是 `5c297e10c58345f2931d15c7414dda2d1a0bfb51`。这个文件本身不是 Clap 命令入口，而是 CLI 侧的 setup 缓存工具；真正的命令解析在 `tools/cli/src/main.rs`，里面的 `Commands` 枚举包含 `Prove`、`GenerateVk`、`Run` 等命令。下面第二章会先读 `setup.rs`，同时在遇到关键函数时下钻到真实定义。 

# 第2章 `tools/cli/src/setup.rs`逐行阅读：setup缓存层如何连接main RISC-V、reduced RISC-V和delegation circuits

本章代码位置：

```text
tools/cli/src/setup.rs
```

这一章先解决一个基础问题：CLI在证明或生成VK时，为什么需要“setup cache”？Airbender里，某个RISC-V程序的证明不是直接拿bytecode就开始证明。证明前需要先根据bytecode生成对应的main circuit setup，也就是main RISC-V电路的预计算对象。这个对象里面包括编译后的约束系统、lookup table driver、FFT/LDE预计算、setup commitment相关预计算，以及GPU witness tracer需要的函数指针。

`setup.rs`做的事情可以概括为：

```text
给定 bytecode
  -> 计算 bytecode hash
  -> 如果之前生成过这个 bytecode 对应的 setup，直接复用
  -> 如果没有，调用 setups::get_main_riscv_circuit_setup 或 reduced setup
  -> 再把 setup trace 转成后续方便使用的 setup evaluations
  -> 缓存在 HashMap 里
```

对delegation circuits也是类似逻辑：

```text
第一次需要 delegation setup 时
  -> 生成所有 delegation circuit precomputations
  -> 为每个 delegation circuit 生成 setup evaluations
  -> 缓存起来

后续再需要时
  -> 直接返回 Arc clone
```

这里先给一个全局图：

```text
SetupCache
  |
  +-- main_circuit_setup
  |     key: hash(bytecode)
  |     value:
  |       MainCircuitPrecomputations<IMStandardIsaConfig>
  |       setup evaluations
  |
  +-- reduced_circuit_setup
  |     key: hash(bytecode)
  |     value:
  |       MainCircuitPrecomputations<IWithoutByteAccessIsaConfigWithDelegation>
  |       setup evaluations
  |
  +-- delegations
  |     all delegation circuit precomputations
  |
  +-- delegation_evals
        setup evaluations for delegation circuits
```

注意，这里出现了一个重要设计：main circuit setup依赖bytecode，所以用`hash(bytecode)`作为缓存key；delegation circuit setup不依赖某个具体RISC-V程序的bytecode，所以只缓存一份。

## 2.1 文件开头：imports

源码：

```rust
use prover::{
    fft::GoodAllocator,
    field::Mersenne31Field,
    risc_v_simulator::cycle::{IMStandardIsaConfig, IWithoutByteAccessIsaConfigWithDelegation},
};
use prover_examples::create_circuit_setup;
use setups::{DelegationCircuitPrecomputations, MainCircuitPrecomputations};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::{collections::hash_map::DefaultHasher, sync::Arc};
```

这几行导入已经暴露了本文件的全部职责。`GoodAllocator`来自`prover::fft`，后面作为泛型约束出现；这个文件里不直接做FFT，但它保存的setup对象内部包含FFT/LDE相关预计算，所以缓存结构也要带着allocator泛型。`Mersenne31Field`是Airbender后端常用的基础域类型，后面`Arc<Vec<Mersenne31Field, B>>`表示已经转置好的setup evaluation数组。`IMStandardIsaConfig`表示标准main RISC-V机器配置，`IWithoutByteAccessIsaConfigWithDelegation`表示reduced machine配置。源码导入位置在`setup.rs`第3到第7行。

先解释几个符号，后面全章统一使用：

```text
F = Mersenne31Field
  Airbender这里用的基础域元素类型。

A: GoodAllocator
  通常用于prover侧大块数据、FFT/LDE、setup precomputations等对象的分配器。

B: GoodAllocator
  这里主要用于保存setup evaluations的Vec分配器。
  可以和A相同，也可以不同。

bytecode: Vec<u32>
  RISC-V程序的机器码，按32-bit word存。
```

`create_circuit_setup`从`prover_examples`导入。它不是“生成约束系统”的函数，它只是把`setup.setup.ldes[0].trace`这种row-major setup trace转换成后续需要的evaluation向量。源码里它在`setup.rs`第8行被导入，在后面的`get_or_create_main_circuit`和`get_or_create_reduced_circuit`里调用。

`MainCircuitPrecomputations`和`DelegationCircuitPrecomputations`来自`setups` crate。后面会下钻看它们的结构。先记住：它们是已经构造好的“可用于证明的电路预计算包”，里面不只是constraint system，还有table driver、twiddles、LDE precomputations、setup precomputations、witness tracer函数指针等。它们的定义在`circuit_defs/setups/src/lib.rs`第119到第139行。

`HashMap`、`Hash`、`Hasher`、`DefaultHasher`用于给bytecode计算缓存key。`Arc`用于共享大对象，避免频繁clone实际数据。源码导入在第10到第12行。

## 2.2 `SetupCache`结构体：缓存哪些东西

源码：

```rust
#[derive(Default)]
pub struct SetupCache<A: GoodAllocator, B: GoodAllocator> {
    pub main_circuit_setup: HashMap<
        u64,
        (
            Arc<MainCircuitPrecomputations<IMStandardIsaConfig, A, B>>,
            Arc<Vec<Mersenne31Field, B>>,
        ),
    >,
    pub reduced_circuit_setup: HashMap<
        u64,
        (
            Arc<MainCircuitPrecomputations<IWithoutByteAccessIsaConfigWithDelegation, A, B>>,
            Arc<Vec<Mersenne31Field, B>>,
        ),
    >,
    pub delegations: Arc<Vec<(u32, DelegationCircuitPrecomputations<A, B>)>>,
    pub delegation_evals: Arc<Vec<(u32, Arc<Vec<Mersenne31Field, B>>)>>,
}
```

这一段是全文件的核心。`SetupCache`是一个泛型结构体，带两个allocator参数：

```rust
pub struct SetupCache<A: GoodAllocator, B: GoodAllocator>
```

这里的`A`和`B`都要求实现`GoodAllocator`。从本文件的用法看，`A`进入`MainCircuitPrecomputations`和`DelegationCircuitPrecomputations`，`B`进入`Vec<Mersenne31Field, B>`。也就是说，`A`偏向setup/prover预计算对象内部使用，`B`偏向setup evaluations数组使用。源码里`SetupCache`定义在第14到第32行。

第一个字段：

```rust
pub main_circuit_setup: HashMap<
    u64,
    (
        Arc<MainCircuitPrecomputations<IMStandardIsaConfig, A, B>>,
        Arc<Vec<Mersenne31Field, B>>,
    ),
>,
```

这个字段缓存标准main RISC-V machine的setup。key是`u64`，后面会看到它来自`DefaultHasher`对bytecode的hash。value是一个二元组：

```text
(
  Arc<MainCircuitPrecomputations<IMStandardIsaConfig, A, B>>,
  Arc<Vec<Mersenne31Field, B>>
)
```

第一项是main circuit的完整预计算对象，机器配置是`IMStandardIsaConfig`。第二项是从setup trace转换出来的一维evaluation数组。

这里要把两个对象区分清楚：

```text
MainCircuitPrecomputations
  完整setup包。
  包含compiled circuit、table driver、twiddles、LDE预计算、setup commitment预计算、witness tracer函数。

Vec<Mersenne31Field>
  从setup.setup.ldes[0].trace转置出来的evaluation向量。
  更像某种方便后续消费的setup data flattened/evaluation representation。
```

第二个字段：

```rust
pub reduced_circuit_setup: HashMap<
    u64,
    (
        Arc<MainCircuitPrecomputations<IWithoutByteAccessIsaConfigWithDelegation, A, B>>,
        Arc<Vec<Mersenne31Field, B>>,
    ),
>,
```

这个字段和`main_circuit_setup`形状一样，但机器配置换成了：

```rust
IWithoutByteAccessIsaConfigWithDelegation
```

这是一种reduced RISC-V machine配置。从名字看，它不支持byte access，但支持delegation。官方Circuit Entry Points文档把reduced/minimal machine列为递归层或更小约束系统使用的机器类型；具体支持哪些opcode，后面读machine configuration时再展开。这里先知道：CLI缓存标准main machine和reduced machine两套setup。

第三个字段：

```rust
pub delegations: Arc<Vec<(u32, DelegationCircuitPrecomputations<A, B>)>>,
```

这个字段缓存所有delegation circuits的预计算对象。`u32`是delegation type id，比如BLAKE2、BigInt各自有一个id。后面的`DelegationCircuitPrecomputations`是对应delegation circuit的完整setup包。

第四个字段：

```rust
pub delegation_evals: Arc<Vec<(u32, Arc<Vec<Mersenne31Field, B>>)>>,
```

它缓存每个delegation circuit对应的setup evaluations。结构和`delegations`平行：

```text
delegations:
  [(delegation_type, delegation_setup)]

delegation_evals:
  [(delegation_type, setup_evaluation_vector)]
```

为什么main/reduced用`HashMap<u64,...>`，delegation不用？

原因在于main/reduced setup依赖bytecode。不同程序ROM不同，main circuit setup也不同，所以要按bytecode hash缓存。delegation circuits是固定专用电路，比如BLAKE2 compression、BigInt control，它们不随某个RISC-V程序bytecode变化，所以只缓存一组。

## 2.3 下钻：`MainCircuitPrecomputations`到底包含什么

既然`SetupCache`最重要的字段是`MainCircuitPrecomputations`，这里先跳进去看定义。

源码：

```rust
pub struct MainCircuitPrecomputations<C: MachineConfig, A: GoodAllocator, B: GoodAllocator = Global>
{
    pub compiled_circuit: cs::one_row_compiler::CompiledCircuitArtifact<Mersenne31Field>,
    pub table_driver: TableDriver<Mersenne31Field>,
    pub twiddles: Twiddles<Mersenne31Complex, A>,
    pub lde_precomputations: LdePrecomputations<A>,
    pub setup: SetupPrecomputations<DEFAULT_TRACE_PADDING_MULTIPLE, A, DefaultTreeConstructor>,
    pub witness_eval_fn_for_gpu_tracer: fn(&mut SimpleWitnessProxy<'_, MainRiscVOracle<'_, C, B>>),
}
```

定义位置在`circuit_defs/setups/src/lib.rs`第119到第127行。

逐个字段解释。

`compiled_circuit`是编译后的约束系统artifact。它来自`cs::one_row_compiler::CompiledCircuitArtifact<Mersenne31Field>`。名字里的`one_row_compiler`说明Airbender会用一套“一行RISC-V machine约束描述”编译成整张trace的约束布局。我们暂时不展开compiler，后面读`get_main_riscv_circuit_setup`和machine configuration时会进入。

`table_driver`是lookup tables的驱动对象。Airbender大量使用ROM lookup、decoder lookup、range table、CSR/delegation table等。`TableDriver<Mersenne31Field>`负责把这些表组织起来，供setup和witness/prover使用。

`twiddles`是FFT相关预计算。这里类型是：

```rust
Twiddles<Mersenne31Complex, A>
```

说明它不只使用基础域`Mersenne31Field`，还会用到扩展/复合域形式`Mersenne31Complex`来做FFT/LDE。

`lde_precomputations`是low-degree extension相关预计算。后端证明要对trace做LDE，setup阶段会提前准备一些和domain、coset、twiddles有关的数据。

`setup`是`SetupPrecomputations`，包含由fixed tables和setup trace生成的预计算。`setup.rs`后面会访问：

```rust
setup.setup.ldes[0].trace
```

这说明`MainCircuitPrecomputations.setup`内部有LDE数据，`ldes[0].trace`保存某个setup trace的row-major表示。

最后的`witness_eval_fn_for_gpu_tracer`是函数指针。它的类型：

```rust
fn(&mut SimpleWitnessProxy<'_, MainRiscVOracle<'_, C, B>>)
```

含义是：给定一个`MainRiscVOracle`，通过`SimpleWitnessProxy`把witness写进trace。这里的`C`就是machine configuration，比如标准main machine用`IMStandardIsaConfig`。

先用一张表总结：

| 字段                               | 作用                      | 现在需要掌握的理解                        |
| -------------------------------- | ----------------------- | -------------------------------- |
| `compiled_circuit`               | 编译后的约束系统                | 后续prover/verifier都围绕它解释trace列和约束 |
| `table_driver`                   | lookup table管理          | ROM、range、decoder、CSR等表从这里来      |
| `twiddles`                       | FFT预计算                  | 后端LDE/FFT用                       |
| `lde_precomputations`            | LDE预计算                  | 后端扩展trace用                       |
| `setup`                          | setup trace和commit相关预计算 | `setup.rs`会从这里取`ldes[0].trace`   |
| `witness_eval_fn_for_gpu_tracer` | witness生成函数             | 后续prover用oracle填witness          |

## 2.4 `impl SetupCache`：缓存对象的方法集合

源码：

```rust
impl<A: GoodAllocator, B: GoodAllocator> SetupCache<A, B> {
```

这表示下面几个函数都属于`SetupCache<A,B>`。同样，`A`和`B`必须是`GoodAllocator`。源码位置是第34行。

这个`impl`里有三个方法：

```text
get_or_create_main_circuit
get_or_create_reduced_circuit
get_or_create_delegations
```

它们的共同模式是：

```text
如果缓存里有，返回缓存引用或clone出来的Arc。
如果没有，现场构造setup，再写入缓存。
```

现在逐个读。

## 2.5 `get_or_create_main_circuit`：标准main RISC-V setup缓存

源码：

```rust
pub fn get_or_create_main_circuit(
    &mut self,
    bytecode: &Vec<u32>,
) -> &(
    Arc<MainCircuitPrecomputations<IMStandardIsaConfig, A, B>>,
    Arc<Vec<Mersenne31Field, B>>,
) {
```

这个函数接收：

```rust
&mut self
bytecode: &Vec<u32>
```

它需要`&mut self`，因为如果缓存里没有对应setup，它要往`HashMap`里插入新项。`bytecode`是RISC-V程序的机器码。这里使用`&Vec<u32>`而不是`&[u32]`，是当前源码写法；后面调用`setups::get_main_riscv_circuit_setup(&bytecode, &worker)`时，Rust会自动把`&Vec<u32>`转成`&[u32]`或按函数需要借用。

返回值是引用：

```rust
&(
    Arc<MainCircuitPrecomputations<IMStandardIsaConfig, A, B>>,
    Arc<Vec<Mersenne31Field, B>>,
)
```

也就是说，函数不会复制setup数据，只返回缓存中那一项的引用。里面的两个实际大对象都包在`Arc`里，方便外部再clone引用计数。源码第35到第41行。

接下来：

```rust
let mut hasher = DefaultHasher::new();
bytecode.hash(&mut hasher);
let hash = hasher.finish();
```

这三行把整个bytecode hash成一个`u64`。源码第42到第44行。

这里不是密码学承诺，只是进程内缓存key。`DefaultHasher`通常用于HashMap这类普通哈希，不应该把它理解成安全绑定bytecode的proof statement。证明系统里真正绑定程序ROM的是后续circuit setup、table driver、commitment、verification key这些对象；这里的hash只是为了避免同一个CLI运行过程中重复生成昂贵setup。

用例子说明：

```text
bytecode A = [0x002082b3, 0x00552023, ...]
hash(bytecode A) = 12345

第一次请求：
  main_circuit_setup 没有 key=12345
  生成setup并插入

第二次请求：
  main_circuit_setup 已有 key=12345
  直接返回缓存项
```

然后进入核心：

```rust
self.main_circuit_setup.entry(hash).or_insert_with(|| {
    let worker = worker::Worker::new_with_num_threads(8);
    let setup = setups::get_main_riscv_circuit_setup(&bytecode, &worker);
    let eval = create_circuit_setup(&setup.setup.ldes[0].trace);
    (Arc::new(setup), Arc::new(eval))
})
```

源码第46到第51行。

这一段要分开看。

第一行：

```rust
self.main_circuit_setup.entry(hash).or_insert_with(|| { ... })
```

这是HashMap的lazy insert模式。如果key存在，直接返回已有value。如果key不存在，执行闭包生成value并插入。这里的闭包不会在缓存命中时执行，所以昂贵的setup只在第一次发生。

闭包第一行：

```rust
let worker = worker::Worker::new_with_num_threads(8);
```

这里创建一个8线程Worker。`Worker`的定义在`worker/src/lib.rs`里，它内部保存`rayon::ThreadPool`和`num_cores`。`new_with_num_threads`会用`ThreadPoolBuilder`创建指定线程数的线程池，并设置固定栈大小。源码定义在`worker/src/lib.rs`第10到第16行，以及第164到第186行。 

为什么setup需要worker？因为生成setup不是轻量操作。它可能要构造lookup tables、编译circuit、做FFT/LDE预计算、生成setup precomputations。这些步骤里会有大量可并行任务，所以统一传入`Worker`。

闭包第二行：

```rust
let setup = setups::get_main_riscv_circuit_setup(&bytecode, &worker);
```

这里进入本章遇到的第一个主函数。我们先跳进去看源码。

## 2.6 下钻：`get_main_riscv_circuit_setup`

源码位置：

```text
circuit_defs/setups/src/circuits/main_riscv/mod.rs
```

完整函数只有36行左右：

```rust
pub fn get_main_riscv_circuit_setup<A: GoodAllocator, B: GoodAllocator>(
    bytecode: &[u32],
    worker: &Worker,
) -> MainCircuitPrecomputations<IMStandardIsaConfig, A, B> {
    let delegation_csrs = IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS;
    let machine: cs::one_row_compiler::CompiledCircuitArtifact<Mersenne31Field> =
        ::risc_v_cycles::get_machine(bytecode, delegation_csrs);
    let table_driver = ::risc_v_cycles::get_table_driver(bytecode, delegation_csrs);

    let twiddles: Twiddles<_, A> = Twiddles::new(::risc_v_cycles::DOMAIN_SIZE, &worker);
    let lde_precomputations = LdePrecomputations::new(
        ::risc_v_cycles::DOMAIN_SIZE,
        ::risc_v_cycles::LDE_FACTOR,
        ::risc_v_cycles::LDE_SOURCE_COSETS,
        &worker,
    );
    let setup =
        SetupPrecomputations::<DEFAULT_TRACE_PADDING_MULTIPLE, A, DefaultTreeConstructor>::from_tables_and_trace_len(
            &table_driver,
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
        witness_eval_fn_for_gpu_tracer: ::risc_v_cycles::witness_eval_fn_for_gpu_tracer,
    }
}
```

这段源码在`main_riscv/mod.rs`第5到第40行。

先看函数签名：

```rust
pub fn get_main_riscv_circuit_setup<A: GoodAllocator, B: GoodAllocator>(
    bytecode: &[u32],
    worker: &Worker,
) -> MainCircuitPrecomputations<IMStandardIsaConfig, A, B>
```

它接收两个输入：

```text
bytecode: &[u32]
  当前要证明的RISC-V程序ROM。

worker: &Worker
  并行执行工具。
```

它返回：

```text
MainCircuitPrecomputations<IMStandardIsaConfig, A, B>
```

这里的机器配置固定为`IMStandardIsaConfig`。这就是标准main RISC-V setup。后面读reduced版本时，返回类型会换成不同的machine config。

第一行：

```rust
let delegation_csrs = IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS;
```

这里从机器配置里取出允许的delegation CSR集合。它告诉main RISC-V circuit：哪些CSR值表示合法delegation调用。比如BLAKE2和BigInt会对应不同delegation type id。这个变量随后传给`get_machine`和`get_table_driver`。

这一步很重要，因为main circuit不是单纯RV32I/M。标准main machine允许delegation，所以它编译电路时需要知道允许哪些delegation CSR；lookup table也需要包含这些CSR相关项。

第二段：

```rust
let machine: cs::one_row_compiler::CompiledCircuitArtifact<Mersenne31Field> =
    ::risc_v_cycles::get_machine(bytecode, delegation_csrs);
let table_driver = ::risc_v_cycles::get_table_driver(bytecode, delegation_csrs);
```

这里生成两个对象。

`get_machine(bytecode, delegation_csrs)`编译main RISC-V machine，得到`CompiledCircuitArtifact<Mersenne31Field>`。它应该包含约束布局、trace长度、setup layout、quotient相关信息等。这里先不深入`risc_v_cycles::get_machine`，下一章会作为重点逐行阅读。

`get_table_driver(bytecode, delegation_csrs)`生成lookup table driver。因为ROM table依赖bytecode，CSR/delegation table依赖delegation CSR whitelist，所以这里同样需要传入`bytecode`和`delegation_csrs`。

从教学角度看，这两行可以写成：

```text
bytecode + allowed delegation CSRs
  |
  +-- get_machine
  |     生成编译后的main RISC-V约束系统
  |
  +-- get_table_driver
        生成当前程序和机器配置对应的lookup tables
```

第三段：

```rust
let twiddles: Twiddles<_, A> = Twiddles::new(::risc_v_cycles::DOMAIN_SIZE, &worker);
```

这一步为大小为`DOMAIN_SIZE`的domain生成FFT twiddles。`DOMAIN_SIZE`来自`risc_v_cycles`这个具体main circuit crate。它表示main RISC-V电路的trace domain大小。setup阶段提前生成twiddles，后续LDE/proving会复用。

第四段：

```rust
let lde_precomputations = LdePrecomputations::new(
    ::risc_v_cycles::DOMAIN_SIZE,
    ::risc_v_cycles::LDE_FACTOR,
    ::risc_v_cycles::LDE_SOURCE_COSETS,
    &worker,
);
```

这一步生成LDE相关预计算。输入包括：

```text
DOMAIN_SIZE
  原始trace domain大小。

LDE_FACTOR
  low-degree extension放大倍数。

LDE_SOURCE_COSETS
  LDE使用的source cosets配置。

worker
  并行工具。
```

这属于后端预计算，但setup必须提前做。我们现在不深入FFT/FRI，只需要知道：main circuit setup不仅保存约束系统，还保存后端打开/证明trace所需的domain预计算。

第五段：

```rust
let setup =
    SetupPrecomputations::<DEFAULT_TRACE_PADDING_MULTIPLE, A, DefaultTreeConstructor>::from_tables_and_trace_len(
        &table_driver,
        ::risc_v_cycles::DOMAIN_SIZE,
        &machine.setup_layout,
        &twiddles,
        &lde_precomputations,
        ::risc_v_cycles::LDE_FACTOR,
        ::risc_v_cycles::TREE_CAP_SIZE,
        &worker,
    );
```

这一段是setup预计算的核心。它把lookup tables、trace长度、machine setup layout、twiddles、LDE预计算、LDE factor、Merkle tree cap size交给`SetupPrecomputations::from_tables_and_trace_len`。

这里有一个重要概念：setup trace。

Airbender里并不是所有列都是prover运行程序后才填的。很多列是固定的，比如ROM table、decoder table、range table、其他固定lookup tables。它们属于setup阶段生成的固定trace或fixed tables。`SetupPrecomputations::from_tables_and_trace_len`就是把这些固定表和setup layout组织成后端可承诺、可打开、可复用的预计算对象。

参数里的：

```rust
&machine.setup_layout
```

说明`get_machine`编译出的machine artifact里包含setup layout。也就是机器约束系统本身知道哪些列属于setup部分、这些列如何布局。

最后返回：

```rust
MainCircuitPrecomputations {
    compiled_circuit: machine,
    table_driver,
    twiddles,
    lde_precomputations,
    setup,
    witness_eval_fn_for_gpu_tracer: ::risc_v_cycles::witness_eval_fn_for_gpu_tracer,
}
```

这一步把所有对象打包。注意最后一个字段：

```rust
witness_eval_fn_for_gpu_tracer: ::risc_v_cycles::witness_eval_fn_for_gpu_tracer
```

这不是立刻生成witness，而是保存一个函数指针。后续证明时，给它一个oracle，它会把RISC-V执行轨迹写成witness trace。我们后面读witness章节时会用到。

回到`setup.rs`，`get_main_riscv_circuit_setup`返回的`setup`就是这一整个`MainCircuitPrecomputations`对象。

## 2.7 下钻：`create_circuit_setup`

回到`setup.rs`里的这行：

```rust
let eval = create_circuit_setup(&setup.setup.ldes[0].trace);
```

这行调用`prover_examples::create_circuit_setup`。源码如下：

```rust
pub fn create_circuit_setup<A: GoodAllocator, B: GoodAllocator, const N: usize>(
    setup_row_major: &RowMajorTrace<Mersenne31Field, N, A>,
) -> Vec<Mersenne31Field, B> {
    #[cfg(feature = "gpu")]
    gpu::initialize_host_allocator_if_needed();

    let mut setup_evaluations =
        Vec::with_capacity_in(setup_row_major.as_slice().len(), B::default());
    unsafe { setup_evaluations.set_len(setup_row_major.as_slice().len()) };
    transpose::transpose(
        setup_row_major.as_slice(),
        &mut setup_evaluations,
        setup_row_major.padded_width,
        setup_row_major.len(),
    );
    setup_evaluations.truncate(setup_row_major.len() * setup_row_major.width());
    setup_evaluations
}
```

这段在`circuit_defs/prover_examples/src/lib.rs`第45到第62行。

这个函数做的事情很具体：把一个row-major trace转置成另一种布局，然后截断padding列。

输入：

```rust
setup_row_major: &RowMajorTrace<Mersenne31Field, N, A>
```

名字说明它是row-major布局。row-major就是按行连续存储：

```text
row0_col0, row0_col1, row0_col2, ...
row1_col0, row1_col1, row1_col2, ...
...
```

但很多证明阶段更喜欢column-major或按列evaluation布局，因为每一列是一条多项式。`transpose::transpose`把原来的row-major矩阵转置成按列排列的evaluation数组。

代码逐行看。

```rust
#[cfg(feature = "gpu")]
gpu::initialize_host_allocator_if_needed();
```

如果启用GPU feature，先初始化host allocator。这是工程细节，保证GPU相关内存分配器可用。

```rust
let mut setup_evaluations =
    Vec::with_capacity_in(setup_row_major.as_slice().len(), B::default());
```

创建一个使用allocator `B`的Vec，容量等于row-major底层slice长度。

```rust
unsafe { setup_evaluations.set_len(setup_row_major.as_slice().len()) };
```

这里直接设置长度，没有初始化元素。这是为了给后面的transpose写入目标buffer。因为马上会被`transpose`完整写满，所以用unsafe跳过初始化成本。

```rust
transpose::transpose(
    setup_row_major.as_slice(),
    &mut setup_evaluations,
    setup_row_major.padded_width,
    setup_row_major.len(),
);
```

这里执行转置。参数可以理解为：

```text
source = setup_row_major.as_slice()
target = setup_evaluations
source width = setup_row_major.padded_width
source height = setup_row_major.len()
```

为什么用`padded_width`而不是`width()`？因为底层row-major trace可能每行为了对齐/GPU布局带有padding列。转置时必须按真实底层stride读，否则行边界会错。

最后：

```rust
setup_evaluations.truncate(setup_row_major.len() * setup_row_major.width());
```

转置完成后，去掉padding列带来的多余位置，只保留：

```text
真实行数 * 真实列数
```

然后返回`setup_evaluations`。

用一个小例子理解。假设setup trace真实宽度是3，但为了对齐，padded_width是4：

```text
row-major with padded width = 4

row0: [a00, a01, a02, pad]
row1: [a10, a11, a12, pad]
```

底层slice是：

```text
[a00, a01, a02, pad, a10, a11, a12, pad]
```

转置后按列排列，大概得到：

```text
[a00, a10, a01, a11, a02, a12, pad, pad]
```

truncate到`len * width = 2 * 3 = 6`后：

```text
[a00, a10, a01, a11, a02, a12]
```

这就是三列各自的evaluation拼起来：

```text
col0: [a00, a10]
col1: [a01, a11]
col2: [a02, a12]
```

所以`create_circuit_setup`这个名字有一点容易误解。它没有重新创建circuit setup，也没有编译约束。它只是把已经存在的setup trace转换成更方便消费的一维evaluation布局。

## 2.8 回到`get_or_create_main_circuit`：返回缓存项

完整闭包是：

```rust
self.main_circuit_setup.entry(hash).or_insert_with(|| {
    let worker = worker::Worker::new_with_num_threads(8);
    let setup = setups::get_main_riscv_circuit_setup(&bytecode, &worker);
    let eval = create_circuit_setup(&setup.setup.ldes[0].trace);
    (Arc::new(setup), Arc::new(eval))
})
```

最后一行：

```rust
(Arc::new(setup), Arc::new(eval))
```

把两个大对象都放进`Arc`。返回值被插入HashMap。之后函数返回这一项的引用。

把整个函数串起来：

```text
get_or_create_main_circuit(bytecode)
  |
  +-- hash(bytecode)
  |
  +-- lookup self.main_circuit_setup[hash]
        |
        +-- if exists:
        |     return cached setup
        |
        +-- if missing:
              worker = Worker(8 threads)
              setup = get_main_riscv_circuit_setup(bytecode, worker)
              eval = create_circuit_setup(setup.setup.ldes[0].trace)
              cache (Arc(setup), Arc(eval))
              return reference
```

这里需要注意一个安全边界：这个hash只是缓存key。它不参与证明安全性。证明时真正要绑定bytecode，需要靠ROM/table driver/compiled circuit/setup commitment等机制。我们后面读`get_machine(bytecode, delegation_csrs)`和`get_table_driver(bytecode, delegation_csrs)`时再具体说明bytecode如何进入ROM table和verification key。

## 2.9 `get_or_create_reduced_circuit`：reduced machine版本

源码：

```rust
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
        // Compute the setup here
        let setup = setups::get_reduced_riscv_circuit_setup(&bytecode, &worker);
        let eval = create_circuit_setup(&setup.setup.ldes[0].trace);
        (Arc::new(setup), Arc::new(eval))
    })
}
```

源码第53到第71行。

这个函数和`get_or_create_main_circuit`完全平行，只是机器配置换成了：

```rust
IWithoutByteAccessIsaConfigWithDelegation
```

并且调用：

```rust
setups::get_reduced_riscv_circuit_setup(&bytecode, &worker)
```

它的流程是：

```text
get_or_create_reduced_circuit(bytecode)
  |
  +-- hash(bytecode)
  |
  +-- lookup self.reduced_circuit_setup[hash]
        |
        +-- missing:
              setup = get_reduced_riscv_circuit_setup(bytecode, worker)
              eval = create_circuit_setup(setup.setup.ldes[0].trace)
              cache
```

为什么要有reduced machine？官方Circuit Entry Points文档里说，main RISC-V machine有多种配置，full ISA用于base proving，reduced/minimal variants用于recursion layers来减小证明成本。`IWithoutByteAccessIsaConfigWithDelegation`这类机器就是较小的配置之一。([GitHub][1])

这里先不用展开reduced machine。后面主线会先读标准main RISC-V，即`IMStandardIsaConfig`和`get_main_riscv_circuit_setup`。Reduced machine等理解完主线再补。

## 2.10 `get_or_create_delegations`：delegation circuits缓存

源码：

```rust
pub fn get_or_create_delegations(
    &mut self,
) -> (
    Arc<Vec<(u32, DelegationCircuitPrecomputations<A, B>)>>,
    Arc<Vec<(u32, Arc<Vec<Mersenne31Field, B>>)>>,
) {
    if self.delegations.is_empty() {
        let worker = worker::Worker::new_with_num_threads(8);
        // Compute the setup here
        self.delegations = Arc::new(setups::all_delegation_circuits_precomputations(&worker));
        let mut delegation_evals = Vec::new();
        for (circuit, setup) in self.delegations.iter() {
            let eval = create_circuit_setup(&setup.setup.ldes[0].trace);
            delegation_evals.push((circuit.clone(), Arc::new(eval)));
        }
        self.delegation_evals = Arc::new(delegation_evals);
    }
    (self.delegations.clone(), self.delegation_evals.clone())
}
```

源码第73到第91行。

先看返回类型：

```rust
(
    Arc<Vec<(u32, DelegationCircuitPrecomputations<A, B>)>>,
    Arc<Vec<(u32, Arc<Vec<Mersenne31Field, B>>)>>,
)
```

第一个返回值是delegation setup列表：

```text
[(delegation_type_id, delegation_precomputations)]
```

第二个返回值是delegation setup evaluations列表：

```text
[(delegation_type_id, setup_evaluation_vector)]
```

第一行逻辑：

```rust
if self.delegations.is_empty() {
```

如果还没有生成delegation setup，就进入构造流程。注意这里没有bytecode hash，因为delegation circuits是固定电路。某个程序有没有调用BLAKE2，不改变BLAKE2 delegation circuit本身的setup。

然后：

```rust
let worker = worker::Worker::new_with_num_threads(8);
```

同样创建8线程Worker。

接着：

```rust
self.delegations = Arc::new(setups::all_delegation_circuits_precomputations(&worker));
```

这里下钻看`all_delegation_circuits_precomputations`。

源码：

```rust
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
        // ...
    ]
}
```

定义在`circuit_defs/setups/src/lib.rs`第204到第225行。

这说明当前`all_delegation_circuits_precomputations`会生成两类delegation circuit setup：

```text
BLAKE2 compression delegation
BigInt with control delegation
```

每个元素都是：

```text
(delegation_type_id, delegation_circuit_precomputations)
```

其中`DELEGATION_TYPE_ID`是main RISC-V程序通过CSR调用delegation时用来识别目标delegation circuit的编号。官方Circuit Entry Points文档也说明每个precompile暴露唯一`DELEGATION_TYPE_ID`，必须和程序写入的CSR值匹配。([GitHub][1])

回到`get_or_create_delegations`：

```rust
let mut delegation_evals = Vec::new();
for (circuit, setup) in self.delegations.iter() {
    let eval = create_circuit_setup(&setup.setup.ldes[0].trace);
    delegation_evals.push((circuit.clone(), Arc::new(eval)));
}
self.delegation_evals = Arc::new(delegation_evals);
```

这段为每个delegation setup生成对应的setup evaluation vector。这里的`circuit`变量是`u32`，也就是delegation type id。`setup`是`DelegationCircuitPrecomputations<A,B>`。每个delegation setup内部也有：

```rust
setup.setup.ldes[0].trace
```

所以同样可以用`create_circuit_setup`转成evaluation vector。

最后：

```rust
(self.delegations.clone(), self.delegation_evals.clone())
```

这里返回的是`Arc` clone，不复制实际大数据。`Arc::clone`只是增加引用计数。

整个函数流程：

```text
get_or_create_delegations()
  |
  +-- if delegations empty:
  |     worker = Worker(8 threads)
  |     delegations = all_delegation_circuits_precomputations(worker)
  |       -> Blake2 setup
  |       -> BigInt setup
  |
  |     for each delegation setup:
  |       eval = create_circuit_setup(setup.setup.ldes[0].trace)
  |       save (delegation_type_id, eval)
  |
  +-- return Arc(delegations), Arc(delegation_evals)
```

## 2.11 本章里的理论点：setup、setup trace、setup evaluations

本章其实出现了三个容易混淆的词：

```text
setup
setup trace
setup evaluations
```

需要先固定含义。

`setup`在Airbender代码里通常指完整的电路预计算对象。比如：

```rust
MainCircuitPrecomputations
DelegationCircuitPrecomputations
```

它们包含很多字段，不只是固定trace。

`setup trace`是setup对象内部的固定trace表示。在本章里通过：

```rust
setup.setup.ldes[0].trace
```

访问。这个trace来自`SetupPrecomputations`内部，和lookup tables、setup layout、trace length有关。它可以理解为“固定表和固定列在后端domain上的表示”。

`setup evaluations`是`create_circuit_setup`返回的：

```rust
Vec<Mersenne31Field, B>
```

它是把row-major setup trace转置、去掉padding后的结果。它更接近“按列排列的固定setup数据”。

用一张图表示：

```text
get_main_riscv_circuit_setup(bytecode)
  |
  v
MainCircuitPrecomputations
  |
  +-- compiled_circuit
  +-- table_driver
  +-- twiddles
  +-- lde_precomputations
  +-- setup: SetupPrecomputations
  |     |
  |     +-- ldes[0].trace   <- setup trace, row-major
  |
  +-- witness_eval_fn_for_gpu_tracer

setup.setup.ldes[0].trace
  |
  v
create_circuit_setup(...)
  |
  v
Vec<Mersenne31Field>     <- setup evaluations
```

## 2.12 本章里的工程点：为什么要缓存

setup生成很贵。标准main RISC-V setup要根据bytecode编译机器、生成table driver、做twiddles和LDE预计算、生成setup precomputations。源码里每次cache miss都会创建Worker并调用`get_main_riscv_circuit_setup`，再调用`create_circuit_setup`。

如果CLI流程里同一个bytecode需要多次用到setup，比如证明、生成VK、生成常量、递归流程里复用，缓存能避免重复构造。

为什么value要用`Arc`？

```text
MainCircuitPrecomputations很大。
Vec<Mersenne31Field>也可能很大。
直接clone会复制大量数据。
Arc clone只复制指针和引用计数。
```

为什么key用bytecode hash？

```text
main/reduced circuit setup依赖bytecode。
相同bytecode可以复用setup。
不同bytecode需要不同ROM/table driver，因此不能复用。
```

为什么delegation不按bytecode hash？

```text
delegation circuit本身固定。
BLAKE2 delegation setup不依赖当前RISC-V程序ROM。
BigInt delegation setup也不依赖当前RISC-V程序ROM。
```

## 2.13 和真实CLI入口的关系

前面已经说过，`tools/cli/src/setup.rs`本身不是命令解析文件。真实CLI命令在`tools/cli/src/main.rs`里。`main.rs`用Clap定义`Commands`，其中包括`Prove`、`ProveFinal`、`Verify`、`VerifyAll`、`Run`、`GenerateVk`、`GenerateConstants`等。源码在`main.rs`第44到第212行。 

`main()`解析命令后按分支调用相应函数。例如`Prove`分支会先读取输入，再调用`create_proofs`；`GenerateVk`分支调用`generate_vk`；`Run`分支调用`run_binary`。这些在`main.rs`第46到第179行。

我目前没有在仓库搜索结果里看到`SetupCache`在其他文件中的引用；GitHub search只返回了`tools/cli/src/setup.rs`本身。也就是说，仅从当前抓取到的源码看，`SetupCache`是一个准备好的缓存工具，但它是否被当前CLI路径实际使用，需要后续继续追`cli_lib::prover_utils`、`vk::generate_vk`等调用链确认。这个点我先明确标记为“待确认”，不在这里强行假设它一定被某个命令调用。

## 2.14 本章小结

本章读完之后，要掌握以下几个结论。

第一，`tools/cli/src/setup.rs`当前是一个setup缓存层，不是Clap命令入口。它的核心结构是`SetupCache<A,B>`，缓存main circuit setup、reduced circuit setup、delegation setup和对应setup evaluation vectors。

第二，标准main circuit setup通过：

```rust
setups::get_main_riscv_circuit_setup(&bytecode, &worker)
```

生成。这个函数进一步调用`risc_v_cycles::get_machine`和`risc_v_cycles::get_table_driver`，并创建twiddles、LDE precomputations和setup precomputations，最后打包成`MainCircuitPrecomputations<IMStandardIsaConfig,A,B>`。

第三，`create_circuit_setup`不会编译电路。它只是把`setup.setup.ldes[0].trace`从row-major布局转置成evaluation vector，并截掉padding列。

第四，delegation setup由`all_delegation_circuits_precomputations`生成，当前包含BLAKE2 compression和BigInt with control两类delegation circuit setup。

第五，缓存key使用`DefaultHasher`对bytecode做`u64` hash，这只是工程缓存key，不是证明安全边界。证明安全边界要等后面读ROM table、compiled circuit、setup precomputations和verification key时再展开。

下一章建议直接进入：

```text
circuit_defs/setups/src/circuits/main_riscv/mod.rs
```

也就是把刚才下钻过的`get_main_riscv_circuit_setup`作为主角，继续逐行拆它调用的：

```text
risc_v_cycles::get_machine
risc_v_cycles::get_table_driver
Twiddles::new
LdePrecomputations::new
SetupPrecomputations::from_tables_and_trace_len
witness_eval_fn_for_gpu_tracer
```

这一章会正式从“CLI缓存层”进入“main RISC-V约束系统setup层”。

[1]: https://raw.githubusercontent.com/matter-labs/zksync-airbender/main/docs/circuit_entry_points.md "raw.githubusercontent.com"

对，这里最容易忘，是因为两个函数名字太像，而且它们确实做了几乎一样的事。但你可以用一句话记住：

`create_table_driver_into_cs`是在“编译电路规则”时告诉compiler有哪些表；`create_table_driver`是在“生成setup固定表”时真的拿到这些表内容。

换成更形象的记法：

```text
create_table_driver_into_cs:
  给设计图用。
  作用是：电路编译器画房子时，要知道有哪些固定表、表有多宽、多大、ID是什么。

create_table_driver:
  给施工材料用。
  作用是：setup/prover真的要把这些表内容铺进setup trace，并承诺它们。
```

这里的`compiler`不是Rust编译器，而是Airbender自己的“电路编译器”。它把machine写出来的抽象变量、约束、lookup、memory query，编译成真实trace列布局和约束布局。源码里`compile_machine`先创建一个`Circuit`，然后调用`create_table_driver_into_cs(&mut cs, machine)`，再调用`M::describe_state_transition(&mut cs)`，最后`cs.finalize()`得到`CircuitOutput`。

### 1. 先把三个角色分清楚

这里有三个东西：

```text
Machine:
  机器语义。
  它知道一行RISC-V执行应该怎么约束。

Circuit / BasicAssembly:
  约束收集器。
  Machine往里面写变量、约束、lookup、memory query、表信息。

TableDriver:
  固定表内容容器。
  它保存真正的lookup表内容，比如decoder表、range表、RomRead表、CSR表。
```

一个很容易记的比喻：

```text
Machine = 老师
  老师知道考试规则：ADD要满足rd = rs1 + rs2，LW要从内存读，SW要写内存。

Circuit / BasicAssembly = 记笔记的人
  老师讲规则时，它把“变量、约束、lookup需求”都记下来。

TableDriver = 附录资料库
  里面放查表资料：ROM表、decoder表、range表、CSR表。
```

所以`compile_machine`做的事情不是执行程序，而是让Machine把“一行CPU执行规则”讲出来，让`BasicAssembly`记录下来。

### 2. 为什么compiler阶段也需要知道表？

因为Machine在描述约束时会说：

```text
当前opcode要查decoder表。
当前pc要查ROM相关表。
某些limb要查range表。
某些bit操作要查And表。
```

如果compiler不知道这些表存在，就没法编译lookup。

举一个ADD例子。假设当前程序里有一条：

```text
pc = 0
instruction = ADD x5, x1, x2
```

证明系统最终要检查几件事：

```text
1. pc=0处的instruction确实来自ROM表。
2. 这个instruction decode出来确实是ADD。
3. 如果是ADD，则x5 = x1 + x2。
4. x1、x2、x5的寄存器读写要进入memory argument。
```

其中第1步和第2步都依赖表：

```text
RomRead表:
  pc -> opcode_low16, opcode_high16

OpTypeBitmask / decoder表:
  opcode bits -> 哪个major family，哪个minor variant

Range表 / bit表:
  检查limb、bit、拆分是否合法
```

所以在编译ADD规则的时候，compiler必须知道：

```text
这套电路会用RomRead表吗？
会用decoder表吗？
这些表在lookup系统中的table_id是什么？
这些表最终会如何编码进setup columns？
```

但注意：compiler阶段不一定是为了马上把表内容铺进setup trace。它首先是为了正确生成电路布局和lookup布局。

### 3. create_table_driver_into_cs：把表注册到Circuit里

源码里`create_table_driver_into_cs`的目标是`cs: &mut CS`，也就是正在构造的Circuit：

```rust
pub fn create_table_driver_into_cs<
    F: PrimeField,
    CS: Circuit<F>,
    M: Machine<F>,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    cs: &mut CS,
    machine: M,
)
```

它里面调用的是：

```rust
cs.materialize_table(table);
cs.add_table_with_content(table, content);
```

也就是说，它不是返回一个独立的`TableDriver`，而是把表信息写进当前Circuit对象。源码中它会注册machine声明的`used_tables`，加入`extra_tables`，再materialize一些通用表，比如`And`、`ZeroEntry`、两个quick decode decomposition表、`U16GetSignAndHighByte`、`RangeCheckSmall`，然后创建decoder table并加入`OpTypeBitmask`。如果machine使用ROM bytecode，还会加入`RomAddressSpaceSeparator`表。

这一步可以记成：

```text
create_table_driver_into_cs:
  “把表告诉正在编译的电路。”
```

它像是在画建筑设计图时，先把材料规格标进图纸：

```text
这个房子会用钢筋表。
这个房子会用水泥规格表。
这个房子会用门窗型号表。
```

但它还不是最后把材料运到工地。

### 4. create_table_driver：生成独立TableDriver

另一个函数`create_table_driver`的目标不同：

```rust
pub fn create_table_driver<
    F: PrimeField,
    M: Machine<F>,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    machine: M,
) -> TableDriver<F>
```

它返回一个独立的`TableDriver<F>`。内部逻辑和`create_table_driver_into_cs`高度相似：materialize used tables，加入extra tables，materialize通用表，加入decoder table，如果使用ROM bytecode就加入`RomAddressSpaceSeparator`。

这一步可以记成：

```text
create_table_driver:
  “真的生成一份固定表资料库。”
```

它不是写进Circuit，而是得到一个能被setup/prover使用的对象。

后面`SetupPrecomputations::from_tables_and_trace_len`会拿这个`TableDriver`，调用`get_main_domain_trace`，把表内容dump出来并写入setup trace。源码里它接收`table_driver`、`trace_len`、`setup_layout`，然后生成main-domain setup trace、做LDE、构造Merkle trees。

也就是说：

```text
TableDriver
  |
  v
setup trace
  |
  v
LDE
  |
  v
Merkle tree commitment
```

### 5. 为什么不能只用一套？

因为compiler和setup/prover处在两个不同阶段。

第一阶段是编译阶段：

```text
目的：
  生成CompiledCircuitArtifact。

需要：
  知道用了哪些表；
  知道表大小；
  知道lookup布局；
  知道setup columns怎么排；
  把Variable编号变成ColumnAddress。

不主要负责：
  生成setup Merkle tree。
```

第二阶段是setup/prover阶段：

```text
目的：
  真的把固定表内容写成setup trace；
  对setup trace做LDE；
  构造setup Merkle tree；
  后续proof引用这些固定表承诺。

需要：
  独立TableDriver对象。
```

所以不能只用`create_table_driver_into_cs`，因为它把表注册进Circuit，不返回独立`TableDriver`给setup用。

也不能只用`create_table_driver`，因为`compile_machine`正在构造的是一个`Circuit`，Machine后面还要继续往这个`Circuit`里写lookup、约束、memory query。表信息必须和这些约束一起进入同一个`CircuitOutput`。

源码也能看出这一点：`CircuitOutput`里本身有`table_driver`字段，同时还有constraints、lookups、shuffle RAM queries等字段。也就是说，compiler阶段的表信息最后会和约束系统一起形成`CircuitOutput`。

### 6. get_machine和get_table_driver为什么都创建表？

这是你最容易忘的点。核心原因是：它们服务不同消费者。

```text
get_machine:
  消费者是compiler。
  它最终要得到CompiledCircuitArtifact。

get_table_driver:
  消费者是setup/prover。
  它最终要得到独立TableDriver。
```

`get_machine`路径里，`default_compile_machine`会先`compile_machine`得到`CircuitOutput`，然后把当前程序的`RomRead`表和CSR表加入`cs_output.table_driver`，再交给`OneRowCompiler`编译。源码中可以看到`default_compile_machine`显式把`TableType::RomRead`和`TableType::SpecialCSRProperties`加入`cs_output.table_driver`。

这一步是为了让compiler知道：

```text
当前程序的ROM表参与这套电路；
当前CSR delegation白名单参与这套电路；
这些表会影响setup layout、table offsets、lookup argument布局。
```

但最终的`CompiledCircuitArtifact`里不保存完整`TableDriver`内容；它保存的是布局、约束、table offsets、total tables size等编译结果。源码里`CompiledCircuitArtifact`字段包括`witness_layout`、`memory_layout`、`setup_layout`、`stage_2_layout`、constraints、`table_offsets`、`total_tables_size`，而不是完整表内容。

所以还需要`get_table_driver`再生成一份独立的真实表内容给setup/prover使用。

你可以这样记：

```text
get_machine:
  我需要表，是为了“编译规则和布局”。

get_table_driver:
  我需要表，是为了“生成setup trace和承诺”。
```

### 7. 一个完整例子：ADD x5, x1, x2

假设bytecode里第一条指令是：

```text
pc = 0
ADD x5, x1, x2
```

#### 编译阶段发生什么？

路径是：

```text
get_machine
  -> default_compile_machine
    -> compile_machine
      -> create_table_driver_into_cs
      -> M::describe_state_transition
      -> CircuitOutput
    -> add RomRead table into CircuitOutput.table_driver
    -> add CSR table into CircuitOutput.table_driver
    -> OneRowCompiler
    -> CompiledCircuitArtifact
```

在`create_table_driver_into_cs`阶段，compiler知道这套电路会用：

```text
decoder表
range表
bit操作表
ROM地址辅助表
其他machine声明的表
```

然后`M::describe_state_transition`描述一行CPU执行规则，大概会产生这样的抽象关系：

```text
从ROM lookup拿到opcode_low16, opcode_high16；
从decoder lookup判断这是ADD；
读取x1；
读取x2；
写入x5；
约束x5 = x1 + x2；
更新pc到pc + 4。
```

此时这些东西还只是：

```text
Variable(10)
Variable(11)
Constraint(...)
LookupQuery(...)
ShuffleRamMemQuery(...)
```

它们还没有落到真实trace列。

然后`OneRowCompiler`把它们变成：

```text
Variable(10) -> witness column 3
Variable(11) -> memory column 8
ADD约束 -> degree_1 constraint 或 degree_2 constraint
ROM lookup -> stage_2 lookup layout
memory query -> memory argument layout
```

最后得到`CompiledCircuitArtifact`。

这个artifact回答：

```text
如果你给我一份执行trace，
我知道应该在哪些列读x1、x2、x5；
我知道应该怎样检查ADD；
我知道应该怎样检查ROM lookup；
我知道应该怎样检查memory argument。
```

#### setup/prover阶段发生什么？

路径是：

```text
get_table_driver
  -> create_table_driver
  -> add RomRead table
  -> add CSR table
  -> TableDriver

SetupPrecomputations::from_tables_and_trace_len
  -> dump tables
  -> write setup trace
  -> LDE
  -> Merkle tree
```

这里真正把表内容铺开：

```text
RomRead表:
  pc=0 -> ADD的opcode_low16, opcode_high16

decoder表:
  ADD opcode bits -> is_add = 1

range表:
  0, 1, 2, ..., 65535

其他通用表:
  bit operation、decomposition等
```

这些固定表进入setup trace，之后被Merkle tree承诺。

所以ADD证明最终依赖两边：

```text
CompiledCircuitArtifact:
  告诉prover/verifier怎么检查ADD。

SetupPrecomputations:
  承诺ROM表和decoder表等固定资料。

Witness trace:
  提供这次执行中x1、x2、x5的具体值。
```

### 8. 一个更容易记住的表格

| 名字                        | 一句话        | 谁用它                       | 它回答的问题                        |
| ------------------------- | ---------- | ------------------------- | ----------------------------- |
| `Machine`                 | RISC-V语义定义 | compiler                  | 一行CPU应该怎么约束？                  |
| `Circuit / BasicAssembly` | 约束收集器      | `compile_machine`         | Machine写出的变量、约束、lookup先放哪？    |
| `CircuitOutput`           | 未排版的电路草稿   | `OneRowCompiler`          | 有哪些变量、约束、lookup、memory query？ |
| `OneRowCompiler`          | 电路排版器      | `default_compile_machine` | 变量和约束落到哪些列？                   |
| `CompiledCircuitArtifact` | 编译后的规则书    | prover/verifier           | 如何检查trace？                    |
| `TableDriver`             | 固定表资料库     | setup/prover              | lookup表里具体有什么？                |
| `SetupPrecomputations`    | 固定表承诺      | prover/verifier           | 固定表如何被绑定到proof？               |

### 9. 最短记忆口诀

你可以这样背：

```text
Machine写规则。
Circuit记规则。
CircuitOutput是草稿。
OneRowCompiler排列。
CompiledCircuitArtifact是规则书。

TableDriver装表。
SetupPrecomputations承诺表。
Witness trace装执行值。
```

再对应这两个函数：

```text
create_table_driver_into_cs:
  表信息进Circuit，服务编译。

create_table_driver:
  表内容进TableDriver，服务setup/prover。
```

最重要的区别不是“代码长得一不一样”，而是“谁消费它”：

```text
compiler消费的是：
  表的存在、大小、ID、布局需求。

setup/prover消费的是：
  表的真实内容，并把它写成setup trace。
```

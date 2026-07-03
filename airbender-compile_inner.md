重要，而且非常重要。`compile_inner` 是 Airbender 约束系统里从“抽象电路规则”进入“可生成 witness / 可证明布局”的核心函数。

你可以把它放在这条线上：

```text
第4章：
Machine::describe_state_transition
  -> BasicAssembly
  -> CircuitOutput

第5章：
CircuitOutput
  -> OneRowCompiler::compile_inner
  -> CompiledCircuitArtifact
  -> SetupPrecomputations

第6章：
CompiledCircuitArtifact + CycleData + TableDriver
  -> evaluate_witness
  -> WitnessEvaluationData
```

`compile_inner` 就是中间这个转换器：

```text
CircuitOutput 里的 Variable / Constraint / LookupQuery / ShuffleRamQuery
  ↓
具体列布局 ColumnAddress / WitnessSubtree / MemorySubtree / SetupLayout
  ↓
CompiledCircuitArtifact
```

源码里两个公开入口都调用它：

```rust
pub fn compile_output_for_chunked_memory_argument(
    &self,
    circuit_output: CircuitOutput<F>,
    trace_len_log2: usize,
) -> CompiledCircuitArtifact<F> {
    Self::compile_inner::<false>(self, circuit_output, trace_len_log2)
}

pub fn compile_to_evaluate_delegations(
    &self,
    circuit_output: CircuitOutput<F>,
    trace_len_log2: usize,
) -> CompiledCircuitArtifact<F> {
    Self::compile_inner::<true>(self, circuit_output, trace_len_log2)
}
```

`compile_output_for_chunked_memory_argument` 用于 main RISC-V；`compile_to_evaluate_delegations` 用于 delegation circuit。区别由 const 泛型 `FOR_DELEGATION` 控制。`false` 是 main RISC-V，`true` 是 delegation。

------

## 1. compile_inner 的定位

源码注释直接写了它的目标：

```rust
// our main purposes are:
// - place variables in particular grid places
// - select whether they go into witness subtree or memory subtree
// - normalize constraints to address particular columns insteap of variable indexes
// - try to apply some heuristrics
```

对应中文：

```text
1. 给每个 Variable 分配具体列位置。
2. 决定变量进 witness subtree 还是 memory subtree。
3. 把基于 Variable 的 Constraint 改写成基于 ColumnAddress 的约束。
4. 做一些变量消除和布局优化。
```

这几句话基本就是 `compile_inner` 的功能边界。

所以它不是 witness 生成函数，也不是 setup trace 生成函数。它做的是：

```text
布局编译。
```

输入是 `CircuitOutput`，输出是 `CompiledCircuitArtifact`。

------

## 2. 它消费什么：CircuitOutput

一进函数，它把 `CircuitOutput` 拆开：

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

这些字段你前面基本都已经见过：

```text
state_input / state_output:
  当前 chunk 的初始状态和最终状态变量。

table_driver:
  固定 lookup table 的目录和表大小信息。

num_of_variables:
  BasicAssembly 创建过多少个 Variable。

constraints:
  普通多项式约束。

lookups:
  RomRead、decoder、range、bit 表等 lookup query。

shuffle_ram_queries:
  main RISC-V 的 3 个内存/寄存器访问槽位。

range_check_expressions:
  require_invariant RangeChecked 生成的 range check 请求。

boolean_vars:
  require_invariant Boolean 生成的 boolean 变量列表。

substitutions:
  Placeholder 到 Variable 的映射。

delegated_computation_requests:
  main RISC-V 发出的 delegation request。

degegated_request_to_process:
  delegation circuit 自己要处理的 request。
```

也就是说，`compile_inner` 是 `CircuitOutput` 的主要消费者。

------

## 3. 先判断当前是 main RISC-V 还是 delegation

`FOR_DELEGATION` 控制两条分支。

delegation 模式要求：

```rust
if FOR_DELEGATION {
    assert!(state_input.is_empty());
    assert!(state_output.is_empty());
    assert!(shuffle_ram_queries.is_empty());
    assert!(linked_variables.is_empty());
    assert!(degegated_request_to_process.is_some());
    assert!(delegated_computation_requests.is_empty());
    assert!(
        batched_memory_accesses.len() > 0
            || register_and_indirect_memory_accesses.len() > 0
    );
```

main RISC-V 模式要求：

```rust
} else {
    assert_eq!(shuffle_ram_queries.len(), 3);
    assert!(linked_variables.is_empty());
    assert!(degegated_request_to_process.is_none());
    assert!(batched_memory_accesses.is_empty());
    assert!(register_and_indirect_memory_accesses.is_empty());
}
```

main RISC-V 的重点是：

```text
shuffle_ram_queries.len() == 3
```

这对应你第四章里一直看的三个访问槽位：

```text
slot0: rs1 read
slot1: rs2 read 或 RAM read
slot2: rd write 或 RAM write
```

所以 `compile_inner::<false>` 硬编码认可 main RISC-V 每行有 3 个 shuffle RAM query。

------

## 4. 计算 trace_len、table size、setup_layout

接着：

```rust
let trace_len = 1usize << trace_len_log2;
let total_tables_size = table_driver.total_tables_len;
let lookup_table_encoding_capacity = trace_len - 1;
let mut num_required_tuples_for_generic_lookup_setup =
    total_tables_size / lookup_table_encoding_capacity;
if total_tables_size % lookup_table_encoding_capacity != 0 {
    num_required_tuples_for_generic_lookup_setup += 1;
}
```

这里计算 setup 表需要多少组 generic lookup setup columns。

含义：

```text
trace_len:
  当前 circuit trace 总行数。

trace_len - 1:
  每组 generic lookup setup columns 能容纳的表行数。

table_driver.total_tables_len:
  所有 fixed lookup table dump 后的总行数。

num_required_tuples_for_generic_lookup_setup:
  需要多少组 setup lookup columns 才能放完所有 fixed table rows。
```

然后：

```rust
let need_timestamps = !FOR_DELEGATION;
let setup_layout =
    SetupLayout::layout_for_lookup_size(total_tables_size, trace_len, need_timestamps);
```

main RISC-V 下 `need_timestamps = true`。所以 main RISC-V 的 setup layout 会包含 timestamp setup columns。delegation 下 `need_timestamps = false`。

这一步很重要，因为第五章里的 `SetupPrecomputations::from_tables_and_trace_len` 会拿这个 `setup_layout` 去写 setup trace。

```text
compile_inner:
  只生成 setup_layout。

SetupPrecomputations:
  根据 setup_layout 真正写 setup trace。
```

------

## 5. 创建 Variable 到 ColumnAddress 的映射

接着：

```rust
let mut num_variables = num_of_variables as u64;

let mut all_variables_to_place = BTreeSet::new();
for variable_idx in 0..num_variables {
    all_variables_to_place.insert(Variable(variable_idx));
}

let mut memory_tree_offset = 0;
// as a byproduct we will also create a map of witness generation functions
let mut layout = BTreeMap::<Variable, ColumnAddress>::new();
```

这里初始化布局状态：

```text
all_variables_to_place:
  所有还没分配列位置的 Variable。

layout:
  Variable -> ColumnAddress 的映射表。

memory_tree_offset:
  当前 memory subtree 已经用了多少列。

num_variables:
  当前变量总数。后面 compiler 还会创建一些辅助变量，所以它会继续增长。
```

`layout` 是后面最关键的数据结构。第四章里你看到的 `Variable(17)`，到了这里会被变成：

```text
Variable(17) -> ColumnAddress::WitnessSubtree(offset)
或
Variable(17) -> ColumnAddress::MemorySubtree(offset)
或
Variable(17) -> ColumnAddress::OptimizedOut(offset)
```

------

## 6. main RISC-V 分支：先布局 memory subtree

前面已经完成了三件事：

```text
1. 拆开 CircuitOutput。
2. 确认当前不是 delegation circuit，而是 main RISC-V circuit。
3. 初始化变量布局状态：
   - all_variables_to_place
   - layout: Variable -> ColumnAddress
   - memory_tree_offset
```

现在代码进入 main RISC-V 分支：

```rust
if FOR_DELEGATION {
    ...
} else {
    ...
}
```

这里的 `FOR_DELEGATION = false`，所以当前处理的是 main RISC-V circuit。

这一大段代码做的事情是：

```text
把 main RISC-V 每一行需要参与 memory argument 的变量，分配到 memory subtree 的具体列里。
```

这里要先区分三个概念。

### 6.0.1 memory subtree 是什么

Airbender 的执行 trace 不是只有一类列。对 main RISC-V 来说，后面 witness 阶段会生成一张 `exec_trace`：

```text
exec_trace row = [ witness subtree columns | memory subtree columns ]
```

其中：

```text
witness subtree:
  放普通电路变量、boolean、range check、lookup multiplicity、普通约束相关变量。

memory subtree:
  放 memory argument 需要检查的访问记录。
  包括寄存器读写、RAM 读写、read timestamp、read value、write value、address。
```

main RISC-V 每个 cycle 固定有 3 个 shuffle RAM 访问槽位：

```text
slot0: rs1 register read
slot1: rs2 register read 或 RAM read
slot2: rd register write 或 RAM write
```

这些槽位的真实值在 witness 阶段由 `CycleData` 和 `MainRiscVOracle` 填入。当前 setup/compile 阶段只决定：

```text
这些值以后应该写到 memory subtree 的哪些列。
```

### 6.0.2 memory_tree_offset 是什么

`memory_tree_offset` 是 memory subtree 的列分配游标。

初始化时：

```rust
let mut memory_tree_offset = 0;
```

含义：

```text
memory subtree 目前还没分配任何列。
```

每放置一个变量，`memory_tree_offset` 就向后移动。

例如一个 32-bit 值通常拆成两个 16-bit limb：

```text
value_low16
value_high16
```

它会占用两个 memory columns。

如果当前：

```text
memory_tree_offset = 0
```

布局一个 32-bit 地址后：

```text
address_low16  -> MemorySubtree(0)
address_high16 -> MemorySubtree(1)
memory_tree_offset = 2
```

再布局一个 32-bit read value：

```text
read_value_low16  -> MemorySubtree(2)
read_value_high16 -> MemorySubtree(3)
memory_tree_offset = 4
```

所以 `memory_tree_offset` 记录的是：

```text
memory subtree 下一次可以分配的列位置。
```

### 6.0.3 这里不填真实值

本节所有代码都不写：

```text
pc = 0
x1 = 7
read_value = 9
timestamp = ...
```

这些真实值属于 witness 阶段。

当前代码只生成：

```text
Variable -> MemorySubtree(column)
```

也就是列布局。

---

## 6.1 创建 lazy init 辅助变量

源码片段：

```rust
let lazy_init_aux_set = {
    let tmp_low_var =
        add_compiler_defined_variable(&mut num_variables, &mut all_variables_to_place);
    let tmp_high_var =
        add_compiler_defined_variable(&mut num_variables, &mut all_variables_to_place);
    let intermediate_borrow_var =
        add_compiler_defined_variable(&mut num_variables, &mut all_variables_to_place);
    let final_borrow_var =
        add_compiler_defined_variable(&mut num_variables, &mut all_variables_to_place);

    let lazy_init_aux_set = (
        [tmp_low_var, tmp_high_var],
        intermediate_borrow_var,
        final_borrow_var,
    );
    range_check_expressions.push(RangeCheckQuery::new(
        tmp_low_var,
        LARGE_RANGE_CHECK_TABLE_WIDTH,
    ));
    range_check_expressions.push(RangeCheckQuery::new(
        tmp_high_var,
        LARGE_RANGE_CHECK_TABLE_WIDTH,
    ));
    boolean_vars.push(intermediate_borrow_var);
    boolean_vars.push(final_borrow_var);

    lazy_init_aux_set
};
```

这段代码创建的是 lazy init / teardown 排序检查需要的辅助变量。

### 6.1.1 lazy init / teardown 是什么

main RISC-V 访问 memory 时，并不是所有 RAM cell 一开始都会真实出现在 trace 里。Airbender 用 lazy init / teardown 处理 memory argument 的边界。

在 setup/compile 层，可以先记住：

```text
lazy init:
  某个 memory address 第一次进入 memory argument 时的初始化记录。

teardown:
  某个 memory address 在当前 chunk 结束时的最终值和最终 timestamp。
```

这些信息后面会进入 memory argument。当前 `compile_inner` 要给这些记录分配列。

为了证明 lazy init 地址顺序正确，compiler 自己创建一些辅助变量：

```text
tmp_low_var
tmp_high_var
intermediate_borrow_var
final_borrow_var
```

这些变量不是 `Machine::describe_state_transition` 里创建的 opcode 变量，而是 `OneRowCompiler` 自己添加的变量。

### 6.1.2 add_compiler_defined_variable 做什么

每次调用：

```rust
add_compiler_defined_variable(&mut num_variables, &mut all_variables_to_place)
```

都会做两件事：

```text
1. 创建一个新的 Variable(num_variables)。
2. 把这个新变量加入 all_variables_to_place。
3. num_variables += 1。
```

也就是说，compiler 自己新增的变量也必须被放置到某个列地址里。

例如原来：

```text
num_variables = 1000
```

调用一次后：

```text
new variable = Variable(1000)
all_variables_to_place 插入 Variable(1000)
num_variables = 1001
```

这里连续创建四个变量：

```text
tmp_low_var
tmp_high_var
intermediate_borrow_var
final_borrow_var
```

### 6.1.3 tmp_low_var / tmp_high_var 是什么

`tmp_low_var` 和 `tmp_high_var` 是地址差值拆分后的两个 16-bit limb。

lazy init 地址排序大概需要检查：

```text
current_address < next_address
```

为了把 32-bit 地址比较拆成 field 友好的形式，需要 low/high 两段辅助值：

```text
tmp_low_var:
  low 16-bit 辅助结果。

tmp_high_var:
  high 16-bit 辅助结果。
```

这两个值必须是 16-bit，所以后面加入 range check：

```rust
range_check_expressions.push(RangeCheckQuery::new(
    tmp_low_var,
    LARGE_RANGE_CHECK_TABLE_WIDTH,
));
range_check_expressions.push(RangeCheckQuery::new(
    tmp_high_var,
    LARGE_RANGE_CHECK_TABLE_WIDTH,
));
```

`LARGE_RANGE_CHECK_TABLE_WIDTH` 在这里对应 16-bit range check。

含义：

```text
tmp_low_var 必须在 0..2^16-1。
tmp_high_var 必须在 0..2^16-1。
```

这里还没有执行 range check。这里只是把 range check 请求加入 `range_check_expressions`。后面 witness subtree 布局阶段会读取这些请求，并生成对应 lookup expression。

### 6.1.4 intermediate_borrow_var / final_borrow_var 是什么

地址比较会产生借位。

```text
intermediate_borrow_var:
  low 16-bit 减法产生的中间借位。

final_borrow_var:
  high 16-bit 减法后的最终借位。
```

这两个变量只能是 0 或 1，所以加入 `boolean_vars`：

```rust
boolean_vars.push(intermediate_borrow_var);
boolean_vars.push(final_borrow_var);
```

后面 witness subtree 布局 boolean 变量时，会给每个 boolean 变量生成：

```text
b^2 - b = 0
```

这条约束强制：

```text
b ∈ {0, 1}
```

### 6.1.5 lazy_init_aux_set 保存什么

源码：

```rust
let lazy_init_aux_set = (
    [tmp_low_var, tmp_high_var],
    intermediate_borrow_var,
    final_borrow_var,
);
```

它把四个辅助变量打包成一个对象：

```text
lazy_init_aux_set =
  (
    [地址差值 low/high 辅助变量],
    中间借位变量,
    最终借位变量
  )
```

后面会把这些变量映射成 `ColumnAddress`，再放进：

```text
CompiledCircuitArtifact.lazy_init_address_aux_vars
```

witness 阶段 `process_lazy_init_work` 会根据这个布局写入真实辅助值。

---

## 6.2 给 lazy init / teardown 创建变量并分配 memory columns

源码片段：

```rust
let shuffle_ram_init_addresses = add_multiple_compiler_defined_variables::<REGISTER_SIZE>(
    &mut num_variables,
    &mut all_variables_to_place,
);
let shuffle_ram_teardown_values = add_multiple_compiler_defined_variables::<REGISTER_SIZE>(
    &mut num_variables,
    &mut all_variables_to_place,
);
let shuffle_ram_teardown_timestamps = add_multiple_compiler_defined_variables::<
    NUM_TIMESTAMP_COLUMNS_FOR_RAM,
>(
    &mut num_variables,
    &mut all_variables_to_place,
);
```

这里创建三组 compiler-defined variables。

### 6.2.1 REGISTER_SIZE 是什么

`REGISTER_SIZE` 表示一个 32-bit RISC-V word 在 field trace 里拆成多少个 limb。

通常这里是两个 16-bit limb：

```text
32-bit value = low16 + high16 * 2^16
```

所以：

```text
REGISTER_SIZE = 2
```

因此：

```rust
add_multiple_compiler_defined_variables::<REGISTER_SIZE>(...)
```

会创建两个变量。

例如：

```text
shuffle_ram_init_addresses = [addr_low_var, addr_high_var]
```

### 6.2.2 shuffle_ram_init_addresses

```rust
let shuffle_ram_init_addresses = add_multiple_compiler_defined_variables::<REGISTER_SIZE>(...);
```

这组变量表示：

```text
当前 row 的 lazy init address。
```

它是 32-bit 地址，所以拆成两个 16-bit limb：

```text
lazy_init_address_low16
lazy_init_address_high16
```

### 6.2.3 shuffle_ram_teardown_values

```rust
let shuffle_ram_teardown_values = add_multiple_compiler_defined_variables::<REGISTER_SIZE>(...);
```

这组变量表示：

```text
当前 row 的 teardown value。
```

也就是某个 memory cell 在 chunk 结束时的最终值。

它也是 32-bit word，所以拆成两个 16-bit limb：

```text
teardown_value_low16
teardown_value_high16
```

### 6.2.4 shuffle_ram_teardown_timestamps

```rust
let shuffle_ram_teardown_timestamps = add_multiple_compiler_defined_variables::<
    NUM_TIMESTAMP_COLUMNS_FOR_RAM,
>(...);
```

这组变量表示：

```text
当前 row 的 teardown timestamp。
```

timestamp 也会拆成多个 limb。`NUM_TIMESTAMP_COLUMNS_FOR_RAM` 表示 timestamp 在 memory subtree 中占多少列。

如果是两个 timestamp limb，那么它类似：

```text
teardown_timestamp_low
teardown_timestamp_high
```

### 6.2.5 分配 lazy init / teardown 的 memory columns

接着源码：

```rust
let lazy_init_addresses_columns = layout_memory_subtree_multiple_variables(
    &mut memory_tree_offset,
    shuffle_ram_init_addresses,
    &mut all_variables_to_place,
    &mut layout,
);
let lazy_teardown_values_columns = layout_memory_subtree_multiple_variables(
    &mut memory_tree_offset,
    shuffle_ram_teardown_values,
    &mut all_variables_to_place,
    &mut layout,
);
let lazy_teardown_timestamps_columns = layout_memory_subtree_multiple_variables(
    &mut memory_tree_offset,
    shuffle_ram_teardown_timestamps,
    &mut all_variables_to_place,
    &mut layout,
);
```

`layout_memory_subtree_multiple_variables` 的作用是：

```text
把一组 Variable 连续放进 memory subtree。
```

它会做几件事：

```text
1. 从 memory_tree_offset 当前值开始分配列。
2. 对每个 Variable 插入：
   Variable -> ColumnAddress::MemorySubtree(offset)
3. 从 all_variables_to_place 删除这些变量。
4. 推进 memory_tree_offset。
5. 返回 ColumnSet，记录这一组变量对应的列范围。
```

举例：

如果当前：

```text
memory_tree_offset = 0
shuffle_ram_init_addresses = [Variable(1000), Variable(1001)]
```

调用后：

```text
Variable(1000) -> MemorySubtree(0)
Variable(1001) -> MemorySubtree(1)

lazy_init_addresses_columns = columns [0, 1]

memory_tree_offset = 2
```

接着布局 teardown value：

```text
Variable(1002) -> MemorySubtree(2)
Variable(1003) -> MemorySubtree(3)

lazy_teardown_values_columns = columns [2, 3]

memory_tree_offset = 4
```

再布局 teardown timestamp：

```text
Variable(1004) -> MemorySubtree(4)
Variable(1005) -> MemorySubtree(5)

lazy_teardown_timestamps_columns = columns [4, 5]

memory_tree_offset = 6
```

这三个 `ColumnSet` 后面会被打包进：

```text
ShuffleRamInitAndTeardownLayout
```

witness 阶段会根据这些列位置写真实 lazy init / teardown 数据。

---

## 7. 布局 main RISC-V 的 3 个 shuffle RAM query

lazy init / teardown 列分配完后，代码开始处理 main RISC-V 每个 cycle 的 3 个 memory/register 访问槽位。

源码先检查顺序：

```rust
assert!(shuffle_ram_queries
    .is_sorted_by(|a, b| a.local_timestamp_in_cycle < b.local_timestamp_in_cycle));
shuffle_ram_queries.windows(2).for_each(|el| {
    assert!(el[0].local_timestamp_in_cycle + 1 == el[1].local_timestamp_in_cycle)
});
```

### 7.1 shuffle_ram_queries 是什么

`shuffle_ram_queries` 来自 `CircuitOutput`。

它是在 `Machine::describe_state_transition` 阶段登记的 memory/register 访问请求。

对 main RISC-V 来说，每一行固定有 3 个：

```text
shuffle_ram_queries[0] -> slot0
shuffle_ram_queries[1] -> slot1
shuffle_ram_queries[2] -> slot2
```

每个 query 描述一条访问：

```text
访问的是寄存器还是 RAM；
地址变量是什么；
read value 变量是什么；
write value 变量是什么；
该访问在当前 cycle 内的 local timestamp 是多少。
```

当前 `compile_inner` 不关心具体是 ADD、LW、SW。它只按照这 3 个 query 的通用结构分配 memory columns。

### 7.2 为什么检查 local_timestamp_in_cycle 连续

每个 cycle 内有多个访问。为了在 memory argument 中排序，需要给同一 cycle 内的访问分配局部顺序：

```text
slot0 -> local_timestamp_in_cycle = 0
slot1 -> local_timestamp_in_cycle = 1
slot2 -> local_timestamp_in_cycle = 2
```

代码检查两件事：

```rust
assert!(shuffle_ram_queries
    .is_sorted_by(|a, b| a.local_timestamp_in_cycle < b.local_timestamp_in_cycle));
```

表示：

```text
这 3 个 query 必须按 local timestamp 从小到大排列。
```

接着：

```rust
shuffle_ram_queries.windows(2).for_each(|el| {
    assert!(el[0].local_timestamp_in_cycle + 1 == el[1].local_timestamp_in_cycle)
});
```

表示：

```text
相邻 query 的 local timestamp 必须正好差 1。
```

所以允许：

```text
0, 1, 2
```

不允许：

```text
0, 2, 3
0, 1, 3
2, 1, 0
```

这个顺序后面会影响每个访问的 write timestamp。

---

## 7.3 遍历每个 shuffle RAM query

源码：

```rust
for (query_idx, memory_query) in shuffle_ram_queries.iter().enumerate() {
    assert_eq!(query_idx, memory_query.local_timestamp_in_cycle);
```

这里 `query_idx` 是循环下标：

```text
query_idx = 0
query_idx = 1
query_idx = 2
```

`memory_query.local_timestamp_in_cycle` 是 query 自己记录的 cycle 内访问顺序。

这句：

```rust
assert_eq!(query_idx, memory_query.local_timestamp_in_cycle);
```

要求：

```text
第 0 个 query 的 local timestamp 是 0。
第 1 个 query 的 local timestamp 是 1。
第 2 个 query 的 local timestamp 是 2。
```

---

## 7.4 每个 slot 都新增 read timestamp 变量

源码：

```rust
let [read_timestamp_low, read_timestamp_high] =
    add_multiple_compiler_defined_variables::<NUM_TIMESTAMP_COLUMNS_FOR_RAM>(
        &mut num_variables,
        &mut all_variables_to_place,
    );
let read_timestamp = layout_memory_subtree_multiple_variables(
    &mut memory_tree_offset,
    [read_timestamp_low, read_timestamp_high],
    &mut all_variables_to_place,
    &mut layout,
);
```

每个 memory/register 访问都要记录：

```text
这次读操作读到的是该地址在哪个历史 timestamp 写入的值。
```

这就是 `read_timestamp`。

例如：

```text
slot0 读取 x1。
x1 上一次被写入的 timestamp 是 ts_x1_old。
那么 slot0.read_timestamp = ts_x1_old。
```

read timestamp 是 witness 阶段由 tracer/oracle 提供的真实值。当前这里只分配列。

### 7.4.1 read_timestamp_low / read_timestamp_high

timestamp 被拆成两个 limb：

```text
read_timestamp_low
read_timestamp_high
```

这两个变量由 compiler 创建：

```rust
add_multiple_compiler_defined_variables::<NUM_TIMESTAMP_COLUMNS_FOR_RAM>(...)
```

然后放进 memory subtree：

```rust
layout_memory_subtree_multiple_variables(...)
```

假设当前 `memory_tree_offset = 6`，那么 slot0 的 read timestamp 可能变成：

```text
read_timestamp_low  -> MemorySubtree(6)
read_timestamp_high -> MemorySubtree(7)
memory_tree_offset = 8
```

返回的 `read_timestamp` 是一个 `ColumnSet`，记录这两个列的位置。

---

## 7.5 每个 slot 都新增 borrow_var

源码：

```rust
let borrow_var =
    add_compiler_defined_variable(&mut num_variables, &mut all_variables_to_place);
boolean_vars.push(borrow_var);
```

每个访问都需要证明：

```text
read_timestamp < write_timestamp
```

因为一次读操作必须读取过去写入的值，不能读取未来写入的值。

为了证明两个 timestamp 的大小关系，需要一个借位辅助变量：

```text
borrow_var
```

这个变量只能是 0 或 1，所以加入 `boolean_vars`：

```rust
boolean_vars.push(borrow_var);
```

后面布局 boolean vars 时，它会进入 witness subtree，并生成：

```text
borrow_var^2 - borrow_var = 0
```

注意：

```text
read_timestamp_low/high 放 memory subtree。
borrow_var 放 witness subtree。
```

因为 read timestamp 是 memory argument 的数据列，而 borrow 是证明 read < write 的辅助 witness。

---

## 7.6 布局 read value

源码：

```rust
let read_value = layout_memory_subtree_multiple_variables(
    &mut memory_tree_offset,
    memory_query.read_value,
    &mut all_variables_to_place,
    &mut layout,
);
```

`memory_query.read_value` 是当前访问读到的值。

不同 slot 下含义不同：

```text
slot0:
  rs1 read value。

slot1:
  rs2 read value 或 RAM read value。

slot2:
  rd old value 或 RAM old value。
```

这个值是 32-bit，所以一般拆成：

```text
read_value_low16
read_value_high16
```

当前代码把这组变量放进 memory subtree。

例如当前 `memory_tree_offset = 8`：

```text
read_value_low16  -> MemorySubtree(8)
read_value_high16 -> MemorySubtree(9)
memory_tree_offset = 10
```

返回的 `read_value` 是列范围。

---

## 7.7 根据 query 类型布局 address

接着源码：

```rust
let address = match memory_query.query_type {
    ShuffleRamQueryType::RegisterOnly { register_index } => {
        let register_index = layout_memory_subtree_variable(
            &mut memory_tree_offset,
            register_index,
            &mut all_variables_to_place,
            &mut layout,
        );

        ShuffleRamAddress::RegisterOnly(RegisterOnlyAccessAddress {
            register_index,
        })
    }
    ShuffleRamQueryType::RegisterOrRam {
        is_register,
        address,
    } => {
        let is_register = layout_memory_subtree_variable(
            &mut memory_tree_offset,
            is_register.get_variable().unwrap(),
            &mut all_variables_to_place,
            &mut layout,
        );
        let address = layout_memory_subtree_multiple_variables(
            &mut memory_tree_offset,
            address,
            &mut all_variables_to_place,
            &mut layout,
        );

        ShuffleRamAddress::RegisterOrRam(RegisterOrRamAccessAddress {
            is_register,
            address,
        })
    }
};
```

这里处理访问地址。

main RISC-V 的 3 个 slot 地址类型不完全一样。

### 7.7.1 RegisterOnly

```rust
ShuffleRamQueryType::RegisterOnly { register_index }
```

这种访问只能是寄存器访问。

main RISC-V 中 slot0 是这种情况：

```text
slot0:
  读取 rs1。
  地址就是 rs1 的 register index。
```

例如：

```text
rs1 = x1
register_index = 1
```

这里调用：

```rust
layout_memory_subtree_variable(...)
```

把 `register_index` 这个变量放进 memory subtree 的一列。

因为寄存器编号小于 32，所以它不需要两个 16-bit limb，一列就够。

返回：

```rust
ShuffleRamAddress::RegisterOnly(RegisterOnlyAccessAddress {
    register_index,
})
```

这里的 `register_index` 已经不是原始 `Variable`，而是 memory subtree 的列位置描述。

### 7.7.2 RegisterOrRam

```rust
ShuffleRamQueryType::RegisterOrRam {
    is_register,
    address,
}
```

这种访问可能是寄存器，也可能是 RAM。

main RISC-V 中 slot1 和 slot2 都可能是这种情况。

slot1：

```text
普通算术指令:
  slot1 = rs2 register read。

load 指令:
  slot1 = RAM read。
```

slot2：

```text
普通算术指令:
  slot2 = rd register write。

store 指令:
  slot2 = RAM write。
```

所以需要两个东西：

```text
is_register:
  1 表示这个访问是 register。
  0 表示这个访问是 RAM。

address:
  如果 is_register = 1，则 address 表示 register index。
  如果 is_register = 0，则 address 表示 RAM address。
```

源码先布局 `is_register`：

```rust
let is_register = layout_memory_subtree_variable(
    &mut memory_tree_offset,
    is_register.get_variable().unwrap(),
    &mut all_variables_to_place,
    &mut layout,
);
```

`is_register` 是一个 boolean 变量，但它被放在 memory subtree，因为 memory argument 需要知道这条访问是 register 还是 RAM。

然后布局 `address`：

```rust
let address = layout_memory_subtree_multiple_variables(
    &mut memory_tree_offset,
    address,
    &mut all_variables_to_place,
    &mut layout,
);
```

`address` 是 32-bit，所以通常拆成两个 16-bit limb，占两列。

最后返回：

```rust
ShuffleRamAddress::RegisterOrRam(RegisterOrRamAccessAddress {
    is_register,
    address,
})
```

这个对象记录了：

```text
is_register flag 在 memory subtree 哪一列；
address low/high 在 memory subtree 哪些列。
```

---

## 7.8 判断当前 query 是 readonly 还是 write

接着源码：

```rust
let query_columns = if memory_query.is_readonly() {
    assert_eq!(memory_query.read_value, memory_query.write_value);

    let query_columns = ShuffleRamQueryReadColumns {
        in_cycle_write_index: memory_query.local_timestamp_in_cycle as u32,
        address,
        read_timestamp,
        read_value,
    };

    ShuffleRamQueryColumns::Readonly(query_columns)
} else {
    let write_value = layout_memory_subtree_multiple_variables(
        &mut memory_tree_offset,
        memory_query.write_value,
        &mut all_variables_to_place,
        &mut layout,
    );

    let query_columns = ShuffleRamQueryWriteColumns {
        in_cycle_write_index: memory_query.local_timestamp_in_cycle as u32,
        address,
        read_timestamp,
        read_value,
        write_value,
    };

    ShuffleRamQueryColumns::Write(query_columns)
};
```

### 7.8.1 readonly query

如果：

```rust
memory_query.is_readonly()
```

说明这条访问只读不写。

例如：

```text
slot0 读取 rs1。
slot1 在 ADD 中读取 rs2。
slot1 在 LOAD 中读取 RAM。
```

readonly query 不需要单独的 write value。

代码检查：

```rust
assert_eq!(memory_query.read_value, memory_query.write_value);
```

这表示：

```text
对于 readonly 访问，read_value 和 write_value 是同一个变量。
```

然后构造：

```rust
ShuffleRamQueryReadColumns {
    in_cycle_write_index,
    address,
    read_timestamp,
    read_value,
}
```

字段含义：

```text
in_cycle_write_index:
  当前访问在 cycle 内的顺序。
  slot0 = 0，slot1 = 1，slot2 = 2。

address:
  访问地址的列位置。

read_timestamp:
  读取历史 timestamp 的列位置。

read_value:
  读取值的列位置。
```

### 7.8.2 write query

如果不是 readonly，就是 write query。

例如：

```text
slot2 在 ADD 中写 rd。
slot2 在 STORE 中写 RAM。
```

write query 需要额外布局 `write_value`：

```rust
let write_value = layout_memory_subtree_multiple_variables(
    &mut memory_tree_offset,
    memory_query.write_value,
    &mut all_variables_to_place,
    &mut layout,
);
```

`write_value` 是本次访问写入的新值，通常也是 32-bit，拆成两个 16-bit limb。

然后构造：

```rust
ShuffleRamQueryWriteColumns {
    in_cycle_write_index,
    address,
    read_timestamp,
    read_value,
    write_value,
}
```

比 readonly 多了：

```text
write_value:
  新写入值的列位置。
```

### 7.8.3 push 到 shuffle_ram_access_sets

源码：

```rust
shuffle_ram_access_sets.push(query_columns);
```

`shuffle_ram_access_sets` 最后会放进 `MemorySubtree`。

它保存 main RISC-V 3 个 slot 的列布局：

```text
shuffle_ram_access_sets[0] -> slot0 的 address / read_timestamp / read_value 列位置
shuffle_ram_access_sets[1] -> slot1 的 address / read_timestamp / read_value / maybe write_value 列位置
shuffle_ram_access_sets[2] -> slot2 的 address / read_timestamp / read_value / write_value 列位置
```

第六章 witness 阶段的 `process_shuffle_ram_accesses` 会根据这个布局，把真实 cycle 数据写入 `memory_row`。

---

## 8. 构造 MemorySubtree

main RISC-V 分支最后生成 memory subtree 描述：

```rust
let shuffle_ram_inits_and_teardowns = ShuffleRamInitAndTeardownLayout {
    lazy_init_addresses_columns,
    lazy_teardown_values_columns,
    lazy_teardown_timestamps_columns,
};

let memory_subtree_placement = MemorySubtree {
    shuffle_ram_inits_and_teardowns: Some(shuffle_ram_inits_and_teardowns),
    shuffle_ram_access_sets,
    delegation_request_layout,
    delegation_processor_layout: None,
    batched_ram_accesses: vec![],
    register_and_indirect_accesses: vec![],
    total_width: memory_tree_offset,
};
```

### 8.1 ShuffleRamInitAndTeardownLayout

```rust
let shuffle_ram_inits_and_teardowns = ShuffleRamInitAndTeardownLayout {
    lazy_init_addresses_columns,
    lazy_teardown_values_columns,
    lazy_teardown_timestamps_columns,
};
```

这个对象记录 lazy init / teardown 的 memory columns：

```text
lazy_init_addresses_columns:
  lazy init address 在 memory subtree 的列范围。

lazy_teardown_values_columns:
  teardown value 在 memory subtree 的列范围。

lazy_teardown_timestamps_columns:
  teardown timestamp 在 memory subtree 的列范围。
```

它不保存真实 address、value、timestamp，只保存列位置。

### 8.2 MemorySubtree 字段

```rust
let memory_subtree_placement = MemorySubtree {
    shuffle_ram_inits_and_teardowns: Some(shuffle_ram_inits_and_teardowns),
    shuffle_ram_access_sets,
    delegation_request_layout,
    delegation_processor_layout: None,
    batched_ram_accesses: vec![],
    register_and_indirect_accesses: vec![],
    total_width: memory_tree_offset,
};
```

字段含义：

```text
shuffle_ram_inits_and_teardowns:
  main RISC-V 有 lazy init / teardown，所以是 Some(...)

shuffle_ram_access_sets:
  main RISC-V 每行 3 个 shuffle RAM query 的列布局。

delegation_request_layout:
  main RISC-V 如果会发 delegation request，这里记录 request 相关列。
  delegation 内部不在本节展开。

delegation_processor_layout:
  main RISC-V 自己不是 delegation processor，所以这里是 None。

batched_ram_accesses:
  当前 main RISC-V 分支为空。

register_and_indirect_accesses:
  当前 main RISC-V 分支为空。

total_width:
  memory subtree 总列数。
  等于 memory_tree_offset 最终值。
```

这个 `MemorySubtree` 会进入最终：

```text
CompiledCircuitArtifact.memory_layout
```

第六章 `evaluate_witness` 会读取：

```text
compiled_circuit.memory_layout.total_width
```

来确定 `exec_trace` 中 memory 部分需要多少列。

---

## 9. 开始布局 witness subtree：multiplicity columns

memory subtree 布局完成后，代码开始布局 witness subtree。

源码：

```rust
let mut witness_tree_offset = 0;
let multiplicities_columns_for_range_check_16 =
    ColumnSet::layout_at(&mut witness_tree_offset, 1);
let multiplicities_columns_for_timestamp_range_check =
    ColumnSet::layout_at(&mut witness_tree_offset, 1);

let multiplicities_columns_for_generic_lookup = ColumnSet::layout_at(
    &mut witness_tree_offset,
    num_required_tuples_for_generic_lookup_setup,
);
```

### 9.1 witness_tree_offset 是什么

`witness_tree_offset` 是 witness subtree 的列分配游标。

它和 `memory_tree_offset` 类似，但管理的是 witness subtree：

```text
memory_tree_offset:
  分配 memory subtree columns。

witness_tree_offset:
  分配 witness subtree columns。
```

初始化：

```rust
let mut witness_tree_offset = 0;
```

表示 witness subtree 当前还没有分配任何列。

### 9.2 为什么先放 multiplicity columns

lookup argument 需要证明：

```text
witness 中实际查询 fixed table 的次数
和
setup table 中固定表行的多重集合
匹配。
```

因此 witness trace 中需要放 multiplicity 列，记录每个 table row 被查询了多少次。

这里先分配三类 multiplicity columns。

### 9.3 range_check_16 multiplicity

```rust
let multiplicities_columns_for_range_check_16 =
    ColumnSet::layout_at(&mut witness_tree_offset, 1);
```

这一列记录 16-bit range check 表的 multiplicity。

第六章 `postprocess_multiplicities` 会把统计结果写进这列。

### 9.4 timestamp range check multiplicity

```rust
let multiplicities_columns_for_timestamp_range_check =
    ColumnSet::layout_at(&mut witness_tree_offset, 1);
```

这一列记录 timestamp range check 表的 multiplicity。

timestamp 比较会产生 range check 查询，所以也要记录 multiplicity。

### 9.5 generic lookup multiplicity

```rust
let multiplicities_columns_for_generic_lookup = ColumnSet::layout_at(
    &mut witness_tree_offset,
    num_required_tuples_for_generic_lookup_setup,
);
```

generic lookup 包括：

```text
RomRead
OpTypeBitmask
SpecialCSRProperties
RomAddressSpaceSeparator
And
ZeroEntry
QuickDecodeDecompositionCheck...
其他 fixed lookup tables
```

`num_required_tuples_for_generic_lookup_setup` 是前面根据：

```text
total_tables_size / (trace_len - 1)
```

算出来的组数。

setup trace 里 generic lookup table rows 分几组放，witness multiplicity columns 也要有对应的组数。

---

## 10. 布局 range check 变量

接着处理 `range_check_expressions`。

源码：

```rust
for range_check in range_check_expressions.iter() {
    let RangeCheckQuery { input, width } = range_check;
    let LookupInput::Variable(..) = input else {
        unimplemented!()
    };
    assert!(
        *width == LARGE_RANGE_CHECK_TABLE_WIDTH || *width == SMALL_RANGE_CHECK_TABLE_WIDTH
    );
}
```

### 10.1 range_check_expressions 是什么

`range_check_expressions` 来自 `CircuitOutput`。

前面构造约束时，如果某个变量需要限制在某个 bit 宽度内，会加入 range check query。

例如：

```text
pc_low 是 16-bit
tmp_low_var 是 16-bit
tmp_high_var 是 16-bit
某些 byte 分解变量是 8-bit
```

这些变量会进入 `range_check_expressions`。

### 10.2 这里只支持 Variable 输入

源码要求：

```rust
let LookupInput::Variable(..) = input else {
    unimplemented!()
};
```

表示这里暂时只处理：

```text
RangeCheckQuery {
  input: LookupInput::Variable(variable),
  width: ...
}
```

暂时不处理复杂表达式形式的 range check。

### 10.3 small 和 large

```rust
assert!(
    *width == LARGE_RANGE_CHECK_TABLE_WIDTH || *width == SMALL_RANGE_CHECK_TABLE_WIDTH
);
```

只允许两类宽度：

```text
SMALL_RANGE_CHECK_TABLE_WIDTH:
  小 range check，通常对应 8-bit。

LARGE_RANGE_CHECK_TABLE_WIDTH:
  大 range check，通常对应 16-bit。
```

### 10.4 把 8-bit 和 16-bit range check 分开

源码：

```rust
let range_check_8_iter = range_check_expressions
    .iter()
    .filter(|el| el.width == SMALL_RANGE_CHECK_TABLE_WIDTH);
let range_check_16_iter = range_check_expressions
    .iter()
    .filter(|el| el.width == LARGE_RANGE_CHECK_TABLE_WIDTH);

let num_range_check_8 = range_check_8_iter.clone().count();
let num_range_check_16 = range_check_16_iter.clone().count();
```

这里把 range check query 分成两组：

```text
8-bit range check variables
16-bit range check variables
```

然后分别布局。

---

## 10.5 布局 8-bit range check variables

源码：

```rust
let range_check_8_columns: ColumnSet<1> =
    ColumnSet::layout_at(&mut witness_tree_offset, num_range_check_8);
let range_check_8_columns_it = range_check_8_columns.iter();

for (input, mut layout_part) in range_check_8_iter.zip(range_check_8_columns_it) {
    let LookupInput::Variable(input) = input.input else {
        unimplemented!()
    };
    let offset = layout_part.next().unwrap();
    let _place = layout_witness_subtree_variable_at_column(
        offset,
        input,
        &mut all_variables_to_place,
        &mut layout,
    );
}
```

执行步骤：

```text
1. 给所有 8-bit range check variables 分配一段 witness columns。
2. 遍历每个 8-bit range check query。
3. 取出其中的 Variable。
4. 把这个 Variable 放到指定 witness column。
5. 更新 layout。
6. 从 all_variables_to_place 删除该 Variable。
```

例如：

```text
Variable(50) 需要 8-bit range check。
```

如果分配到 witness subtree 第 10 列：

```text
Variable(50) -> WitnessSubtree(10)
```

之后 layout 里会记录：

```text
layout[Variable(50)] = ColumnAddress::WitnessSubtree(10)
```

---

## 10.6 布局 16-bit range check variables，并生成 lookup expression

源码：

```rust
let mut range_check_16_lookup_expressions = vec![];

let range_check_16_columns: ColumnSet<1> =
    ColumnSet::layout_at(&mut witness_tree_offset, num_range_check_16);

for (range_check, layout_part) in range_check_16_iter.zip(range_check_16_columns.iter()) {
    let RangeCheckQuery { input, .. } = range_check;
    let LookupInput::Variable(variable) = input else {
        unimplemented!()
    };
    let mut layout_part = layout_part;
    let offset = layout_part.next().unwrap();
    let place = layout_witness_subtree_variable_at_column(
        offset,
        *variable,
        &mut all_variables_to_place,
        &mut layout,
    );
    let lookup_expr = LookupExpression::Variable(place);
    range_check_16_lookup_expressions.push(lookup_expr)
}
```

16-bit range check 也会分配 witness columns。

区别是，这里还会生成：

```text
LookupExpression::Variable(place)
```

也就是告诉后面的 lookup argument：

```text
这个 witness column 里的值，需要去 16-bit range table 里查。
```

例如：

```text
pc_low -> WitnessSubtree(20)
```

会生成：

```text
LookupExpression::Variable(WitnessSubtree(20))
```

后面 witness 阶段会统计：

```text
pc_low 实际值查询了 16-bit range table 的哪一行。
```

### 10.7 pc_low 的例子

如果前面构造约束时有：

```text
RangeCheckQuery(pc_low, 16)
```

那么在这里：

```text
1. compile_inner 遍历 range_check_16_iter。
2. 找到 pc_low 这个 Variable。
3. 给 pc_low 分配 witness subtree column。
4. 插入 layout:
   pc_low -> WitnessSubtree(offset)
5. 生成:
   LookupExpression::Variable(WitnessSubtree(offset))
6. 加入 range_check_16_lookup_expressions。
```

这里仍然没有检查 `pc_low` 的真实值。真实值在 witness 阶段填入。

---

## 11. 布局 boolean vars，并生成 boolean constraint

源码：

```rust
let mut constraints = constraints;
// normalize again just in case
for (el, _) in constraints.iter_mut() {
    el.normalize();
}

let mut compiled_quadratic_terms = vec![];
let mut compiled_linear_terms = vec![];

let mut boolean_vars_start = witness_tree_offset;
let num_boolean_vars = boolean_vars.len();
let boolean_vars_columns_range =
    ColumnSet::layout_at(&mut boolean_vars_start, num_boolean_vars);

// first we can layout booleans
for variable in boolean_vars.into_iter() {
    assert!(
        all_variables_to_place.remove(&variable),
        "variable {:?} was already placed",
        variable
    );
    let place = ColumnAddress::WitnessSubtree(witness_tree_offset);
    layout.insert(variable, place);
    witness_tree_offset += 1;

    let mut quadratic_terms = vec![];
    let mut linear_terms = vec![];
    quadratic_terms.push((F::ONE, place, place));
    linear_terms.push((F::MINUS_ONE, place));

    let compiled_term = CompiledDegree2Constraint {
        quadratic_terms: quadratic_terms.into_boxed_slice(),
        linear_terms: linear_terms.into_boxed_slice(),
        constant_term: F::ZERO,
    };

    compiled_quadratic_terms.push(compiled_term);
}
```

### 11.1 boolean_vars 是什么

`boolean_vars` 是所有需要限制为 boolean 的变量集合。

来源包括：

```text
1. Machine 代码中显式要求某些变量是 boolean。
2. compiler 自己创建的 borrow 变量。
3. opcode flag、is_register flag、carry flag 等。
```

当前阶段要做两件事：

```text
1. 给每个 boolean variable 分配 witness column。
2. 给每个 boolean variable 生成 boolean constraint。
```

### 11.2 给 boolean variable 分配 witness column

这段代码：

```rust
assert!(
    all_variables_to_place.remove(&variable),
    "variable {:?} was already placed",
    variable
);
let place = ColumnAddress::WitnessSubtree(witness_tree_offset);
layout.insert(variable, place);
witness_tree_offset += 1;
```

执行顺序：

```text
1. 从 all_variables_to_place 删除该变量。
   如果删除失败，说明变量已经被放置过，直接报错。

2. 创建列地址：
   ColumnAddress::WitnessSubtree(witness_tree_offset)

3. 写入 layout：
   variable -> WitnessSubtree(witness_tree_offset)

4. witness_tree_offset += 1
```

例如：

```text
Variable(80) 是 boolean。
当前 witness_tree_offset = 30。
```

布局后：

```text
Variable(80) -> WitnessSubtree(30)
witness_tree_offset = 31
```

### 11.3 生成 b^2 - b = 0

源码：

```rust
quadratic_terms.push((F::ONE, place, place));
linear_terms.push((F::MINUS_ONE, place));
```

这生成的约束是：

```text
1 * place * place - 1 * place = 0
```

也就是：

```text
b^2 - b = 0
```

在 field 中，这个约束只允许：

```text
b = 0
或
b = 1
```

所以它强制该变量是 boolean。

### 11.4 生成 CompiledDegree2Constraint

源码：

```rust
let compiled_term = CompiledDegree2Constraint {
    quadratic_terms: quadratic_terms.into_boxed_slice(),
    linear_terms: linear_terms.into_boxed_slice(),
    constant_term: F::ZERO,
};

compiled_quadratic_terms.push(compiled_term);
```

这里生成的是已经使用 `ColumnAddress` 的约束，不再是 `Variable` 约束。

例如：

```text
Variable(80) -> WitnessSubtree(30)
```

boolean 约束变成：

```text
WitnessSubtree(30)^2 - WitnessSubtree(30) = 0
```

这个约束会进入最终：

```text
CompiledCircuitArtifact.degree_2_constraints
```

---

## 12. 编译普通 lookup query

接下来处理 `lookups`。

源码：

```rust
let mut width_3_lookups = vec![];

for lookup_query in lookups {
    let LookupQuery { row, table } = lookup_query;
    assert_eq!(row.len(), 3);

    let mut input_columns = Vec::with_capacity(3);
    for el in row.into_iter() {
        match el {
            LookupInput::Variable(single_var) => {
                let place = if let Some(place) = layout.get(&single_var) {
                    *place
                } else {
                    let column = layout_witness_subtree_variable(
                        &mut witness_tree_offset,
                        single_var,
                        &mut all_variables_to_place,
                        &mut layout,
                    );
                    let place = ColumnAddress::WitnessSubtree(column.start);

                    place
                };

                let lookup_expr = LookupExpression::Variable(place);
                input_columns.push(lookup_expr);
            }
            LookupInput::Expression {
                linear_terms,
                constant_coeff,
            } => {
                ...
            }
        }
    }

    let table_index = match table {
        LookupQueryTableType::Constant(constant) => TableIndex::Constant(constant),
        LookupQueryTableType::Variable(variable) => {
            ...
        }
    };

    let lookup = LookupSetDescription {
        input_columns: input_columns.try_into().unwrap(),
        table_index,
    };
    width_3_lookups.push(lookup);
}
```

### 12.1 lookups 是什么

`lookups` 来自 `CircuitOutput`。

在 `Machine::describe_state_transition` 阶段，电路会登记很多 fixed table lookup，例如：

```text
RomRead:
  根据 ROM address 查 opcode low/high。

OpTypeBitmask:
  根据 opcode/funct3/funct7 查 decoder flags。

RomAddressSpaceSeparator:
  根据地址高位判断 ROM/RAM 范围。

Range/bit/decode 辅助表:
  检查某些分解或 bit 关系。
```

这些 lookup 在 `CircuitOutput` 中还是抽象形式：

```text
LookupQuery {
  row: Vec<LookupInput>,
  table: LookupQueryTableType
}
```

其中 `row` 里的元素可能是：

```text
LookupInput::Variable(Variable)
LookupInput::Expression { linear_terms, constant_coeff }
```

`compile_inner` 要把它们变成基于列地址的 lookup 描述。

---

## 12.2 width_3_lookups 是什么

源码要求：

```rust
assert_eq!(row.len(), 3);
```

说明这里处理的是 width-3 lookup。

这里的 `row.len() == 3` 表示 lookup query 有 3 个主要 field 元素。

再加上 table id 后，dump 到 setup table 时可能会有统一编码格式。

`width_3_lookups` 保存编译后的 lookup 描述，最后进入：

```text
WitnessSubtree.width_3_lookups
```

---

## 12.3 LookupInput::Variable 的处理

源码：

```rust
LookupInput::Variable(single_var) => {
    let place = if let Some(place) = layout.get(&single_var) {
        *place
    } else {
        let column = layout_witness_subtree_variable(
            &mut witness_tree_offset,
            single_var,
            &mut all_variables_to_place,
            &mut layout,
        );
        let place = ColumnAddress::WitnessSubtree(column.start);

        place
    };

    let lookup_expr = LookupExpression::Variable(place);
    input_columns.push(lookup_expr);
}
```

执行逻辑：

```text
1. 如果这个 Variable 已经在 layout 里：
   直接拿它的 ColumnAddress。

2. 如果这个 Variable 还没有被放置：
   把它放进 witness subtree。
   更新 layout。
   推进 witness_tree_offset。

3. 把 ColumnAddress 包装成 LookupExpression::Variable。
4. 加入 input_columns。
```

例子：

```text
low_opcode_var -> WitnessSubtree(40)
```

会变成：

```text
LookupExpression::Variable(WitnessSubtree(40))
```

---

## 12.4 LookupInput::Expression 的处理

源码省略部分逻辑大概是：

```rust
LookupInput::Expression {
    linear_terms,
    constant_coeff,
} => {
    let mut compiled_linear_terms = vec![];
    for (coeff, var) in linear_terms.iter() {
        let place = if let Some(place) = layout.get(var) {
            *place
        } else {
            let column = layout_witness_subtree_variable(
                &mut witness_tree_offset,
                *var,
                &mut all_variables_to_place,
                &mut layout,
            );
            let place = ColumnAddress::WitnessSubtree(column.start);

            place
        };
        compiled_linear_terms.push((*coeff, place));
    }
    let compiled_constraint = CompiledDegree1Constraint {
        linear_terms: compiled_linear_terms.into_boxed_slice(),
        constant_term: constant_coeff,
    };
    let lookup_expr = LookupExpression::Expression(compiled_constraint);
    input_columns.push(lookup_expr);
}
```

`LookupInput::Expression` 表示 lookup 输入不是单个变量，而是一个线性表达式。

例如：

```text
rom_address = pc_low + 2^16 * rom_address_low
```

在 `CircuitOutput` 里可能表示为：

```text
LookupInput::Expression {
  linear_terms: [
    (1, pc_low),
    (2^16, rom_address_low)
  ],
  constant_coeff: 0
}
```

`compile_inner` 会把每个变量替换成列地址：

```text
pc_low -> WitnessSubtree(10)
rom_address_low -> WitnessSubtree(11)
```

然后得到：

```text
LookupExpression::Expression(
  1 * WitnessSubtree(10)
  + 2^16 * WitnessSubtree(11)
)
```

注意：

```text
这里不会计算 rom_address 的真实值。
这里只有列地址和线性表达式描述。
```

---

## 12.5 table id 的处理

源码：

```rust
let table_index = match table {
    LookupQueryTableType::Constant(constant) => TableIndex::Constant(constant),
    LookupQueryTableType::Variable(variable) => {
        let column = layout_witness_subtree_variable(
            &mut witness_tree_offset,
            variable,
            &mut all_variables_to_place,
            &mut layout,
        );
        let place = ColumnAddress::WitnessSubtree(column.start);
        TableIndex::Variable(place)
    }
};
```

lookup table 可以是两种：

```text
Constant table:
  表 id 在 compile 阶段已知。
  例如 RomRead、OpTypeBitmask。

Variable table:
  表 id 本身来自 witness 变量。
```

main RISC-V 常见 fixed table lookup 是 constant table：

```text
TableIndex::Constant(TableType::RomRead)
TableIndex::Constant(TableType::OpTypeBitmask)
```

如果 table id 是变量，compiler 会把这个变量也放进 witness subtree。

### 12.6 生成 LookupSetDescription

源码：

```rust
let lookup = LookupSetDescription {
    input_columns: input_columns.try_into().unwrap(),
    table_index,
};
width_3_lookups.push(lookup);
```

`LookupSetDescription` 是编译后的 lookup 描述：

```text
input_columns:
  lookup row 的 3 个输入/输出表达式，已经从 Variable 转成 ColumnAddress。

table_index:
  这次 lookup 查询哪张表。
```

它会进入：

```text
CompiledCircuitArtifact.witness_layout.width_3_lookups
```

第六章 witness 阶段会根据它：

```text
1. 从 witness row 读取 lookup 输入。
2. 根据 table_index 找表。
3. 通过 TableDriver 查表。
4. 写 lookup_mapping。
5. 累加 multiplicity。
```

---

## 12.7 RomRead lookup 的例子

在构造约束阶段，RomRead lookup 可能是：

```text
LookupQuery {
  row = [
    rom_address_expr,
    opcode_low_var,
    opcode_high_var
  ],
  table = RomRead
}
```

进入 `compile_inner` 后：

```text
rom_address_expr:
  被编译成 LookupExpression::Expression(...ColumnAddress...)

opcode_low_var:
  被编译成 LookupExpression::Variable(WitnessSubtree(...))

opcode_high_var:
  被编译成 LookupExpression::Variable(WitnessSubtree(...))

table:
  被编译成 TableIndex::Constant(RomRead)
```

最终变成：

```text
LookupSetDescription {
  input_columns = [
    LookupExpression::Expression(rom_address compiled expression),
    LookupExpression::Variable(opcode_low column),
    LookupExpression::Variable(opcode_high column),
  ],
  table_index = TableIndex::Constant(RomRead)
}
```

这里仍然没有查 `RomRead` 表。这里只告诉后续 witness/prove 阶段：

```text
这几个列上的值必须构成 RomRead 表中的一行。
```

---

## 13. 编译 timestamp range check expressions

接下来处理 timestamp 比较相关的 range check expressions。

源码：

```rust
let mut compiled_timestamp_comparison_expressions = vec![];

for input in timestamp_range_check_expressions_to_compile.into_iter() {
    let LookupInput::Expression {
        linear_terms,
        constant_coeff,
    } = input
    else {
        panic!()
    };
    let mut compiled_linear_terms = vec![];
    for (coeff, var) in linear_terms.iter() {
        let place = layout
            .get(var)
            .copied()
            .expect("all variables must be already placed");
        compiled_linear_terms.push((*coeff, place));
    }
    let compiled_constraint = CompiledDegree1Constraint {
        linear_terms: compiled_linear_terms.into_boxed_slice(),
        constant_term: constant_coeff,
    };
    let lookup_expr = LookupExpression::Expression(compiled_constraint);
    compiled_timestamp_comparison_expressions.push(lookup_expr);
}
```

### 13.1 timestamp_range_check_expressions_to_compile 是什么

前面布局 memory accesses 时，需要证明：

```text
read_timestamp < write_timestamp
```

为了证明这个不等式，会构造一些表达式，并要求这些表达式落在 timestamp range table 中。

这些表达式暂时放在：

```text
timestamp_range_check_expressions_to_compile
```

等相关变量都已经有 `ColumnAddress` 后，这里再把它们编译成：

```text
LookupExpression::Expression(CompiledDegree1Constraint)
```

### 13.2 为什么要求变量必须已经 placed

源码：

```rust
let place = layout
    .get(var)
    .copied()
    .expect("all variables must be already placed");
```

这里不再给变量新分配列，而是要求它们已经在前面布局完成。

原因是 timestamp 比较相关变量大多来自：

```text
memory subtree 的 read timestamp columns
witness subtree 的 borrow variables
setup subtree 的 timestamp columns
```

这些布局在前面已经确定了。

### 13.3 生成 compiled timestamp lookup expression

每个表达式从：

```text
coeff_0 * Variable(a)
+ coeff_1 * Variable(b)
+ constant
```

变成：

```text
coeff_0 * ColumnAddress(a)
+ coeff_1 * ColumnAddress(b)
+ constant
```

然后包装成：

```text
LookupExpression::Expression(...)
```

最后加入：

```text
compiled_timestamp_comparison_expressions
```

这些表达式后面会进入：

```text
WitnessSubtree.timestamp_range_check_lookup_expressions
```

---

## 13.4 main RISC-V shuffle RAM timestamp 的特殊处理

源码片段：

```rust
let offset_for_special_shuffle_ram_timestamps_range_check_expressions = {
    let offset_for_special_shuffle_ram_timestamps_range_check_expressions =
        compiled_timestamp_comparison_expressions.len();

    for data in shuffle_ram_extra_range_check_16_partial_sets.into_iter() {
        let ShuffleRamTimestampComparisonPartialData {
            intermediate_borrow,
            read_timestamp,
            local_timestamp_in_cycle,
        } = data;
        ...
        let write_low_place =
            ColumnAddress::SetupSubtree(setup_layout.timestamp_setup_columns.start());
        ...
        let write_high_place = ColumnAddress::SetupSubtree(
            setup_layout.timestamp_setup_columns.start() + 1,
        );
        ...
    }

    offset_for_special_shuffle_ram_timestamps_range_check_expressions
};
```

这里出现了：

```text
ColumnAddress::SetupSubtree(...)
```

说明这些表达式不只读取 witness/memory columns，还读取 setup trace 中的 timestamp setup columns。

### 13.4.1 为什么 timestamp 比较会访问 setup subtree

main RISC-V 的 write timestamp 不是一个普通 witness 变量。

它由三部分组成：

```text
1. 当前 trace row 的基础 timestamp。
2. 当前 cycle 内的 local_timestamp_in_cycle。
3. 当前 circuit chunk 的 sequence offset。
```

其中基础 timestamp 的 low/high 部分来自 setup trace 的 timestamp columns。

所以编译 timestamp range check expression 时，需要引用：

```text
setup_layout.timestamp_setup_columns.start()
setup_layout.timestamp_setup_columns.start() + 1
```

也就是：

```text
SetupSubtree(timestamp_low_column)
SetupSubtree(timestamp_high_column)
```

### 13.4.2 local_timestamp_in_cycle 的作用

每个 cycle 里有 3 个访问：

```text
slot0 local_timestamp_in_cycle = 0
slot1 local_timestamp_in_cycle = 1
slot2 local_timestamp_in_cycle = 2
```

源码在 low 部分表达式中减去：

```text
local_timestamp_in_cycle
```

这样同一 row 内的不同访问会有不同的 write timestamp。

### 13.4.3 offset_for_special_shuffle_ram_timestamps_range_check_expressions

这个变量记录：

```text
shuffle RAM timestamp 特殊表达式在 timestamp_range_check_lookup_expressions 里的起始位置。
```

第六章 witness 阶段会用这个 offset 来区分哪些 timestamp range check 表达式需要额外加入 circuit sequence 的高位贡献。

---

## 14. 优化掉部分变量到 OptimizedOut

接着代码尝试减少 witness columns。

源码核心逻辑：

```rust
let optimized_out_variables = {
    let initial_len = all_variables_to_place.len();
    let mut optimized_out_variables = vec![];
    let mut tried_variables = BTreeSet::new();
    'outer: loop {
        let mut to_remove: Option<(Variable, Vec<usize>, Vec<usize>)> = None;
        for variable in all_variables_to_place.iter() {
            ...
            for (constraint_id, (constraint, prevent_optimizations)) in
                constraints.iter().enumerate()
            {
                if *prevent_optimizations {
                    continue;
                }
                if constraint.degree() > 1 {
                    continue;
                }
                if constraint.degree_for_var(variable) == 0 {
                    continue;
                }
                defining_constraints.push((constraint_id, constraint));
            }
            ...
        }
        ...
    }
};
```

### 14.1 为什么要优化变量

有些变量不需要占用 witness column。

例如有一个线性约束：

```text
c = a + b
```

如果 `c` 只在少数约束中出现，可以把 `c` 用 `a + b` 替换掉。

这样：

```text
原来需要 c 这一列。
优化后 c 不占 trace 列。
witness 生成时临时算一下 c，放 scratch space。
```

这可以减少 witness trace 宽度。

### 14.2 哪些变量可以优化

源码筛选条件包括：

```text
1. 变量还在 all_variables_to_place 中。
   表示它还没有被分配列。

2. 存在线性 defining constraint。
   也就是可以通过一个一次约束表达出该变量。

3. constraint.degree() <= 1。
   只使用线性关系做替换。

4. 替换后其他约束 degree 不能超过 2。
   Airbender 当前约束编译目标是 degree 1 或 degree 2。

5. 该变量不能是 placeholder substitution 的目标变量。

6. 该变量不能是 state_input。

7. 该变量不能是 state_output。
```

### 14.3 为什么 placeholder substitution 不能优化

源码限制：

```rust
for (_, v) in substitutions.iter() {
    if v == variable {
        continue 'outer;
    }
}
```

`substitutions` 记录：

```text
Placeholder -> Variable
```

例如：

```text
PcInit -> pc_low_var / pc_high_var
ShuffleRamReadValue -> value limb variable
```

witness generator 会根据这个映射，把 oracle 提供的值写到对应变量位置。

如果把这些变量优化掉，placeholder 的写入目标会消失，所以不能优化。

### 14.4 为什么 state_input / state_output 不能优化

源码限制：

```rust
if state_input.contains(&variable) {
    continue;
}

if state_output.contains(&variable) {
    continue;
}
```

`state_input` 和 `state_output` 用于 chunk 边界：

```text
state_input:
  当前 chunk 第一行状态。

state_output:
  当前 chunk 倒数第一行状态。
```

它们后面要生成：

```text
public_inputs
state_linkage_constraints
```

如果优化掉，chunk 之间状态连接会找不到对应列。

### 14.5 替换约束

如果找到可优化变量，代码会尝试：

```rust
let defining_constraint = constraints[defining_constraint_idx].0.clone();
let mut expression =
    defining_constraint.express_variable(variable_to_optimize_out);
expression.normalize();
```

这表示：

```text
用 defining constraint 把 variable_to_optimize_out 表达成其他变量的线性表达式。
```

然后替换其他出现该变量的约束：

```rust
let rewritten_constraint = existing_constraint
    .clone()
    .substitute_variable(variable_to_optimize_out, expression.clone());
```

如果替换后 degree 超过 2：

```rust
if rewritten_constraint.degree() > 2 {
    can_be_optimized_out = false;
}
```

就不能优化。

### 14.6 成功优化后

成功后：

```rust
let existed = all_variables_to_place.remove(&variable_to_optimize_out);
assert!(existed);
optimized_out_variables.push(variable_to_optimize_out);
...
constraints = new_constraints;
```

表示：

```text
1. 从 all_variables_to_place 删除该变量。
2. 记录到 optimized_out_variables。
3. 删除 defining constraint。
4. 用替换后的 constraints 替换旧 constraints。
```

然后：

```rust
let scratch_space_size_for_witness_gen = optimized_out_variables.len();

let mut optimized_out_offset = 0;
for var in optimized_out_variables.into_iter() {
    layout.insert(var, ColumnAddress::OptimizedOut(optimized_out_offset));
    optimized_out_offset += 1;
}
```

这表示：

```text
被优化掉的变量不占 witness column。
它们进入 OptimizedOut(offset)。
```

例如：

```text
Variable(200) -> OptimizedOut(0)
Variable(250) -> OptimizedOut(1)
```

第六章 witness 阶段会分配：

```text
scratch_space_size_for_witness_gen = 2
```

然后 witness generator 可以在 scratch space 中临时保存这些值。

---

## 15. 放置剩余变量，并编译普通 constraints

优化完成后，仍然留在 `all_variables_to_place` 的变量都要放进 witness subtree。

源码：

```rust
let mut scratch_space_columns_start = witness_tree_offset;
let scratch_space_columns_range = ColumnSet::layout_at(
    &mut scratch_space_columns_start,
    all_variables_to_place.len(),
);

// and then we will just place all other variable
for variable in all_variables_to_place.into_iter() {
    layout.insert(variable, ColumnAddress::WitnessSubtree(witness_tree_offset));
    witness_tree_offset += 1;
}
```

### 15.1 剩余变量是什么

这些变量没有被放进 memory subtree，也不是 range check 专门放置的变量，也不是 boolean 专门放置的变量，也没有被优化掉。

它们通常是普通约束里需要的中间值。

这些变量直接按顺序放进 witness subtree。

例如：

```text
Variable(300) -> WitnessSubtree(100)
Variable(301) -> WitnessSubtree(101)
Variable(302) -> WitnessSubtree(102)
```

### 15.2 scratch_space_columns_range

源码中创建：

```rust
let scratch_space_columns_range = ColumnSet::layout_at(
    &mut scratch_space_columns_start,
    all_variables_to_place.len(),
);
```

这里的命名有点容易让初学者混淆。

它不是 `OptimizedOut` 的 scratch space。它记录的是最后一批普通 witness variables 的列范围，也会放进 `WitnessSubtree.scratch_space_columns_range`。

后面 witness layout 会保存这个范围。

### 15.3 编译普通 constraints

接着代码把普通约束从 `Variable` 形式改成 `ColumnAddress` 形式。

源码：

```rust
for (constraint, _) in constraints.into_iter() {
    assert!(constraint
        .terms
        .is_sorted_by(|a, b| a.degree() >= b.degree()));

    match constraint.degree() {
        2 => {
            let mut quadratic_terms = vec![];
            let mut linear_terms = vec![];
            let mut constant_term = F::ZERO;
            for term in constraint.terms.into_iter() {
                match term.degree() {
                    2 => {
                        let coeff = term.get_coef();
                        let [a, b] = term.as_slice() else { panic!() };
                        assert!(*a <= *b);
                        let a = layout.get(a).copied().unwrap();
                        let b = layout.get(b).copied().unwrap();
                        quadratic_terms.push((coeff, a, b));
                    }
                    1 => {
                        let coeff = term.get_coef();
                        let [a] = term.as_slice() else { panic!() };
                        let a = layout.get(a).copied().unwrap();
                        linear_terms.push((coeff, a));
                    }
                    0 => {
                        constant_term.add_assign(&term.get_coef());
                    }
                    _ => {
                        unreachable!()
                    }
                }
            }

            let compiled_term = CompiledDegree2Constraint {
                quadratic_terms: quadratic_terms.into_boxed_slice(),
                linear_terms: linear_terms.into_boxed_slice(),
                constant_term,
            };

            compiled_quadratic_terms.push(compiled_term);
        }
        1 => {
            ...
        }
        _ => {
            unreachable!()
        }
    }
}
```

### 15.4 Constraint<Variable> 到 CompiledDegree2Constraint

原始约束可能是：

```text
Variable(1) * Variable(2) + Variable(3) - 5 = 0
```

`layout` 里有：

```text
Variable(1) -> WitnessSubtree(10)
Variable(2) -> WitnessSubtree(11)
Variable(3) -> MemorySubtree(5)
```

编译后变成：

```text
WitnessSubtree(10) * WitnessSubtree(11)
+ MemorySubtree(5)
- 5 = 0
```

这就是：

```text
CompiledDegree2Constraint
```

它里面不再保存 `Variable`，只保存 `ColumnAddress`。

### 15.5 Constraint<Variable> 到 CompiledDegree1Constraint

如果约束 degree 是 1，例如：

```text
Variable(1) + Variable(2) - Variable(3) = 0
```

会生成：

```text
CompiledDegree1Constraint
```

里面保存的是：

```text
linear_terms:
  coeff + ColumnAddress

constant_term:
  常数项
```

### 15.6 为什么证明阶段需要 ColumnAddress

证明阶段面对的是 trace：

```text
row 0: column 0, column 1, column 2, ...
row 1: column 0, column 1, column 2, ...
```

它不能直接处理：

```text
Variable(123)
```

它必须知道：

```text
Variable(123) 在哪一类 trace 的第几列。
```

所以 `compile_inner` 这里把所有约束从：

```text
Constraint over Variable
```

改写成：

```text
CompiledConstraint over ColumnAddress
```

---

## 16. 生成 state linkage 和 public inputs

源码：

```rust
assert_eq!(state_input.len(), state_output.len());
let mut linking_constraints = vec![];
let mut public_inputs_first_row = vec![];
let mut public_inputs_one_row_before_last = vec![];
for (i, f) in state_input.into_iter().zip(state_output.into_iter()) {
    // final -> NEXT initial
    let i = layout.get(&i).expect("must be compiled");
    let f = layout.get(&f).expect("must be compiled");
    linking_constraints.push((*f, *i));
    public_inputs_first_row.push((BoundaryConstraintLocation::FirstRow, *i));
    public_inputs_one_row_before_last
        .push((BoundaryConstraintLocation::OneBeforeLastRow, *f));
}
```

### 16.1 state_input / state_output 是什么

`state_input` 和 `state_output` 来自 `compile_machine`。

`Machine::describe_state_transition` 返回一行状态转移的初始状态和最终状态。`compile_machine` 把它们收集到：

```text
CircuitOutput.state_input
CircuitOutput.state_output
```

对 main RISC-V 来说，这些状态包括：

```text
pc
寄存器相关状态
memory argument 边界相关状态
其他 machine state
```

具体字段不在本节展开。

### 16.2 为什么 state_input 和 state_output 长度必须相同

源码：

```rust
assert_eq!(state_input.len(), state_output.len());
```

因为每个输入状态变量都要对应一个输出状态变量。

例如：

```text
state_input[0]  对应 state_output[0]
state_input[1]  对应 state_output[1]
...
```

这样才能表达：

```text
当前 chunk 的 final state
连接到
下一个 chunk 的 initial state
```

### 16.3 linking_constraints

源码：

```rust
linking_constraints.push((*f, *i));
```

这里保存的是：

```text
(state_output_column, state_input_column)
```

含义：

```text
当前 chunk 的 state_output
要等于
下一个 chunk 的 state_input
```

这用于多 chunk 证明时连接状态。

### 16.4 public_inputs_first_row

源码：

```rust
public_inputs_first_row.push((BoundaryConstraintLocation::FirstRow, *i));
```

这表示：

```text
第一行的 state_input column 是 public input / boundary input。
```

第六章 witness 生成结束后，会从 `exec_trace` 第一行读取这些值。

### 16.5 public_inputs_one_row_before_last

源码：

```rust
public_inputs_one_row_before_last
    .push((BoundaryConstraintLocation::OneBeforeLastRow, *f));
```

这表示：

```text
倒数第一行真实 cycle 的 state_output column 是 public input / boundary output。
```

这里是 `OneBeforeLastRow`，不是 `LastRow`，因为 main RISC-V 每个 chunk 真实 cycle 只使用前 `trace_len - 1` 行。

### 16.6 main RISC-V 必须有 public inputs

源码：

```rust
let mut public_inputs = public_inputs_first_row;
public_inputs.extend(public_inputs_one_row_before_last);

if FOR_DELEGATION {
    assert!(public_inputs.is_empty());
} else {
    assert!(public_inputs.len() > 0);
}
```

main RISC-V 下 `FOR_DELEGATION = false`，所以要求：

```text
public_inputs.len() > 0
```

因为 main RISC-V 需要公开或边界约束初始状态和最终状态。

---

## 17. 编译 substitutions

源码：

```rust
let mut compiled_substitutions = Vec::with_capacity(substitutions.len());

for (k, v) in substitutions.iter() {
    let place = layout.get(&v).copied().expect("must be compiled");
    compiled_substitutions.push((*k, place));
}
```

### 17.1 substitutions 是什么

`substitutions` 来自 `CircuitOutput`。

它记录：

```text
Placeholder -> Variable
```

`Placeholder` 是 witness 阶段从 oracle 获取真实值的名字。

例如：

```text
Placeholder::PcInit
Placeholder::ShuffleRamReadValue(0)
Placeholder::ShuffleRamReadTimestamp(1)
```

在构造约束阶段，这些 placeholder 会绑定到某些 `Variable`。

例如：

```text
(Placeholder::PcInit, 0) -> pc_low_var
(Placeholder::PcInit, 1) -> pc_high_var
```

### 17.2 为什么要编译 substitutions

witness 阶段不能只知道：

```text
PcInit -> pc_low_var
```

它需要知道：

```text
PcInit -> 哪个 trace column
```

所以这里通过 `layout` 把 `Variable` 转成 `ColumnAddress`：

```rust
let place = layout.get(&v).copied().expect("must be compiled");
```

然后生成：

```text
Placeholder -> ColumnAddress
```

例如：

```text
(Placeholder::PcInit, 0) -> WitnessSubtree(10)
(Placeholder::PcInit, 1) -> WitnessSubtree(11)
```

这样第六章 witness generator 才能把 oracle 返回的 `pc` 值写到正确列。

---

## 18. 构造 WitnessSubtree

源码：

```rust
let witness_layout = WitnessSubtree {
    multiplicities_columns_for_range_check_16,
    multiplicities_columns_for_timestamp_range_check,
    multiplicities_columns_for_generic_lookup,
    range_check_8_columns,
    range_check_16_columns,
    width_3_lookups,
    range_check_16_lookup_expressions,
    timestamp_range_check_lookup_expressions: compiled_timestamp_comparison_expressions,
    offset_for_special_shuffle_ram_timestamps_range_check_expressions,
    boolean_vars_columns_range,
    scratch_space_columns_range,
    total_width: witness_tree_offset,
};
```

`WitnessSubtree` 是 witness subtree 的完整列布局描述。

它进入最终：

```text
CompiledCircuitArtifact.witness_layout
```

### 18.1 multiplicities_columns_for_range_check_16

```text
16-bit range check multiplicity 列位置。
```

第六章 witness 阶段会统计每个 16-bit 值被查询了多少次，并写入这些列。

### 18.2 multiplicities_columns_for_timestamp_range_check

```text
timestamp range check multiplicity 列位置。
```

用于 timestamp 比较相关的 range table multiplicity。

### 18.3 multiplicities_columns_for_generic_lookup

```text
generic fixed lookup table multiplicity 列位置。
```

用于：

```text
RomRead
OpTypeBitmask
RomAddressSpaceSeparator
SpecialCSRProperties
其他 fixed lookup tables
```

### 18.4 range_check_8_columns

```text
8-bit range check 变量所在列。
```

### 18.5 range_check_16_columns

```text
16-bit range check 变量所在列。
```

### 18.6 width_3_lookups

```text
所有 width-3 lookup 的编译后布局描述。
```

每个元素是：

```text
LookupSetDescription {
  input_columns,
  table_index,
}
```

### 18.7 range_check_16_lookup_expressions

```text
16-bit range check lookup expressions。
```

描述哪些 witness columns 要查 16-bit range table。

### 18.8 timestamp_range_check_lookup_expressions

```text
timestamp 比较相关的 range check expressions。
```

这些表达式可能同时引用：

```text
MemorySubtree
WitnessSubtree
SetupSubtree
```

### 18.9 offset_for_special_shuffle_ram_timestamps_range_check_expressions

```text
main RISC-V shuffle RAM timestamp 特殊表达式在 timestamp_range_check_lookup_expressions 里的起始位置。
```

第六章 witness 阶段用它处理 circuit sequence 对 timestamp high 部分的贡献。

### 18.10 boolean_vars_columns_range

```text
boolean variables 的列范围。
```

这些列对应的变量都有 `b^2 - b = 0` 约束。

### 18.11 scratch_space_columns_range

```text
最后一批普通 witness variables 的列范围。
```

### 18.12 total_width

```text
witness subtree 总列数。
```

第六章 `evaluate_witness` 会使用：

```text
compiled_circuit.witness_layout.total_width
```

来分割：

```text
exec_trace row = [witness_row | memory_row]
```

---

## 19. 构造 stage_2_layout 和 table_offsets

源码：

```rust
assert_eq!(
    setup_layout.generic_lookup_setup_columns.num_elements(),
    num_required_tuples_for_generic_lookup_setup
);

let stage_2_layout = LookupAndMemoryArgumentLayout::from_compiled_parts(
    &witness_layout,
    &memory_subtree_placement,
    &setup_layout,
);

let table_offsets = table_driver
    .table_starts_offsets()
    .map(|el| el as u32)
    .to_vec();
```

### 19.1 检查 setup generic lookup column 组数

```rust
assert_eq!(
    setup_layout.generic_lookup_setup_columns.num_elements(),
    num_required_tuples_for_generic_lookup_setup
);
```

前面根据：

```text
total_tables_size / (trace_len - 1)
```

算出了需要多少组 generic lookup setup columns。

这里检查 `setup_layout` 里实际分配的组数等于这个数量。

如果不一致，说明：

```text
setup layout 不能容纳所有 fixed lookup table rows。
```

### 19.2 stage_2_layout

```rust
let stage_2_layout = LookupAndMemoryArgumentLayout::from_compiled_parts(
    &witness_layout,
    &memory_subtree_placement,
    &setup_layout,
);
```

`stage_2_layout` 把三种布局组合起来：

```text
witness_layout:
  witness columns 和 lookup multiplicity 信息。

memory_layout:
  memory argument 访问列信息。

setup_layout:
  fixed lookup table rows 和 timestamp/range setup columns。
```

它供 lookup argument 和 memory argument 的后续阶段使用。

这里不展开 stage 2 的 proving 细节。

### 19.3 table_offsets

```rust
let table_offsets = table_driver
    .table_starts_offsets()
    .map(|el| el as u32)
    .to_vec();
```

`table_offsets` 记录每张 fixed lookup table 在拼接后的 `all_generic_tables` 中的起始位置。

例如：

```text
all_generic_tables = [
  RomRead row 0,
  RomRead row 1,
  ...
  OpTypeBitmask row 0,
  OpTypeBitmask row 1,
  ...
  SpecialCSRProperties row 0,
  ...
]
```

那么：

```text
table_offsets[RomRead] = RomRead 第一行的 global index
table_offsets[OpTypeBitmask] = OpTypeBitmask 第一行的 global index
table_offsets[SpecialCSRProperties] = CSR 表第一行的 global index
```

第六章 witness lookup 会记录：

```text
某次 lookup 查询命中了 all_generic_tables 的第几个 global row。
```

这个 global row index 会进入 `lookup_mapping` 和 multiplicity 统计。

---

## 20. 返回 CompiledCircuitArtifact

最后构造返回值：

```rust
let result = CompiledCircuitArtifact {
    witness_layout,
    memory_layout: memory_subtree_placement,
    setup_layout,
    stage_2_layout,
    degree_2_constraints: compiled_quadratic_terms,
    degree_1_constraints: compiled_linear_terms,
    state_linkage_constraints: linking_constraints,
    public_inputs,
    scratch_space_size_for_witness_gen,
    variable_mapping: layout,
    lazy_init_address_aux_vars,
    memory_queries_timestamp_comparison_aux_vars,
    batched_memory_access_timestamp_comparison_aux_vars,
    register_and_indirect_access_timestamp_comparison_aux_vars,
    trace_len,
    table_offsets,
    total_tables_size,
};

result
```

`CompiledCircuitArtifact` 是 `compile_inner` 的最终产物。

它不是 witness。

它不是 proof。

它是已经编译好的电路布局和约束描述。

---

## 20.1 witness_layout

```text
witness subtree 的完整列布局。
```

第六章 witness 生成时，根据它写：

```text
witness_row
```

---

## 20.2 memory_layout

```text
memory subtree 的完整列布局。
```

第六章 witness 生成时，根据它写：

```text
memory_row
```

---

## 20.3 setup_layout

```text
setup trace 的列布局。
```

第五章 `SetupPrecomputations::get_main_domain_trace` 会根据它写：

```text
range_check_16 setup table
timestamp_range_check setup table
generic lookup setup columns
```

---

## 20.4 stage_2_layout

```text
lookup argument 和 memory argument 后续阶段需要的组合布局。
```

它由：

```text
witness_layout
memory_layout
setup_layout
```

组合生成。

---

## 20.5 degree_2_constraints

```text
所有 degree-2 约束。
```

包括：

```text
普通二次约束
boolean constraint: b^2 - b = 0
```

---

## 20.6 degree_1_constraints

```text
所有 degree-1 线性约束。
```

---

## 20.7 state_linkage_constraints

```text
chunk 之间状态连接约束。
```

用于连接：

```text
当前 chunk 的 final state
和
下一个 chunk 的 initial state
```

---

## 20.8 public_inputs

```text
第一行和倒数第一行需要作为边界值读取的列。
```

第六章 witness 生成完成后，会从 `exec_trace` 中读取这些值。

---

## 20.9 scratch_space_size_for_witness_gen

```text
witness 生成时需要的 scratch space 大小。
```

来源是：

```text
optimized_out_variables.len()
```

被优化掉的变量不占 trace 列，但 witness generator 仍然可能临时计算它们。

---

## 20.10 variable_mapping

```text
Variable -> ColumnAddress 的完整映射。
```

它记录所有变量最终去了哪里：

```text
WitnessSubtree
MemorySubtree
OptimizedOut
```

---

## 20.11 lazy_init_address_aux_vars

```text
lazy init 地址排序检查的辅助变量列位置。
```

由前面 `lazy_init_aux_set` 里的变量转成 `ColumnAddress` 得到。

---

## 20.12 memory_queries_timestamp_comparison_aux_vars

```text
每个 shuffle RAM query 的 timestamp 比较 borrow 变量列位置。
```

第六章 `process_shuffle_ram_accesses` 会写这些辅助变量。

---

## 20.13 batched_memory_access_timestamp_comparison_aux_vars

main RISC-V 分支中 batched memory accesses 为空，所以这里通常是空的 placeholder layout。

---

## 20.14 register_and_indirect_access_timestamp_comparison_aux_vars

main RISC-V 分支中 register_and_indirect_accesses 为空，所以这里通常是空的 placeholder layout。

---

## 20.15 trace_len

```text
当前 circuit trace 总行数。
```

它等于：

```text
1 << trace_len_log2
```

main RISC-V 每个 chunk 的真实 cycle 数通常是：

```text
trace_len - 1
```

---

## 20.16 table_offsets

```text
每张 fixed lookup table 在 all_generic_tables 中的起始位置。
```

用于 lookup mapping 和 multiplicity 统计。

---

## 20.17 total_tables_size

```text
所有 fixed lookup table rows dump 后的总行数。
```

它必须和 setup trace 生成阶段 `table_driver.total_tables_len` 对齐。

---

## 21. 本段总结

这一段 `compile_inner::<false>` 做了完整的列布局编译：

```text
1. 给 memory argument 相关变量分配 memory subtree columns。
2. 给 witness 相关变量分配 witness subtree columns。
3. 给 range check、boolean、lookup、timestamp comparison 创建布局描述。
4. 把普通 Constraint<Variable> 编译成 ColumnAddress 形式。
5. 生成 state linkage 和 public inputs。
6. 返回 CompiledCircuitArtifact。
```

这段代码不做：

```text
不运行 guest。
不填 pc / register / memory 的真实值。
不查 RomRead 表。
不生成 setup trace。
不生成 proof。
```

它只做：

```text
把 CircuitOutput 里的抽象规则，编译成后续 setup、witness、prove 都能使用的列布局和约束描述。
```


------

## 最短结论

`compile_inner` 非常重要。它是 Airbender 从“约束描述”进入“可证明电路布局”的核心函数。

它做的事情是：

```text
CircuitOutput
  -> 检查 main RISC-V / delegation 形状
  -> 计算 setup_layout
  -> 布局 memory subtree
  -> 布局 witness subtree
  -> 编译 range check
  -> 编译 boolean constraints
  -> 编译 lookup queries
  -> 编译 timestamp range check expressions
  -> 优化掉部分变量
  -> 编译普通 constraints
  -> 生成 public inputs / state linkage
  -> 生成 CompiledCircuitArtifact
```

它在你现在的学习路线里位置很高：

```text
第4章 describe_state_transition:
  写规则。

第5章 compile_inner:
  把规则排成列布局。

第6章 evaluate_witness:
  按这个列布局填真实执行值。

第7章 prove:
  按这个列布局检查约束并生成证明。
```

所以看 Airbender 约束系统，`compile_inner` 必看。它不需要一次性逐行全部啃完，但要先掌握这四块：

```text
1. CircuitOutput 拆包。
2. memory subtree 布局。
3. witness subtree 布局。
4. CompiledCircuitArtifact 返回值。
```
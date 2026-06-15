下面开始写第一篇。为了后面能持续逐行展开，我先把符号、代码地图、阅读主线和“setup到底在setup什么”定下来。后续我会按这个风格继续：每遇到关键函数就下钻，遇到理论对象就立即解释它的数学含义和代码意义，并持续用同一套例子贯穿。

我这次先写第0章和第1章。依据是Airbender官方docs中的Circuit Entry Points、Repo Layout、Circuit Overview和AIR-style constraints相关说明：官方文档把circuit entry-point函数定义为编译circuit、取得TableDriver、生成layout/quotient artifacts、调用witness generator的入口，并明确列出`get_main_riscv_circuit_setup`是main RISC-V machine的入口之一；Repo Layout说明`cs/`是AIR circuit API和实现，`circuit_defs/`是CPU/GPU circuit glue、RISC-V ISA tests、chunking和STARK verifier相关逻辑，`tools/`是高层CLI；Circuit Overview说明Airbender证明固定周期数的RV32I+M执行，bytecode在ROM，register被建模为统一memory argument里的独立地址空间。([GitHub][1])

## 第0章 Airbender阅读路线：先把它当成一个新的zkVM

Airbender不要先用SP1的chip/trace/LogUp/Zerocheck/PCS结构去套。SP1里我们习惯从ExecutionRecord、chip、trace、AIR、LogUp、Zerocheck、PCS一路读下去；Airbender的代码组织更像一套“用Rust DSL写出的AIR机器”，主线在`cs/`和`circuit_defs/`。你这次的目标也不是后端证明系统，而是main RISC-V约束系统怎么被setup出来，所以阅读主线应该从setup入口开始，再进入main RISC-V machine configuration。

本轮阅读先固定三条线：

```text
第一条线：setup入口
tools/cli/src/setup.rs
  -> circuit_defs/setups/src/circuits/main_riscv/mod.rs
  -> get_main_riscv_circuit_setup

第二条线：约束系统
cs/src/circuit.rs
cs/src/cs_reference.rs
cs/src/machine
cs/src/machine_configurations
cs/src/ops
cs/src/devices/optimization_context.rs
cs/src/tables.rs
cs/src/csr_properties.rs

第三条线：VM / witness
risc_v_simulator
witness_eval_generator
gpu_witness_eval_generator
```

第一条线回答：setup命令到底构造了什么？

第二条线回答：main RISC-V circuit一行表示什么，状态、ROM、register、RAM、opcode、lookup、delegation怎么进入约束？

第三条线回答：prover实际执行程序后，witness怎样填进这套约束系统？

当前阶段先不深入后端prover。Airbender文档里确实有Stage 1到Stage 5的prover pipeline：LDE、memory/lookup/delegation arguments、constraint quotient、DEEP quotient、FRI folding。这个以后只需要知道接口，暂时不展开。我们的重点是Stage 3之前“约束系统和witness对象是怎么来的”。([GitHub][2])

## 第0.1节 代码目录怎么分层

Airbender官方Repo Layout给了一个很有用的粗分层：`cs/`包含AIR circuit API和实现，类似一套自定义DSL；`circuit_defs/`包含CPU/GPU glue、RISC-V ISA circuit tests、CPU prover chunking和STARK verifier相关逻辑；`tools/`是高层CLI；`risc_v_simulator/`是RISC-V simulator；`witness_eval_generator/`和`gpu_witness_eval_generator/`负责witness生成闭包和GPU witness路径。([GitHub][3])

可以先记成：

```text
tools/
  人从命令行进来。setup、prove、verify这类命令一般在这里调度。

circuit_defs/
  把具体circuit包装成可编译、可setup、可生成artifact、可测试的工程入口。
  你关心的 get_main_riscv_circuit_setup 在这里。

cs/
  约束系统本体。这里定义如何声明变量、约束、lookup、machine configuration、opcode gadget。
  这是你后面要精读的核心。

risc_v_simulator/
  执行RISC-V程序，帮助生成witness或测试约束覆盖。

witness_eval_generator/
gpu_witness_eval_generator/
  把约束系统的witness assignment逻辑转换成CPU/GPU可用的执行路径。

prover/
gpu_prover/
verifier/
full_statement_verifier/
  后端证明和验证。当前阶段只理解接口，不进入细节。
```

这样看，`circuit_defs/setups`不是约束系统本体，它更像“把约束系统编译成可用setup对象”的地方。真正的约束语义大多会继续下钻到`cs/src/machine_configurations`、`cs/src/ops`、`cs/src/devices`和`cs/src/tables.rs`。

## 第0.2节 当前阅读对象：get_main_riscv_circuit_setup

官方Circuit Entry Points文档说明，每个circuit crate下面的entry-point函数是读circuit的第一入口：它们用来编译circuit、获得lookup tables，也会生成layout和quotient source等辅助artifacts；文档也明确列出六类setup函数，其中包括`get_main_riscv_circuit_setup`。([GitHub][1])

这句话非常关键。它说明`get_main_riscv_circuit_setup`不是普通业务函数，它承担的是“把main RISC-V约束系统编译成工程可消费对象”的职责。

先用一句话给它定位：

```text
get_main_riscv_circuit_setup 是 main RISC-V machine 的 setup 构造入口。
它把机器配置、ROM/lookup tables、约束布局、witness生成接口和证明所需的预计算对象组织起来。
```

这里的setup大概率会涉及几类对象。后续逐行读代码时，我们会逐一确认：

```text
1. machine type / machine configuration
   当前main RISC-V电路支持哪些opcode、是否支持delegation、是否有signed mul/div。

2. bytecode / ROM
   被证明程序的指令如何进入ROM table。

3. TableDriver
   lookup table集合。包括ROM、decoder、range、CSR/delegation等表。

4. compiled circuit artifact
   从Rust约束描述编译出来的电路结构。

5. layout / quotient artifacts
   后续verifier或GPU prover需要的布局和quotient代码。

6. witness evaluator
   prover执行程序后，如何填充trace/witness列。
```

这就是后面读`get_main_riscv_circuit_setup`时的提纲。每看到一个函数调用，我们都问：它属于这六类中的哪一类？

## 第1章 Airbender主电路的大图

Airbender主电路证明的是RISC-V程序的固定周期执行。官方Circuit Overview写得很明确：它要证明RV32I+M程序在固定cycle数上确定性执行，使用machine mode，trusted-code模型下没有异常；bytecode放在ROM，register作为独立地址空间放进统一memory argument，RAM和register访问都编码成`RegisterOrRam` query。([GitHub][2])

这一段可以拆成几个核心判断。

第一，main circuit的基本单位是cycle。一个row可以先粗略理解为一个RISC-V执行cycle。后面读代码时需要确认是否每个cycle严格对应一行，或者某些辅助区域被单独布局；但在理解main RISC-V state transition时，先把一行当成一步RISC-V执行最容易。

第二，显式状态很少。文档说每行显式维护的状态通常只有`pc`，并拆成16-bit limbs。register和memory值不作为“跨行显式状态”直接存在，而是通过global shuffle RAM argument流动。([GitHub][2])

第三，ROM和RAM分开。instruction fetch是ROM lookup，普通load/store和register access走RAM/register统一argument。官方文档直接强调ROM不是RAM，opcode fetch不会混进一般RAM read。([GitHub][2])

第四，register不是特殊trace数组，而是统一memory argument里的一个地址空间。寄存器访问用`is_register = 1`区分，但仍编码成`RegisterOrRam` query，并放进同一个global shuffle memory argument。([GitHub][2])

把这些放成一张学习图：

```text
RISC-V cycle row
  |
  +-- explicit state
  |     pc_low, pc_high
  |
  +-- ROM lookup
  |     pc -> instruction
  |     instruction -> opcode flags / operands
  |
  +-- register/RAM queries
  |     rs1 read
  |     rs2 read or RAM read
  |     rd write or RAM write
  |
  +-- opcode gadget candidates
  |     ADD / SUB / LOAD / STORE / BRANCH / CSR / ...
  |
  +-- orthogonal selection
  |     only active opcode contributes constraints
  |
  +-- next state
        pc_next
```

这套结构和SP1有明显差异。SP1里你会看到每个chip各自消费ExecutionRecord事件并生成trace；Airbender这里更像一张统一的RISC-V machine row，每一行同时做decode、operand query、候选opcode关系、选择active relation、写回和pc更新。

## 第1.1节 一个贯穿例子：ADD + LW/SW + delegation

我们后面需要一个固定例子，不然setup和约束会很抽象。先定义一个最小程序片段：

```text
初始：
  x1 = 7
  x2 = 9
  x10 = 0x1000
  mem[0x1000] = 0

程序：
  ADD x5, x1, x2       // x5 = 16
  SW  x5, 0(x10)       // mem[0x1000] = 16
  LW  x6, 0(x10)       // x6 = 16
  ADD x7, x6, x2       // x7 = 25
```

后面如果读delegation，再加一条CSR调用：

```text
CSRRW ..., delegation_csr_id, ...
```

这个例子在Airbender里可以拆成三种约束对象。

第一种是ROM lookup。每个cycle都要证明当前`pc`读出的instruction确实来自bytecode ROM。比如`pc = 0x2000`时，ROM表里存的是`ADD x5,x1,x2`的编码。

第二种是register/RAM query。第一条ADD需要读取x1和x2，写x5。Airbender把register当作memory argument里的独立地址空间，因此这三个寄存器访问会成为register query。SW会读取x10和x5，并向RAM地址0x1000写入16。LW会读取x10，并从RAM地址0x1000读回16。

第三种是opcode gadget。ADD约束加法语义，SW/LW约束地址计算、读写类型、RAM query内容，branch/jump约束pc_next。每个cycle会准备多个候选opcode关系，但最后只选择当前decode出来的那个opcode。

## 第1.2节 约束系统的第一原则：execute candidates, select active

Airbender文档把Optimization Context with orthogonal selection列为核心模式。其思想是：每个cycle里先为很多指令handler准备候选计算，每个关系都带一个`exec_flag`；真正enforce时，根据互斥opcode flags只选择active variant，其余candidate即使witness值无意义，也不会进入有效约束。([GitHub][2])

把它翻译成初学者能直接用的说法：

```text
一行里不会只调用当前opcode对应的代码。
它会让很多opcode handler都生成候选关系。
每个候选关系都带一个开关。
只有当前opcode的开关为1，其它为0。
最后统一 enforce_all，把开关为1的关系纳入约束。
```

以ADD为例，假设当前instruction确实是：

```text
ADD x5, x1, x2
```

decode阶段会得到一组opcode flags：

```text
is_add = 1
is_sub = 0
is_lw  = 0
is_sw  = 0
...
```

然后ADD handler可能生成：

```text
candidate_add_result = rs1_value + rs2_value
candidate_rd_write = candidate_add_result
candidate_pc_next = pc + 4
```

SUB handler也可能生成自己的候选关系：

```text
candidate_sub_result = rs1_value - rs2_value
```

但SUB的`exec_flag = is_sub = 0`，所以它不会影响最终约束。ADD的`exec_flag = is_add = 1`，所以最终约束系统要求：

```text
rd_value = rs1_value + rs2_value
pc_next = pc + 4
```

这种设计的工程意义是减少大量mux和selector。不是每个opcode单独建一套完全隔离的约束路径，而是让相同形状的关系批量收集，最后用互斥flags选择。文档也提到这种设计能复用变量、减少选择成本，并更适合GPU证明。([GitHub][2])

后面读代码时，看到`OptimizationContext`、`append_add_relation`、`append_lookup_relation`、`enforce_all`这类函数，都要把它们放回这个模式里理解：

```text
append_*:
  先记录候选关系，不一定立刻把它变成最终约束。

exec_flag:
  表示这条候选关系是否属于当前opcode。

enforce_all:
  批量处理这些候选关系，并根据opcode flags选择active one。
```

## 第1.3节 Airbender约束和普通程序代码的差异

初学者读这类代码最容易卡住的一点是：它看起来像Rust函数，但它不是在“执行RISC-V程序”。它是在“构造约束”。

比如普通Rust代码里：

```rust
let c = a + b;
```

意思是现在真的算出`c`。

约束系统代码里类似的表达更像：

```text
创建一个变量c；
声明约束 c = a + b；
给prover一个机会把c的witness填成a+b。
```

所以后面读Airbender代码时，要把每个变量分成两层看：

```text
变量本身：
  constraint variable，出现在AIR约束里。

变量的值：
  witness value，prover执行程序后填进去。
```

约束系统的目标不是替prover执行程序，而是写出检查规则。prover可以填任意witness，但如果填错，约束不成立。

用ADD例子：

```text
RISC-V语义：
  x5 = x1 + x2 = 7 + 9 = 16

witness：
  prover在某些列里填 rs1=7, rs2=9, rd=16

constraint：
  rd - rs1 - rs2 = 0
```

如果prover填成rd=17，则：

```text
17 - 7 - 9 = 1
```

约束不为0，证明失败。

Airbender文档里的AIR-style constraints说明，代码中会用`Term<F>`表示单个monomial，用`Constraint<F>`表示多个term相加形成的约束；内部可能临时使用更高degree表达式，但最终归一化后的约束要求degree≤2。这个degree限制后面很重要，因为它影响所有复杂RISC-V语义如何被拆成低度关系。([GitHub][4])

## 第1.4节 main circuit的一行大概长什么样

先不要陷入具体列布局。我们先给一个教学版row结构，后面读layout compiler和setup时再修正。

```text
Main RISC-V row
  explicit state:
    pc_low
    pc_high

  ROM/decode:
    instruction
    opcode flags
    rd, rs1, rs2
    imm
    funct3 / funct7 等decode字段

  memory/register queries:
    query_0: rs1 read
    query_1: rs2 read 或 RAM read
    query_2: rd write 或 RAM write

  opcode candidate data:
    add relation
    sub relation
    load/store relation
    branch relation
    mul/div relation
    csr/delegation relation

  next state:
    pc_next_low
    pc_next_high
```

官方文档说系统通常只保留少量显式状态，主要是拆成16-bit limbs的pc；register和memory值都通过global shuffle RAM argument流动。([GitHub][2]) 所以这里的row不是“把32个register全放在一行里”。寄存器值通过query读写，不是作为32个跨行状态列保存。

这点非常重要，因为它决定了后续阅读方式。

SP1风格里，我们可能会问：

```text
x1寄存器的值存在什么trace列？
```

Airbender里更适合问：

```text
这一行有没有一个register read query读取x1？
这个query如何被memory/register argument约束成和之前写入一致？
```

对ADD x5,x1,x2：

```text
query_0: read register x1 -> 7
query_1: read register x2 -> 9
query_2: write register x5 <- 16
```

ADD opcode约束负责说明：

```text
16 = 7 + 9
```

memory/register argument负责说明：

```text
read x1 的值7来自初始化或之前某次write
read x2 的值9来自初始化或之前某次write
write x5 把x5的新值更新成16
```

这两部分合在一起，才证明了ADD这一步的VM语义。

### 1.4.1 用同一条ADD对比SP1和Airbender的阅读方式

这里用同一条指令做对比：

```text
ADD x5, x1, x2
```

假设执行前：

```text
x1 = 7
x2 = 9
```

执行后应该得到：

```text
x5 = 16
```

这条指令在SP1和Airbender里都要被证明，但两边的组织方式差异很大。

#### SP1里的读法：从事件到chip，再到各张证明表

在SP1里，executor先真正执行guest指令。执行到：

```text
ADD x5, x1, x2
```

时，executor会知道：

```text
rs1 = x1 = 7
rs2 = x2 = 9
rd  = x5 = 16
```

然后它把这次ADD执行产生的事件写进ExecutionRecord。对ADD来说，核心事件会进入类似`add_events`这样的集合；寄存器读写也会作为memory相关记录进入record。此时可以把ExecutionRecord理解成执行日志：

```text
ExecutionRecord
  add_events:
    ADD event:
      pc = 当前pc
      opcode = ADD
      rs1_value = 7
      rs2_value = 9
      rd_value = 16

  memory/register events:
    read  x1 -> 7
    read  x2 -> 9
    write x5 <- 16
```

后面进入证明系统时，不是一个统一的RISC-V row直接处理所有内容，而是不同chip分别读取ExecutionRecord里和自己有关的事件。

对这条ADD，AddChip会读取`add_events`，生成Add trace的一行：

```text
AddChip trace row
  opcode = ADD
  input_a = 7
  input_b = 9
  result = 16
  is_real = 1
```

在你的贯穿例子里，结果按16-bit limb写成：

```text
result limbs = [16, 0, 0, 0]
```

AddChip的AIR约束负责说明：

```text
7 + 9 = 16
```

如果需要range或byte检查，AddChip会在dependency阶段补充对应lookup需求。例如结果limb需要证明落在16-bit范围内，于是会向RangeChip登记类似：

```text
Range(16, 16, 0)
Range(0, 16, 0)
Range(0, 16, 0)
Range(0, 16, 0)
```

RangeChip之后读取这些lookup multiplicity，生成自己的Range trace。LogUp再检查AddChip发出的lookup需求和Range表提供的项是否一致。

寄存器读写的一致性不由AddChip单独完成。AddChip只证明这条ADD的算术关系。`x1`的值为什么是7，`x2`的值为什么是9，`x5`写入16以后后续读到的值为什么一致，这些属于memory/register相关表的工作。也就是说，SP1里一条ADD会影响多张表：

```text
ADD x5, x1, x2
  |
  +-- AddChip
  |     证明加法语义：7 + 9 = 16
  |
  +-- Memory/Register相关chip
  |     证明x1、x2、x5这些寄存器读写一致
  |
  +-- RangeChip / ByteChip
  |     证明limb、byte、range lookup一致
  |
  +-- Global / interaction相关机制
        证明跨表、跨shard的交互一致
```

所以在SP1里读一条opcode，通常会问：

```text
executor把它写进ExecutionRecord的哪个事件集合？
哪个chip读取这个事件？
这个chip生成哪张trace？
这个chip还登记了哪些lookup或global interaction？
AIR::eval里怎样约束这张trace？
```

SP1的核心阅读单位更偏向“事件集合 + chip表”。AddChip的row是ADD事件；RangeChip的row是Range lookup项；MemoryLocalChip的row是内存或寄存器状态变化。它们共同证明同一条guest指令的不同侧面。

#### Airbender里的读法：从一个RISC-V cycle row看ROM、query和候选关系

Airbender读同一条指令时，入口不再是ExecutionRecord里的ADD事件，也不是AddChip。更自然的读法是：这一行main RISC-V row正在证明一个cycle的状态转移。

假设当前pc指向：

```text
ADD x5, x1, x2
```

这一行大致可以拆成几部分。

第一，显式状态里有pc。Airbender通常不会把32个register都放在row里作为跨行状态。它显式保留的状态很少，主要是pc的limb：

```text
explicit state:
  pc_low
  pc_high
```

第二，当前pc会去ROM里取instruction：

```text
pc -> ROM lookup -> instruction
```

如果ROM里这个pc对应的就是ADD编码，decode之后会得到：

```text
opcode flag:
  is_add = 1
  is_sub = 0
  is_lw  = 0
  is_sw  = 0
  ...

decoded fields:
  rd  = 5
  rs1 = 1
  rs2 = 2
```

第三，这一行会准备register queries。因为ADD需要读x1、读x2、写x5，所以这一行会形成类似：

```text
query_0: read  register x1 -> 7
query_1: read  register x2 -> 9
query_2: write register x5 <- 16
```

这里的register不是一组专门的跨行trace列。它们被放进统一memory/register argument里。可以把register访问想象成带地址空间标记的memory query：

```text
read register x1:
  is_register = 1
  address = 1
  value = 7

read register x2:
  is_register = 1
  address = 2
  value = 9

write register x5:
  is_register = 1
  address = 5
  value = 16
```

第四，opcode candidate logic会准备ADD、SUB、LOAD、STORE、BRANCH等候选关系。当前行的decode flag说明只有ADD是active，所以最终会选择ADD关系。

ADD候选关系说明：

```text
rd_value = rs1_value + rs2_value
```

代入这一行的witness：

```text
16 = 7 + 9
```

同时pc更新关系说明：

```text
pc_next = pc + 4
```

第五，memory/register argument负责证明query值的一致性。ADD本地约束只说明：

```text
16 = 7 + 9
```

但它不单独说明x1为什么是7、x2为什么是9。这个问题交给统一memory/register argument处理：

```text
read x1 的值7来自初始化或之前某次write
read x2 的值9来自初始化或之前某次write
write x5 把x5的新值更新成16
```

因此，Airbender里一条ADD更像是一个RISC-V cycle row中的多个部件协同工作：

```text
ADD x5, x1, x2
  |
  +-- explicit state
  |     当前pc
  |
  +-- ROM/decode
  |     pc查ROM得到instruction
  |     decode得到is_add、rd、rs1、rs2
  |
  +-- register queries
  |     read x1 -> 7
  |     read x2 -> 9
  |     write x5 <- 16
  |
  +-- opcode candidate relation
  |     ADD关系被is_add选中
  |     约束16 = 7 + 9
  |
  +-- memory/register argument
        证明register read/write的时间一致性
```

所以在Airbender里读一条opcode，更适合问：

```text
这一行的pc如何从ROM取到instruction？
decode如何得到opcode flags和rd/rs1/rs2？
这一行构造了哪些RegisterOrRam queries？
当前opcode的候选关系如何被active flag选中？
memory/register argument如何保证query值来自正确的历史状态？
```

#### 再用SW对比一次

再看一条store：

```text
SW x5, 0(x10)
```

假设：

```text
x5 = 16
x10 = 0x1000
```

执行后：

```text
mem[0x1000] = 16
```

在SP1里，executor会生成store相关事件。后续通常会有不同chip分别处理：

```text
StoreWordChip:
  证明SW指令本身的语义
  地址 = x10 + offset
  写入值 = x5

MemoryLocalChip:
  证明当前shard内部这个地址的状态从旧值更新到新值

GlobalChip / global interaction:
  把这个内存状态变化接到跨表、跨shard的一致性检查里

ByteChip / RangeChip:
  处理地址、timestamp、value limb相关lookup
```

所以SP1读SW时，会沿着事件和表展开：

```text
SW event
  -> StoreWord trace
  -> MemoryLocal trace
  -> Global interaction
  -> Byte/Range lookup
```

在Airbender里，SW仍然是一行main RISC-V cycle row。它会做ROM/decode，发现当前opcode是SW，然后构造register和RAM queries：

```text
query_0: read register x10 -> 0x1000
query_1: read register x5  -> 16
query_2: write RAM[0x1000] <- 16
```

SW的opcode relation负责说明：

```text
address = x10 + offset
value_to_store = x5
```

统一memory/register argument负责说明：

```text
x10的读值来自初始化或之前写入
x5的读值来自之前ADD写入
RAM[0x1000]被更新成16
```

所以Airbender读SW时，更像是在同一个cycle row里同时看：

```text
ROM/decode
register reads
RAM write
store address relation
memory/register argument
pc_next
```

#### 两种阅读方式的核心差别

可以把两者的差异压成一张表：

| 问题              | SP1里的读法                            | Airbender里的读法                                                  |
| --------------- | ---------------------------------- | -------------------------------------------------------------- |
| 一条ADD首先在哪里出现    | ExecutionRecord里的ADD事件             | main RISC-V row里的当前cycle                                       |
| 谁证明ADD算术语义      | AddChip                            | 当前row中被`is_add`选中的ADD candidate relation                       |
| register值如何进入证明 | executor记录寄存器读写事件，再由memory相关chip处理 | 当前row构造RegisterOrRam query，进入统一memory/register argument        |
| 一条opcode会影响什么   | 多张chip表：Add、Memory、Range、Global等   | 同一个RISC-V row里的ROM/decode、queries、candidate relation、pc update |
| 阅读入口            | 找事件集合和消费它的chip                     | 找row state、decode、query、active opcode relation                 |
| 常见问题            | 这个事件被哪个chip读取？生成哪张trace？           | 这一行构造了哪些query？哪个opcode flag为1？                                 |

所以，SP1更像“先执行成事件日志，再由多张chip表分别证明”。Airbender更像“每个RISC-V cycle row同时包含decode、query、候选关系和状态转移”。

这个差别会影响后续所有代码阅读。

读SP1时，看到ADD，会自然去找：

```text
add_events
AddChip
generate_trace_into
Air::eval
Range lookup
```

读Airbender时，看到ADD，要去找：

```text
main machine row
ROM read / decoder
opcode flags
RegisterOrRam query
OptimizationContext里的ADD relation
opt_ctx.enforce_all
state transition / pc_next
```

这就是为什么在Airbender里问“x1寄存器的值存在什么trace列”不太合适。更合适的问题是：

```text
这一行是否构造了一个读取x1的register query？
这个query如何进入global shuffle memory argument？
ADD relation如何使用这个query返回的value？
```

这套问题和Airbender的代码结构更贴合。


## 第1.5节 ROM、RAM、register三者的关系

Airbender把bytecode放在ROM里。每个cycle根据pc做instruction fetch。文档明确说instruction fetch是由pc keyed的ROM lookup，ROM和RAM分离。([GitHub][2])

先画成：

```text
pc
 |
 v
ROM lookup
 |
 v
instruction
 |
 v
decode fields
 |
 v
opcode flags + rd/rs1/rs2/imm
```

RAM和register走另一条线：

```text
register / RAM access
 |
 v
RegisterOrRam query
 |
 v
global shuffle memory argument
```

寄存器通过`is_register = 1`选择独立地址空间。也就是说，x1和RAM地址1不是同一个东西。它们可以共享统一argument格式，但通过地址空间标记区分。

教学上可以把访问地址写成：

```text
Register x1:
  is_register = 1
  address = 1

RAM[0x1000]:
  is_register = 0
  address = 0x1000
```

于是ADD x5,x1,x2的三个query大概是：

```text
read  (is_register=1, addr=1, value=7)
read  (is_register=1, addr=2, value=9)
write (is_register=1, addr=5, value=16)
```

SW x5,0(x10)大概是：

```text
read  (is_register=1, addr=10, value=0x1000)
read  (is_register=1, addr=5, value=16)
write (is_register=0, addr=0x1000, value=16)
```

LW x6,0(x10)大概是：

```text
read  (is_register=1, addr=10, value=0x1000)
read  (is_register=0, addr=0x1000, value=16)
write (is_register=1, addr=6, value=16)
```

后面读代码时，我们要找到这些query是在哪里创建、以什么结构保存、Stage 2/Stage 3怎么处理。当前只先建立概念。

## 第1.6节 lazy init和teardown先怎么理解

Airbender把执行切成chunks。每个chunk只证明一段固定cycle数。问题是：某个chunk第一次读取x1或RAM[0x1000]时，它的值从哪里来？

官方文档说每个chunk有lazy init和teardown，前者记录初始值和timestamp，后者记录最终值和timestamp，并跨chunk连接保证连续性。([GitHub][2])

初学者可以先这样理解：

```text
lazy init:
  这个chunk第一次碰到某个register/RAM地址时，给它一个入口状态。
  这个入口状态不是凭空来的，最终要和前一个chunk或public input连接。

teardown:
  这个chunk结束时，把碰过的地址的最终状态拿出来。
  后续chunk的lazy init要能接上这些最终状态。
```

以前面的例子：

```text
chunk开始：
  x1 = 7
  x2 = 9
  x10 = 0x1000
  RAM[0x1000] = 0

chunk执行：
  ADD, SW, LW, ADD

chunk结束：
  x5 = 16
  x6 = 16
  x7 = 25
  RAM[0x1000] = 16
```

lazy init会覆盖第一次读取的地址，例如x1、x2、x10、RAM[0x1000]。teardown会给出最终被触碰地址的状态，例如x5、x6、x7、RAM[0x1000]。后端memory argument和statement verifier再保证这些入口/出口状态的全局一致性。

当前阶段先不要深入grand product。我们只要知道：main RISC-V circuit不是独立证明一段孤立执行，它通过lazy init/teardown和global memory/register argument连接多个chunks。

## 第1.7节 delegation先怎么放进主线

官方Circuit Entry Points文档说明，delegation circuits是通过专用CSR值调用的自包含gadgets；每个precompile有唯一`DELEGATION_TYPE_ID`，必须和程序写入的CSR值匹配。文档列出的例子包括bigint和BLAKE2 compression。([GitHub][1])

现在先用一个简单模型理解：

```text
main RISC-V circuit:
  识别某条CSR指令是在请求delegation。
  约束delegation type id、输入输出位置、memory/register交互。
  不在main circuit里展开整个BLAKE2或bigint计算。

delegation circuit:
  专门证明BLAKE2或bigint内部计算。
  通过共享的memory/delegation argument和main circuit连接。
```

这和普通opcode不同。ADD、LW、SW这类语义由main RISC-V circuit直接约束；BLAKE2、bigint这种复杂操作会转到专用delegation circuit。后面读main setup时，只需要先搞清楚：main circuit如何允许这些CSR、如何记录delegation请求、如何把请求和delegation circuit对上。

## 第1.8节 这套笔记后续怎么展开

从下一篇开始，我建议按这个顺序写详细代码阅读笔记：

```text
第2章 tools/cli/src/setup.rs
  setup命令入口
  命令参数
  如何选择main RISC-V setup
  输出artifact

第3章 get_main_riscv_circuit_setup函数签名
  泛型参数
  输入
  返回值
  主流程分段

第4章 get_main_riscv_circuit_setup逐行
  每个局部变量
  每个子函数
  每个setup artifact
  遇到子函数就下钻解释

第5章 Machine configuration
  FullIsaMachineWithDelegationNoExceptionHandling
  opcode集合
  delegation CSR whitelist
  no exception handling的含义

第6章 Constraint system API
  CS
  Variable
  Term
  Constraint
  Boolean
  Register
  LookupInput
  TableType

第7章 main RISC-V state transition
  pc state
  ROM fetch
  decoder
  register/RAM queries
  opcode handlers
  optimization context
  writeback

第8章 instruction gadgets
  ADD/SUB
  LW/SW
  branch/jump
  MUL/DIV
  CSR/delegation

第9章 witness path
  simulator
  witness evaluator
  CPU/GPU witness generator
```

每章都按固定格式写：

```text
1. 代码位置
2. 这一层解决什么问题
3. 关键类型
4. 主流程图
5. 逐段代码解释
6. 贯穿例子代入
7. 和上一章/下一章的连接
```

## 本轮小结

当前先记住四句话。

第一，Airbender main RISC-V circuit主要从`get_main_riscv_circuit_setup`这类entry point读起，entry point负责编译circuit、拿TableDriver、生成layout/quotient artifacts以及连接witness generator等工程对象。([GitHub][1])

第二，`cs/`是约束系统本体，里面有AIR API、machine configuration、opcode ops、optimization context、tables；`circuit_defs/`更多是把这些约束系统组织成可setup、可测试、可证明的工程入口。([GitHub][3])

第三，main circuit证明的是固定cycle数的RV32I+M执行，bytecode在ROM，register和RAM通过统一memory argument处理，显式跨行状态主要是pc。([GitHub][2])

第四，Airbender main RISC-V约束的核心模式是先准备多个opcode候选关系，再通过互斥flags选择当前opcode的关系；后面读`OptimizationContext`时，要一直带着这个模型。([GitHub][2])

下一步我可以直接开始写《第2章：tools/cli/src/setup.rs逐行阅读》。如果你希望更快进入核心，也可以跳过CLI，直接从`circuit_defs/setups/src/circuits/main_riscv/mod.rs`里的`get_main_riscv_circuit_setup`开始。

[1]: https://raw.githubusercontent.com/matter-labs/zksync-airbender/main/docs/circuit_entry_points.md "raw.githubusercontent.com"
[2]: https://raw.githubusercontent.com/matter-labs/zksync-airbender/main/docs/circuit_overview.md "raw.githubusercontent.com"
[3]: https://raw.githubusercontent.com/matter-labs/zksync-airbender/main/docs/repo_layout.md "raw.githubusercontent.com"
[4]: https://github.com/matter-labs/zksync-airbender/tree/main/docs "zksync-airbender/docs at main · matter-labs/zksync-airbender · GitHub"

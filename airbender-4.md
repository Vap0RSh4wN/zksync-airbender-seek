新版第四章应当替换旧版第四章，不需要回读旧版。旧版里关于pc查ROM、decode、三槽位shuffle RAM query、ADD/LW/SW例子的内容，已经放进新版的后半部分；新版前半部分增加Variable、Num、Boolean、Term、Constraint这层，把Machine代码写成多项式约束的路径补完整。

# 第四章 Machine语义怎样写成多项式约束

第三章解释`get_machine`怎样产出`CompiledCircuitArtifact`，`get_table_driver`怎样产出`TableDriver`，`SetupPrecomputations`怎样用`TableDriver`和`setup_layout`生成setup trace。本章进入`get_machine`内部，解释RISC-V状态转移怎样变成`CircuitOutput`里的约束、lookup和shuffle RAM query。

`compile_machine`创建`cs = C::new()`，调用`create_table_driver_into_cs(&mut cs, machine)`注册固定表，再调用`M::describe_state_transition(&mut cs)`写CPU状态转移规则。`cs.finalize()`返回`CircuitOutput`，后续`OneRowCompiler`把`CircuitOutput`编译成`CompiledCircuitArtifact`。

```text id="dps31r"
compile_machine
  -> C::new()
  -> create_table_driver_into_cs(&mut cs, machine)
  -> M::describe_state_transition(&mut cs)
  -> cs.finalize()
  -> CircuitOutput
  -> OneRowCompiler
  -> CompiledCircuitArtifact
```

`describe_state_transition`不执行guest程序。它定义一行CPU trace必须满足的关系。ADD、LW、SW、branch、CSR这些RISC-V语义在这里变成电路变量、多项式约束、lookup查询和memory query。

## 4.1 BasicAssembly记录CircuitOutput的原材料

`BasicAssembly`实现`Circuit` trait。它内部保存`constraint_storage`、`lookup_storage`、`shuffle_ram_queries`、`boolean_variables`、`rangechecked_expressions`、`table_driver`、delegation request和witness graph。`BasicAssembly::new()`创建空列表和空`TableDriver`。

Machine代码调用`cs.add_variable()`创建新变量，调用`cs.add_constraint(...)`登记多项式约束，调用`cs.get_variables_from_lookup_constrained(...)`登记固定表lookup，调用`cs.add_shuffle_ram_query(...)`登记寄存器/RAM访问。`BasicAssembly::finalize()`把这些对象写入`CircuitOutput`，字段包括`table_driver`、`shuffle_ram_queries`、`constraints`、`lookups`、`range_check_expressions`、`boolean_vars`和`substitutions`。

```text id="4qmz8g"
Machine代码
  创建变量
  构造表达式
  登记约束
  登记lookup
  登记shuffle RAM query

BasicAssembly::finalize
  输出CircuitOutput
```

`CircuitOutput`还没有真实列位置。`OneRowCompiler`之后才会把`Variable`映射到witness列、memory列、setup列或stage2列。

## 4.2 Variable是电路变量编号

`Variable`只是一个`u64`编号。源码定义为`pub struct Variable(pub u64)`，并用`u64::MAX`表示placeholder变量。

`BasicAssembly::add_variable()`返回当前编号，然后把`no_index_assigned`加一。它不会填入真实执行值。

ADD例子里，Machine代码可能创建这些变量：

```text id="p67hks"
Variable(10)  pc_low
Variable(11)  pc_high
Variable(12)  opcode_low
Variable(13)  opcode_high
Variable(20)  rs1_value_low
Variable(21)  rs1_value_high
Variable(22)  rs2_value_low
Variable(23)  rs2_value_high
Variable(24)  rd_value_low
Variable(25)  rd_value_high
Variable(30)  is_add
```

这些名字只是解释用。源码只保存编号。真实执行时，如果`x1=7`、`x2=9`、`x5=16`，witness生成阶段会把具体field value写入对应变量所属的trace列。

```text id="sjcblx"
编译阶段：
  Variable(20)代表某个待赋值位置

witness生成阶段：
  Variable(20)对应的列值 = 7

约束检查阶段：
  约束系统检查Variable(20)、Variable(22)、Variable(24)之间的多项式关系
```

## 4.3 Num和Boolean封装变量、常量和flag

Machine代码大量使用`Num`和`Boolean`，因为RISC-V语义里既有变量，也有常量和布尔flag。

`Num`表示变量或常量：

```rust id="yj5os9"
pub enum Num<F: PrimeField> {
    Var(Variable),
    Constant(F),
}
```

`Num::Var(v)`指向电路变量，`Num::Constant(c)`保存编译期已知field元素。这个封装让gadget代码在操作常量时少分配变量。

`Boolean`表示电路里的布尔值：

```rust id="q1oc9g"
pub enum Boolean {
    Is(Variable),
    Not(Variable),
    Constant(bool),
}
```

`Boolean::Is(v)`表示变量`v`必须取0或1。`Boolean::Constant(false)`表示编译期常量false。

`Boolean::new`通过`circuit.add_boolean_variable()`创建布尔变量。源码注释给出布尔约束：

```text id="kuq4mw"
(1 - a) * a = 0
```

这个多项式只允许`a=0`或`a=1`。

decoder输出的`is_add`、`r_insn`、`i_insn`等flag都属于这个层次。它们不是Rust的`bool`，而是电路变量或常量。witness给这些flag赋值，约束系统检查flag和opcode decode结果一致。

## 4.4 Term表示单项式

`Term`是多项式里的原子项。源码定义了两种形式：常数，或者`coeff * variables...`形式的单项式。`inner`保存变量数组，`degree`保存次数。

```rust id="dgb9ft"
pub enum Term<F: PrimeField> {
    Constant(F),
    Expression {
        coeff: F,
        inner: [Variable; TERM_INNER_CAPACITY],
        degree: usize,
    },
}
```

例子：

```text id="dnmeu2"
5
  -> Term::Constant(5)

3 * Variable(20)
  -> degree = 1
  -> coeff = 3
  -> inner = [Variable(20)]

2 * Variable(20) * Variable(30)
  -> degree = 2
  -> coeff = 2
  -> inner = [Variable(20), Variable(30)]
```

`Term::normalize()`会排序变量，使`x*y`和`y*x`得到同一种表示。这个归一化让后续同类项合并可以通过变量数组比较完成。

## 4.5 Constraint表示二次以内多项式等于0

`Constraint`是一组`Term`的和：

```rust id="4qxgja"
pub struct Constraint<F: PrimeField> {
    pub terms: Vec<Term<F>>,
}
```

源码注释说明，`Constraint`表示稀疏多项式，算术操作会归一化、合并同类项，并要求归一化后的次数不超过2。

约束系统采用等于0形式。RISC-V语义中的：

```text id="bz5vv6"
rd = rs1 + rs2
```

写成`Constraint`就是：

```text id="0t6svq"
rs1 + rs2 - rd = 0
```

如果ADD约束只在`is_add = 1`时启用，代码会构造带flag的二次约束：

```text id="m0ornf"
is_add * (rs1 + rs2 - rd) = 0
```

当`is_add = 1`，括号里的加法关系必须成立；当`is_add = 0`，这条ADD关系不限制当前行。`Constraint::normalize()`最后检查次数不超过2。

`Constraint::split_max_quadratic`会把一个约束拆成二次项、一次项和常数，后续compiler和prover可以按列布局处理这些项。

## 4.6 Machine代码进入Term和Constraint

`FullIsaMachineWithDelegationNoExceptionHandling::describe_state_transition`选择decoder表划分和decoder boolean keys，然后调用`optimized_base_isa_state_transition`。

`optimized_base_isa_state_transition`创建初始状态，取出pc，对pc低16位登记range check，然后调用decode函数读取opcode并预分配memory query。

pc读取ROM的代码提供了进入`Term`和`Constraint`的第一个例子。`read_opcode_from_rom`先查`RomAddressSpaceSeparator`，得到`is_ram_range`和`rom_address_low`，随后约束`is_ram_range = 0`，保证instruction fetch来自ROM。接着代码构造ROM地址：

```rust id="a1wjw4"
let rom_address_constraint = Term::from(pc.0[0].get_variable())
    + Term::from((F::from_u64_unchecked(1 << 16), rom_address_low));
```

这段表达式对应：

```text id="5zzubx"
rom_address = pc_low + 2^16 * rom_address_low
```

随后`read_opcode_from_rom`用这个线性表达式查`RomRead`表，得到opcode低16位和高16位。

```text id="nyqma2"
pc变量
  -> Term组合出rom_address
  -> RomRead lookup
  -> opcode_low, opcode_high变量
```

第三章解释`RomRead`表怎样进入setup trace；这里解释CPU row怎样用`Term`构造lookup输入，并把lookup输出当成opcode变量使用。

## 4.7 ADD语义进入OptimizationContext

ADD的MachineOp定义在`add_sub.rs`。`AddOp::apply`从`boolean_set`里取得`ADD_OP_KEY`对应的`exec_flag`，从decoder结果里取得`src1`和`src2`，调用`opt_ctx.append_add_relation(src1, src2, exec_flag, cs)`，再把返回的`res`作为候选rd写回值放进`CommonDiffs`。

```rust id="ljooub"
let exec_flag = boolean_set.get_major_flag(ADD_OP_KEY);

let src1 = inputs.get_rs1_or_equivalent().get_register();
let src2 = inputs.get_rs2_or_equivalent().get_register();

let (res, _of_flag) = opt_ctx.append_add_relation(src1, src2, exec_flag, cs);

CommonDiffs {
    exec_flag,
    rd_value: vec![(returned_value, exec_flag)],
    new_pc_value: NextPcValue::Default,
    ...
}
```

ADD x5,x1,x2对应的RISC-V语义是：

```text id="62uw79"
x5_new = x1 + x2
next_pc = pc + 4
```

Airbender把32-bit寄存器拆成两个16-bit limb：

```text id="7gb6px"
x1 = [a_low, a_high]
x2 = [b_low, b_high]
x5_new = [c_low, c_high]
```

`AddOp::apply`不立刻把低位和高位加法约束全部写进`cs`。它把add relation登记到`OptimizationContext`。`optimized_base_isa_state_transition`处理完所有opcode family后调用`opt_ctx.enforce_all(cs)`，`OptimizationContext`再把收集的add/sub关系写成`Constraint`。

这个分两步的设计让多个opcode family共用一批中间变量和约束模板，减少变量数量。`OptimizationContext`内部保存`add_sub_relations`、range check relation、lookup relation、mul/div relation、is_zero relation等延迟关系。

## 4.8 ADD关系变成两条16-bit加法约束

`OptimizationContext::enforce_all`处理add/sub relation时，会把32-bit加法拆成低16位和高16位两条约束。源码构造`constraint_low = a_constraint_low + b_constraint_low - c_constraint_low`，再减去`2^16 * carry_intermediate`并调用`cs.add_constraint(constraint_low)`。随后源码构造高16位约束，把低位carry加进去，并减去`2^16 * carry_out`。

数学形式如下：

```text id="aitccq"
a_low + b_low - c_low - 2^16 * carry = 0

a_high + b_high + carry - c_high - 2^16 * carry_out = 0
```

如果ADD flag参与选择，`a_constraint_low`、`b_constraint_low`、`c_constraint_low`会先被flag过滤。当前行是ADD时，flag为1，约束使用ADD的输入；当前行是SUB、LW或branch时，ADD relation不会约束那一行的寄存器结果。

ADD x5,x1,x2，设`x1=7`、`x2=9`：

```text id="y5zsni"
a_low = 7
a_high = 0
b_low = 9
b_high = 0
c_low = 16
c_high = 0
carry = 0
carry_out = 0
```

两条约束变成：

```text id="snz8qu"
7 + 9 - 16 - 65536 * 0 = 0
0 + 0 + 0 - 0 - 65536 * 0 = 0
```

Machine代码没有把“7”和“9”写死。它创建变量和约束；witness生成阶段填入7和9；约束检查阶段验证这些变量满足上面的多项式关系。

## 4.9 opcode、pc和decoder进入CPU row

`optimized_base_isa_state_transition`从`initial_state`取pc，然后调用`optimized_decode_and_preallocate_mem_queries_for_bytecode_in_rom`。这个函数先执行`read_opcode_from_rom`，再调用`OptimizedDecoder::decode`。

```text id="6yze48"
initial_state.pc
  -> read_opcode_from_rom
  -> opcode_low, opcode_high
  -> OptimizedDecoder::decode
  -> raw_decoder_output
  -> flags_source
```

decoder输出包括rs1、rs2、rd、imm、funct3、funct12和opcode格式flag。信任代码配置下，如果decoder判断opcode无效，代码把`invalid_opcode`作为线性约束加入Circuit，使无效opcode路径不可满足。

ADD x5,x1,x2在decoder层产生：

```text id="m7e1nr"
opcode_format = R-type
ADD_OP_KEY flag = 1
rs1 = 1
rs2 = 2
rd = 5
imm不参与ADD
```

`AddOp`声明自己的decoder子空间：`OPERATION_OP, funct3=000, func7=0000000`匹配ADD；`OPERATION_OP_IMM, funct3=000`匹配ADDI。二者共用`ADD_OP_KEY`。

## 4.10 src2在R/I/S/B格式之间选择

decode函数返回`opcode_format_bits`。代码把这些flag拆成R、I、S、B、U、J格式，并用`Register::choose_from_orthogonal_variants`选择`src2`。

```rust id="mt5amq"
let [r_insn, i_insn, s_insn, b_insn, _u_insn, _j_insn] = opcode_format_bits;

let src2 = Register::choose_from_orthogonal_variants(
    cs,
    &[r_insn, i_insn, s_insn, b_insn],
    &[
        rs2_value_if_register,
        raw_decoder_output.imm,
        rs2_value_if_register,
        rs2_value_if_register,
    ],
);
```

ADD是R-type，`src2`来自rs2寄存器。ADDI是I-type，`src2`来自immediate。`AddOp::apply`只读取`inputs.get_rs2_or_equivalent()`，不关心当前指令是ADD还是ADDI；decode阶段已经把第二个操作数统一成`src2`。

```text id="o0yd4n"
ADD x5,x1,x2:
  src1 = value(x1)
  src2 = value(x2)

ADDI x5,x1,3:
  src1 = value(x1)
  src2 = 3
```

这个设计把RISC-V格式差异放在decode阶段，把算术family的代码写成统一的二输入关系。

## 4.11 三个shuffle RAM query槽位

`optimized_decode_and_preallocate_mem_queries_for_bytecode_in_rom`返回长度为3的`ShuffleRamMemQuery`数组。第一个槽位读取rs1；第二个槽位读取rs2或load的RAM值；第三个槽位写rd或store的RAM值。

rs1槽位固定为寄存器读取。源码从`Placeholder::ShuffleRamReadValue(0)`创建value变量，用`raw_decoder_output.rs1`作为寄存器地址，调用`form_mem_op_for_register_only`构造query。

rs2/load槽位使用`RegisterOrRam`。初始`is_register = true`，address来自`Placeholder::ShuffleRamAddress(1)`，load opcode后续会把它改成RAM读取。

rd/store槽位也使用`RegisterOrRam`。源码创建read value、write value和address placeholder，后续writeback或store逻辑约束这些变量。

ADD x5,x1,x2对应：

```text id="e6l9jk"
slot 0:
  read register x1
  read_value = 7
  write_value = 7

slot 1:
  read register x2
  read_value = 9
  write_value = 9

slot 2:
  write register x5
  read_value = old_x5
  write_value = 16
```

这些值属于witness trace。Machine代码在第四章建立query结构和约束，memory argument在后续章节检查同一地址的读写顺序。

LW和SW使用同一组三个槽位：

```text id="qjz2qu"
LW x5,0(x10)

slot 0:
  read register x10
  得到base address

slot 1:
  read RAM at effective_address
  is_register = false

slot 2:
  write register x5
  is_register = true
```

```text id="qwgrbv"
SW x6,0(x10)

slot 0:
  read register x10
  得到base address

slot 1:
  read register x6
  得到store value

slot 2:
  write RAM at effective_address
  is_register = false
```

`optimized_base_isa_state_transition`把slot 1交给`LoadOp::spec_apply`，把slot 2交给`StoreOp::spec_apply`，这两个opcode family会修改预分配的query。

## 4.12 opcode family生成CommonDiffs

decode和operand准备完成后，`optimized_base_isa_state_transition`依次调用ADD、SUB、LUI、AUIPC、Binary、MUL、DIVREM、Conditional、Shift、Jump、Load、Store、CSR等family的`apply`或`spec_apply`，每个family返回`CommonDiffs`。

`CommonDiffs`记录一个opcode family的候选结果：

```text id="ns3uzs"
exec_flag:
  当前family是否执行

rd_value:
  当前family想写入rd的值

new_pc_value:
  当前family想设置的新pc

trapped / trap_reason:
  当前family是否产生trap
```

ADD的`CommonDiffs`包含`rd_value=res`和`new_pc_value=Default`。Jump或branch会提供非默认pc。Load会提供从RAM读取或sign extension后的rd值。Store通常不写rd，它修改slot 2成为RAM写入。

所有family都登记候选结果，writeback阶段按flag选择最终状态。这个设计让一行CPU trace覆盖多种RISC-V指令。

## 4.13 pc+4默认更新和jump/branch覆盖

`optimized_base_isa_state_transition`调用`calculate_pc_next_no_overflows(cs, pc)`计算默认下一条pc。源码创建`pc_next_low`变量，给它登记16-bit range check，构造低16位加4的carry约束，计算`pc_next_high`，并约束高半部分不等于`2^16`。

ADD使用默认pc更新：

```text id="svy35y"
pc = 0x0000
next_pc = 0x0004
```

Jump和branch family会在`CommonDiffs`里提供新的pc候选。writeback调用`CommonDiffs::select_final_pc_value`，在候选pc和默认`pc+4`之间选择最终pc。源码随后构造`MinimalStateRegistersInMemory { pc: new_pc }`作为最终状态。

## 4.14 writeback绑定rd地址和值

`opt_ctx.enforce_all(cs)`把延迟关系写进Circuit后，`optimized_base_isa_state_transition`调用`writeback_no_exception_with_opcodes_in_rom`合并opcode family结果。

writeback先选择最终rd值：

```rust id="a9jwkm"
let new_reg_val = CommonDiffs::select_final_rd_value(cs, &application_results);
```

随后代码用opcode格式flag构造`update_rd`。R-type、I-type、J-type、U-type写rd；B-type不写rd。

RISC-V规定x0恒为0。writeback检查rd是否为0，并把写入x0的值mask成0。源码创建`reg_is_zero = cs.is_zero(Num::Var(rd))`，然后用`1 - reg_is_zero`乘以`new_reg_val`生成真正写入寄存器的值。

writeback随后约束第三个memory query槽位的地址和值：

```rust id="ohw74k"
cs.add_constraint((rd_constraint.clone() - Term::from(address[0])) * update_rd.clone());
cs.add_constraint((Term::from(address[1])) * update_rd.clone());
```

这两条约束表示：当前opcode写rd时，slot 2的地址低16位等于rd，高16位等于0。源码接着约束slot 2的write value等于`reg_write_value_low/high`。

ADD x5,x1,x2得到：

```text id="rv9ymd"
rd = 5
update_rd = 1
new_reg_val = 16

slot 2 address_low = 5
slot 2 address_high = 0
slot 2 write_value = 16
```

B-type不写rd，源码单独约束slot 2地址和值为0，把branch建模成写x0。

## 4.15 writeback把三个query登记进CircuitOutput

writeback在处理完rd和pc后调用：

```rust id="ev149f"
cs.add_shuffle_ram_query(rs1_query);
cs.add_shuffle_ram_query(rs2_or_mem_load_query);
cs.add_shuffle_ram_query(rd_or_mem_store_query);
```

这三行把预分配并修改后的三个query写入`BasicAssembly.shuffle_ram_queries`。

后续`OneRowCompiler::compile_inner::<false>`要求main circuit正好有三个shuffle RAM query。源码里对main path断言`shuffle_ram_queries.len() == 3`。

```text id="z9stta"
describe_state_transition
  -> 预分配三个query
  -> opcode family修改query
  -> writeback登记三个query
  -> CircuitOutput.shuffle_ram_queries.len() == 3
```

三槽位设计把ADD、LW、SW放进同一个CPU row形状。ADD使用三个寄存器槽位；LW把slot 1改成RAM read；SW把slot 2改成RAM write。

## 4.16 ADD x5,x1,x2的完整状态转移对象

假设bytecode第0个word是：

```text id="vk0t5w"
pc = 0x0000
instruction = ADD x5, x1, x2
```

执行时寄存器状态：

```text id="ucgjuz"
x1 = 7
x2 = 9
old x5 = 100
```

CPU row包含这些对象。

ROM读取：

```text id="hmhftr"
pc_high -> RomAddressSpaceSeparator
  is_ram_range = 0
  rom_address_low = 0

pc_low + 2^16 * rom_address_low -> RomRead
  opcode_low16
  opcode_high16
```

decoder：

```text id="rx30z8"
opcode -> OptimizedDecoder
  invalid_opcode = 0
  R-type flag = 1
  ADD_OP_KEY flag = 1
  rs1 = 1
  rs2 = 2
  rd = 5
```

operand和query：

```text id="prn9ps"
slot 0:
  address = x1
  read_value = 7

slot 1:
  address = x2
  read_value = 9

slot 2:
  address = x5
  read_value = 100
  write_value = 16
```

ADD relation：

```text id="rub9qr"
src1 = 7
src2 = 9
res = 16

低16位约束：
  7 + 9 - 16 - 2^16 * 0 = 0

高16位约束：
  0 + 0 + 0 - 0 - 2^16 * 0 = 0
```

writeback：

```text id="xsp66a"
new_reg_val = 16
rd = 5
rd != 0
slot 2 address = 5
slot 2 write_value = 16
new_pc = pc + 4 = 0x0004
```

CircuitOutput保存这些关系的电路形式：

```text id="gtkdyc"
constraints:
  pc range check
  ROM address construction
  invalid opcode constraint
  ADD low/high limb constraints
  rd address/write value constraints
  pc+4 constraints
  boolean/range constraints

lookups:
  RomAddressSpaceSeparator
  RomRead
  decoder辅助lookup
  range/bit辅助lookup

shuffle_ram_queries:
  read x1
  read x2
  write x5
```

真实数值7、9、16属于witness trace。setup trace提供RomRead、decoder、range等固定表内容。CompiledCircuitArtifact提供这些约束和query的列布局。

## 4.17 CircuitOutput等待OneRowCompiler排成列布局

第四章产生的`CircuitOutput`仍然使用`Variable`编号。`OneRowCompiler`之后会把这些编号映射到列地址，生成`witness_layout`、`memory_layout`、`setup_layout`、`stage_2_layout`、约束布局、public input位置、table offsets等对象。`CompiledCircuitArtifact`字段里包含这些layout和约束结果。

```text id="lxmfu3"
第四章：
  Machine语义 -> CircuitOutput
  对象：Variable、Term、Constraint、LookupQuery、ShuffleRamMemQuery

第三章：
  CircuitOutput -> CompiledCircuitArtifact
  对象：layout、constraint columns、table offsets、setup layout
```

`get_machine`的结果可以重新理解为：

```text id="atb62t"
Machine代码定义RISC-V规则。
BasicAssembly记录变量、约束、lookup、memory query。
OneRowCompiler给这些对象分配列位置。
CompiledCircuitArtifact保存列布局和约束布局。
```

`get_table_driver`保留另一条线：

```text id="jhx97h"
TableDriver保存真实固定表内容。
SetupPrecomputations把固定表内容写入setup trace。
CPU row里的lookup query必须匹配setup trace里的表行。
```

## 4.18 本章替换旧版第四章后的阅读顺序

新版第四章已经融合旧版第四章内容，不需要回读旧版。

阅读顺序变成：

```text id="j44hyz"
第三章：
  get_machine / get_table_driver / setup trace
  理解固定证明环境

第四章：
  Variable / Num / Boolean / Term / Constraint
  理解Machine代码怎样写约束
  理解pc查ROM、decoder、三槽位shuffle RAM query、ADD/LW/SW如何进入CircuitOutput

第五章：
  witness生成怎样给Variable赋值
  execution trace怎样满足第四章登记的约束和query
```

第四章完成后，读Airbender的CPU电路代码会有两个坐标：看到`Term`和`Constraint`时，定位到多项式约束；看到`ShuffleRamMemQuery`时，定位到寄存器/RAM访问；看到`LookupQuery`时，定位到setup trace固定表查询；看到`CommonDiffs`时，定位到opcode family的候选状态更新。

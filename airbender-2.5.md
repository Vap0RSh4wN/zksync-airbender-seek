我重新看了真实源码。结论先说清楚：你的建议是对的，`tools/cli/src/setup.rs`目前不应该作为主阅读入口。它确实更像一个setup缓存封装：里面定义`SetupCache`，按bytecode hash缓存main/reduced/delegation setup，并在缓存miss时调用`setups::get_main_riscv_circuit_setup`、`get_reduced_riscv_circuit_setup`和`all_delegation_circuits_precomputations`。但我在当前仓库里搜索`SetupCache get_or_create_main_circuit`，只找到`tools/cli/src/setup.rs`自身，没有看到它被`Commands::Prove`主路径调用。`Commands::Prove`真正走的是`main.rs -> create_proofs -> create_proofs_internal -> setups::get_main_riscv_circuit_setup`这条线。 

所以第二章要改成：**从真实proving入口读到main RISC-V setup**。`setup.rs`只放在旁支说明里，不作为主线。

## 第二章阅读顺序重排

现在主线改成下面这样：

```text
tools/cli/src/main.rs
  -> Commands::Prove
  -> create_proofs(...)

tools/cli/src/prover_utils.rs
  -> create_proofs(...)
  -> create_proofs_internal(...)

create_proofs_internal(...)
  -> Machine::Standard 分支
  -> setups::get_main_riscv_circuit_setup(...)
  -> setups::all_delegation_circuits_precomputations(...)
  -> prover_examples::prove_image_execution(...)

circuit_defs/setups/src/circuits/main_riscv/mod.rs
  -> get_main_riscv_circuit_setup(...)

circuit_defs/risc_v_cycles/src/lib.rs
  -> get_machine(...)
  -> get_table_driver(...)
  -> witness_eval_fn_for_gpu_tracer(...)
  -> generate_artifacts(...)

cs/src/machine/...
  -> 约束定义本体
```

这个顺序有两个好处。第一，它从用户实际执行`airbender prove`时的路径开始，不会误读一个没接入主路径的缓存封装。第二，它不会一下跳进`cs/src/machine`，而是先看到setup到底把哪些东西交给prover，这样后面读约束定义时更清楚每个对象最终服务于哪里。

## 第2章 从CLI到main RISC-V setup

Prove命令到main RISC-V setup的执行序列是：

```text
tools/cli/src/main.rs
  -> Commands::Prove
  -> fetch_input_data(input)
  -> create_proofs(...)

tools/cli/src/prover_utils.rs
  -> load_binary_from_path(bin)
  -> get_padded_binary(...)
  -> create_proofs_internal(...)

create_proofs_internal(...)
  -> Machine::Standard
  -> get_main_riscv_circuit_setup(...)
  -> all_delegation_circuits_precomputations(...)
  -> prove_image_execution(...)
```

第二章只处理入口对象怎样变成setup输入：CLI字段、外部输入、padded bytecode、cycle upper bound、Machine分支、CPU Standard setup。约束系统内部结构从第3章开始展开。

### 2.1 CLI入口：Commands::Prove

代码位置：

```text
tools/cli/src/main.rs
```

这个文件首先从`cli_lib::prover_utils`引入几个和证明相关的函数，其中包括：

```text
create_proofs
create_final_proofs_from_program_proof
generate_oracle_data_from_metadata
serialize_to_file
u32_from_hex_string
ProvingLimit
DEFAULT_CYCLES
```

CLI主文件负责解析命令和参数，证明工作由prover_utils.rs执行。源码里这些导入在文件开头可以直接看到。

`Commands`枚举里有很多子命令。和当前学习主线最相关的是`Prove`：

```text
Prove {
  bin,
  input,
  output_dir,
  final_proof_name,
  machine,
  prev_metadata,
  cycles,
  until,
  mode,
  tmp_dir,
  gpu,
}
```

这些字段都会影响证明路径。`bin`是要证明的RISC-V binary；`input`是非确定输入，可以来自文件或RPC；`machine`选择机器类型，默认是`standard`；`cycles`控制最多跑多少RISC-V cycles；`gpu`决定走CPU还是GPU proving路径；`until`、`mode`、`tmp_dir`主要和递归证明有关。源码中`Prove`命令的字段定义在`Commands`枚举里。

machine字段的类型从execution_utils::Machine引入。这个枚举后面会影响create_proofs_internal进入Standard、Reduced还是ReducedLog23分支。Machine枚举定义了Standard、Reduced、ReducedLog23和ReducedFinal四种类型。

主函数main里先初始化logger，然后Cli::parse解析命令行。Commands::Prove分支先调用fetch_input_data(input)，再调用create_proofs。

代码位置：

```text
/home/ars/zksync-airbender-seek/tools/cli/src/main.rs
```

```rust
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .format_module_path(false)
        .format_target(false)
        .init();
    let cli = Cli::parse();
    match &cli.command {
        Commands::Prove {
            bin,
            input,
            output_dir,
            final_proof_name,
            machine,
            prev_metadata,
            cycles,
            until,
            mode,
            tmp_dir,
            gpu,
        } => {
            let input_data = fetch_input_data(input).expect("Failed to fetch");
            create_proofs(
                bin,
                output_dir,
                final_proof_name,
                input_data,
                prev_metadata,
                machine,
                cycles,
                until,
                *mode,
                tmp_dir,
                gpu.clone(),
            );
        }
        // 省略代码
    }
}
```

上游输入是CLI argv解析出的Commands::Prove字段。当前分支只做两次转交：fetch_input_data把InputConfig转成Option<Vec<u32>>，create_proofs接收binary路径、输入、Machine选择和cycle上限。下游setup函数还没有出现；main.rs不读取binary内容，也不创建Worker。

### 2.2 输入数据：fetch_input_data做了什么

Prove命令允许输入来自文件，也允许来自RPC。fetch_input_data根据input_file或input_rpc选择读取方式。Hex输入每8个十六进制字符解析为一个u32；ProverInputJson输入从JSON字段prover_input取base64字节串，再按4字节小端切成Vec<u32>。

代码位置：

```text
/home/ars/zksync-airbender-seek/tools/cli/src/main.rs
```

```rust
fn fetch_input_data(input: &InputConfig) -> Result<Option<Vec<u32>>, reqwest::Error> {
    let (data, input_type) = if let Some(input_file) = &input.input_file {
        (
            Some(fs::read_to_string(input_file).unwrap().trim().to_string()),
            input.input_type.clone(),
        )
    } else if let Some(url) = &input.input_rpc {
        (fetch_data_from_json_rpc(&url)?, InputType::ProverInputJson)
    } else {
        return Ok(None);
    };

    match input_type {
        InputType::Hex => Ok(data.map(|d| u32_from_hex_string(&d))),
        InputType::ProverInputJson => {
            if let Some(data) = data {
                let json: Value = serde_json::from_str(&data).expect("Failed to parse JSON");
                let prover_input = json["prover_input"].as_str().unwrap_or_default();

                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&prover_input)
                    .expect("Failed to decode base64 input");

                let prover_input: Vec<u32> = decoded
                    .chunks_exact(4)
                    .map(|chunk| u32::from_le_bytes(chunk.try_into().unwrap()))
                    .collect();
                Ok(Some(prover_input))
            } else {
                Ok(None)
            }
        }
    }
}
```

上游输入是InputConfig。当前函数返回Option<Vec<u32>>：没有输入源时返回None，文件/RPC输入成功解析后返回Some。Vec<u32>的字序和后面QuasiUARTSource一致，guest读外部输入时按u32消费这些值。

这段输入会变成后面的non_determinism_data。guest程序除了binary本身，还可以读取非确定输入。第二章只跟踪数据容器：

```text
input_data: Option<Vec<u32>>
  |
  v
non_determinism_data: Vec<u32>
  |
  v
QuasiUARTSource
```

create_proofs_internal会把这些u32逐个push进QuasiUARTSource.oracle。

### 2.3 create_proofs：证明的第一层封装

代码位置：

```text
tools/cli/src/prover_utils.rs
```

create_proofs接收main.rs转交的CLI对象。它仍然不编译约束；它把文件路径和CLI参数转换成create_proofs_internal需要的对象。

代码位置：

```text
/home/ars/zksync-airbender-seek/tools/cli/src/prover_utils.rs
```

```rust
pub fn create_proofs(
    bin_path: &String,
    output_dir: &String,
    final_proof_name: &String,
    input_data: Option<Vec<u32>>,
    prev_metadata: &Option<String>,
    machine: &Machine,
    cycles: &Option<usize>,
    until: &Option<ProvingLimit>,
    recursion_mode: RecursionStrategy,
    tmp_dir: &Option<String>,
    use_gpu: bool,
) {
    let prev_metadata: Option<ProofMetadata> = prev_metadata
        .as_ref()
        .map(|prev_metadata| deserialize_from_file(&prev_metadata));

    let binary = load_binary_from_path(bin_path);

    let num_instances = (cycles.unwrap_or(DEFAULT_CYCLES) / risc_v_cycles::NUM_CYCLES) + 1;

    let non_determinism_data = input_data.unwrap_or_default();

    // 省略代码
}
```

上游输入是binary路径、输入数据、Machine选择、cycle上限、递归参数和GPU开关。当前函数产生四个关键对象：prev_metadata、binary、num_instances、non_determinism_data。下游create_proofs_internal按Machine选择setup和proving路径。

binary来自load_binary_from_path：

代码位置：

```text
/home/ars/zksync-airbender-seek/tools/cli/src/prover_utils.rs
```

```rust
pub fn load_binary_from_path(path: &String) -> Vec<u32> {
    let mut file = std::fs::File::open(path).expect("must open provided file");
    let mut buffer = vec![];
    file.read_to_end(&mut buffer).expect("must read the file");
    get_padded_binary(&buffer)
}
```

load_binary_from_path读取磁盘文件字节，然后调用get_padded_binary。

代码位置：

```text
/home/ars/zksync-airbender-seek/execution_utils/src/lib.rs
```

```rust
pub fn get_padded_binary(binary: &[u8]) -> Vec<u32> {
    let mut bytecode = binary
        .as_chunks::<4>()
        .0
        .iter()
        .map(|el| u32::from_le_bytes(*el))
        .collect();
    trace_and_split::setups::pad_bytecode_for_proving(&mut bytecode);

    bytecode
}
```

上游输入是原始binary bytes。当前函数按4字节小端生成Vec<u32>，再调用pad_bytecode_for_proving把bytecode扩展到main ROM上界。下游risc_v_cycles::get_machine_for_rom_bound会检查bytecode长度，ROM table也使用这个padded bytecode生成。

Airbender的main circuit接受固定ROM容量的bytecode。risc_v_cycles::get_machine_for_rom_bound会检查：

```text
bytecode.len() == MAX_ROM_SIZE / 4
```

也就是bytecode必须已经被pad到固定ROM容量。`risc_v_cycles`里`MAX_ROM_SIZE = 1 << 21`字节，因此bytecode的`u32`长度应为：

[
\frac{2^{21}}{4}=2^{19}
]

源码里`MAX_ROM_SIZE`定义为`1 << 21`，`get_machine_for_rom_bound`也明确assert bytecode长度等于对应ROM容量除以4。 

bytecode符号如下：

```text
B = padded bytecode，类型 Vec<u32>
|B| = MAX_ROM_SIZE / 4 = 2^19
```

这个`B`后面会进入ROM table。

num_instances用cycles和NUM_CYCLES计算：

```text
num_instances = cycles / risc_v_cycles::NUM_CYCLES + 1
```

源码里DEFAULT_CYCLES=32_000_000。create_proofs使用cycles.unwrap_or(DEFAULT_CYCLES)计算num_instances。

而`risc_v_cycles::NUM_CYCLES`定义为：

```text
DOMAIN_SIZE - 1
```

其中：

```text
DOMAIN_SIZE = 1 << 22
```

所以每个main RISC-V proof instance大约覆盖：

[
2^{22}-1
]

个RISC-V cycles。源码里这些常量在`circuit_defs/risc_v_cycles/src/lib.rs`开头。

NUM_CYCLES=DOMAIN_SIZE-1与trace布局有关：trace长度是2²²，每个main proof chunk使用trace_len-1个真实cycle，剩余边界行服务状态衔接和约束边界。第8章的prove_image_execution_for_machine_with_gpu_tracers也用trace_len-1计算cycles_per_circuit。

input_data.unwrap_or_default把None变成空Vec。CLI没有传输入时，guest仍然可以执行不读取非确定输入的程序；如果程序读取了输入，VM执行阶段会从空QuasiUARTSource读取并触发对应执行错误或约束不满足。

gpu开关决定是否创建GpuSharedState。不用GPU时gpu_state为None，Machine::Standard分支进入CPU路径并直接调用get_main_riscv_circuit_setup。启用GPU时GpuSharedState::new创建GPU execution prover，Machine::Standard分支会走commit_memory_and_prove，绕开CPU分支中的setup函数调用。

最后，`create_proofs`调用：

```text
create_proofs_internal(
  &binary,
  non_determinism_data,
  machine,
  num_instances,
  prev_metadata.map(...),
  &mut gpu_state,
  &mut total_proof_time,
)
```

create_proofs_internal随后选择真实proving分支。

### 2.4 create_proofs_internal：真实选择main RISC-V setup的位置

create_proofs_internal接收的对象已经完成CLI解析和文件读取：

```text
binary: &Vec<u32>
non_determinism_data: Vec<u32>
machine: &Machine
num_instances: usize
prev_end_params_output: Option<...>
gpu_shared_state: &mut Option<&mut GpuSharedState>
total_proof_time: &mut Option<f64>
```

源码里函数签名在`prover_utils.rs`中。

代码位置：

```text
/home/ars/zksync-airbender-seek/tools/cli/src/prover_utils.rs
```

```rust
pub fn create_proofs_internal(
    binary: &Vec<u32>,
    non_determinism_data: Vec<u32>,
    machine: &Machine,
    num_instances: usize,
    prev_end_params_output: Option<([u32; 8], Option<[u32; 16]>)>,
    gpu_shared_state: &mut Option<&mut GpuSharedState>,
    total_proof_time: &mut Option<f64>,
) -> (ProofList, ProofMetadata) {
    let worker = worker::Worker::new();

    let mut non_determinism_source = QuasiUARTSource::default();

    for entry in non_determinism_data {
        non_determinism_source.oracle.push_back(entry);
    }

    let (proof_list, register_values) = match machine {
        Machine::Standard => {
            // 省略代码
        }
        Machine::Reduced => {
            // 省略代码
        }
        Machine::ReducedLog23 => {
            // 省略代码
        }
        Machine::ReducedFinal => {
            panic!("Should only be used in final proof generation.");
        }
    };

    // 省略代码
}
```

上游输入是padded binary、non_determinism_data、Machine枚举和num_instances。当前函数创建Worker，并把non_determinism_data转入QuasiUARTSource.oracle。Worker会被setup函数和prover使用；QuasiUARTSource会被VM执行路径消费。

Airbender把外部输入抽象成UART-like oracle source。VM执行时，guest读取非确定输入会从这个source取u32。

```text
CLI input
  -> Vec<u32>
  -> QuasiUARTSource.oracle
  -> VM / prover witness path
```

match machine决定setup和证明机器。Machine::Standard进入base proving；Machine::Reduced和Machine::ReducedLog23服务后续递归层；Machine::ReducedFinal不应通过create_proofs_internal进入，代码直接panic。

### 2.5 Machine::Standard 的CPU路径

Machine::Standard对应base proving，也就是用main RISC-V circuit证明guest program的执行。create_proofs_internal进入这个分支后先检查prev_end_params_output。basic proof没有上一层递归输出，传入prev metadata会直接panic。这个检查把base proving和后面的reduced/recursive proving分开。

代码位置：

```text
/home/ars/zksync-airbender-seek/tools/cli/src/prover_utils.rs
```

```rust
pub fn create_proofs_internal(
    binary: &Vec<u32>,
    non_determinism_data: Vec<u32>,
    machine: &Machine,
    num_instances: usize,
    prev_end_params_output: Option<([u32; 8], Option<[u32; 16]>)>,
    gpu_shared_state: &mut Option<&mut GpuSharedState>,
    total_proof_time: &mut Option<f64>,
) -> (ProofList, ProofMetadata) {
    // 省略代码

    let (proof_list, register_values) = match machine {
        Machine::Standard => {
            if prev_end_params_output.is_some() {
                panic!("Are you sure that you want to pass --prev-metadata to basic proof?");
            }
            let (basic_proofs, delegation_proofs, register_values) =
                if let Some(gpu_shared_state) = gpu_shared_state {
                    // 省略代码
                } else {
                    let main_circuit_precomputations =
                        setups::get_main_riscv_circuit_setup::<Global, Global>(&binary, &worker);
                    let delegation_precomputations =
                        setups::all_delegation_circuits_precomputations::<Global, Global>(&worker);

                    prover_examples::prove_image_execution(
                        num_instances,
                        &binary,
                        non_determinism_source,
                        &main_circuit_precomputations,
                        &delegation_precomputations,
                        &worker,
                    )
                };

            // 省略代码
        }
        // 省略代码
    };

    // 省略代码
}
```

上游输入已经完成两次转换。binary来自load_binary_from_path，类型是Vec<u32>，并且已经按main ROM上界padding。non_determinism_data已经写入QuasiUARTSource，后续VM执行从这个source读取外部输入。worker在create_proofs_internal开头创建，用于setup预计算和证明阶段并行任务。

CPU路径由gpu_shared_state是否为空决定。gpu_shared_state为None时，函数在当前线程路径创建main circuit setup和delegation setup，然后调用prove_image_execution。这个分支没有经过tools/cli/src/setup.rs里的SetupCache；SetupCache是缓存封装，当前CPU base proving直接调用setups crate。

main setup和delegation setup都必须早于prove_image_execution创建。main_circuit_precomputations依赖binary，因为ROM表必须由当前program bytecode生成。delegation_precomputations不依赖binary，只依赖delegation circuit定义和worker。prove_image_execution同时需要binary、非确定输入、main setup和delegation setup，才能执行VM、生成main witness、收集delegation witness并产生proof。

```text
setups::get_main_riscv_circuit_setup::<Global, Global>(&binary, &worker)

setups::all_delegation_circuits_precomputations::<Global, Global>(&worker)

prover_examples::prove_image_execution(...)
```

CPU标准路径的对象流是：

```text
create_proofs_internal
  |
  v
match machine
  |
  v
Machine::Standard
  |
  v
gpu_shared_state == None
  |
  +-- get_main_riscv_circuit_setup(binary, worker)
  |
  +-- all_delegation_circuits_precomputations(worker)
  |
  +-- prove_image_execution(
        num_instances,
        binary,
        non_determinism_source,
        main_circuit_precomputations,
        delegation_precomputations,
        worker
      )
```

get_main_riscv_circuit_setup返回main_circuit_precomputations。这个对象保存编译后的main RISC-V约束系统、ROM/CSR lookup table内容、FFT/LDE预计算、setup固定列预计算，以及witness evaluator函数指针。它在prove_image_execution之前创建，并且使用当前binary生成program-specific ROM表。

all_delegation_circuits_precomputations返回delegation_precomputations。当前default machine会创建BLAKE2和BigInt delegation circuit setup。main circuit本身只发出DelegatedComputationRequest，具体delegation witness和delegation proof由prove_image_execution后半段处理。这个分工使main RISC-V circuit可以通过CSR记录请求，delegation circuit再证明对应的专用计算和memory访问。

prove_image_execution的签名显示了两个setup对象怎样被消费。

代码位置：

```text
/home/ars/zksync-airbender-seek/circuit_defs/prover_examples/src/lib.rs
```

```rust
pub fn prove_image_execution<
    ND: NonDeterminismCSRSource<VectorMemoryImplWithRom>,
    A: GoodAllocator,
>(
    num_instances_upper_bound: usize,
    bytecode: &[u32],
    non_determinism: ND,
    risc_v_circuit_precomputations: &MainCircuitPrecomputations<IMStandardIsaConfig, A>,
    delegation_circuits_precomputations: &[(u32, DelegationCircuitPrecomputations<A>)],
    worker: &worker::Worker,
) -> (Vec<Proof>, Vec<(u32, Vec<Proof>)>, Vec<FinalRegisterValue>) {
    prove_image_execution_for_machine_with_gpu_tracers::<ND, IMStandardIsaConfig, A>(
        num_instances_upper_bound,
        bytecode,
        non_determinism,
        risc_v_circuit_precomputations,
        delegation_circuits_precomputations,
        worker,
    )
}
```

上游输入来自Machine::Standard CPU分支。当前函数把MachineConfig固定为IMStandardIsaConfig，然后进入prove_image_execution_for_machine_with_gpu_tracers。函数名保留gpu_tracers，但CPU路径也使用这个执行trace和witness入口。函数内部会调用trace_execution_for_gpu、evaluate_witness和prove。

prove_image_execution的返回值对应ProofList字段。它返回三项：

```text
Vec<Proof>
  main RISC-V circuit proofs，对应create_proofs_internal里的basic_proofs。

Vec<(u32, Vec<Proof>)>
  delegation proofs，按delegation type id分组，对应delegation_proofs。

Vec<FinalRegisterValue>
  最终寄存器状态，对应register_values，后续写入ProofMetadata。
```

Machine::Standard分支随后把basic_proofs和delegation_proofs放进ProofList。reduced_proofs和reduced_log_23_proofs保持空数组，因为base proving还没有进入递归压缩机器。register_values会进入ProofMetadata，CLI后续序列化metadata时使用它记录guest执行结束后的寄存器状态。

Machine::Standard CPU分支的边界由此确定：它创建setup对象，调用base proof入口，返回basic_proofs、delegation_proofs和register_values。约束编译属于get_main_riscv_circuit_setup内部；VM执行和witness/proof属于prove_image_execution内部。

### 2.6 Reduced和ReducedLog23分支先怎么处理

`Machine::Reduced`和`Machine::ReducedLog23`分支结构和`Standard`很像，但它们调用的是不同setup函数：

```text
get_reduced_riscv_circuit_setup
get_reduced_riscv_log_23_circuit_setup
```

源码中`Reduced`分支调用`get_reduced_riscv_circuit_setup`，`ReducedLog23`分支调用`get_reduced_riscv_log_23_circuit_setup`。 

Reduced和ReducedLog23主要服务递归证明层或更小机器配置。main RISC-V约束系统的入口仍然是Machine::Standard。

Airbender包含多种机器配置。execution_utils::Machine枚举里有Standard、Reduced、ReducedLog23等机器类型；Circuit Entry Points文档也列出多种main machine configurations，包括full ISA、with delegation、without signed mul/div、minimal等配置。

所以后面说“main RISC-V circuit”时，需要分清：

```text
Standard main RISC-V machine:
  主要用于base proving。

Reduced / ReducedLog23:
  主要用于递归或缩小约束规模。

Delegation circuits:
  BLAKE2、BigInt等专用电路，由main machine通过CSR请求触发。
```

### 2.7 tools/cli/src/setup.rs：为什么不作为主线读

tools/cli/src/setup.rs定义SetupCache。

这个文件定义了：

```text
SetupCache<A, B>
  main_circuit_setup
  reduced_circuit_setup
  delegations
  delegation_evals
```

它的`get_or_create_main_circuit`会用bytecode hash作为key，如果缓存中没有，就新建worker，调用：

```text
setups::get_main_riscv_circuit_setup(bytecode, worker)
```

随后还调用：

```text
create_circuit_setup(&setup.setup.ldes[0].trace)
```

把setup里的trace拿去生成某种evaluation cache。源码对应在`setup.rs`里。 

`get_or_create_reduced_circuit`和`get_or_create_delegations`也做类似事情，分别缓存reduced setup和delegation setup。 

但目前我没有在主`Commands::Prove`路径里看到它。代码搜索`SetupCache get_or_create_main_circuit`也只返回`tools/cli/src/setup.rs`本身。

所以读法调整为：

```text
主线：
  main.rs -> prover_utils.rs -> get_main_riscv_circuit_setup

旁支：
  setup.rs 作为缓存封装了解即可。
  等后面遇到外部工具、服务端缓存或GPU setup复用时再回来读。
```

setup.rs有工程价值，但当前Commands::Prove主路径不经过它。缓存、Arc、HashMap、eval cache这些工程封装属于旁支，main RISC-V约束系统入口在prover_utils.rs和setups crate。

## 第2.8节 get_main_riscv_circuit_setup第一眼看什么

代码位置：

```text
/home/ars/zksync-airbender-seek/circuit_defs/setups/src/circuits/main_riscv/mod.rs
```

这个文件非常短，只有一个函数。真实源码里`get_main_riscv_circuit_setup`的主体只有几十行。

函数签名是：

```text
get_main_riscv_circuit_setup<A: GoodAllocator, B: GoodAllocator>(
    bytecode: &[u32],
    worker: &Worker,
) -> MainCircuitPrecomputations<IMStandardIsaConfig, A, B>
```

参数和返回值先在入口处绑定清楚。

`A: GoodAllocator`和`B: GoodAllocator`是内存分配器类型参数。Airbender大量使用大数组、FFT/LDE buffer、trace buffer和GPU/CPU不同内存布局，所以很多预计算对象都参数化在allocator上。CPU路径里调用的是：

```text
::<Global, Global>
```

也就是普通全局allocator。`create_proofs_internal`里标准CPU路径正是这样调用的。

bytecode: &[u32]是已经padding好的RISC-V program ROM。原始ELF bytes经过load_binary_from_path和get_padded_binary后，变成按4字节小端排列的u32数组。

`worker: &Worker`用于并行预计算。后面`Twiddles::new`、`LdePrecomputations::new`和`SetupPrecomputations::from_tables_and_trace_len`都会用它。

返回值是：

```text
MainCircuitPrecomputations<IMStandardIsaConfig, A, B>
```

这个结构在`setups/src/lib.rs`里定义，包含六个字段：

```text
compiled_circuit
table_driver
twiddles
lde_precomputations
setup
witness_eval_fn_for_gpu_tracer
```

源码列出了这些字段。

get_main_riscv_circuit_setup不执行guest程序，也不生成witness。它构造precomputations，给后续prove_image_execution使用。

返回值结构如下：

```text
MainCircuitPrecomputations
  |
  +-- compiled_circuit
  |     编译后的main RISC-V约束系统描述
  |
  +-- table_driver
  |     lookup tables，包括ROM表、CSR delegation表等
  |
  +-- twiddles
  |     FFT / LDE需要的旋转因子
  |
  +-- lde_precomputations
  |     LDE domain和coset相关预计算
  |
  +-- setup
  |     根据tables和trace length生成的setup commitment / trees / LDE trace
  |
  +-- witness_eval_fn_for_gpu_tracer
        GPU tracer用的witness evaluator函数指针
```

### 2.9 get_main_riscv_circuit_setup逐行解释

代码位置：

```text
/home/ars/zksync-airbender-seek/circuit_defs/setups/src/circuits/main_riscv/mod.rs
```

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

上游输入来自Machine::Standard CPU分支：bytecode是padded Vec<u32>的切片，worker由create_proofs_internal创建。当前函数完成四类转换：编译machine、生成table_driver、创建FFT/LDE预计算、生成setup固定列预计算。下游MainCircuitPrecomputations被prove_image_execution消费。

delegation_csrs取自IMStandardIsaConfig：

```text
delegation_csrs = IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS
```

它决定main RISC-V machine允许哪些CSR触发delegation。risc_v_cycles/src/lib.rs也把ALLOWED_DELEGATION_CSRS导出为IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS。

main machine只允许白名单里的CSR触发precompile。后面会生成CSR properties table，证明当前CSR调用属于允许的delegation集合。

machine来自risc_v_cycles::get_machine：

```text
machine = risc_v_cycles::get_machine(bytecode, delegation_csrs)
```

返回类型被标注为：

```text
cs::one_row_compiler::CompiledCircuitArtifact<Mersenne31Field>
```

get_machine把RISC-V machine约束描述编译成CompiledCircuitArtifact。它进入risc_v_cycles crate，创建ROM表、CSR表，然后调用default_compile_machine编译FullIsaMachineWithDelegationNoExceptionHandling。

`risc_v_cycles::get_machine`本身只是转发到`get_machine_for_rom_bound`。

table_driver来自risc_v_cycles::get_table_driver：

```text
table_driver = risc_v_cycles::get_table_driver(bytecode, delegation_csrs)
```

`table_driver`只构造lookup tables，不编译全部machine。源码里这一行紧接着`get_machine`。

compiled_circuit和table_driver分属两个对象：

```text
compiled_circuit:
  描述约束长什么样。
  例如这一列和那一列要满足加法关系，某个lookup要查RomRead表。

table_driver:
  保存具体lookup table内容。
  例如当前bytecode对应的ROM表，当前允许delegation CSR对应的CSR properties表。
```

在ADD例子中，ADD本身的加法约束属于compiled circuit；但“pc=0x2000对应instruction=ADD x5,x1,x2”属于ROM lookup table内容，放在table driver里。

twiddles来自DOMAIN_SIZE：

```text
twiddles = Twiddles::new(DOMAIN_SIZE, worker)
```

`DOMAIN_SIZE`来自`risc_v_cycles`，值是(2^{22})。 

twiddles是FFT需要的预计算旋转因子。Airbender后端需要把trace多项式做LDE和commitment；setup阶段先准备这些FFT辅助数据。

符号上可以记：

[
H = 2^{22}
]

H是main RISC-V trace domain size。每个instance可执行cycle数是：

[
N = H - 1
]

lde_precomputations来自DOMAIN_SIZE、LDE_FACTOR和LDE_SOURCE_COSETS：

```text
lde_precomputations = LdePrecomputations::new(
    DOMAIN_SIZE,
    LDE_FACTOR,
    LDE_SOURCE_COSETS,
    worker,
)
```

源码里`LDE_FACTOR = 2`，`LDE_SOURCE_COSETS = &[0,1]`。 

LDE是low-degree extension。先不展开FRI，只用最小解释：

```text
原始trace domain大小是 H。
为了做低度测试和commitment，后端会在更大的domain上评价这些多项式。
LDE_FACTOR=2 表示扩展到大约 2H 的评价域。
```

这属于后端接口，但setup阶段要提前准备。

setup来自table_driver和machine.setup_layout：

```text
setup = SetupPrecomputations::from_tables_and_trace_len(
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

SetupPrecomputations::from_tables_and_trace_len从table_driver、trace长度、machine.setup_layout、twiddles、LDE预计算和Merkle cap size生成setup precomputations。

machine.setup_layout来自CompiledCircuitArtifact。编译后的circuit不仅包含约束，还包含setup trace布局。SetupPrecomputations::from_tables_and_trace_len用这个布局和table contents生成setup阶段需要的trace、LDE和Merkle tree。

三类输入的关系如下：

```text
compiled circuit says:
  我需要哪些setup columns、lookup tables、固定列布局。

table_driver:
  保存这些lookup tables的具体内容。

SetupPrecomputations:
  根据布局和表内容生成固定列预计算。
```

返回值把这些对象打包：

```text
MainCircuitPrecomputations {
  compiled_circuit: machine,
  table_driver,
  twiddles,
  lde_precomputations,
  setup,
  witness_eval_fn_for_gpu_tracer: risc_v_cycles::witness_eval_fn_for_gpu_tracer,
}
```

源码返回这些字段。

create_proofs_internal把返回值命名为main_circuit_precomputations，然后传给prove_image_execution。

因此，`get_main_riscv_circuit_setup`的完整作用可以压缩成一句：

```text
根据当前bytecode和standard ISA delegation CSR白名单，函数编译main RISC-V约束系统，构造ROM/CSR lookup tables，准备FFT/LDE/setup commitment相关数据，并把这些对象打包给prover。
```

## 第2.10节 下钻一层：risc_v_cycles::get_machine

代码位置：

```text
/home/ars/zksync-airbender-seek/circuit_defs/risc_v_cycles/src/lib.rs
```

这个文件是main RISC-V circuit crate的外层入口。它先定义几个关键常量：

```text
DOMAIN_SIZE = 1 << 22
NUM_CYCLES = DOMAIN_SIZE - 1
LDE_FACTOR = 2
LDE_SOURCE_COSETS = &[0, 1]
TREE_CAP_SIZE = 32
MAX_ROM_SIZE = 1 << 21
```

源码里这些常量集中在文件开头。

这些常量以后会贯穿整套笔记。先统一符号：

```text
H = DOMAIN_SIZE = 2^22
N = NUM_CYCLES = H - 1
ρ = LDE_FACTOR = 2
ROM_BYTES = MAX_ROM_SIZE = 2^21
ROM_WORDS = ROM_BYTES / 4 = 2^19
```

`get_machine`调用：

```text
get_machine_for_rom_bound::<ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(bytecode, delegation_csrs)
```

源码里`ROM_ADDRESS_SPACE_SECOND_WORD_BITS`来自`MAX_ROM_SIZE.trailing_zeros() - 16`。 

真实函数如下：

```rust
pub fn get_machine(
    bytecode: &[u32],
    delegation_csrs: &[u32],
) -> one_row_compiler::CompiledCircuitArtifact<field::Mersenne31Field> {
    get_machine_for_rom_bound::<ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(bytecode, delegation_csrs)
}

pub fn get_machine_for_rom_bound<const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize>(
    bytecode: &[u32],
    delegation_csrs: &[u32],
) -> one_row_compiler::CompiledCircuitArtifact<field::Mersenne31Field> {
    assert_eq!(
        bytecode.len(),
        (1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS)) / 4
    );
    use crate::machine::machine_configurations::create_csr_table_for_delegation;
    use prover::cs::machine::machine_configurations::create_table_for_rom_image;
    use prover::cs::tables::TableType;

    let machine = FullIsaMachineWithDelegationNoExceptionHandling;
    let rom_table = create_table_for_rom_image::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(
        &bytecode,
        TableType::RomRead.to_table_id(),
    );
    let csr_table = create_csr_table_for_delegation(
        true,
        delegation_csrs,
        TableType::SpecialCSRProperties.to_table_id(),
    );

    let compiled_machine = default_compile_machine::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(
        machine,
        rom_table,
        Some(csr_table),
        DOMAIN_SIZE.trailing_zeros() as usize,
    );

    compiled_machine
}
```

上游输入是bytecode和delegation_csrs。当前函数先检查ROM长度，再创建FullIsaMachineWithDelegationNoExceptionHandling、ROM表、CSR表，最后调用default_compile_machine。下游返回CompiledCircuitArtifact，get_main_riscv_circuit_setup把它保存为compiled_circuit。

ROM_ADDRESS_SPACE_SECOND_WORD_BITS表示ROM地址高位部分的宽度。MAX_ROM_SIZE=2²¹ bytes，trailing_zeros()是21，减16后是5。main machine默认支持的ROM上界可以拆成低16位加5个高位。

进入`get_machine_for_rom_bound`后，第一件事情是检查bytecode长度：

```text
bytecode.len() == (1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS)) / 4
```

源码中有这个assert。

如果`ROM_ADDRESS_SPACE_SECOND_WORD_BITS = 5`，那么：

[
1 << (16+5) = 2^{21}
]

这是字节数；除以4以后是：

[
2^{19}
]

也就是`Vec<u32>`长度。这个检查保证bytecode已经pad到完整ROM容量。

第二件事是创建machine：

```text
machine = FullIsaMachineWithDelegationNoExceptionHandling
```

源码里把`Machine` type alias设成`FullIsaMachineWithDelegationNoExceptionHandling`，`formal_machine_for_compilation()`也返回这个类型的值。

名字很长，拆开看：

```text
FullIsa:
  支持完整IM类RISC-V指令集合。

WithDelegation:
  支持通过CSR调用delegation circuits。

NoExceptionHandling:
  假设trusted code，不处理trap/exception路径。
```

官方Circuit Entry Points文档也说明`FullIsaMachineWithDelegationNoExceptionHandling`是full ISA加delegation CSR calls，不包含exception handling logic。

第三件事是创建ROM表：

```text
create_table_for_rom_image(bytecode, TableType::RomRead.to_table_id())
```

源码中这一段在`get_machine_for_rom_bound`里。

create_table_for_rom_image把当前要证明的程序bytecode变成ROM lookup table。后面main circuit每个cycle根据pc查ROM，证明当前instruction来自这份bytecode。

用贯穿例子：

```text
pc = 0x2000
instruction = ADD x5, x1, x2
```

ROM表里应该有一项类似：

```text
RomRead(pc=0x2000, instruction_encoding=...)
```

ROM表由bytecode决定，属于program-specific setup data。create_table_for_rom_image的编码细节留到ROM表章节展开。

第四件事是创建CSR delegation表：

```text
create_csr_table_for_delegation(
  true,
  delegation_csrs,
  TableType::SpecialCSRProperties.to_table_id()
)
```

源码中这一段紧接ROM表。

这个表用于约束哪些CSR值是合法delegation调用。官方文档说delegation circuits通过专用CSR值被RISC-V程序调用，每个precompile有唯一`DELEGATION_TYPE_ID`，必须和程序写入的CSR值匹配。

最后调用default_compile_machine：

```text
default_compile_machine(
  machine,
  rom_table,
  Some(csr_table),
  DOMAIN_SIZE.trailing_zeros() as usize,
)
```

源码中这是get_machine_for_rom_bound的返回值。

default_compile_machine开始编译约束系统。它接收：

```text
machine:
  RISC-V machine配置和约束描述。

rom_table:
  当前程序的ROM lookup table。

csr_table:
  允许的delegation CSR table。

log_domain_size:
  DOMAIN_SIZE.trailing_zeros() = 22
```

返回：

```text
CompiledCircuitArtifact<Mersenne31Field>
```

也就是后面`get_main_riscv_circuit_setup`里的`compiled_circuit`。

所以`risc_v_cycles::get_machine`这条线可以画成：

```text
bytecode + delegation_csrs
  |
  +-- assert bytecode is padded to ROM bound
  |
  +-- create FullIsaMachineWithDelegationNoExceptionHandling
  |
  +-- create ROM table from bytecode
  |
  +-- create CSR delegation table
  |
  +-- default_compile_machine(...)
        |
        v
     CompiledCircuitArtifact
```

## 第2.11节 下钻一层：risc_v_cycles::get_table_driver

`get_table_driver`和`get_machine`接收相同输入：

```text
bytecode
delegation_csrs
```

然后转发到`get_table_driver_for_rom_bound`。

它同样先assert bytecode长度。随后：

```text
create_table_driver(machine)
create_table_for_rom_image(...)
table_driver.add_table_with_content(TableType::RomRead, ...)
create_csr_table_for_delegation(...)
table_driver.add_table_with_content(TableType::SpecialCSRProperties, ...)
```

源码里这些步骤在`get_table_driver_for_rom_bound`中。

真实函数如下：

代码位置：

```text
/home/ars/zksync-airbender-seek/circuit_defs/risc_v_cycles/src/lib.rs
```

```rust
pub fn get_table_driver(
    bytecode: &[u32],
    delegation_csrs: &[u32],
) -> prover::cs::tables::TableDriver<Mersenne31Field> {
    get_table_driver_for_rom_bound::<ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(bytecode, delegation_csrs)
}

pub fn get_table_driver_for_rom_bound<const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize>(
    bytecode: &[u32],
    delegation_csrs: &[u32],
) -> prover::cs::tables::TableDriver<Mersenne31Field> {
    assert_eq!(
        bytecode.len(),
        (1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS)) / 4
    );

    use crate::machine::machine_configurations::create_csr_table_for_delegation;
    use prover::cs::machine::machine_configurations::create_table_driver;
    use prover::cs::machine::machine_configurations::create_table_for_rom_image;
    use prover::cs::tables::LookupWrapper;
    use prover::cs::tables::TableType;

    let machine = FullIsaMachineWithDelegationNoExceptionHandling;
    let mut table_driver = create_table_driver::<_, _, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(machine);
    let rom_table = create_table_for_rom_image::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(
        &bytecode,
        TableType::RomRead.to_table_id(),
    );
    table_driver.add_table_with_content(TableType::RomRead, LookupWrapper::Dimensional3(rom_table));
    let csr_table = create_csr_table_for_delegation(
        true,
        delegation_csrs,
        TableType::SpecialCSRProperties.to_table_id(),
    );
    table_driver.add_table_with_content(
        TableType::SpecialCSRProperties,
        LookupWrapper::Dimensional3(csr_table),
    );

    table_driver
}
```

上游输入仍然是bytecode和delegation_csrs。当前函数创建TableDriver，并显式加入RomRead和SpecialCSRProperties两张带内容的表。下游get_main_riscv_circuit_setup把table_driver交给SetupPrecomputations::from_tables_and_trace_len；prove_image_execution里的evaluate_witness也会拿到同一个table_driver。

get_machine和get_table_driver的边界如下：

```text
get_machine:
  编译machine，并把ROM/CSR table传进compiler。
  输出CompiledCircuitArtifact。

get_table_driver:
  单独构造TableDriver。
  输出lookup table内容集合。
```

prover后面需要约束描述，也需要lookup table内容本身。SetupPrecomputations::from_tables_and_trace_len直接接收&table_driver；evaluate_witness也接收table_driver，为witness生成和lookup相关数据提供同一份表内容。

用例子解释：

```text
ADD约束:
  rd = rs1 + rs2
  这属于 compiled_circuit。

ROM约束:
  pc = 0x2000 时 instruction 是 ADD x5,x1,x2
  这需要 TableDriver 里的 RomRead table。

CSR delegation约束:
  某个CSR id 是否允许触发BLAKE2或BigInt delegation
  这需要 TableDriver 里的 SpecialCSRProperties table。
```

### 2.12 witness_eval_fn_for_gpu_tracer暂时怎么理解

`risc_v_cycles/src/lib.rs`里还有：

```text
witness_eval_fn_for_gpu_tracer(proxy)
```

它会调用`sealed::evaluate_witness_fn`，而这个函数来自：

```text
include!("../generated/witness_generation_fn.rs")
```

源码中可以看到`sealed`模块include了生成的witness函数，`witness_eval_fn_for_gpu_tracer`拿到函数指针后调用它。

Airbender使用生成的witness evaluation函数填充变量。至少GPU tracer命名的路径使用generated/witness_generation_fn.rs里的函数。get_main_riscv_circuit_setup最后把这个函数指针放进MainCircuitPrecomputations。

witness相关对象关系如下：

```text
compiled_circuit:
  描述约束。

witness_eval_fn_for_gpu_tracer:
  给定执行oracle / witness proxy，把具体witness值填进对应变量。
```

witness章节会继续追MainRiscVOracle、SimpleWitnessProxy和生成文件。

### 2.13 generate_artifacts：verifier layout生成入口

`risc_v_cycles/src/lib.rs`还有`generate_artifacts()`。它使用dummy bytecode生成compiled machine，然后写出：

```text
generated/layout
generated/circuit_layout.rs
generated/quotient.rs
```

源码中它先用全零dummy bytecode填满ROM大小，再调用`get_machine`，然后调用`verifier_generator::generate_for_description`生成layout和quotient代码。

每次prove不会执行generate_artifacts。电路代码变更后，开发者用它生成verifier/layout artifacts。官方Circuit Entry Points文档也说generate_artifacts用于刷新verifier layout和quotient source。

所以后面读主proving pipeline时先不深入它。但写最终笔记时要单独留一节：setup entry point不仅服务prover，也服务verifier artifact generation。

## 第二章小结

第二章确认的执行序列如下：

```text
Commands::Prove
  -> fetch_input_data
  -> create_proofs
  -> load_binary_from_path
  -> get_padded_binary
  -> create_proofs_internal
  -> Machine::Standard CPU branch
  -> get_main_riscv_circuit_setup
  -> all_delegation_circuits_precomputations
  -> prove_image_execution
```

tools/cli/src/setup.rs不在当前主prove路径中。它定义SetupCache，可以缓存main/reduced/delegation setup；目前搜索只看到它自身使用。服务端、缓存化prover或GPU封装如果引用SetupCache，再回到这个文件。

下一章应该正式进入：

```text
circuit_defs/setups/src/circuits/main_riscv/mod.rs
```

不过这个文件本身很短，所以第三章不能只读这41行。它必须沿着两条调用继续下钻：

```text
risc_v_cycles::get_machine
  -> get_machine_for_rom_bound
  -> create_table_for_rom_image
  -> create_csr_table_for_delegation
  -> default_compile_machine

risc_v_cycles::get_table_driver
  -> create_table_driver
  -> add RomRead table
  -> add SpecialCSRProperties table
```

第三章的目标会是：把`CompiledCircuitArtifact`、`TableDriver`、`MainCircuitPrecomputations`和`SetupPrecomputations`四个对象讲透。

## 第3章 从setup返回对象进入compiler边界

第三章从已经定位到的CPU证明入口开始：

```text
tools/cli/src/prover_utils.rs
  create_proofs_internal
    Machine::Standard + CPU
      -> setups::get_main_riscv_circuit_setup(binary, worker)
      -> setups::all_delegation_circuits_precomputations(worker)
      -> prover_examples::prove_image_execution(...)
```

这一章只解释get_main_riscv_circuit_setup怎样把padded bytecode变成prover需要的固定输入。opcode gadget、Term和Constraint从第4章开始展开。

阅读源码的顺序如下：

```text
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

cs/src/lib.rs
  default_compile_machine

cs/src/machine/machine_configurations/mod.rs
  compile_machine

cs/src/cs/circuit.rs
  CircuitOutput

cs/src/one_row_compiler/compile_layout.rs
  compile_output_for_chunked_memory_argument

cs/src/one_row_compiler/mod.rs
  CompiledCircuitArtifact

prover/src/prover_stages/mod.rs
  SetupPrecomputations::from_tables_and_trace_len
```

### 3.1 本章统一符号和对象分类

第二章已经得到get_main_riscv_circuit_setup的两个输入：

```text
bytecode: &[u32]
worker: &Worker
```

bytecode不是原始app.bin字节。create_proofs已经调用load_binary_from_path和get_padded_binary，把app.bin按4字节小端切成Vec<u32>，再pad到固定ROM上界。main RISC-V circuit使用：

```text
H = DOMAIN_SIZE = 2^22
N = NUM_CYCLES = H - 1
ρ = LDE_FACTOR = 2
ROM_BYTES = MAX_ROM_SIZE = 2^21
ROM_WORDS = ROM_BYTES / 4 = 2^19
```

H是trace domain大小。一个main circuit instance最多承载N个RISC-V cycle。ROM_WORDS是bytecode进入get_machine前必须满足的长度。

本章涉及四类数据：

```text
program-specific fixed data:
  当前bytecode生成的RomRead表，当前允许delegation CSR生成的SpecialCSRProperties表。

constraint description:
  Machine写出的变量、约束、lookup、memory query，经OneRowCompiler编译成CompiledCircuitArtifact。

setup precomputations:
  固定表内容按照setup_layout写入setup trace，做LDE并构造Merkle tree。

witness-time input:
  VM执行basic_fibonacci或dynamic_fibonacci产生的trace，不在get_main_riscv_circuit_setup中生成。
```

public、private和commitment在这一章的边界也要分开：

```text
bytecode:
  程序固定输入。它决定ROM表和setup tree；proof最终通过setup_tree_caps和end_pc绑定这份程序。

ROM / CSR lookup tables:
  固定表内容。prover不把它们当witness选择，setup阶段生成承诺。

witness trace:
  程序执行后产生。寄存器读写、RAM访问、opcode选择值都属于witness generation路径。

setup trees:
  对固定setup trace做Merkle commitment。Proof里的setup_tree_caps来自这里。
```

### 3.2 get_main_riscv_circuit_setup把五类对象装进MainCircuitPrecomputations

代码位置：

```text
circuit_defs/setups/src/circuits/main_riscv/mod.rs
```

函数签名：

```rust
pub fn get_main_riscv_circuit_setup<A: GoodAllocator, B: GoodAllocator>(
    bytecode: &[u32],
    worker: &Worker,
) -> MainCircuitPrecomputations<IMStandardIsaConfig, A, B> {
    ...
}
```

A和B是allocator类型。Airbender会分配大规模trace、FFT buffer、LDE buffer和Merkle tree输入。CPU路径传入Global, Global；GPU路径可能选择不同内存布局。

函数体按执行顺序构造五类对象：

```rust
let delegation_csrs = IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS;

let machine =
    risc_v_cycles::get_machine(bytecode, delegation_csrs);

let table_driver =
    risc_v_cycles::get_table_driver(bytecode, delegation_csrs);

let twiddles =
    Twiddles::new(risc_v_cycles::DOMAIN_SIZE, worker);

let lde_precomputations =
    LdePrecomputations::new(
        risc_v_cycles::DOMAIN_SIZE,
        risc_v_cycles::LDE_FACTOR,
        risc_v_cycles::LDE_SOURCE_COSETS,
        worker,
    );

let setup =
    SetupPrecomputations::from_tables_and_trace_len(
        &table_driver,
        risc_v_cycles::DOMAIN_SIZE,
        &machine.setup_layout,
        &twiddles,
        &lde_precomputations,
        risc_v_cycles::LDE_FACTOR,
        risc_v_cycles::TREE_CAP_SIZE,
        worker,
    );
```

返回结构在setups crate中定义：

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

字段按下游使用方式分类：

```text
compiled_circuit:
  OneRowCompiler输出的约束artifact。prove_image_execution和prover_stages::prove用它读取trace布局、约束布局、public input位置和setup_layout。

table_driver:
  所有lookup table内容。setup阶段用它生成固定setup trace；witness evaluator也能用它查表值和表内索引。

twiddles:
  FFT预计算。后端把trace evaluations转换到需要的domain时使用。

lde_precomputations:
  LDE预计算。后端把多项式从主domain评价到扩展domain时使用。

setup:
  固定setup trace的LDE结果和Merkle tree。Proof中的setup_tree_caps来自setup.trees。

witness_eval_fn_for_gpu_tracer:
  witness generation函数指针。它把MainRiscVOracle提供的执行数据写入witness trace；get_main_riscv_circuit_setup本身不执行VM。
```

basic_fibonacci例子中，bytecode来自examples/basic_fibonacci/app.bin。get_main_riscv_circuit_setup会为这份app.bin生成RomRead表和setup tree。dynamic_fibonacci同样走这个函数，只是后面的QuasiUARTSource含有input.txt里的n值；input.txt不会改变ROM表，除非程序binary本身变化。

### 3.3 get_machine生成CompiledCircuitArtifact

代码位置：

```text
circuit_defs/risc_v_cycles/src/lib.rs
```

get_main_riscv_circuit_setup调用：

```rust
let machine: cs::one_row_compiler::CompiledCircuitArtifact<Mersenne31Field> =
    risc_v_cycles::get_machine(bytecode, delegation_csrs);
```

get_machine只转发到带ROM上界的版本：

```rust
pub fn get_machine(
    bytecode: &[u32],
    delegation_csrs: &[u32],
) -> one_row_compiler::CompiledCircuitArtifact<field::Mersenne31Field> {
    get_machine_for_rom_bound::<ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(bytecode, delegation_csrs)
}
```

ROM_ADDRESS_SPACE_SECOND_WORD_BITS由MAX_ROM_SIZE计算：

```text
MAX_ROM_SIZE = 2^21 bytes
ROM_ADDRESS_SPACE_SECOND_WORD_BITS = trailing_zeros(2^21) - 16 = 5
```

get_machine_for_rom_bound先检查bytecode长度：

```rust
assert_eq!(
    bytecode.len(),
    (1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS)) / 4
);
```

代入5：

\[
\frac{2^{16+5}}{4}=2^{19}
\]

这个assert要求bytecode已经pad到2^19个u32。get_padded_binary完成这个pad。没有pad时，ROM表大小、setup_layout和证明电路的固定列无法对齐。

get_machine_for_rom_bound创建三项输入：

```rust
let machine = FullIsaMachineWithDelegationNoExceptionHandling;

let rom_table =
    create_table_for_rom_image::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(
        &bytecode,
        TableType::RomRead.to_table_id(),
    );

let csr_table =
    create_csr_table_for_delegation(
        true,
        delegation_csrs,
        TableType::SpecialCSRProperties.to_table_id(),
    );

let compiled_machine =
    default_compile_machine::<_, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(
        machine,
        rom_table,
        Some(csr_table),
        DOMAIN_SIZE.trailing_zeros() as usize,
    );
```

FullIsaMachineWithDelegationNoExceptionHandling指定main RISC-V machine支持的ISA和异常策略：

```text
FullIsa:
  RV32I + M相关指令，支持load/store、branch、jump、mul/div等。

WithDelegation:
  CSR可以触发BLAKE2、BigInt等delegation circuits。

NoExceptionHandling:
  trusted code模型。非法opcode、未对齐访问等不会走trap分支；约束无法满足时proof失败。
```

default_compile_machine的输出就是CompiledCircuitArtifact，get_main_riscv_circuit_setup把它保存到MainCircuitPrecomputations.compiled_circuit。

### 3.4 create_table_for_rom_image把bytecode写成RomRead表

代码位置：

```text
cs/src/machine/machine_configurations/mod.rs
```

ROM表函数：

```rust
pub fn create_table_for_rom_image<
    F: PrimeField,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    image: &[u32],
    id: u32,
) -> LookupTable<F, 3> {
    ...
}
```

LookupTable<F, 3>表示每行宽度为3的lookup table。RomRead表使用1个key列和2个value列：

```text
key:
  pc address，必须4字节对齐。

value:
  opcode_low_16
  opcode_high_16
```

源码注释也写了行形状：

```text
(0, image bytes 0..2, image bytes 2..4)
(4, image bytes 4..6, image bytes 6..8)
```

Airbender使用Mersenne31Field，模数是2^31 - 1。一个任意u32可能大于这个域模数，不能直接安全地放进一个field element。ROM表把32-bit opcode拆成两个16-bit值：

\[
opcode = opcode_{low16} + 2^{16}\cdot opcode_{high16}
\]

例子：某条指令编码为0x00b50533。

```text
opcode_low_16  = 0x0533
opcode_high_16 = 0x00b5
```

如果这条指令位于pc=0x00000004，RomRead表中对应行是：

```text
[4, 0x0533, 0x00b5]
```

create_table_for_rom_image遍历整个ROM地址空间：

```rust
let keys_len = 1usize << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS - 2);
```

代入5后：

\[
keys\_len = 2^{16+5-2}=2^{19}
\]

每个i对应地址i * 4。image范围内使用真实opcode，image范围外使用UNIMP_OPCODE。对于已经pad满的bytecode，image.len()等于keys_len。

RomRead表在约束中的作用是：每个cycle根据pc查出当前opcode的低16位和高16位。后续decoder约束使用这两个16-bit值判断这一行执行哪类指令。

### 3.5 create_csr_table_for_delegation把CSR白名单写成SpecialCSRProperties表

main RISC-V machine通过CSR调用delegation circuit。delegation circuit是独立电路，例如BLAKE2 compression和BigInt with control。guest程序把特定CSR值写入CSR指令，main circuit通过SpecialCSRProperties表判断该CSR是否允许触发delegation。

函数位置：

```text
cs/src/machine/machine_configurations/mod.rs
```

代码：

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

get_main_riscv_circuit_setup传入的delegation_csrs来自IMStandardIsaConfig::ALLOWED_DELEGATION_CSRS。这个白名单和setups::all_delegation_circuits_precomputations中的delegation_type_id必须一致，否则guest发出的delegation request和可证明的delegation circuit对不上。

basic_fibonacci不写CSR触发BLAKE2或BigInt，所以prove_image_execution不会产出delegation proof。hashed_fibonacci会触发BLAKE2 delegation，ProofMetadata.delegation_proof_count会出现对应delegation_type_id和proof数量。

### 3.6 LookupTable、LookupWrapper和TableDriver

RomRead表和SpecialCSRProperties表都以LookupTable形式存在。

代码位置：

```text
cs/src/tables.rs
```

LookupTable核心字段：

```rust
pub struct LookupTable<F: PrimeField, const N: usize> {
    pub name: String,
    pub lookup_data: Arc<HashMap<LookupKey<F, N>, LookupValue<F, N>>>,
    pub content_data: Arc<HashMap<DataKey<F, N>, usize>>,
    pub data: Arc<Vec<[F; N]>>,
    pub quick_value_lookup_fn: ValueLookupFn<F, N>,
    pub quick_index_lookup_fn: IndexLookupFn<F, N>,
    pub num_key_columns: usize,
    pub num_value_columns: usize,
    pub id: u32,
}
```

字段用途：

```text
data:
  表的完整行内容。SetupPrecomputations最终把这些行写进setup trace。

lookup_data:
  key到value的HashMap。witness evaluator可用key查value。

content_data:
  完整行到行号的HashMap。lookup multiplicity和setup编码需要行索引。

num_key_columns / num_value_columns:
  表行如何切分。例如RomRead是1个key列、2个value列。

id:
  TableType对应的数值ID。TableDriver用ID把不同表放进固定槽位。
```

LookupWrapper把不同宽度的LookupTable统一成一个枚举：

```rust
pub enum LookupWrapper<F: PrimeField> {
    Uninitialized,
    Dimensional1(LookupTable<F, 1>),
    Dimensional2(LookupTable<F, 2>),
    Dimensional3(LookupTable<F, 3>),
}
```

TableType定义所有表类型：

```rust
pub enum TableType {
    ZeroEntry = 0,
    OpTypeBitmask,
    ...
    RomAddressSpaceSeparator,
    RomRead,
    SpecialCSRProperties,
    ...
}
```

TableDriver持有所有表：

```rust
pub struct TableDriver<F: PrimeField> {
    pub tables: [LookupWrapper<F>; TABLE_TYPES_UPPER_BOUNDS],
    offsets_for_multiplicities: [usize; TABLE_TYPES_UPPER_BOUNDS],
    pub total_tables_len: usize,
}
```

TableDriver::add_table_with_content检查传入表的id和TableType匹配：

```rust
let id = table.get_table_id() as usize;
assert_eq!(id, table_type.to_table_id() as usize);
```

TableDriver::dump_tables把所有表拼成Vec<[F; 4]>。前3列来自具体表行，第4列保存table id。SetupPrecomputations使用dump_tables生成generic lookup setup columns。

### 3.7 get_table_driver生成证明和setup共用的表内容

risc_v_cycles::get_table_driver与get_machine使用相同输入：

```rust
pub fn get_table_driver(
    bytecode: &[u32],
    delegation_csrs: &[u32],
) -> TableDriver<Mersenne31Field> {
    get_table_driver_for_rom_bound::<ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(bytecode, delegation_csrs)
}
```

get_table_driver_for_rom_bound执行：

```text
1. 检查bytecode长度等于ROM_WORDS。
2. create_table_driver(machine)生成通用表、decoder表和ROM地址辅助表。
3. create_table_for_rom_image生成RomRead表。
4. add_table_with_content加入RomRead。
5. create_csr_table_for_delegation生成SpecialCSRProperties表。
6. add_table_with_content加入SpecialCSRProperties。
```

create_table_driver(machine)会注册机器使用的普通表：

```rust
let used_tables = M::define_used_tables();
for table in used_tables.into_iter() {
    table_driver.materialize_table(table);
}

table_driver.materialize_table(TableType::And);
table_driver.materialize_table(TableType::ZeroEntry);
table_driver.materialize_table(TableType::QuickDecodeDecompositionCheck4x4x4);
table_driver.materialize_table(TableType::QuickDecodeDecompositionCheck7x3x6);
table_driver.materialize_table(TableType::U16GetSignAndHighByte);
table_driver.materialize_table(TableType::RangeCheckSmall);

let decoder_table = M::create_decoder_table(TableType::OpTypeBitmask.to_table_id());
table_driver.add_table_with_content(TableType::OpTypeBitmask, LookupWrapper::Dimensional3(decoder_table));
```

RomRead和SpecialCSRProperties不在create_table_driver内部加入，因为它们依赖当前bytecode和delegation_csrs。get_table_driver_for_rom_bound把这两张program-specific表补进去。

get_machine也会接收RomRead和SpecialCSRProperties，但目的不同：

```text
get_machine:
  把Machine描述和program-specific表交给default_compile_machine，输出CompiledCircuitArtifact。

get_table_driver:
  构造后续setup和witness evaluation要使用的TableDriver，保存表内容。
```

两者重复创建RomRead和SpecialCSRProperties，输出服务不同边界：compiled_circuit描述约束布局，table_driver提供表内容。

### 3.8 default_compile_machine把Machine写进BasicAssembly

代码位置：

```text
cs/src/lib.rs
```

default_compile_machine接收四个输入：

```rust
pub fn default_compile_machine<
    M: crate::machine::Machine<Mersenne31Field>,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    machine: M,
    bytecode_table: LookupTable<Mersenne31Field, 3>,
    csr_table: Option<LookupTable<Mersenne31Field, 3>>,
    trace_len_log2: usize,
) -> CompiledCircuitArtifact<Mersenne31Field>
```

输入含义：

```text
machine:
  FullIsaMachineWithDelegationNoExceptionHandling。它知道支持哪些opcode，怎样写一行状态转移。

bytecode_table:
  RomRead表，当前程序的ROM内容。

csr_table:
  SpecialCSRProperties表，当前允许的delegation CSR白名单。

trace_len_log2:
  log2(H)。main RISC-V为22。
```

函数执行：

```rust
let mut cs_output = compile_machine::<
    Mersenne31Field,
    BasicAssembly<Mersenne31Field>,
    M,
    ROM_ADDRESS_SPACE_SECOND_WORD_BITS,
>(machine);

cs_output.table_driver.add_table_with_content(
    TableType::RomRead,
    LookupWrapper::Dimensional3(bytecode_table),
);

if let Some(csr_table) = csr_table {
    cs_output.table_driver.add_table_with_content(
        TableType::SpecialCSRProperties,
        LookupWrapper::Dimensional3(csr_table),
    );
}

let compiler = OneRowCompiler::default();
let compiler_output =
    compiler.compile_output_for_chunked_memory_argument(cs_output, trace_len_log2);
```

BasicAssembly是Circuit trait的一个实现。Machine代码不会直接写最终AIR列，它向BasicAssembly申请变量、添加Constraint、添加lookup、添加memory query。BasicAssembly最后finalize成CircuitOutput。

### 3.9 compile_machine生成CircuitOutput

代码位置：

```text
cs/src/machine/machine_configurations/mod.rs
```

compile_machine的主体：

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
```

create_table_driver_into_cs把通用表注册到Circuit。它和create_table_driver做的事情相似，但目标对象不同：create_table_driver生成独立TableDriver，create_table_driver_into_cs把表写进BasicAssembly内部。

M::describe_state_transition写出单cycle RISC-V状态转移。main machine的实现位于：

```text
cs/src/machine/machine_configurations/full_isa_with_delegation_no_exceptions/mod.rs
```

它最终调用optimized_base_isa_state_transition。第5章会展开opcode、decoder和OptimizationContext。

CircuitOutput是compile_machine的输出：

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

字段按AIR语义分类：

```text
state_input / state_output:
  跨行状态。main RISC-V通常把pc等最小状态连接到下一行。

constraints:
  多项式等式约束。每个Constraint最终表示一个在有效行上等于0的表达式。

lookups:
  普通lookup查询。例如range check、decoder、ROM read等。

shuffle_ram_queries:
  RAM和register统一memory argument的查询。

delegated_computation_requests:
  main circuit发出的delegation请求。

boolean_vars:
  需要满足b*(b-1)=0的布尔变量。

substitutions:
  Placeholder到Variable的映射，witness generation和生成代码会用到。
```

CircuitOutput仍然以Variable编号描述约束。它还没有决定变量落在哪一列，也没有把约束拆成后端能直接评价的ColumnAddress。

### 3.10 OneRowCompiler把CircuitOutput编译成CompiledCircuitArtifact

代码位置：

```text
cs/src/one_row_compiler/compile_layout.rs
```

default_compile_machine调用：

```rust
let compiler = OneRowCompiler::default();
let compiler_output =
    compiler.compile_output_for_chunked_memory_argument(cs_output, trace_len_log2);
```

compile_output_for_chunked_memory_argument内部进入compile_inner。源码注释列出它的任务：

```rust
// - place variables in particular grid places
// - select whether they go into witness subtree or memory subtree
// - normalize constraints to address particular columns insteap of variable indexes
// - try to apply some heuristrics
```

这四句对应AIR编译边界：

```text
Variable -> ColumnAddress:
  gadget阶段只知道Variable编号；compiler决定每个变量在trace矩阵中的列位置。

witness subtree / memory subtree / setup subtree:
  不同类别的列进入不同布局。memory argument和setup列需要特殊处理。

Constraint -> CompiledDegree1Constraint / CompiledDegree2Constraint:
  约束从变量表达式变成列地址表达式。

lookup / RAM / delegation layouts:
  compiler把CircuitOutput中的查询容器组织成stage_2_layout、memory_layout等结构。
```

CompiledCircuitArtifact定义在cs/src/one_row_compiler/mod.rs：

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

字段按后续消费者分组：

```text
witness_layout:
  witness trace列布局。witness_eval_fn_for_gpu_tracer写这些列。

memory_layout:
  shuffle RAM相关列布局。

setup_layout:
  fixed setup trace的列布局。SetupPrecomputations使用它写固定列。

stage_2_layout:
  lookup和memory argument第二阶段需要的列布局。

degree_1_constraints / degree_2_constraints:
  已经按ColumnAddress编译的约束。prover和verifier评价这些约束。

state_linkage_constraints:
  相邻行之间的状态连接，例如上一行final pc等于下一行initial pc。

public_inputs:
  public input出现在哪些边界行和列。例如最终PC会参与end_params计算。

variable_mapping:
  原始Variable到ColumnAddress的映射。witness generation需要知道变量对应哪一列。

trace_len:
  H = 2^22。

table_offsets / total_tables_size:
  所有lookup table拼接后的偏移，用于lookup multiplicity和setup编码。
```

CompiledCircuitArtifact不会保存guest执行结果。basic_fibonacci执行后x10=144属于witness trace和public output约定；compiled_circuit只描述“某些列必须满足RISC-V一步执行关系”。

### 3.11 SetupLayout描述固定列怎样写入setup trace

SetupLayout定义在cs/src/definitions/setup_tree.rs：

```rust
pub struct SetupLayout {
    pub timestamp_setup_columns: ColumnSet<NUM_TIMESTAMP_COLUMNS_FOR_RAM>,
    pub range_check_16_setup_column: ColumnSet<1>,
    pub timestamp_range_check_setup_column: ColumnSet<1>,
    pub generic_lookup_setup_columns: ColumnSet<NUM_COLUMNS_FOR_COMMON_TABLE_WIDTH_SETUP>,
    pub total_width: usize,
}
```

SetupLayout::layout_for_lookup_size根据所有lookup table总长度和trace_len计算generic_lookup_setup_columns需要多少列组：

```rust
let encoding_capacity = trace_len - 1;
let mut num_required_setup_tuples = lookups_total_table_len / encoding_capacity;
if lookups_total_table_len % encoding_capacity != 0 {
    num_required_setup_tuples += 1;
}
```

每个setup列组最多编码trace_len - 1行表内容。最后一行不用于普通setup内容，因为prover_stages会调整c0相关值。这个设计解释了为什么NUM_CYCLES = DOMAIN_SIZE - 1频繁出现：主domain的最后一行保留给边界或协议处理。

SetupLayout包含三类固定列：

```text
timestamp_setup_columns:
  shuffle RAM时间戳相关固定列。

range_check_16_setup_column / timestamp_range_check_setup_column:
  范围检查固定表。

generic_lookup_setup_columns:
  TableDriver.dump_tables拼接出来的普通lookup表内容，包括RomRead、decoder、CSR properties等。
```

### 3.12 SetupPrecomputations把TableDriver写入setup trace并承诺

代码位置：

```text
prover/src/prover_stages/mod.rs
```

SetupPrecomputations结构：

```rust
pub struct SetupPrecomputations<const N: usize, A: GoodAllocator, T: MerkleTreeConstructor> {
    pub ldes: Vec<CosetBoundTracePart<N, A>>,
    pub trees: Vec<T>,
}
```

from_tables_and_trace_len执行：

```rust
let mut main_domain_trace =
    Self::get_main_domain_trace(table_driver, trace_len, setup_layout, worker);

adjust_to_zero_c0_var_length(&mut main_domain_trace, 0..setup_layout.total_width, worker);

let ldes = compute_wide_ldes(
    main_domain_trace,
    twiddles,
    lde_precomputations,
    0,
    lde_factor,
    worker,
);

for domain in ldes.iter() {
    let tree = T::construct_for_coset(&domain.trace, subtree_cap_size, true, worker);
    trees.push(tree);
}
```

get_main_domain_trace把TableDriver写进RowMajorTrace：

```rust
let main_domain_trace =
    RowMajorTrace::new_zeroed_for_size(trace_len, setup_layout.total_width, A::default());

let all_generic_tables = table_driver.dump_tables();

for (tuple_idx, encoding_chunk) in all_generic_tables_ref.iter().enumerate() {
    if absolute_row_idx < encoding_chunk.len() {
        let table_row = encoding_chunk[absolute_row_idx];
        let range = setup_layout
            .generic_lookup_setup_columns
            .get_range(tuple_idx);
        trace_view_row[range].copy_from_slice(&table_row);
    }
}
```

RowMajorTrace按行保存setup固定列。每一行是一组field elements。generic lookup setup columns中，每个表行占4个field elements：

```text
[table_column_0, table_column_1, table_column_2, table_id]
```

RomRead原始宽度是3，dump_tables加上table_id变成4列。这样prover后续可以把所有lookup table统一编码成同一类generic lookup setup。

from_tables_and_trace_len的输出：

```text
ldes:
  setup trace在各个LDE coset上的评价结果。LDE_FACTOR=2时有两个coset。

trees:
  每个coset的Merkle tree。Proof中的setup_tree_caps来自这些trees的cap。
```

setup属于固定列承诺。它不包含guest执行时的寄存器值。basic_fibonacci和dynamic_fibonacci如果使用相同app.bin和相同circuit版本，setup由bytecode和表决定；dynamic_fibonacci输入n变化不会改变setup。

### 3.13 Twiddles和LdePrecomputations属于后端预计算

Twiddles定义在fft/src/row_major/precomputes.rs：

```rust
pub struct Twiddles<E: TwoAdicField, A: GoodAllocator> {
    pub forward_twiddles: Vec<E, A>,
    pub forward_twiddles_not_bitreversed: Vec<E, A>,
    pub inverse_twiddles: Vec<E, A>,
    pub omega: E,
    pub omega_inv: E,
    pub domain_size: usize,
    pub grinded_fft_forward_twiddles: Radix4,
    pub grinded_fft_inverse_twiddles: Radix4,
}
```

omega是大小为domain_size的单位根：

\[
\omega^H = 1
\]

Twiddles::new预计算FFT和inverse FFT需要的幂。证明后端会把trace values和setup trace values转换到多项式或扩展评价域。

LdePrecomputations保存从某个source coset到LDE cosets所需的幂和coset offset：

```rust
pub struct LdePrecomputations<A: GoodAllocator> {
    pub domain_bound_precomputations: Vec<Option<DomainBoundLdePrecomputations<A>>>,
    pub domain_size: usize,
    pub lde_factor: usize,
}
```

main RISC-V使用：

```text
DOMAIN_SIZE = 2^22
LDE_FACTOR = 2
LDE_SOURCE_COSETS = [0, 1]
```

后端使用这些预计算检查低度性质和构造FRI proof。第三章只需要记录它们的边界：get_main_riscv_circuit_setup准备FFT/LDE数据，约束语义不在Twiddles和LdePrecomputations中。

### 3.14 get_machine和get_table_driver的双路径关系

get_main_riscv_circuit_setup同时调用get_machine和get_table_driver。两条路径使用同一个bytecode和delegation_csrs：

```text
bytecode + delegation_csrs
  -> get_machine
       -> create ROM table
       -> create CSR table
       -> default_compile_machine
       -> CompiledCircuitArtifact

bytecode + delegation_csrs
  -> get_table_driver
       -> create ordinary tables
       -> create decoder table
       -> create ROM table
       -> create CSR table
       -> TableDriver
```

CompiledCircuitArtifact和TableDriver之间的关系：

```text
CompiledCircuitArtifact:
  约束和列布局。它描述某个lookup查询应查哪类表、某个变量落在哪个ColumnAddress。

TableDriver:
  表内容。它保存RomRead、SpecialCSRProperties、decoder、range等表的真实行。

SetupPrecomputations:
  使用CompiledCircuitArtifact.setup_layout和TableDriver生成固定setup trace、LDE和Merkle tree。
```

ADD指令例子：

```text
RomRead表行:
  [pc, opcode_low16, opcode_high16, table_id]

decoder表行:
  [opcode bit decomposition pieces, opcode flags, table_id]

constraint:
  当AddOp flag = 1时，rd_value = rs1_value + rs2_value
```

RomRead表和decoder表属于setup固定数据；rd_value、rs1_value、rs2_value属于witness trace；AddOp flag由opcode解码约束和lookup共同约束。

### 3.15 prove_image_execution消费MainCircuitPrecomputations

第三章的返回点是prover_examples::prove_image_execution。它接收：

```rust
prove_image_execution(
    num_instances,
    &binary,
    non_determinism_source,
    &main_circuit_precomputations,
    &delegation_precomputations,
    &worker,
)
```

main_circuit_precomputations中的字段在后续证明中分工如下：

```text
compiled_circuit:
  决定trace列数、public input位置、约束评价、memory layout。

table_driver:
  提供lookup表内容和行索引。

setup:
  提供固定列的LDE结果和Merkle commitments。

twiddles / lde_precomputations:
  支持trace和setup trace的FFT/LDE。

witness_eval_fn_for_gpu_tracer:
  根据执行trace填充witness列。
```

prove_image_execution先执行RISC-V程序并收集CycleData，再生成witness trace，随后调用prover_stages::prove产生Proof。执行trace和witness trace不是setup函数的输出；setup函数只准备证明所需的固定结构和固定表承诺。

basic_fibonacci贯穿例子：

```text
app.bin
  -> padded bytecode
  -> RomRead table
  -> setup trace
  -> setup Merkle tree

VM执行
  -> x10 = 144
  -> witness trace
  -> proof public input / register_values
```

dynamic_fibonacci只改变VM执行输入：

```text
input.txt = 0007a120
  -> fetch_input_data
  -> QuasiUARTSource.oracle = [500000]
  -> csr_read_word() returns 500000
```

这个输入不会进入get_main_riscv_circuit_setup。它进入prove_image_execution的non_determinism_source，属于witness generation路径。

### 3.16 第三章阅读检查点

读完第三章后，源码对象应当形成以下对应：

```text
get_main_riscv_circuit_setup
  返回MainCircuitPrecomputations。

get_machine
  返回CompiledCircuitArtifact，包含约束布局和setup_layout。

get_table_driver
  返回TableDriver，包含具体lookup table内容。

default_compile_machine
  把Machine + ROM/CSR表送入BasicAssembly和OneRowCompiler。

compile_machine
  调用M::describe_state_transition，生成CircuitOutput。

OneRowCompiler
  把CircuitOutput中的Variable、Constraint、LookupQuery、MemoryQuery编译成ColumnAddress布局。

SetupPrecomputations::from_tables_and_trace_len
  把TableDriver中的固定表写入setup trace，做LDE，构造Merkle tree。
```

第四章进入Term、Constraint和Variable。那一章解释Machine代码怎样把RISC-V语义写成多项式约束。

## 第4章 约束表达式模型：Term、Constraint、Variable

Airbender的RISC-V gadget不直接写多项式字符串。machine代码使用Variable代表trace中的一个未知值，使用Term表达单项式，使用Constraint表达若干Term的和。Circuit::add_constraint接收Constraint，最终约束含义是该多项式在每个有效cycle上等于0。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/constraint.rs
```

```rust
pub enum Term<F: PrimeField> {
    Constant(F),
    Expression {
        coeff: F,
        inner: [Variable; TERM_INNER_CAPACITY],
        degree: usize,
    },
}
```

Term::Constant表示常数项。Term::Expression表示一个单项式：

\[
coeff \cdot \prod_{i=0}^{degree-1} inner_i
\]

inner数组最多保存4个变量，degree给出实际使用的变量数量。Term允许中间表达式临时到4次，这是为了让Rust运算符重载在构造表达式时有足够空间。Constraint在normalize后要求最高次数不超过2。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/constraint.rs
```

```rust
pub struct Constraint<F: PrimeField> {
    pub terms: Vec<Term<F>>,
}

impl<F: PrimeField> Constraint<F> {
    #[track_caller]
    pub fn normalize(&mut self) {
        self.terms.iter_mut().for_each(|el| el.normalize());
        self.terms.sort();

        let initial_degree = self.degree();

        let mut combined: Vec<Term<F>> = Vec::with_capacity(self.terms.len());
        for el in self.terms.drain(..) {
            let mut did_combine = false;
            for existing in combined.iter_mut() {
                if existing.combine(&el) {
                    existing.normalize();
                    did_combine = true;
                    break;
                }
            }
            if did_combine {
                continue;
            } else {
                combined.push(el);
            }
        }

        self.terms = combined
            .into_iter()
            .filter(|el| el.is_zero() == false)
            .collect();
        let final_degree = self.degree();
        assert!(final_degree <= 2);

        if final_degree == 0 && self.terms == vec![Term::Constant(F::ZERO)] {
            *self = Constraint::empty();
            return;
        }

        self.terms.iter_mut().for_each(|el| el.normalize());
        self.terms.sort();

        assert!(final_degree <= initial_degree);
    }
}
```

上游输入是gadget构造出的表达式，例如a+b-c或flag*(x-y)。当前函数排序变量、合并同类项、删除零项，并检查最终次数不超过2。下游compiler把Constraint拆成线性项、二次项和常数项，放进quotient布局。

RISC-V约束里常见的形态有三类：

```text
线性关系：
  x + y - z = 0

布尔约束：
  b * (b - 1) = 0

带选择器的条件约束：
  flag * (x - y) = 0
```

条件约束解释了为什么opcode选择可以保持低次数。flag是布尔变量，x-y是线性表达式，乘积是二次约束。某个opcode未执行时flag=0，这条约束自动满足；该opcode执行时flag=1，约束要求x=y。

Constraint还提供split_max_quadratic。compiler使用这个形状把多项式拆成二次项、线性项和常数项。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/constraint.rs
```

```rust
pub fn split_max_quadratic(mut self) -> (Vec<(F, Variable, Variable)>, Vec<(F, Variable)>, F) {
    self.normalize();
    let mut quadratic_terms = Vec::with_capacity(self.terms.len());
    let mut linear_terms = Vec::with_capacity(self.terms.len());
    let mut constant_term = F::ZERO;
    let mut constant_used = false;
    for term in self.terms.into_iter() {
        match term.degree() {
            2 => {
                let Term::Expression {
                    coeff,
                    inner,
                    degree,
                } = term
                else {
                    panic!();
                };
                assert_eq!(degree, 2);
                quadratic_terms.push((coeff, inner[0], inner[1]));
            }
            1 => {
                let Term::Expression {
                    coeff,
                    inner,
                    degree,
                } = term
                else {
                    panic!();
                };
                assert_eq!(degree, 1);
                linear_terms.push((coeff, inner[0]));
            }
            0 => {
                assert!(constant_used == false);
                constant_term = term.get_coef();
                constant_used = true;
            }
            a @ _ => {
                panic!("Degree {} is not supported", a);
            }
        }
    }

    (quadratic_terms, linear_terms, constant_term)
}
```

上游输入是已经normalize的Constraint。当前函数把每个Term按次数分类。下游one-row compiler使用这个三元组构造约束评价代码和quotient项。最高次数限制解释了后面opcode gadget为什么大量使用flag乘线性表达式，避免把多个条件、多项式乘在一起。

## 第5章 Machine接口和main RISC-V配置

Machine trait把一台RISC-V机器拆成三部分：支持哪些opcode，使用哪些lookup table，单cycle状态转移怎样写成Circuit约束。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/mod.rs
```

```rust
pub trait Machine<F: PrimeField>: 'static + Clone + Default {
    const ASSUME_TRUSTED_CODE: bool;
    const OUTPUT_EXACT_EXCEPTIONS: bool;
    const USE_ROM_FOR_BYTECODE: bool;

    type State: BaseMachineState<F>;

    fn all_supported_opcodes() -> Vec<Box<dyn DecodableMachineOp>>;

    fn define_used_tables() -> BTreeSet<TableType>;
    fn define_additional_tables(&self) -> Vec<(TableType, LookupWrapper<F>)> {
        vec![]
    }

    fn describe_state_transition<CS: Circuit<F>, const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize>(
        circuit: &mut CS,
    ) -> (Self::State, Self::State)
    where
        [(); { <Self as Machine<F>>::ASSUME_TRUSTED_CODE } as usize]:,
        [(); { <Self as Machine<F>>::OUTPUT_EXACT_EXCEPTIONS } as usize]:;
}
```

上游compile_machine以泛型M: Machine传入具体机器。当前trait要求机器列出opcode集合，并在describe_state_transition里向Circuit写变量、约束、lookup和memory query。下游compile_machine用返回的initial_state和final_state填state_input、state_output，OneRowCompiler再把这些状态变量连接到相邻trace行。

main RISC-V使用FullIsaMachineWithDelegationNoExceptionHandling。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/machine_configurations/full_isa_with_delegation_no_exceptions/mod.rs
```

```rust
impl<F: PrimeField> Machine<F> for FullIsaMachineWithDelegationNoExceptionHandling {
    const ASSUME_TRUSTED_CODE: bool = true;
    const OUTPUT_EXACT_EXCEPTIONS: bool = false;
    const USE_ROM_FOR_BYTECODE: bool = true;

    type State = MinimalStateRegistersInMemory<F>;

    fn all_supported_opcodes() -> Vec<Box<dyn DecodableMachineOp>> {
        vec![
            Box::new(AddOp),
            Box::new(SubOp),
            Box::new(LuiOp),
            Box::new(AuiPc),
            Box::new(BinaryOp),
            Box::new(MulOp::<true>),
            Box::new(DivRemOp::<true>),
            Box::new(ConditionalOp::<true>),
            Box::new(ShiftOp::<true, false>),
            Box::new(JumpOp),
            Box::new(LoadOp::<true, true>),
            Box::new(StoreOp::<true>),
            Box::new(CsrOp::<false, false, false>),
        ]
    }

    fn describe_state_transition<CS: Circuit<F>, const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize>(
        cs: &mut CS,
    ) -> (Self::State, Self::State)
    where
        [(); { <Self as Machine<F>>::ASSUME_TRUSTED_CODE } as usize]:,
        [(); { <Self as Machine<F>>::OUTPUT_EXACT_EXCEPTIONS } as usize]:,
    {
        let (splitting, _) = <Self as Machine<F>>::produce_decoder_table_stub();
        let boolean_keys = <Self as Machine<F>>::all_decoder_keys();

        optimized_base_isa_state_transition::<
            F,
            CS,
            { <Self as Machine<F>>::ASSUME_TRUSTED_CODE },
            { <Self as Machine<F>>::OUTPUT_EXACT_EXCEPTIONS },
            true,
            true,
            ROM_ADDRESS_SPACE_SECOND_WORD_BITS,
        >(
            cs,
            splitting,
            boolean_keys,
        )
    }
}
```

这段代码给出main machine的边界。ASSUME_TRUSTED_CODE=true表示电路按受信程序处理非法指令和异常，不覆盖完整trap系统。OUTPUT_EXACT_EXCEPTIONS=false表示最终状态不输出精确异常信息。USE_ROM_FOR_BYTECODE=true表示instruction来自ROM lookup。State选择MinimalStateRegistersInMemory，机器跨行状态只保存pc，寄存器和RAM通过shuffle RAM argument维护。

all_supported_opcodes列出ADD、SUB、LUI、AUIPC、Binary、Mul、DivRem、Conditional、Shift、Jump、Load、Store、CSR。这个列表同时影响decoder table和state transition。produce_decoder_table_stub会扫描opcode、funct3、funct7子空间，生成decoder table里的bitmask。describe_state_transition把splitting和boolean_keys传给optimized_base_isa_state_transition，后者写出单cycle电路。

## 第6章 单cycle状态转移：pc、decode、opcode diff、writeback

MinimalStateRegistersInMemory只保存pc。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/machine_configurations/minimal_state.rs
```

```rust
pub struct MinimalStateRegistersInMemory<F: PrimeField> {
    pub pc: Register<F>,
}

impl<F: PrimeField> MinimalStateRegistersInMemory<F> {
    pub fn initialize<CS: Circuit<F>>(circuit: &mut CS) -> Self {
        let pc = PcWrapper::<F>::initialize(circuit);

        Self { pc: pc.pc }
    }
}
```

上游describe_state_transition调用initialize。当前函数从PcWrapper分配pc相关变量，返回初始状态。下游optimized_base_isa_state_transition使用pc读ROM，writeback阶段构造final_state.pc。寄存器值不在State里跨行保存，单cycle内的寄存器读写通过三条ShuffleRamMemQuery进入memory argument。

状态转移入口如下。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/machine_configurations/full_isa_no_exceptions/optimized_state_transition.rs
```

```rust
pub(crate) fn optimized_base_isa_state_transition<
    F: PrimeField,
    CS: Circuit<F>,
    const ASSUME_TRUSTED_CODE: bool,
    const OUTPUT_EXACT_EXCEPTIONS: bool,
    const PERFORM_DELEGATION: bool,
    const SUPPORT_SIGNED_MUL_DIV: bool,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    cs: &mut CS,
    decode_table_splitting: [usize; 2],
    boolean_keys: DecoderOutputExtraKeysHolder,
) -> (
    MinimalStateRegistersInMemory<F>,
    MinimalStateRegistersInMemory<F>,
) {
    let initial_state = MinimalStateRegistersInMemory::<F>::initialize(cs);

    let pc = *initial_state.get_pc();

    cs.require_invariant(
        pc.0[0].get_variable(),
        Invariant::RangeChecked {
            width: LIMB_WIDTH as u32,
        },
    );

    let (memory_queries, src1, src2, raw_decoder_output, flags_source, opcode_types_bits) =
        optimized_decode_and_preallocate_mem_queries_for_bytecode_in_rom::<
            F,
            CS,
            ASSUME_TRUSTED_CODE,
            PERFORM_DELEGATION,
            ROM_ADDRESS_SPACE_SECOND_WORD_BITS,
        >(cs, pc, decode_table_splitting, boolean_keys);

    let next_pc = calculate_pc_next_no_overflows(cs, pc);

    let mut opt_ctx = OptimizationContext::<F, CS>::new();

    let src1 = RegisterDecompositionWithSign::parse_reg(cs, src1);
    let src2 = RegisterDecompositionWithSign::parse_reg(cs, src2);

    let decoder_output = BasicDecodingResultWithSigns {
        pc_next: next_pc,
        src1,
        src2,
        rs2_index: raw_decoder_output.rs2.clone(),
        imm: raw_decoder_output.imm,
        funct3: raw_decoder_output.funct3,
        funct12: raw_decoder_output.funct12,
    };

    // 省略代码
}
```

上游FullIsaMachineWithDelegationNoExceptionHandling::describe_state_transition传入decoder table splitting和boolean_keys。当前函数先初始化pc，对pc低16位加range check，然后读ROM并decode，同时预分配三条memory query。next_pc默认是pc+4。OptimizationContext用于复用查表和算术辅助变量。decoder_output把pc_next、src1、src2、rs2_index、imm、funct3、funct12整理成opcode gadget共享输入。

decode函数同时处理ROM读取、decoder lookup和memory query预分配。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/machine_configurations/state_transition_parts/decode_and_read_operands.rs
```

```rust
pub(crate) fn optimized_decode_and_preallocate_mem_queries_for_bytecode_in_rom<
    F: PrimeField,
    CS: Circuit<F>,
    const ASSUME_TRUSTED_CODE: bool,
    const PERFORM_DELEGATION: bool,
    const ROM_ADDRESS_SPACE_SECOND_WORD_BITS: usize,
>(
    cs: &mut CS,
    pc: Register<F>,
    decode_table_splitting: [usize; 2],
    boolean_keys: DecoderOutputExtraKeysHolder,
) -> (
    [ShuffleRamMemQuery; 3],
    Register<F>,
    Register<F>,
    OptimizedDecoderOutput<F>,
    BasicFlagsSource,
    [Boolean; NUM_INSTRUCTION_TYPES_IN_DECODE_BITS],
) {
    let next_opcode = read_opcode_from_rom::<F, CS, ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(cs, pc);

    let decoder_input = DecoderInput {
        instruction: next_opcode,
    };
    let (invalid_opcode, raw_decoder_output, opcode_format_bits, other_bits) =
        OptimizedDecoder::decode::<F, CS>(&decoder_input, cs, decode_table_splitting);

    if ASSUME_TRUSTED_CODE {
        cs.add_constraint_allow_explicit_linear_prevent_optimizations(Constraint::<F>::from(
            invalid_opcode,
        ));
    } else {
        unimplemented!()
    }

    let flags_source = BasicFlagsSource::new(boolean_keys, other_bits);

    // 省略代码

    (
        memory_queries.try_into().unwrap(),
        src1,
        src2,
        raw_decoder_output,
        flags_source,
        opcode_format_bits,
    )
}
```

上游输入是pc、decoder table layout和boolean_keys。当前函数通过read_opcode_from_rom读取instruction，再调用OptimizedDecoder::decode得到raw_decoder_output和opcode flags。invalid_opcode在受信代码模式下被约束为0，非法instruction会让电路不可满足。函数随后创建三条memory query：rs1只读register；rs2起初作为register读，但load可以把它改成RAM读；rd起初作为register写，但store可以把它改成RAM写。下游opcode gadget共用src1、src2和flags_source，writeback统一提交这三条query。

三条query的时间戳槽位来自ops/mod.rs：

```rust
pub const RS1_LOAD_LOCAL_TIMESTAMP: usize = 0;
pub const RS2_LOAD_LOCAL_TIMESTAMP: usize = 1;
pub const RD_STORE_LOCAL_TIMESTAMP: usize = 2;
```

一个cycle最多三次memory/register访问：rs1读、rs2或load读、rd或store写。local_timestamp_in_cycle记录该访问在cycle内部的顺序，global timestamp由chunk序号、cycle序号和local timestamp共同决定。

opcode gadget逐个执行，并各自返回CommonDiffs。ADD示例足够说明大部分算术指令的形态。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/add_sub.rs
```

```rust
impl<
        F: PrimeField,
        ST: BaseMachineState<F>,
        RS: RegisterValueSource<F>,
        DE: DecoderOutputSource<F, RS>,
        BS: IndexableBooleanSet,
    > MachineOp<F, ST, RS, DE, BS> for AddOp
{
    fn apply<
        CS: Circuit<F>,
        const ASSUME_TRUSTED_CODE: bool,
        const OUTPUT_EXACT_EXCEPTIONS: bool,
    >(
        cs: &mut CS,
        _machine_state: &ST,
        inputs: &DE,
        boolean_set: &BS,
        opt_ctx: &mut OptimizationContext<F, CS>,
    ) -> CommonDiffs<F> {
        opt_ctx.reset_indexers();
        let exec_flag = boolean_set.get_major_flag(ADD_OP_KEY);

        let src1 = inputs.get_rs1_or_equivalent().get_register();
        let src2 = inputs.get_rs2_or_equivalent().get_register();

        let (res, _of_flag) = opt_ctx.append_add_relation(src1, src2, exec_flag, cs);

        let returned_value = [
            Constraint::<F>::from(res.0[0].get_variable()),
            Constraint::<F>::from(res.0[1].get_variable()),
        ];

        CommonDiffs {
            exec_flag,
            trapped: None,
            trap_reason: None,
            rd_value: vec![(returned_value, exec_flag)],
            new_pc_value: NextPcValue::Default,
        }
    }
}
```

上游输入是decoder_output和flags_source。exec_flag来自ADD_OP_KEY，只有ADD或ADDI匹配时为1。append_add_relation写入加法关系，并用exec_flag约束该关系只在ADD族指令上生效。当前函数不写rd寄存器，也不修改pc状态；它返回rd候选值和默认pc选择。下游writeback_no_exception_with_opcodes_in_rom把所有CommonDiffs合并，选出最终rd值和最终pc。

CommonDiffs保存每个opcode对状态的候选修改。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/devices/diffs.rs
```

```rust
pub struct CommonDiffs<F: PrimeField> {
    pub exec_flag: Boolean,
    pub trapped: Option<Boolean>,
    pub trap_reason: Option<Num<F>>,
    pub rd_value: Vec<([Constraint<F>; 2], Boolean)>,
    pub new_pc_value: NextPcValue<F>,
}

impl<F: PrimeField> CommonDiffs<F> {
    #[track_caller]
    pub fn select_final_rd_value<CS: Circuit<F>>(cs: &mut CS, sources: &[Self]) -> Register<F> {
        let result = std::array::from_fn(|word_idx| {
            let mut flags = vec![];
            let mut variants = vec![];
            for el in sources.iter() {
                for (rd_candidate, flag) in el.rd_value.iter() {
                    let limb_constraint = rd_candidate[word_idx].clone();
                    assert!(limb_constraint.degree() <= 1);
                    flags.push(*flag);
                    variants.push(limb_constraint);
                }
            }

            let result = cs.choose_from_orthogonal_variants_for_linear_terms(&flags, &variants);

            result
        });

        let new_reg_val = Register(result);

        new_reg_val
    }
}
```

上游输入是所有opcode gadget返回的CommonDiffs。select_final_rd_value收集每个rd候选值及其flag，通过choose_from_orthogonal_variants_for_linear_terms选出唯一执行opcode对应的值。该函数要求候选rd limb是线性表达式，因为选择器乘线性表达式后仍是二次约束。下游writeback使用new_reg_val约束rd_or_mem_store_query的写值。

writeback阶段提交memory query和final_state。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/machine_configurations/state_transition_parts/writeback_no_exceptions.rs
```

```rust
pub(crate) fn writeback_no_exception_with_opcodes_in_rom<
    F: PrimeField,
    CS: Circuit<F>,
    const ASSUME_TRUSTED_CODE: bool,
    const PERFORM_DELEGATION: bool,
>(
    cs: &mut CS,
    opcode_format_bits: [Boolean; NUM_INSTRUCTION_TYPES_IN_DECODE_BITS],
    rd_constraint: Constraint<F>,
    rs1_query: ShuffleRamMemQuery,
    rs2_or_mem_load_query: ShuffleRamMemQuery,
    rd_or_mem_store_query: ShuffleRamMemQuery,
    application_results: Vec<CommonDiffs<F>>,
    default_next_pc: Register<F>,
    opt_ctx: &OptimizationContext<F, CS>,
) -> MinimalStateRegistersInMemory<F> {
    // 省略代码

            let new_reg_val = CommonDiffs::select_final_rd_value(cs, &application_results);

            let [r_insn, i_insn, _s_insn, b_insn, u_insn, j_insn] = opcode_format_bits;

            let update_rd = Constraint::from(r_insn.get_variable().unwrap())
                + Constraint::from(i_insn.get_variable().unwrap())
                + Constraint::from(j_insn.get_variable().unwrap())
                + Constraint::from(u_insn.get_variable().unwrap());

            // 省略代码

            cs.add_shuffle_ram_query(rs1_query);
            cs.add_shuffle_ram_query(rs2_or_mem_load_query);
            cs.add_shuffle_ram_query(rd_or_mem_store_query);

            let new_pc =
                CommonDiffs::select_final_pc_value(cs, &application_results, default_next_pc);

            let final_state = MinimalStateRegistersInMemory { pc: new_pc };

            final_state

    // 省略代码
}
```

上游输入包含decoder给出的rd、三个memory query、所有opcode候选diff和默认next_pc。当前函数先选择最终rd值，约束rd写地址和x0写入规则，再把三条ShuffleRamMemQuery加入Circuit。select_final_pc_value在branch、jump等指令提供Custom pc时选择新pc，否则使用pc+4。下游compile_machine把final_state.pc加入state_output，OneRowCompiler用state_input/state_output把相邻行的pc连接起来。

## 第7章 register/RAM统一访问和delegation入口

Airbender把register和RAM放进统一的shuffle RAM argument。register访问和RAM访问都形成ShuffleRamMemQuery，区别在query_type。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/cs/circuit.rs
```

```rust
pub enum ShuffleRamQueryType {
    RegisterOnly {
        register_index: Variable,
    },
    RegisterOrRam {
        is_register: Boolean,
        address: [Variable; REGISTER_SIZE],
    },
}

pub struct ShuffleRamMemQuery {
    pub query_type: ShuffleRamQueryType,
    pub local_timestamp_in_cycle: usize,
    pub read_value: [Variable; REGISTER_SIZE],
    pub write_value: [Variable; REGISTER_SIZE],
}
```

RegisterOnly用于rs1：它只可能读register。RegisterOrRam用于rs2/load和rd/store：is_register为1时address解释成register编号，为0时address解释成RAM地址。read_value和write_value都是两个16-bit limb，组成一个32-bit word。若read_value和write_value相同，is_readonly返回true；store和rd写入会让write_value使用独立变量。

decode阶段先把rs2和rd/store都建成RegisterOrRam，默认is_register=true。LoadOp和StoreOp在自己的spec_apply里会修改对应query，使它变成RAM读或RAM写。writeback提交最终query，query_type已经携带register/RAM分类。

CSR delegation走同一个状态转移入口。FullIsaMachineWithDelegationNoExceptionHandling把PERFORM_DELEGATION设为true，因此optimized_base_isa_state_transition在CSR位置调用apply_csr_with_delegation。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/common_impls/csr_with_delegation.rs
```

```rust
pub fn apply_csr_with_delegation<
    F: PrimeField,
    CS: Circuit<F>,
    ST: BaseMachineState<F>,
    RS: RegisterValueSource<F>,
    DE: DecoderOutputSource<F, RS>,
    BS: IndexableBooleanSet,
    const SUPPORT_CSRRC: bool,
    const SUPPORT_CSRRS: bool,
    const SUPPORT_CSR_IMMEDIATES: bool,
    const ASSUME_TRUSTED_CODE: bool,
    const OUTPUT_EXACT_EXCEPTIONS: bool,
>(
    cs: &mut CS,
    _machine_state: &ST,
    inputs: &DE,
    boolean_set: &BS,
    opt_ctx: &mut OptimizationContext<F, CS>,
) -> CommonDiffs<F> {
    // 省略代码

            let csr_index = inputs.funct12();
            let [is_supported_csr, is_for_delegation] = opt_ctx
                .append_lookup_relation_from_linear_terms::<1, 2>(
                    cs,
                    &[csr_index.clone()],
                    TableType::SpecialCSRProperties.to_num(),
                    exec_flag,
                );

            cs.add_constraint(
                (Term::from(1) - Term::from(is_supported_csr)) * exec_flag.get_terms(),
            );

            let should_delegate = cs.add_variable_from_constraint(
                Term::from(is_for_delegation) * Term::from(exec_flag),
            );

            let offset = src1.0[1];

            let offset_masked =
                cs.add_variable_from_constraint(Term::from(should_delegate) * Term::from(offset));
            let csr_index_masked =
                cs.add_variable_from_constraint(Term::from(should_delegate) * csr_index);

            let delegation_request = DelegatedComputationRequest {
                execute: should_delegate,
                degegation_type: csr_index_masked,
                memory_offset_high: offset_masked,
            };
            cs.add_delegation_request(delegation_request);

    // 省略代码
}
```

上游输入是CSR instruction的funct12、src1和exec_flag。当前函数查询SpecialCSRProperties表，确认该CSR受支持，并读出is_for_delegation。should_delegate等于is_for_delegation乘exec_flag。src1高16位作为delegation memory offset。DelegatedComputationRequest记录execute、delegation type和memory offset high。下游CircuitOutput.delegated_computation_requests保存这类请求，OneRowCompiler和prover stage2把main circuit发出的delegation request与delegation circuit处理记录做集合一致性检查。

MainRiscVOracle把VM执行trace提供给witness evaluator。CSR delegation在执行trace里表现为SingleCycleTracingData.delegation_request。

代码位置：

```text
/home/ars/zksync-airbender-seek/prover/src/tracers/main_cycle_optimized.rs
```

```rust
pub struct SingleCycleTracingData {
    pub pc: u32,
    pub rs1_read_value: u32,
    pub rs1_read_timestamp: TimestampData,
    pub rs1_reg_idx: u16,
    pub rs2_or_mem_word_read_value: u32,
    pub rs2_or_mem_word_address: RegIndexOrMemWordIndex,
    pub rs2_or_mem_read_timestamp: TimestampData,
    pub delegation_request: u16,
    pub rd_or_mem_word_read_value: u32,
    pub rd_or_mem_word_write_value: u32,
    pub rd_or_mem_word_address: RegIndexOrMemWordIndex,
    pub rd_or_mem_read_timestamp: TimestampData,
    pub non_determinism_read: u32,
}
```

上游VM执行每个cycle时填充SingleCycleTracingData。当前结构记录pc、三条memory/register访问的值和时间戳、非确定输入读取值，以及delegation_request。下游MainRiscVOracle按Placeholder读取这些字段，evaluate_witness把字段写入witness trace。

代码位置：

```text
/home/ars/zksync-airbender-seek/prover/src/tracers/oracles/main_risc_v_circuit.rs
```

```rust
fn get_u16_witness_from_placeholder(&self, placeholder: Placeholder, trace_step: usize) -> u16 {
    let cycle_data = &self.cycle_data.per_cycle_data[trace_step];

    match placeholder {
        Placeholder::DegelationABIOffset => 0,
        Placeholder::DelegationType => cycle_data.delegation_request,

        Placeholder::ShuffleRamAddress(access_idx) => match access_idx {
            0 => cycle_data.rs1_reg_idx as u16,
            1 => cycle_data.rs2_or_mem_word_address.as_u32_formal_address() as u16,
            2 => cycle_data.rd_or_mem_word_address.as_u32_formal_address() as u16,
            _ => {
                unreachable!()
            }
        },
        Placeholder::ExecuteDelegation => (cycle_data.delegation_request != 0) as u16,
        a @ _ => {
            panic!("placeholder {:?} is not supported as u16 query", a);
        }
    }
}
```

上游输入是CycleData中的某一行。当前函数把Placeholder映射到trace字段：DelegationType来自delegation_request，ShuffleRamAddress按访问编号取rs1、rs2/load、rd/store三条地址，ExecuteDelegation由delegation_request是否为0决定。下游witness evaluator根据编译时留下的Placeholder索引请求这些值，生成完整witness trace。

## 第8章 prove_image_execution消费setup和witness

create_proofs_internal把main_circuit_precomputations传给prove_image_execution。这个函数进入VM执行、witness生成和证明阶段。

代码位置：

```text
/home/ars/zksync-airbender-seek/circuit_defs/prover_examples/src/lib.rs
```

```rust
pub fn prove_image_execution<
    ND: NonDeterminismCSRSource<VectorMemoryImplWithRom>,
    A: GoodAllocator,
>(
    num_instances_upper_bound: usize,
    bytecode: &[u32],
    non_determinism: ND,
    risc_v_circuit_precomputations: &MainCircuitPrecomputations<IMStandardIsaConfig, A>,
    delegation_circuits_precomputations: &[(u32, DelegationCircuitPrecomputations<A>)],
    worker: &worker::Worker,
) -> (Vec<Proof>, Vec<(u32, Vec<Proof>)>, Vec<FinalRegisterValue>) {
    prove_image_execution_for_machine_with_gpu_tracers::<ND, IMStandardIsaConfig, A>(
        num_instances_upper_bound,
        bytecode,
        non_determinism,
        risc_v_circuit_precomputations,
        delegation_circuits_precomputations,
        worker,
    )
}
```

上游输入来自Machine::Standard CPU分支。当前函数固定MachineConfig为IMStandardIsaConfig，再转入prove_image_execution_for_machine_with_gpu_tracers。函数名包含gpu_tracers，但CPU路径也使用这个入口组织执行trace和witness；tracer结构适合GPU，也可由CPU代码消费。

proof函数先读取compiled_circuit.trace_len，计算cycles_per_circuit=trace_len-1，然后运行VM并拆分trace。

代码位置：

```text
/home/ars/zksync-airbender-seek/circuit_defs/prover_examples/src/lib.rs
```

```rust
pub fn prove_image_execution_for_machine_with_gpu_tracers<
    ND: NonDeterminismCSRSource<VectorMemoryImplWithRom>,
    C: MachineConfig,
    A: GoodAllocator,
>(
    num_instances_upper_bound: usize,
    bytecode: &[u32],
    non_determinism: ND,
    risc_v_circuit_precomputations: &MainCircuitPrecomputations<C, A>,
    delegation_circuits_precomputations: &[(u32, DelegationCircuitPrecomputations<A>)],
    worker: &worker::Worker,
) -> (Vec<Proof>, Vec<(u32, Vec<Proof>)>, Vec<FinalRegisterValue>) {
    let trace_len = risc_v_circuit_precomputations.compiled_circuit.trace_len;
    let cycles_per_circuit = trace_len - 1;

    let lde_factor = risc_v_circuit_precomputations
        .lde_precomputations
        .lde_factor;

    let (
        main_circuits_witness,
        inits_and_teardowns,
        delegation_circuits_witness,
        final_register_values,
    ) = trace_execution_for_gpu::<ND, C, A>(
        num_instances_upper_bound,
        bytecode,
        non_determinism,
        trace_len,
        worker,
    );

    // 省略代码
}
```

上游输入是setup阶段生成的compiled_circuit和LDE预计算。当前函数用trace_len决定每个main proof覆盖多少cycle，再调用trace_execution_for_gpu执行VM。返回的main_circuits_witness按chunk存放CycleData；inits_and_teardowns保存shuffle RAM lazy init和teardown；delegation_circuits_witness按delegation type分组；final_register_values用于最终register公开状态和memory argument初始贡献。下游代码先commit memory tree，再由FS transcript抽取memory/delegation challenges。

VM执行入口在trace_execution_for_gpu里调用run_and_split_for_gpu。

代码位置：

```text
/home/ars/zksync-airbender-seek/circuit_defs/prover_examples/src/lib.rs
```

```rust
pub fn trace_execution_for_gpu<
    ND: NonDeterminismCSRSource<VectorMemoryImplWithRom>,
    C: MachineConfig,
    A: GoodAllocator,
>(
    num_instances_upper_bound: usize,
    bytecode: &[u32],
    mut non_determinism: ND,
    trace_len: usize,
    worker: &worker::Worker,
) -> (
    Vec<CycleData<C>>,
    (
        usize,
        Vec<ShuffleRamSetupAndTeardown>,
    ),
    HashMap<u16, Vec<DelegationWitness>>,
    Vec<FinalRegisterValue>,
) {
    let cycles_per_circuit = trace_len - 1;
    let max_cycles_to_run = num_instances_upper_bound * cycles_per_circuit;

    let delegation_factories = setups::delegation_factories_for_machine::<C, Global>();

    let (
        final_pc,
        main_circuits_witness,
        delegation_circuits_witness,
        final_register_values,
        init_and_teardown_chunks,
    ) = run_and_split_for_gpu::<ND, C, Global>(
        max_cycles_to_run,
        trace_len,
        bytecode,
        &mut non_determinism,
        delegation_factories,
        worker,
    );

    let init_and_teardown_chunks = chunk_lazy_init_and_teardown(
        main_circuits_witness.len(),
        cycles_per_circuit,
        &init_and_teardown_chunks,
        worker,
    );

    (
        main_circuits_witness,
        init_and_teardown_chunks,
        delegation_circuits_witness,
        final_register_values,
    )
}
```

上游输入是bytecode、非确定输入、trace_len和delegation factories。当前函数计算最大执行cycle数，创建delegation witness factory，然后调用VM执行器。run_and_split_for_gpu返回main trace、delegation trace、最终寄存器和memory teardown数据。chunk_lazy_init_and_teardown把RAM初始化与收尾记录切成和main circuit witness相同的chunk。下游prove_image_execution使用这些对象生成memory tree和witness trace。

单个main chunk进入evaluate_witness。

代码位置：

```text
/home/ars/zksync-airbender-seek/circuit_defs/prover_examples/src/lib.rs
```

```rust
let oracle = MainRiscVOracle {
    cycle_data: witness_chunk,
};

let witness_trace = evaluate_witness(
    &risc_v_circuit_precomputations.compiled_circuit,
    risc_v_circuit_precomputations.witness_eval_fn_for_gpu_tracer,
    cycles_per_circuit,
    &oracle,
    &shuffle_rams.lazy_init_data,
    &risc_v_circuit_precomputations.table_driver,
    circuit_sequence,
    worker,
    A::default(),
);
```

上游输入是某个CycleData chunk、compiled_circuit、witness evaluator函数指针、shuffle RAM lazy init数据和TableDriver。当前函数根据compiled_circuit里的Placeholder、layout和witness函数，从oracle读取pc、register/RAM访问、delegation request等字段，生成exec_trace和aux_data。下游prove把witness_trace、setup、twiddles和LDE预计算交给后端。

证明调用位于同一函数中。

代码位置：

```text
/home/ars/zksync-airbender-seek/circuit_defs/prover_examples/src/lib.rs
```

```rust
let (_, proof) = prove(
    &risc_v_circuit_precomputations.compiled_circuit,
    &public_inputs,
    &external_values,
    witness_trace,
    &risc_v_circuit_precomputations.setup,
    &risc_v_circuit_precomputations.twiddles,
    &risc_v_circuit_precomputations.lde_precomputations,
    circuit_sequence,
    None,
    lde_factor,
    risc_v_cycles::TREE_CAP_SIZE,
    NUM_QUERIES,
    POW_BITS,
    worker,
);
```

上游输入已经包含执行witness、setup固定列、memory/delegation challenge和public inputs。当前函数进入后端证明过程。FRI/PCS属于后端证明边界，此处保留接口关系：compiled_circuit给出约束布局，witness_trace给出执行列，setup给出固定列，twiddles和lde_precomputations支持多项式评价和commitment。下游返回Proof，prove_image_execution把main proof的memory_grand_product_accumulator乘入总memory accumulator，把delegation_argument_accumulator加入总delegation sum。

delegation proof也在同一个prove_image_execution_for_machine_with_gpu_tracers里生成。main circuit proof把delegation_argument_accumulator加到总和，delegation circuit proof把自己的accumulator从总和里减掉。函数结尾检查：

```rust
assert_eq!(memory_grand_product, Mersenne31Quartic::ONE);
assert_eq!(delegation_argument_sum, Mersenne31Quartic::ZERO);
```

memory_grand_product等于1表示所有main和delegation中的memory/register访问与最终寄存器贡献匹配。delegation_argument_sum等于0表示main circuit发出的delegation request与delegation circuits处理的request匹配。这个检查发生在prover侧，用于确认生成的proof对象内部accumulator一致；verifier侧会用proof里的commitment和挑战重做同类校验。

## 第9章 典型指令gadget：load/store与控制流

第6章已经给出optimized_base_isa_state_transition的执行顺序：decode阶段预分配三条memory query，opcode gadget逐个产生CommonDiffs，writeback阶段统一提交rd、pc和shuffle RAM query。ADD只展示了普通算术指令怎样返回rd_value。load、store、branch、jump覆盖另外两类关键行为：load/store会修改预分配的RegisterOrRam query，branch/jump会通过new_pc_value覆盖默认pc+4。

### LoadOp怎样把rs2 query改成RAM读

decode阶段给rs2创建了第二条ShuffleRamMemQuery。默认形态是RegisterOrRam，is_register初始等价于true，address保存rs2 register index。LoadOp::spec_apply接收这条query的可变引用；LW/LH/LHU/LB/LBU执行时，它会把query改成RAM读或ROM读占位，并把读取结果作为rd候选值返回。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/load.rs
```

```rust
impl<const SUPPORT_SIGNED: bool, const SUPPORT_LESS_THAN_WORD: bool>
    LoadOp<SUPPORT_SIGNED, SUPPORT_LESS_THAN_WORD>
{
    pub fn spec_apply<
        F: PrimeField,
        CS: Circuit<F>,
        ST: BaseMachineState<F>,
        RS: RegisterValueSource<F>,
        DE: DecoderOutputSource<F, RS>,
        BS: IndexableBooleanSet,
        const ASSUME_TRUSTED_CODE: bool,
        const OUTPUT_EXACT_EXCEPTIONS: bool,
    >(
        cs: &mut CS,
        _machine_state: &ST,
        inputs: &DE,
        boolean_set: &BS,
        rs2_or_mem_load_query: &mut ShuffleRamMemQuery,
        opt_ctx: &mut OptimizationContext<F, CS>,
    ) -> CommonDiffs<F> {
        opt_ctx.reset_indexers();

        assert!(ST::opcodes_are_in_rom());

        let execute_family = boolean_set.get_major_flag(LOAD_COMMON_OP_KEY);
        let src1 = inputs.get_rs1_or_equivalent();
        let funct3 = inputs.funct3();

        // 省略代码
    }
}
```

上游optimized_base_isa_state_transition传入rs2_or_mem_load_query。当前函数先取LOAD_COMMON_OP_KEY对应的execute_family，再从decoder_output取src1和funct3。src1是base address，imm来自inputs.get_imm，二者相加得到unaligned_address。funct3区分LB、LH、LW、LBU、LHU。

load使用MemoryOffsetGetBits表拆出地址低两位。bit_0表示byte offset的最低位，bit_1表示半字位置。对受信代码，LW要求bit_0+bit_1=0，LH/LHU要求bit_0=0；不满足时约束不可满足。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/load.rs
```

```rust
let (unaligned_address, _of_flag) =
    opt_ctx.append_add_relation(src1, imm, execute_family, cs);

let [bit_0, bit_1] = opt_ctx.append_lookup_relation(
    cs,
    &[unaligned_address.0[0].get_variable()],
    TableType::MemoryOffsetGetBits.to_num(),
    execute_family,
);
let aligned_address_low_constraint = {
    Constraint::from(unaligned_address.0[0].get_variable())
        - (Term::from(bit_1) * Term::from(2))
        - Term::from(bit_0)
};

if ASSUME_TRUSTED_CODE {
    cs.add_constraint((Term::from(bit_0) + Term::from(bit_1)) * exec_word.get_terms());

    cs.add_constraint(Term::from(bit_0) * exec_half_word.get_terms());
} else {
    todo!();
}
```

上游输入是src1、imm和execute_family。当前片段得到unaligned_address，并把低16位中的低两位从lookup表读出。aligned_address_low_constraint等于unaligned_address.low去掉低两位。下游ROM读和RAM读都使用这个aligned address，subword load再用bit_0、bit_1选择byte或half-word。

Airbender把load地址分成ROM读和RAM读。RomAddressSpaceSeparator根据地址高16位判断地址属于RAM区还是ROM区。ROM读通过RomRead表取instruction word；RAM读通过rs2_or_mem_load_query.read_value取memory argument给出的值。两条路径都会产生候选rd值。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/load.rs
```

```rust
let [is_ram_range, address_high_bits_for_rom] = opt_ctx.append_lookup_relation(
    cs,
    &[unaligned_address.0[1].get_variable()],
    TableType::RomAddressSpaceSeparator.to_num(),
    execute_family,
);

let is_rom_read = cs.add_variable_from_constraint(
    Term::from(execute_family.get_variable().unwrap())
        * (Term::from(1u64) - Term::from(is_ram_range)),
);
let is_ram_read = cs.add_variable_from_constraint(
    Term::from(execute_family.get_variable().unwrap()) * Term::from(is_ram_range),
);

// 省略代码

CommonDiffs {
    exec_flag: execute_family,
    trapped: None,
    trap_reason: None,
    rd_value: vec![
        (
            [
                Constraint::from(rom_value_low),
                Constraint::from(rom_value_high),
            ],
            Boolean::Is(is_rom_read),
        ),
        (
            [
                Constraint::from(ram_value_low),
                Constraint::from(ram_value_high),
            ],
            Boolean::Is(is_ram_read),
        ),
    ],
    new_pc_value: NextPcValue::Default,
}
```

上游输入是unaligned_address.high和execute_family。当前片段得到is_rom_read和is_ram_read。rd_value里放两组候选值：ROM候选由RomRead和ExtendLoadedValue产生，RAM候选由memory query的read_value和ExtendLoadedValue产生。下游CommonDiffs::select_final_rd_value会在writeback阶段按flag选择最终rd值。

LoadOp还会重写rs2_or_mem_load_query.query_type。执行load时，第二条query表示RAM读；未执行load时，第二条query回到普通rs2 register读。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/load.rs
```

```rust
let ShuffleRamQueryType::RegisterOrRam {
    is_register,
    address,
} = &mut rs2_or_mem_load_query.query_type
else {
    unreachable!()
};
let t = cs.add_variable_from_constraint_allow_explicit_linear(
    Term::from(1u64) - Term::from(execute_family),
);
*is_register = Boolean::Is(t);

let rs2_index = inputs.get_rs2_index();
cs.add_constraint(
    (rs2_index - Term::from(address[0]))
        * (Term::from(1u64) - Term::from(execute_family)),
);
cs.add_constraint(
    Term::from(address[1]) * (Term::from(1u64) - Term::from(execute_family)),
);
```

上游输入是decode阶段预分配的query和rs2_index。当前片段把is_register设为1-execute_family。LOAD未执行时，address必须等于rs2 register index；LOAD执行时，这两条约束被乘上0，RAM地址约束由is_ram_read路径负责。下游writeback统一调用cs.add_shuffle_ram_query(rs2_or_mem_load_query)。

### StoreOp怎样把rd query改成RAM写

StoreOp::spec_apply接收第三条ShuffleRamMemQuery的可变引用。decode阶段把这条query作为rd写回预分配；store执行时，StoreOp把它改成RAM写，并约束写地址和write_value。store不产生rd_value，因为RISC-V store不会写rd。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/store.rs
```

```rust
impl<const SUPPORT_LESS_THAN_WORD: bool> StoreOp<SUPPORT_LESS_THAN_WORD> {
    pub fn spec_apply<
        F: PrimeField,
        CS: Circuit<F>,
        ST: BaseMachineState<F>,
        RS: RegisterValueSource<F>,
        DE: DecoderOutputSource<F, RS>,
        BS: IndexableBooleanSet,
        const ASSUME_TRUSTED_CODE: bool,
        const OUTPUT_EXACT_EXCEPTIONS: bool,
    >(
        cs: &mut CS,
        _machine_state: &ST,
        inputs: &DE,
        boolean_set: &BS,
        rd_or_mem_store_query: &mut ShuffleRamMemQuery,
        opt_ctx: &mut OptimizationContext<F, CS>,
    ) -> CommonDiffs<F> {
        opt_ctx.reset_indexers();

        assert!(ST::opcodes_are_in_rom());

        let execute_family = boolean_set.get_major_flag(STORE_COMMON_OP_KEY);

        let src1 = inputs.get_rs1_or_equivalent();
        let src2 = inputs.get_rs2_or_equivalent();

        // 省略代码
    }
}
```

上游optimized_base_isa_state_transition传入rd_or_mem_store_query。当前函数取STORE_COMMON_OP_KEY，src1作为base address，src2作为要写入的值。SW、SH、SB由minor flags区分。

store先计算地址并检查对齐，然后用RomAddressSpaceSeparator禁止写ROM。受信代码模式下，若store地址落在ROM范围，execute_family乘以1-is_ram_range会产生非零约束，电路不可满足。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/store.rs
```

```rust
let (unaligned_address, _of_flag) =
    opt_ctx.append_add_relation(src1, imm, execute_family, cs);

let [bit_0, bit_1] = opt_ctx.append_lookup_relation(
    cs,
    &[unaligned_address.0[0].get_variable()],
    TableType::MemoryOffsetGetBits.to_num(),
    execute_family,
);
let aligned_address_low_constraint = {
    Constraint::from(unaligned_address.0[0].get_variable())
        - (Term::from(bit_1) * Term::from(2))
        - Term::from(bit_0)
};

if ASSUME_TRUSTED_CODE {
    cs.add_constraint((Term::from(bit_0) + Term::from(bit_1)) * exec_word.get_terms());

    cs.add_constraint(Term::from(bit_0) * exec_half_word.get_terms());
} else {
    todo!();
}

let [is_ram_range, _address_high_bits_for_rom] = opt_ctx.append_lookup_relation(
    cs,
    &[unaligned_address.0[1].get_variable()],
    TableType::RomAddressSpaceSeparator.to_num(),
    execute_family,
);

if ASSUME_TRUSTED_CODE {
    cs.add_constraint(
        execute_family.get_terms() * (Term::from(1) - Term::from(is_ram_range)),
    );
} else {
    todo!()
}
```

上游输入是store地址和execute_family。当前片段完成对齐检查和RAM范围检查。下游rd_or_mem_store_query的address必须等于aligned address；write_value必须等于store写入后的完整32-bit word。

SB/SH需要保留同一word中未写入的byte或half-word。StoreOp读取rd_or_mem_store_query.read_value作为base_value，再用StoreByteSourceContribution和StoreByteExistingContribution两个表组合新旧字节。SW直接把src2完整写入write_value。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/store.rs
```

```rust
let base_value = rd_or_mem_store_query.read_value;
let src_half_word = src2
    .get_register_with_decomposition_and_sign()
    .unwrap()
    .u16_limbs[0]
    .get_variable();
let subword_to_use_for_update = cs.add_variable_from_constraint(
    Term::from(bit_1) * Term::from(base_value[1])
        + (Term::from(1u64) - Term::from(bit_1)) * Term::from(base_value[0]),
);

let [update_contribution] = opt_ctx.append_lookup_relation(
    cs,
    &[src_half_word, bit_0],
    TableType::StoreByteSourceContribution.to_num(),
    execute_family,
);
let [to_keep_contribution] = opt_ctx.append_lookup_relation(
    cs,
    &[subword_to_use_for_update, bit_0],
    TableType::StoreByteExistingContribution.to_num(),
    execute_family,
);

// 省略代码

cs.add_constraint(
    (Term::from(selected_low) - Term::from(rd_or_mem_store_query.write_value[0]))
        * (Term::from(execute_family) - Term::from(exec_word)),
);
cs.add_constraint(
    (Term::from(selected_high) - Term::from(rd_or_mem_store_query.write_value[1]))
        * (Term::from(execute_family) - Term::from(exec_word)),
);
```

上游输入是store value、旧memory word和地址低两位。当前片段对SB/SH构造写后word。selected_low和selected_high是完整32-bit write_value的两个16-bit limb。下游shuffle RAM argument会验证该query的read_value来自旧memory状态，write_value成为新memory状态。

store执行时，第三条query从rd写回变成RAM写；store未执行时，第三条query继续作为rd写回使用。StoreOp把is_register设为1-execute_family。地址在store未执行时不需要额外约束，因为writeback阶段会用rd_constraint约束rd写地址。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/store.rs
```

```rust
let ShuffleRamQueryType::RegisterOrRam { is_register, .. } =
    &mut rd_or_mem_store_query.query_type
else {
    unreachable!()
};
let t = cs.add_variable_from_constraint_allow_explicit_linear(
    Term::from(1u64) - Term::from(execute_family),
);
*is_register = Boolean::Is(t);

CommonDiffs {
    exec_flag: execute_family,
    trapped: None,
    trap_reason: None,
    rd_value: vec![],
    new_pc_value: NextPcValue::Default,
}
```

上游输入是rd_or_mem_store_query。当前片段改变query_type并返回空rd_value。下游writeback仍会把rd_or_mem_store_query提交给shuffle RAM。若store执行，query_type标记RAM写；若store未执行，writeback把它约束成rd register写。

### JumpOp怎样返回rd和新pc

JumpOp覆盖JAL和JALR。JAL把pc加imm作为跳转目标；JALR把rs1加imm作为目标，并清理最低位。JumpOp还要把pc_next作为rd候选值，因为JAL/JALR会把返回地址写入rd。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/jump.rs
```

```rust
impl<
        F: PrimeField,
        ST: BaseMachineState<F>,
        RS: RegisterValueSource<F>,
        DE: DecoderOutputSource<F, RS>,
        BS: IndexableBooleanSet,
    > MachineOp<F, ST, RS, DE, BS> for JumpOp
{
    fn apply<
        CS: Circuit<F>,
        const ASSUME_TRUSTED_CODE: bool,
        const OUTPUT_EXACT_EXCEPTIONS: bool,
    >(
        cs: &mut CS,
        machine_state: &ST,
        inputs: &DE,
        boolean_set: &BS,
        opt_ctx: &mut OptimizationContext<F, CS>,
    ) -> CommonDiffs<F> {
        opt_ctx.reset_indexers();
        let exec_flag = boolean_set.get_major_flag(JUMP_COMMON_OP_KEY);
        let pc_next = inputs.get_pc_next();

        let src1 = inputs.get_rs1_or_equivalent().get_register();
        let imm = inputs.get_imm();

        let is_jal = boolean_set.get_minor_flag(JUMP_COMMON_OP_KEY, JAL_OP_KEY);
        let pc = *machine_state.get_pc();

        let src1 = Register::choose::<CS>(cs, &is_jal, &pc, &src1);
        let (x, _of_flag) = opt_ctx.append_add_relation(src1, imm, exec_flag, cs);

        // 省略代码
    }
}
```

上游输入是当前pc、decoder输出的rs1/imm/pc_next和jump flags。当前函数用is_jal在pc和rs1之间选择加法左操作数，再计算x=pc+imm或rs1+imm。下游JumpCleanupOffset表处理JALR最低位清理和对齐检查。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/jump.rs
```

```rust
let [bit_1, dst_low] = opt_ctx.append_lookup_relation(
    cs,
    &[x.0[0].get_variable()],
    TableType::JumpCleanupOffset.to_num(),
    exec_flag,
);
let is_misaligned_addr = bit_1;

cs.add_constraint(Term::from(is_misaligned_addr) * exec_flag.get_terms());

let dst_low = Num::Var(dst_low);
let dst_high = x.0[1];
let dst = Register([dst_low, dst_high]);

let returned_value = [
    Constraint::<F>::from(pc_next.0[0].get_variable()),
    Constraint::<F>::from(pc_next.0[1].get_variable()),
];

CommonDiffs {
    exec_flag,
    trapped: None,
    trap_reason: None,
    rd_value: vec![(returned_value, exec_flag)],
    new_pc_value: NextPcValue::Custom(dst),
}
```

上游输入是x和exec_flag。当前片段用JumpCleanupOffset得到dst_low，并在受信代码模式下禁止misaligned jump。returned_value是pc_next，也就是返回地址。new_pc_value是Custom(dst)。下游writeback会把returned_value写入rd，并用CommonDiffs::select_final_pc_value把final_state.pc改成dst。

### ConditionalOp怎样同时服务SLT和branch

ConditionalOp覆盖SLT/SLTI/SLTU/SLTIU和BEQ/BNE/BLT/BGE/BLTU/BGEU。它先统一计算src1-src2，得到underflow flag和eq flag，再用funct3、比较flag和符号位查条件表。条件表返回两个值：should_jump和comparison_value。branch使用should_jump更新pc，SLT使用comparison_value写rd。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/conditional.rs
```

```rust
impl<
        F: PrimeField,
        ST: BaseMachineState<F>,
        RS: RegisterValueSource<F>,
        DE: DecoderOutputSource<F, RS>,
        BS: IndexableBooleanSet,
        const SUPPORT_SIGNED: bool,
    > MachineOp<F, ST, RS, DE, BS> for ConditionalOp<SUPPORT_SIGNED>
{
    fn apply<
        CS: Circuit<F>,
        const ASSUME_TRUSTED_CODE: bool,
        const OUTPUT_EXACT_EXCEPTIONS: bool,
    >(
        cs: &mut CS,
        machine_state: &ST,
        inputs: &DE,
        boolean_set: &BS,
        opt_ctx: &mut OptimizationContext<F, CS>,
    ) -> CommonDiffs<F> {
        opt_ctx.reset_indexers();
        let exec_flag = boolean_set.get_major_flag(CONDITIONAL_COMMON_OP_KEY);

        let src1 = inputs.get_rs1_or_equivalent();
        let src2 = inputs.get_rs2_or_equivalent();

        let (diff, uf_flag) =
            opt_ctx.append_sub_relation(src1.get_register(), src2.get_register(), exec_flag, cs);

        let eq_flag = opt_ctx.append_is_zero_relation(diff, exec_flag, cs);

        let bltu_flag = uf_flag;

        // 省略代码
    }
}
```

上游输入是decoder输出的src1、src2、funct3和imm。当前函数用减法关系得到无符号小于flag，用is_zero关系得到相等flag。对于signed比较，函数还读取src1和src2的符号位，把funct3、uf_flag、eq_flag、sign bits组合成key_constraint。

代码位置：

```text
/home/ars/zksync-airbender-seek/cs/src/machine/ops/conditional.rs
```

```rust
let table_id = if SUPPORT_SIGNED {
    TableType::ConditionalOpAllConditionsResolver
} else {
    TableType::ConditionalOpUnsignedConditionsResolver
};

let [should_jump, comparison_value] = opt_ctx
    .append_lookup_relation_from_linear_terms::<1, 2>(
        cs,
        &[key_constraint],
        table_id.to_num(),
        exec_flag,
    );

let exec_jump = should_jump;
let trapped = cs.add_variable_from_constraint(
    Term::from(should_jump) * Term::from(is_misaligned_addr),
);

cs.add_constraint(Term::from(trapped) * exec_flag.get_terms());

let new_pc_low = cs.add_variable_from_constraint(
    Term::from(exec_jump) * Term::from(true_jmp_address.0[0].get_variable())
        + (Term::from(1) - Term::from(exec_jump))
            * Term::from(pc_next.0[0].get_variable()),
);
let new_pc_high = cs.add_variable_from_constraint(
    Term::from(exec_jump) * Term::from(true_jmp_address.0[1].get_variable())
        + (Term::from(1) - Term::from(exec_jump))
            * Term::from(pc_next.0[1].get_variable()),
);

let pc = Register([Num::Var(new_pc_low), Num::Var(new_pc_high)]);

let returned_value = [
    Constraint::<F>::from(comparison_value),
    Constraint::<F>::empty(),
];

CommonDiffs {
    exec_flag: exec_flag,
    trapped: None,
    trap_reason: None,
    rd_value: vec![(returned_value, exec_flag)],
    new_pc_value: NextPcValue::Custom(pc),
}
```

上游输入是key_constraint、branch目标true_jmp_address和pc_next。当前片段通过条件表得到should_jump和comparison_value。new_pc在branch taken时等于true_jmp_address，在branch not taken或SLT类指令时等于pc_next。returned_value低16位等于comparison_value，高16位为空。下游writeback会对B-type指令把rd地址约束到x0，对I/R-type SLT类指令写rd。

LoadOp、StoreOp、JumpOp、ConditionalOp补齐了第6章中的三个缺口：memory gadget怎样改写预分配query，控制流gadget怎样覆盖pc，复合opcode族怎样用major flag和minor flag共享低次数约束。后续阅读mul/div时同样沿用这个框架：decoder给出flags和操作数，gadget返回CommonDiffs或修改query，writeback把候选状态提交给CircuitOutput。

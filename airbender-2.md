我重新看了真实源码。结论先说清楚：你的建议是对的，tools/cli/src/setup.rs目前不应该作为主阅读入口。它确实更像一个setup缓存封装：里面定义SetupCache，按bytecode hash缓存main/reduced/delegation setup，并在缓存miss时调用setups::get_main_riscv_circuit_setup、get_reduced_riscv_circuit_setup和all_delegation_circuits_precomputations。但我在当前仓库里搜索SetupCache get_or_create_main_circuit，只找到tools/cli/src/setup.rs自身，没有看到它被Commands::Prove主路径调用。Commands::Prove真正走的是main.rs -> create_proofs -> create_proofs_internal -> setups::get_main_riscv_circuit_setup这条线。 

所以第二章要改成：**从真实proving入口读到main RISC-V setup**。setup.rs只放在旁支说明里，不作为主线。

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

这个顺序有两个好处。第一，它从用户实际执行airbender prove时的路径开始，不会误读一个没接入主路径的缓存封装。第二，它不会一下跳进cs/src/machine，而是先看到setup到底把哪些东西交给prover，这样后面读约束定义时更清楚每个对象最终服务于哪里。

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

这个文件首先从cli_lib::prover_utils引入几个和证明相关的函数，其中包括：

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

Commands枚举里有很多子命令。和当前学习主线最相关的是Prove：

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

这些字段都会影响证明路径。bin是要证明的RISC-V binary；input是非确定输入，可以来自文件或RPC；machine选择机器类型，默认是standard；cycles控制最多跑多少RISC-V cycles；gpu决定走CPU还是GPU proving路径；until、mode、tmp_dir主要和递归证明有关。源码中Prove命令的字段定义在Commands枚举里。

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

也就是bytecode必须已经被pad到固定ROM容量。risc_v_cycles里MAX_ROM_SIZE = 1 << 21字节，因此bytecode的u32长度应为：

[
\frac{2^{21}}{4}=2^{19}
]

源码里MAX_ROM_SIZE定义为1 << 21，get_machine_for_rom_bound也明确assert bytecode长度等于对应ROM容量除以4。 

bytecode符号如下：

```text
B = padded bytecode，类型 Vec<u32>
|B| = MAX_ROM_SIZE / 4 = 2^19
```

这个B后面会进入ROM table。

num_instances用cycles和NUM_CYCLES计算：

```text
num_instances = cycles / risc_v_cycles::NUM_CYCLES + 1
```

源码里DEFAULT_CYCLES=32_000_000。create_proofs使用cycles.unwrap_or(DEFAULT_CYCLES)计算num_instances。

而risc_v_cycles::NUM_CYCLES定义为：

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

个RISC-V cycles。源码里这些常量在circuit_defs/risc_v_cycles/src/lib.rs开头。

NUM_CYCLES=DOMAIN_SIZE-1与trace布局有关：trace长度是2²²，每个main proof chunk使用trace_len-1个真实cycle，剩余边界行服务状态衔接和约束边界。第8章的prove_image_execution_for_machine_with_gpu_tracers也用trace_len-1计算cycles_per_circuit。

input_data.unwrap_or_default把None变成空Vec。CLI没有传输入时，guest仍然可以执行不读取非确定输入的程序；如果程序读取了输入，VM执行阶段会从空QuasiUARTSource读取并触发对应执行错误或约束不满足。

gpu开关决定是否创建GpuSharedState。不用GPU时gpu_state为None，Machine::Standard分支进入CPU路径并直接调用get_main_riscv_circuit_setup。启用GPU时GpuSharedState::new创建GPU execution prover，Machine::Standard分支会走commit_memory_and_prove，绕开CPU分支中的setup函数调用。

最后，create_proofs调用：

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

源码里函数签名在prover_utils.rs中。

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

Machine::Reduced和Machine::ReducedLog23分支结构和Standard很像，但它们调用的是不同setup函数：

```text
get_reduced_riscv_circuit_setup
get_reduced_riscv_log_23_circuit_setup
```

源码中Reduced分支调用get_reduced_riscv_circuit_setup，ReducedLog23分支调用get_reduced_riscv_log_23_circuit_setup。 

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

它的get_or_create_main_circuit会用bytecode hash作为key，如果缓存中没有，就新建worker，调用：

```text
setups::get_main_riscv_circuit_setup(bytecode, worker)
```

随后还调用：

```text
create_circuit_setup(&setup.setup.ldes[0].trace)
```

把setup里的trace拿去生成某种evaluation cache。源码对应在setup.rs里。 

get_or_create_reduced_circuit和get_or_create_delegations也做类似事情，分别缓存reduced setup和delegation setup。 

但目前我没有在主Commands::Prove路径里看到它。代码搜索SetupCache get_or_create_main_circuit也只返回tools/cli/src/setup.rs本身。

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

这个文件非常短，只有一个函数。真实源码里get_main_riscv_circuit_setup的主体只有几十行。

函数签名是：

```text
get_main_riscv_circuit_setup<A: GoodAllocator, B: GoodAllocator>(
    bytecode: &[u32],
    worker: &Worker,
) -> MainCircuitPrecomputations<IMStandardIsaConfig, A, B>
```

参数和返回值先在入口处绑定清楚。

A: GoodAllocator和B: GoodAllocator是内存分配器类型参数。Airbender大量使用大数组、FFT/LDE buffer、trace buffer和GPU/CPU不同内存布局，所以很多预计算对象都参数化在allocator上。CPU路径里调用的是：

```text
::<Global, Global>
```

也就是普通全局allocator。create_proofs_internal里标准CPU路径正是这样调用的。

bytecode: &[u32]是已经padding好的RISC-V program ROM。原始ELF bytes经过load_binary_from_path和get_padded_binary后，变成按4字节小端排列的u32数组。

worker: &Worker用于并行预计算。后面Twiddles::new、LdePrecomputations::new和SetupPrecomputations::from_tables_and_trace_len都会用它。

返回值是：

```text
MainCircuitPrecomputations<IMStandardIsaConfig, A, B>
```

这个结构在setups/src/lib.rs里定义，包含六个字段：

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

risc_v_cycles::get_machine本身只是转发到get_machine_for_rom_bound。

table_driver来自risc_v_cycles::get_table_driver：

```text
table_driver = risc_v_cycles::get_table_driver(bytecode, delegation_csrs)
```

table_driver只构造lookup tables，不编译全部machine。源码里这一行紧接着get_machine。

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

DOMAIN_SIZE来自risc_v_cycles，值是(2^{22})。 

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

源码里LDE_FACTOR = 2，LDE_SOURCE_COSETS = &[0,1]。 

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

因此，get_main_riscv_circuit_setup的完整作用可以压缩成一句：

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

get_machine调用：

```text
get_machine_for_rom_bound::<ROM_ADDRESS_SPACE_SECOND_WORD_BITS>(bytecode, delegation_csrs)
```

源码里ROM_ADDRESS_SPACE_SECOND_WORD_BITS来自MAX_ROM_SIZE.trailing_zeros() - 16。 

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

进入get_machine_for_rom_bound后，第一件事情是检查bytecode长度：

```text
bytecode.len() == (1 << (16 + ROM_ADDRESS_SPACE_SECOND_WORD_BITS)) / 4
```

源码中有这个assert。

如果ROM_ADDRESS_SPACE_SECOND_WORD_BITS = 5，那么：

[
1 << (16+5) = 2^{21}
]

这是字节数；除以4以后是：

[
2^{19}
]

也就是Vec<u32>长度。这个检查保证bytecode已经pad到完整ROM容量。

第二件事是创建machine：

```text
machine = FullIsaMachineWithDelegationNoExceptionHandling
```

源码里把Machine type alias设成FullIsaMachineWithDelegationNoExceptionHandling，formal_machine_for_compilation()也返回这个类型的值。

名字很长，拆开看：

```text
FullIsa:
  支持完整IM类RISC-V指令集合。

WithDelegation:
  支持通过CSR调用delegation circuits。

NoExceptionHandling:
  假设trusted code，不处理trap/exception路径。
```

官方Circuit Entry Points文档也说明FullIsaMachineWithDelegationNoExceptionHandling是full ISA加delegation CSR calls，不包含exception handling logic。

第三件事是创建ROM表：

```text
create_table_for_rom_image(bytecode, TableType::RomRead.to_table_id())
```

源码中这一段在get_machine_for_rom_bound里。

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

这个表用于约束哪些CSR值是合法delegation调用。官方文档说delegation circuits通过专用CSR值被RISC-V程序调用，每个precompile有唯一DELEGATION_TYPE_ID，必须和程序写入的CSR值匹配。

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

也就是后面get_main_riscv_circuit_setup里的compiled_circuit。

所以risc_v_cycles::get_machine这条线可以画成：

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

get_table_driver和get_machine接收相同输入：

```text
bytecode
delegation_csrs
```

然后转发到get_table_driver_for_rom_bound。

它同样先assert bytecode长度。随后：

```text
create_table_driver(machine)
create_table_for_rom_image(...)
table_driver.add_table_with_content(TableType::RomRead, ...)
create_csr_table_for_delegation(...)
table_driver.add_table_with_content(TableType::SpecialCSRProperties, ...)
```

源码里这些步骤在get_table_driver_for_rom_bound中。

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

risc_v_cycles/src/lib.rs里还有：

```text
witness_eval_fn_for_gpu_tracer(proxy)
```

它会调用sealed::evaluate_witness_fn，而这个函数来自：

```text
include!("../generated/witness_generation_fn.rs")
```

源码中可以看到sealed模块include了生成的witness函数，witness_eval_fn_for_gpu_tracer拿到函数指针后调用它。

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

risc_v_cycles/src/lib.rs还有generate_artifacts()。它使用dummy bytecode生成compiled machine，然后写出：

```text
generated/layout
generated/circuit_layout.rs
generated/quotient.rs
```

源码中它先用全零dummy bytecode填满ROM大小，再调用get_machine，然后调用verifier_generator::generate_for_description生成layout和quotient代码。

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

第三章的目标会是：把CompiledCircuitArtifact、TableDriver、MainCircuitPrecomputations和SetupPrecomputations四个对象讲透。
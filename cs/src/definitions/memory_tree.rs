use super::*;

#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ShuffleRamInitAndTeardownLayout {
    pub lazy_init_addresses_columns: ColumnSet<REGISTER_SIZE>,
    pub lazy_teardown_values_columns: ColumnSet<REGISTER_SIZE>,
    pub lazy_teardown_timestamps_columns: ColumnSet<NUM_TIMESTAMP_COLUMNS_FOR_RAM>,
}

impl ShuffleRamInitAndTeardownLayout {
    pub const fn empty() -> Self {
        Self {
            lazy_init_addresses_columns: ColumnSet::empty(),
            lazy_teardown_values_columns: ColumnSet::empty(),
            lazy_teardown_timestamps_columns: ColumnSet::empty(),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MemorySubtree {
    /// main RISC-V 有 lazy init / teardown，所以是 Some(...)
    pub shuffle_ram_inits_and_teardowns: Option<ShuffleRamInitAndTeardownLayout>,
    /// main RISC-V 每行 3 个 shuffle RAM query 的列布局。
    pub shuffle_ram_access_sets: Vec<ShuffleRamQueryColumns>,
    /// main RISC-V 如果会发 delegation request，这里记录 request 相关列。
    pub delegation_request_layout: Option<DelegationRequestLayout>,
    /// main RISC-V 自己不是 delegation processor，所以这里是 None。
    pub delegation_processor_layout: Option<DelegationProcessingLayout>,
    pub batched_ram_accesses: Vec<BatchedRamAccessColumns>,
    pub register_and_indirect_accesses: Vec<RegisterAndIndirectAccessDescription>,
    /// memory subtree 总列数。
    /// 等于 memory_tree_offset 最终值。
    pub total_width: usize,
}

impl MemorySubtree {
    pub fn as_compiled<'a>(
        &'a self,
        buffer: &'a mut Vec<CompiledRegisterAndIndirectAccessDescription<'a>>,
    ) -> CompiledMemorySubtree<'a> {
        assert!(buffer.is_empty());
        for el in self.register_and_indirect_accesses.iter() {
            buffer.push(el.as_compiled());
        }

        CompiledMemorySubtree {
            shuffle_ram_inits_and_teardowns: self.shuffle_ram_inits_and_teardowns,
            shuffle_ram_access_sets: &self.shuffle_ram_access_sets,
            delegation_request_layout: self.delegation_request_layout,
            delegation_processor_layout: self.delegation_processor_layout,
            batched_ram_accesses: &self.batched_ram_accesses,
            register_and_indirect_accesses: &*buffer,
            total_width: self.total_width,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CompiledMemorySubtree<'a> {
    pub shuffle_ram_inits_and_teardowns: Option<ShuffleRamInitAndTeardownLayout>,
    pub shuffle_ram_access_sets: &'a [ShuffleRamQueryColumns],
    pub delegation_request_layout: Option<DelegationRequestLayout>,
    pub delegation_processor_layout: Option<DelegationProcessingLayout>,
    pub batched_ram_accesses: &'a [BatchedRamAccessColumns],
    pub register_and_indirect_accesses: &'a [CompiledRegisterAndIndirectAccessDescription<'a>],
    pub total_width: usize,
}

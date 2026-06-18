use super::*;

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct SetupLayout {
    /// 用于shuffle RAM timestamp相关固定列。main RISC-V需要，因为它有RAM/register memory argument。
    pub timestamp_setup_columns: ColumnSet<NUM_TIMESTAMP_COLUMNS_FOR_RAM>,
    /// 是16-bit range check固定列。很多RISC-V值会拆成16-bit limb，所以需要固定范围表。
    pub range_check_16_setup_column: ColumnSet<1>,
    /// 是timestamp相关范围检查固定列。
    pub timestamp_range_check_setup_column: ColumnSet<1>,
    /// 用于所有普通lookup表的统一编码。ROM表、decoder表、CSR表、其他通用表最终都会通过TableDriver.dump_tables()拼成统一格式，写入这里。
    pub generic_lookup_setup_columns: ColumnSet<NUM_COLUMNS_FOR_COMMON_TABLE_WIDTH_SETUP>,
    /// 是setup trace总列宽。
    pub total_width: usize,
}

impl SetupLayout {
    pub fn layout_for_lookup_size(
        lookups_total_table_len: usize,
        trace_len: usize,
        need_shuffle_ram_timestamps: bool,
    ) -> Self {
        assert!(trace_len.is_power_of_two());
        // 所有generic lookup表总共有lookups_total_table_len行。每组generic lookup setup columns最多放trace_len - 1行。
        // 如果放不下，就多开一组columns。
        let encoding_capacity = trace_len - 1;
        let mut num_required_setup_tuples = lookups_total_table_len / encoding_capacity;
        if lookups_total_table_len % encoding_capacity != 0 {
            num_required_setup_tuples += 1;
        }
        let mut offset = 0;
        let timestamp_setup_columns = if need_shuffle_ram_timestamps {
            ColumnSet::layout_at(&mut offset, 1)
        } else {
            ColumnSet::empty()
        };

        let range_check_16_setup_column = ColumnSet::layout_at(&mut offset, 1);
        let timestamp_range_check_setup_column = ColumnSet::layout_at(&mut offset, 1);
        let generic_lookup_setup_columns =
            ColumnSet::layout_at(&mut offset, num_required_setup_tuples);
        let total_width = offset;

        Self {
            timestamp_setup_columns,
            range_check_16_setup_column,
            timestamp_range_check_setup_column,
            generic_lookup_setup_columns,
            total_width,
        }
    }
}

use super::*;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
/// 描述 witness subtree 的完整列布局。
///
/// 这一层不保存某一行的具体 witness 值，而是记录：
/// - 哪些列用于 multiplicity 统计；
/// - 哪些列承载 8-bit / 16-bit range check 相关变量；
/// - 哪些 lookup 表达式需要在 witness 阶段被评价；
/// - 布尔变量列和 scratch 空间列分别落在哪些位置；
/// - witness subtree 总共有多少列。
///
/// 编译后的 `CompiledCircuitArtifact.witness_layout` 会持有这个结构。
pub struct WitnessSubtree<F: PrimeField> {
    /// 16-bit range check 表的 multiplicity 列位置。
    ///
    /// witness 阶段会统计每个 16-bit 值在 range table 中被查询了多少次，
    /// 再把对应 multiplicity 写进这些列。
    pub multiplicities_columns_for_range_check_16: ColumnSet<1>,
    /// timestamp range check 表的 multiplicity 列位置。
    ///
    /// 这些列服务于 timestamp 比较相关的 range lookup 统计。
    pub multiplicities_columns_for_timestamp_range_check: ColumnSet<1>,
    /// 通用 fixed lookup 表的 multiplicity 列位置。
    ///
    /// 这里覆盖的表包括 RomRead、OpTypeBitmask、RomAddressSpaceSeparator、
    /// SpecialCSRProperties 以及其他普通 fixed lookup tables。
    pub multiplicities_columns_for_generic_lookup: ColumnSet<1>,
    /// 8-bit range check 变量所在列。
    pub range_check_8_columns: ColumnSet<1>,
    /// 16-bit range check 变量所在列。
    pub range_check_16_columns: ColumnSet<1>,
    // #[serde(bound(
    //     deserialize = "LookupSetDescription<F, COMMON_TABLE_WIDTH>: serde::Deserialize<'de>"
    // ))]
    // #[serde(bound(serialize = "LookupSetDescription<F, COMMON_TABLE_WIDTH>: serde::Serialize"))]
    /// 所有 width-3 lookup 的编译前布局描述。
    ///
    /// 每个元素通常记录：
    /// - 这组 lookup 的输入来自哪些列；
    /// - 它指向哪一张 fixed table。
    pub width_3_lookups: Vec<LookupSetDescription<F, COMMON_TABLE_WIDTH>>,
    /// 16-bit range check 对应的 lookup expressions。
    ///
    /// 这些表达式描述 witness subtree 里的哪些列要去查询 16-bit range table。
    pub range_check_16_lookup_expressions: Vec<LookupExpression<F>>,
    /// timestamp 比较相关的 range check expressions。
    ///
    /// 这些表达式可能同时引用 witness subtree、memory subtree 和 setup subtree 中的列。
    pub timestamp_range_check_lookup_expressions: Vec<LookupExpression<F>>,
    /// main RISC-V 特殊 shuffle RAM timestamp 表达式在
    /// `timestamp_range_check_lookup_expressions` 里的起始位置。
    ///
    /// witness 阶段会用这个偏移量定位“需要额外叠加 circuit sequence 贡献”的那一段表达式。
    pub offset_for_special_shuffle_ram_timestamps_range_check_expressions: usize,
    /// 布尔变量列的范围。
    ///
    /// 这些列中的变量都需要满足布尔约束 `b^2 - b = 0`。
    pub boolean_vars_columns_range: ColumnSet<1>,
    /// scratch space 对应的普通 witness 列范围。
    ///
    /// 这些列不承担固定语义字段，主要作为编译器生成中间值时的工作空间。
    pub scratch_space_columns_range: ColumnSet<1>,
    /// witness subtree 的总列数。
    ///
    /// witness evaluator 会用它把一行执行 trace 分成 witness 部分和后续 memory 部分。
    pub total_width: usize,
}

impl<F: PrimeField> WitnessSubtree<F> {
    pub fn as_compiled<'a>(
        &'a self,
        buffer: &'a mut Vec<VerifierCompiledLookupSetDescription<'a, F, COMMON_TABLE_WIDTH>>,
        single_lookup_expressions_buffer: &'a mut Vec<VerifierCompiledLookupExpression<'a, F>>,
    ) -> CompiledWitnessSubtree<'a, F> {
        assert!(buffer.is_empty());
        for el in self.width_3_lookups.iter() {
            buffer.push(el.as_compiled());
        }

        for el in self.range_check_16_lookup_expressions.iter() {
            single_lookup_expressions_buffer.push(el.as_compiled());
        }
        let offset = single_lookup_expressions_buffer.len();
        for el in self.timestamp_range_check_lookup_expressions.iter() {
            single_lookup_expressions_buffer.push(el.as_compiled());
        }

        let range_check_16_lookup_expressions = &single_lookup_expressions_buffer[..offset];
        let timestamp_range_check_lookup_expressions = &single_lookup_expressions_buffer[offset..];

        CompiledWitnessSubtree {
            multiplicities_columns_for_range_check_16: self
                .multiplicities_columns_for_range_check_16,
            multiplicities_columns_for_timestamp_range_check: self
                .multiplicities_columns_for_timestamp_range_check,
            multiplicities_columns_for_generic_lookup: self
                .multiplicities_columns_for_generic_lookup,
            range_check_16_columns: self.range_check_16_columns,
            width_3_lookups: &buffer[..],
            range_check_16_lookup_expressions,
            timestamp_range_check_lookup_expressions,
            offset_for_special_shuffle_ram_timestamps_range_check_expressions: self
                .offset_for_special_shuffle_ram_timestamps_range_check_expressions,
            boolean_vars_columns_range: self.boolean_vars_columns_range,
            scratch_space_columns_range: self.scratch_space_columns_range,
            total_width: self.total_width,
        }
    }
}

#[derive(Clone, Copy, Debug)]
/// `WitnessSubtree` 的 verifier/compiled 视图。
///
/// 这层把可拥有的 `Vec<_>` 转成切片引用，方便编译后的电路布局在 prover / verifier
/// 阶段直接读取，不再重复分配。
pub struct CompiledWitnessSubtree<'a, F: PrimeField> {
    /// 16-bit range check 表的 multiplicity 列位置。
    pub multiplicities_columns_for_range_check_16: ColumnSet<1>,
    /// timestamp range check 表的 multiplicity 列位置。
    pub multiplicities_columns_for_timestamp_range_check: ColumnSet<1>,
    /// 通用 fixed lookup 表的 multiplicity 列位置。
    pub multiplicities_columns_for_generic_lookup: ColumnSet<1>,
    /// 16-bit range check 变量所在列。
    pub range_check_16_columns: ColumnSet<1>,
    /// 所有 width-3 lookup 的编译后描述。
    pub width_3_lookups: &'a [VerifierCompiledLookupSetDescription<'a, F, COMMON_TABLE_WIDTH>],
    /// 16-bit range check lookup expressions 的编译后切片。
    pub range_check_16_lookup_expressions: &'a [VerifierCompiledLookupExpression<'a, F>],
    /// timestamp range check lookup expressions 的编译后切片。
    pub timestamp_range_check_lookup_expressions: &'a [VerifierCompiledLookupExpression<'a, F>],
    /// main RISC-V 特殊 shuffle RAM timestamp 表达式的起始偏移。
    pub offset_for_special_shuffle_ram_timestamps_range_check_expressions: usize,
    /// 布尔变量列的范围。
    pub boolean_vars_columns_range: ColumnSet<1>,
    /// scratch space 列的范围。
    pub scratch_space_columns_range: ColumnSet<1>,
    /// witness subtree 的总列数。
    pub total_width: usize,
}

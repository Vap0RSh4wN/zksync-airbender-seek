use super::*;

/// 1. 创建一个新的 Variable(num_variables)。
/// 2. 把这个新变量加入 all_variables_to_place。
/// 3. num_variables += 1。返回这个新变量。
pub fn add_compiler_defined_variable(
    num_variables: &mut u64,
    all_variables_to_place: &mut BTreeSet<Variable>,
) -> Variable {
    let var = Variable(*num_variables);
    *num_variables += 1;
    all_variables_to_place.insert(var);

    var
}

pub fn add_multiple_compiler_defined_variables<const N: usize>(
    num_variables: &mut u64,
    all_variables_to_place: &mut BTreeSet<Variable>,
) -> [Variable; N] {
    let output = std::array::from_fn(|_| {
        let var = Variable(*num_variables);
        *num_variables += 1;
        all_variables_to_place.insert(var);

        var
    });

    output
}

pub fn memory_tree_columns_into_addresses<const N: usize>(
    columns_set: ColumnSet<N>,
    index: usize,
) -> [ColumnAddress; N] {
    let mut it = columns_set.iter();
    it.advance_by(index).expect("in range");
    let mut range = it.next().expect("column range").into_iter();
    let addresses = std::array::from_fn(|_| ColumnAddress::MemorySubtree(range.next().unwrap()));

    addresses
}

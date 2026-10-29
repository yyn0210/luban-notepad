//! Tab-group layout helpers.

/// Maximum number of editor groups in one window.
pub const MAX_GROUPS: usize = 2;

/// Which side of the first group the second group occupies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SplitDirection {
    #[default]
    None,
    Right,
    Down,
}

impl SplitDirection {
    pub fn is_split(self) -> bool {
        !matches!(self, SplitDirection::None)
    }
}

/// Indices of the tabs belonging to `group`, in tab order.
pub fn group_tabs(groups: &[usize], group: usize) -> Vec<usize> {
    groups
        .iter()
        .enumerate()
        .filter_map(|(index, membership)| (*membership == group).then_some(index))
        .collect()
}

/// Merge the second group's tabs after the first group's, preserving each
/// group's order. `groups` is the flat membership list.
pub fn merge_groups(groups: &[usize]) -> Vec<usize> {
    let first = group_tabs(groups, 0);
    let second = group_tabs(groups, 1);
    first.into_iter().chain(second).collect()
}

/// The group that receives focus after closing the last tab of `active` when a
/// split is open.
pub fn fallback_group(active: usize, split: SplitDirection) -> Option<usize> {
    if !split.is_split() {
        return None;
    }
    Some(if active == 0 { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_queries_preserve_tab_order() {
        let groups = [0, 0, 1, 0, 1];
        assert_eq!(group_tabs(&groups, 0), vec![0, 1, 3]);
        assert_eq!(group_tabs(&groups, 1), vec![2, 4]);
        assert_eq!(merge_groups(&groups), vec![0, 1, 3, 2, 4]);
    }

    #[test]
    fn fallback_only_exists_when_split() {
        assert_eq!(fallback_group(0, SplitDirection::None), None);
        assert_eq!(fallback_group(0, SplitDirection::Right), Some(1));
        assert_eq!(fallback_group(1, SplitDirection::Down), Some(0));
    }
}

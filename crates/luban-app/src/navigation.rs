//! Per-window navigation history and breadcrumb model.

use std::path::Path;

/// One location remembered for back/forward navigation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NavigationEntry {
    pub tab_id: u64,
    pub cursor: usize,
    pub anchor: usize,
    pub scroll: f32,
}

/// Back/forward history for one window.
#[derive(Clone, Debug, Default)]
pub struct NavigationHistory {
    entries: Vec<NavigationEntry>,
    current: usize,
}

/// Maximum number of remembered locations.
pub const MAX_HISTORY: usize = 100;

impl NavigationHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a new location after a jump. Recording discards forward entries.
    /// Consecutive duplicates are ignored.
    pub fn record(&mut self, entry: NavigationEntry) {
        if let Some(current) = self.entries.get(self.current) {
            if current.tab_id == entry.tab_id
                && current.cursor == entry.cursor
                && current.anchor == entry.anchor
            {
                return;
            }
        }
        self.entries.truncate(self.current + 1);
        self.entries.push(entry);
        if self.entries.len() > MAX_HISTORY {
            let excess = self.entries.len() - MAX_HISTORY;
            self.entries.drain(..excess);
        }
        self.current = self.entries.len().saturating_sub(1);
    }

    pub fn can_back(&self) -> bool {
        self.current > 0
    }

    pub fn can_forward(&self) -> bool {
        self.current + 1 < self.entries.len()
    }

    /// Step back. The returned entry becomes the current location.
    #[cfg(test)]
    pub fn back(&mut self) -> Option<NavigationEntry> {
        if !self.can_back() {
            return None;
        }
        self.current -= 1;
        self.entries.get(self.current).copied()
    }

    #[cfg(test)]
    pub fn forward(&mut self) -> Option<NavigationEntry> {
        if !self.can_forward() {
            return None;
        }
        self.current += 1;
        self.entries.get(self.current).copied()
    }

    /// Step back until `keep` accepts an entry. Entries whose tab was closed
    /// are skipped permanently.
    pub fn back_while(
        &mut self,
        mut keep: impl FnMut(NavigationEntry) -> bool,
    ) -> Option<NavigationEntry> {
        while self.can_back() {
            self.current -= 1;
            if let Some(entry) = self.entries.get(self.current).copied() {
                if keep(entry) {
                    return Some(entry);
                }
            }
        }
        None
    }

    pub fn forward_while(
        &mut self,
        mut keep: impl FnMut(NavigationEntry) -> bool,
    ) -> Option<NavigationEntry> {
        while self.can_forward() {
            self.current += 1;
            if let Some(entry) = self.entries.get(self.current).copied() {
                if keep(entry) {
                    return Some(entry);
                }
            }
        }
        None
    }

    /// Drop every entry belonging to a closed tab.
    pub fn remove_tab(&mut self, tab_id: u64) {
        let current_entry = self.entries.get(self.current).copied();
        let mut removed_before = 0usize;
        let mut index = 0usize;
        self.entries.retain(|entry| {
            let keep = entry.tab_id != tab_id;
            if !keep && index < self.current {
                removed_before += 1;
            }
            index += 1;
            keep
        });
        self.current = self.current.saturating_sub(removed_before);
        if !self.entries.is_empty() {
            self.current = self.current.min(self.entries.len() - 1);
        } else {
            self.current = 0;
        }
        if let Some(entry) = current_entry {
            if entry.tab_id != tab_id {
                if let Some(position) = self
                    .entries
                    .iter()
                    .rposition(|candidate| *candidate == entry)
                {
                    self.current = position;
                }
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.current = 0;
    }

    #[cfg(test)]
    pub fn entries(&self) -> &[NavigationEntry] {
        &self.entries
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BreadcrumbAction {
    /// Workspace or root label; clicking is inert.
    Root,
    /// Open quick open with this relative directory prefix.
    Directory(String),
    /// Open quick open with this file name.
    File(String),
    /// Select and jump to the symbol starting at this 1-based line.
    Symbol(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreadcrumbSegment {
    pub label: String,
    pub action: BreadcrumbAction,
}

/// Build breadcrumb segments: root, relative directories, file name, symbol.
///
/// `relative` uses `/` separators. A `None` relative path (untitled, partial,
/// disk-engine, or outside the workspace) yields only the root and file
/// segments; callers also pass `symbol = None` in that case.
pub fn breadcrumbs(
    root: Option<&str>,
    relative: Option<&Path>,
    file_name: &str,
    symbol: Option<&str>,
) -> Vec<BreadcrumbSegment> {
    let mut out = Vec::new();
    if let Some(root) = root.filter(|root| !root.is_empty()) {
        out.push(BreadcrumbSegment {
            label: root.to_string(),
            action: BreadcrumbAction::Root,
        });
    }
    if let Some(relative) = relative {
        let text = relative.to_string_lossy().replace('\\', "/");
        let mut prefix = String::new();
        let parts: Vec<&str> = text.split('/').collect();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            if part.is_empty() {
                continue;
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            out.push(BreadcrumbSegment {
                label: (*part).to_string(),
                action: BreadcrumbAction::Directory(prefix.clone()),
            });
        }
    }
    out.push(BreadcrumbSegment {
        label: file_name.to_string(),
        action: BreadcrumbAction::File(file_name.to_string()),
    });
    if let Some(symbol) = symbol.filter(|symbol| !symbol.is_empty()) {
        out.push(BreadcrumbSegment {
            label: symbol.to_string(),
            action: BreadcrumbAction::Symbol(0),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tab: u64, cursor: usize) -> NavigationEntry {
        NavigationEntry {
            tab_id: tab,
            cursor,
            anchor: cursor,
            scroll: 0.0,
        }
    }

    #[test]
    fn records_and_walks_back_and_forward() {
        let mut history = NavigationHistory::new();
        assert!(!history.can_back());
        history.record(entry(1, 0));
        history.record(entry(1, 10));
        history.record(entry(2, 5));
        assert!(history.can_back());
        assert_eq!(history.back(), Some(entry(1, 10)));
        assert_eq!(history.back(), Some(entry(1, 0)));
        assert_eq!(history.back(), None);
        assert_eq!(history.forward(), Some(entry(1, 10)));
        assert_eq!(history.forward(), Some(entry(2, 5)));
        assert_eq!(history.forward(), None);
    }

    #[test]
    fn recording_after_back_discards_forward_entries() {
        let mut history = NavigationHistory::new();
        history.record(entry(1, 0));
        history.record(entry(1, 10));
        history.record(entry(2, 5));
        history.back();
        history.back();
        history.record(entry(3, 1));
        assert_eq!(history.entries(), &[entry(1, 0), entry(3, 1)]);
        assert!(!history.can_forward());
    }

    #[test]
    fn duplicate_records_are_ignored() {
        let mut history = NavigationHistory::new();
        history.record(entry(1, 2));
        history.record(entry(1, 2));
        assert_eq!(history.len(), 1);
        history.record(NavigationEntry {
            scroll: 4.0,
            ..entry(1, 2)
        });
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn history_is_capped() {
        let mut history = NavigationHistory::new();
        for index in 0..150 {
            history.record(entry(1, index));
        }
        assert_eq!(history.len(), MAX_HISTORY);
        assert_eq!(history.entries()[0].cursor, 50);
        assert_eq!(history.back(), Some(entry(1, 148)));
    }

    #[test]
    fn skipped_tabs_are_passed_over() {
        let mut history = NavigationHistory::new();
        history.record(entry(1, 0));
        history.record(entry(2, 0));
        history.record(entry(3, 0));
        let found = history.back_while(|entry| entry.tab_id != 2);
        assert_eq!(found, Some(entry(1, 0)));
    }

    #[test]
    fn removing_a_tab_keeps_the_current_location() {
        let mut history = NavigationHistory::new();
        history.record(entry(1, 0));
        history.record(entry(2, 0));
        history.record(entry(3, 0));
        history.back();
        // Current is tab 2; closing tab 1 must not move the cursor.
        history.remove_tab(1);
        assert_eq!(history.entries(), &[entry(2, 0), entry(3, 0)]);
        assert_eq!(history.back(), None);
        assert_eq!(history.forward(), Some(entry(3, 0)));
    }

    #[test]
    fn breadcrumbs_include_directories_and_symbol() {
        let segments = breadcrumbs(
            Some("Project"),
            Some(Path::new("src/ui/main.rs")),
            "main.rs",
            Some("run"),
        );
        let labels: Vec<_> = segments.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["Project", "src", "ui", "main.rs", "run"]);
        assert_eq!(
            segments[1].action,
            BreadcrumbAction::Directory("src".into())
        );
        assert_eq!(
            segments[2].action,
            BreadcrumbAction::Directory("src/ui".into())
        );
        assert_eq!(segments[3].action, BreadcrumbAction::File("main.rs".into()));
    }

    #[test]
    fn breadcrumbs_without_a_workspace_start_at_the_file() {
        let segments = breadcrumbs(None, None, "Untitled 1", None);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].label, "Untitled 1");
    }
}

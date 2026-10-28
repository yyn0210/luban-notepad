//! Shared helpers for the command palette and quick open.
//!
//! The UI state lives in `app.rs`; this module builds the grouped command
//! model and the quick-open target list so both can be unit-tested without a
//! window.

use std::path::PathBuf;

use crate::commands::{CommandCategory, CommandSpec, COMMANDS};
use crate::util;

/// Commands grouped by category, preserving registry order.
pub fn command_groups() -> Vec<(CommandCategory, Vec<&'static CommandSpec>)> {
    CommandCategory::ALL
        .iter()
        .filter_map(|category| {
            let commands: Vec<_> = COMMANDS
                .iter()
                .filter(|command| command.category == *category)
                .collect();
            (!commands.is_empty()).then_some((*category, commands))
        })
        .collect()
}

/// A target selected from the quick-open list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuickTarget {
    /// Activate an already open tab.
    Tab(u64),
    /// Open a recent file.
    File(PathBuf),
}

/// One quick-open row before filtering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickEntry {
    pub label: String,
    pub detail: String,
    pub target: QuickTarget,
}

pub struct OpenTabInfo<'a> {
    pub id: u64,
    pub name: String,
    pub path: Option<&'a PathBuf>,
    pub modified: bool,
}

/// Open tabs first, then recent files that are not already open.
pub fn quick_entries(tabs: &[OpenTabInfo<'_>], recent: &[PathBuf]) -> Vec<QuickEntry> {
    let mut entries: Vec<QuickEntry> = tabs
        .iter()
        .map(|tab| QuickEntry {
            label: if tab.modified {
                format!("● {}", tab.name)
            } else {
                tab.name.clone()
            },
            detail: tab
                .path
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            target: QuickTarget::Tab(tab.id),
        })
        .collect();
    for path in recent {
        if tabs
            .iter()
            .any(|tab| tab.path.is_some_and(|open| util::same_path(open, path)))
        {
            continue;
        }
        entries.push(QuickEntry {
            label: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            detail: path.display().to_string(),
            target: QuickTarget::File(path.clone()),
        });
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_cover_every_command_once() {
        let groups = command_groups();
        let total: usize = groups.iter().map(|(_, commands)| commands.len()).sum();
        assert_eq!(total, COMMANDS.len());
    }

    #[test]
    fn quick_open_lists_tabs_then_unopened_recent_files() {
        let open = PathBuf::from("/tmp/open.txt");
        let other = PathBuf::from("/tmp/other.txt");
        let tabs = vec![OpenTabInfo {
            id: 7,
            name: "open.txt".to_string(),
            path: Some(&open),
            modified: true,
        }];
        let entries = quick_entries(&tabs, &[open.clone(), other.clone()]);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].target, QuickTarget::Tab(7));
        assert!(entries[0].label.starts_with('●'));
        assert_eq!(entries[1].target, QuickTarget::File(other));
        assert_eq!(entries[1].label, "other.txt");
    }

    #[test]
    fn quick_open_handles_untitled_tabs() {
        let tabs = vec![OpenTabInfo {
            id: 2,
            name: "Untitled 1".to_string(),
            path: None,
            modified: false,
        }];
        let entries = quick_entries(&tabs, &[]);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].detail.is_empty());
    }
}

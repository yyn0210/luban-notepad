//! Windowed-editor state for disk-engine documents.
//!
//! The engine owns the complete file. The editor holds one bounded window;
//! this module tracks where that window lives in the document and the pending
//! window/save/search jobs that move or persist it.

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::{Duration, Instant};

use luban_core::disk_engine::WindowText;

use crate::dialogs::SaveThen;

/// How close the viewport may get to a window edge before the next window is
/// requested.
pub const EDGE_LINES: u64 = 200;

/// Where the viewport lands inside a freshly loaded window.
pub const PREFETCH_LINES: u64 = 600;

/// What a window request is anchored to. Line targets keep the editor's local
/// line numbers aligned; offset targets are used inside over-long lines and at
/// the end of the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowTarget {
    Line(u64),
    Offset(u64),
}

/// A window load in flight. `generation` discards results that arrive after
/// the user edited or scrolled again.
#[derive(Debug)]
pub struct WindowRequest {
    pub generation: u64,
    pub cursor: u64,
    pub top_line: u64,
    pub select: Option<(u64, u64)>,
}

#[derive(Debug)]
pub struct SaveJob {
    pub generation: u64,
    pub then: SaveThen,
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<AtomicU64>,
    pub total: u64,
    pub restore_read_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchKind {
    FindNext,
    FindAll,
    ReplaceOne,
    ReplaceAll,
}

#[derive(Debug)]
pub struct SearchJob {
    pub generation: u64,
    pub kind: SearchKind,
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<AtomicU64>,
    pub total: u64,
}

/// Per-tab state for a disk-engine document.
#[derive(Debug)]
pub struct HugeFileUi {
    pub window_start: u64,
    pub window_len: u64,
    pub window_line: u64,
    pub window_newlines: u64,
    pub window_column: u64,
    pub starts_mid_line: bool,
    pub at_eof: bool,
    pub last_text: String,
    pub next_generation: u64,
    pub pending_window: Option<WindowRequest>,
    pub save: Option<SaveJob>,
    /// A close/exit request that arrived while a save was already running.
    pub queued_save_then: Option<SaveThen>,
    pub search: Option<SearchJob>,
    /// The caret is outside the mounted window. Stores the local sentinel we
    /// placed in the editor and the real global offset, so scrolling alone
    /// does not lose the caret.
    pub offscreen_cursor: Option<(u64, u64)>,
    /// When the editor window was last replaced. Scheduling pauses briefly so
    /// the editor can lay the new text out before edge detection runs again.
    pub loaded_at: Instant,
    /// The undo-history limit was reported once for this document.
    pub history_trimmed_notified: bool,
}

impl HugeFileUi {
    pub fn new(window: WindowText) -> Self {
        Self {
            window_start: window.start,
            window_len: window.len,
            window_line: window.line,
            window_newlines: window.newlines,
            window_column: window.start_column,
            starts_mid_line: window.starts_mid_line,
            at_eof: window.at_eof,
            last_text: window.text,
            next_generation: 1,
            pending_window: None,
            save: None,
            queued_save_then: None,
            search: None,
            offscreen_cursor: None,
            loaded_at: Instant::now(),
            history_trimmed_notified: false,
        }
    }

    /// Install a newly loaded window.
    pub fn set_window(&mut self, window: WindowText) {
        self.window_start = window.start;
        self.window_len = window.len;
        self.window_line = window.line;
        self.window_newlines = window.newlines;
        self.window_column = window.start_column;
        self.starts_mid_line = window.starts_mid_line;
        self.at_eof = window.at_eof;
        self.last_text = window.text;
        self.pending_window = None;
        self.loaded_at = Instant::now();
    }

    /// Windows are not swapped again immediately after a load.
    pub fn settle_elapsed(&self) -> bool {
        self.loaded_at.elapsed() >= Duration::from_millis(300)
    }

    pub fn local_of_global(&self, global: u64) -> Option<u64> {
        global
            .checked_sub(self.window_start)
            .filter(|local| *local <= self.window_len)
    }

    pub fn global_line_of_local_line(&self, local_line: u64) -> u64 {
        self.window_line.saturating_add(local_line)
    }

    pub fn window_end_line(&self) -> u64 {
        self.window_line.saturating_add(self.window_newlines)
    }

    pub fn cancel_pending(&mut self) {
        self.pending_window = None;
        self.next_generation = self.next_generation.wrapping_add(1);
    }

    pub fn next_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        generation
    }

    pub fn is_busy(&self) -> bool {
        self.save.is_some() || self.search.is_some()
    }
}

/// Diff two window snapshots, returning the changed byte range and the new
/// text. Both inputs are UTF-8; the returned offsets are char boundaries.
pub fn diff_bytes<'a>(old: &str, new: &'a str) -> (usize, usize, &'a str) {
    let old_bytes = old.as_bytes();
    let new_bytes = new.as_bytes();
    let min_len = old_bytes.len().min(new_bytes.len());
    let mut prefix = 0;
    while prefix < min_len && old_bytes[prefix] == new_bytes[prefix] {
        prefix += 1;
    }
    while prefix > 0 && !old.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let max_suffix = min_len - prefix;
    let mut suffix = 0;
    while suffix < max_suffix
        && old_bytes[old_bytes.len() - 1 - suffix] == new_bytes[new_bytes.len() - 1 - suffix]
    {
        suffix += 1;
    }
    while suffix > 0
        && (!old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }
    let removed = old.len() - suffix - prefix;
    let inserted = &new[prefix..new.len() - suffix];
    (prefix, removed, inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_finds_a_minimal_change() {
        let (start, removed, inserted) = diff_bytes("hello world", "hello brave world");
        assert_eq!(start, 6);
        assert_eq!(removed, 0);
        assert_eq!(inserted, "brave ");
    }

    #[test]
    fn diff_handles_unicode() {
        let (start, removed, inserted) = diff_bytes("aé世z", "aé世界z");
        assert_eq!(start, 6);
        assert_eq!(removed, 0);
        assert_eq!(inserted, "界");
    }

    #[test]
    fn window_mapping_tracks_edits() {
        let window = WindowText {
            text: "abc\ndef\n".into(),
            start: 100,
            len: 8,
            line: 10,
            newlines: 2,
            start_column: 1,
            starts_mid_line: false,
            ends_mid_line: false,
            at_eof: false,
        };
        let ui = HugeFileUi::new(window);
        assert_eq!(ui.local_of_global(104), Some(4));
        assert_eq!(ui.window_end_line(), 12);
    }
}

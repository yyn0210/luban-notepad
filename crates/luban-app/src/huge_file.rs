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
use crate::huge_scrollbar::HugeScrollShared;

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
    pub requested_at: Instant,
}

/// A jump that arrived while another window read was in flight. Only the
/// newest one is kept; it is issued when the in-flight read finishes, so a tab
/// never has two disk reads running at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowJump {
    pub target: WindowTarget,
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
    /// The newest jump that arrived while `pending_window` was in flight.
    pub queued_window: Option<WindowJump>,
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
    /// Global line currently shown at the top of the editor viewport.
    pub scroll_top_line: u64,
    /// Top line a just-installed window will show once the editor applies its
    /// deferred scroll offset. Sync reports this instead of deriving from the
    /// previous window's offset, which is still in the scroll handle for one
    /// frame after the swap.
    pub pending_top_line: Option<u64>,
    /// Global line a file-global scrollbar drag wants to reach. Kept until the
    /// viewport arrives, so a pending window load cannot overwrite a newer
    /// pointer position.
    pub scroll_target: Option<u64>,
    /// Shared state with the rendered file-global scrollbar.
    pub scroll: HugeScrollShared,
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
            queued_window: None,
            save: None,
            queued_save_then: None,
            search: None,
            offscreen_cursor: None,
            loaded_at: Instant::now(),
            history_trimmed_notified: false,
            scroll_top_line: window.line,
            pending_top_line: None,
            scroll_target: None,
            scroll: HugeScrollShared::default(),
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
        self.pending_top_line = None;
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

    pub fn window_end_line(&self) -> u64 {
        self.window_line.saturating_add(self.window_newlines)
    }

    pub fn cancel_pending(&mut self) {
        self.pending_window = None;
        self.queued_window = None;
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

/// 1-based global line at the top of a viewport scrolled by `offset_y`.
///
/// The editor scrolls in whole rows and lays them out on a uniform
/// `line_height` grid, so the top row follows from the offset alone. The
/// offset is updated immediately by wheel input, while `visible_row_range`
/// describes the previous layout; the scrollbar and gutter use this value to
/// stay in step with the content.
pub fn top_line_from_scroll(
    offset_y: f32,
    line_height: f32,
    window_line: u64,
    window_newlines: u64,
) -> u64 {
    if line_height <= 0.0 || !offset_y.is_finite() {
        return window_line;
    }
    let rows = ((-offset_y).max(0.0) / line_height).floor();
    let rows = if rows.is_finite() { rows as u64 } else { 0 };
    window_line
        .saturating_add(rows)
        .min(window_line.saturating_add(window_newlines))
}

/// Part of a row hidden above the viewport top when the viewport is scrolled
/// by `offset_y`. Whole-row scrolling leaves `0`.
pub fn scroll_row_clip(offset_y: f32, line_height: f32) -> f32 {
    if line_height <= 0.0 || !offset_y.is_finite() {
        return 0.0;
    }
    let scrolled = (-offset_y).max(0.0);
    let clip = scrolled % line_height;
    if clip.is_finite() {
        clip
    } else {
        0.0
    }
}

/// Largest 1-based top line that still fills `viewport_lines` rows of a
/// document with `total_lines` lines. A document shorter than one screen has a
/// single valid top line.
pub fn max_top_line(total_lines: u64, viewport_lines: u64) -> u64 {
    total_lines
        .saturating_sub(viewport_lines.max(1).saturating_sub(1))
        .max(1)
}

/// Largest top line that still fills a viewport inside a mounted window.
///
/// A window starting at `window_line` with `window_newlines` line breaks ends
/// at `window_line + window_newlines`. The last full screen starts at
/// `end - viewport_lines + 1`; a window shorter than the viewport has only its
/// first line as a valid top.
pub fn window_max_top_line(window_line: u64, window_newlines: u64, viewport_lines: u64) -> u64 {
    window_line
        .saturating_add(window_newlines)
        .saturating_sub(viewport_lines.max(1).saturating_sub(1))
        .max(window_line)
}

/// Clamp a desired top line into the mounted window's valid viewport tops.
pub fn clamp_top_line(
    desired: u64,
    window_line: u64,
    window_newlines: u64,
    viewport_lines: u64,
) -> u64 {
    desired.clamp(
        window_line,
        window_max_top_line(window_line, window_newlines, viewport_lines),
    )
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

    fn sample_window() -> WindowText {
        WindowText {
            text: "abc\n".into(),
            start: 0,
            len: 4,
            line: 1,
            newlines: 1,
            start_column: 1,
            starts_mid_line: false,
            ends_mid_line: false,
            at_eof: false,
        }
    }

    #[test]
    fn cancelling_pending_drops_the_queued_jump() {
        let mut ui = HugeFileUi::new(sample_window());
        ui.queued_window = Some(WindowJump {
            target: WindowTarget::Line(5),
            cursor: 7,
            top_line: 1,
            select: None,
        });
        ui.cancel_pending();
        assert!(ui.queued_window.is_none());
    }

    #[test]
    fn installing_a_window_keeps_the_queued_jump() {
        // The app issues the queued jump from the completion path, so
        // `set_window` must not drop it.
        let mut ui = HugeFileUi::new(sample_window());
        ui.queued_window = Some(WindowJump {
            target: WindowTarget::Line(5),
            cursor: 7,
            top_line: 1,
            select: None,
        });
        ui.set_window(sample_window());
        assert!(ui.queued_window.is_some());
    }

    #[test]
    fn max_top_line_leaves_a_full_screen() {
        assert_eq!(max_top_line(100, 10), 91);
        assert_eq!(max_top_line(10, 10), 1);
        assert_eq!(max_top_line(5, 10), 1);
        assert_eq!(max_top_line(100, 1), 100);
        assert_eq!(max_top_line(100, 0), 100);
        assert_eq!(max_top_line(0, 10), 1);
    }

    #[test]
    fn window_top_line_cannot_scroll_past_the_window_end() {
        // Window lines 10..=30, viewport 10 rows: the last full screen starts
        // at 21, not at the window end.
        assert_eq!(window_max_top_line(10, 20, 10), 21);
        assert_eq!(clamp_top_line(30, 10, 20, 10), 21);
        assert_eq!(clamp_top_line(5, 10, 20, 10), 10);
        assert_eq!(clamp_top_line(15, 10, 20, 10), 15);
        // A window shorter than the viewport has one valid top line.
        assert_eq!(window_max_top_line(10, 3, 10), 10);
        assert_eq!(clamp_top_line(12, 10, 3, 10), 10);
    }

    #[test]
    fn range_end_clamp_keeps_the_last_screen() {
        // The reproduced case: window starts near the range end with 634 line
        // breaks and a 37-row viewport.
        let window_line = 12_548_479;
        let newlines = 634;
        let viewport = 37;
        let end = window_line + newlines;
        assert_eq!(
            window_max_top_line(window_line, newlines, viewport),
            end - 36
        );
        assert_eq!(
            clamp_top_line(end - 5, window_line, newlines, viewport),
            end - 36
        );
    }

    #[test]
    fn scroll_offset_yields_the_top_line() {
        // No scroll, scrolled down three whole rows, and a positive offset
        // (overscroll) all resolve inside the window.
        assert_eq!(top_line_from_scroll(0.0, 18.0, 100, 500), 100);
        assert_eq!(top_line_from_scroll(-54.0, 18.0, 100, 500), 103);
        assert_eq!(top_line_from_scroll(9.0, 18.0, 100, 500), 100);
        // A fractional offset still reports the row at the viewport top.
        assert_eq!(top_line_from_scroll(-54.9, 18.0, 100, 500), 103);
        // Never past the last line of the mounted window.
        assert_eq!(top_line_from_scroll(-18.0 * 900.0, 18.0, 100, 500), 600);
        // Before the first layout the caller keeps the window's first line.
        assert_eq!(top_line_from_scroll(-54.0, 0.0, 100, 500), 100);
        assert_eq!(top_line_from_scroll(f32::NAN, 18.0, 100, 500), 100);
    }

    #[test]
    fn scroll_clip_is_the_hidden_part_of_the_top_row() {
        assert_eq!(scroll_row_clip(0.0, 18.0), 0.0);
        assert_eq!(scroll_row_clip(-18.0, 18.0), 0.0);
        assert_eq!(scroll_row_clip(-27.0, 18.0), 9.0);
        assert_eq!(scroll_row_clip(9.0, 18.0), 0.0);
        assert_eq!(scroll_row_clip(-27.0, 0.0), 0.0);
    }
}

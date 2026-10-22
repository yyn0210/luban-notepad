//! Undo/redo history.
//!
//! Edits are recorded as a minimal replacement (start, removed text, inserted
//! text). Consecutive typing and deletion are coalesced into a single history
//! entry so that one Ctrl+Z undoes a burst of typing rather than a single
//! character.

use std::time::{Duration, Instant};

/// How an edit was produced. Used only for coalescing heuristics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    Typing,
    Delete,
    Other,
}

/// One reversible text replacement. Positions are character indices.
#[derive(Clone, Debug)]
pub struct Edit {
    pub start: usize,
    pub removed: String,
    pub inserted: String,
    pub cursor_before: usize,
    pub cursor_after: usize,
    pub selection_before: Option<(usize, usize)>,
    pub selection_after: Option<(usize, usize)>,
    pub kind: EditKind,
    pub at: Instant,
}

impl Edit {
    pub fn removed_chars(&self) -> usize {
        self.removed.chars().count()
    }

    pub fn inserted_chars(&self) -> usize {
        self.inserted.chars().count()
    }
}

const MERGE_WINDOW: Duration = Duration::from_millis(700);
const DEFAULT_LIMIT: usize = 500;

/// Bounded undo/redo stacks.
#[derive(Debug)]
pub struct UndoStack {
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    limit: usize,
}

impl Default for UndoStack {
    fn default() -> Self {
        Self::new(DEFAULT_LIMIT)
    }
}

impl UndoStack {
    pub fn new(limit: usize) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            limit: limit.max(1),
        }
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Record a new edit, coalescing it with the previous one when possible.
    pub fn push(&mut self, edit: Edit) {
        self.redo.clear();
        if let Some(last) = self.undo.last_mut() {
            if can_merge(last, &edit) {
                merge(last, edit);
                return;
            }
        }
        self.undo.push(edit);
        if self.undo.len() > self.limit {
            self.undo.remove(0);
        }
    }

    /// Record an edit immediately without attempting to coalesce.
    pub fn push_barrier(&mut self, edit: Edit) {
        self.redo.clear();
        self.undo.push(edit);
        if self.undo.len() > self.limit {
            self.undo.remove(0);
        }
    }

    pub fn undo(&mut self) -> Option<Edit> {
        let edit = self.undo.pop()?;
        self.redo.push(edit.clone());
        Some(edit)
    }

    pub fn redo(&mut self) -> Option<Edit> {
        let edit = self.redo.pop()?;
        self.undo.push(edit.clone());
        Some(edit)
    }
}

fn can_merge(last: &Edit, next: &Edit) -> bool {
    if next.at.duration_since(last.at) > MERGE_WINDOW {
        return false;
    }
    match (last.kind, next.kind) {
        (EditKind::Typing, EditKind::Typing) => {
            // Do not grow one huge undo entry across a newline.
            if last.inserted.contains('\n') || last.inserted.contains('\r') {
                return false;
            }
            next.removed.is_empty()
                && last.removed.is_empty()
                && next.start == last.start + last.inserted_chars()
        }
        (EditKind::Delete, EditKind::Delete) => {
            if last.removed.contains('\n') || last.removed.contains('\r') {
                return false;
            }
            // Backspace: the next deletion starts before the previous one.
            if !next.removed.is_empty() && next.inserted.is_empty() {
                next.start + next.removed_chars() == last.start || next.start == last.start
            } else {
                false
            }
        }
        _ => false,
    }
}

fn merge(last: &mut Edit, next: Edit) {
    match (last.kind, next.kind) {
        (EditKind::Typing, EditKind::Typing) => {
            last.inserted.push_str(&next.inserted);
        }
        (EditKind::Delete, EditKind::Delete) => {
            if next.start < last.start {
                // Backspace: prepend the newly deleted text.
                let mut removed = next.removed;
                removed.push_str(&last.removed);
                last.removed = removed;
                last.start = next.start;
            } else {
                // Forward delete: append.
                last.removed.push_str(&next.removed);
            }
        }
        _ => {}
    }
    last.cursor_after = next.cursor_after;
    last.selection_after = next.selection_after;
    last.at = next.at;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(start: usize, removed: &str, inserted: &str, kind: EditKind, at: Instant) -> Edit {
        Edit {
            start,
            removed: removed.into(),
            inserted: inserted.into(),
            cursor_before: start,
            cursor_after: start + inserted.chars().count(),
            selection_before: None,
            selection_after: None,
            kind,
            at,
        }
    }

    #[test]
    fn undoes_and_redoes() {
        let mut stack = UndoStack::new(10);
        let now = Instant::now();
        stack.push(edit(0, "", "a", EditKind::Other, now));
        assert!(stack.can_undo());
        let e = stack.undo().unwrap();
        assert_eq!(e.inserted, "a");
        assert!(stack.can_redo());
        let e = stack.redo().unwrap();
        assert_eq!(e.inserted, "a");
        assert!(!stack.can_redo());
    }

    #[test]
    fn coalesces_typing() {
        let mut stack = UndoStack::new(10);
        let now = Instant::now();
        stack.push(edit(0, "", "h", EditKind::Typing, now));
        stack.push(edit(
            1,
            "",
            "i",
            EditKind::Typing,
            now + Duration::from_millis(50),
        ));
        assert_eq!(stack.undo_len(), 1);
        let e = stack.undo().unwrap();
        assert_eq!(e.inserted, "hi");
    }

    #[test]
    fn coalesces_backspaces() {
        let mut stack = UndoStack::new(10);
        let now = Instant::now();
        // "abc" -> delete c -> delete b
        stack.push(edit(2, "c", "", EditKind::Delete, now));
        stack.push(edit(
            1,
            "b",
            "",
            EditKind::Delete,
            now + Duration::from_millis(30),
        ));
        assert_eq!(stack.undo_len(), 1);
        let e = stack.undo().unwrap();
        assert_eq!(e.start, 1);
        assert_eq!(e.removed, "bc");
    }

    #[test]
    fn does_not_coalesce_after_pause_or_newline() {
        let mut stack = UndoStack::new(10);
        let now = Instant::now();
        stack.push(edit(0, "", "a", EditKind::Typing, now));
        stack.push(edit(
            1,
            "",
            "b",
            EditKind::Typing,
            now + Duration::from_secs(2),
        ));
        assert_eq!(stack.undo_len(), 2);

        stack.push(edit(
            2,
            "",
            "\n",
            EditKind::Typing,
            now + Duration::from_secs(3),
        ));
        stack.push(edit(
            3,
            "",
            "c",
            EditKind::Typing,
            now + Duration::from_secs(3),
        ));
        assert_eq!(stack.undo_len(), 4);
    }

    #[test]
    fn new_edit_clears_redo() {
        let mut stack = UndoStack::new(10);
        let now = Instant::now();
        stack.push(edit(0, "", "a", EditKind::Other, now));
        stack.undo();
        assert!(stack.can_redo());
        stack.push(edit(0, "", "b", EditKind::Other, now));
        assert!(!stack.can_redo());
    }

    #[test]
    fn respects_history_limit() {
        let mut stack = UndoStack::new(2);
        let now = Instant::now();
        for i in 0..5 {
            stack.push(edit(i, "", "x", EditKind::Other, now));
        }
        assert_eq!(stack.undo_len(), 2);
    }
}

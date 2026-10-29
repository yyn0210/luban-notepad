//! Line bookmarks for the editor.
//!
//! Bookmarks are view metadata: they are never written into the document and
//! never enter the undo stack. The session stores 1-based line numbers, while
//! an open document keeps them in memory and adjusts them when text changes.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::text::LineIndex;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookmarkSet {
    lines: BTreeSet<usize>,
}

impl BookmarkSet {
    pub fn from_lines(lines: impl IntoIterator<Item = usize>) -> Self {
        let mut set = Self::default();
        for line in lines {
            if line > 0 {
                set.lines.insert(line);
            }
        }
        set
    }

    pub fn lines(&self) -> impl Iterator<Item = usize> + '_ {
        self.lines.iter().copied()
    }

    pub fn as_vec(&self) -> Vec<usize> {
        self.lines.iter().copied().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn contains(&self, line: usize) -> bool {
        self.lines.contains(&line)
    }

    pub fn insert(&mut self, line: usize) -> bool {
        line > 0 && self.lines.insert(line)
    }

    pub fn remove(&mut self, line: usize) -> bool {
        self.lines.remove(&line)
    }

    /// Toggle a 1-based line and return whether it is now bookmarked.
    pub fn toggle(&mut self, line: usize) -> bool {
        if line == 0 {
            return false;
        }
        if !self.lines.remove(&line) {
            self.lines.insert(line);
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }

    /// Next bookmark strictly after `line`.
    pub fn next_after(&self, line: usize) -> Option<usize> {
        self.lines.range(line + 1..).next().copied()
    }

    /// Previous bookmark strictly before `line`.
    pub fn previous_before(&self, line: usize) -> Option<usize> {
        self.lines.range(..line).next_back().copied()
    }

    /// Drop bookmarks past the end of a document with `line_count` lines.
    pub fn clamp(&mut self, line_count: usize) {
        let line_count = line_count.max(1);
        self.lines.retain(|line| *line <= line_count);
    }

    /// Adjust bookmarks after `old_text` became `new_text`.
    ///
    /// The edit region is the characters between the common prefix and suffix.
    /// A bookmark whose line start lies inside that region can no longer be
    /// mapped reliably and is removed; bookmarks after it shift by the length
    /// difference and are converted back to line numbers.
    pub fn adjust(&mut self, old_text: &str, new_text: &str) {
        if self.lines.is_empty() || old_text == new_text {
            return;
        }
        let old_chars: Vec<char> = old_text.chars().collect();
        let new_chars: Vec<char> = new_text.chars().collect();
        let old_len = old_chars.len();
        let new_len = new_chars.len();
        let mut prefix = 0;
        while prefix < old_len && prefix < new_len && old_chars[prefix] == new_chars[prefix] {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < old_len.saturating_sub(prefix)
            && suffix < new_len.saturating_sub(prefix)
            && old_chars[old_len - 1 - suffix] == new_chars[new_len - 1 - suffix]
        {
            suffix += 1;
        }
        let old_index = LineIndex::new(old_text);
        let new_index = LineIndex::new(new_text);
        let old_suffix_start = old_len.saturating_sub(suffix);
        let mut adjusted = BTreeSet::new();
        for line in self.lines.iter().copied() {
            let start = old_index.line_start(line.saturating_sub(1));
            if start < prefix {
                adjusted.insert(line);
            } else if start >= old_suffix_start {
                let shifted = start as isize + (new_len as isize - old_len as isize);
                if shifted >= 0 {
                    let new_line = new_index.line_of_char(shifted as usize) + 1;
                    if new_line <= new_index.len() {
                        adjusted.insert(new_line);
                    }
                }
            }
        }
        self.lines = adjusted;
    }

    /// Raw text of the bookmarked lines, including their terminators.
    pub fn selected_text(&self, text: &str) -> String {
        let index = LineIndex::new(text);
        let mut out = String::new();
        for line in self.lines.iter().copied() {
            let start = index.line_start(line.saturating_sub(1));
            let end = index.line_end_including_newline(line.saturating_sub(1));
            out.push_str(&crate::text::slice_chars(text, start, end));
        }
        out
    }

    /// Remove the bookmarked lines. The bookmark set is cleared because the
    /// lines it referred to no longer exist.
    pub fn delete_lines(&mut self, text: &str) -> String {
        let mut lines: Vec<(String, String)> = Vec::new();
        let index = LineIndex::new(text);
        for line in 0..index.len() {
            let start = index.line_start(line);
            let content_end = index.line_content_end(line);
            let end = index.line_end_including_newline(line);
            lines.push((
                crate::text::slice_chars(text, start, content_end),
                crate::text::slice_chars(text, content_end, end),
            ));
        }
        let mut out = String::new();
        for (line, (content, ending)) in lines.into_iter().enumerate() {
            if !self.lines.contains(&(line + 1)) {
                out.push_str(&content);
                out.push_str(&ending);
            }
        }
        self.lines.clear();
        out
    }

    /// Cut the bookmarked lines, returning the clipboard text and the new
    /// document text.
    pub fn cut_lines(&mut self, text: &str) -> (String, String) {
        let clipboard = self.selected_text(text);
        let remaining = self.delete_lines(text);
        (clipboard, remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggles_and_navigates() {
        let mut set = BookmarkSet::default();
        assert!(set.toggle(3));
        assert!(set.toggle(1));
        assert!(!set.toggle(3));
        assert_eq!(set.as_vec(), vec![1]);
        assert_eq!(set.next_after(1), None);
        assert_eq!(set.previous_before(5), Some(1));
        set.insert(7);
        assert_eq!(set.next_after(1), Some(7));
        assert_eq!(set.previous_before(7), Some(1));
    }

    #[test]
    fn adjusts_offsets_after_insert_and_delete() {
        let mut set = BookmarkSet::from_lines([2, 4]);
        set.adjust("a\nb\nc\nd\n", "a\nX\nb\nc\nd\n");
        assert_eq!(set.as_vec(), vec![3, 5]);

        let mut set = BookmarkSet::from_lines([3, 5]);
        set.adjust("a\nX\nb\nc\nd\n", "a\nb\nc\nd\n");
        assert_eq!(set.as_vec(), vec![2, 4]);
    }

    #[test]
    fn removes_bookmark_when_its_line_disappears() {
        let mut set = BookmarkSet::from_lines([2]);
        set.adjust("a\nb\nc\n", "a\nc\n");
        assert!(set.is_empty());
    }

    #[test]
    fn selection_cut_and_delete() {
        let set = BookmarkSet::from_lines([2, 4]);
        assert_eq!(set.selected_text("a\nb\nc\nd\n"), "b\nd\n");
        let mut set = BookmarkSet::from_lines([2, 4]);
        let (clipboard, text) = set.cut_lines("a\nb\nc\nd\n");
        assert_eq!(clipboard, "b\nd\n");
        assert_eq!(text, "a\nc\n");
        assert!(set.is_empty());
    }

    #[test]
    fn clamps_to_document() {
        let mut set = BookmarkSet::from_lines([1, 5, 9]);
        set.clamp(6);
        assert_eq!(set.as_vec(), vec![1, 5]);
    }
}

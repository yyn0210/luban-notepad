//! The editor document: text, file metadata, selection and undo history.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use crate::encoding::{self, TextEncoding};
use crate::eol::{self, Eol};
use crate::error::{Error, Result};
use crate::language;
use crate::text::{self, LineIndex};
use crate::undo::{Edit, EditKind, UndoStack};

/// Files larger than this are rejected by default. The GUI allows changing the
/// limit in Preferences; 256 MiB is beyond what a text widget can usefully
/// render while still protecting against accidental huge opens.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 256 * 1024 * 1024;

/// An open text document.
#[derive(Debug)]
pub struct Document {
    pub path: Option<PathBuf>,
    pub text: String,
    pub encoding: TextEncoding,
    pub eol: Eol,
    pub language: String,
    /// Caret position as a character index.
    pub cursor: usize,
    /// Selection anchor as a character index. Equal to `cursor` without a
    /// selection.
    pub anchor: usize,
    pub scroll_offset: f32,
    pub undo: UndoStack,
    pub read_only: bool,
    pub last_modified: Option<SystemTime>,
    pub file_size: u64,
    pub untitled_name: String,
    revision: u64,
    saved_revision: u64,
}

impl Document {
    /// Create an empty, untitled document.
    pub fn new_untitled(name: impl Into<String>) -> Self {
        Self {
            path: None,
            text: String::new(),
            encoding: TextEncoding::Utf8,
            eol: Eol::native(),
            language: language::PLAIN_TEXT.to_string(),
            cursor: 0,
            anchor: 0,
            scroll_offset: 0.0,
            undo: UndoStack::default(),
            read_only: false,
            last_modified: None,
            file_size: 0,
            untitled_name: name.into(),
            revision: 0,
            saved_revision: 0,
        }
    }

    /// Build a document from already-decoded text (used for session restore
    /// and tests).
    pub fn from_text(
        text: impl Into<String>,
        encoding: TextEncoding,
        eol: Eol,
        language_name: impl Into<String>,
    ) -> Self {
        let text = text.into();
        let mut doc = Self::new_untitled("new 1");
        doc.file_size = text.len() as u64;
        doc.text = text;
        doc.encoding = encoding;
        doc.eol = eol;
        doc.language = language_name.into();
        doc.revision = 1;
        doc.saved_revision = 1;
        doc
    }

    /// Open a file, detecting encoding, line endings and language.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_limit(path, DEFAULT_MAX_FILE_SIZE)
    }

    pub fn open_with_limit(path: impl AsRef<Path>, max_size: u64) -> Result<Self> {
        let path = path.as_ref();
        let metadata = fs::metadata(path)?;
        if metadata.is_dir() {
            return Err(Error::InvalidPath(path.to_path_buf()));
        }
        if metadata.len() > max_size {
            return Err(Error::FileTooLarge {
                size: metadata.len(),
                limit: max_size,
            });
        }
        let bytes = fs::read(path)?;
        if encoding::looks_binary(&bytes) {
            return Err(Error::BinaryFile(path.to_path_buf()));
        }
        let decoded = encoding::decode(&bytes);
        let detected_eol = eol::detect(&decoded.text);
        let language_name = language::detect_from_path(path);
        let mut doc = Self::new_untitled("new 1");
        doc.path = Some(path.to_path_buf());
        // The editing buffer is always LF; the file's convention lives in
        // `doc.eol` and is restored when saving.
        doc.text = eol::to_lf(&decoded.text);
        doc.encoding = decoded.encoding;
        doc.eol = detected_eol;
        doc.language = language_name.to_string();
        doc.file_size = metadata.len();
        doc.last_modified = metadata.modified().ok();
        doc.read_only = metadata.permissions().readonly();
        doc.revision = 1;
        doc.saved_revision = 1;
        Ok(doc)
    }

    /// Create an untitled document containing text (e.g. from a template).
    pub fn with_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let detected_eol = eol::detect(&text);
        let mut doc = Self::new_untitled("new 1");
        doc.text = eol::to_lf(&text);
        doc.eol = detected_eol;
        doc.revision = 1;
        doc.saved_revision = 0; // unsaved content
        doc
    }

    pub fn display_name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.untitled_name.clone())
    }

    pub fn full_path_display(&self) -> String {
        self.path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| self.untitled_name.clone())
    }

    pub fn is_untitled(&self) -> bool {
        self.path.is_none()
    }

    pub fn is_modified(&self) -> bool {
        self.revision != self.saved_revision
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn mark_saved(&mut self) {
        self.saved_revision = self.revision;
    }

    /// Mark the document dirty after a metadata-only change.
    pub fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn line_count(&self) -> usize {
        LineIndex::new(&self.text).len()
    }

    pub fn line_col(&self) -> (usize, usize) {
        LineIndex::new(&self.text).line_col(self.cursor)
    }

    pub fn selection(&self) -> (usize, usize) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    pub fn has_selection(&self) -> bool {
        self.anchor != self.cursor
    }

    pub fn selected_text(&self) -> String {
        let (a, b) = self.selection();
        text::slice_chars(&self.text, a, b)
    }

    pub fn set_selection(&mut self, anchor: usize, cursor: usize) {
        let total = text::char_count(&self.text);
        self.anchor = anchor.min(total);
        self.cursor = cursor.min(total);
    }

    pub fn select_all(&mut self) {
        let total = text::char_count(&self.text);
        self.anchor = 0;
        self.cursor = total;
    }

    pub fn char_count(&self) -> usize {
        text::char_count(&self.text)
    }

    /// Apply a replacement edit and record it in the undo history.
    pub fn apply_edit(
        &mut self,
        start: usize,
        removed_chars: usize,
        inserted: &str,
        kind: EditKind,
        cursor_after: usize,
        anchor_after: usize,
    ) -> bool {
        let total = self.char_count();
        if start > total {
            return false;
        }
        let removed_chars = removed_chars.min(total - start);
        if removed_chars == 0 && inserted.is_empty() {
            return false;
        }
        let removed = text::slice_chars(&self.text, start, start + removed_chars);
        let selection_before = (self.anchor, self.cursor);

        let b0 = text::char_to_byte(&self.text, start);
        let b1 = text::char_to_byte(&self.text, start + removed_chars);
        let mut new_text = String::with_capacity(self.text.len() + inserted.len() - (b1 - b0));
        new_text.push_str(&self.text[..b0]);
        new_text.push_str(inserted);
        new_text.push_str(&self.text[b1..]);

        let edit = Edit {
            start,
            removed,
            inserted: inserted.to_owned(),
            cursor_before: self.cursor,
            cursor_after: cursor_after.min(text::char_count(&new_text)),
            selection_before: Some(selection_before),
            selection_after: Some((
                anchor_after.min(text::char_count(&new_text)),
                cursor_after.min(text::char_count(&new_text)),
            )),
            kind,
            at: Instant::now(),
        };
        self.text = new_text;
        self.cursor = edit.cursor_after;
        self.anchor = edit.selection_after.map(|s| s.0).unwrap_or(self.cursor);
        self.revision = self.revision.wrapping_add(1);
        self.undo.push(edit);
        true
    }

    /// Reconcile an arbitrary whole-buffer replacement (as produced by a text
    /// widget) with the document, deriving a minimal undo edit.
    pub fn set_text_from_widget(
        &mut self,
        new_text: &str,
        cursor_after: usize,
        anchor_after: usize,
    ) -> bool {
        if new_text == self.text {
            self.cursor = cursor_after.min(self.char_count());
            self.anchor = anchor_after.min(self.char_count());
            return false;
        }
        let (start, removed, inserted) = diff_chars(&self.text, new_text);
        let kind = if removed.is_empty() && !inserted.is_empty() {
            EditKind::Typing
        } else if !removed.is_empty() && inserted.is_empty() {
            EditKind::Delete
        } else {
            EditKind::Other
        };
        self.apply_edit(
            start,
            removed.chars().count(),
            &inserted,
            kind,
            cursor_after,
            anchor_after,
        )
    }

    pub fn undo(&mut self) -> bool {
        let Some(edit) = self.undo.undo() else {
            return false;
        };
        if !self.apply_history(&edit, true) {
            // The history no longer matches the buffer; drop it to avoid
            // corrupting the document.
            self.undo.clear();
            return false;
        }
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(edit) = self.undo.redo() else {
            return false;
        };
        if !self.apply_history(&edit, false) {
            self.undo.clear();
            return false;
        }
        true
    }

    fn apply_history(&mut self, edit: &Edit, undo: bool) -> bool {
        let (remove_len, insert_text, selection) = if undo {
            (
                edit.inserted.chars().count(),
                edit.removed.as_str(),
                edit.selection_before,
            )
        } else {
            (
                edit.removed.chars().count(),
                edit.inserted.as_str(),
                edit.selection_after,
            )
        };
        let total = self.char_count();
        if edit.start > total || remove_len > total - edit.start {
            return false;
        }
        let current = text::slice_chars(&self.text, edit.start, edit.start + remove_len);
        let expected = if undo {
            edit.inserted.as_str()
        } else {
            edit.removed.as_str()
        };
        if current != expected {
            return false;
        }
        let b0 = text::char_to_byte(&self.text, edit.start);
        let b1 = text::char_to_byte(&self.text, edit.start + remove_len);
        let mut new_text = String::with_capacity(self.text.len() + insert_text.len());
        new_text.push_str(&self.text[..b0]);
        new_text.push_str(insert_text);
        new_text.push_str(&self.text[b1..]);
        self.text = new_text;
        self.revision = self.revision.wrapping_add(1);
        if let Some((anchor, cursor)) = selection {
            self.anchor = anchor.min(self.char_count());
            self.cursor = cursor.min(self.char_count());
        }
        true
    }

    /// Change the line-ending convention used when saving.
    ///
    /// Editing always uses LF internally, so this is a metadata change and is
    /// intentionally not part of the text undo history. It does mark the
    /// document dirty, because the bytes written to disk will change.
    pub fn convert_eol(&mut self, new_eol: Eol) -> bool {
        if self.eol == new_eol {
            return false;
        }
        self.eol = new_eol;
        self.touch();
        true
    }

    pub fn set_encoding(&mut self, new_encoding: TextEncoding) -> bool {
        if self.encoding == new_encoding {
            return false;
        }
        self.encoding = new_encoding;
        self.touch();
        true
    }

    pub fn set_language(&mut self, name: impl Into<String>) -> bool {
        let name = name.into();
        if self.language == name {
            return false;
        }
        self.language = name;
        true
    }

    pub fn auto_detect_language(&mut self) {
        if let Some(path) = &self.path {
            self.language = language::detect_from_path(path).to_string();
        }
    }

    /// Save to the current path. Returns `Ok(true)` when the selected encoding
    /// could not represent every character (the file was still written).
    pub fn save(&mut self) -> Result<bool> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| Error::Other(crate::i18n::tr("core.document.no_path")))?;
        self.save_to_path(&path)
    }

    pub fn save_as(&mut self, path: impl AsRef<Path>) -> Result<bool> {
        let path = path.as_ref().to_path_buf();
        let lossy = self.save_to_path(&path)?;
        let was_plain = self.language == language::PLAIN_TEXT;
        self.path = Some(path);
        if was_plain {
            self.auto_detect_language();
        }
        Ok(lossy)
    }

    fn save_to_path(&mut self, path: &Path) -> Result<bool> {
        if self.read_only {
            return Err(Error::Other(crate::i18n::tr("core.document.read_only")));
        }
        let with_eol = eol::normalize(&self.text, self.eol);
        let (bytes, lossy) = encoding::encode(&with_eol, self.encoding);
        fs::write(path, &bytes)?;
        let metadata = fs::metadata(path)?;
        self.file_size = metadata.len();
        self.last_modified = metadata.modified().ok();
        self.mark_saved();
        Ok(lossy)
    }

    /// Reload the file from disk, discarding the undo history.
    pub fn reload(&mut self) -> Result<()> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| Error::Other(crate::i18n::tr("core.document.no_path")))?;
        let replacement = Self::open(&path)?;
        self.text = replacement.text;
        self.encoding = replacement.encoding;
        self.eol = replacement.eol;
        self.last_modified = replacement.last_modified;
        self.file_size = replacement.file_size;
        self.read_only = replacement.read_only;
        self.undo.clear();
        self.cursor = self.cursor.min(self.char_count());
        self.anchor = self.anchor.min(self.char_count());
        self.revision = self.revision.wrapping_add(1);
        self.saved_revision = self.revision;
        Ok(())
    }

    /// Whether the file on disk appears to have changed since opening/saving.
    pub fn changed_on_disk(&self) -> bool {
        let Some(path) = &self.path else {
            return false;
        };
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        let modified_changed = match (metadata.modified().ok(), self.last_modified) {
            (Some(current), Some(known)) => current > known,
            _ => false,
        };
        modified_changed || metadata.len() != self.file_size
    }

    pub fn replace_selection(&mut self, replacement: &str) -> bool {
        let (start, end) = self.selection();
        let inserted_len = replacement.chars().count();
        let new_cursor = start + inserted_len;
        self.apply_edit(
            start,
            end - start,
            replacement,
            EditKind::Other,
            new_cursor,
            new_cursor,
        )
    }

    pub fn insert_text(&mut self, inserted: &str) -> bool {
        let (start, end) = self.selection();
        let new_cursor = start + inserted.chars().count();
        self.apply_edit(
            start,
            end - start,
            inserted,
            EditKind::Typing,
            new_cursor,
            new_cursor,
        )
    }

    /// Replace the whole buffer while keeping the undo history useful.
    pub fn replace_all_text(&mut self, new_text: &str, kind: EditKind) -> bool {
        if new_text == self.text {
            return false;
        }
        let cursor = self.cursor.min(text::char_count(new_text));
        let anchor = self.anchor.min(text::char_count(new_text));
        self.apply_edit(0, self.char_count(), new_text, kind, cursor, anchor)
    }
}

/// Compute the minimal character-level replacement turning `old` into `new`.
fn diff_chars(old: &str, new: &str) -> (usize, String, String) {
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
    let start = text::byte_to_char(old, prefix);
    let removed = old[prefix..old.len() - suffix].to_owned();
    let inserted = new[prefix..new.len() - suffix].to_owned();
    (start, removed, inserted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(name: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn new_document_is_clean() {
        let doc = Document::new_untitled("new 1");
        assert!(!doc.is_modified());
        assert_eq!(doc.line_count(), 1);
        assert_eq!(doc.display_name(), "new 1");
        assert!(doc.is_untitled());
    }

    #[test]
    fn applies_edit_and_tracks_dirty_state() {
        let mut doc = Document::new_untitled("new 1");
        assert!(doc.apply_edit(0, 0, "hello", EditKind::Typing, 5, 5));
        assert_eq!(doc.text, "hello");
        assert_eq!(doc.cursor, 5);
        assert!(doc.is_modified());
        assert!(doc.undo());
        assert_eq!(doc.text, "");
        assert_eq!(doc.cursor, 0);
        assert!(doc.redo());
        assert_eq!(doc.text, "hello");
        assert_eq!(doc.cursor, 5);
        assert!(doc.is_modified());
    }

    #[test]
    fn diff_reconciles_widget_edits() {
        let mut doc = Document::from_text("hello world", TextEncoding::Utf8, Eol::Lf, "Plain Text");
        doc.set_selection(11, 11);
        assert!(doc.set_text_from_widget("hello brave world", 17, 17));
        assert_eq!(doc.text, "hello brave world");
        assert!(doc.undo());
        assert_eq!(doc.text, "hello world");
        assert!(doc.redo());
        assert_eq!(doc.text, "hello brave world");
    }

    #[test]
    fn handles_unicode_diff() {
        let mut doc = Document::from_text("aé世z", TextEncoding::Utf8, Eol::Lf, "Plain Text");
        doc.set_selection(3, 3);
        assert!(doc.set_text_from_widget("aé世界z", 4, 4));
        assert_eq!(doc.text, "aé世界z");
        assert!(doc.undo());
        assert_eq!(doc.text, "aé世z");
    }

    #[test]
    fn merges_consecutive_typing() {
        let mut doc = Document::new_untitled("new 1");
        doc.set_text_from_widget("h", 1, 1);
        doc.set_text_from_widget("he", 2, 2);
        doc.set_text_from_widget("hel", 3, 3);
        assert_eq!(doc.undo.undo_len(), 1);
        assert!(doc.undo());
        assert_eq!(doc.text, "");
    }

    #[test]
    fn saves_and_reopens_utf8_bom_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        let mut doc = Document::new_untitled("new 1");
        doc.text = "héllo\nworld\n".into();
        doc.encoding = TextEncoding::Utf8Bom;
        doc.convert_eol(Eol::CrLf);
        doc.mark_saved();
        doc.save_as(&path).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.starts_with(&[0xEF, 0xBB, 0xBF]));
        assert!(bytes.windows(2).any(|w| w == b"\r\n"));

        let reopened = Document::open(&path).unwrap();
        assert_eq!(reopened.encoding, TextEncoding::Utf8Bom);
        assert_eq!(reopened.eol, Eol::CrLf);
        // Internally the editor always works with LF.
        assert_eq!(reopened.text, "héllo\nworld\n");
        assert!(!reopened.is_modified());
    }

    #[test]
    fn saves_utf16_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf16.txt");
        let mut doc =
            Document::from_text("hello 世界", TextEncoding::Utf16Le, Eol::Lf, "Plain Text");
        doc.save_as(&path).unwrap();
        let reopened = Document::open(&path).unwrap();
        assert_eq!(reopened.encoding, TextEncoding::Utf16Le);
        assert_eq!(reopened.text, "hello 世界");
    }

    #[test]
    fn rejects_binary_files() {
        let (_dir, path) = temp_file("bin.dat", &[0, 1, 2, 3, 0, 5]);
        let err = Document::open(&path).unwrap_err();
        assert!(matches!(err, Error::BinaryFile(_)));
    }

    #[test]
    fn rejects_oversized_files() {
        let (_dir, path) = temp_file("big.txt", b"1234567890");
        let err = Document::open_with_limit(&path, 5).unwrap_err();
        assert!(matches!(err, Error::FileTooLarge { .. }));
    }

    #[test]
    fn converts_eol_without_touching_the_editing_buffer() {
        let mut doc = Document::from_text("a\nb\nc", TextEncoding::Utf8, Eol::Lf, "Plain Text");
        assert!(doc.convert_eol(Eol::CrLf));
        assert_eq!(doc.text, "a\nb\nc");
        assert_eq!(doc.eol, Eol::CrLf);
        assert!(doc.is_modified());
        assert!(!doc.convert_eol(Eol::CrLf));
    }

    #[test]
    fn reload_reads_disk_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reload.txt");
        fs::write(&path, "first").unwrap();
        let mut doc = Document::open(&path).unwrap();
        fs::write(&path, "second").unwrap();
        assert!(doc.changed_on_disk());
        doc.reload().unwrap();
        assert_eq!(doc.text, "second");
        assert!(!doc.is_modified());
    }

    #[test]
    fn selection_and_line_column() {
        let mut doc =
            Document::from_text("one\ntwo\nthree", TextEncoding::Utf8, Eol::Lf, "Plain Text");
        doc.set_selection(7, 4);
        assert_eq!(doc.selection(), (4, 7));
        assert_eq!(doc.selected_text(), "two");
        assert_eq!(doc.line_col(), (2, 1));
        doc.select_all();
        assert_eq!(doc.selected_text(), "one\ntwo\nthree");
    }
}

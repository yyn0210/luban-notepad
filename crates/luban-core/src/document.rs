//! The editor document: text, file metadata, selection and undo history.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Instant, SystemTime};

use crate::atomic_write::AtomicFile;
use crate::disk_engine::{DiskEngine, WindowText};
use crate::encoding::{self, TextEncoding};
use crate::eol::{self, Eol};
use crate::error::{Error, Result};
use crate::language;
use crate::text::{self, LineIndex};
use crate::undo::{Edit, EditKind, UndoStack};

/// Files up to this size are opened wholly and edited in memory. Larger text
/// files go through the disk engine instead of being rejected; the GUI still
/// lets users lower or raise the limit in Preferences.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 1024 * 1024 * 1024;

/// A document opened from a byte range rather than the whole file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartialRange {
    /// First byte of the loaded range.
    pub start: u64,
    /// Exclusive end byte of the loaded range.
    pub end: u64,
    /// Size of the complete source file.
    pub total: u64,
}

/// Relationship between the in-memory document and the file on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskStatus {
    /// The file exists with the size and modification time recorded at
    /// open/save time.
    Unchanged,
    /// The file exists but its size or modification time differs.
    Changed,
    /// The file no longer exists (deleted or renamed).
    Missing,
    /// The document has never been saved to a path.
    Untracked,
}

/// State of a document backed by the disk engine.
///
/// The editor only holds a bounded window; `cursor` and `anchor` are global
/// logical byte offsets, not character indices.
#[derive(Debug)]
pub struct HugeFileState {
    pub engine: Arc<RwLock<DiskEngine>>,
    pub cursor: u64,
    pub anchor: u64,
    /// 1-based global line at the top of the viewport (used by the session).
    pub scroll_line: u64,
    /// 1-based global line and column of the caret. Kept separately from the
    /// editor window so scrolling the window does not lose an off-screen
    /// caret.
    pub cursor_line: u64,
    pub cursor_column: u64,
    /// First window prepared by [`Document::open_huge`], consumed when the
    /// editor is mounted.
    pub initial_window: Option<WindowText>,
    /// Lock-free copies of engine facts for status rendering and prompts.
    /// They are refreshed after every edit, save and reload so the UI never
    /// blocks on a long-running background operation.
    pub total_chars: AtomicU64,
    pub total_lines: AtomicU64,
    pub modified: AtomicBool,
    pub revision: AtomicU64,
}

impl HugeFileState {
    pub fn read(&self) -> RwLockReadGuard<'_, DiskEngine> {
        self.engine
            .read()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, DiskEngine> {
        self.engine
            .write()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Copy engine facts into the lock-free cache. Call while holding a guard.
    pub fn refresh_metadata(&self, engine: &DiskEngine) {
        self.total_chars
            .store(engine.total_chars(), Ordering::Relaxed);
        self.total_lines
            .store(engine.total_lines(), Ordering::Relaxed);
        self.modified.store(engine.is_modified(), Ordering::Relaxed);
        self.revision.store(engine.revision(), Ordering::Relaxed);
    }
}

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
    /// The file was opened although its content looks binary. Its buffer keeps
    /// the original bytes (via a reversible decoding) and is written back
    /// without line-ending conversion.
    non_text: bool,
    /// Set when only a portion of the source file is loaded. Such documents
    /// can be edited but never saved over the source path.
    partial: Option<PartialRange>,
    revision: u64,
    saved_revision: u64,
    /// The text buffer lives only in the editor while this is set. Operations
    /// that need the full text must materialize it first with
    /// [`Document::attach_text`].
    text_detached: bool,
    cached_chars: Option<usize>,
    /// Set when the document is backed by the disk engine. `text` stays empty.
    huge: Option<HugeFileState>,
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
            non_text: false,
            partial: None,
            revision: 0,
            saved_revision: 0,
            text_detached: false,
            cached_chars: Some(0),
            huge: None,
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
        doc.cached_chars = Some(text::char_count(&text));
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
        Self::from_file_bytes(
            path,
            bytes,
            metadata.len(),
            metadata.modified().ok(),
            metadata.permissions().readonly(),
        )
    }

    /// Open a whole file while reporting read progress and honoring a cancel
    /// flag. Used by the large-file controller for files within the size limit.
    pub fn open_with_progress(
        path: impl AsRef<Path>,
        max_size: u64,
        cancel: &std::sync::atomic::AtomicBool,
        progress: &std::sync::atomic::AtomicU64,
    ) -> Result<Self> {
        let path = path.as_ref();
        let metadata = fs::metadata(path)?;
        if metadata.is_dir() {
            return Err(Error::InvalidPath(path.to_path_buf()));
        }
        let bytes = crate::large_file::read_whole(path, max_size, cancel, progress)?;
        Self::from_file_bytes(
            path,
            bytes,
            metadata.len(),
            metadata.modified().ok(),
            metadata.permissions().readonly(),
        )
    }

    /// Open a text file through the disk engine. The file is indexed on the
    /// calling thread (use a background executor) and the editor receives the
    /// first window around `target_line`.
    pub fn open_huge(
        path: impl AsRef<Path>,
        target_line: u64,
        cancel: &std::sync::atomic::AtomicBool,
        progress: &std::sync::atomic::AtomicU64,
    ) -> Result<Self> {
        let path = path.as_ref();
        let engine = DiskEngine::open(path, cancel, progress)?;
        let total_chars = engine.total_chars();
        let mut doc = Self::new_untitled("new 1");
        doc.path = Some(path.to_path_buf());
        doc.encoding = engine.encoding();
        doc.eol = engine.eol();
        doc.language = crate::large_file::language_for(path);
        doc.file_size = engine.source_size();
        doc.last_modified = engine.last_modified();
        doc.read_only = engine.is_read_only();
        doc.cached_chars = Some(usize::try_from(total_chars).unwrap_or(usize::MAX));
        doc.text_detached = true;
        let line = target_line.max(1).min(engine.total_lines());
        let offset = engine.offset_for_line(line, 1).unwrap_or(0);
        let window = engine.read_window(offset, crate::disk_engine::WINDOW_BYTES)?;
        let cursor = window.start;
        let scroll_line = window.line;
        let huge = HugeFileState {
            engine: Arc::new(RwLock::new(engine)),
            cursor,
            anchor: cursor,
            scroll_line,
            cursor_line: line,
            cursor_column: 1,
            initial_window: Some(window),
            total_chars: AtomicU64::new(total_chars),
            total_lines: AtomicU64::new(0),
            modified: AtomicBool::new(false),
            revision: AtomicU64::new(1),
        };
        {
            let engine = huge.read();
            huge.refresh_metadata(&engine);
        }
        doc.huge = Some(huge);
        doc.revision = 1;
        doc.saved_revision = 1;
        Ok(doc)
    }

    /// Build a document from an engine restored out of recovery data.
    ///
    /// The engine is already marked modified; the caret and top line come from
    /// the recovery checkpoint so the tab looks like it did before the crash.
    pub fn from_recovered_engine(mut engine: DiskEngine) -> Result<Self> {
        let (cursor, scroll_line) = engine.recovery_position();
        let total_chars = engine.total_chars();
        let total_lines = engine.total_lines();
        let mut doc = Self::new_untitled("new 1");
        doc.path = Some(engine.path().to_path_buf());
        doc.encoding = engine.encoding();
        doc.eol = engine.eol();
        doc.language = crate::large_file::language_for(engine.path());
        doc.file_size = engine.source_size();
        doc.last_modified = engine.last_modified();
        doc.read_only = engine.is_read_only();
        doc.cached_chars = Some(usize::try_from(total_chars).unwrap_or(usize::MAX));
        doc.text_detached = true;
        if let Some(range) = engine.range() {
            doc.partial = Some(PartialRange {
                start: range.start,
                end: range.end,
                total: engine.source_size(),
            });
        }
        let line = scroll_line.clamp(1, total_lines.max(1));
        let window = match engine.read_window_from_line(line, crate::disk_engine::WINDOW_BYTES) {
            Ok(window) => window,
            Err(error) => {
                // The rebuilt engine cannot be mounted, but the recovery data
                // may still be the only copy of the edits.
                engine.preserve_recovery();
                return Err(error);
            }
        };
        let cursor = if cursor >= window.start && cursor <= window.start.saturating_add(window.len)
        {
            cursor
        } else {
            window.start
        };
        let (cursor_line, cursor_column) = {
            let offset = cursor.min(window.start.saturating_add(window.len));
            let (line, column) = engine
                .line_col_at(offset)
                .unwrap_or((window.line, window.start_column));
            (line, column)
        };
        let huge = HugeFileState {
            engine: Arc::new(RwLock::new(engine)),
            cursor,
            anchor: cursor,
            scroll_line: window.line,
            cursor_line,
            cursor_column,
            initial_window: Some(window),
            total_chars: AtomicU64::new(total_chars),
            total_lines: AtomicU64::new(total_lines),
            modified: AtomicBool::new(true),
            revision: AtomicU64::new(1),
        };
        {
            let engine = huge.read();
            huge.refresh_metadata(&engine);
        }
        doc.huge = Some(huge);
        doc.revision = 1;
        doc.saved_revision = 0;
        Ok(doc)
    }

    /// Open a line-aligned byte range of a text file through the disk engine.
    ///
    /// Only the selected range is indexed. The editor mounts windows inside it
    /// and line numbers are local to the range. The document is marked partial
    /// so it can only be saved as a new file.
    pub fn open_huge_range(
        path: impl AsRef<Path>,
        from: u64,
        to: u64,
        target_line: u64,
        cancel: &std::sync::atomic::AtomicBool,
        progress: &std::sync::atomic::AtomicU64,
    ) -> Result<Self> {
        let path = path.as_ref();
        let engine = DiskEngine::open_range(path, from, to, cancel, progress)?;
        let range = engine.range().ok_or_else(|| Error::InvalidRange {
            start: from,
            end: to,
            size: engine.source_size(),
        })?;
        let total_chars = engine.total_chars();
        let source_size = engine.source_size();
        let mut doc = Self::new_untitled("new 1");
        doc.path = Some(path.to_path_buf());
        doc.encoding = engine.encoding();
        doc.eol = engine.eol();
        doc.language = crate::large_file::language_for(path);
        doc.file_size = source_size;
        doc.last_modified = engine.last_modified();
        // The source is protected by `partial`; the range stays editable and
        // save-as writes only the selected bytes.
        doc.read_only = false;
        doc.cached_chars = Some(usize::try_from(total_chars).unwrap_or(usize::MAX));
        doc.text_detached = true;
        doc.partial = Some(PartialRange {
            start: range.start,
            end: range.end,
            total: source_size,
        });
        let line = target_line.max(1).min(engine.total_lines());
        let offset = engine.offset_for_line(line, 1).unwrap_or(0);
        let window = engine.read_window(offset, crate::disk_engine::WINDOW_BYTES)?;
        let cursor = window.start;
        let scroll_line = window.line;
        let huge = HugeFileState {
            engine: Arc::new(RwLock::new(engine)),
            cursor,
            anchor: cursor,
            scroll_line,
            cursor_line: line,
            cursor_column: 1,
            initial_window: Some(window),
            total_chars: AtomicU64::new(total_chars),
            total_lines: AtomicU64::new(0),
            modified: AtomicBool::new(false),
            revision: AtomicU64::new(1),
        };
        {
            let engine = huge.read();
            huge.refresh_metadata(&engine);
        }
        doc.huge = Some(huge);
        doc.revision = 1;
        doc.saved_revision = 1;
        Ok(doc)
    }

    /// Open only `from..to` of a file. The range is aligned to line boundaries
    /// by [`crate::large_file::read_range`]. The resulting document is marked
    /// partial and cannot be saved over its source path.
    pub fn open_range(
        path: impl AsRef<Path>,
        from: u64,
        to: u64,
        max_len: u64,
        cancel: &std::sync::atomic::AtomicBool,
        progress: &std::sync::atomic::AtomicU64,
    ) -> Result<Self> {
        let path = path.as_ref();
        let content = crate::large_file::read_range(path, from, to, max_len, cancel, progress)?;
        let mut doc = Self::new_untitled("new 1");
        doc.path = Some(path.to_path_buf());
        doc.text = content.text;
        doc.cached_chars = Some(text::char_count(&doc.text));
        doc.encoding = content.encoding;
        doc.eol = content.eol;
        doc.language = crate::large_file::language_for(path);
        doc.file_size = content.total_size;
        doc.last_modified = content.last_modified;
        // The source is protected by `partial` rather than the permission
        // flag, so a read-only source can still be saved as a new file.
        doc.read_only = false;
        doc.non_text = content.non_text;
        doc.partial = Some(PartialRange {
            start: content.start,
            end: content.end,
            total: content.total_size,
        });
        doc.revision = 1;
        doc.saved_revision = 1;
        Ok(doc)
    }

    fn from_file_bytes(
        path: &Path,
        bytes: Vec<u8>,
        file_size: u64,
        last_modified: Option<SystemTime>,
        read_only: bool,
    ) -> Result<Self> {
        // Notepad++ opens binary content as text instead of refusing it. The
        // flag changes how the buffer is decoded and saved, not whether the
        // file can be opened.
        let non_text = encoding::looks_binary(&bytes);
        let decoded = if non_text {
            encoding::decode_lossless(bytes)
        } else {
            encoding::decode_owned(bytes)
        };
        let detected_eol = eol::detect(&decoded.text);
        // The editing buffer is always LF; the file's convention lives in
        // `doc.eol` and is restored when saving. Non-text buffers keep the
        // original line endings so that saving does not rewrite bytes.
        let text = if non_text {
            decoded.text
        } else {
            eol::to_lf_owned(decoded.text)
        };
        let path_language = language::detect_from_path(path);
        let language_name = if path_language == language::PLAIN_TEXT {
            language::detect_from_content(&text).unwrap_or(path_language)
        } else {
            path_language
        };
        let mut doc = Self::new_untitled("new 1");
        doc.path = Some(path.to_path_buf());
        doc.text = text;
        doc.cached_chars = Some(text::char_count(&doc.text));
        doc.encoding = decoded.encoding;
        doc.eol = detected_eol;
        doc.language = language_name.to_string();
        doc.file_size = file_size;
        doc.last_modified = last_modified;
        doc.read_only = read_only;
        doc.non_text = non_text;
        doc.revision = 1;
        doc.saved_revision = 1;
        Ok(doc)
    }

    /// Create an untitled document containing text (e.g. from a template).
    pub fn with_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let detected_eol = eol::detect(&text);
        let mut doc = Self::new_untitled("new 1");
        doc.text = eol::to_lf_owned(text);
        doc.cached_chars = Some(text::char_count(&doc.text));
        doc.eol = detected_eol;
        doc.revision = 1;
        doc.saved_revision = 0; // unsaved content
        doc
    }

    /// Whether the document's buffer currently lives only in the editor.
    pub fn is_text_detached(&self) -> bool {
        self.text_detached
    }

    pub fn is_huge(&self) -> bool {
        self.huge.is_some()
    }

    pub fn huge(&self) -> Option<&HugeFileState> {
        self.huge.as_ref()
    }

    pub fn huge_mut(&mut self) -> Option<&mut HugeFileState> {
        self.huge.as_mut()
    }

    /// Take the window prepared by [`Document::open_huge`].
    pub fn take_initial_window(&mut self) -> Option<WindowText> {
        self.huge
            .as_mut()
            .and_then(|huge| huge.initial_window.take())
    }

    /// Whether this document was opened from content that looks binary. Such
    /// documents keep their original line endings and are saved without
    /// re-encoding untouched bytes.
    pub fn is_non_text(&self) -> bool {
        self.non_text
    }

    /// Mark a recreated buffer as non-text so saving keeps its raw bytes.
    pub fn mark_non_text(&mut self) {
        self.non_text = true;
    }

    /// The byte range when only a portion of the source file is loaded.
    pub fn partial_range(&self) -> Option<PartialRange> {
        self.partial
    }

    /// Replace the partial-range marker after a save-as or reload changed the
    /// document's backing file.
    pub fn set_partial_range(&mut self, range: Option<PartialRange>) {
        self.partial = range;
    }

    pub fn is_partial(&self) -> bool {
        self.partial.is_some()
    }

    /// Size of the complete source file, including unopened bytes.
    pub fn source_size(&self) -> u64 {
        if self.huge.is_some() {
            return self.file_size;
        }
        self.partial
            .map(|range| range.total)
            .unwrap_or(self.file_size)
    }

    /// Move the buffer out for the editor and release the document's copy.
    ///
    /// Returns the text and its character count. The document keeps the count
    /// so selection and status queries stay O(1) until the text is attached
    /// again.
    pub fn detach_text(&mut self) -> (String, usize) {
        let count = self.char_count();
        self.text_detached = true;
        if self.huge.is_some() {
            // The disk engine owns the text; there is no document snapshot.
            return (String::new(), count);
        }
        (std::mem::take(&mut self.text), count)
    }

    /// Install a materialized snapshot, computing the character count.
    pub fn attach_text(&mut self, text: String) {
        let count = text::char_count(&text);
        self.attach_text_counted(text, count);
    }

    /// Install a materialized snapshot with a known character count.
    pub fn attach_text_counted(&mut self, text: String, char_count: usize) {
        if self.huge.is_some() {
            // Never materialize a disk-engine document into `text`.
            return;
        }
        self.text = text;
        self.cached_chars = Some(char_count);
        self.text_detached = false;
    }

    /// Compute and cache the character count without other work.
    pub fn prime_char_count(&mut self) -> usize {
        if self.cached_chars.is_none() {
            self.cached_chars = Some(text::char_count(&self.text));
        }
        self.cached_chars.unwrap_or(0)
    }

    /// Record a whole-buffer replacement that was applied directly in the
    /// editor, without materializing a second copy in the document.
    pub fn record_external_replacement(&mut self, char_count: usize, anchor: usize, cursor: usize) {
        self.text.clear();
        self.text_detached = true;
        self.cached_chars = Some(char_count);
        self.anchor = anchor.min(char_count);
        self.cursor = cursor.min(char_count);
        self.revision = self.revision.wrapping_add(1);
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
        if let Some(huge) = &self.huge {
            return huge.modified.load(Ordering::Relaxed);
        }
        self.revision != self.saved_revision
    }

    pub fn revision(&self) -> u64 {
        if let Some(huge) = &self.huge {
            return huge.revision.load(Ordering::Relaxed);
        }
        self.revision
    }

    pub fn mark_saved(&mut self) {
        if self.huge.is_some() {
            // The engine rebases itself after a save.
            return;
        }
        self.saved_revision = self.revision;
    }

    /// Mark the document dirty after a metadata-only change.
    pub fn touch(&mut self) {
        if self.huge.is_some() {
            return;
        }
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn line_count(&self) -> usize {
        if let Some(huge) = &self.huge {
            return usize::try_from(huge.total_lines.load(Ordering::Relaxed)).unwrap_or(usize::MAX);
        }
        LineIndex::new(&self.text).len()
    }

    pub fn line_col(&self) -> (usize, usize) {
        if let Some(huge) = &self.huge {
            let (line, column) = huge
                .engine
                .try_read()
                .map(|engine| engine.line_col_at(huge.cursor).unwrap_or((1, 1)))
                .unwrap_or((1, 1));
            return (
                usize::try_from(line).unwrap_or(usize::MAX),
                usize::try_from(column).unwrap_or(usize::MAX),
            );
        }
        LineIndex::new(&self.text).line_col(self.cursor)
    }

    pub fn selection(&self) -> (usize, usize) {
        if let Some(huge) = &self.huge {
            let (anchor, cursor) = (huge.anchor, huge.cursor);
            let (a, b) = if anchor <= cursor {
                (anchor, cursor)
            } else {
                (cursor, anchor)
            };
            return (
                usize::try_from(a).unwrap_or(usize::MAX),
                usize::try_from(b).unwrap_or(usize::MAX),
            );
        }
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    pub fn has_selection(&self) -> bool {
        if let Some(huge) = &self.huge {
            return huge.anchor != huge.cursor;
        }
        self.anchor != self.cursor
    }

    pub fn selected_text(&self) -> String {
        if self.huge.is_some() {
            // Callers use the editor's rope for huge documents.
            return String::new();
        }
        let (a, b) = self.selection();
        text::slice_chars(&self.text, a, b)
    }

    pub fn set_selection(&mut self, anchor: usize, cursor: usize) {
        if let Some(huge) = &mut self.huge {
            let total = huge.read().total_len();
            let anchor = u64::try_from(anchor).unwrap_or(u64::MAX).min(total);
            let cursor = u64::try_from(cursor).unwrap_or(u64::MAX).min(total);
            huge.anchor = anchor;
            huge.cursor = cursor;
            return;
        }
        let total = self.char_count();
        self.anchor = anchor.min(total);
        self.cursor = cursor.min(total);
    }

    pub fn select_all(&mut self) {
        if let Some(huge) = &mut self.huge {
            let total = huge.read().total_len();
            huge.anchor = 0;
            huge.cursor = total;
            return;
        }
        let total = self.char_count();
        self.anchor = 0;
        self.cursor = total;
    }

    pub fn char_count(&self) -> usize {
        if let Some(huge) = &self.huge {
            return usize::try_from(huge.total_chars.load(Ordering::Relaxed)).unwrap_or(usize::MAX);
        }
        match self.cached_chars {
            Some(count) => count,
            None => text::char_count(&self.text),
        }
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
        if self.text_detached {
            return false;
        }
        let total = self.char_count();
        if start > total {
            return false;
        }
        let removed_chars = removed_chars.min(total - start);
        if removed_chars == 0 && inserted.is_empty() {
            return false;
        }
        let selection_before = (self.anchor, self.cursor);

        let b0 = text::char_to_byte(&self.text, start);
        let b1 = text::char_to_byte(&self.text, start + removed_chars);
        let removed = self.text[b0..b1].to_owned();
        let inserted_chars = inserted.chars().count();
        let new_count = total - removed_chars + inserted_chars;
        let mut new_text = String::with_capacity(self.text.len() + inserted.len() - (b1 - b0));
        new_text.push_str(&self.text[..b0]);
        new_text.push_str(inserted);
        new_text.push_str(&self.text[b1..]);

        let cursor_after = cursor_after.min(new_count);
        let anchor_after = anchor_after.min(new_count);
        let edit = Edit {
            start,
            removed,
            inserted: inserted.to_owned(),
            cursor_before: self.cursor,
            cursor_after,
            selection_before: Some(selection_before),
            selection_after: Some((anchor_after, cursor_after)),
            kind,
            at: Instant::now(),
        };
        self.text = new_text;
        self.cached_chars = Some(new_count);
        self.cursor = cursor_after;
        self.anchor = anchor_after;
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
        if self.text_detached {
            return false;
        }
        if new_text == self.text {
            let total = self.char_count();
            self.cursor = cursor_after.min(total);
            self.anchor = anchor_after.min(total);
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
        if self.text_detached {
            return false;
        }
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
        let b0 = text::char_to_byte(&self.text, edit.start);
        let b1 = text::char_to_byte(&self.text, edit.start + remove_len);
        let expected = if undo {
            edit.inserted.as_str()
        } else {
            edit.removed.as_str()
        };
        if &self.text[b0..b1] != expected {
            return false;
        }
        let inserted_chars = insert_text.chars().count();
        let new_count = total - remove_len + inserted_chars;
        let mut new_text = String::with_capacity(self.text.len() + insert_text.len());
        new_text.push_str(&self.text[..b0]);
        new_text.push_str(insert_text);
        new_text.push_str(&self.text[b1..]);
        self.text = new_text;
        self.cached_chars = Some(new_count);
        self.revision = self.revision.wrapping_add(1);
        if let Some((anchor, cursor)) = selection {
            self.anchor = anchor.min(new_count);
            self.cursor = cursor.min(new_count);
        }
        true
    }

    /// Change the line-ending convention used when saving.
    ///
    /// Editing always uses LF internally, so this is a metadata change and is
    /// intentionally not part of the text undo history. It does mark the
    /// document dirty, because the bytes written to disk will change.
    pub fn convert_eol(&mut self, new_eol: Eol) -> bool {
        // Converting line endings would rewrite arbitrary bytes in a non-text
        // file and break the byte-preserving save guarantee. Disk-engine
        // documents keep the encoding they were opened with.
        if self.huge.is_some() || self.non_text || self.eol == new_eol {
            return false;
        }
        self.eol = new_eol;
        self.touch();
        true
    }

    pub fn set_encoding(&mut self, new_encoding: TextEncoding) -> bool {
        if self.huge.is_some() || self.encoding == new_encoding {
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
            let path_language = language::detect_from_path(path);
            let detected = if path_language == language::PLAIN_TEXT {
                language::detect_from_content(&self.text).unwrap_or(path_language)
            } else {
                path_language
            };
            self.language = detected.to_string();
        }
    }

    /// Save to the current path. Returns `Ok(true)` when the selected encoding
    /// could not represent every character (the file was still written).
    pub fn save(&mut self) -> Result<bool> {
        if self.huge.is_some() {
            return Err(Error::Other(
                crate::i18n::tr("core.document.huge_save_flow").to_string(),
            ));
        }
        if self.partial.is_some() {
            return Err(Error::Other(crate::i18n::tr(
                "core.document.partial_save_as",
            )));
        }
        let path = self
            .path
            .clone()
            .ok_or_else(|| Error::Other(crate::i18n::tr("core.document.no_path")))?;
        self.save_to_path(&path)
    }

    pub fn save_as(&mut self, path: impl AsRef<Path>) -> Result<bool> {
        if self.huge.is_some() {
            return Err(Error::Other(
                crate::i18n::tr("core.document.huge_save_flow").to_string(),
            ));
        }
        let path = path.as_ref().to_path_buf();
        if self.partial.is_some()
            && self
                .path
                .as_ref()
                .is_some_and(|source| same_document_path(source, &path))
        {
            return Err(Error::Other(crate::i18n::tr(
                "core.document.partial_same_path",
            )));
        }
        let lossy = self.save_to_path(&path)?;
        let was_plain = self.language == language::PLAIN_TEXT;
        self.path = Some(path);
        self.partial = None;
        if was_plain {
            self.auto_detect_language();
        }
        Ok(lossy)
    }

    fn save_to_path(&mut self, path: &Path) -> Result<bool> {
        if self.read_only {
            return Err(Error::Other(crate::i18n::tr("core.document.read_only")));
        }
        if self.text_detached {
            return Err(Error::Other("document text is not materialized".into()));
        }
        let mut atomic = AtomicFile::create(path)?;
        let lossy = if self.non_text {
            // Non-text buffers were decoded reversibly and keep their original
            // line endings; write them back without normalization so untouched
            // bytes stay identical.
            let (bytes, lossy) = encoding::encode(&self.text, self.encoding);
            atomic.write(&bytes)?;
            lossy
        } else if matches!(self.encoding, TextEncoding::Utf8 | TextEncoding::Utf8Bom)
            && self.eol == Eol::Lf
            && !self.text.as_bytes().contains(&b'\r')
        {
            // Fast path: LF UTF-8 is already the on-disk representation.
            atomic.write(self.encoding.bom())?;
            atomic.write(self.text.as_bytes())?;
            false
        } else {
            let with_eol = eol::normalize(&self.text, self.eol);
            let (bytes, lossy) =
                if matches!(self.encoding, TextEncoding::Utf8 | TextEncoding::Utf8Bom) {
                    let mut bytes = Vec::with_capacity(self.encoding.bom().len() + with_eol.len());
                    bytes.extend_from_slice(self.encoding.bom());
                    bytes.extend_from_slice(with_eol.as_bytes());
                    (bytes, false)
                } else {
                    encoding::encode(&with_eol, self.encoding)
                };
            atomic.write(&bytes)?;
            lossy
        };
        atomic.commit()?;
        let metadata = fs::metadata(path)?;
        self.file_size = metadata.len();
        self.last_modified = metadata.modified().ok();
        self.prime_char_count();
        self.mark_saved();
        Ok(lossy)
    }

    /// Reload the file from disk, discarding the undo history.
    pub fn reload(&mut self) -> Result<()> {
        if self.huge.is_some() {
            return Err(Error::Other(
                crate::i18n::tr("core.document.huge_save_flow").to_string(),
            ));
        }
        if self.partial.is_some() {
            return Err(Error::Other(crate::i18n::tr(
                "core.document.partial_reload",
            )));
        }
        let path = self
            .path
            .clone()
            .ok_or_else(|| Error::Other(crate::i18n::tr("core.document.no_path")))?;
        let replacement = Self::open(&path)?;
        self.text = replacement.text;
        self.cached_chars = replacement.cached_chars;
        self.text_detached = false;
        self.encoding = replacement.encoding;
        self.eol = replacement.eol;
        self.last_modified = replacement.last_modified;
        self.file_size = replacement.file_size;
        self.read_only = replacement.read_only;
        self.non_text = replacement.non_text;
        self.undo.clear();
        self.cursor = self.cursor.min(self.char_count());
        self.anchor = self.anchor.min(self.char_count());
        self.revision = self.revision.wrapping_add(1);
        self.saved_revision = self.revision;
        Ok(())
    }

    /// Whether the file on disk appears to have changed since opening/saving.
    pub fn changed_on_disk(&self) -> bool {
        matches!(
            self.disk_status(),
            DiskStatus::Changed | DiskStatus::Missing
        )
    }

    /// Compare the recorded file identity with the file currently on disk.
    pub fn disk_status(&self) -> DiskStatus {
        if let Some(huge) = &self.huge {
            return huge
                .engine
                .try_read()
                .map(|engine| engine.disk_status())
                .unwrap_or(DiskStatus::Unchanged);
        }
        let Some(path) = &self.path else {
            return DiskStatus::Untracked;
        };
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return DiskStatus::Missing;
            }
            // An unreadable file is treated as changed so a save never
            // silently overwrites something we could not inspect.
            Err(_) => return DiskStatus::Changed,
        };
        let modified_changed = match (metadata.modified().ok(), self.last_modified) {
            (Some(current), Some(known)) => current != known,
            _ => false,
        };
        if modified_changed || metadata.len() != self.file_size {
            DiskStatus::Changed
        } else {
            DiskStatus::Unchanged
        }
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
        if self.text_detached || new_text == self.text {
            return false;
        }
        let count = text::char_count(new_text);
        let cursor = self.cursor.min(count);
        let anchor = self.anchor.min(count);
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

fn same_document_path(a: &Path, b: &Path) -> bool {
    // Resolve `.`/`..` and symlinks when both paths exist, so a Save As target
    // that aliases the source cannot truncate it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) {
            if ma.dev() == mb.dev() && ma.ino() == mb.ino() {
                return true;
            }
        }
    }
    if let (Ok(a), Ok(b)) = (fs::canonicalize(a), fs::canonicalize(b)) {
        return if cfg!(windows) {
            a.to_string_lossy()
                .eq_ignore_ascii_case(&b.to_string_lossy())
        } else {
            a == b
        };
    }
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
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
    fn opens_binary_files_losslessly() {
        let original = vec![0x00, 0x01, 0xE9, 0xFF, b'\r', b'\n', 0x00, 0x7F];
        let (_dir, path) = temp_file("bin.dat", &original);
        let mut doc = Document::open(&path).unwrap();
        assert!(doc.is_non_text());
        assert_eq!(doc.encoding, TextEncoding::Windows1252);
        assert_eq!(doc.eol, Eol::CrLf);
        doc.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!doc.is_modified());
    }

    #[test]
    fn opens_utf8_non_text_files_losslessly() {
        let original = b"ab\0cd\xE4\xB8\x96\n".to_vec();
        let (_dir, path) = temp_file("utf8-nul.txt", &original);
        let mut doc = Document::open(&path).unwrap();
        assert!(doc.is_non_text());
        assert_eq!(doc.encoding, TextEncoding::Utf8);
        doc.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn non_text_edits_only_change_the_edited_bytes() {
        let original = vec![0x00, 0x01, 0x02, 0x03, 0x00];
        let (_dir, path) = temp_file("edit.dat", &original);
        let mut doc = Document::open(&path).unwrap();
        assert!(doc.apply_edit(1, 1, "X", EditKind::Typing, 2, 2));
        doc.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), vec![0x00, b'X', 0x02, 0x03, 0x00]);
    }

    #[test]
    fn non_text_documents_reject_eol_conversion() {
        let (_dir, path) = temp_file("eol.dat", &[0, 1, b'\n', 2]);
        let mut doc = Document::open(&path).unwrap();
        assert!(doc.is_non_text());
        assert!(!doc.convert_eol(Eol::CrLf));
        assert!(!doc.is_modified());
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
    fn disk_status_tracks_modification_and_deletion() {
        let (_dir, path) = temp_file("status.txt", b"initial");
        let doc = Document::open(&path).unwrap();
        assert_eq!(doc.disk_status(), DiskStatus::Unchanged);
        assert!(!doc.changed_on_disk());

        fs::write(&path, b"external edit").unwrap();
        assert_eq!(doc.disk_status(), DiskStatus::Changed);
        assert!(doc.changed_on_disk());

        fs::remove_file(&path).unwrap();
        assert_eq!(doc.disk_status(), DiskStatus::Missing);
        assert!(doc.changed_on_disk());
    }

    #[test]
    fn saving_replaces_the_file_atomically_and_refreshes_metadata() {
        let (_dir, path) = temp_file("save.txt", b"old content");
        let mut doc = Document::open(&path).unwrap();
        doc.attach_text("new content".to_string());
        doc.touch();
        doc.save().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new content");
        assert_eq!(doc.file_size, 11);
        assert_eq!(doc.disk_status(), DiskStatus::Unchanged);
        assert!(!doc.is_modified());
        let leftovers = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".luban-save-"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[cfg(unix)]
    #[test]
    fn failed_save_keeps_the_original_bytes() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = temp_file("locked.txt", b"protected");
        let mut doc = Document::open(&path).unwrap();
        doc.attach_text("replacement".to_string());
        doc.touch();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        assert!(doc.save().is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "protected");
        assert!(doc.is_modified());
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

    #[test]
    fn detaches_and_restores_the_text_snapshot() {
        let mut doc =
            Document::from_text("héllo\nworld", TextEncoding::Utf8, Eol::Lf, "Plain Text");
        let (text, count) = doc.detach_text();
        assert!(doc.is_text_detached());
        assert!(doc.text.is_empty());
        assert_eq!(count, 11);
        assert_eq!(doc.char_count(), 11);

        // Edits require the buffer to be materialized again.
        assert!(!doc.apply_edit(0, 0, "x", EditKind::Typing, 1, 1));
        doc.set_selection(99, 99);
        assert_eq!(doc.selection(), (11, 11));

        doc.attach_text_counted(text, count);
        assert!(!doc.is_text_detached());
        assert_eq!(doc.text, "héllo\nworld");
        assert_eq!(doc.char_count(), 11);
        assert!(doc.apply_edit(0, 0, "x", EditKind::Typing, 1, 1));
        assert_eq!(doc.char_count(), 12);
    }

    #[test]
    fn cached_char_count_tracks_edits_and_undo() {
        let mut doc = Document::from_text("abc", TextEncoding::Utf8, Eol::Lf, "Plain Text");
        assert_eq!(doc.char_count(), 3);
        doc.apply_edit(3, 0, "世界", EditKind::Typing, 5, 5);
        assert_eq!(doc.char_count(), 5);
        assert!(doc.undo());
        assert_eq!(doc.char_count(), 3);
        assert!(doc.redo());
        assert_eq!(doc.char_count(), 5);
    }

    #[test]
    fn saves_lf_utf8_without_changing_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lf.txt");
        let mut doc =
            Document::from_text("héllo\nworld\n", TextEncoding::Utf8, Eol::Lf, "Plain Text");
        doc.save_as(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), "héllo\nworld\n".as_bytes());
        assert_eq!(doc.char_count(), 12);
    }

    #[test]
    fn saves_owned_crlf_and_cr() {
        let dir = tempfile::tempdir().unwrap();
        for (eol, expected) in [
            (Eol::CrLf, "a\r\nb\r\n".as_bytes().to_vec()),
            (Eol::Cr, b"a\rb\r".to_vec()),
        ] {
            let path = dir.path().join(format!("{}.txt", eol.label()));
            let mut doc = Document::from_text("a\nb\n", TextEncoding::Utf8, eol, "Plain Text");
            doc.save_as(&path).unwrap();
            assert_eq!(fs::read(&path).unwrap(), expected);
            assert_eq!(doc.text, "a\nb\n", "{eol:?}");
            assert_eq!(doc.char_count(), 4);
        }
    }

    #[test]
    fn partial_documents_cannot_overwrite_their_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        fs::write(&path, b"aaa\nbbb\nccc\n").unwrap();
        let mut doc = Document::open_range(
            &path,
            4,
            12,
            1024,
            &std::sync::atomic::AtomicBool::new(false),
            &std::sync::atomic::AtomicU64::new(0),
        )
        .unwrap();
        assert!(doc.is_partial());
        assert_eq!(
            doc.partial_range(),
            Some(PartialRange {
                start: 4,
                end: 12,
                total: 12
            })
        );
        assert_eq!(doc.text, "bbb\nccc\n");
        assert_eq!(doc.source_size(), 12);

        doc.touch();
        assert!(doc.save().is_err());
        assert!(doc.save_as(&path).is_err());
        let alias = dir.path().join(".").join("big.txt");
        assert!(doc.save_as(&alias).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"aaa\nbbb\nccc\n");

        let copy = dir.path().join("part.txt");
        doc.save_as(&copy).unwrap();
        assert!(!doc.is_partial());
        assert_eq!(doc.path.as_deref(), Some(copy.as_path()));
        assert_eq!(fs::read(&copy).unwrap(), b"bbb\nccc\n");
        assert!(doc.reload().is_ok());
    }

    #[test]
    fn huge_documents_expose_engine_metadata_and_refuse_direct_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.txt");
        let mut contents = String::new();
        while contents.len() < 2 * 1024 * 1024 {
            contents.push_str("a line of huge text\n");
        }
        fs::write(&path, contents.as_bytes()).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let progress = std::sync::atomic::AtomicU64::new(0);
        let mut doc = Document::open_huge(&path, 1, &cancel, &progress).unwrap();
        assert!(doc.is_huge());
        assert!(doc.is_text_detached());
        assert_eq!(doc.char_count(), contents.chars().count());
        assert_eq!(doc.line_count(), contents.matches('\n').count() + 1);
        assert!(!doc.is_modified());
        assert_eq!(doc.source_size(), contents.len() as u64);
        assert!(doc.take_initial_window().is_some());
        assert!(doc.save().is_err());
        assert!(doc.reload().is_err());
        assert!(!doc.convert_eol(Eol::CrLf));
        assert!(!doc.set_encoding(TextEncoding::Utf16Le));
        assert!(!doc.apply_edit(0, 0, "x", EditKind::Typing, 1, 1));
        let (text, count) = doc.detach_text();
        assert!(text.is_empty());
        assert_eq!(count, contents.chars().count());
        doc.select_all();
        assert!(doc.has_selection());
        assert_eq!(doc.selection().1, contents.chars().count());
    }

    #[test]
    fn huge_range_documents_expose_the_source_range_and_local_lines() {
        let (_dir, path) = temp_file("range-doc.txt", b"one\ntwo\nthree\nfour\n");
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let progress = std::sync::atomic::AtomicU64::new(0);
        let mut doc = Document::open_huge_range(&path, 4, 9, 1, &cancel, &progress).unwrap();
        assert!(doc.is_huge());
        assert!(doc.is_partial());
        assert_eq!(
            doc.partial_range(),
            Some(PartialRange {
                start: 4,
                end: 14,
                total: 19,
            })
        );
        assert_eq!(doc.source_size(), 19);
        assert_eq!(doc.char_count(), "two\nthree\n".chars().count());
        assert_eq!(doc.line_count(), 3);
        assert!(!doc.is_modified());
        let window = doc.take_initial_window().unwrap();
        assert_eq!(window.text, "two\nthree\n");
        assert_eq!(window.line, 1);
        assert!(window.at_eof);
        assert!(doc.save().is_err());
        assert!(doc.save_as(&path).is_err());
    }

    #[test]
    fn partial_documents_cannot_reload_from_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        fs::write(&path, b"aaa\nbbb\nccc\n").unwrap();
        let mut doc = Document::open_range(
            &path,
            0,
            8,
            1024,
            &std::sync::atomic::AtomicBool::new(false),
            &std::sync::atomic::AtomicU64::new(0),
        )
        .unwrap();
        assert!(doc.reload().is_err());
        assert!(doc.is_partial());
    }
}

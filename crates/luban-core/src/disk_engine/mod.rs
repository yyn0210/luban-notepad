//! UltraEdit-style disk engine for text files larger than the whole-file limit.
//!
//! The engine keeps the complete document on disk. It holds a block index over
//! the original file, a copy-on-write journal for edited ranges and a piece
//! table describing the current logical text. Editors mount a bounded window
//! (see [`WINDOW_BYTES`]) and swap it as the user scrolls.
//!
//! The logical text is UTF-8 with `\n` line endings, matching the rest of the
//! application. Source blocks are decoded on demand; unmodified blocks are
//! copied byte-for-byte when saving so encoding, BOM, line endings and every
//! untouched byte survive a save.
//!
//! The engine has no GUI dependencies and is safe to use from a background
//! executor behind an `RwLock`.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::encoding::{self, TextEncoding};
use crate::eol::{self, Eol};
use crate::error::{Error, Result};
use crate::i18n::tr;
use crate::search::SearchEngine;

mod journal;
mod save;

pub use journal::cleanup_stale;
pub use save::{SaveReport, SaveSnapshot};

use journal::{read_exact_at, Journal};

/// Target size of the editor window. It bounds the editor's rope while leaving
/// roughly 20k lines of 100 characters on each side of the cursor.
pub const WINDOW_BYTES: u64 = 2 * 1024 * 1024;

/// Upper bound on the number of lines in one window. A file made of tiny lines
/// must not turn the window into a million-line rope.
pub const WINDOW_MAX_LINES: u64 = 50_000;

/// Preferred size of an index block.
const BLOCK_TARGET_BYTES: usize = 1024 * 1024;

/// Bytes read at a time while indexing.
const READ_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Undo history limits. The journal remains append-only; dropping the oldest
/// entry only forgets how to undo it.
const HISTORY_LIMIT_OPS: usize = 1000;
const HISTORY_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// Bytes scanned by one search step.
pub const SEARCH_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// Extra bytes read past a search chunk so matches crossing the boundary are
/// still recognised.
const SEARCH_OVERLAP_BYTES: u64 = 1024 * 1024;

/// How many decoded blocks stay cached for window reads.
const BLOCK_CACHE_ENTRIES: usize = 8;

/// File facts gathered before deciding which open mode to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileProbe {
    pub size: u64,
    pub encoding: TextEncoding,
    pub eol: Eol,
    pub non_text: bool,
    pub read_only: bool,
    pub last_modified: Option<SystemTime>,
}

/// A bounded slice of the logical text mounted in the editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowText {
    pub text: String,
    /// Logical byte offset of the first character.
    pub start: u64,
    /// Logical byte length of `text`.
    pub len: u64,
    /// 1-based global line of the first character.
    pub line: u64,
    /// Line breaks inside the window.
    pub newlines: u64,
    /// 1-based column of the first character.
    pub start_column: u64,
    /// The window starts in the middle of a long line.
    pub starts_mid_line: bool,
    /// The window ends before a line break.
    pub ends_mid_line: bool,
    pub at_eof: bool,
}

/// Result of applying an edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditOutcome {
    pub start: u64,
    pub removed_len: u64,
    pub inserted_len: u64,
    pub line: u64,
}

/// Where an undo or redo left the document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryOutcome {
    pub offset: u64,
    pub line: u64,
    pub cursor: u64,
}

/// A match expressed in logical byte offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub start: u64,
    pub end: u64,
    pub line: u64,
}

/// Outcome of a streaming replace-all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplaceReport {
    pub count: usize,
    pub cancelled: bool,
    pub bytes_scanned: u64,
}

/// One indexed block of the base file.
#[derive(Clone, Debug)]
pub(super) struct Block {
    /// Byte offset in the base file body (after the BOM).
    pub src_offset: u64,
    /// Byte length in the base file.
    pub src_len: u64,
    /// Logical byte offset of the decoded block.
    pub logical_start: u64,
    /// Logical byte length of the decoded block.
    pub logical_len: u64,
    pub chars: u64,
    /// 1-based global line of the first character.
    pub line: u64,
    pub newlines: u64,
}

#[derive(Clone, Debug)]
pub(super) enum PieceKind {
    Source { block: u32 },
    Journal { offset: u64 },
}

/// A contiguous run of the current logical text.
#[derive(Clone, Debug)]
pub(super) struct Piece {
    pub kind: PieceKind,
    /// Logical byte offset inside the decoded block or the journal.
    pub offset: u64,
    pub len: u64,
    pub chars: u64,
    pub newlines: u64,
    pub logical_start: u64,
    pub line: u64,
    /// The piece can be copied from the base file without re-encoding.
    pub raw: bool,
}

#[derive(Clone, Debug)]
pub(super) struct Hunk {
    pub start: u64,
    pub before: Vec<Piece>,
    pub after: Vec<Piece>,
}

#[derive(Clone, Debug)]
struct EditOp {
    hunks: Vec<Hunk>,
    cursor_before: u64,
    cursor_after: u64,
    before_bytes: u64,
}

struct BlockCache {
    map: HashMap<u32, Arc<String>>,
    order: VecDeque<u32>,
}

impl BlockCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, index: u32) -> Option<Arc<String>> {
        self.map.get(&index).cloned()
    }

    fn insert(&mut self, index: u32, text: Arc<String>) {
        if self.map.contains_key(&index) {
            return;
        }
        while self.order.len() >= BLOCK_CACHE_ENTRIES {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.order.push_back(index);
        self.map.insert(index, text);
    }
}

/// Positioned reader over the base file.
#[derive(Debug)]
pub(super) struct BaseFile {
    file: File,
}

impl BaseFile {
    pub(super) fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
        })
    }

    pub(super) fn read(&self, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
        let mut buffer = vec![0u8; usize::try_from(len).unwrap_or(0)];
        if !buffer.is_empty() {
            read_exact_at(&self.file, &mut buffer, offset)?;
        }
        Ok(buffer)
    }
}

/// The disk-backed document.
pub struct DiskEngine {
    path: PathBuf,
    encoding: TextEncoding,
    eol: Eol,
    bom_len: u64,
    base_len: u64,
    body_len: u64,
    base_modified: Option<SystemTime>,
    read_only: bool,
    editing_locked: bool,
    base: Arc<BaseFile>,
    pub(super) blocks: Vec<Block>,
    pub(super) pieces: Vec<Piece>,
    pub(super) journal: Journal,
    total_len: u64,
    total_chars: u64,
    total_newlines: u64,
    history: Vec<EditOp>,
    history_pos: usize,
    history_bytes: u64,
    /// Applied edits were dropped by the history limit. They are part of the
    /// current content but can no longer be undone.
    history_trimmed: bool,
    revision: u64,
    cache: Mutex<BlockCache>,
}

impl std::fmt::Debug for DiskEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskEngine")
            .field("path", &self.path)
            .field("encoding", &self.encoding)
            .field("eol", &self.eol)
            .field("base_len", &self.base_len)
            .field("total_len", &self.total_len)
            .field("blocks", &self.blocks.len())
            .field("pieces", &self.pieces.len())
            .field("modified", &self.is_modified())
            .field("read_only", &self.read_only)
            .finish()
    }
}

impl DiskEngine {
    /// Inspect a file without indexing it.
    pub fn probe(path: &Path) -> Result<FileProbe> {
        let metadata = fs::metadata(path)?;
        if metadata.is_dir() {
            return Err(Error::InvalidPath(path.to_path_buf()));
        }
        let size = metadata.len();
        let head = read_head(path, size)?;
        let non_text = encoding::looks_binary(&head);
        let decoded = encoding::decode(&head);
        Ok(FileProbe {
            size,
            encoding: decoded.encoding,
            eol: eol::detect(&decoded.text),
            non_text,
            read_only: metadata.permissions().readonly(),
            last_modified: metadata.modified().ok(),
        })
    }

    /// Build an engine over `path`. The whole file is scanned once; nothing is
    /// copied. Non-text files are rejected so callers keep their existing
    /// binary/partial-open behaviour.
    pub fn open(path: &Path, cancel: &AtomicBool, progress: &AtomicU64) -> Result<Self> {
        progress.store(0, Ordering::Relaxed);
        let probe = Self::probe(path)?;
        if probe.non_text {
            return Err(Error::Other(tr("core.disk_engine.not_text").to_string()));
        }
        let base = Arc::new(BaseFile::open(path)?);
        let bom_len = detect_bom_len(&base, probe.encoding, probe.size)?;
        let blocks = index_blocks(&base, bom_len, probe.size, probe.encoding, cancel, progress)?;
        let mut engine = Self {
            path: path.to_path_buf(),
            encoding: probe.encoding,
            eol: probe.eol,
            bom_len,
            base_len: probe.size,
            body_len: probe.size.saturating_sub(bom_len),
            base_modified: probe.last_modified,
            read_only: probe.read_only,
            editing_locked: false,
            base,
            blocks,
            pieces: Vec::new(),
            journal: Journal::create()?,
            total_len: 0,
            total_chars: 0,
            total_newlines: 0,
            history: Vec::new(),
            history_pos: 0,
            history_bytes: 0,
            history_trimmed: false,
            revision: 0,
            cache: Mutex::new(BlockCache::new()),
        };
        engine.rebuild_pieces_from_blocks();
        // The index describes the file as it was when the scan started. If it
        // changed while scanning, mounting the tab would mix versions.
        if let Ok(metadata) = fs::metadata(path) {
            let changed = metadata.len() != probe.size
                || match (metadata.modified().ok(), probe.last_modified) {
                    (Some(current), Some(known)) => current != known,
                    _ => false,
                };
            if changed {
                return Err(Error::Other(
                    tr("core.disk_engine.file_changed_during_index").to_string(),
                ));
            }
        }
        progress.store(probe.size, Ordering::Relaxed);
        Ok(engine)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn encoding(&self) -> TextEncoding {
        self.encoding
    }

    pub fn eol(&self) -> Eol {
        self.eol
    }

    /// Size of the base file on disk, including the BOM.
    pub fn base_size(&self) -> u64 {
        self.base_len
    }

    pub fn last_modified(&self) -> Option<SystemTime> {
        self.base_modified
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Total logical bytes (UTF-8, LF) of the document.
    pub fn total_len(&self) -> u64 {
        self.total_len
    }

    /// Total characters in the document.
    pub fn total_chars(&self) -> u64 {
        self.total_chars
    }

    /// Total lines; an empty document has one line.
    pub fn total_lines(&self) -> u64 {
        self.total_newlines + 1
    }

    pub fn is_modified(&self) -> bool {
        self.history_pos > 0 || self.history_trimmed
    }

    /// Whether the oldest applied edits were dropped by the history limit.
    pub fn history_trimmed(&self) -> bool {
        self.history_trimmed
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn can_undo(&self) -> bool {
        self.history_pos > 0
    }

    pub fn can_redo(&self) -> bool {
        self.history_pos < self.history.len()
    }

    pub fn journal_len(&self) -> u64 {
        self.journal.len()
    }

    /// Conservative upper bound for the saved file size.
    pub fn estimated_save_size(&self) -> u64 {
        self.total_len
            .saturating_mul(3)
            .saturating_add(self.bom_len)
            .saturating_add(4096)
    }

    /// Reject edits while a background save is copying the snapshot.
    pub fn set_editing_locked(&mut self, locked: bool) {
        self.editing_locked = locked;
    }

    pub fn editing_locked(&self) -> bool {
        self.editing_locked
    }

    /// Whether the file changed on disk since it was opened or last saved.
    pub fn changed_on_disk(&self) -> bool {
        let Ok(metadata) = fs::metadata(&self.path) else {
            return false;
        };
        if metadata.len() != self.base_len {
            return true;
        }
        match (metadata.modified().ok(), self.base_modified) {
            (Some(current), Some(known)) => current != known,
            _ => false,
        }
    }

    // ------------------------------------------------------------------
    // Reading
    // ------------------------------------------------------------------

    /// Read a window containing `cursor`, preferring to start at a line
    /// boundary so the editor's local line numbers stay meaningful.
    pub fn read_window(&self, cursor: u64, max_bytes: u64) -> Result<WindowText> {
        self.read_window_bounded(cursor, max_bytes, WINDOW_MAX_LINES)
    }

    fn read_window_bounded(
        &self,
        cursor: u64,
        max_bytes: u64,
        max_lines: u64,
    ) -> Result<WindowText> {
        let max_bytes = max_bytes.max(1);
        if self.total_len == 0 {
            return Ok(WindowText {
                text: String::new(),
                start: 0,
                len: 0,
                line: 1,
                newlines: 0,
                start_column: 1,
                starts_mid_line: false,
                ends_mid_line: false,
                at_eof: true,
            });
        }
        let cursor = cursor.min(self.total_len);
        let look_back = (max_bytes / 2).max(1);
        let line_start = self.line_start_before(cursor, look_back)?;
        let start = match line_start {
            Some(start) if cursor.saturating_sub(start) <= look_back => start,
            _ => self.floor_char_boundary(cursor.min(self.total_len.saturating_sub(1))),
        };
        self.read_window_from(start, max_bytes, max_lines)
    }

    /// Read a window starting at the given global line.
    pub fn read_window_from_line(&self, line: u64, max_bytes: u64) -> Result<WindowText> {
        let offset = self.offset_for_line(line, 1)?;
        self.read_window_bounded(offset, max_bytes, WINDOW_MAX_LINES)
    }

    fn read_window_from(&self, start: u64, max_bytes: u64, max_lines: u64) -> Result<WindowText> {
        let start = self.floor_char_boundary(start.min(self.total_len));
        let (text, newlines) = self.collect_limited(start, max_bytes, max_lines)?;
        let len = text.len() as u64;
        let line = self.line_at(start)?;
        let starts_mid_line = start > 0 && self.byte_at(start - 1)? != b'\n';
        let start_column = if starts_mid_line {
            self.column_at(start)?
        } else {
            1
        };
        let at_eof = start.saturating_add(len) >= self.total_len;
        let ends_mid_line = !at_eof && !text.ends_with('\n');
        Ok(WindowText {
            text,
            start,
            len,
            line,
            newlines,
            start_column,
            starts_mid_line,
            ends_mid_line,
            at_eof,
        })
    }

    /// Read up to `len` logical bytes starting at `start`.
    pub fn read_text(&self, start: u64, len: u64) -> Result<String> {
        self.collect_text(start, len)
    }

    fn collect_text(&self, start: u64, len: u64) -> Result<String> {
        if len == 0 || start >= self.total_len {
            return Ok(String::new());
        }
        let end = start.saturating_add(len).min(self.total_len);
        let mut remaining = end - start;
        let mut index = self.piece_index_at(start);
        let mut out = String::with_capacity(usize::try_from(remaining).unwrap_or(0).min(8 << 20));
        while remaining > 0 && index < self.pieces.len() {
            let piece = &self.pieces[index];
            let text = self.piece_text(piece)?;
            let local = start.saturating_sub(piece.logical_start).min(piece.len);
            let take = (piece.len - local).min(remaining);
            let begin = usize::try_from(local).unwrap_or(0);
            let finish = usize::try_from(local + take).unwrap_or(text.len());
            if begin >= text.len() {
                break;
            }
            let finish = finish.min(text.len());
            if !text.is_char_boundary(begin) || !text.is_char_boundary(finish) {
                break;
            }
            out.push_str(&text[begin..finish]);
            remaining -= take;
            index += 1;
        }
        Ok(out)
    }

    fn collect_limited(&self, start: u64, max_bytes: u64, max_lines: u64) -> Result<(String, u64)> {
        if start >= self.total_len {
            return Ok((String::new(), 0));
        }
        let mut remaining = max_bytes;
        let mut remaining_lines = max_lines;
        let mut index = self.piece_index_at(start);
        let mut out = String::new();
        let mut newlines = 0u64;
        while remaining > 0 && remaining_lines > 0 && index < self.pieces.len() {
            let piece = &self.pieces[index];
            let text = self.piece_text(piece)?;
            let local = usize::try_from(start.saturating_sub(piece.logical_start)).unwrap_or(0);
            if local >= text.len() {
                index += 1;
                continue;
            }
            let local_offset = start.saturating_sub(piece.logical_start);
            let available = piece.len.saturating_sub(local_offset);
            let take = available.min(remaining);
            let finish = usize::try_from(local_offset.saturating_add(take))
                .unwrap_or(text.len())
                .min(text.len());
            let begin = local;
            let mut finish = if finish > begin && !text.is_char_boundary(finish) {
                let mut adjusted = finish;
                while adjusted > begin && !text.is_char_boundary(adjusted) {
                    adjusted -= 1;
                }
                adjusted
            } else {
                finish
            };
            // Never cross more line breaks than requested.
            let mut count = count_newlines(&text[begin..finish]);
            if count > remaining_lines {
                let mut seen = 0u64;
                let mut cutoff = begin;
                for (offset, byte) in text[begin..finish].bytes().enumerate() {
                    cutoff = begin + offset;
                    if byte == b'\n' {
                        seen += 1;
                        if seen == remaining_lines {
                            cutoff += 1;
                            break;
                        }
                    }
                }
                finish = cutoff.max(begin + 1).min(finish);
                count = count_newlines(&text[begin..finish]);
            }
            if finish <= begin {
                break;
            }
            out.push_str(&text[begin..finish]);
            remaining = remaining.saturating_sub((finish - begin) as u64);
            remaining_lines = remaining_lines.saturating_sub(count);
            newlines += count;
            index += 1;
        }
        Ok((out, newlines))
    }

    fn piece_text(&self, piece: &Piece) -> Result<String> {
        match piece.kind {
            PieceKind::Source { block } => {
                let text = self.block_text(block)?;
                let begin = usize::try_from(piece.offset).unwrap_or(0);
                let end = usize::try_from(piece.offset + piece.len).unwrap_or(text.len());
                if begin >= text.len() || end > text.len() {
                    return Ok(String::new());
                }
                let begin = clamp_boundary(&text, begin);
                let end = clamp_boundary(&text, end);
                Ok(text[begin..end].to_owned())
            }
            PieceKind::Journal { offset } => {
                let bytes = self.journal.read(offset, piece.len)?;
                String::from_utf8(bytes)
                    .map_err(|_| Error::Other(tr("core.disk_engine.journal_corrupt").to_string()))
            }
        }
    }

    fn block_text(&self, index: u32) -> Result<Arc<String>> {
        if let Ok(cache) = self.cache.lock() {
            if let Some(text) = cache.get(index) {
                return Ok(text);
            }
        }
        let block = self
            .blocks
            .get(index as usize)
            .ok_or_else(|| Error::Other(tr("core.disk_engine.corrupt_index").to_string()))?;
        let raw = self
            .base
            .read(self.bom_len + block.src_offset, block.src_len)?;
        let text = Arc::new(eol::to_lf_owned(decode_body(&raw, self.encoding)));
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(index, text.clone());
        }
        Ok(text)
    }

    fn byte_at(&self, offset: u64) -> Result<u8> {
        let text = self.collect_text(offset, 1)?;
        Ok(text.as_bytes().first().copied().unwrap_or(0))
    }

    /// Largest character boundary at or before `offset`.
    fn floor_char_boundary(&self, offset: u64) -> u64 {
        let offset = offset.min(self.total_len);
        let index = self.piece_index_at(offset);
        if index >= self.pieces.len() {
            return self.total_len;
        }
        let piece = &self.pieces[index];
        let local = offset.saturating_sub(piece.logical_start);
        if let Ok(text) = self.piece_text(piece) {
            let mut local = local.min(text.len() as u64) as usize;
            while local > 0 && !text.is_char_boundary(local) {
                local -= 1;
            }
            return piece.logical_start.saturating_add(local as u64);
        }
        offset
    }

    /// 1-based line containing `offset`.
    pub fn line_at(&self, offset: u64) -> Result<u64> {
        let offset = offset.min(self.total_len);
        let index = self.piece_index_at(offset);
        if index >= self.pieces.len() {
            return Ok(self.total_lines());
        }
        let piece = &self.pieces[index];
        let local = offset.saturating_sub(piece.logical_start);
        if local == 0 {
            return Ok(piece.line);
        }
        let text = self.piece_text(piece)?;
        let local = clamp_boundary(&text, usize::try_from(local).unwrap_or(text.len()));
        Ok(piece.line + count_newlines(&text[..local]))
    }

    /// 1-based column of `offset`. Columns count characters.
    pub fn column_at(&self, offset: u64) -> Result<u64> {
        let offset = offset.min(self.total_len);
        if offset == 0 {
            return Ok(1);
        }
        let window = (8 * 1024 * 1024).min(offset);
        let start = self.floor_char_boundary(offset - window);
        let text = self.collect_text(start, offset - start)?;
        let column = match text.rfind('\n') {
            Some(index) => text[index + 1..].chars().count() as u64 + 1,
            None => text.chars().count() as u64 + 1,
        };
        Ok(column)
    }

    /// Globally unique 1-based line of the first character at or after `offset`.
    pub fn line_col_at(&self, offset: u64) -> Result<(u64, u64)> {
        Ok((self.line_at(offset)?, self.column_at(offset)?))
    }

    fn line_start_before(&self, offset: u64, limit: u64) -> Result<Option<u64>> {
        if offset == 0 {
            return Ok(Some(0));
        }
        let window = limit.max(1).min(offset);
        let start = self.floor_char_boundary(offset - window);
        let text = self.collect_text(start, offset - start)?;
        if let Some(index) = text.rfind('\n') {
            return Ok(Some(start + index as u64 + 1));
        }
        if start == 0 {
            Ok(Some(0))
        } else {
            Ok(None)
        }
    }

    /// Logical byte offset of a 1-based line/column.
    pub fn offset_for_line(&self, line: u64, column: u64) -> Result<u64> {
        if self.pieces.is_empty() {
            return Ok(0);
        }
        let line = line.clamp(1, self.total_lines());
        let mut index = self
            .pieces
            .partition_point(|piece| piece.line <= line)
            .saturating_sub(1);
        // A long line is split across several pieces that all report the same
        // starting line, so the last such piece starts mid-line. Walk back to
        // the first piece of the line or the column would be counted from the
        // end of the line.
        while index > 0 && self.pieces[index - 1].line == line {
            index -= 1;
        }
        let piece = &self.pieces[index];
        // A piece may start in the middle of an over-long line. Anchor the
        // scan at the true line start when it lies within a megabyte.
        let anchor = self
            .line_start_before(piece.logical_start, 1024 * 1024)?
            .unwrap_or(piece.logical_start);
        let anchor_line = self.line_at(anchor)?;
        let mut remaining = line.saturating_sub(anchor_line);
        let mut offset = anchor;
        let mut cursor = self.piece_index_at(anchor);
        while remaining > 0 && cursor < self.pieces.len() {
            let current = &self.pieces[cursor];
            let text = self.piece_text(current)?;
            let begin = usize::try_from(anchor.saturating_sub(current.logical_start))
                .unwrap_or(0)
                .min(text.len());
            let mut found = None;
            for (index, byte) in text.bytes().enumerate().skip(begin) {
                if byte == b'\n' {
                    remaining -= 1;
                    if remaining == 0 {
                        found = Some(current.logical_start + index as u64 + 1);
                        break;
                    }
                }
            }
            match found {
                Some(next) => {
                    offset = next;
                    break;
                }
                None => {
                    cursor += 1;
                    if cursor < self.pieces.len() {
                        offset = self.pieces[cursor].logical_start;
                    }
                }
            }
        }
        if remaining > 0 {
            return Ok(self.total_len);
        }
        // Find the byte offset of the requested column inside the target line.
        let mut advance = column.max(1).saturating_sub(1);
        let mut position = offset;
        while advance > 0 && position < self.total_len {
            let available = self.total_len.saturating_sub(position);
            let chunk = self.collect_text(position, available.min(4 * 1024 * 1024))?;
            if chunk.is_empty() {
                break;
            }
            let mut consumed = 0u64;
            let mut hit_newline = false;
            for ch in chunk.chars() {
                if ch == '\n' {
                    hit_newline = true;
                    break;
                }
                if consumed == advance {
                    break;
                }
                position += ch.len_utf8() as u64;
                consumed += 1;
            }
            advance -= consumed;
            if hit_newline || consumed == 0 {
                break;
            }
        }
        Ok(position.min(self.total_len))
    }

    // ------------------------------------------------------------------
    // Editing
    // ------------------------------------------------------------------

    /// Apply one editor change. `start` and `removed` are logical byte offsets;
    /// `inserted` is LF-normalised UTF-8.
    pub fn apply_edit(&mut self, start: u64, removed: u64, inserted: &str) -> Result<EditOutcome> {
        if self.read_only {
            return Err(Error::Other(tr("core.document.read_only").to_string()));
        }
        if self.editing_locked {
            return Err(Error::Other(
                tr("core.disk_engine.save_in_progress").to_string(),
            ));
        }
        if start > self.total_len {
            return Err(Error::Other(
                tr("core.disk_engine.invalid_edit").to_string(),
            ));
        }
        let removed = removed.min(self.total_len - start);
        if removed == 0 && inserted.is_empty() {
            return Ok(EditOutcome {
                start,
                removed_len: 0,
                inserted_len: 0,
                line: self.line_at(start)?,
            });
        }
        let line = self.line_at(start)?;
        let hunk = self.apply_hunk(start, removed, inserted)?;
        let inserted_len = inserted.len() as u64;
        self.push_history(
            vec![hunk],
            start,
            start.saturating_add(inserted_len),
            inserted_len,
        );
        Ok(EditOutcome {
            start,
            removed_len: removed,
            inserted_len,
            line,
        })
    }

    fn apply_hunk(&mut self, start: u64, removed: u64, inserted: &str) -> Result<Hunk> {
        let mut index = self.piece_index_at(start);
        if index < self.pieces.len() && start > self.pieces[index].logical_start {
            index = self.split_piece(index, start - self.pieces[index].logical_start)?;
        }
        let mut before = Vec::new();
        let mut remaining = removed;
        let mut remove_index = index;
        while remaining > 0 && remove_index < self.pieces.len() {
            let length = self.pieces[remove_index].len;
            if length <= remaining {
                before.push(self.pieces[remove_index].clone());
                remaining -= length;
                remove_index += 1;
            } else {
                self.split_piece(remove_index, remaining)?;
                before.push(self.pieces[remove_index].clone());
                remaining = 0;
                remove_index += 1;
            }
        }
        let after = if inserted.is_empty() {
            Vec::new()
        } else {
            let offset = self.journal.append(inserted.as_bytes())?;
            vec![Piece {
                kind: PieceKind::Journal { offset },
                offset: 0,
                len: inserted.len() as u64,
                chars: inserted.chars().count() as u64,
                newlines: count_newlines(inserted),
                logical_start: 0,
                line: 0,
                raw: false,
            }]
        };
        self.pieces
            .splice(index..remove_index, after.iter().cloned());
        self.recompute_from(index);
        let before_len: u64 = before.iter().map(|piece| piece.len).sum();
        let before_chars: u64 = before.iter().map(|piece| piece.chars).sum();
        let before_newlines: u64 = before.iter().map(|piece| piece.newlines).sum();
        let after_len: u64 = after.iter().map(|piece| piece.len).sum();
        let after_chars: u64 = after.iter().map(|piece| piece.chars).sum();
        let after_newlines: u64 = after.iter().map(|piece| piece.newlines).sum();
        self.total_len = self.total_len - before_len + after_len;
        self.total_chars = self.total_chars - before_chars + after_chars;
        self.total_newlines = self.total_newlines - before_newlines + after_newlines;
        self.revision = self.revision.wrapping_add(1);
        Ok(Hunk {
            start,
            before,
            after,
        })
    }

    fn push_history(
        &mut self,
        hunks: Vec<Hunk>,
        cursor_before: u64,
        cursor_after: u64,
        _inserted: u64,
    ) {
        if self.history_pos < self.history.len() {
            for dropped in &self.history[self.history_pos..] {
                self.history_bytes = self.history_bytes.saturating_sub(dropped.before_bytes);
            }
            self.history.truncate(self.history_pos);
        }
        let before_bytes: u64 = hunks
            .iter()
            .flat_map(|hunk| hunk.before.iter())
            .map(|piece| piece.len)
            .sum();
        self.history.push(EditOp {
            hunks,
            cursor_before,
            cursor_after,
            before_bytes,
        });
        self.history_pos = self.history.len();
        self.history_bytes = self.history_bytes.saturating_add(before_bytes);
        self.trim_history();
    }

    fn trim_history(&mut self) {
        while self.history_pos == self.history.len()
            && !self.history.is_empty()
            && (self.history.len() > HISTORY_LIMIT_OPS || self.history_bytes > HISTORY_LIMIT_BYTES)
        {
            let oldest = self.history.remove(0);
            self.history_trimmed = true;
            self.history_pos = self.history_pos.saturating_sub(1);
            self.history_bytes = self.history_bytes.saturating_sub(oldest.before_bytes);
            let before_len: u64 = oldest
                .hunks
                .iter()
                .flat_map(|hunk| hunk.before.iter())
                .map(|piece| piece.len)
                .sum();
            let after_len: u64 = oldest
                .hunks
                .iter()
                .flat_map(|hunk| hunk.after.iter())
                .map(|piece| piece.len)
                .sum();
            let delta = after_len as i64 - before_len as i64;
            if delta != 0 {
                for op in &mut self.history {
                    for hunk in &mut op.hunks {
                        hunk.start = shift_saturating(hunk.start, -delta);
                    }
                    op.cursor_before = shift_saturating(op.cursor_before, -delta);
                    op.cursor_after = shift_saturating(op.cursor_after, -delta);
                }
            }
        }
    }

    pub fn undo(&mut self) -> Option<HistoryOutcome> {
        if self.editing_locked || self.history_pos == 0 {
            return None;
        }
        let op = self.history[self.history_pos - 1].clone();
        for hunk in op.hunks.iter().rev() {
            if !self.splice_hunk(hunk, true) {
                self.history.clear();
                self.history_pos = 0;
                self.history_bytes = 0;
                return None;
            }
        }
        self.history_pos -= 1;
        self.revision = self.revision.wrapping_add(1);
        self.compact_pieces();
        let cursor = op.cursor_before.min(self.total_len);
        let line = self.line_at(cursor).unwrap_or(1);
        Some(HistoryOutcome {
            offset: cursor,
            line,
            cursor,
        })
    }

    pub fn redo(&mut self) -> Option<HistoryOutcome> {
        if self.editing_locked || self.history_pos >= self.history.len() {
            return None;
        }
        let op = self.history[self.history_pos].clone();
        for hunk in op.hunks.iter() {
            if !self.splice_hunk(hunk, false) {
                self.history.truncate(self.history_pos);
                return None;
            }
        }
        self.history_pos += 1;
        self.revision = self.revision.wrapping_add(1);
        self.compact_pieces();
        let cursor = op.cursor_after.min(self.total_len);
        let line = self.line_at(cursor).unwrap_or(1);
        Some(HistoryOutcome {
            offset: cursor,
            line,
            cursor,
        })
    }

    fn splice_hunk(&mut self, hunk: &Hunk, undo: bool) -> bool {
        let (remove, insert) = if undo {
            (&hunk.after, &hunk.before)
        } else {
            (&hunk.before, &hunk.after)
        };
        let start = hunk.start.min(self.total_len);
        let mut index = self.piece_index_at(start);
        if index < self.pieces.len() && start > self.pieces[index].logical_start {
            match self.split_piece(index, start - self.pieces[index].logical_start) {
                Ok(next) => index = next,
                Err(_) => return false,
            }
        }
        let mut remaining: u64 = remove.iter().map(|piece| piece.len).sum();
        let mut remove_index = index;
        while remaining > 0 && remove_index < self.pieces.len() {
            let length = self.pieces[remove_index].len;
            if length <= remaining {
                remaining -= length;
                remove_index += 1;
            } else {
                match self.split_piece(remove_index, remaining) {
                    Ok(_) => {
                        remaining = 0;
                        remove_index += 1;
                    }
                    Err(_) => return false,
                }
            }
        }
        if remaining > 0 {
            return false;
        }
        let removed_len: u64 = self.pieces[index..remove_index]
            .iter()
            .map(|piece| piece.len)
            .sum();
        let removed_chars: u64 = self.pieces[index..remove_index]
            .iter()
            .map(|piece| piece.chars)
            .sum();
        let removed_newlines: u64 = self.pieces[index..remove_index]
            .iter()
            .map(|piece| piece.newlines)
            .sum();
        let insert_len: u64 = insert.iter().map(|piece| piece.len).sum();
        let insert_chars: u64 = insert.iter().map(|piece| piece.chars).sum();
        let insert_newlines: u64 = insert.iter().map(|piece| piece.newlines).sum();
        self.pieces
            .splice(index..remove_index, insert.iter().cloned());
        self.recompute_from(index);
        self.total_len = self.total_len - removed_len + insert_len;
        self.total_chars = self.total_chars - removed_chars + insert_chars;
        self.total_newlines = self.total_newlines - removed_newlines + insert_newlines;
        true
    }

    fn split_piece(&mut self, index: usize, local: u64) -> Result<usize> {
        let Some(piece) = self.pieces.get(index).cloned() else {
            return Ok(index);
        };
        if local == 0 {
            return Ok(index);
        }
        if local >= piece.len {
            return Ok(index + 1);
        }
        let text = self.piece_text(&piece)?;
        let mut local = usize::try_from(local).unwrap_or(text.len()).min(text.len());
        while local > 0 && !text.is_char_boundary(local) {
            local -= 1;
        }
        if local == 0 {
            return Ok(index);
        }
        let local = local as u64;
        if local >= piece.len {
            return Ok(index + 1);
        }
        let prefix = &text[..local as usize];
        let left_chars = prefix.chars().count() as u64;
        let left_newlines = count_newlines(prefix);
        let left = Piece {
            kind: piece.kind.clone(),
            offset: piece.offset,
            len: local,
            chars: left_chars,
            newlines: left_newlines,
            logical_start: piece.logical_start,
            line: piece.line,
            raw: false,
        };
        let right = Piece {
            kind: match piece.kind {
                PieceKind::Source { block } => PieceKind::Source { block },
                PieceKind::Journal { offset } => PieceKind::Journal {
                    offset: offset + local,
                },
            },
            offset: piece.offset + local,
            len: piece.len - local,
            chars: piece.chars.saturating_sub(left_chars),
            newlines: piece.newlines.saturating_sub(left_newlines),
            logical_start: piece.logical_start + local,
            line: piece.line + left_newlines,
            raw: false,
        };
        self.pieces[index] = left;
        self.pieces.insert(index + 1, right);
        Ok(index + 1)
    }

    /// Merge adjacent pieces that came from the same source block or the same
    /// journal run. Fully covered source blocks become raw again, so undoing
    /// every edit restores byte-exact saving.
    fn compact_pieces(&mut self) {
        let mut out: Vec<Piece> = Vec::with_capacity(self.pieces.len());
        for piece in self.pieces.drain(..) {
            if let Some(last) = out.last_mut() {
                let same_block = match (&last.kind, &piece.kind) {
                    (PieceKind::Source { block: a }, PieceKind::Source { block: b }) => a == b,
                    (PieceKind::Journal { .. }, PieceKind::Journal { .. }) => true,
                    _ => false,
                };
                if same_block && last.offset + last.len == piece.offset {
                    last.len += piece.len;
                    last.chars += piece.chars;
                    last.newlines += piece.newlines;
                    if let PieceKind::Source { block } = last.kind {
                        if let Some(block) = self.blocks.get(block as usize) {
                            last.raw = last.offset == 0 && last.len == block.logical_len;
                        }
                    }
                    continue;
                }
            }
            out.push(piece);
        }
        self.pieces = out;
        self.recompute_from(0);
    }

    fn recompute_from(&mut self, index: usize) {
        let (mut logical, mut line) = if index == 0 {
            (0u64, 1u64)
        } else {
            let previous = &self.pieces[index - 1];
            (
                previous.logical_start + previous.len,
                previous.line + previous.newlines,
            )
        };
        for piece in &mut self.pieces[index..] {
            piece.logical_start = logical;
            piece.line = line;
            logical += piece.len;
            line += piece.newlines;
        }
    }

    fn piece_index_at(&self, offset: u64) -> usize {
        self.pieces
            .partition_point(|piece| piece.logical_start + piece.len <= offset)
    }

    fn rebuild_pieces_from_blocks(&mut self) {
        self.pieces.clear();
        let mut total_len = 0u64;
        let mut total_chars = 0u64;
        let mut total_newlines = 0u64;
        for (index, block) in self.blocks.iter().enumerate() {
            total_len += block.logical_len;
            total_chars += block.chars;
            total_newlines += block.newlines;
            if block.logical_len == 0 {
                continue;
            }
            self.pieces.push(Piece {
                kind: PieceKind::Source {
                    block: index as u32,
                },
                offset: 0,
                len: block.logical_len,
                chars: block.chars,
                newlines: block.newlines,
                logical_start: block.logical_start,
                line: block.line,
                raw: true,
            });
        }
        self.total_len = total_len;
        self.total_chars = total_chars;
        self.total_newlines = total_newlines;
    }

    // ------------------------------------------------------------------
    // Search
    // ------------------------------------------------------------------

    /// Find the next match at or after `from` (or before it when `forward` is
    /// false). Scans the document in bounded chunks.
    pub fn find(
        &self,
        engine: &SearchEngine,
        from: u64,
        forward: bool,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> Result<Option<SearchHit>> {
        if self.total_len == 0 {
            return Ok(None);
        }
        if forward {
            let start = from.min(self.total_len);
            if let Some(hit) = self.scan_forward(engine, start, cancel, progress)? {
                return Ok(Some(hit));
            }
            if start > 0 {
                // Wrap around to the beginning.
                return self.scan_forward(engine, 0, cancel, progress);
            }
            Ok(None)
        } else {
            let until = from.min(self.total_len);
            self.scan_backward(engine, until, cancel, progress)
        }
    }

    fn scan_forward(
        &self,
        engine: &SearchEngine,
        from: u64,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> Result<Option<SearchHit>> {
        let mut position = from;
        while position < self.total_len {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let chunk_end = position
                .saturating_add(SEARCH_CHUNK_BYTES)
                .min(self.total_len);
            let read_end = chunk_end
                .saturating_add(SEARCH_OVERLAP_BYTES)
                .min(self.total_len);
            let text = self.collect_text(position, read_end - position)?;
            let limit = usize::try_from(chunk_end - position).unwrap_or(text.len());
            if let Some(found) = engine.find_first_in(&text, 0) {
                if found.start < limit {
                    let start = position + found.start as u64;
                    return Ok(Some(SearchHit {
                        start,
                        end: position + found.end as u64,
                        line: self.line_at(start)?,
                    }));
                }
            }
            progress.store(chunk_end, Ordering::Relaxed);
            position = chunk_end;
        }
        progress.store(self.total_len, Ordering::Relaxed);
        Ok(None)
    }

    fn scan_backward(
        &self,
        engine: &SearchEngine,
        until: u64,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> Result<Option<SearchHit>> {
        let mut position = 0u64;
        let mut last = None;
        while position < until {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let chunk_end = position.saturating_add(SEARCH_CHUNK_BYTES).min(until);
            let read_end = chunk_end
                .saturating_add(SEARCH_OVERLAP_BYTES)
                .min(self.total_len);
            let text = self.collect_text(position, read_end - position)?;
            let limit = usize::try_from(chunk_end - position).unwrap_or(text.len());
            if let Some(found) = engine.find_last_in(&text, limit) {
                let start = position + found.start as u64;
                if start < until {
                    last = Some(SearchHit {
                        start,
                        end: position + found.end as u64,
                        line: self.line_at(start)?,
                    });
                }
            }
            progress.store(chunk_end, Ordering::Relaxed);
            position = chunk_end;
        }
        Ok(last)
    }

    /// Count matches without materialising the whole document.
    pub fn count_matches(
        &self,
        engine: &SearchEngine,
        limit: usize,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> Result<Vec<SearchHit>> {
        let mut hits = Vec::new();
        let mut position = 0u64;
        while position < self.total_len && hits.len() < limit {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let chunk_end = position
                .saturating_add(SEARCH_CHUNK_BYTES)
                .min(self.total_len);
            let read_end = chunk_end
                .saturating_add(SEARCH_OVERLAP_BYTES)
                .min(self.total_len);
            let text = self.collect_text(position, read_end - position)?;
            let limit_bytes = usize::try_from(chunk_end - position).unwrap_or(text.len());
            for found in engine.find_all_bytes(&text, usize::MAX) {
                if found.start >= limit_bytes {
                    break;
                }
                let start = position + found.start as u64;
                hits.push(SearchHit {
                    start,
                    end: position + found.end as u64,
                    line: self.line_at(start)?,
                });
                if hits.len() >= limit {
                    break;
                }
            }
            progress.store(chunk_end, Ordering::Relaxed);
            position = chunk_end;
        }
        Ok(hits)
    }

    /// Replace every match, applying the edits in bounded batches. On cancel
    /// or error the complete operation is rolled back.
    pub fn replace_all(
        &mut self,
        engine: &SearchEngine,
        replacement: &str,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> Result<ReplaceReport> {
        // A streaming replace-all owns the editing lock itself; only a
        // read-only source rejects it.
        if self.read_only {
            return Err(Error::Other(tr("core.document.read_only").to_string()));
        }
        let mut hunks = Vec::new();
        let mut count = 0usize;
        let mut position = 0u64;
        let mut cancelled = false;
        let mut failure: Option<Error> = None;
        while position < self.total_len {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            let chunk_end = position
                .saturating_add(SEARCH_CHUNK_BYTES)
                .min(self.total_len);
            let read_end = chunk_end
                .saturating_add(SEARCH_OVERLAP_BYTES)
                .min(self.total_len);
            let text = match self.collect_text(position, read_end - position) {
                Ok(text) => text,
                Err(err) => {
                    failure = Some(err);
                    break;
                }
            };
            let limit = usize::try_from(chunk_end - position).unwrap_or(text.len());
            let matches: Vec<_> = engine
                .find_all_bytes(&text, 4_000_000)
                .into_iter()
                .filter(|found| found.start < limit)
                .collect();
            match self.apply_replacement_batch(position, &text, &matches, engine, replacement) {
                Ok((batch_hunks, batch_count, batch_delta)) => {
                    hunks.extend(batch_hunks);
                    count += batch_count;
                    position = (chunk_end as i64 + batch_delta).max(0) as u64;
                }
                Err(err) => {
                    failure = Some(err);
                    break;
                }
            }
            progress.store(position.min(self.total_len), Ordering::Relaxed);
        }
        if cancelled || failure.is_some() {
            // Roll back every applied batch.
            for hunk in hunks.iter().rev() {
                self.splice_hunk(hunk, true);
            }
            self.compact_pieces();
            return match failure {
                Some(err) => Err(err),
                None => Ok(ReplaceReport {
                    count: 0,
                    cancelled: true,
                    bytes_scanned: position,
                }),
            };
        }
        if hunks.is_empty() {
            return Ok(ReplaceReport {
                count: 0,
                cancelled: false,
                bytes_scanned: self.total_len,
            });
        }
        self.push_history(hunks, 0, 0, 0);
        Ok(ReplaceReport {
            count,
            cancelled: false,
            bytes_scanned: self.total_len,
        })
    }

    fn apply_replacement_batch(
        &mut self,
        position: u64,
        text: &str,
        matches: &[crate::search::ByteMatch],
        engine: &SearchEngine,
        replacement: &str,
    ) -> Result<(Vec<Hunk>, usize, i64)> {
        let mut hunks = Vec::new();
        let mut count = 0usize;
        let mut delta: i64 = 0;
        let mut index = 0usize;
        while index < matches.len() {
            let range_start = matches[index].start;
            let mut range_end = matches[index].end;
            let mut end_index = index + 1;
            while end_index < matches.len() {
                let gap = matches[end_index].start.saturating_sub(range_end);
                if gap > 64 * 1024 {
                    break;
                }
                range_end = range_end.max(matches[end_index].end);
                end_index += 1;
            }
            let mut replaced = String::new();
            let mut last = range_start;
            for matched in &matches[index..end_index] {
                replaced.push_str(&text[last..matched.start]);
                replaced.push_str(&engine.replacement_for_bytes(text, *matched, replacement));
                last = matched.end;
            }
            replaced.push_str(&text[last..range_end]);
            let start = (position as i64 + range_start as i64 + delta).max(0) as u64;
            let old_len = (range_end - range_start) as u64;
            let hunk = self.apply_hunk(start, old_len, &replaced)?;
            delta += replaced.len() as i64 - old_len as i64;
            count += end_index - index;
            hunks.push(hunk);
            index = end_index;
        }
        Ok((hunks, count, delta))
    }

    // ------------------------------------------------------------------
    // Save / reload
    // ------------------------------------------------------------------

    /// Rebase the engine on a file just produced by a save.
    pub fn adopt_saved(&mut self, report: SaveReport, path: PathBuf) -> Result<()> {
        let base = Arc::new(BaseFile::open(&path)?);
        self.path = path;
        self.base = base;
        self.base_len = report.size;
        self.body_len = report.size.saturating_sub(self.bom_len);
        self.base_modified = report.modified;
        self.read_only = report.read_only;
        self.editing_locked = false;
        self.blocks = report.blocks;
        self.journal.reset()?;
        self.history.clear();
        self.history_pos = 0;
        self.history_bytes = 0;
        self.history_trimmed = false;
        self.rebuild_pieces_from_blocks();
        self.revision = self.revision.wrapping_add(1);
        if let Ok(mut cache) = self.cache.lock() {
            cache.map.clear();
            cache.order.clear();
        }
        Ok(())
    }

    /// Re-index the file from scratch, discarding edits.
    pub fn reload(&mut self, cancel: &AtomicBool, progress: &AtomicU64) -> Result<()> {
        progress.store(0, Ordering::Relaxed);
        let probe = Self::probe(&self.path)?;
        if probe.non_text {
            return Err(Error::Other(tr("core.disk_engine.not_text").to_string()));
        }
        let base = Arc::new(BaseFile::open(&self.path)?);
        let bom_len = detect_bom_len(&base, probe.encoding, probe.size)?;
        let blocks = index_blocks(&base, bom_len, probe.size, probe.encoding, cancel, progress)?;
        self.encoding = probe.encoding;
        self.eol = probe.eol;
        self.bom_len = bom_len;
        self.base_len = probe.size;
        self.body_len = probe.size.saturating_sub(bom_len);
        self.base_modified = probe.last_modified;
        self.read_only = probe.read_only;
        self.base = base;
        self.blocks = blocks;
        self.journal.reset()?;
        self.history.clear();
        self.history_pos = 0;
        self.history_bytes = 0;
        self.history_trimmed = false;
        self.rebuild_pieces_from_blocks();
        self.revision = self.revision.wrapping_add(1);
        if let Ok(mut cache) = self.cache.lock() {
            cache.map.clear();
            cache.order.clear();
        }
        progress.store(probe.size, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn bom_len(&self) -> u64 {
        self.bom_len
    }
}

fn read_head(path: &Path, size: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut file = File::open(path)?;
    let mut head = vec![0u8; usize::try_from(size.min(64 * 1024)).unwrap_or(0)];
    let mut filled = 0usize;
    while filled < head.len() {
        let read = file.read(&mut head[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    head.truncate(filled);
    Ok(head)
}

fn detect_bom_len(base: &BaseFile, encoding: TextEncoding, size: u64) -> Result<u64> {
    let bom = encoding.bom();
    if bom.is_empty() || size < bom.len() as u64 {
        return Ok(0);
    }
    let head = base.read(0, bom.len() as u64)?;
    Ok(if head == bom { bom.len() as u64 } else { 0 })
}

fn decode_body(bytes: &[u8], encoding: TextEncoding) -> String {
    let (text, _) = encoding.codec().decode_without_bom_handling(bytes);
    text.into_owned()
}

fn count_newlines(text: &str) -> u64 {
    text.as_bytes()
        .iter()
        .filter(|byte| **byte == b'\n')
        .count() as u64
}

/// Count characters and line breaks in one pass. Valid UTF-8 has one leading
/// byte per scalar value, so counting non-continuation bytes avoids a full
/// char decode over every block.
fn count_chars_newlines(text: &str) -> (u64, u64) {
    let mut chars = 0u64;
    let mut newlines = 0u64;
    for &byte in text.as_bytes() {
        if byte & 0xC0 != 0x80 {
            chars += 1;
        }
        if byte == b'\n' {
            newlines += 1;
        }
    }
    (chars, newlines)
}

fn clamp_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn shift_saturating(value: u64, delta: i64) -> u64 {
    if delta >= 0 {
        value.saturating_add(delta as u64)
    } else {
        value.saturating_sub(delta.unsigned_abs())
    }
}

fn index_blocks(
    base: &BaseFile,
    bom_len: u64,
    size: u64,
    encoding: TextEncoding,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> Result<Vec<Block>> {
    let body_len = size.saturating_sub(bom_len);
    let mut blocks = Vec::new();
    let mut buffer: Vec<u8> = Vec::with_capacity(BLOCK_TARGET_BYTES + 2 * READ_CHUNK_BYTES);
    let mut begin = 0usize;
    let mut consumed = 0u64;
    let mut read_pos = 0u64;
    let mut eof = body_len == 0;
    let mut logical_start = 0u64;
    let mut line = 1u64;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        // Keep at least one target block plus a chunk buffered.
        while buffer.len() - begin < BLOCK_TARGET_BYTES + READ_CHUNK_BYTES && !eof {
            let want = (READ_CHUNK_BYTES as u64).min(body_len - read_pos) as usize;
            if want == 0 {
                eof = true;
                break;
            }
            let chunk = base.read(bom_len + read_pos, want as u64)?;
            if chunk.is_empty() {
                eof = true;
                break;
            }
            read_pos += chunk.len() as u64;
            buffer.extend_from_slice(&chunk);
            progress.store(bom_len + read_pos, Ordering::Relaxed);
            if read_pos >= body_len {
                eof = true;
            }
        }
        let available = buffer.len() - begin;
        if available == 0 {
            break;
        }
        let view = &buffer[begin..];
        let target = BLOCK_TARGET_BYTES.min(available);
        let boundary = choose_boundary(view, target, encoding).clamp(1, available);
        let raw = &buffer[begin..begin + boundary];
        let text = eol::to_lf_owned(decode_body(raw, encoding));
        let (block_chars, newlines) = count_chars_newlines(&text);
        let logical_len = text.len() as u64;
        blocks.push(Block {
            src_offset: consumed,
            src_len: boundary as u64,
            logical_start,
            logical_len,
            chars: block_chars,
            line,
            newlines,
        });
        logical_start += logical_len;
        line += newlines;
        consumed += boundary as u64;
        begin += boundary;
        // Compact only occasionally so indexing does not memmove every block.
        if begin >= 16 * 1024 * 1024 {
            buffer.drain(..begin);
            begin = 0;
        }
    }

    progress.store(size, Ordering::Relaxed);
    Ok(blocks)
}

/// Prefer the last complete line inside the target; fall back to a character
/// boundary when a single line is longer than a block.
fn choose_boundary(buffer: &[u8], target: usize, encoding: TextEncoding) -> usize {
    if target >= buffer.len() {
        return buffer.len();
    }
    if let Some(end) = newline_before(buffer, target, encoding) {
        return end;
    }
    char_boundary(buffer, target, encoding)
}

fn newline_before(buffer: &[u8], target: usize, encoding: TextEncoding) -> Option<usize> {
    match encoding {
        TextEncoding::Utf16Le => newline_unit_before(buffer, target, true),
        TextEncoding::Utf16Be => newline_unit_before(buffer, target, false),
        _ => {
            let mut index = target.min(buffer.len());
            while index > 0 {
                index -= 1;
                match buffer[index] {
                    b'\n' => return Some(index + 1),
                    b'\r' => {
                        let end = if buffer.get(index + 1) == Some(&b'\n') {
                            index + 2
                        } else {
                            index + 1
                        };
                        return Some(end);
                    }
                    _ => {}
                }
            }
            None
        }
    }
}

fn newline_unit_before(buffer: &[u8], target: usize, little_endian: bool) -> Option<usize> {
    let mut index = (target.min(buffer.len()) & !1).saturating_sub(2);
    loop {
        let unit = if little_endian {
            u16::from_le_bytes([buffer[index], buffer[index + 1]])
        } else {
            u16::from_be_bytes([buffer[index], buffer[index + 1]])
        };
        if unit == b'\n' as u16 {
            return Some(index + 2);
        }
        if unit == b'\r' as u16 {
            let mut end = index + 2;
            if end + 1 < buffer.len() {
                let next = if little_endian {
                    u16::from_le_bytes([buffer[end], buffer[end + 1]])
                } else {
                    u16::from_be_bytes([buffer[end], buffer[end + 1]])
                };
                if next == b'\n' as u16 {
                    end += 2;
                }
            }
            return Some(end);
        }
        if index < 2 {
            return None;
        }
        index -= 2;
    }
}

fn char_boundary(buffer: &[u8], target: usize, encoding: TextEncoding) -> usize {
    let target = target.min(buffer.len());
    if target == 0 {
        return 0;
    }
    match encoding {
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            if buffer.len() >= 2 && target == buffer.len() && !target.is_multiple_of(2) {
                target - 1
            } else {
                target & !1
            }
        }
        TextEncoding::Utf8 | TextEncoding::Utf8Bom => {
            let mut index = target;
            while index > 0 && index < buffer.len() && (buffer[index] & 0xC0) == 0x80 {
                index -= 1;
            }
            if index == 0 {
                index = target.min(buffer.len());
                while index < buffer.len() && (buffer[index] & 0xC0) == 0x80 {
                    index += 1;
                }
            }
            index
        }
        TextEncoding::Windows1252 | TextEncoding::Iso88591 => target,
        _ => {
            let codec = encoding.codec();
            let lowest = target.saturating_sub(4);
            for candidate in (lowest..=target).rev() {
                let mut decoder = codec.new_decoder_without_bom_handling();
                let mut out = String::new();
                let (_, read, _) = decoder.decode_to_string(&buffer[..candidate], &mut out, false);
                if read == candidate {
                    return candidate;
                }
            }
            target
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchOptions;
    use std::io::Write;

    fn write_file(name: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        (dir, path)
    }

    fn open(path: &Path) -> DiskEngine {
        DiskEngine::open(path, &AtomicBool::new(false), &AtomicU64::new(0)).unwrap()
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn indexes_and_reads_windows() {
        let (_dir, path) = write_file("a.txt", b"one\ntwo\nthree\nfour\n");
        let engine = open(&path);
        assert_eq!(engine.total_lines(), 5);
        assert_eq!(engine.total_len(), 19);
        let window = engine.read_window(0, 1024).unwrap();
        assert_eq!(window.text, "one\ntwo\nthree\nfour\n");
        assert_eq!(window.line, 1);
        assert!(window.at_eof);
        assert!(!window.starts_mid_line);
        let window = engine.read_window_from_line(3, 1024).unwrap();
        assert_eq!(window.text, "three\nfour\n");
        assert_eq!(window.line, 3);
    }

    #[test]
    fn edits_shift_lines_and_offsets() {
        let (_dir, path) = write_file("b.txt", b"alpha\nbeta\ngamma\n");
        let mut engine = open(&path);
        engine.apply_edit(6, 0, "B").unwrap();
        assert_eq!(
            engine.read_text(0, engine.total_len()).unwrap(),
            "alpha\nBbeta\ngamma\n"
        );
        assert!(engine.is_modified());
        assert_eq!(engine.line_at(7).unwrap(), 2);
        let outcome = engine.undo().unwrap();
        assert_eq!(outcome.offset, 6);
        assert_eq!(
            engine.read_text(0, engine.total_len()).unwrap(),
            "alpha\nbeta\ngamma\n"
        );
        assert!(!engine.is_modified());
        engine.redo().unwrap();
        assert_eq!(
            engine.read_text(0, engine.total_len()).unwrap(),
            "alpha\nBbeta\ngamma\n"
        );
    }

    #[test]
    fn undo_restores_the_original_text() {
        let original = "first\nsecond\nthird\nfourth\n";
        let (_dir, path) = write_file("c.txt", original.as_bytes());
        let mut engine = open(&path);
        engine.apply_edit(0, 0, "TOP\n").unwrap();
        engine.apply_edit(10, 6, "SECOND").unwrap();
        let end = engine.total_len();
        engine.apply_edit(end, 0, "TAIL\n").unwrap();
        while engine.undo().is_some() {}
        assert_eq!(engine.read_text(0, engine.total_len()).unwrap(), original);
        assert!(!engine.is_modified());
    }

    #[test]
    fn windows_do_not_split_multibyte_characters() {
        let (_dir, path) = write_file("d.txt", "héllo\n世界\n".as_bytes());
        let engine = open(&path);
        let window = engine.read_window(0, 8).unwrap();
        assert!(window.text.len() <= 8);
        assert!(window.text.is_char_boundary(window.text.len()));
        assert!(window.text.starts_with('h'));
    }

    #[test]
    fn supports_utf16_and_crlf() {
        let text = "alpha\r\nbeta\r\ngamma\r\n";
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let (_dir, path) = write_file("e.txt", &bytes);
        let engine = open(&path);
        assert_eq!(engine.encoding(), TextEncoding::Utf16Le);
        assert_eq!(engine.eol(), Eol::CrLf);
        assert_eq!(engine.total_lines(), 4);
        assert_eq!(
            engine.read_text(0, engine.total_len()).unwrap(),
            "alpha\nbeta\ngamma\n"
        );
        let window = engine.read_window_from_line(2, 1024).unwrap();
        assert_eq!(window.text, "beta\ngamma\n");
    }

    #[test]
    fn long_lines_split_at_character_boundaries() {
        let mut bytes = vec![b'a'; 3 * 1024 * 1024];
        bytes.push(b'\n');
        let (_dir, path) = write_file("f.txt", &bytes);
        let engine = open(&path);
        assert!(engine.blocks.len() >= 3);
        assert_eq!(engine.total_len(), 3 * 1024 * 1024 + 1);
        assert_eq!(engine.total_lines(), 2);
        for block in &engine.blocks {
            assert!(block.logical_len <= BLOCK_TARGET_BYTES as u64);
        }
    }

    #[test]
    fn finds_and_replaces_literal_text() {
        let (_dir, path) = write_file("g.txt", b"aaa\nbbb\naaa\n");
        let mut engine = open(&path);
        let search = SearchEngine::new("aaa", SearchOptions::default());
        let hit = engine
            .find(&search, 1, true, &no_cancel(), &AtomicU64::new(0))
            .unwrap()
            .unwrap();
        assert_eq!(hit.start, 8);
        assert_eq!(hit.line, 3);
        let report = engine
            .replace_all(&search, "x", &no_cancel(), &AtomicU64::new(0))
            .unwrap();
        assert_eq!(report.count, 2);
        assert_eq!(
            engine.read_text(0, engine.total_len()).unwrap(),
            "x\nbbb\nx\n"
        );
    }

    #[test]
    fn replace_all_can_be_undone_with_one_step() {
        let (_dir, path) = write_file("h.txt", b"one one one\n");
        let mut engine = open(&path);
        let search = SearchEngine::new("one", SearchOptions::default());
        engine
            .replace_all(&search, "1", &no_cancel(), &AtomicU64::new(0))
            .unwrap();
        assert_eq!(engine.read_text(0, engine.total_len()).unwrap(), "1 1 1\n");
        engine.undo().unwrap();
        assert_eq!(
            engine.read_text(0, engine.total_len()).unwrap(),
            "one one one\n"
        );
    }

    #[test]
    fn save_round_trips_and_preserves_encoding() {
        let original = "héllo\r\nworld\r\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.txt");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(original.as_bytes());
        fs::write(&path, &bytes).unwrap();
        let mut engine = open(&path);
        engine.apply_edit(2, 0, "X").unwrap();
        let snapshot = engine.snapshot();
        let temp = dir.path().join("out.txt");
        let report = snapshot
            .write(&temp, false, &AtomicU64::new(0), &no_cancel())
            .unwrap();
        engine.adopt_saved(report, temp.clone()).unwrap();
        assert!(!engine.is_modified());
        let written = fs::read(&temp).unwrap();
        assert!(written.starts_with(&[0xEF, 0xBB, 0xBF]));
        let reopened = open(&temp);
        assert_eq!(
            reopened.read_text(0, reopened.total_len()).unwrap(),
            "hXéllo\nworld\n"
        );
        assert_eq!(reopened.eol(), Eol::CrLf);
    }

    #[test]
    fn save_copies_unmodified_blocks_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.txt");
        let mut data = vec![b'a'; BLOCK_TARGET_BYTES + 10];
        data[BLOCK_TARGET_BYTES..].copy_from_slice(b"\r\nsecond\r\n");
        fs::write(&path, &data).unwrap();
        let mut engine = open(&path);
        engine.apply_edit(0, 0, "X").unwrap();
        let snapshot = engine.snapshot();
        let temp = dir.path().join("out.txt");
        snapshot
            .write(&temp, false, &AtomicU64::new(0), &no_cancel())
            .unwrap();
        let written = fs::read(&temp).unwrap();
        assert_eq!(written[0], b'X');
        assert_eq!(&written[1..], &data[..]);
    }

    #[test]
    fn goto_line_and_column() {
        let (_dir, path) = write_file("goto.txt", b"one\ntwo\nthree");
        let engine = open(&path);
        assert_eq!(engine.offset_for_line(1, 1).unwrap(), 0);
        assert_eq!(engine.offset_for_line(2, 2).unwrap(), 5);
        assert_eq!(engine.offset_for_line(3, 4).unwrap(), 11);
        assert_eq!(engine.line_col_at(11).unwrap(), (3, 4));
        assert_eq!(engine.total_lines(), 3);
        assert_eq!(engine.offset_for_line(99, 1).unwrap(), 8);
        assert_eq!(engine.offset_for_line(3, 99).unwrap(), engine.total_len());
    }

    #[test]
    fn window_at_eof_starts_on_a_character_boundary() {
        let text = "alpha\n世界世界世界";
        let (_dir, path) = write_file("utf8.txt", text.as_bytes());
        let engine = open(&path);
        let window = engine.read_window(engine.total_len(), 8).unwrap();
        assert!(engine.total_len() >= window.start);
        assert!(text.is_char_boundary(window.start as usize));
        assert!(text[window.start as usize..].is_char_boundary(0));
        assert!(window.text.is_char_boundary(window.text.len()));
        assert!(window.at_eof);
    }

    #[test]
    fn long_lines_can_be_traversed_with_offset_windows() {
        let mut data = vec![b'x'; 5 * 1024 * 1024];
        data.extend_from_slice(b"\nend\n");
        let (_dir, path) = write_file("long.txt", &data);
        let engine = open(&path);
        assert_eq!(engine.total_lines(), 3);
        let middle = engine.read_window(2 * 1024 * 1024, 1024 * 1024).unwrap();
        assert_eq!(middle.start, 2 * 1024 * 1024);
        assert_eq!(middle.line, 1);
        assert!(middle.starts_mid_line);
        assert_eq!(middle.start_column, 2 * 1024 * 1024 + 1);
        let repeated: String = engine.read_text(0, engine.total_len()).unwrap();
        assert_eq!(repeated.len(), 5 * 1024 * 1024 + 5);
        assert!(repeated.ends_with("xxx\nend\n"));
    }

    #[test]
    fn long_lines_resolve_to_their_first_character() {
        let mut data = vec![b'x'; 3 * 1024 * 1024];
        data.extend_from_slice(b"\nsecond\n");
        let (_dir, path) = write_file("longline.txt", &data);
        let engine = open(&path);
        assert!(engine.blocks.len() >= 3);
        // Line 1 spans several pieces; line 1/column 1 must be the file start,
        // not the start of the piece that happens to contain the last chunk.
        assert_eq!(engine.offset_for_line(1, 1).unwrap(), 0);
        assert_eq!(engine.offset_for_line(1, 2).unwrap(), 1);
        assert_eq!(
            engine.offset_for_line(1, 3 * 1024 * 1024).unwrap(),
            3 * 1024 * 1024 - 1
        );
        // Columns beyond the 4 MiB chunk used for scanning still resolve.
        let long_start = 3 * 1024 * 1024;
        assert_eq!(
            engine.offset_for_line(1, 3 * 1024 * 1024 + 1).unwrap(),
            long_start as u64
        );
        // The next line starts after the newline and is not confused with the
        // over-long previous line.
        assert_eq!(engine.offset_for_line(2, 1).unwrap(), long_start as u64 + 1);
        assert_eq!(engine.offset_for_line(2, 4).unwrap(), long_start as u64 + 4);
        assert_eq!(engine.offset_for_line(3, 1).unwrap(), long_start as u64 + 8);
    }

    #[test]
    fn multibyte_text_survives_block_splits() {
        let mut text = String::new();
        while text.len() < 3 * 1024 * 1024 {
            text.push_str("世界和平 héllo\n");
        }
        let (_dir, path) = write_file("blocks.txt", text.as_bytes());
        let engine = open(&path);
        assert!(engine.blocks.len() >= 3);
        let read = engine.read_text(0, engine.total_len()).unwrap();
        assert_eq!(read, text);
        for block in &engine.blocks {
            assert!(text.is_char_boundary(block.logical_start as usize));
        }
    }

    #[test]
    fn undo_across_windows_restores_the_original() {
        let mut text = String::new();
        while text.len() < 2 * 1024 * 1024 {
            text.push_str("line with some text\n");
        }
        let (_dir, path) = write_file("undo.txt", text.as_bytes());
        let mut engine = open(&path);
        let middle = engine.total_len() / 2;
        engine.apply_edit(0, 0, "START\n").unwrap();
        engine.apply_edit(middle, 10, "").unwrap();
        let end = engine.total_len();
        engine.apply_edit(end, 0, "\nEND").unwrap();
        let edited = engine.read_text(0, engine.total_len()).unwrap();
        assert_ne!(edited, text);
        while engine.undo().is_some() {}
        assert_eq!(engine.read_text(0, engine.total_len()).unwrap(), text);
        assert!(!engine.is_modified());
        assert!(!engine.can_undo());
        assert!(engine.can_redo());
        while engine.redo().is_some() {}
        assert_eq!(engine.read_text(0, engine.total_len()).unwrap(), edited);
    }

    #[test]
    fn trimmed_history_keeps_the_document_dirty() {
        let (_dir, path) = write_file("trim.txt", b"seed\n");
        let mut engine = open(&path);
        for _ in 0..1005 {
            let end = engine.total_len();
            engine.apply_edit(end, 0, "x").unwrap();
        }
        assert!(engine.history_trimmed());
        assert!(engine.is_modified());
        while engine.undo().is_some() {}
        // Five oldest insertions were dropped from the history but remain in
        // the document; it must stay dirty so saving cannot be skipped.
        assert!(engine.is_modified());
        assert_eq!(
            engine.read_text(0, engine.total_len()).unwrap(),
            "seed\nxxxxx"
        );
    }

    #[test]
    fn regex_replace_all_expands_captures() {
        let (_dir, path) = write_file("regex.txt", b"one 1\ntwo 2\n");
        let mut engine = open(&path);
        let search = SearchEngine::new(
            r"(\w+) (\d)",
            SearchOptions {
                regex: true,
                ..SearchOptions::default()
            },
        );
        let report = engine
            .replace_all(&search, "$2-$1", &no_cancel(), &AtomicU64::new(0))
            .unwrap();
        assert_eq!(report.count, 2);
        assert_eq!(
            engine.read_text(0, engine.total_len()).unwrap(),
            "1-one\n2-two\n"
        );
        engine.undo().unwrap();
        assert_eq!(
            engine.read_text(0, engine.total_len()).unwrap(),
            "one 1\ntwo 2\n"
        );
    }

    #[test]
    fn saves_utf16_with_edit_and_keeps_the_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf16.txt");
        let text = "alpha\r\n世界\r\n";
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        fs::write(&path, &bytes).unwrap();
        let mut engine = open(&path);
        let offset = engine.offset_for_line(2, 1).unwrap();
        engine.apply_edit(offset, 0, "edited ").unwrap();
        let temp = dir.path().join("out16.txt");
        let report = engine
            .snapshot()
            .write(&temp, false, &AtomicU64::new(0), &no_cancel())
            .unwrap();
        engine.adopt_saved(report, temp.clone()).unwrap();
        let written = fs::read(&temp).unwrap();
        assert!(written.starts_with(&[0xFF, 0xFE]));
        let reopened = open(&temp);
        assert_eq!(reopened.encoding(), TextEncoding::Utf16Le);
        assert_eq!(reopened.eol(), Eol::CrLf);
        assert_eq!(
            reopened.read_text(0, reopened.total_len()).unwrap(),
            "alpha\nedited 世界\n"
        );
    }

    #[test]
    fn saves_utf16_be_with_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf16be.txt");
        let text = "alpha\n世界\n";
        let mut bytes = vec![0xFE, 0xFF];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        fs::write(&path, &bytes).unwrap();
        let mut engine = open(&path);
        assert_eq!(engine.encoding(), TextEncoding::Utf16Be);
        engine.apply_edit(0, 0, "X").unwrap();
        let temp = dir.path().join("out-be.txt");
        let report = engine
            .snapshot()
            .write(&temp, false, &AtomicU64::new(0), &no_cancel())
            .unwrap();
        engine.adopt_saved(report, temp.clone()).unwrap();
        let written = fs::read(&temp).unwrap();
        assert!(written.starts_with(&[0xFE, 0xFF]));
        let reopened = open(&temp);
        assert_eq!(reopened.encoding(), TextEncoding::Utf16Be);
        assert_eq!(
            reopened.read_text(0, reopened.total_len()).unwrap(),
            "Xalpha\n世界\n"
        );
    }

    #[test]
    fn bomless_utf16_stays_bomless_after_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf16le.txt");
        let text = "hello\nworld\n";
        let mut bytes = Vec::new();
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        fs::write(&path, &bytes).unwrap();
        let mut engine = open(&path);
        assert_eq!(engine.encoding(), TextEncoding::Utf16Le);
        engine.apply_edit(0, 0, "X").unwrap();
        let temp = dir.path().join("out-le.txt");
        engine
            .snapshot()
            .write(&temp, false, &AtomicU64::new(0), &no_cancel())
            .unwrap();
        let written = fs::read(&temp).unwrap();
        assert!(!written.starts_with(&[0xFF, 0xFE]));
        let reopened = open(&temp);
        assert_eq!(
            reopened.read_text(0, reopened.total_len()).unwrap(),
            "Xhello\nworld\n"
        );
    }

    #[test]
    fn saves_windows1252_with_cr_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ansi.txt");
        let original = b"caf\xE9\ravec\rtoi\r";
        fs::write(&path, original).unwrap();
        let mut engine = open(&path);
        assert_eq!(engine.encoding(), TextEncoding::Windows1252);
        assert_eq!(engine.eol(), Eol::Cr);
        engine.apply_edit(0, 0, "le ").unwrap();
        let temp = dir.path().join("out-ansi.txt");
        let report = engine
            .snapshot()
            .write(&temp, false, &AtomicU64::new(0), &no_cancel())
            .unwrap();
        engine.adopt_saved(report, temp.clone()).unwrap();
        let written = fs::read(&temp).unwrap();
        assert_eq!(written, b"le caf\xE9\ravec\rtoi\r");
    }

    #[test]
    fn empty_files_can_be_edited_and_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        fs::write(&path, b"").unwrap();
        let mut engine = open(&path);
        assert_eq!(engine.total_len(), 0);
        assert_eq!(engine.total_lines(), 1);
        assert_eq!(engine.read_window(0, 1024).unwrap().text, "");
        // An empty file has no line-ending evidence, so it falls back to the
        // platform convention.
        assert_eq!(engine.eol(), Eol::native());
        engine.apply_edit(0, 0, "hello\n").unwrap();
        assert_eq!(engine.total_lines(), 2);
        let temp = dir.path().join("empty-out.txt");
        engine
            .snapshot()
            .write(&temp, false, &AtomicU64::new(0), &no_cancel())
            .unwrap();
        let expected = eol::normalize("hello\n", Eol::native());
        assert_eq!(fs::read(&temp).unwrap(), expected.as_bytes());
    }

    #[test]
    fn files_without_a_trailing_newline_keep_their_last_line() {
        let (_dir, path) = write_file("tail.txt", b"first\nlast");
        let engine = open(&path);
        assert_eq!(engine.total_lines(), 2);
        assert_eq!(engine.offset_for_line(2, 1).unwrap(), 6);
        let window = engine.read_window_from_line(2, 1024).unwrap();
        assert_eq!(window.text, "last");
        assert!(window.at_eof);
        assert!(!window.ends_mid_line);
    }

    #[test]
    fn read_only_files_reject_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.txt");
        fs::write(&path, b"read only\n").unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).unwrap();
        let mut engine = open(&path);
        assert!(engine.is_read_only());
        assert!(engine.apply_edit(0, 0, "x").is_err());
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(&path, permissions).unwrap();
    }

    #[test]
    fn reload_discards_edits_and_reads_the_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reload.txt");
        fs::write(&path, b"first\n").unwrap();
        let mut engine = open(&path);
        engine.apply_edit(0, 0, "x").unwrap();
        fs::write(&path, b"second\nline\n").unwrap();
        engine.reload(&no_cancel(), &AtomicU64::new(0)).unwrap();
        assert!(!engine.is_modified());
        assert_eq!(engine.total_lines(), 3);
        assert_eq!(
            engine.read_text(0, engine.total_len()).unwrap(),
            "second\nline\n"
        );
    }

    #[test]
    fn external_changes_are_detected_before_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("change.txt");
        fs::write(&path, b"original\n").unwrap();
        let mut engine = open(&path);
        engine.apply_edit(0, 0, "x").unwrap();
        fs::write(&path, b"changed by someone else\n").unwrap();
        assert!(engine.changed_on_disk());
        let temp = dir.path().join("out.txt");
        let error = engine
            .snapshot()
            .write(&temp, false, &AtomicU64::new(0), &no_cancel())
            .unwrap_err();
        assert!(error.to_string().contains("changed"));
        assert!(!temp.exists());
        assert_eq!(fs::read(&path).unwrap(), b"changed by someone else\n");
    }

    #[test]
    fn force_save_after_an_external_replace_keeps_the_indexed_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("force.txt");
        fs::write(&path, b"original\n").unwrap();
        let mut engine = open(&path);
        engine.apply_edit(0, 0, "x").unwrap();
        let replacement = dir.path().join("replacement.txt");
        fs::write(&replacement, b"changed by someone else\n").unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert!(engine.changed_on_disk());
        let snapshot = engine.snapshot();
        let temp = dir.path().join("out.txt");
        let error = snapshot
            .write(&temp, false, &AtomicU64::new(0), &no_cancel())
            .unwrap_err();
        assert!(error.to_string().contains("changed"));
        assert!(!temp.exists());
        // The user confirmed overwriting: the open-time handle still reads the
        // indexed bytes, so the external replacement is discarded.
        let report = snapshot
            .write(&path, true, &AtomicU64::new(0), &no_cancel())
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"xoriginal\n");
        engine.adopt_saved(report, path.clone()).unwrap();
        assert_eq!(
            engine.read_text(0, engine.total_len()).unwrap(),
            "xoriginal\n"
        );
        assert!(!engine.is_modified());
    }

    #[test]
    fn save_as_from_a_read_only_source_is_writable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readonly.txt");
        fs::write(&path, b"hello\n").unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).unwrap();
        let engine = open(&path);
        assert!(engine.is_read_only());
        let target = dir.path().join("copy.txt");
        engine
            .snapshot()
            .write(&target, false, &AtomicU64::new(0), &no_cancel())
            .unwrap();
        assert!(!fs::metadata(&target).unwrap().permissions().readonly());
        assert_eq!(fs::read(&target).unwrap(), b"hello\n");
    }

    #[test]
    fn crlf_edits_keep_unmodified_line_endings() {
        let original = "one\r\ntwo\r\nthree\r\n";
        let (_dir, path) = write_file("crlf.txt", original.as_bytes());
        let mut engine = open(&path);
        engine.apply_edit(3, 0, "!").unwrap();
        let temp = _dir.path().join("crlf-out.txt");
        let report = engine
            .snapshot()
            .write(&temp, false, &AtomicU64::new(0), &no_cancel())
            .unwrap();
        engine.adopt_saved(report, temp.clone()).unwrap();
        assert_eq!(fs::read(&temp).unwrap(), b"one!\r\ntwo\r\nthree\r\n");
    }

    #[test]
    fn probe_reports_non_text() {
        let (_dir, path) = write_file("k.bin", &[0, 1, 2, 0, 4]);
        let probe = DiskEngine::probe(&path).unwrap();
        assert!(probe.non_text);
        let cancel = AtomicBool::new(false);
        let progress = AtomicU64::new(0);
        let result = DiskEngine::open(&path, &cancel, &progress);
        assert!(result.is_err());
    }
}

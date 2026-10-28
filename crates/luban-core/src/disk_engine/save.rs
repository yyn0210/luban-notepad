//! Streaming save for the disk engine.
//!
//! A save writes "untouched source blocks copied byte-for-byte" plus "journal
//! ranges re-encoded with the original encoding and line endings" through a
//! same-directory temporary file. The temporary file is fsynced and renamed
//! over the target, so the original file is never truncated in place.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use crate::atomic_write::AtomicFile;
use crate::encoding::TextEncoding;
use crate::eol::{self, Eol};
use crate::error::{Error, Result};
use crate::i18n::tr;

use super::journal::read_exact_at;
use super::{decode_body, BaseFile, Block, DiskEngine, Piece, PieceKind, BLOCK_TARGET_BYTES};

/// Facts produced by a completed save.
#[derive(Debug)]
pub struct SaveReport {
    pub(super) blocks: Vec<Block>,
    pub(super) size: u64,
    pub(super) modified: Option<SystemTime>,
    pub(super) read_only: bool,
    pub(super) lossy: bool,
}

impl SaveReport {
    pub fn file_size(&self) -> u64 {
        self.size
    }

    pub fn modified(&self) -> Option<SystemTime> {
        self.modified
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Some characters could not be represented in the document encoding.
    pub fn lossy(&self) -> bool {
        self.lossy
    }
}

/// A frozen view of the document used by a background save.
#[derive(Clone, Debug)]
pub struct SaveSnapshot {
    pub(super) path: PathBuf,
    pub(super) encoding: TextEncoding,
    pub(super) eol: Eol,
    pub(super) bom_len: u64,
    pub(super) base_len: u64,
    /// Absolute file offset of the indexed body. Range views use it to read
    /// untouched source blocks; whole-file engines use the BOM length.
    pub(super) body_start: u64,
    /// Source range for a range view; `None` for a whole file.
    pub(super) range: Option<super::RangeSpec>,
    pub(super) base_modified: Option<SystemTime>,
    pub(super) blocks: Vec<Block>,
    pub(super) pieces: Vec<Piece>,
    pub(super) journal_path: Option<PathBuf>,
    /// The file handle used to build the index. Keeping it lets an explicit
    /// overwrite read the bytes the index refers to even when the path was
    /// replaced on disk.
    pub(super) base: Arc<BaseFile>,
}

impl DiskEngine {
    /// Copy the piece table and file facts. The clone is independent of later
    /// edits: journal ranges are append-only, so they stay readable.
    pub fn snapshot(&self) -> SaveSnapshot {
        SaveSnapshot {
            path: self.path().to_path_buf(),
            encoding: self.encoding(),
            eol: self.eol(),
            bom_len: self.bom_len(),
            base_len: self.base_size(),
            body_start: self.body_start,
            range: self.range,
            base_modified: self.last_modified(),
            blocks: self.blocks.clone(),
            pieces: self.pieces.clone(),
            journal_path: self
                .journal
                .as_ref()
                .map(|journal| journal.path().to_path_buf()),
            base: self.base.clone(),
        }
    }
}

impl SaveSnapshot {
    /// Write the document to `target`. `progress` receives bytes written.
    ///
    /// `force` skips the external-change checks after the user explicitly
    /// confirmed overwriting the file on disk.
    pub fn write(
        &self,
        target: &Path,
        force: bool,
        progress: &AtomicU64,
        cancel: &AtomicBool,
    ) -> Result<SaveReport> {
        progress.store(0, Ordering::Relaxed);
        if self.range.is_some() && self.is_same_file(target) {
            return Err(Error::Other(
                tr("core.document.partial_same_path").to_string(),
            ));
        }
        if !force {
            self.verify_source()?;
        }
        let mut atomic = AtomicFile::create(target)?;
        // Replacing the source keeps its permissions; a new save-as target
        // uses the default mode so a read-only source does not leave the new
        // document read-only.
        let same_file = self.is_same_file(atomic.target_path());
        let source_permissions = if same_file {
            fs::metadata(&self.path)
                .ok()
                .map(|metadata| metadata.permissions())
        } else {
            None
        };
        let base = &self.base;
        let journal = if self
            .pieces
            .iter()
            .any(|piece| matches!(piece.kind, PieceKind::Journal { .. }))
        {
            let path = self
                .journal_path
                .as_ref()
                .ok_or_else(|| Error::Other(tr("core.disk_engine.journal_corrupt").to_string()))?;
            Some(File::open(path)?)
        } else {
            None
        };
        let mut body_written = 0u64;
        let mut lossy = false;
        let mut blocks: Vec<Block> = Vec::new();
        let mut pending: Option<Block> = None;

        if self.bom_len > 0 {
            atomic.write(self.encoding.bom())?;
        }
        for piece in &self.pieces {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let (bytes, raw) = self.piece_bytes(piece, base, journal.as_ref())?;
            let (actual, piece_lossy) = if raw {
                (bytes, false)
            } else {
                let text = String::from_utf8(bytes).map_err(|_| {
                    Error::Other(tr("core.disk_engine.journal_corrupt").to_string())
                })?;
                let normalized = eol::normalize(&text, self.eol);
                encode_body(&normalized, self.encoding)
            };
            lossy |= piece_lossy;
            atomic.write(&actual)?;
            let written = actual.len() as u64;
            let start = body_written;
            body_written += written;

            match pending.as_mut() {
                Some(block) if block.logical_len + piece.len <= BLOCK_TARGET_BYTES as u64 => {
                    block.src_len += written;
                    block.logical_len += piece.len;
                    block.chars += piece.chars;
                    block.newlines += piece.newlines;
                }
                Some(_) => {
                    blocks.push(pending.take().expect("pending block"));
                    pending = Some(Block {
                        src_offset: start,
                        src_len: written,
                        logical_start: piece.logical_start,
                        logical_len: piece.len,
                        chars: piece.chars,
                        line: piece.line,
                        newlines: piece.newlines,
                    });
                }
                None => {
                    pending = Some(Block {
                        src_offset: start,
                        src_len: written,
                        logical_start: piece.logical_start,
                        logical_len: piece.len,
                        chars: piece.chars,
                        line: piece.line,
                        newlines: piece.newlines,
                    });
                }
            }
            progress.store(self.bom_len + body_written, Ordering::Relaxed);
        }
        if let Some(block) = pending.take() {
            blocks.push(block);
        }
        // The caller-provided permission snapshot (rather than a metadata read
        // after writing) keeps the previous save-as semantics: a range view
        // written to a new path is not made read-only by its source.
        if let Some(permissions) = source_permissions {
            atomic.set_commit_permissions(permissions);
        }
        if !force {
            self.verify_source()?;
        }
        atomic.commit()?;
        let metadata = fs::metadata(target)?;
        Ok(SaveReport {
            blocks,
            size: metadata.len(),
            modified: metadata.modified().ok(),
            read_only: metadata.permissions().readonly(),
            lossy,
        })
    }

    fn verify_source(&self) -> Result<()> {
        let metadata = fs::metadata(&self.path)?;
        if metadata.len() != self.base_len {
            return Err(Error::Other(
                tr("core.disk_engine.external_change").to_string(),
            ));
        }
        if let (Some(current), Some(known)) = (metadata.modified().ok(), self.base_modified) {
            if current != known {
                return Err(Error::Other(
                    tr("core.disk_engine.external_change").to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Returns either the original bytes (`raw == true`) or the piece's
    /// LF-normalised UTF-8 text.
    fn is_same_file(&self, target: &Path) -> bool {
        if target == self.path {
            return true;
        }
        match (fs::canonicalize(target), fs::canonicalize(&self.path)) {
            (Ok(target), Ok(source)) => target == source,
            _ => false,
        }
    }

    fn piece_bytes(
        &self,
        piece: &Piece,
        base: &BaseFile,
        journal: Option<&File>,
    ) -> Result<(Vec<u8>, bool)> {
        if piece.raw {
            if let PieceKind::Source { block } = piece.kind {
                if let Some(block) = self.blocks.get(block as usize) {
                    if piece.offset == 0 && piece.len == block.logical_len {
                        let mut bytes = vec![0u8; usize::try_from(block.src_len).unwrap_or(0)];
                        read_exact_at(&base.file, &mut bytes, self.body_start + block.src_offset)?;
                        return Ok((bytes, true));
                    }
                }
            }
        }
        match piece.kind {
            PieceKind::Source { block } => {
                let block = self.blocks.get(block as usize).ok_or_else(|| {
                    Error::Other(tr("core.disk_engine.corrupt_index").to_string())
                })?;
                let mut raw = vec![0u8; usize::try_from(block.src_len).unwrap_or(0)];
                read_exact_at(&base.file, &mut raw, self.body_start + block.src_offset)?;
                let decoded = eol::to_lf_owned(decode_body(&raw, self.encoding));
                let begin = usize::try_from(piece.offset)
                    .unwrap_or(0)
                    .min(decoded.len());
                let end = usize::try_from(piece.offset + piece.len)
                    .unwrap_or(decoded.len())
                    .min(decoded.len());
                let begin = clamp_boundary(&decoded, begin);
                let end = clamp_boundary(&decoded, end);
                Ok((decoded.as_bytes()[begin..end].to_vec(), false))
            }
            PieceKind::Journal { offset } => {
                let journal = journal.ok_or_else(|| {
                    Error::Other(tr("core.disk_engine.journal_corrupt").to_string())
                })?;
                let mut bytes = vec![0u8; usize::try_from(piece.len).unwrap_or(0)];
                read_exact_at(journal, &mut bytes, offset)?;
                Ok((bytes, false))
            }
        }
    }
}

fn encode_body(text: &str, encoding: TextEncoding) -> (Vec<u8>, bool) {
    if encoding.is_utf16() {
        let little_endian = encoding == TextEncoding::Utf16Le;
        let mut out = Vec::with_capacity(2 + text.len() * 2);
        for unit in text.encode_utf16() {
            out.extend_from_slice(&if little_endian {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        return (out, false);
    }
    let (body, _, had_errors) = encoding.codec().encode(text);
    (body.into_owned(), had_errors)
}

fn clamp_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

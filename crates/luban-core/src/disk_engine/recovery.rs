//! Durable recovery data for disk-engine documents.
//!
//! Each engine that receives an edit owns a directory under the application
//! configuration called `recovery/<id>/` containing:
//!
//! - `checkpoint.json` — a complete piece table plus the source identity;
//! - `edits.bin` — the append-only copy-on-write journal referenced by pieces;
//! - `ops.jsonl` — one JSON record per committed edit (including undo/redo and
//!   replace-all batches), replayed on top of the checkpoint.
//!
//! Checkpoints are written atomically at open and every [`CHECKPOINT_OPS`]
//! operations. A checkpoint records how many operation records it already
//! reflects, so replay starts at the right line even when the operations file
//! was not truncated. Normal saves and close discard the directory; a crash or
//! forced termination leaves it behind for the next start.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::encoding::TextEncoding;
use crate::eol::Eol;

use super::{Block, Piece};

pub const CHECKPOINT_NAME: &str = "checkpoint.json";
pub const JOURNAL_NAME: &str = "edits.bin";
pub const OPS_NAME: &str = "ops.jsonl";
pub const CHECKPOINT_VERSION: u32 = 1;
/// Operations between checkpoints. The operations log keeps every edit, so
/// this only bounds replay time after a crash.
pub const CHECKPOINT_OPS: u64 = 128;
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);
/// Recovery directories older than this are removed at startup.
pub const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

static RECOVERY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// A serializable system time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timestamp {
    pub secs: u64,
    pub nanos: u32,
}

impl Timestamp {
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now()).unwrap_or(Self { secs: 0, nanos: 0 })
    }

    pub fn from_system_time(time: SystemTime) -> Option<Self> {
        let duration = time.duration_since(UNIX_EPOCH).ok()?;
        Some(Self {
            secs: duration.as_secs(),
            nanos: duration.subsec_nanos(),
        })
    }

    pub fn to_system_time(self) -> SystemTime {
        UNIX_EPOCH + Duration::new(self.secs, self.nanos)
    }
}

/// One replaced range in a logged operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Splice {
    pub start: u64,
    pub removed: u64,
    pub after: Vec<Piece>,
}

/// One committed operation. `cursor` is the logical caret afterwards.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoggedOp {
    pub cursor: u64,
    /// 1-based top line at the time of the operation. Older logs default to 0
    /// and fall back to the checkpoint value.
    #[serde(default)]
    pub scroll_line: u64,
    pub splices: Vec<Splice>,
}

/// Everything needed to rebuild an engine on top of a freshly indexed source.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub version: u32,
    pub path: PathBuf,
    pub range: Option<[u64; 2]>,
    pub encoding: TextEncoding,
    pub eol: Eol,
    pub bom_len: u64,
    pub body_start: u64,
    pub base_len: u64,
    pub body_len: u64,
    pub base_modified: Option<Timestamp>,
    pub read_only: bool,
    pub blocks: Vec<Block>,
    pub pieces: Vec<Piece>,
    pub total_len: u64,
    pub total_chars: u64,
    pub total_newlines: u64,
    /// Operation records already reflected in `pieces`.
    pub ops_applied: u64,
    /// The document had unsaved edits when the checkpoint was written.
    pub modified: bool,
    pub cursor: u64,
    pub scroll_line: u64,
    pub saved_at: Timestamp,
}

/// A recovery directory found at startup.
#[derive(Clone, Debug)]
pub struct RecoveryInfo {
    pub id: String,
    pub dir: PathBuf,
    pub checkpoint: Checkpoint,
    /// Valid operation records present in `ops.jsonl`.
    pub ops: u64,
    /// At least one change is not part of the source file.
    pub modified: bool,
}

impl RecoveryInfo {
    pub fn path(&self) -> &Path {
        &self.checkpoint.path
    }

    pub fn range(&self) -> Option<[u64; 2]> {
        self.checkpoint.range
    }

    pub fn cursor(&self) -> u64 {
        self.checkpoint.cursor
    }

    pub fn scroll_line(&self) -> u64 {
        self.checkpoint.scroll_line
    }

    pub fn source_size(&self) -> u64 {
        self.checkpoint.base_len
    }
}

/// Root of the recovery area inside the application configuration directory.
pub fn recovery_root() -> PathBuf {
    #[cfg(test)]
    {
        test_root()
    }
    #[cfg(not(test))]
    {
        crate::settings::config_dir().join("recovery")
    }
}

/// Unit tests must never write recovery data into the developer's real
/// configuration directory. All core tests share one process-wide directory.
#[cfg(test)]
fn test_root() -> PathBuf {
    use std::sync::OnceLock;
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        std::env::temp_dir().join(format!(
            "luban-notepad-recovery-tests-{}",
            std::process::id()
        ))
    })
    .clone()
}

/// Scan for recoverable documents.
///
/// Directories older than `max_age` are removed. Unreadable or incomplete
/// entries are logged and skipped; a directory is never deleted before the
/// retention period, so a damaged entry can still be inspected by hand.
pub fn scan(max_age: Duration) -> Vec<RecoveryInfo> {
    let root = recovery_root();
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let aged = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        let id = entry.file_name().to_string_lossy().into_owned();
        // Another running instance still owns this directory; never offer its
        // edits for recovery and never delete it.
        if owner_alive(&id) {
            continue;
        }
        match read_info(&id, &dir) {
            Ok(Some(info)) => {
                if aged {
                    remove_expired(&dir);
                } else {
                    found.push(info);
                }
            }
            Ok(None) => {
                if aged {
                    remove_expired(&dir);
                }
            }
            Err(error) => {
                log::warn!(
                    "ignoring unreadable recovery data at {}: {error}",
                    dir.display()
                );
                if aged {
                    remove_expired(&dir);
                }
            }
        }
    }
    found.sort_by_key(|info| std::cmp::Reverse(info.checkpoint.saved_at.secs));
    found
}

/// Remove an expired recovery directory, reporting a cleanup failure instead
/// of silently keeping it forever.
fn remove_expired(dir: &Path) {
    if let Err(error) = fs::remove_dir_all(dir) {
        if error.kind() != io::ErrorKind::NotFound {
            log::warn!(
                "could not remove expired recovery data at {}: {error}",
                dir.display()
            );
        }
    }
}

fn read_info(id: &str, dir: &Path) -> io::Result<Option<RecoveryInfo>> {
    let checkpoint_path = dir.join(CHECKPOINT_NAME);
    let checkpoint = match fs::read(&checkpoint_path) {
        Ok(bytes) => {
            let checkpoint: Checkpoint = serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if checkpoint.version != CHECKPOINT_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsupported recovery checkpoint version",
                ));
            }
            checkpoint
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let ops = read_ops(&dir.join(OPS_NAME))?;
    let modified = checkpoint.modified || ops.len() as u64 > checkpoint.ops_applied;
    if !modified {
        return Ok(None);
    }
    Ok(Some(RecoveryInfo {
        id: id.to_string(),
        dir: dir.to_path_buf(),
        checkpoint,
        ops: ops.len() as u64,
        modified,
    }))
}

/// Parse an operations log, stopping at the first unterminated or corrupt
/// record. A crash in the middle of an append must not invalidate the edits
/// that were fully written before it.
pub fn read_ops(path: &Path) -> io::Result<Vec<LoggedOp>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut ops = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<LoggedOp>(line) {
            Ok(op) => ops.push(op),
            Err(error) => {
                log::warn!("ignoring incomplete recovery operation: {error}");
                break;
            }
        }
    }
    Ok(ops)
}

/// Remove a recovery directory after its contents were restored or discarded.
pub fn discard(dir: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Live recovery writer owned by an engine.
#[derive(Debug)]
pub(super) struct Recovery {
    id: String,
    dir: PathBuf,
    ops_file: Option<File>,
    ops: u64,
    checkpoint_ops: u64,
    last_checkpoint: Instant,
    removed: bool,
}

impl Recovery {
    /// Create a fresh recovery directory and its operations log.
    pub(super) fn create() -> io::Result<(Self, PathBuf)> {
        let id = unique_id();
        let dir = recovery_root().join(&id);
        fs::create_dir_all(&dir)?;
        let recovery = Self::open(id, dir.clone())?;
        Ok((recovery, dir))
    }

    /// Attach to an existing recovery directory found at startup.
    pub(super) fn attach(
        id: String,
        dir: PathBuf,
        ops: u64,
        checkpoint_ops: u64,
    ) -> io::Result<Self> {
        let mut recovery = Self::open(id, dir)?;
        recovery.ops = ops;
        recovery.checkpoint_ops = checkpoint_ops.min(ops);
        Ok(recovery)
    }

    fn open(id: String, dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        repair_ops_file(&dir.join(OPS_NAME))?;
        let ops_file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(dir.join(OPS_NAME))?;
        let ops = read_ops(&dir.join(OPS_NAME))?.len() as u64;
        Ok(Self {
            id,
            dir,
            ops_file: Some(ops_file),
            ops,
            checkpoint_ops: 0,
            last_checkpoint: Instant::now(),
            removed: false,
        })
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn ops(&self) -> u64 {
        self.ops
    }

    /// Append one committed operation. The file is not buffered, so a process
    /// kill after this returns cannot lose the record.
    pub(super) fn append(&mut self, op: &LoggedOp) -> io::Result<()> {
        let mut line = serde_json::to_vec(op).map_err(io::Error::other)?;
        line.push(b'\n');
        self.ops_file
            .as_mut()
            .ok_or_else(|| io::Error::other("recovery log is closed"))?
            .write_all(&line)?;
        self.ops += 1;
        Ok(())
    }

    pub(super) fn write_checkpoint(&mut self, checkpoint: &Checkpoint) -> io::Result<()> {
        let json = serde_json::to_vec(checkpoint).map_err(io::Error::other)?;
        crate::atomic_write::write_atomic(&self.dir.join(CHECKPOINT_NAME), &json)?;
        self.checkpoint_ops = checkpoint.ops_applied;
        self.last_checkpoint = Instant::now();
        Ok(())
    }

    pub(super) fn should_checkpoint(&self) -> bool {
        self.ops.saturating_sub(self.checkpoint_ops) >= CHECKPOINT_OPS
            || self.last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL
    }

    pub(super) fn remove(mut self) {
        self.removed = true;
        self.ops_file = None;
        let _ = fs::remove_dir_all(&self.dir);
    }

    /// Release the files without deleting the directory. Used when a rebuilt
    /// engine cannot be mounted but the recovery data must stay available.
    pub(super) fn preserve(mut self) {
        self.removed = true;
        self.ops_file = None;
    }
}

impl Drop for Recovery {
    fn drop(&mut self) {
        // A dropped engine that was not discarded explicitly (normal close)
        // must not leave a recovery prompt behind. A crash bypasses Drop, which
        // is exactly what keeps the data available.
        self.ops_file.take();
        if !self.removed {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// Whether the process that created a recovery directory is still running.
///
/// The directory name starts with the owning pid. A live owner means another
/// instance is editing the document, so its recovery data is neither offered
/// nor cleaned up by this instance.
fn owner_alive(id: &str) -> bool {
    // Unit tests create and inspect recovery data inside one process; they
    // must be able to see their own entries.
    #[cfg(test)]
    {
        let _ = id;
        false
    }
    #[cfg(not(test))]
    {
        let Some(pid) = id.split('-').next().and_then(|pid| pid.parse::<u32>().ok()) else {
            return false;
        };
        if pid == std::process::id() {
            return true;
        }
        process_alive(pid)
    }
}

#[cfg(all(not(test), target_os = "linux"))]
fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(all(not(test), unix, not(target_os = "linux")))]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(all(not(test), windows))]
fn process_alive(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output();
    match output {
        Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()),
        Err(_) => false,
    }
}

#[cfg(all(not(test), not(any(unix, windows))))]
fn process_alive(_pid: u32) -> bool {
    false
}

fn unique_id() -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = RECOVERY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{pid}-{nanos:x}-{sequence}")
}

/// Drop an unterminated record left by a kill during an append, or terminate
/// it when the JSON itself was complete. Without this the next appended record
/// would be glued to the damaged tail.
fn repair_ops_file(path: &Path) -> io::Result<()> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if data.is_empty() || data.ends_with(b"\n") {
        return Ok(());
    }
    let complete_len = data
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let tail = String::from_utf8_lossy(&data[complete_len..]);
    if serde_json::from_str::<LoggedOp>(tail.trim()).is_ok() {
        let mut file = OpenOptions::new().append(true).open(path)?;
        file.write_all(b"\n")?;
    } else {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(complete_len as u64)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk_engine::{PieceKind, BLOCK_TARGET_BYTES};

    fn sample_piece() -> Piece {
        Piece {
            kind: PieceKind::Journal { offset: 0 },
            offset: 0,
            len: 5,
            chars: 5,
            newlines: 0,
            logical_start: 0,
            line: 1,
            raw: false,
        }
    }

    fn sample_checkpoint(path: &Path, modified: bool, ops_applied: u64) -> Checkpoint {
        Checkpoint {
            version: CHECKPOINT_VERSION,
            path: path.to_path_buf(),
            range: None,
            encoding: TextEncoding::Utf8,
            eol: Eol::Lf,
            bom_len: 0,
            body_start: 0,
            base_len: 10,
            body_len: 10,
            base_modified: None,
            read_only: false,
            blocks: Vec::new(),
            pieces: vec![sample_piece()],
            total_len: 5,
            total_chars: 5,
            total_newlines: 0,
            ops_applied,
            modified,
            cursor: 3,
            scroll_line: 1,
            saved_at: Timestamp::now(),
        }
    }

    fn sample_op(cursor: u64) -> LoggedOp {
        LoggedOp {
            cursor,
            scroll_line: 1,
            splices: vec![Splice {
                start: 0,
                removed: 0,
                after: vec![sample_piece()],
            }],
        }
    }

    #[test]
    fn checkpoint_and_operations_round_trip() {
        let root = tempfile::tempdir().unwrap();
        // Keep the test out of the real configuration directory.
        let recovery = Recovery {
            id: "test".into(),
            dir: root.path().to_path_buf(),
            ops_file: Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .read(true)
                    .open(root.path().join(OPS_NAME))
                    .unwrap(),
            ),
            ops: 0,
            checkpoint_ops: 0,
            last_checkpoint: Instant::now(),
            removed: true,
        };
        let mut recovery = recovery;
        recovery.append(&sample_op(1)).unwrap();
        let checkpoint = sample_checkpoint(&root.path().join("source.txt"), true, 1);
        recovery.write_checkpoint(&checkpoint).unwrap();

        let info = read_info("test", root.path()).unwrap().unwrap();
        assert_eq!(info.ops, 1);
        assert_eq!(info.checkpoint.ops_applied, 1);
        assert_eq!(info.checkpoint.cursor, 3);
        assert!(info.modified);
        let ops = read_ops(&root.path().join(OPS_NAME)).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].cursor, 1);
    }

    #[test]
    fn clean_checkpoints_are_not_recoverable() {
        let root = tempfile::tempdir().unwrap();
        let checkpoint = sample_checkpoint(&root.path().join("source.txt"), false, 0);
        let json = serde_json::to_vec(&checkpoint).unwrap();
        fs::write(root.path().join(CHECKPOINT_NAME), json).unwrap();
        fs::write(root.path().join(OPS_NAME), b"").unwrap();
        assert!(read_info("test", root.path()).unwrap().is_none());
    }

    #[test]
    fn partial_operation_line_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(OPS_NAME);
        let mut data = serde_json::to_vec(&sample_op(1)).unwrap();
        data.push(b'\n');
        data.extend_from_slice(b"{\"cursor\":2,\"splices\":[");
        fs::write(&path, data).unwrap();
        assert_eq!(read_ops(&path).unwrap().len(), 1);
    }

    #[test]
    fn a_damaged_tail_is_repaired_before_appending() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(OPS_NAME);
        let mut data = serde_json::to_vec(&sample_op(1)).unwrap();
        data.push(b'\n');
        data.extend_from_slice(b"{\"cursor\":2,\"splices\":[");
        fs::write(&path, &data).unwrap();
        repair_ops_file(&path).unwrap();
        let repaired = fs::read(&path).unwrap();
        assert!(repaired.ends_with(b"\n"));
        assert_eq!(read_ops(&path).unwrap().len(), 1);

        // A complete record without a trailing newline only gains one.
        let complete = serde_json::to_vec(&sample_op(3)).unwrap();
        fs::write(&path, &complete).unwrap();
        repair_ops_file(&path).unwrap();
        assert_eq!(read_ops(&path).unwrap().len(), 1);
        assert!(fs::read(&path).unwrap().ends_with(b"\n"));
    }

    #[test]
    fn checkpoint_after_block_boundaries_survives() {
        // The checkpoint is plain JSON; a piece referencing a source block and
        // one referencing the journal must both round-trip.
        let dir = tempfile::tempdir().unwrap();
        let mut checkpoint = sample_checkpoint(&dir.path().join("source.txt"), true, 0);
        checkpoint.blocks.push(Block {
            src_offset: 0,
            src_len: 4096,
            logical_start: 0,
            logical_len: 4096,
            chars: 4096,
            line: 1,
            newlines: 100,
        });
        checkpoint.pieces.insert(
            0,
            Piece {
                kind: PieceKind::Source { block: 0 },
                offset: 0,
                len: 4096,
                chars: 4096,
                newlines: 100,
                logical_start: 0,
                line: 1,
                raw: true,
            },
        );
        let path = dir.path().join(CHECKPOINT_NAME);
        fs::write(&path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        let loaded: Checkpoint = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(loaded.blocks.len(), 1);
        assert_eq!(loaded.pieces.len(), 2);
        assert!(loaded.blocks[0].src_len <= BLOCK_TARGET_BYTES as u64 * 4);
    }
}

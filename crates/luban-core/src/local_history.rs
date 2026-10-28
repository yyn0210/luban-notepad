//! Local history: snapshots of a file's on-disk content taken before a save.
//!
//! Snapshots live under the application configuration directory, bucketed by a
//! stable hash of the source path. Each bucket is independent, so one damaged
//! directory cannot hide another file's history. Entries are raw bytes: the
//! caller decodes them with the document's encoding when restoring.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::settings::config_dir;

const SNAPSHOT_EXT: &str = "snap";

/// One stored version of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub path: PathBuf,
    pub taken_at: SystemTime,
    pub size: u64,
}

/// Per-user local history store.
#[derive(Clone, Debug)]
pub struct HistoryStore {
    root: PathBuf,
}

impl Default for HistoryStore {
    fn default() -> Self {
        Self::system()
    }
}

impl HistoryStore {
    /// Store under the application configuration directory.
    pub fn system() -> Self {
        Self::new(config_dir().join("history"))
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Capture the current on-disk content of `source` before it is replaced.
    ///
    /// Returns `None` when the file is missing, empty, unreadable, or still
    /// identical to the newest snapshot. Failure to write the snapshot is
    /// returned to the caller, which must not let it block the save.
    pub fn snapshot(&self, source: &Path) -> io::Result<Option<HistoryEntry>> {
        let metadata = match fs::metadata(source) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if metadata.len() == 0 {
            return Ok(None);
        }
        let bucket = self.bucket_path(source);
        let mut existing = self.entries_in(&bucket)?;
        existing.sort_by_key(|entry| std::cmp::Reverse(entry.file_name.clone()));
        if let Some(newest) = existing.first() {
            let newest_path = bucket.join(&newest.file_name);
            if files_equal(source, &newest_path)? {
                return Ok(None);
            }
        }
        fs::create_dir_all(&bucket)?;
        let entry = self.write_entry(&bucket, source)?;
        Ok(Some(entry))
    }

    /// Snapshots for `source`, newest first.
    pub fn list(&self, source: &Path) -> io::Result<Vec<HistoryEntry>> {
        let bucket = self.bucket_path(source);
        let mut entries = self.entries_in(&bucket)?;
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.file_name.clone()));
        Ok(entries
            .into_iter()
            .map(|entry| HistoryEntry {
                path: bucket.join(entry.file_name),
                taken_at: entry.taken_at,
                size: entry.size,
            })
            .collect())
    }

    pub fn read(&self, entry: &HistoryEntry) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        fs::File::open(&entry.path)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Remove every snapshot belonging to `source`.
    pub fn clear(&self, source: &Path) -> io::Result<()> {
        match fs::remove_dir_all(self.bucket_path(source)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Drop expired snapshots and trim each file bucket to `max_bytes`.
    ///
    /// The newest snapshot of a file is never removed by the size limit, so a
    /// readable recovery point remains even when one snapshot exceeds the cap.
    pub fn prune(&self, keep: Duration, max_bytes: u64) -> io::Result<()> {
        let dirs = match fs::read_dir(&self.root) {
            Ok(dirs) => dirs,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let now = SystemTime::now();
        for dir in dirs.flatten() {
            let bucket = dir.path();
            if !bucket.is_dir() {
                continue;
            }
            let mut entries = match self.entries_in(&bucket) {
                Ok(entries) => entries,
                Err(error) => {
                    log::warn!("cannot read local history {}: {error}", bucket.display());
                    continue;
                }
            };
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.file_name.clone()));
            let cutoff = now.checked_sub(keep);
            for entry in &entries {
                if cutoff.is_some_and(|cutoff| entry.taken_at < cutoff) {
                    remove_quietly(&bucket.join(&entry.file_name));
                }
            }
            let mut entries = match self.entries_in(&bucket) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.file_name.clone()));
            let mut total: u64 = entries.iter().map(|entry| entry.size).sum();
            for entry in entries.iter().skip(1).rev() {
                if total <= max_bytes {
                    break;
                }
                remove_quietly(&bucket.join(&entry.file_name));
                total = total.saturating_sub(entry.size);
            }
            if self
                .entries_in(&bucket)
                .is_ok_and(|entries| entries.is_empty())
            {
                let _ = fs::remove_dir(&bucket);
            }
        }
        Ok(())
    }

    /// Total bytes currently stored for `source`.
    pub fn total_size(&self, source: &Path) -> io::Result<u64> {
        Ok(self
            .entries_in(&self.bucket_path(source))?
            .iter()
            .map(|entry| entry.size)
            .sum())
    }

    fn bucket_path(&self, source: &Path) -> PathBuf {
        self.root.join(bucket_key(source))
    }

    fn write_entry(&self, bucket: &Path, source: &Path) -> io::Result<HistoryEntry> {
        let now = SystemTime::now();
        let nanos = now
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for sequence in 0..1000u16 {
            let name = format!("{nanos:020}-{sequence:04}.{SNAPSHOT_EXT}");
            let path = bucket.join(&name);
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(mut file) => {
                    let size = match copy_snapshot(source, &mut file) {
                        Ok(size) => size,
                        Err(error) => {
                            remove_quietly(&path);
                            return Err(error);
                        }
                    };
                    file.flush()?;
                    let _ = file.sync_all();
                    return Ok(HistoryEntry {
                        path,
                        taken_at: now,
                        size,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique local-history snapshot name",
        ))
    }

    fn entries_in(&self, bucket: &Path) -> io::Result<Vec<StoredEntry>> {
        let dir = match fs::read_dir(bucket) {
            Ok(dir) => dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut entries = Vec::new();
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(stem) = name.strip_suffix(&format!(".{SNAPSHOT_EXT}")) else {
                continue;
            };
            let Some((nanos, _)) = stem.split_once('-') else {
                continue;
            };
            let Ok(nanos) = nanos.parse::<u128>() else {
                continue;
            };
            let metadata = match entry.metadata() {
                Ok(metadata) if metadata.is_file() => metadata,
                _ => continue,
            };
            let taken_at = UNIX_EPOCH
                .checked_add(Duration::from_nanos(nanos.min(u64::MAX as u128) as u64))
                .unwrap_or(UNIX_EPOCH);
            entries.push(StoredEntry {
                file_name: name,
                taken_at,
                size: metadata.len(),
            });
        }
        Ok(entries)
    }
}

struct StoredEntry {
    file_name: String,
    taken_at: SystemTime,
    size: u64,
}

fn copy_snapshot(source: &Path, destination: &mut fs::File) -> io::Result<u64> {
    let mut input = io::BufReader::new(fs::File::open(source)?);
    io::copy(&mut input, destination)
}

fn files_equal(left: &Path, right: &Path) -> io::Result<bool> {
    let left_meta = fs::metadata(left)?;
    let right_meta = match fs::metadata(right) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if left_meta.len() != right_meta.len() {
        return Ok(false);
    }
    let mut left = io::BufReader::with_capacity(64 * 1024, fs::File::open(left)?);
    let mut right = io::BufReader::with_capacity(64 * 1024, fs::File::open(right)?);
    let mut left_buf = vec![0u8; 64 * 1024];
    let mut right_buf = vec![0u8; 64 * 1024];
    loop {
        let left_read = left.read(&mut left_buf)?;
        let right_read = right.read(&mut right_buf)?;
        if left_read != right_read || left_buf[..left_read] != right_buf[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn remove_quietly(path: &Path) {
    if let Err(error) = fs::remove_file(path) {
        if error.kind() != io::ErrorKind::NotFound {
            log::warn!("cannot remove local history {}: {error}", path.display());
        }
    }
}

/// Stable bucket name for a source path.
fn bucket_key(source: &Path) -> String {
    let canonical = fs::canonicalize(source).unwrap_or_else(|_| absolute(source));
    let mut text = canonical.to_string_lossy().into_owned();
    if cfg!(windows) {
        text.make_ascii_lowercase();
    }
    format!("{:016x}", fnv1a(text.as_bytes()))
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, HistoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path().join("history"));
        (dir, store)
    }

    #[test]
    fn snapshots_the_previous_content_and_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("note.txt");
        let (_store_dir, store) = store();
        fs::write(&source, b"first").unwrap();
        let first = store.snapshot(&source).unwrap().unwrap();
        assert_eq!(fs::read(&first.path).unwrap(), b"first");
        assert_eq!(first.size, 5);
        assert!(store.snapshot(&source).unwrap().is_none());

        fs::write(&source, b"second").unwrap();
        let second = store.snapshot(&source).unwrap().unwrap();
        assert_eq!(fs::read(&second.path).unwrap(), b"second");
        let entries = store.list(&source).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].size, 6);
        assert_eq!(entries[1].size, 5);
    }

    #[test]
    fn empty_and_missing_files_produce_no_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("empty.txt");
        let (_store_dir, store) = store();
        fs::write(&source, b"").unwrap();
        assert!(store.snapshot(&source).unwrap().is_none());
        assert!(store
            .snapshot(&dir.path().join("absent.txt"))
            .unwrap()
            .is_none());
        assert!(store.list(&source).unwrap().is_empty());
    }

    #[test]
    fn read_returns_raw_bytes_and_clear_removes_the_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("note.txt");
        let (_store_dir, store) = store();
        fs::write(&source, [0xff, 0x00, 0x41]).unwrap();
        let entry = store.snapshot(&source).unwrap().unwrap();
        assert_eq!(store.read(&entry).unwrap(), vec![0xff, 0x00, 0x41]);
        assert!(store.total_size(&source).unwrap() >= 3);
        store.clear(&source).unwrap();
        assert!(store.list(&source).unwrap().is_empty());
        store.clear(&source).unwrap();
    }

    #[test]
    fn prune_drops_expired_entries() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("note.txt");
        let (_store_dir, store) = store();
        fs::write(&source, b"one").unwrap();
        let entry = store.snapshot(&source).unwrap().unwrap();
        let old = SystemTime::now() - Duration::from_secs(48 * 3600);
        // Age the entry by rewriting its timestamp prefix.
        let bucket = entry.path.parent().unwrap().to_path_buf();
        let name = entry
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let suffix = name.split_once('-').unwrap().1;
        fs::remove_file(&entry.path).unwrap();
        let nanos = old.duration_since(UNIX_EPOCH).unwrap().as_nanos();
        fs::write(bucket.join(format!("{nanos:020}-{suffix}")), b"one").unwrap();

        store
            .prune(Duration::from_secs(24 * 3600), u64::MAX)
            .unwrap();
        assert!(store.list(&source).unwrap().is_empty());
    }

    #[test]
    fn size_limit_keeps_the_newest_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("note.txt");
        let (_store_dir, store) = store();
        fs::write(&source, b"aaaaaaaaaa").unwrap();
        store.snapshot(&source).unwrap().unwrap();
        fs::write(&source, b"bbbbbbbbbb").unwrap();
        store.snapshot(&source).unwrap().unwrap();
        fs::write(&source, b"cccccccccc").unwrap();
        store.snapshot(&source).unwrap().unwrap();
        assert_eq!(store.list(&source).unwrap().len(), 3);

        store
            .prune(Duration::from_secs(3650 * 24 * 3600), 0)
            .unwrap();
        let entries = store.list(&source).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(store.read(&entries[0]).unwrap(), b"cccccccccc");
    }

    #[test]
    fn buckets_are_stable_and_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        assert_eq!(bucket_key(&a), bucket_key(&a));
        assert_ne!(bucket_key(&a), bucket_key(&b));
    }
}

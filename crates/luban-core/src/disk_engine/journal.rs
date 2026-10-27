//! Append-only edit journal used by the disk engine's copy-on-write model.
//!
//! Inserted text never touches the source file. It is appended to a private
//! temporary file and referenced by the piece table. The directory is removed
//! when the engine (or the last snapshot sharing it) is dropped.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static JOURNAL_SEQUENCE: AtomicU64 = AtomicU64::new(1);

const JOURNAL_DIR_PREFIX: &str = "luban-notepad-edit-";
const FILE_NAME: &str = "edits.bin";

pub struct Journal {
    dir: PathBuf,
    path: PathBuf,
    writer: File,
    len: u64,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("dir", &self.dir)
            .field("len", &self.len)
            .finish()
    }
}

impl Journal {
    pub fn create() -> io::Result<Self> {
        let dir = unique_dir()?;
        Self::create_in(dir)
    }

    fn create_in(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let path = dir.join(FILE_NAME);
        let writer = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(&path)?;
        Ok(Self {
            dir,
            path,
            writer,
            len: 0,
        })
    }

    /// Append `bytes` and return the journal offset they were written at.
    pub fn append(&mut self, bytes: &[u8]) -> io::Result<u64> {
        use std::io::Write;
        let offset = self.len;
        self.writer.write_all(bytes)?;
        self.len += bytes.len() as u64;
        Ok(offset)
    }

    pub fn read(&self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let mut buffer = vec![0u8; usize::try_from(len).unwrap_or(0)];
        if buffer.is_empty() {
            return Ok(buffer);
        }
        let file = File::open(&self.path)?;
        read_exact_at(&file, &mut buffer, offset)?;
        Ok(buffer)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Drop every journal byte. Used after a successful save rebases the
    /// document on the newly written file.
    pub fn reset(&mut self) -> io::Result<()> {
        self.writer.set_len(0)?;
        use std::io::Seek;
        self.writer.seek(std::io::SeekFrom::Start(0))?;
        self.len = 0;
        Ok(())
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Positioned read that does not disturb a shared cursor.
pub fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buffer, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut filled = 0usize;
        while filled < buffer.len() {
            let read = file.seek_read(&mut buffer[filled..], offset + filled as u64)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unexpected end of file",
                ));
            }
            filled += read;
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let mut clone = file.try_clone()?;
        use std::io::{Read as _, Seek as _, SeekFrom};
        clone.seek(SeekFrom::Start(offset))?;
        clone.read_exact(buffer)
    }
}

fn unique_dir() -> io::Result<PathBuf> {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = JOURNAL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(base.join(format!("{JOURNAL_DIR_PREFIX}{pid}-{nanos:x}-{sequence}")))
}

/// Remove journal directories left behind by crashed or killed instances.
///
/// Only directories older than `max_age` are touched so another running
/// instance keeps its journals.
pub fn cleanup_stale(max_age: std::time::Duration) -> usize {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(JOURNAL_DIR_PREFIX) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        let aged = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        if aged && fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_reads_back() {
        let mut journal = Journal::create().unwrap();
        assert_eq!(journal.append(b"hello").unwrap(), 0);
        assert_eq!(journal.append(b" world").unwrap(), 5);
        assert_eq!(journal.len(), 11);
        assert_eq!(journal.read(0, 5).unwrap(), b"hello");
        assert_eq!(journal.read(5, 6).unwrap(), b" world");
        journal.reset().unwrap();
        assert_eq!(journal.len(), 0);
        assert_eq!(journal.read(0, 0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn drop_removes_the_directory() {
        let journal = Journal::create().unwrap();
        let dir = journal.dir().to_path_buf();
        assert!(dir.is_dir());
        drop(journal);
        assert!(!dir.exists());
    }

    #[test]
    fn cleanup_only_touches_old_journals() {
        let journal = Journal::create().unwrap();
        let dir = journal.dir().to_path_buf();
        // The sweep covers the shared system temp directory, so it may also
        // collect stale journals left behind by other runs; only this fresh
        // one must survive.
        cleanup_stale(std::time::Duration::from_secs(3600));
        assert!(dir.is_dir());
    }
}

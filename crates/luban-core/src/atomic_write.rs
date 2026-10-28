//! Atomic file replacement shared by document and disk-engine saves.
//!
//! Content is written to a uniquely named temporary file in the target's own
//! directory, flushed and fsynced, then renamed over the target. A crash or a
//! failed write therefore leaves either the old file or the complete new file
//! behind, never a truncated mixture. Existing permissions are preserved and a
//! symbolic link target is followed so the link itself survives the save.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const TEMP_MARKER: &str = ".luban-save-";
/// Stale temporary files from a killed process are only removed after this
/// age, so a concurrent instance is never disturbed.
const STALE_TEMP_AGE: Duration = Duration::from_secs(60 * 60);

/// A temporary file that is atomically renamed over its target on commit.
pub struct AtomicFile {
    target: PathBuf,
    temp: PathBuf,
    permissions: Option<Permissions>,
    file: Option<File>,
    committed: bool,
}

impl std::fmt::Debug for AtomicFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtomicFile")
            .field("target", &self.target)
            .field("temp", &self.temp)
            .finish()
    }
}

impl AtomicFile {
    /// Create the temporary file next to `target`.
    ///
    /// A read-only existing target is rejected before anything is written; the
    /// caller is expected to report the error and offer "Save As" instead of
    /// silently falling back to a truncating write.
    pub fn create(target: &Path) -> io::Result<Self> {
        let target = resolve_write_target(target)?;
        let permissions = match fs::metadata(&target) {
            Ok(metadata) => {
                if metadata.permissions().readonly() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("{} is read-only", target.display()),
                    ));
                }
                Some(metadata.permissions())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let parent = target.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        cleanup_stale_temps(parent, &target);
        let temp = temp_path_for(&target);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        Ok(Self {
            target,
            temp,
            permissions,
            file: Some(file),
            committed: false,
        })
    }

    /// The writable temporary file. It is flushed and fsynced by [`commit`].
    pub fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("temporary file is open")
    }

    /// Apply these permissions to the temporary file at commit time instead of
    /// the target's current permissions.
    pub fn set_commit_permissions(&mut self, permissions: Permissions) {
        self.permissions = Some(permissions);
    }

    /// Flush, fsync, restore permissions and atomically replace the target.
    pub fn commit(mut self) -> io::Result<()> {
        let mut file = self.file.take().expect("temporary file is open");
        file.flush()?;
        file.sync_all()?;
        drop(file);
        if let Some(permissions) = self.permissions.take() {
            fs::set_permissions(&self.temp, permissions)?;
        }
        replace_file(&self.temp, &self.target)?;
        self.committed = true;
        // Persist the directory entry on Unix. Some filesystems reject fsync
        // on directories, so a failure here never turns a successful save into
        // an error.
        #[cfg(unix)]
        if let Some(parent) = self.target.parent() {
            if let Ok(directory) = File::open(parent) {
                let _ = directory.sync_all();
            }
        }
        Ok(())
    }

    /// Path of the temporary file (used by tests).
    #[cfg(test)]
    pub fn temp_path(&self) -> &Path {
        &self.temp
    }

    /// Path actually written to after resolving a symbolic link.
    pub fn target_path(&self) -> &Path {
        &self.target
    }
}

impl Drop for AtomicFile {
    fn drop(&mut self) {
        if !self.committed {
            self.file.take();
            let _ = fs::remove_file(&self.temp);
        }
    }
}

/// Write `bytes` to `target` through a same-directory temporary file.
pub fn write_atomic(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut atomic = AtomicFile::create(target)?;
    atomic.file_mut().write_all(bytes)?;
    atomic.commit()
}

/// Resolve a symbolic link to the file it points at.
///
/// A dangling link is an error: replacing the link with a regular file would
/// silently break the user's link instead of updating its target.
fn resolve_write_target(target: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(target),
        Ok(_) => Ok(target.to_path_buf()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(target.to_path_buf()),
        Err(error) => Err(error),
    }
}

fn temp_path_for(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".to_string());
    let unique = format!(
        ".{name}{TEMP_MARKER}{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    target
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(unique)
}

/// Remove temporary files left by a crashed or killed save of the same target.
fn cleanup_stale_temps(parent: &Path, target: &Path) {
    let Some(name) = target.file_name().map(|name| name.to_string_lossy()) else {
        return;
    };
    let prefix = format!(".{name}{TEMP_MARKER}");
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        if !file_name.to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= STALE_TEMP_AGE);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Atomically replace `target` with `temp` (which must be on the same
/// filesystem).
pub fn replace_file(temp: &Path, target: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let from: Vec<u16> = temp
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let to: Vec<u16> = target
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let replaced = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if replaced == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(temp, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_replaces_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, "old").unwrap();
        write_atomic(&path, b"new content").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new content");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(TEMP_MARKER))
            .collect();
        assert!(leftovers.is_empty(), "temporary file left behind");
    }

    #[test]
    fn failure_keeps_the_original_and_removes_the_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, "original").unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        let mut atomic = AtomicFile::create(&path).unwrap();
        atomic.file_mut().write_all(b"partial").unwrap();
        let temp = atomic.temp_path().to_path_buf();
        drop(atomic);
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        assert!(!temp.exists());
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn commit_can_follow_partial_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        let mut atomic = AtomicFile::create(&path).unwrap();
        atomic.file_mut().write_all(b"hello ").unwrap();
        atomic.file_mut().write_all(b"world").unwrap();
        atomic.commit().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[cfg(unix)]
    #[test]
    fn preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_only_target_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.txt");
        fs::write(&path, "protected").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        let error = write_atomic(&path, b"changed").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_to_string(&path).unwrap(), "protected");
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlinks_and_keeps_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let actual = dir.path().join("actual.txt");
        let link = dir.path().join("link.txt");
        fs::write(&actual, "old").unwrap();
        std::os::unix::fs::symlink(&actual, &link).unwrap();
        write_atomic(&link, b"through the link").unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&actual).unwrap(), "through the link");
    }
}

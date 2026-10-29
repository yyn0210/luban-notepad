//! File-tree operations: create, rename, delete and trash.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameError {
    Empty,
    Dot,
    Separator,
    Nul,
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::i18n::tr(match self {
            Self::Empty => "core.file_ops.name_empty",
            Self::Dot => "core.file_ops.name_dot",
            Self::Separator => "core.file_ops.name_separator",
            Self::Nul => "core.file_ops.name_nul",
        }))
    }
}

/// Validate a single path component typed by the user.
pub fn validate_name(name: &str) -> std::result::Result<&str, NameError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(NameError::Empty);
    }
    if trimmed == "." || trimmed == ".." {
        return Err(NameError::Dot);
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err(NameError::Separator);
    }
    if trimmed.contains('\0') {
        return Err(NameError::Nul);
    }
    Ok(trimmed)
}

/// Create a new empty file. Never overwrites: an existing path is an error.
pub fn create_file(parent: &Path, name: &str) -> Result<PathBuf> {
    let name = validate_name(name).map_err(|error| Error::Other(error.to_string()))?;
    let path = parent.join(name);
    if path.exists() {
        return Err(Error::Other(crate::i18n::tr("core.file_ops.exists")));
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok(path)
}

/// Create a new directory. Never reuses an existing path.
pub fn create_dir(parent: &Path, name: &str) -> Result<PathBuf> {
    let name = validate_name(name).map_err(|error| Error::Other(error.to_string()))?;
    let path = parent.join(name);
    if path.exists() {
        return Err(Error::Other(crate::i18n::tr("core.file_ops.exists")));
    }
    fs::create_dir(&path)?;
    Ok(path)
}

/// Rename a file or directory. The destination must not exist and its final
/// component must be a valid name (no `.`/`..`, separators or NUL).
pub fn rename_path(from: &Path, to: &Path) -> Result<()> {
    if from == to {
        return Ok(());
    }
    let target_name = to
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Other(NameError::Empty.to_string()))?;
    validate_name(target_name).map_err(|error| Error::Other(error.to_string()))?;
    if from.parent() != to.parent() {
        return Err(Error::Other(crate::i18n::tr(
            "core.file_ops.rename_same_parent",
        )));
    }
    if to.exists() {
        return Err(Error::Other(crate::i18n::tr("core.file_ops.exists")));
    }
    if !from.exists() {
        return Err(Error::InvalidPath(from.to_path_buf()));
    }
    fs::rename(from, to)?;
    Ok(())
}

/// Remove a file or directory tree.
pub fn delete_permanently(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Delete through the platform trash. The caller may fall back to
/// [`delete_permanently`] after an explicit confirmation.
pub fn delete_to_trash(path: &Path) -> Result<()> {
    trash_path(path).map_err(|error| {
        Error::Other(crate::i18n::tr_args(
            "core.file_ops.trash_failed",
            &[("error", error.to_string())],
        ))
    })
}

#[cfg(not(target_os = "windows"))]
fn trash_path(path: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().ok_or_else(|| io::Error::other("no home directory"))?;
        return move_unique(path, &home.join(".Trash"));
    }
    #[cfg(not(target_os = "macos"))]
    {
        xdg_trash(path)
    }
}

#[cfg(target_os = "windows")]
fn trash_path(path: &Path) -> io::Result<()> {
    let is_dir = path.is_dir();
    let escaped = path.to_string_lossy().replace('\'', "''");
    let expression = if is_dir {
        format!(
            "Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteDirectory('{escaped}','OnlyErrorDialogs','SendToRecycleBin')"
        )
    } else {
        format!(
            "Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile('{escaped}','OnlyErrorDialogs','SendToRecycleBin')"
        )
    };
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &expression])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "the recycle operation failed with {status}"
        )))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn xdg_trash(path: &Path) -> io::Result<()> {
    let data_dir = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/share")))
        .ok_or_else(|| io::Error::other("no XDG data directory"))?;
    let files_dir = data_dir.join("Trash/files");
    let info_dir = data_dir.join("Trash/info");
    fs::create_dir_all(&files_dir)?;
    fs::create_dir_all(&info_dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("cannot trash a filesystem root"))?
        .to_string_lossy()
        .into_owned();
    let target = unique_path(&files_dir, &name);
    let info_target = info_dir.join(format!(
        "{}.trashinfo",
        target.file_name().unwrap().to_string_lossy()
    ));
    move_across_devices(path, &target)?;
    let info = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_encode(&path.to_string_lossy()),
        deletion_date()
    );
    if let Err(error) = fs::write(&info_target, info) {
        let _ = fs::rename(&target, path);
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn move_unique(path: &Path, directory: &Path) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("cannot trash a filesystem root"))?
        .to_string_lossy()
        .into_owned();
    let target = unique_path(directory, &name);
    move_across_devices(path, &target)
}

fn unique_path(directory: &Path, name: &str) -> PathBuf {
    let candidate = directory.join(name);
    if !candidate.exists() && !directory.join(format!("{name}.trashinfo")).exists() {
        return candidate;
    }
    let path = Path::new(name);
    let (stem, extension) = match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) => (
            name.trim_end_matches(&format!(".{extension}")),
            Some(extension),
        ),
        None => (name, None),
    };
    for index in 1..10_000 {
        let candidate = match extension {
            Some(extension) => directory.join(format!("{stem}.{index}.{extension}")),
            None => directory.join(format!("{stem}.{index}")),
        };
        if !candidate.exists()
            && !directory
                .join(format!(
                    "{}.trashinfo",
                    candidate.file_name().unwrap().to_string_lossy()
                ))
                .exists()
        {
            return candidate;
        }
    }
    directory.join(format!(
        "{stem}.{}.{}",
        std::process::id(),
        extension.unwrap_or("")
    ))
}

fn move_across_devices(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc_exdev()) => {
            copy_tree(from, to)?;
            if from.is_dir() {
                fs::remove_dir_all(from)
            } else {
                fs::remove_file(from)
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn libc_exdev() -> i32 {
    18
}

#[cfg(windows)]
fn libc_exdev() -> i32 {
    17 // ERROR_NOT_SAME_DEVICE
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from, to)?;
        Ok(())
    }
}

/// Percent-encode a path for the `.trashinfo` file.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Current UTC time in the `YYYY-MM-DDTHH:MM:SS` form required by the XDG
/// trash specification. Implemented without a date-time dependency.
fn deletion_date() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        seconds_of_day / 3600,
        (seconds_of_day / 60) % 60,
        seconds_of_day % 60
    )
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_names() {
        assert_eq!(validate_name(" a.txt ").unwrap(), "a.txt");
        assert_eq!(validate_name(""), Err(NameError::Empty));
        assert_eq!(validate_name("  "), Err(NameError::Empty));
        assert_eq!(validate_name("."), Err(NameError::Dot));
        assert_eq!(validate_name(".."), Err(NameError::Dot));
        assert_eq!(validate_name("a/b"), Err(NameError::Separator));
        assert_eq!(validate_name("a\\b"), Err(NameError::Separator));
        assert_eq!(validate_name("a\0b"), Err(NameError::Nul));
    }

    #[test]
    fn creates_files_and_directories_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let file = create_file(dir.path(), "new.txt").unwrap();
        assert!(file.is_file());
        assert_eq!(fs::read(&file).unwrap(), b"");
        assert!(create_file(dir.path(), "new.txt").is_err());

        let folder = create_dir(dir.path(), "sub").unwrap();
        assert!(folder.is_dir());
        assert!(create_dir(dir.path(), "sub").is_err());
        assert!(create_file(dir.path(), "bad/name").is_err());
    }

    #[test]
    fn renames_and_rejects_existing_targets() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.txt");
        let to = dir.path().join("b.txt");
        fs::write(&from, "x").unwrap();
        rename_path(&from, &to).unwrap();
        assert!(!from.exists());
        assert!(to.exists());

        fs::write(&from, "y").unwrap();
        assert!(rename_path(&from, &to).is_err());
        assert_eq!(fs::read(&from).unwrap(), b"y");
        assert_eq!(fs::read(&to).unwrap(), b"x");

        // A rename never moves an item into another directory.
        let other = dir.path().join("other");
        fs::create_dir(&other).unwrap();
        assert!(rename_path(&from, &other.join("a.txt")).is_err());
        assert!(rename_path(&from, Path::new("../escape.txt")).is_err());
    }

    #[test]
    fn permanent_delete_removes_trees() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c.txt");
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        fs::write(&nested, "x").unwrap();
        delete_permanently(&dir.path().join("a")).unwrap();
        assert!(!dir.path().join("a").exists());
    }

    #[cfg(unix)]
    #[test]
    fn trash_moves_files_into_the_xdg_trash() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_DATA_HOME", home.path());
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("old.txt");
        fs::write(&file, "content").unwrap();
        trash_path(&file).unwrap();
        assert!(!file.exists());
        let trashed = home.path().join("Trash/files/old.txt");
        assert_eq!(fs::read(&trashed).unwrap(), b"content");
        let info = home.path().join("Trash/info/old.txt.trashinfo");
        let info = fs::read_to_string(info).unwrap();
        assert!(info.contains("[Trash Info]"));
        assert!(info.contains("Path="));
        assert!(info.contains("DeletionDate="));
    }

    #[test]
    fn percent_encoding_keeps_slashes_and_escapes_spaces() {
        assert_eq!(percent_encode("/a/b c"), "/a/b%20c");
        assert_eq!(percent_encode("é"), "%C3%A9");
    }

    #[test]
    fn civil_dates_are_correct() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
    }

    #[test]
    fn unique_names_avoid_collisions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        assert_eq!(unique_path(dir.path(), "a.txt"), dir.path().join("a.1.txt"));
        assert_eq!(unique_path(dir.path(), "b.txt"), dir.path().join("b.txt"));
    }
}

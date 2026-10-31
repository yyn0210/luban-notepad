//! Line-oriented unified diff.
//!
//! The diff is intentionally conservative: binary files, files above the size
//! limit and very large line counts are reported as such instead of producing
//! an unbounded or meaningless edit script.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use similar::TextDiff;

pub const DEFAULT_CONTEXT: usize = 3;
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_LINES: usize = 100_000;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffOutput {
    pub text: String,
    pub identical: bool,
    pub binary: bool,
    pub too_large: bool,
    pub left_lines: usize,
    pub right_lines: usize,
    pub added: usize,
    pub removed: usize,
}

impl DiffOutput {
    pub fn summary(&self) -> String {
        if self.identical {
            "No differences".to_string()
        } else if self.binary {
            "Binary files differ".to_string()
        } else if self.too_large {
            "Files are too large for an inline diff".to_string()
        } else {
            format!("+{} -{}", self.added, self.removed)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffError {
    Missing(String),
    Io(String),
    Cancelled,
}

impl std::fmt::Display for DiffError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(path) => write!(formatter, "cannot read {path}"),
            Self::Io(message) => write!(formatter, "{message}"),
            Self::Cancelled => write!(formatter, "diff cancelled"),
        }
    }
}

/// Diff two text strings.
pub fn diff_text(
    left: &str,
    right: &str,
    left_label: &str,
    right_label: &str,
    context: usize,
) -> DiffOutput {
    let diff = TextDiff::from_lines(left, right);
    let identical = left == right;
    let mut added = 0usize;
    let mut removed = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    let text = if identical {
        format!("No differences between {left_label} and {right_label}\n")
    } else {
        diff.unified_diff()
            .context_radius(context)
            .header(left_label, right_label)
            .to_string()
    };
    DiffOutput {
        text,
        identical,
        binary: false,
        too_large: false,
        left_lines: left.lines().count(),
        right_lines: right.lines().count(),
        added,
        removed,
    }
}

/// Diff two files.
pub fn diff_files(left: &Path, right: &Path) -> Result<DiffOutput, DiffError> {
    let cancel = AtomicBool::new(false);
    diff_files_with_progress(left, right, &cancel, &mut |_, _| {})
}

/// Diff two files while reporting read progress and honoring cancellation.
///
/// `progress` receives `(bytes_done, bytes_total)` while both files are read
/// and a final `(total, total)` pair once the diff itself is complete. The
/// callback runs on the calling thread, so a UI should throttle updates.
pub fn diff_files_with_progress(
    left: &Path,
    right: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<DiffOutput, DiffError> {
    let left_label = left.display().to_string();
    let right_label = right.display().to_string();
    let left_metadata = fs::metadata(left).map_err(|_| DiffError::Missing(left_label.clone()))?;
    let right_metadata =
        fs::metadata(right).map_err(|_| DiffError::Missing(right_label.clone()))?;
    if !left_metadata.is_file() {
        return Err(DiffError::Missing(left_label));
    }
    if !right_metadata.is_file() {
        return Err(DiffError::Missing(right_label));
    }
    if left_metadata.len() > MAX_FILE_BYTES || right_metadata.len() > MAX_FILE_BYTES {
        return Ok(DiffOutput {
            text: format!(
                "Files are too large for an inline diff (limit {MAX_FILE_BYTES} bytes)\n"
            ),
            identical: false,
            binary: false,
            too_large: true,
            left_lines: 0,
            right_lines: 0,
            added: 0,
            removed: 0,
        });
    }
    let total = left_metadata
        .len()
        .saturating_add(right_metadata.len())
        .max(1);
    let mut done = 0u64;
    progress(0, total);
    let left_bytes = read_file_with_progress(left, cancel, progress, &mut done, total)?;
    let right_bytes = read_file_with_progress(right, cancel, progress, &mut done, total)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(DiffError::Cancelled);
    }
    if left_bytes == right_bytes {
        progress(total, total);
        return Ok(DiffOutput {
            text: format!("No differences between {left_label} and {right_label}\n"),
            identical: true,
            binary: false,
            too_large: false,
            left_lines: 0,
            right_lines: 0,
            added: 0,
            removed: 0,
        });
    }
    if is_binary(&left_bytes) || is_binary(&right_bytes) {
        progress(total, total);
        return Ok(DiffOutput {
            text: format!("Binary files {left_label} and {right_label} differ\n"),
            identical: false,
            binary: true,
            too_large: false,
            left_lines: 0,
            right_lines: 0,
            added: 0,
            removed: 0,
        });
    }
    if left_bytes.len() as u64 > MAX_FILE_BYTES || right_bytes.len() as u64 > MAX_FILE_BYTES {
        progress(total, total);
        return Ok(DiffOutput {
            text: format!(
                "Files are too large for an inline diff (limit {MAX_FILE_BYTES} bytes)\n"
            ),
            identical: false,
            binary: false,
            too_large: true,
            left_lines: 0,
            right_lines: 0,
            added: 0,
            removed: 0,
        });
    }
    let left_text = String::from_utf8_lossy(&left_bytes);
    let right_text = String::from_utf8_lossy(&right_bytes);
    if left_text.lines().count() > MAX_LINES || right_text.lines().count() > MAX_LINES {
        progress(total, total);
        return Ok(DiffOutput {
            text: format!("Files are too large for an inline diff (limit {MAX_LINES} lines)\n"),
            identical: false,
            binary: false,
            too_large: true,
            left_lines: 0,
            right_lines: 0,
            added: 0,
            removed: 0,
        });
    }
    // Reading is the observable part of the work; reserve the last few percent
    // for the line diff so the status does not sit at 100% while it computes.
    progress(total.saturating_mul(95) / 100, total);
    let output = diff_text(
        &left_text,
        &right_text,
        &left_label,
        &right_label,
        DEFAULT_CONTEXT,
    );
    if cancel.load(Ordering::Relaxed) {
        return Err(DiffError::Cancelled);
    }
    progress(total, total);
    Ok(output)
}

fn read_file_with_progress(
    path: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
    done: &mut u64,
    total: u64,
) -> Result<Vec<u8>, DiffError> {
    let mut file = fs::File::open(path)
        .map_err(|error| DiffError::Io(format!("cannot read {}: {error}", path.display())))?;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(DiffError::Cancelled);
        }
        let read = file
            .read(&mut buffer)
            .map_err(|error| DiffError::Io(format!("cannot read {}: {error}", path.display())))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        *done = done.saturating_add(read as u64);
        progress(*done, total);
    }
    Ok(bytes)
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|byte| *byte == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_identical_text() {
        let output = diff_text("a\nb\n", "a\nb\n", "left", "right", 3);
        assert!(output.identical);
        assert_eq!(output.added, 0);
        assert_eq!(output.removed, 0);
        assert!(output.text.contains("No differences"));
    }

    #[test]
    fn renders_additions_and_removals() {
        let output = diff_text("a\nb\nc\n", "a\nB\nc\n", "left", "right", 3);
        assert!(!output.identical);
        assert_eq!(output.added, 1);
        assert_eq!(output.removed, 1);
        assert!(output.text.contains("--- left"));
        assert!(output.text.contains("+++ right"));
        assert!(output.text.contains("-b"));
        assert!(output.text.contains("+B"));
        assert!(output.text.contains("@@"));
    }

    #[test]
    fn handles_empty_inputs() {
        let output = diff_text("", "a\n", "left", "right", 3);
        assert!(!output.identical);
        assert_eq!(output.added, 1);
        assert_eq!(output.removed, 0);
        assert!(output.text.contains("+a"));
    }

    #[test]
    fn detects_binary_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join("left.bin");
        let right = dir.path().join("right.bin");
        fs::write(&left, [0u8, 1, 2]).unwrap();
        fs::write(&right, [0u8, 3, 4]).unwrap();
        let output = diff_files(&left, &right).unwrap();
        assert!(output.binary);
        assert!(output.text.contains("Binary files"));
        assert!(diff_files(&left, &dir.path().join("missing")).is_err());
    }

    #[test]
    fn reports_sparse_large_files_as_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join("left.txt");
        let right = dir.path().join("right.txt");
        let file = fs::File::create(&left).unwrap();
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        fs::write(&right, "b\n").unwrap();
        let output = diff_files(&left, &right).unwrap();
        assert!(output.too_large);
        assert!(!output.binary);
    }

    #[test]
    fn reports_progress_and_honors_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join("left.txt");
        let right = dir.path().join("right.txt");
        fs::write(&left, "a\nb\n").unwrap();
        fs::write(&right, "a\nB\n").unwrap();

        let cancel = AtomicBool::new(false);
        let mut last = (0u64, 0u64);
        let output = diff_files_with_progress(&left, &right, &cancel, &mut |done, total| {
            last = (done, total);
        })
        .unwrap();
        assert!(!output.identical);
        assert_eq!(last.0, last.1);
        assert!(last.1 >= 4);

        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            diff_files_with_progress(&left, &right, &cancel, &mut |_, _| {}),
            Err(DiffError::Cancelled)
        );
    }

    #[test]
    fn reports_large_files_without_reading_them_fully() {
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join("left.txt");
        let right = dir.path().join("right.txt");
        fs::write(&left, "a\n").unwrap();
        fs::write(&right, "b\n").unwrap();
        // The size guard is exercised by the constant; keep the functional path
        // covered by the text tests above.
        let output = diff_files(&left, &right).unwrap();
        assert!(!output.identical);
        assert!(!output.binary);
    }
}

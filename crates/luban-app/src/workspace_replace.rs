//! Workspace-wide replace preview, stale-file detection and undo records.

use std::path::{Path, PathBuf};

use luban_core::search::{SearchEngine, SearchOptions};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePreview {
    pub path: PathBuf,
    /// Path shown in the preview, relative to its workspace root.
    pub relative: String,
    pub matches: usize,
    /// Up to five `original → replacement` examples.
    pub samples: Vec<(String, String)>,
    /// Fingerprint of the bytes the preview was computed from.
    pub fingerprint: u64,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedFile {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplacePreview {
    pub files: Vec<FilePreview>,
    pub skipped: Vec<SkippedFile>,
    pub total_matches: usize,
    pub truncated: bool,
}

impl ReplacePreview {
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn confirmable(&self) -> bool {
        !self.files.is_empty() && self.total_matches > 0
    }
}

/// One tab restored by "undo workspace replace".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoEntry {
    /// Tab id when the replacement was applied.
    pub tab_id: Option<u64>,
    pub path: Option<PathBuf>,
    /// Complete text before the replacement.
    pub text: String,
    pub modified: bool,
    pub cursor: usize,
    pub anchor: usize,
    /// The tab existed before the replacement.
    pub was_open: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplaceUndo {
    pub entries: Vec<UndoEntry>,
}

/// Build a one-line preview example using the same search engine as the real
/// replacement. Returns `None` when the line contains no match.
pub fn replacement_sample(
    line: &str,
    query: &str,
    options: SearchOptions,
    replacement: &str,
) -> Option<(String, String)> {
    let engine = SearchEngine::new(query.to_string(), options);
    if engine.error().is_some() {
        return None;
    }
    let (replaced, count) = engine.replace_all(line, replacement, None).ok()?;
    (count > 0).then_some((line.to_string(), replaced))
}

/// Fast, stable content fingerprint used to detect files that changed between
/// preview and apply.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Root-relative path with `/` separators, or the absolute path when the file
/// is outside every root.
pub fn relative_path(roots: &[PathBuf], path: &Path) -> String {
    for root in roots {
        if let Ok(relative) = path.strip_prefix(root) {
            return relative.to_string_lossy().replace('\\', "/");
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_only_when_the_line_matches() {
        let sample =
            replacement_sample("let foo = 1;", "foo", SearchOptions::default(), "bar").unwrap();
        assert_eq!(sample.0, "let foo = 1;");
        assert_eq!(sample.1, "let bar = 1;");
        assert!(replacement_sample("nothing", "foo", SearchOptions::default(), "bar").is_none());
    }

    #[test]
    fn samples_follow_regex_and_case_options() {
        let options = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };
        let sample = replacement_sample("abc123", r"\d+", options, "#").unwrap();
        assert_eq!(sample.1, "abc#");

        let options = SearchOptions {
            match_case: true,
            ..SearchOptions::default()
        };
        assert!(replacement_sample("ABC", "abc", options, "x").is_none());
    }

    #[test]
    fn fingerprint_detects_changes() {
        assert_eq!(fingerprint(b"abc"), fingerprint(b"abc"));
        assert_ne!(fingerprint(b"abc"), fingerprint(b"abd"));
        assert_ne!(fingerprint(b""), fingerprint(b"a"));
    }

    #[test]
    fn relative_paths_use_the_first_matching_root() {
        let roots = vec![PathBuf::from("/w"), PathBuf::from("/other")];
        assert_eq!(
            relative_path(&roots, Path::new("/w/src/main.rs")),
            "src/main.rs"
        );
        assert_eq!(
            relative_path(&roots, Path::new("/outside/file.txt")),
            "/outside/file.txt"
        );
    }

    #[test]
    fn preview_requires_matches_to_confirm() {
        let mut preview = ReplacePreview::default();
        assert!(!preview.confirmable());
        preview.files.push(FilePreview {
            path: PathBuf::from("/w/a.txt"),
            relative: "a.txt".into(),
            matches: 0,
            samples: Vec::new(),
            fingerprint: 0,
            size: 0,
        });
        assert!(!preview.confirmable());
        preview.total_matches = 1;
        assert!(preview.confirmable());
    }
}

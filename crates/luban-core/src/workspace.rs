//! Folder browsing and "Find in Files".

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use walkdir::{DirEntry, WalkDir};

use crate::encoding::{decode, looks_binary};
use crate::error::{Error, Result};
use crate::search::{SearchEngine, SearchOptions};
use crate::text::{char_to_byte, LineIndex};

/// An entry in the folder explorer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

/// List immediate children of `dir`, directories first and names sorted
/// case-insensitively.
pub fn list_directory(dir: impl AsRef<Path>, show_hidden: bool) -> Result<Vec<FileEntry>> {
    let dir = dir.as_ref();
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if !show_hidden && is_hidden(&path, &name) {
            continue;
        }
        let is_dir = path.is_dir();
        entries.push(FileEntry { path, name, is_dir });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

pub fn is_hidden(path: &Path, name: &str) -> bool {
    if name.starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if let Ok(meta) = fs::metadata(path) {
            const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
            if meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0 {
                return true;
            }
        }
    }
    let _ = path;
    false
}

#[derive(Clone, Debug)]
pub struct FindInFilesOptions {
    pub query: String,
    pub folder: PathBuf,
    /// Comma/semicolon/space separated wildcards such as `*.rs,*.toml`.
    pub masks: String,
    pub options: SearchOptions,
    pub max_results: usize,
    pub max_file_size: u64,
    pub include_hidden: bool,
    pub skip_common_ignored_folders: bool,
}

impl Default for FindInFilesOptions {
    fn default() -> Self {
        Self {
            query: String::new(),
            folder: PathBuf::from("."),
            masks: String::new(),
            options: SearchOptions::default(),
            max_results: 2000,
            max_file_size: 16 * 1024 * 1024,
            include_hidden: false,
            skip_common_ignored_folders: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMatch {
    pub path: PathBuf,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number (characters, not bytes).
    pub column: usize,
    pub line_text: String,
    pub match_text: String,
    pub match_len: usize,
}

#[derive(Clone, Debug, Default)]
pub struct FindInFilesOutcome {
    pub matches: Vec<FileMatch>,
    pub files_scanned: usize,
    pub files_with_matches: usize,
    pub truncated: bool,
    pub cancelled: bool,
}

const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    "node_modules",
    "target",
    "__pycache__",
    ".idea",
    ".vscode",
    "dist",
];

/// Search a folder tree. The callback is invoked with the number of files
/// scanned after each file, allowing a GUI to display progress.
pub fn search_in_files(
    options: &FindInFilesOptions,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(usize),
) -> Result<FindInFilesOutcome> {
    if options.query.is_empty() {
        return Err(Error::Other(crate::i18n::tr("core.search.empty_text")));
    }
    if !options.folder.is_dir() {
        return Err(Error::InvalidPath(options.folder.clone()));
    }
    let engine = SearchEngine::new(options.query.clone(), options.options);
    if let Some(err) = engine.error() {
        return Err(Error::Other(crate::i18n::tr_args(
            "core.search.invalid_regex",
            &[("error", err.to_string())],
        )));
    }

    let mut outcome = FindInFilesOutcome::default();
    let masks = Masks::new(&options.masks);
    let mut walker = WalkDir::new(&options.folder)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| should_descend(entry, options));

    loop {
        if cancel.load(Ordering::Relaxed) {
            outcome.cancelled = true;
            break;
        }
        let Some(entry) = walker.next() else { break };
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !masks.matches(path.file_name().and_then(|n| n.to_str()).unwrap_or("")) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > options.max_file_size {
            continue;
        }
        let Ok(bytes) = fs::read(path) else { continue };
        if looks_binary(&bytes) {
            continue;
        }
        let decoded = decode(&bytes);
        let matches = engine.find_all(
            &decoded.text,
            None,
            options.max_results.saturating_sub(outcome.matches.len()),
        );
        outcome.files_scanned += 1;
        on_progress(outcome.files_scanned);
        if matches.is_empty() {
            continue;
        }
        outcome.files_with_matches += 1;
        let index = LineIndex::new(&decoded.text);
        for m in matches {
            let (line, column) = index.line_col(m.start);
            let line_text = index.line_text(&decoded.text, line.saturating_sub(1));
            let b0 = char_to_byte(&decoded.text, m.start);
            let b1 = char_to_byte(&decoded.text, m.end);
            outcome.matches.push(FileMatch {
                path: path.to_path_buf(),
                line,
                column,
                line_text: line_text.trim_end_matches(['\r', '\n']).to_string(),
                match_text: decoded.text[b0..b1].to_string(),
                match_len: m.len(),
            });
            if outcome.matches.len() >= options.max_results {
                outcome.truncated = true;
                break;
            }
        }
        if outcome.truncated {
            break;
        }
    }

    outcome.matches.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.column.cmp(&b.column))
    });
    Ok(outcome)
}

fn should_descend(entry: &DirEntry, options: &FindInFilesOptions) -> bool {
    let name = entry.file_name().to_string_lossy();
    if entry.depth() == 0 {
        return true;
    }
    if !options.include_hidden && name.starts_with('.') {
        return false;
    }
    if options.skip_common_ignored_folders
        && entry.file_type().is_dir()
        && IGNORED_DIRS.iter().any(|d| d.eq_ignore_ascii_case(&name))
    {
        return false;
    }
    true
}

struct Masks {
    patterns: Vec<String>,
}

impl Masks {
    fn new(raw: &str) -> Self {
        let patterns = raw
            .split([',', ';', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase())
            .collect();
        Self { patterns }
    }

    fn matches(&self, name: &str) -> bool {
        if self.patterns.is_empty() {
            return true;
        }
        let name = name.to_lowercase();
        self.patterns.iter().any(|p| wildcard_match(p, &name))
    }
}

/// Minimal glob matcher supporting `*` and `?`.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut resume = 0usize;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            resume = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn lists_directories_first() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "b.txt", "b");
        write(dir.path(), "a.txt", "a");
        fs::create_dir(dir.path().join("zdir")).unwrap();
        fs::create_dir(dir.path().join("adir")).unwrap();
        let entries = list_directory(dir.path(), false).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["adir", "zdir", "a.txt", "b.txt"]);
        assert!(entries[0].is_dir);
    }

    #[test]
    fn hides_dot_files() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".secret", "x");
        write(dir.path(), "visible", "x");
        let visible = list_directory(dir.path(), false).unwrap();
        assert_eq!(visible.len(), 1);
        let all = list_directory(dir.path(), true).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn wildcard_matching() {
        assert!(wildcard_match("*.rs", "main.rs"));
        assert!(!wildcard_match("*.rs", "main.py"));
        assert!(wildcard_match("read?e.*", "readme.md"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
        assert!(!wildcard_match("a*b*c", "aXXbYY"));
    }

    #[test]
    fn finds_in_files_with_masks() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.rs",
            "fn main() {\n    println!(\"hi\");\n}\n",
        );
        write(dir.path(), "b.txt", "fn not_a_rust_file\n");
        write(dir.path(), "sub/c.rs", "pub fn other() {}\n");
        let options = FindInFilesOptions {
            query: "fn".into(),
            folder: dir.path().to_path_buf(),
            masks: "*.rs".into(),
            ..FindInFilesOptions::default()
        };
        let outcome = search_in_files(&options, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(outcome.matches.len(), 2);
        assert!(outcome
            .matches
            .iter()
            .all(|m| m.path.extension().unwrap() == "rs"));
        assert!(outcome.matches.iter().any(|m| m.path.ends_with("sub/c.rs")));
        assert_eq!(outcome.files_with_matches, 2);
    }

    #[test]
    fn reports_line_and_column() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "first\nsecond target\n");
        let options = FindInFilesOptions {
            query: "target".into(),
            folder: dir.path().to_path_buf(),
            ..FindInFilesOptions::default()
        };
        let outcome = search_in_files(&options, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(outcome.matches.len(), 1);
        let m = &outcome.matches[0];
        assert_eq!((m.line, m.column), (2, 8));
        assert_eq!(m.line_text, "second target");
        assert_eq!(m.match_text, "target");
    }

    #[test]
    fn skips_ignored_folders_and_binaries() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "target/generated.rs", "needle");
        write(dir.path(), "ok.rs", "needle");
        fs::write(dir.path().join("bin.dat"), [0u8, 1, 2, 0]).unwrap();
        let options = FindInFilesOptions {
            query: "needle".into(),
            folder: dir.path().to_path_buf(),
            ..FindInFilesOptions::default()
        };
        let outcome = search_in_files(&options, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(outcome.matches.len(), 1);
        assert!(outcome.matches[0].path.ends_with("ok.rs"));
    }

    #[test]
    fn respects_result_limit() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "x x x x x\n");
        let options = FindInFilesOptions {
            query: "x".into(),
            folder: dir.path().to_path_buf(),
            max_results: 3,
            ..FindInFilesOptions::default()
        };
        let outcome = search_in_files(&options, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(outcome.matches.len(), 3);
        assert!(outcome.truncated);
    }

    #[test]
    fn cancellation_is_honored() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "x");
        let options = FindInFilesOptions {
            query: "x".into(),
            folder: dir.path().to_path_buf(),
            ..FindInFilesOptions::default()
        };
        let cancel = AtomicBool::new(true);
        let outcome = search_in_files(&options, &cancel, &mut |_| {}).unwrap();
        assert!(outcome.cancelled);
        assert!(outcome.matches.is_empty());
    }

    #[test]
    fn invalid_regex_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let options = FindInFilesOptions {
            query: "(bad".into(),
            folder: dir.path().to_path_buf(),
            options: SearchOptions {
                regex: true,
                ..SearchOptions::default()
            },
            ..FindInFilesOptions::default()
        };
        assert!(search_in_files(&options, &AtomicBool::new(false), &mut |_| {}).is_err());
    }

    #[test]
    fn supports_unicode_matches() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "你好世界\n");
        let options = FindInFilesOptions {
            query: "世界".into(),
            folder: dir.path().to_path_buf(),
            ..FindInFilesOptions::default()
        };
        let outcome = search_in_files(&options, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(outcome.matches.len(), 1);
        assert_eq!(outcome.matches[0].column, 3);
        assert_eq!(outcome.matches[0].match_text, "世界");
    }
}

//! Row model, sorting, search matching and display formatting for the picker.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use luban_core::workspace::{wildcard_match, FileEntryInfo};

/// What a rendered row represents. Row 0 is always the parent entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerRowKind {
    Parent,
    Directory,
    File,
}

/// One display row. The list is rebuilt only when the listing, filter, search
/// or sort changes, so the virtual list can read a cheap snapshot each frame.
#[derive(Clone, Debug, PartialEq)]
pub struct PickerRow {
    pub kind: PickerRowKind,
    pub path: PathBuf,
    pub name: String,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub is_symlink: bool,
    /// Byte range of the case-insensitive substring match, when searching.
    pub match_range: Option<(usize, usize)>,
}

impl PickerRow {
    pub fn parent(dir: &Path, name: impl Into<String>) -> Self {
        Self {
            kind: PickerRowKind::Parent,
            path: dir.to_path_buf(),
            name: name.into(),
            size: None,
            modified: None,
            is_symlink: false,
            match_range: None,
        }
    }

    pub fn from_entry(entry: &FileEntryInfo, match_range: Option<(usize, usize)>) -> Self {
        Self {
            kind: if entry.is_dir {
                PickerRowKind::Directory
            } else {
                PickerRowKind::File
            },
            path: entry.path.clone(),
            name: entry.name.clone(),
            size: entry.size,
            modified: entry.modified,
            is_symlink: entry.is_symlink,
            match_range,
        }
    }

    pub fn is_file(&self) -> bool {
        self.kind == PickerRowKind::File
    }

    pub fn is_dir(&self) -> bool {
        self.kind == PickerRowKind::Directory
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortSpec {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            descending: false,
        }
    }
}

/// Natural, case-insensitive name comparison: `file2` sorts before `file10`.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut left = a.chars().peekable();
    let mut right = b.chars().peekable();
    loop {
        match (left.peek().copied(), right.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(l), Some(r)) => {
                if l.is_ascii_digit() && r.is_ascii_digit() {
                    let lnum = take_number(&mut left);
                    let rnum = take_number(&mut right);
                    let ltrim = lnum.trim_start_matches('0');
                    let rtrim = rnum.trim_start_matches('0');
                    let ord = ltrim
                        .len()
                        .cmp(&rtrim.len())
                        .then_with(|| ltrim.cmp(rtrim))
                        .then_with(|| lnum.len().cmp(&rnum.len()));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    let lord = l.to_lowercase().cmp(r.to_lowercase());
                    if lord != Ordering::Equal {
                        return lord;
                    }
                    left.next();
                    right.next();
                }
            }
        }
    }
}

fn take_number(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut number = String::new();
    while let Some(ch) = chars.peek().copied() {
        if !ch.is_ascii_digit() {
            break;
        }
        number.push(ch);
        chars.next();
    }
    number
}

/// Directories always lead, and are always ordered by natural name. Files use
/// the active column and direction. Ties fall back to the natural name.
pub fn compare_entries(a: &FileEntryInfo, b: &FileEntryInfo, sort: SortSpec) -> Ordering {
    let dir_order = b.is_dir.cmp(&a.is_dir);
    if dir_order != Ordering::Equal {
        return dir_order;
    }
    if a.is_dir {
        return natural_cmp(&a.name, &b.name);
    }
    let ord = match sort.key {
        SortKey::Name => natural_cmp(&a.name, &b.name),
        SortKey::Size => a.size.unwrap_or(0).cmp(&b.size.unwrap_or(0)),
        SortKey::Modified => a.modified.cmp(&b.modified),
    };
    let ord = if sort.descending { ord.reverse() } else { ord };
    ord.then_with(|| natural_cmp(&a.name, &b.name))
}

/// Case-insensitive substring search. A query containing `*` or `?` is treated
/// as a wildcard pattern.
pub fn search_matches(name: &str, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    let name = name.to_lowercase();
    if query.contains('*') || query.contains('?') {
        return wildcard_match(&query.to_lowercase(), &name);
    }
    name.contains(&query.to_lowercase())
}

/// Byte range of the literal match in `name`, used to highlight search hits.
/// Wildcards and Unicode case-folding that changes byte length only hide the
/// highlight; matching itself remains correct.
pub fn search_match_range(name: &str, query: &str) -> Option<(usize, usize)> {
    let query = query.trim();
    if query.is_empty() || query.contains('*') || query.contains('?') {
        return None;
    }
    let lower_name = name.to_lowercase();
    let lower_query = query.to_lowercase();
    if lower_name.len() != name.len() {
        return None;
    }
    lower_name
        .find(&lower_query)
        .map(|start| (start, start + lower_query.len()))
}

/// Human-readable file size. Directories and unknown sizes render as an em
/// dash so the column stays aligned.
pub fn format_size(size: Option<u64>) -> String {
    let Some(size) = size else {
        return "—".to_string();
    };
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    if size < 1024 {
        return format!("{size} B");
    }
    let (value, unit) = if (size as f64) < MIB {
        (size as f64 / KIB, "KB")
    } else if (size as f64) < GIB {
        (size as f64 / MIB, "MB")
    } else {
        (size as f64 / GIB, "GB")
    };
    let mut text = format!("{value:.1}");
    if text.ends_with(".0") {
        text.truncate(text.len() - 2);
    }
    format!("{text} {unit}")
}

/// Local-time `YYYY-MM-DD HH:MM` for the modified column.
pub fn format_modified(modified: Option<SystemTime>) -> String {
    let Some(modified) = modified else {
        return "—".to_string();
    };
    let datetime: chrono::DateTime<chrono::Local> = modified.into();
    datetime.format("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool, size: u64) -> FileEntryInfo {
        FileEntryInfo {
            path: PathBuf::from("/tmp").join(name),
            name: name.to_string(),
            is_dir,
            is_symlink: false,
            size: (!is_dir).then_some(size),
            modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(size)),
        }
    }

    #[test]
    fn natural_names_sort_numbers_by_value() {
        assert_eq!(natural_cmp("file2.txt", "file10.txt"), Ordering::Less);
        assert_eq!(natural_cmp("file10.txt", "file2.txt"), Ordering::Greater);
        assert_eq!(natural_cmp("File2.TXT", "file2.txt"), Ordering::Equal);
        assert_eq!(natural_cmp("a", "a1"), Ordering::Less);
    }

    #[test]
    fn directories_lead_regardless_of_the_sort_column() {
        let mut entries = [
            entry("b.txt", false, 10),
            entry("zdir", true, 0),
            entry("a.txt", false, 20),
            entry("adir", true, 0),
        ];
        entries.sort_by(|a, b| compare_entries(a, b, SortSpec::default()));
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["adir", "zdir", "a.txt", "b.txt"]);
    }

    #[test]
    fn file_sorting_honours_size_direction() {
        let mut entries = [
            entry("small.txt", false, 1),
            entry("large.txt", false, 100),
            entry("middle.txt", false, 50),
        ];
        let sort = SortSpec {
            key: SortKey::Size,
            descending: true,
        };
        entries.sort_by(|a, b| compare_entries(a, b, sort));
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["large.txt", "middle.txt", "small.txt"]);
    }

    #[test]
    fn search_supports_substrings_and_wildcards() {
        assert!(search_matches("Main.RS", "main"));
        assert!(search_matches("Main.RS", "*.rs"));
        assert!(search_matches("notes.md", "notes.*"));
        assert!(!search_matches("notes.md", "*.rs"));
        assert!(search_matches("anything", "  "));
        assert_eq!(search_match_range("Main.RS", "in"), Some((2, 4)));
        assert_eq!(search_match_range("Main.RS", "*.rs"), None);
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(format_size(None), "—");
        assert_eq!(format_size(Some(512)), "512 B");
        assert_eq!(format_size(Some(1024)), "1 KB");
        assert_eq!(format_size(Some(1536)), "1.5 KB");
        assert_eq!(format_size(Some(1024 * 1024)), "1 MB");
    }
}

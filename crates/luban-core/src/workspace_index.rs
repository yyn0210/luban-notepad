//! Background workspace index.
//!
//! The index walks every workspace root once, honouring `.gitignore` when
//! requested, and records files and directories for the file tree, quick open
//! and workspace search. It never reads file contents and never modifies the
//! file system.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ignore::WalkBuilder;

use crate::workspace::is_hidden;

/// Default cap for one workspace root. Larger trees keep the partial index.
pub const MAX_INDEX_FILES: usize = 200_000;

#[derive(Clone, Copy, Debug)]
pub struct IndexOptions {
    pub respect_gitignore: bool,
    pub include_hidden: bool,
    pub max_files: usize,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            respect_gitignore: true,
            include_hidden: false,
            max_files: MAX_INDEX_FILES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedEntry {
    pub path: PathBuf,
    pub root: PathBuf,
    /// Path relative to the root, using `/` separators.
    pub relative: String,
    pub name: String,
    pub is_dir: bool,
    pub depth: usize,
    pub parent: Option<PathBuf>,
}

/// The result of one index build.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceIndex {
    pub roots: Vec<PathBuf>,
    pub entries: Vec<IndexedEntry>,
    children: HashMap<PathBuf, Vec<usize>>,
    pub file_count: usize,
    pub truncated_roots: Vec<PathBuf>,
    pub cancelled: bool,
    pub respect_gitignore: bool,
    pub include_hidden: bool,
}

impl WorkspaceIndex {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Files only, in walk order.
    pub fn files(&self) -> impl Iterator<Item = &IndexedEntry> {
        self.entries.iter().filter(|entry| !entry.is_dir)
    }

    pub fn files_vec(&self) -> Vec<&IndexedEntry> {
        self.files().collect()
    }

    /// Children of `dir` with directories first and names sorted
    /// case-insensitively.
    pub fn children_of(&self, dir: &Path) -> impl Iterator<Item = &IndexedEntry> {
        self.children
            .get(dir)
            .into_iter()
            .flatten()
            .map(|index| &self.entries[*index])
    }

    pub fn contains_dir(&self, dir: &Path) -> bool {
        self.children.contains_key(dir)
    }

    /// Find an indexed file by absolute path.
    pub fn file_at(&self, path: &Path) -> Option<&IndexedEntry> {
        self.entries
            .iter()
            .find(|entry| !entry.is_dir && crate::settings::paths_equal(&entry.path, path))
    }

    pub fn relative_of(&self, path: &Path) -> Option<String> {
        for root in &self.roots {
            if let Ok(relative) = path.strip_prefix(root) {
                return Some(relative.to_string_lossy().replace('\\', "/"));
            }
        }
        None
    }

    fn finish(&mut self) {
        let entries = &self.entries;
        for indices in self.children.values_mut() {
            indices.sort_by(|a, b| {
                let a = &entries[*a];
                let b = &entries[*b];
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
        }
    }
}

/// Build the index for `roots`. `cancel` is polled between entries so a
/// workspace switch can abandon an in-flight build.
pub fn build_index(
    roots: &[PathBuf],
    options: IndexOptions,
    cancel: &AtomicBool,
) -> WorkspaceIndex {
    let mut index = WorkspaceIndex {
        roots: roots.to_vec(),
        respect_gitignore: options.respect_gitignore,
        include_hidden: options.include_hidden,
        ..WorkspaceIndex::default()
    };
    for root in roots {
        if cancel.load(Ordering::Relaxed) {
            index.cancelled = true;
            break;
        }
        if !root.is_dir() {
            continue;
        }
        let mut files_in_root = 0usize;
        let mut truncated = false;
        let walker = build_walker(root, options).build();
        for entry in walker {
            if cancel.load(Ordering::Relaxed) {
                index.cancelled = true;
                break;
            }
            let Ok(entry) = entry else { continue };
            let depth = entry.depth();
            if depth == 0 {
                continue;
            }
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            let is_dir = file_type.is_dir();
            if !is_dir && !file_type.is_file() {
                // Symlinks are never followed, and other special files are
                // invisible to navigation and search.
                continue;
            }
            if !options.include_hidden
                && is_hidden(entry.path(), &entry.file_name().to_string_lossy())
            {
                continue;
            }
            if !is_dir {
                if files_in_root >= options.max_files {
                    truncated = true;
                    break;
                }
                files_in_root += 1;
            }
            let path = entry.path().to_path_buf();
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let name = entry.file_name().to_string_lossy().into_owned();
            index.entries.push(IndexedEntry {
                path: path.clone(),
                root: root.clone(),
                relative,
                name,
                is_dir,
                depth,
                parent: path.parent().map(Path::to_path_buf),
            });
            let position = index.entries.len() - 1;
            let parent = path.parent().map(Path::to_path_buf);
            if let Some(parent) = parent {
                index.children.entry(parent).or_default().push(position);
            }
        }
        index.file_count += files_in_root;
        if truncated {
            index.truncated_roots.push(root.clone());
        }
        if index.cancelled {
            break;
        }
    }
    index.finish();
    index
}

fn build_walker(root: &Path, options: IndexOptions) -> WalkBuilder {
    let respect = options.respect_gitignore;
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!options.include_hidden)
        .follow_links(false)
        .git_ignore(respect)
        .git_global(respect)
        .git_exclude(respect)
        .ignore(false)
        .require_git(false)
        .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".git");
    builder
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
    fn collects_files_and_directories_with_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/main.rs", "fn main() {}");
        write(dir.path(), "src/ignored.log", "noise");
        write(dir.path(), "target/generated.rs", "gen");
        write(dir.path(), "README.md", "readme");
        fs::write(dir.path().join(".gitignore"), "*.log\ntarget/\n").unwrap();
        let index = build_index(
            &[dir.path().to_path_buf()],
            IndexOptions::default(),
            &AtomicBool::new(false),
        );
        let mut files: Vec<_> = index.files().map(|file| file.relative.as_str()).collect();
        files.sort();
        assert_eq!(files, ["README.md", "src/main.rs"]);
        assert!(index.contains_dir(&dir.path().join("src")));
        assert!(!index.contains_dir(&dir.path().join("target")));
        assert_eq!(index.file_count, 2);
        assert!(index.truncated_roots.is_empty());
    }

    #[test]
    fn disabling_gitignore_keeps_ignored_files() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.log", "x");
        write(dir.path(), "b.rs", "x");
        fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        let index = build_index(
            &[dir.path().to_path_buf()],
            IndexOptions {
                respect_gitignore: false,
                ..IndexOptions::default()
            },
            &AtomicBool::new(false),
        );
        let mut files: Vec<_> = index.files().map(|file| file.name.as_str()).collect();
        files.sort();
        // `.gitignore` itself stays hidden because include_hidden is false.
        assert_eq!(files, ["a.log", "b.rs"]);
    }

    #[test]
    fn hidden_files_follow_the_option() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".secret", "x");
        write(dir.path(), "visible", "x");
        let index = build_index(
            &[dir.path().to_path_buf()],
            IndexOptions::default(),
            &AtomicBool::new(false),
        );
        assert_eq!(index.file_count, 1);
        let all = build_index(
            &[dir.path().to_path_buf()],
            IndexOptions {
                include_hidden: true,
                ..IndexOptions::default()
            },
            &AtomicBool::new(false),
        );
        assert_eq!(all.file_count, 2);
    }

    #[test]
    fn children_are_sorted_directories_first() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "b.txt", "x");
        write(dir.path(), "a.txt", "x");
        write(dir.path(), "zdir/x", "x");
        let index = build_index(
            &[dir.path().to_path_buf()],
            IndexOptions::default(),
            &AtomicBool::new(false),
        );
        let names: Vec<_> = index
            .children_of(dir.path())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["zdir", "a.txt", "b.txt"]);
    }

    #[test]
    fn truncates_at_the_file_cap() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..10 {
            write(dir.path(), &format!("f{index}.txt"), "x");
        }
        let index = build_index(
            &[dir.path().to_path_buf()],
            IndexOptions {
                max_files: 4,
                ..IndexOptions::default()
            },
            &AtomicBool::new(false),
        );
        assert_eq!(index.file_count, 4);
        assert_eq!(index.truncated_roots, vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn cancellation_stops_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "x");
        let cancel = AtomicBool::new(true);
        let index = build_index(
            &[dir.path().to_path_buf()],
            IndexOptions::default(),
            &cancel,
        );
        assert!(index.cancelled);
        assert!(index.entries.is_empty());
    }

    #[test]
    fn relative_paths_use_forward_slashes() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a/b/c.txt", "x");
        let index = build_index(
            &[dir.path().to_path_buf()],
            IndexOptions::default(),
            &AtomicBool::new(false),
        );
        let file = index.files().next().unwrap();
        assert_eq!(file.relative, "a/b/c.txt");
        assert_eq!(
            index.relative_of(&dir.path().join("a").join("b").join("c.txt")),
            Some("a/b/c.txt".to_string())
        );
    }
}

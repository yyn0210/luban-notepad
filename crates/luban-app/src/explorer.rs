use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use luban_core::workspace::{list_directory, FileEntry};
use luban_core::workspace_index::WorkspaceIndex;

/// One flattened tree row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub depth: usize,
    /// Root the entry belongs to, used for root-specific actions.
    pub root: PathBuf,
}

#[derive(Debug, Default)]
pub struct ExplorerState {
    /// Workspace roots in display order.
    pub roots: Vec<PathBuf>,
    pub expanded: HashSet<PathBuf>,
    pub selected: Option<PathBuf>,
    /// Direct listing cache used before the index is ready.
    cache: HashMap<PathBuf, Vec<FileEntry>>,
    pub width: f32,
}

impl ExplorerState {
    pub fn new() -> Self {
        Self {
            width: 230.0,
            ..Default::default()
        }
    }

    /// Replace the workspace roots, collapsing every previous expansion.
    pub fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = roots;
        self.expanded.clear();
        self.cache.clear();
        self.selected = None;
        for root in &self.roots {
            self.expanded.insert(root.clone());
        }
    }

    pub fn clear(&mut self) {
        self.set_roots(Vec::new());
    }

    pub fn entries(&mut self, path: &Path, show_hidden: bool) -> &[FileEntry] {
        if !self.cache.contains_key(path) {
            let entries = list_directory(path, show_hidden).unwrap_or_default();
            self.cache.insert(path.to_path_buf(), entries);
        }
        self.cache.get(path).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn toggle_directory(&mut self, path: &Path, show_hidden: bool) {
        if self.expanded.contains(path) {
            self.expanded.remove(path);
        } else {
            self.expanded.insert(path.to_path_buf());
            // Refresh the listing when re-opening, so new files show up.
            self.cache.remove(path);
            let _ = self.entries(path, show_hidden);
        }
    }

    pub fn collapse(&mut self, path: &Path) {
        self.expanded.remove(path);
    }

    pub fn expand(&mut self, path: &Path) {
        self.expanded.insert(path.to_path_buf());
    }

    pub fn refresh(&mut self) {
        self.cache.clear();
    }

    pub fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    pub fn select(&mut self, path: PathBuf) {
        self.selected = Some(path);
    }

    pub fn root_name_for(&self, root: &Path) -> String {
        root_name(root).unwrap_or_else(|| root.display().to_string())
    }

    /// Rename an expanded/selected path and every descendant.
    pub fn rename_path(&mut self, from: &Path, to: &Path) {
        let remap = |path: &Path| -> Option<PathBuf> {
            if path == from {
                Some(to.to_path_buf())
            } else {
                path.strip_prefix(from).ok().map(|suffix| to.join(suffix))
            }
        };
        self.expanded = self
            .expanded
            .drain()
            .filter_map(|path| remap(&path))
            .collect();
        self.selected = self.selected.as_deref().and_then(remap);
        self.cache.clear();
        for root in &mut self.roots {
            if let Some(path) = remap(root) {
                *root = path;
            }
        }
    }

    /// Drop selection and expansion entries below a deleted path.
    pub fn remove_path(&mut self, path: &Path) {
        self.expanded
            .retain(|candidate| !candidate.starts_with(path));
        if self
            .selected
            .as_deref()
            .is_some_and(|selected| selected.starts_with(path))
        {
            self.selected = None;
        }
        self.cache.clear();
    }

    /// Directory that receives a new entry: the selected directory, or the
    /// parent of the selected file.
    pub fn target_directory(&self) -> Option<PathBuf> {
        let selected = self.selected.clone()?;
        if selected.is_dir() {
            Some(selected)
        } else {
            selected.parent().map(Path::to_path_buf)
        }
    }

    /// Flatten the expanded tree. The workspace index supplies gitignore-aware
    /// children; without an index the direct listing is used.
    pub fn visible_rows(
        &mut self,
        index: Option<&WorkspaceIndex>,
        show_hidden: bool,
    ) -> Vec<VisibleEntry> {
        let roots = self.roots.clone();
        let mut rows = Vec::new();
        for root in roots {
            rows.push(VisibleEntry {
                path: root.clone(),
                name: self.root_name_for(&root),
                is_dir: true,
                depth: 0,
                root: root.clone(),
            });
            if self.expanded.contains(&root) {
                self.push_children(&root, &root, 1, index, show_hidden, &mut rows);
            }
        }
        rows
    }

    fn push_children(
        &mut self,
        root: &Path,
        dir: &Path,
        depth: usize,
        index: Option<&WorkspaceIndex>,
        show_hidden: bool,
        rows: &mut Vec<VisibleEntry>,
    ) {
        if depth > 32 {
            return;
        }
        let children: Vec<(PathBuf, String, bool)> = match index {
            Some(index) => index
                .children_of(dir)
                .map(|entry| (entry.path.clone(), entry.name.clone(), entry.is_dir))
                .collect(),
            None => self
                .entries(dir, show_hidden)
                .iter()
                .map(|entry| (entry.path.clone(), entry.name.clone(), entry.is_dir))
                .collect(),
        };
        for (path, name, is_dir) in children {
            rows.push(VisibleEntry {
                path: path.clone(),
                name,
                is_dir,
                depth,
                root: root.to_path_buf(),
            });
            if is_dir && self.expanded.contains(&path) {
                self.push_children(root, &path, depth + 1, index, show_hidden, rows);
            }
        }
    }
}

fn root_name(root: &Path) -> Option<String> {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .or_else(|| Some(root.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_expand_initially() {
        let mut explorer = ExplorerState::new();
        explorer.set_roots(vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert!(explorer.is_expanded(Path::new("/a")));
        assert!(explorer.is_expanded(Path::new("/b")));
        assert_eq!(explorer.roots.len(), 2);
        explorer.set_roots(vec![PathBuf::from("/c")]);
        assert_eq!(explorer.roots, vec![PathBuf::from("/c")]);
        assert!(!explorer.is_expanded(Path::new("/a")));
    }

    #[test]
    fn rename_remaps_selection_and_descendants() {
        let mut explorer = ExplorerState::new();
        explorer.set_roots(vec![PathBuf::from("/w")]);
        explorer.expand(Path::new("/w/src"));
        explorer.select(PathBuf::from("/w/src/main.rs"));
        explorer.rename_path(Path::new("/w/src"), Path::new("/w/code"));
        assert!(explorer.is_expanded(Path::new("/w/code")));
        assert_eq!(explorer.selected, Some(PathBuf::from("/w/code/main.rs")));
        assert!(!explorer.is_expanded(Path::new("/w/src")));
    }

    #[test]
    fn deleting_clears_related_state() {
        let mut explorer = ExplorerState::new();
        explorer.set_roots(vec![PathBuf::from("/w")]);
        explorer.expand(Path::new("/w/src"));
        explorer.select(PathBuf::from("/w/src/main.rs"));
        explorer.remove_path(Path::new("/w/src"));
        assert!(!explorer.is_expanded(Path::new("/w/src")));
        assert!(explorer.selected.is_none());
    }

    #[test]
    fn target_directory_follows_the_selection() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let file = src.join("main.rs");
        std::fs::write(&file, "x").unwrap();
        let mut explorer = ExplorerState::new();
        explorer.set_roots(vec![dir.path().to_path_buf()]);
        assert!(explorer.target_directory().is_none());
        explorer.select(src.clone());
        assert_eq!(explorer.target_directory(), Some(src.clone()));
        explorer.select(file);
        assert_eq!(explorer.target_directory(), Some(src));
    }
}

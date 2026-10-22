use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use luban_core::workspace::{list_directory, FileEntry};

#[derive(Debug, Default)]
pub struct ExplorerState {
    pub root: Option<PathBuf>,
    pub expanded: HashSet<PathBuf>,
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

    pub fn set_root(&mut self, root: Option<PathBuf>) {
        self.root = root;
        self.expanded.clear();
        self.cache.clear();
        if let Some(root) = &self.root {
            self.expanded.insert(root.clone());
        }
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

    pub fn refresh(&mut self) {
        self.cache.clear();
    }

    pub fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    pub fn root_name(&self) -> Option<String> {
        self.root.as_ref().and_then(|root| {
            root.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .or_else(|| Some(root.display().to_string()))
        })
    }
}

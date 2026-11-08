//! File picker state: navigation, filtering, sorting, selection and preview.

mod listing;
mod places;
mod preview;

pub use listing::{
    compare_entries, format_modified, format_size, search_match_range, search_matches, PickerRow,
    PickerRowKind, SortKey, SortSpec,
};
pub use places::{build_sections, PlaceIcon, PlaceSection};
pub use preview::{load_preview, PreviewContent, PreviewKind, PreviewState};

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{FocusHandle, ScrollHandle, UniformListScrollHandle};
use luban_core::export::ExportFormat;
use luban_core::settings::MAX_RECENT_DIRS;
use luban_core::workspace::{wildcard_match, FileEntryInfo};

use crate::dialogs::SaveThen;
use crate::i18n::{tr, tr_args};

static NEXT_LISTING_GENERATION: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_listing_generation() -> u64 {
    NEXT_LISTING_GENERATION.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileDialogMode {
    Open,
    Save,
    Folder,
}

#[derive(Clone, Debug)]
pub enum FileDialogPurpose {
    OpenFiles,
    OpenFolder,
    FindInFilesFolder,
    SaveAs {
        tab_id: u64,
        then: SaveThen,
    },
    Export {
        format: ExportFormat,
    },
    ImportTheme,
    /// Open a `.luban-workspace` file.
    OpenWorkspace,
    /// Write the current workspace definition to a chosen path.
    SaveWorkspace,
    SaveWorkspaceAs,
    /// Add a folder as a workspace root.
    AddFolder,
    /// Import a global settings JSON file.
    ImportSettings,
    /// Export the global settings JSON file.
    ExportSettings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileFilter {
    All,
    Text,
    Patterns {
        label: String,
        patterns: Vec<String>,
    },
}

/// Globs for the "text files" filter. Matching is case-insensitive because the
/// candidate name is lowercased before it reaches the matcher.
pub const TEXT_FILE_PATTERNS: &[&str] = &[
    "*.txt",
    "*.md",
    "*.markdown",
    "*.log",
    "*.json",
    "*.jsonc",
    "*.toml",
    "*.yaml",
    "*.yml",
    "*.ini",
    "*.cfg",
    "*.conf",
    "*.xml",
    "*.html",
    "*.htm",
    "*.css",
    "*.js",
    "*.jsx",
    "*.ts",
    "*.tsx",
    "*.rs",
    "*.py",
    "*.rb",
    "*.go",
    "*.java",
    "*.kt",
    "*.kts",
    "*.c",
    "*.h",
    "*.cc",
    "*.cpp",
    "*.cxx",
    "*.hpp",
    "*.hh",
    "*.cs",
    "*.php",
    "*.sh",
    "*.bash",
    "*.zsh",
    "*.fish",
    "*.bat",
    "*.cmd",
    "*.ps1",
    "*.sql",
    "*.csv",
    "*.tsv",
    "*.tex",
    "*.rst",
    "*.adoc",
    "*.diff",
    "*.patch",
    "*.properties",
    "*.gradle",
];

impl FileFilter {
    pub fn matches(&self, name: &str) -> bool {
        match self {
            FileFilter::All => true,
            FileFilter::Text => {
                let name = name.to_lowercase();
                TEXT_FILE_PATTERNS
                    .iter()
                    .any(|pattern| wildcard_match(pattern, &name))
            }
            FileFilter::Patterns { patterns, .. } => {
                if patterns.is_empty() {
                    return true;
                }
                let name = name.to_lowercase();
                patterns
                    .iter()
                    .any(|pattern| wildcard_match(pattern, &name))
            }
        }
    }

    /// Menu label. Preset patterns show their glob; custom filters show the
    /// typed patterns.
    pub fn label(&self) -> String {
        match self {
            FileFilter::All => tr("dialog.filter_all"),
            FileFilter::Text => tr("dialog.filter_text"),
            FileFilter::Patterns { label, patterns } => {
                if !label.trim().is_empty() {
                    label.clone()
                } else if patterns.is_empty() {
                    tr("dialog.filter_all")
                } else {
                    patterns.join("; ")
                }
            }
        }
    }

    /// True when this filter shows every file. A custom filter with no
    /// patterns behaves like "all files".
    pub fn is_all(&self) -> bool {
        match self {
            FileFilter::All => true,
            FileFilter::Text => false,
            FileFilter::Patterns { patterns, .. } => patterns.is_empty(),
        }
    }

    /// Extension appended by Save As when the typed name has none.
    pub fn suggested_extension(&self) -> Option<String> {
        match self {
            FileFilter::All => None,
            FileFilter::Text => Some("txt".to_string()),
            FileFilter::Patterns { patterns, .. } => {
                if patterns.len() != 1 {
                    return None;
                }
                let pattern = patterns[0].trim();
                let extension = pattern.strip_prefix("*.")?;
                (!extension.is_empty()
                    && !extension.contains('*')
                    && !extension.contains('?')
                    && !extension.contains('/')
                    && !extension.contains('\\'))
                .then(|| extension.to_string())
            }
        }
    }
}

pub fn parse_filter_patterns(input: &str) -> Vec<String> {
    input
        .split([';', ','])
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_lowercase)
        .collect()
}

pub fn filter_from_patterns(input: &str) -> FileFilter {
    FileFilter::Patterns {
        label: input.trim().to_string(),
        patterns: parse_filter_patterns(input),
    }
}

/// Append the active filter's extension to a filename without one.
pub fn default_extension(name: &str, filter: &FileFilter) -> String {
    let name = name.trim();
    if name.is_empty() || Path::new(name).extension().is_some() {
        return name.to_string();
    }
    match filter.suggested_extension() {
        Some(extension) => format!("{name}.{extension}"),
        None => name.to_string(),
    }
}

/// Default filter for a dialog purpose. Save As starts on text files; the
/// import/export/workspace flows start on their own file type.
pub fn default_filter(purpose: &FileDialogPurpose) -> FileFilter {
    let patterns = |label: &str, patterns: &[&str]| FileFilter::Patterns {
        label: label.to_string(),
        patterns: patterns
            .iter()
            .map(|pattern| pattern.to_lowercase())
            .collect(),
    };
    match purpose {
        FileDialogPurpose::SaveAs { .. } => FileFilter::Text,
        FileDialogPurpose::Export { format } => match format {
            ExportFormat::Html => patterns("*.html", &["*.html", "*.htm"]),
            ExportFormat::Rtf => patterns("*.rtf", &["*.rtf"]),
        },
        FileDialogPurpose::OpenWorkspace
        | FileDialogPurpose::SaveWorkspace
        | FileDialogPurpose::SaveWorkspaceAs => {
            patterns("*.luban-workspace", &["*.luban-workspace"])
        }
        FileDialogPurpose::ImportTheme => patterns("*.tmTheme", &["*.tmtheme"]),
        FileDialogPurpose::ImportSettings | FileDialogPurpose::ExportSettings => {
            patterns("*.json", &["*.json"])
        }
        _ => FileFilter::All,
    }
}

/// The filter choices offered by the dropdown for one purpose.
pub fn filter_presets(purpose: &FileDialogPurpose) -> Vec<FileFilter> {
    let mut presets = vec![FileFilter::All, FileFilter::Text];
    let default = default_filter(purpose);
    if !presets.contains(&default) {
        presets.push(default);
    }
    presets
}

pub struct NewFolderState {
    pub error: Option<String>,
}

pub struct FileDialogState {
    pub mode: FileDialogMode,
    pub purpose: FileDialogPurpose,
    pub dir: PathBuf,
    pub entries: Vec<FileEntryInfo>,
    /// Display snapshot, row 0 is the parent entry.
    pub rows: Arc<Vec<PickerRow>>,
    /// Selected files. Directories are never part of the selection.
    pub selected: BTreeSet<PathBuf>,
    /// Active row; row 0 is the parent entry.
    pub active: Option<usize>,
    /// Row from which Shift-selection extends.
    pub anchor: Option<usize>,
    pub filter: FileFilter,
    pub custom_filter: bool,
    pub search: String,
    pub sort: SortSpec,
    pub loading: bool,
    /// Incremented on every directory listing; stale results are discarded.
    pub generation: u64,
    pub error: Option<String>,
    pub history: Vec<PathBuf>,
    pub history_index: usize,
    pub show_hidden: bool,
    pub address_edit: bool,
    pub new_folder: Option<NewFolderState>,
    pub show_preview: bool,
    pub preview: PreviewState,
    pub places: Vec<PlaceSection>,
    pub recent_dirs: Vec<PathBuf>,
    pub list_focus: Option<FocusHandle>,
    pub list_scroll: UniformListScrollHandle,
    /// Shared base handle of `list_scroll`, used to draw the overlay bar.
    pub list_base_scroll: ScrollHandle,
    pub preview_scroll: ScrollHandle,
    type_ahead: String,
    type_ahead_at: Option<Instant>,
}

impl FileDialogState {
    pub fn new(
        mode: FileDialogMode,
        purpose: FileDialogPurpose,
        dir: PathBuf,
        entries: Vec<FileEntryInfo>,
    ) -> Self {
        let filter = default_filter(&purpose);
        let list_scroll = UniformListScrollHandle::new();
        let list_base_scroll = list_scroll.0.borrow().base_handle.clone();
        Self {
            mode,
            purpose,
            dir: dir.clone(),
            entries,
            rows: Arc::new(Vec::new()),
            selected: BTreeSet::new(),
            active: None,
            anchor: None,
            filter,
            custom_filter: false,
            search: String::new(),
            sort: SortSpec::default(),
            loading: false,
            generation: 0,
            error: None,
            history: vec![dir],
            history_index: 0,
            show_hidden: false,
            address_edit: false,
            new_folder: None,
            show_preview: true,
            preview: PreviewState::new(),
            places: Vec::new(),
            recent_dirs: Vec::new(),
            list_focus: None,
            list_scroll,
            list_base_scroll,
            preview_scroll: ScrollHandle::default(),
            type_ahead: String::new(),
            type_ahead_at: None,
        }
    }

    pub fn with_list_focus(mut self, focus: FocusHandle) -> Self {
        self.list_focus = Some(focus);
        self
    }

    pub fn title(&self) -> String {
        tr(match self.mode {
            FileDialogMode::Open => match &self.purpose {
                FileDialogPurpose::OpenWorkspace => "dialog.open_workspace",
                FileDialogPurpose::ImportSettings => "dialog.import_settings",
                _ => "dialog.open_file",
            },
            FileDialogMode::Save => match &self.purpose {
                FileDialogPurpose::Export { format } => match format {
                    ExportFormat::Html => "dialog.export_html",
                    ExportFormat::Rtf => "dialog.export_rtf",
                },
                FileDialogPurpose::SaveWorkspace | FileDialogPurpose::SaveWorkspaceAs => {
                    "dialog.save_workspace"
                }
                FileDialogPurpose::ExportSettings => "dialog.export_settings",
                _ => "dialog.save_as",
            },
            FileDialogMode::Folder => match self.purpose {
                FileDialogPurpose::OpenFolder => "dialog.open_folder",
                FileDialogPurpose::FindInFilesFolder => "dialog.choose_search_folder",
                FileDialogPurpose::AddFolder => "dialog.add_folder",
                _ => "dialog.choose_folder",
            },
        })
    }

    pub fn confirm_label(&self) -> String {
        tr(match self.mode {
            FileDialogMode::Open => "dialog.open",
            FileDialogMode::Save => "dialog.save",
            FileDialogMode::Folder => "dialog.choose_folder",
        })
    }

    #[cfg(test)]
    pub fn entry_is_visible(&self, entry: &FileEntryInfo) -> bool {
        if !self.search.trim().is_empty() {
            search_matches(&entry.name, &self.search)
                && (entry.is_dir || self.filter.matches(&entry.name))
        } else {
            entry.is_dir || self.filter.matches(&entry.name)
        }
    }

    /// Rebuild the display snapshot after listing, filter, search or sort
    /// changes. This is the only place rows are allocated.
    pub fn recompute_rows(&mut self) {
        let filter = self.filter.clone();
        let search = self.search.clone();
        let sort = self.sort;
        let search_active = !search.trim().is_empty();
        let mut indices: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                if search_active {
                    search_matches(&entry.name, &search)
                        && (entry.is_dir || filter.matches(&entry.name))
                } else {
                    entry.is_dir || filter.matches(&entry.name)
                }
            })
            .map(|(index, _)| index)
            .collect();
        indices.sort_by(|&a, &b| compare_entries(&self.entries[a], &self.entries[b], sort));
        let mut rows = Vec::with_capacity(indices.len() + 1);
        rows.push(PickerRow::parent(&self.dir, ".."));
        for index in indices {
            let entry = &self.entries[index];
            let range = if search.trim().is_empty() {
                None
            } else {
                search_match_range(&entry.name, &search)
            };
            rows.push(PickerRow::from_entry(entry, range));
        }
        self.rows = Arc::new(rows);
        self.clamp_active();
        self.prune_selection();
    }

    fn clamp_active(&mut self) {
        let last = self.rows.len().saturating_sub(1);
        if let Some(active) = self.active {
            self.active = Some(active.min(last));
        }
        if let Some(anchor) = self.anchor {
            self.anchor = Some(anchor.min(last));
        }
    }

    pub fn visible_row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn row(&self, index: usize) -> Option<&PickerRow> {
        self.rows.get(index)
    }

    pub fn active_path(&self) -> Option<PathBuf> {
        self.active
            .and_then(|row| self.rows.get(row))
            .map(|row| row.path.clone())
    }

    pub fn row_for_path(&self, path: &Path) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| crate::util::same_path(&row.path, path))
    }

    pub fn focus_path(&mut self, path: &Path) {
        if let Some(row) = self.row_for_path(path) {
            self.active = Some(row);
            self.anchor = Some(row);
        }
    }

    /// Selected files in display order.
    pub fn selected_in_visible_order(&self) -> Vec<PathBuf> {
        self.rows
            .iter()
            .filter(|row| row.is_file() && self.selected.contains(&row.path))
            .map(|row| row.path.clone())
            .collect()
    }

    #[cfg(test)]
    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    pub fn selected_total_size(&self) -> u64 {
        self.rows
            .iter()
            .filter(|row| row.is_file() && self.selected.contains(&row.path))
            .filter_map(|row| row.size)
            .sum()
    }

    pub fn first_visible_file(&self) -> Option<&PickerRow> {
        self.rows.iter().find(|row| row.is_file())
    }

    /// First entry after the parent row, which may be a file or folder.
    pub fn first_visible_entry(&self) -> Option<&PickerRow> {
        self.rows.get(1)
    }

    pub fn set_filter(&mut self, filter: FileFilter) {
        if self.filter == filter && !self.custom_filter {
            return;
        }
        self.filter = filter;
        self.custom_filter = false;
        self.active = None;
        self.anchor = None;
        self.recompute_rows();
    }

    pub fn set_custom_filter(&mut self, input: &str) {
        self.filter = filter_from_patterns(input);
        self.custom_filter = true;
        self.active = None;
        self.anchor = None;
        self.recompute_rows();
    }

    pub fn set_search(&mut self, query: String) {
        if self.search == query {
            return;
        }
        self.search = query;
        self.active = None;
        self.anchor = None;
        self.recompute_rows();
        if !self.search.trim().is_empty() {
            self.active = self.rows.iter().position(|row| row.is_file());
            self.anchor = self.active;
            if let Some(row) = self.active {
                let path = self.rows[row].path.clone();
                self.select_only(&path);
            }
        }
    }

    pub fn set_sort(&mut self, key: SortKey) {
        let active_path = self.active_path();
        if self.sort.key == key {
            self.sort.descending = !self.sort.descending;
        } else {
            self.sort = SortSpec {
                key,
                descending: false,
            };
        }
        self.recompute_rows();
        if let Some(path) = active_path {
            self.focus_path(&path);
        }
    }

    pub fn prune_selection(&mut self) {
        let visible: BTreeSet<PathBuf> = self
            .rows
            .iter()
            .filter(|row| row.is_file())
            .map(|row| row.path.clone())
            .collect();
        self.selected.retain(|path| visible.contains(path));
    }

    pub fn select_only(&mut self, path: &Path) {
        self.selected.clear();
        self.selected.insert(path.to_path_buf());
    }

    pub fn toggle_selection(&mut self, path: &Path) {
        if !self.selected.remove(path) {
            self.selected.insert(path.to_path_buf());
        }
    }

    pub fn select_all_visible(&mut self) {
        for row in self.rows.iter().filter(|row| row.is_file()) {
            self.selected.insert(row.path.clone());
        }
    }

    pub fn set_active(&mut self, row: usize) {
        if self.rows.is_empty() {
            self.active = None;
            return;
        }
        self.active = Some(row.min(self.rows.len() - 1));
    }

    /// Move to a row and make it the only selection. Directories clear the
    /// selection because only files can be opened.
    pub fn select_row(&mut self, row: usize) {
        if self.rows.is_empty() {
            self.active = None;
            self.anchor = None;
            return;
        }
        let row = row.min(self.rows.len() - 1);
        self.active = Some(row);
        self.anchor = Some(row);
        match self.rows.get(row) {
            Some(entry) if entry.is_file() => {
                let path = entry.path.clone();
                self.select_only(&path);
            }
            _ => self.selected.clear(),
        }
    }

    /// Add every file between the anchor row and `row` to the selection.
    pub fn extend_selection_to(&mut self, row: usize) {
        if self.rows.is_empty() {
            return;
        }
        let row = row.min(self.rows.len() - 1);
        let start = self.anchor.or(self.active).unwrap_or(row);
        let (low, high) = if start <= row {
            (start, row)
        } else {
            (row, start)
        };
        if self.anchor.is_none() {
            self.anchor = Some(start);
        }
        for index in low..=high {
            if let Some(entry) = self.rows.get(index).filter(|entry| entry.is_file()) {
                self.selected.insert(entry.path.clone());
            }
        }
        self.active = Some(row);
    }

    pub fn toggle_active(&mut self) {
        if self.active.is_none() {
            self.set_active(1.min(self.rows.len().saturating_sub(1)));
        }
        let Some(row) = self.active else {
            return;
        };
        if let Some(entry) = self.rows.get(row) {
            if entry.is_file() {
                let path = entry.path.clone();
                self.toggle_selection(&path);
                self.anchor = Some(row);
            }
        }
    }

    pub fn type_ahead_char(&mut self, ch: char) -> String {
        let now = Instant::now();
        if self
            .type_ahead_at
            .is_none_or(|at| now.duration_since(at) > Duration::from_millis(800))
        {
            self.type_ahead.clear();
        }
        self.type_ahead.push(ch);
        self.type_ahead_at = Some(now);
        self.type_ahead.clone()
    }

    pub fn type_ahead_backspace(&mut self) -> String {
        self.type_ahead.pop();
        self.type_ahead_at = Some(Instant::now());
        self.type_ahead.clone()
    }

    /// Row whose name starts with the type-ahead buffer, starting at the
    /// current row and wrapping once.
    pub fn type_ahead_row(&self, buffer: &str) -> Option<usize> {
        let needle = buffer.to_lowercase();
        if needle.is_empty() || self.rows.len() <= 1 {
            return None;
        }
        let total = self.rows.len() - 1;
        let start = self.active.unwrap_or(0).saturating_sub(1);
        (0..total)
            .map(|step| 1 + (start + step) % total)
            .find(|&row| {
                self.rows
                    .get(row)
                    .is_some_and(|entry| entry.name.to_lowercase().starts_with(&needle))
            })
    }

    pub fn accepts_listing(&self, generation: u64, dir: &Path) -> bool {
        self.generation == generation && self.dir == dir
    }

    pub fn with_default_extension(&self, name: &str) -> String {
        default_extension(name, &self.filter)
    }

    pub fn parent_dir(&self) -> Option<PathBuf> {
        self.dir.parent().map(Path::to_path_buf)
    }

    pub fn can_back(&self) -> bool {
        self.history_index > 0
    }

    pub fn can_forward(&self) -> bool {
        self.history_index + 1 < self.history.len()
    }

    /// Record the current folder as a new history entry, discarding forward
    /// entries. Consecutive duplicates are ignored.
    pub fn push_history(&mut self) {
        if self.history.get(self.history_index) == Some(&self.dir) {
            return;
        }
        self.history.truncate(self.history_index + 1);
        self.history.push(self.dir.clone());
        if self.history.len() > 50 {
            let excess = self.history.len() - 50;
            self.history.drain(..excess);
        }
        self.history_index = self.history.len().saturating_sub(1);
    }

    pub fn go_back(&mut self) -> Option<PathBuf> {
        if !self.can_back() {
            return None;
        }
        self.history_index -= 1;
        Some(self.history[self.history_index].clone())
    }

    pub fn go_forward(&mut self) -> Option<PathBuf> {
        if !self.can_forward() {
            return None;
        }
        self.history_index += 1;
        Some(self.history[self.history_index].clone())
    }

    pub fn remember_recent(&mut self, dir: &Path) {
        self.recent_dirs
            .retain(|path| !crate::util::same_path(path, dir));
        self.recent_dirs.insert(0, dir.to_path_buf());
        self.recent_dirs.truncate(MAX_RECENT_DIRS);
    }
}

/// Breadcrumb segments for the address bar. The final segment is the current
/// folder.
pub fn breadcrumbs(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut segments: Vec<(String, PathBuf)> = Vec::new();
    let mut path = PathBuf::new();
    for component in dir.components() {
        path.push(component.as_os_str());
        match component {
            Component::Prefix(prefix) => {
                segments.push((
                    prefix.as_os_str().to_string_lossy().into_owned(),
                    path.clone(),
                ));
            }
            Component::RootDir => {
                if let Some(last) = segments.last_mut() {
                    last.0 = format!("{}\\", last.0);
                    last.1 = path.clone();
                } else {
                    segments.push(("/".to_string(), path.clone()));
                }
            }
            Component::Normal(name) => {
                segments.push((name.to_string_lossy().into_owned(), path.clone()));
            }
            Component::CurDir | Component::ParentDir => {}
        }
    }
    segments
}

/// Error text for a failed navigation, with the path included.
pub fn not_a_folder_message(path: &Path) -> String {
    tr_args(
        "dialog.not_a_folder",
        &[("path", path.display().to_string())],
    )
}

/// Error text for an entry that disappeared between listing and confirming.
pub fn file_not_found_message(path: &Path) -> String {
    tr_args(
        "dialog.file_not_found",
        &[("path", path.display().to_string())],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> FileEntryInfo {
        FileEntryInfo {
            path: PathBuf::from("/tmp").join(name),
            name: name.to_string(),
            is_dir,
            is_symlink: false,
            size: (!is_dir).then_some(name.len() as u64),
            modified: None,
        }
    }

    fn state(entries: Vec<FileEntryInfo>) -> FileDialogState {
        FileDialogState::new(
            FileDialogMode::Open,
            FileDialogPurpose::OpenFiles,
            PathBuf::from("/tmp"),
            entries,
        )
    }

    #[test]
    fn labels_follow_the_locale() {
        let _guard = crate::i18n::test_lock();
        let open = FileDialogState::new(
            FileDialogMode::Open,
            FileDialogPurpose::OpenFiles,
            PathBuf::from("/tmp"),
            Vec::new(),
        );
        let save = FileDialogState::new(
            FileDialogMode::Save,
            FileDialogPurpose::SaveAs {
                tab_id: 1,
                then: SaveThen::Nothing,
            },
            PathBuf::from("/tmp"),
            Vec::new(),
        );
        let folder = FileDialogState::new(
            FileDialogMode::Folder,
            FileDialogPurpose::FindInFilesFolder,
            PathBuf::from("/tmp"),
            Vec::new(),
        );

        rust_i18n::set_locale("en");
        assert_eq!(open.title(), "Open File");
        assert_eq!(open.confirm_label(), "Open");
        assert_eq!(save.title(), "Save As");
        assert_eq!(save.confirm_label(), "Save");
        assert_eq!(folder.title(), "Choose Search Folder");
        assert_eq!(folder.confirm_label(), "Choose Folder");

        rust_i18n::set_locale("zh-CN");
        assert_eq!(open.title(), "打开文件");
        assert_eq!(open.confirm_label(), "打开");
        assert_eq!(save.title(), "另存为");
        assert_eq!(save.confirm_label(), "保存");
        assert_eq!(folder.title(), "选择搜索文件夹");
        assert_eq!(folder.confirm_label(), "选择文件夹");
        rust_i18n::set_locale("en");
    }

    #[test]
    fn filters_keep_directories_and_apply_extensions() {
        let mut dialog = state(vec![
            entry("src", true),
            entry("notes.md", false),
            entry("image.png", false),
        ]);
        dialog.set_filter(FileFilter::Text);
        dialog.recompute_rows();
        assert_eq!(dialog.visible_row_count(), 3);
        assert!(dialog.entry_is_visible(&entry("src", true)));
        assert!(!dialog.entry_is_visible(&entry("image.png", false)));

        dialog.set_custom_filter("*.png");
        assert!(dialog.entry_is_visible(&entry("image.png", false)));
        assert!(!dialog.entry_is_visible(&entry("notes.md", false)));
        assert_eq!(default_extension("notes", &FileFilter::Text), "notes.txt");
        let json = filter_from_patterns("*.json");
        assert_eq!(default_extension("settings", &json), "settings.json");
    }

    #[test]
    fn search_selects_first_file_and_prunes_hidden_selection() {
        let mut dialog = state(vec![
            entry("a.rs", false),
            entry("main.rs", false),
            entry("b.txt", false),
        ]);
        dialog.select_only(Path::new("/tmp/a.rs"));
        dialog.set_search("main".to_string());
        assert!(!dialog.selected.contains(Path::new("/tmp/a.rs")));
        assert_eq!(dialog.selected_count(), 1);
        assert!(dialog.selected.contains(Path::new("/tmp/main.rs")));
        assert_eq!(
            dialog.active,
            dialog.row_for_path(Path::new("/tmp/main.rs"))
        );
    }

    #[test]
    fn sorting_toggles_direction_and_keeps_the_active_row() {
        let mut dialog = state(vec![entry("b.txt", false), entry("a.txt", false)]);
        dialog.recompute_rows();
        dialog.focus_path(Path::new("/tmp/a.txt"));
        dialog.set_sort(SortKey::Name);
        assert!(dialog.sort.descending);
        assert_eq!(dialog.active_path(), Some(PathBuf::from("/tmp/a.txt")));
    }

    #[test]
    fn history_supports_back_forward_and_deduplicates() {
        let mut dialog = state(Vec::new());
        dialog.dir = PathBuf::from("/tmp/a");
        dialog.push_history();
        dialog.dir = PathBuf::from("/tmp/b");
        dialog.push_history();
        assert!(dialog.can_back());
        assert_eq!(dialog.go_back(), Some(PathBuf::from("/tmp/a")));
        assert_eq!(dialog.go_back(), Some(PathBuf::from("/tmp")));
        assert!(!dialog.can_back());
        assert_eq!(dialog.go_forward(), Some(PathBuf::from("/tmp/a")));
        assert_eq!(dialog.go_forward(), Some(PathBuf::from("/tmp/b")));
        assert!(!dialog.can_forward());
        dialog.dir = PathBuf::from("/tmp/b");
        dialog.push_history();
        assert_eq!(dialog.history.len(), 3);
    }

    #[test]
    fn type_ahead_finds_the_next_matching_row() {
        let mut dialog = state(vec![
            entry("alpha.txt", false),
            entry("beta.txt", false),
            entry("bravo.txt", false),
        ]);
        dialog.recompute_rows();
        dialog.active = Some(0);
        let row = dialog.type_ahead_row("b").unwrap();
        assert_eq!(dialog.rows[row].name, "beta.txt");
        dialog.active = Some(row);
        let row = dialog.type_ahead_row("br").unwrap();
        assert_eq!(dialog.rows[row].name, "bravo.txt");
    }

    #[test]
    fn purpose_defaults_cover_workspaces_settings_and_exports() {
        assert_eq!(
            default_filter(&FileDialogPurpose::SaveAs {
                tab_id: 1,
                then: SaveThen::Nothing
            }),
            FileFilter::Text
        );
        let workspace = default_filter(&FileDialogPurpose::OpenWorkspace);
        assert!(workspace.matches("notes.LUBAN-WORKSPACE"));
        let settings = default_filter(&FileDialogPurpose::ExportSettings);
        assert!(settings.matches("settings.json"));
        assert!(!settings.matches("settings.toml"));
        let html = default_filter(&FileDialogPurpose::Export {
            format: ExportFormat::Html,
        });
        assert!(html.matches("page.html"));
    }

    #[test]
    fn breadcrumbs_split_absolute_paths() {
        let segments = breadcrumbs(Path::new("/home/user/docs"));
        let labels: Vec<_> = segments.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, vec!["/", "home", "user", "docs"]);
        assert_eq!(segments.last().unwrap().1, PathBuf::from("/home/user/docs"));
    }
}

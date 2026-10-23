use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::component::input::InputState;
use gpui_kit::{AppContext as _, Context, Entity, FocusHandle, ScrollHandle, Subscription, Window};
use luban_core::workspace::{wildcard_match, FileEntry};

use crate::app::LubanApp;
use crate::i18n::tr;

/// Listing generations are process-wide so a stale task from a closed dialog
/// can never match a newly opened dialog that happens to list the same
/// directory. Dialog-local counters would both start at 1 and collide.
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
    SaveAs { tab_id: u64, then: SaveThen },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileFilter {
    #[default]
    All,
    Text,
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
    pub fn matches(self, name: &str) -> bool {
        match self {
            FileFilter::All => true,
            FileFilter::Text => {
                let name = name.to_lowercase();
                TEXT_FILE_PATTERNS
                    .iter()
                    .any(|pattern| wildcard_match(pattern, &name))
            }
        }
    }
}

/// Append `.txt` to a filename without an extension when the text filter is
/// active. Names that already carry an extension are left untouched.
pub fn default_extension(name: &str, filter: FileFilter) -> String {
    if filter == FileFilter::Text && !name.is_empty() && Path::new(name).extension().is_none() {
        format!("{name}.txt")
    } else {
        name.to_string()
    }
}

pub struct FileDialogState {
    pub mode: FileDialogMode,
    pub purpose: FileDialogPurpose,
    pub dir: PathBuf,
    pub entries: Vec<FileEntry>,
    /// Selected files. Directories are never part of the selection.
    pub selected: BTreeSet<PathBuf>,
    /// Active row in the rendered list; row 0 is the `..` parent entry.
    pub active: Option<usize>,
    /// Row from which Shift-selection extends.
    pub anchor: Option<usize>,
    pub filter: FileFilter,
    pub loading: bool,
    /// Incremented on every directory listing; stale results are discarded.
    pub generation: u64,
    pub error: Option<String>,
    pub list_focus: Option<FocusHandle>,
    pub list_scroll: ScrollHandle,
}

impl FileDialogState {
    pub fn new(
        mode: FileDialogMode,
        purpose: FileDialogPurpose,
        dir: PathBuf,
        entries: Vec<FileEntry>,
    ) -> Self {
        Self {
            mode,
            purpose,
            dir,
            entries,
            selected: BTreeSet::new(),
            active: None,
            anchor: None,
            filter: FileFilter::All,
            loading: false,
            generation: 0,
            error: None,
            list_focus: None,
            list_scroll: ScrollHandle::default(),
        }
    }

    pub fn with_list_focus(mut self, focus: FocusHandle) -> Self {
        self.list_focus = Some(focus);
        self
    }

    pub fn title(&self) -> String {
        tr(match self.mode {
            FileDialogMode::Open => "dialog.open_file",
            FileDialogMode::Save => "dialog.save_as",
            FileDialogMode::Folder => match self.purpose {
                FileDialogPurpose::OpenFolder => "dialog.open_folder",
                FileDialogPurpose::FindInFilesFolder => "dialog.choose_search_folder",
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

    pub fn entry_is_visible(&self, entry: &FileEntry) -> bool {
        entry.is_dir || self.filter.matches(&entry.name)
    }

    /// Rows include the parent `..` row at index 0.
    pub fn visible_row_count(&self) -> usize {
        1 + self
            .entries
            .iter()
            .filter(|entry| self.entry_is_visible(entry))
            .count()
    }

    /// Entry shown at a rendered row; row 0 is the parent row.
    pub fn entry_for_row(&self, row: usize) -> Option<&FileEntry> {
        if row == 0 {
            return None;
        }
        self.entries
            .iter()
            .filter(|entry| self.entry_is_visible(entry))
            .nth(row - 1)
    }

    pub fn visible_files(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .filter(|entry| !entry.is_dir && self.filter.matches(&entry.name))
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// Selected files in the order they appear in the list.
    pub fn selected_in_visible_order(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .filter(|entry| {
                !entry.is_dir
                    && self.filter.matches(&entry.name)
                    && self.selected.contains(&entry.path)
            })
            .map(|entry| entry.path.clone())
            .collect()
    }

    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    pub fn set_filter(&mut self, filter: FileFilter) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        self.prune_selection();
        self.active = None;
        self.anchor = None;
    }

    pub fn prune_selection(&mut self) {
        let visible: BTreeSet<PathBuf> = self
            .entries
            .iter()
            .filter(|entry| !entry.is_dir && self.filter.matches(&entry.name))
            .map(|entry| entry.path.clone())
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
        for path in self.visible_files() {
            self.selected.insert(path);
        }
    }

    pub fn set_active(&mut self, row: usize) {
        self.active = Some(row.min(self.visible_row_count().saturating_sub(1)));
    }

    /// Move to a row and make it the only selection (directories clear the
    /// selection because only files can be opened).
    pub fn select_row(&mut self, row: usize) {
        let row = row.min(self.visible_row_count().saturating_sub(1));
        self.active = Some(row);
        self.anchor = Some(row);
        match self.entry_for_row(row) {
            Some(entry) if !entry.is_dir => {
                let path = entry.path.clone();
                self.select_only(&path);
            }
            _ => self.selected.clear(),
        }
    }

    /// Add every file between the anchor row and `row` to the selection.
    pub fn extend_selection_to(&mut self, row: usize) {
        let row = row.min(self.visible_row_count().saturating_sub(1));
        let start = self.anchor.or(self.active).unwrap_or(row);
        let (low, high) = if start <= row {
            (start, row)
        } else {
            (row, start)
        };
        let paths: Vec<PathBuf> = (low..=high)
            .filter_map(|row| self.entry_for_row(row))
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.path.clone())
            .collect();
        if self.anchor.is_none() {
            self.anchor = Some(start);
        }
        for path in paths {
            self.selected.insert(path);
        }
        self.active = Some(row);
    }

    pub fn toggle_active(&mut self) {
        if self.active.is_none() {
            self.set_active(0);
        }
        let Some(row) = self.active else {
            return;
        };
        if let Some(entry) = self.entry_for_row(row) {
            if !entry.is_dir {
                let path = entry.path.clone();
                self.toggle_selection(&path);
                self.anchor = Some(row);
            }
        }
    }

    pub fn accepts_listing(&self, generation: u64, dir: &Path) -> bool {
        self.generation == generation && self.dir == dir
    }

    pub fn with_default_extension(&self, name: &str) -> String {
        default_extension(name, self.filter)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseIntent {
    Tab(u64),
    AllTabs,
    Exit,
}

pub struct ConfirmState {
    pub intent: CloseIntent,
    pub title: String,
    pub detail: String,
}

impl ConfirmState {
    pub fn new(intent: CloseIntent, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            intent,
            title: title.into(),
            detail: detail.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveThen {
    Nothing,
    CloseTab(u64),
    Exit,
    CloseAll,
}

pub struct SaveAsRequest {
    pub tab_id: u64,
    pub path: PathBuf,
    pub then: SaveThen,
}

pub struct GoToLineState {
    pub input: Entity<InputState>,
    pub subscription: Option<Subscription>,
}

impl GoToLineState {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("dialog.line_placeholder")));
        Self {
            input,
            subscription: None,
        }
    }
}

pub struct LanguagePickerState {
    pub filter: Entity<InputState>,
}

impl LanguagePickerState {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>) -> Self {
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("dialog.filter_languages")));
        Self { filter }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> FileEntry {
        FileEntry {
            path: PathBuf::from("/tmp").join(name),
            name: name.to_string(),
            is_dir,
        }
    }

    fn state(entries: Vec<FileEntry>) -> FileDialogState {
        FileDialogState::new(
            FileDialogMode::Open,
            FileDialogPurpose::OpenFiles,
            PathBuf::from("/tmp"),
            entries,
        )
    }

    #[test]
    fn file_dialog_labels_follow_the_locale() {
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
    fn text_filter_matches_case_insensitively_and_keeps_directories() {
        let mut dialog = state(vec![
            entry("src", true),
            entry("notes.md", false),
            entry("NOTES.MD", false),
            entry("image.png", false),
            entry("archive.zip", false),
        ]);
        assert_eq!(dialog.visible_row_count(), 6);

        dialog.set_filter(FileFilter::Text);

        assert_eq!(dialog.visible_row_count(), 4);
        assert!(dialog.entry_is_visible(&entry("src", true)));
        assert!(dialog.entry_is_visible(&entry("notes.md", false)));
        assert!(dialog.entry_is_visible(&entry("NOTES.MD", false)));
        assert!(!dialog.entry_is_visible(&entry("image.png", false)));
        assert_eq!(
            dialog.visible_files(),
            vec![
                PathBuf::from("/tmp/notes.md"),
                PathBuf::from("/tmp/NOTES.MD")
            ]
        );
    }

    #[test]
    fn switching_filters_removes_hidden_selections() {
        let mut dialog = state(vec![entry("notes.md", false), entry("image.png", false)]);
        dialog.select_all_visible();
        assert_eq!(dialog.selected_count(), 2);

        dialog.set_filter(FileFilter::Text);

        assert_eq!(dialog.selected_count(), 1);
        assert!(dialog.selected.contains(&PathBuf::from("/tmp/notes.md")));
        assert_eq!(dialog.active, None);
        assert_eq!(dialog.anchor, None);
    }

    #[test]
    fn text_filter_appends_only_missing_extensions() {
        assert_eq!(default_extension("notes", FileFilter::Text), "notes.txt");
        assert_eq!(default_extension("notes.md", FileFilter::Text), "notes.md");
        assert_eq!(default_extension("notes", FileFilter::All), "notes");
        assert_eq!(default_extension("", FileFilter::Text), "");
    }

    #[test]
    fn selection_supports_single_toggle_all_and_range() {
        let mut dialog = state(vec![
            entry("a.rs", false),
            entry("b.rs", false),
            entry("c.rs", false),
        ]);

        dialog.select_only(Path::new("/tmp/a.rs"));
        assert_eq!(dialog.selected_count(), 1);

        dialog.toggle_selection(Path::new("/tmp/b.rs"));
        assert_eq!(dialog.selected_count(), 2);
        dialog.toggle_selection(Path::new("/tmp/b.rs"));
        assert_eq!(dialog.selected_count(), 1);

        dialog.select_all_visible();
        assert_eq!(dialog.selected_count(), 3);

        dialog.selected.clear();
        dialog.set_active(1);
        dialog.anchor = Some(1);
        dialog.extend_selection_to(3);
        assert_eq!(
            dialog.selected_in_visible_order(),
            vec![
                PathBuf::from("/tmp/a.rs"),
                PathBuf::from("/tmp/b.rs"),
                PathBuf::from("/tmp/c.rs")
            ]
        );
        assert_eq!(dialog.active, Some(3));
    }

    #[test]
    fn keyboard_rows_clamp_and_directories_clear_selection() {
        let mut dialog = state(vec![entry("dir", true), entry("file.txt", false)]);

        dialog.set_active(99);
        assert_eq!(dialog.active, Some(2));

        dialog.select_row(1);
        assert_eq!(dialog.active, Some(1));
        assert_eq!(dialog.selected_count(), 0);

        dialog.select_row(2);
        assert_eq!(dialog.selected_count(), 1);
        dialog.select_row(0);
        assert_eq!(dialog.active, Some(0));
        assert_eq!(dialog.selected_count(), 0);
    }

    #[test]
    fn stale_listing_results_are_rejected() {
        let mut dialog = state(Vec::new());
        dialog.generation = 2;

        assert!(dialog.accepts_listing(2, Path::new("/tmp")));
        assert!(!dialog.accepts_listing(1, Path::new("/tmp")));
        assert!(!dialog.accepts_listing(2, Path::new("/var")));
    }

    #[test]
    fn listing_generations_do_not_collide_across_dialogs() {
        let first = next_listing_generation();
        let second = next_listing_generation();
        assert!(second > first);

        let mut reopened = state(Vec::new());
        reopened.generation = second;
        // The same directory is not enough: a result from the previous dialog
        // must be rejected even after closing and reopening it.
        assert!(!reopened.accepts_listing(first, Path::new("/tmp")));
    }
}

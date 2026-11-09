use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gpui_kit::component::input::InputState;
use gpui_kit::{AppContext as _, Context, Entity, Window};
use luban_core::search::{Match, SearchEngine, SearchOptions};
use luban_core::settings::FindWindowPlacement;
use luban_core::workspace::FileMatch;

use crate::app::LubanApp;
use crate::i18n::{tr, tr_args};

pub const MAX_HIGHLIGHTS: usize = 20_000;
/// Upper bound for the search-results list inside the floating window.
pub const MAX_ALL_RESULTS: usize = 5_000;

/// Pages of the floating find/replace window, mirroring Notepad--.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FindTab {
    #[default]
    Find,
    Replace,
    FindInFiles,
    Mark,
}

impl FindTab {
    pub const ALL: [FindTab; 4] = [
        FindTab::Find,
        FindTab::Replace,
        FindTab::FindInFiles,
        FindTab::Mark,
    ];

    pub fn label_key(self) -> &'static str {
        match self {
            FindTab::Find => "find.tab.find",
            FindTab::Replace => "find.tab.replace",
            FindTab::FindInFiles => "find.tab.in_files",
            FindTab::Mark => "find.tab.mark",
        }
    }

    pub fn index(self) -> usize {
        match self {
            FindTab::Find => 0,
            FindTab::Replace => 1,
            FindTab::FindInFiles => 2,
            FindTab::Mark => 3,
        }
    }
}

/// Search mode radio group: Notepad--'s Normal / Regular expression / Extend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchMode {
    #[default]
    Normal,
    Regex,
    Extend,
}

impl SearchMode {
    pub fn label_key(self) -> &'static str {
        match self {
            SearchMode::Normal => "find.mode.normal",
            SearchMode::Regex => "find.mode.regex",
            SearchMode::Extend => "find.mode.extend",
        }
    }

    pub fn index(self) -> usize {
        match self {
            SearchMode::Normal => 0,
            SearchMode::Regex => 1,
            SearchMode::Extend => 2,
        }
    }
}

/// One row in the results list of the floating window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindResultItem {
    pub matched: Match,
    pub line: usize,
    pub column: usize,
    pub preview: String,
}

/// Persistent mark-all state for one document. Matches are recomputed from the
/// query when the document changes so marks follow edits.
#[derive(Clone, Debug)]
pub struct MarkHighlight {
    pub query: String,
    pub options: SearchOptions,
    pub matches: Vec<Match>,
    /// Document revision the matches were computed for.
    pub revision: u64,
}

pub struct FindPanel {
    pub open: bool,
    pub tab: FindTab,
    pub query: Entity<InputState>,
    pub replacement: Entity<InputState>,
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
    /// Notepad-- "Extend" mode: `\n`, `\r`, `\t`, `\0` and `\xHH` are expanded
    /// in the query before searching.
    pub extended: bool,
    pub enhanced_regex: bool,
    pub in_selection: bool,
    pub dot_matches_newline: bool,
    pub backward: bool,
    pub wrap_around: bool,
    pub matches: Vec<Match>,
    pub current: Option<usize>,
    pub error: Option<String>,
    pub dirty: bool,
    /// Keep the current status line after a live refresh. Set by replace,
    /// count, find-all and clipboard actions so their result stays visible
    /// until the query or options change.
    pub status_sticky: bool,
    pub scope: Option<(usize, usize)>,
    pub highlighted_revision: u64,
    /// Bumped whenever the match set or current match changes, so decorations
    /// can skip rebuilding on unrelated renders.
    pub generation: u64,
    pub status: String,
    /// Rows produced by an explicit "Find All in Current Document".
    pub all_results: Vec<FindResultItem>,
    pub all_results_limited: bool,
    /// Floating window geometry in window-relative points.
    pub window_x: f32,
    pub window_y: f32,
    pub window_width: f32,
    pub window_height: f32,
    pub window_placed: bool,
    /// Pointer offset from the window origin while the title bar is dragged.
    pub drag_offset: Option<(f32, f32)>,
    /// Pointer origin and size captured when resize starts.
    pub resize_origin: Option<(f32, f32, f32, f32)>,
    /// Mark tab: independent query and options, like Notepad--.
    pub mark_query: Entity<InputState>,
    pub mark_match_case: bool,
    pub mark_whole_word: bool,
    pub mark_regex: bool,
    pub mark_extended: bool,
    pub mark_enhanced_regex: bool,
    pub mark_status: String,
    /// Global matches collected by an explicit Find All on a disk-engine tab.
    pub huge_results: Vec<luban_core::disk_engine::SearchHit>,
    pub huge_limit: usize,
    pub huge_truncated: bool,
}

impl FindPanel {
    pub fn new(
        placement: &FindWindowPlacement,
        window: &mut Window,
        cx: &mut Context<LubanApp>,
    ) -> Self {
        let query =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("find.query_placeholder")));
        let replacement = cx
            .new(|cx| InputState::new(window, cx).placeholder(tr("find.replacement_placeholder")));
        let mark_query =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("find.mark_placeholder")));
        Self {
            open: false,
            tab: FindTab::Find,
            query,
            replacement,
            match_case: false,
            whole_word: false,
            regex: false,
            extended: false,
            enhanced_regex: false,
            in_selection: false,
            dot_matches_newline: false,
            backward: false,
            wrap_around: true,
            matches: Vec::new(),
            current: None,
            error: None,
            dirty: true,
            status_sticky: false,
            scope: None,
            highlighted_revision: u64::MAX,
            generation: 0,
            status: String::new(),
            all_results: Vec::new(),
            all_results_limited: false,
            window_x: placement.x.unwrap_or(0.0),
            window_y: placement.y.unwrap_or(0.0),
            window_width: placement.width,
            window_height: placement.height,
            window_placed: placement.x.is_some() && placement.y.is_some(),
            drag_offset: None,
            resize_origin: None,
            mark_query,
            mark_match_case: false,
            mark_whole_word: false,
            mark_regex: false,
            mark_extended: false,
            mark_enhanced_regex: false,
            mark_status: String::new(),
            huge_results: Vec::new(),
            huge_limit: 5000,
            huge_truncated: false,
        }
    }

    pub fn mode(&self) -> SearchMode {
        if self.extended {
            SearchMode::Extend
        } else if self.regex {
            SearchMode::Regex
        } else {
            SearchMode::Normal
        }
    }

    /// Switch the search mode, keeping the mode flags mutually consistent.
    pub fn set_mode(&mut self, mode: SearchMode) {
        self.regex = mode == SearchMode::Regex;
        self.extended = mode == SearchMode::Extend;
    }

    pub fn mark_mode(&self) -> SearchMode {
        if self.mark_extended {
            SearchMode::Extend
        } else if self.mark_regex {
            SearchMode::Regex
        } else {
            SearchMode::Normal
        }
    }

    pub fn set_mark_mode(&mut self, mode: SearchMode) {
        self.mark_regex = mode == SearchMode::Regex;
        self.mark_extended = mode == SearchMode::Extend;
    }

    pub fn options(&self) -> SearchOptions {
        SearchOptions {
            match_case: self.match_case,
            whole_word: self.whole_word,
            regex: self.regex,
            enhanced_regex: self.enhanced_regex,
            dot_matches_newline: self.dot_matches_newline,
            wrap_around: self.wrap_around,
            in_selection: self.in_selection,
        }
    }

    pub fn mark_options(&self) -> SearchOptions {
        SearchOptions {
            match_case: self.mark_match_case,
            whole_word: self.mark_whole_word,
            regex: self.mark_regex,
            enhanced_regex: self.mark_enhanced_regex,
            dot_matches_newline: false,
            wrap_around: true,
            in_selection: false,
        }
    }

    pub fn query_text(&self, cx: &gpui_kit::App) -> String {
        self.query.read(cx).value().to_string()
    }

    pub fn replacement_text(&self, cx: &gpui_kit::App) -> String {
        self.replacement.read(cx).value().to_string()
    }

    pub fn mark_query_text(&self, cx: &gpui_kit::App) -> String {
        self.mark_query.read(cx).value().to_string()
    }

    fn pattern_for(&self, query: &str, extended: bool) -> String {
        if extended {
            luban_core::search::expand_extended(query)
        } else {
            query.to_string()
        }
    }

    pub fn engine(&self, query: &str) -> SearchEngine {
        SearchEngine::new(self.pattern_for(query, self.extended), self.options())
    }

    pub fn effective_scope(
        &self,
        has_selection: bool,
        selection: (usize, usize),
    ) -> Option<(usize, usize)> {
        if self.in_selection {
            if let Some(scope) = self.scope {
                Some(scope)
            } else if has_selection {
                Some(selection)
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn refresh(&mut self, text: &str, query: &str) {
        self.error = None;
        self.generation = self.generation.wrapping_add(1);
        self.all_results.clear();
        self.all_results_limited = false;
        if query.is_empty() {
            self.matches.clear();
            self.current = None;
            self.status.clear();
            return;
        }
        let engine = self.engine(query);
        if let Some(error) = engine.error() {
            self.error = Some(error.to_string());
            self.matches.clear();
            self.current = None;
            return;
        }
        self.matches = engine.find_all(text, self.scope, MAX_HIGHLIGHTS);
        self.current = None;
        if !self.status_sticky {
            self.status = if self.matches.len() >= MAX_HIGHLIGHTS {
                tr_args(
                    "find.match_count_limited",
                    &[
                        ("count", self.matches.len().to_string()),
                        ("first", MAX_HIGHLIGHTS.to_string()),
                    ],
                )
            } else if self.matches.len() == 1 {
                tr("find.match_count.one")
            } else {
                tr_args(
                    "find.match_count.other",
                    &[("count", self.matches.len().to_string())],
                )
            };
        }
    }

    pub fn replace_all(
        &self,
        text: &str,
        query: &str,
        replacement: &str,
    ) -> Result<(String, usize), String> {
        if query.is_empty() {
            return Err(tr("find.empty_query"));
        }
        self.engine(query)
            .replace_all(text, replacement, self.scope)
    }

    pub fn replace_match(
        &self,
        text: &str,
        query: &str,
        matched: Match,
        replacement: &str,
    ) -> String {
        self.engine(query).replace_match(text, matched, replacement)
    }

    pub fn clear(&mut self) {
        self.matches.clear();
        self.all_results.clear();
        self.all_results_limited = false;
        self.huge_results.clear();
        self.huge_truncated = false;
        self.current = None;
        self.error = None;
        self.status.clear();
        self.status_sticky = false;
        self.scope = None;
        self.highlighted_revision = u64::MAX;
        self.generation = self.generation.wrapping_add(1);
    }
}

/// Matches for one file, shown as an expandable group.
#[derive(Clone, Debug)]
pub struct FileGroup {
    pub path: PathBuf,
    pub relative: String,
    pub matches: Vec<FileMatch>,
}

pub struct FindInFilesPanel {
    pub open: bool,
    pub query: Entity<InputState>,
    pub folder: Entity<InputState>,
    pub masks: Entity<InputState>,
    pub include: Entity<InputState>,
    pub exclude: Entity<InputState>,
    pub replacement: Entity<InputState>,
    pub show_replace: bool,
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub enhanced_regex: bool,
    /// Search only open documents with a path instead of the folder.
    pub open_documents: bool,
    /// Search every workspace root instead of the folder field.
    pub workspace: bool,
    pub respect_gitignore: bool,
    pub results: Vec<FileMatch>,
    pub groups: Vec<FileGroup>,
    pub collapsed: HashSet<PathBuf>,
    pub files_scanned: usize,
    pub running: bool,
    pub cancel: Option<Arc<AtomicBool>>,
    pub status: String,
    pub error: Option<String>,
    pub generation: u64,
    pub truncated: bool,
    pub cancelled: bool,
}

impl FindInFilesPanel {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>) -> Self {
        let query =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("ff.query_placeholder")));
        let folder = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr("ff.folder_placeholder"))
                .default_value(
                    std::env::current_dir()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                )
        });
        let masks =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("ff.masks_placeholder")));
        let include =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("ff.include_placeholder")));
        let exclude =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("ff.exclude_placeholder")));
        let replacement =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("ff.replacement_placeholder")));
        Self {
            open: false,
            query,
            folder,
            masks,
            include,
            exclude,
            replacement,
            show_replace: false,
            match_case: false,
            whole_word: false,
            regex: false,
            enhanced_regex: false,
            open_documents: false,
            workspace: false,
            respect_gitignore: true,
            results: Vec::new(),
            groups: Vec::new(),
            collapsed: HashSet::new(),
            files_scanned: 0,
            running: false,
            cancel: None,
            status: String::new(),
            error: None,
            generation: 0,
            truncated: false,
            cancelled: false,
        }
    }

    pub fn search_options(&self) -> SearchOptions {
        SearchOptions {
            match_case: self.match_case,
            whole_word: self.whole_word,
            regex: self.regex,
            enhanced_regex: self.enhanced_regex,
            dot_matches_newline: true,
            wrap_around: true,
            in_selection: false,
        }
    }

    pub fn folder_path(&self, cx: &gpui_kit::App) -> PathBuf {
        let text = self.folder.read(cx).value().to_string();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        } else {
            crate::util::absolutize(std::path::Path::new(trimmed))
        }
    }

    pub fn query_text(&self, cx: &gpui_kit::App) -> String {
        self.query.read(cx).value().to_string()
    }

    pub fn masks_text(&self, cx: &gpui_kit::App) -> String {
        self.masks.read(cx).value().to_string()
    }

    pub fn include_text(&self, cx: &gpui_kit::App) -> String {
        self.include.read(cx).value().to_string()
    }

    pub fn exclude_text(&self, cx: &gpui_kit::App) -> String {
        self.exclude.read(cx).value().to_string()
    }

    pub fn replacement_text(&self, cx: &gpui_kit::App) -> String {
        self.replacement.read(cx).value().to_string()
    }

    /// Compile the include/exclude fields. Invalid patterns are an error and
    /// abort the search.
    pub fn filter(&self, cx: &gpui_kit::App) -> Result<luban_core::globs::GlobFilter, String> {
        luban_core::globs::GlobFilter::new(&self.include_text(cx), &self.exclude_text(cx))
            .map_err(|error| error.to_string())
    }

    /// Adopt the include/exclude defaults stored in the settings.
    pub fn use_settings_filter(
        &mut self,
        include: &str,
        exclude: &str,
        window: &mut Window,
        cx: &mut Context<LubanApp>,
    ) {
        self.include.update(cx, |input, cx| {
            input.set_value(include.to_string(), window, cx);
        });
        self.exclude.update(cx, |input, cx| {
            input.set_value(exclude.to_string(), window, cx);
        });
        cx.notify();
    }

    /// Replace the result list and group it by file. The first file and the
    /// files holding the first 20 matches start expanded.
    pub fn set_results(&mut self, results: Vec<FileMatch>, roots: &[PathBuf]) {
        self.results = results;
        let mut by_file: Vec<FileGroup> = Vec::new();
        for matched in &self.results {
            if let Some(group) = by_file.iter_mut().find(|group| group.path == matched.path) {
                group.matches.push(matched.clone());
            } else {
                by_file.push(FileGroup {
                    path: matched.path.clone(),
                    relative: crate::workspace_replace::relative_path(roots, &matched.path),
                    matches: vec![matched.clone()],
                });
            }
        }
        self.collapsed.clear();
        let mut seen = 0usize;
        for (index, group) in by_file.iter().enumerate() {
            let expanded = index == 0 || seen < 20;
            if !expanded {
                self.collapsed.insert(group.path.clone());
            }
            seen += group.matches.len();
        }
        self.groups = by_file;
    }

    pub fn toggle_group(&mut self, path: &Path) {
        if !self.collapsed.remove(path) {
            self.collapsed.insert(path.to_path_buf());
        }
    }

    pub fn clear_results(&mut self) {
        self.results.clear();
        self.groups.clear();
        self.collapsed.clear();
        self.files_scanned = 0;
        self.truncated = false;
        self.cancelled = false;
        self.error = None;
        self.status.clear();
    }
}

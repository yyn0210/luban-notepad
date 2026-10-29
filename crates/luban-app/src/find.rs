use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gpui_kit::component::input::InputState;
use gpui_kit::{AppContext as _, Context, Entity, Window};
use luban_core::search::{Match, SearchEngine, SearchOptions};
use luban_core::workspace::FileMatch;

use crate::app::LubanApp;
use crate::i18n::{tr, tr_args};

pub const MAX_HIGHLIGHTS: usize = 20_000;

pub struct FindPanel {
    pub open: bool,
    pub show_replace: bool,
    pub query: Entity<InputState>,
    pub replacement: Entity<InputState>,
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub enhanced_regex: bool,
    pub in_selection: bool,
    pub dot_matches_newline: bool,
    pub matches: Vec<Match>,
    pub current: Option<usize>,
    pub error: Option<String>,
    pub dirty: bool,
    pub scope: Option<(usize, usize)>,
    pub highlighted_revision: u64,
    /// Bumped whenever the match set or current match changes, so decorations
    /// can skip rebuilding on unrelated renders.
    pub generation: u64,
    pub status: String,
    /// Global matches collected by an explicit Find All on a disk-engine tab.
    pub huge_results: Vec<luban_core::disk_engine::SearchHit>,
    pub huge_limit: usize,
    pub huge_truncated: bool,
}

impl FindPanel {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>) -> Self {
        let query =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("find.query_placeholder")));
        let replacement = cx
            .new(|cx| InputState::new(window, cx).placeholder(tr("find.replacement_placeholder")));
        Self {
            open: false,
            show_replace: false,
            query,
            replacement,
            match_case: false,
            whole_word: false,
            regex: false,
            enhanced_regex: false,
            in_selection: false,
            dot_matches_newline: false,
            matches: Vec::new(),
            current: None,
            error: None,
            dirty: true,
            scope: None,
            highlighted_revision: u64::MAX,
            generation: 0,
            status: String::new(),
            huge_results: Vec::new(),
            huge_limit: 5000,
            huge_truncated: false,
        }
    }

    pub fn options(&self) -> SearchOptions {
        SearchOptions {
            match_case: self.match_case,
            whole_word: self.whole_word,
            regex: self.regex,
            enhanced_regex: self.enhanced_regex,
            dot_matches_newline: self.dot_matches_newline,
            wrap_around: true,
            in_selection: self.in_selection,
        }
    }

    pub fn query_text(&self, cx: &gpui_kit::App) -> String {
        self.query.read(cx).value().to_string()
    }

    pub fn replacement_text(&self, cx: &gpui_kit::App) -> String {
        self.replacement.read(cx).value().to_string()
    }

    pub fn engine(&self, query: &str) -> SearchEngine {
        SearchEngine::new(query, self.options())
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
        self.huge_results.clear();
        self.huge_truncated = false;
        self.current = None;
        self.error = None;
        self.status.clear();
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

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::assets::IconName;
use gpui_kit::base::input::{AddCursorAbove, AddCursorBelow};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::input::{
    Copy, Cut, Editor, EditorState, FoldRange, Indent, Input, InputEvent, InputState, Outdent,
    Paste, Position, RangeDecoration, RangeDecorationCollection, RangeDecorationStyle, Redo,
    RopeExt, Search, SelectAll, TabSize, TextDecoration, TextDecorationCollection, Undo,
};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::{ScrollableElement as _, Scrollbar, ScrollbarMode};
use gpui_kit::component::slider::SliderEvent;
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::toolbar::{Toolbar, ToolbarGroup};
use gpui_kit::component::{Disableable as _, Selectable as _, Sizable as _, Size, TitleBar};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::*;
use luban_core::atomic_write::write_atomic;
use luban_core::bookmarks::BookmarkSet;
use luban_core::disk_engine::{DiskEngine, RecoveryInfo, WindowText, WINDOW_BYTES};
use luban_core::document::{DiskStatus, Document, PartialRange};
use luban_core::encoding::{self, TextEncoding};
use luban_core::eol::{self, Eol};
use luban_core::export::{self, ExportFormat};
use luban_core::file_ops;
use luban_core::globs::GlobFilter;
use luban_core::i18n::language_display_name;
use luban_core::language;
use luban_core::large_file::{format_bytes, RangeMode, SizeUnit};
use luban_core::line_ops::{self, SortMode};
use luban_core::local_history::{HistoryEntry, HistoryStore};
use luban_core::macros::SavedMacro;
use luban_core::search::{ByteMatch, Match, ReplacePreview};
use luban_core::session::{GroupSession, Session, SessionTab, SplitLayout, WindowSession};
use luban_core::settings::{LanguageSettings, Settings, ThemeChoice};
use luban_core::text as textutil;
use luban_core::undo::EditKind;
use luban_core::workspace::{
    list_directory, search_workspace, FileEntry, FileMatch, FindInFilesOptions, FindInFilesOutcome,
    WorkspaceSearchOptions,
};
use luban_core::workspace_file::{WorkspaceFile, WorkspaceSettings};
use luban_core::workspace_index::{build_index, IndexOptions, WorkspaceIndex};

use crate::column_editor::ColumnEditorState;
use crate::commands::{self, CommandContext, Keymap};
use crate::dialogs::{
    next_listing_generation, CloseIntent, ConfirmState, FileDialogMode, FileDialogPurpose,
    FileDialogState, FileFilter, GoToLineState, LanguagePickerState, SaveAsRequest, SaveThen,
};
use crate::document_map;
use crate::explorer::ExplorerState;
use crate::file_watch::{FileWatcher, WatchEventKind};
use crate::find::{FindInFilesPanel, FindPanel};
use crate::huge_file::{
    clamp_top_line, diff_bytes, max_top_line, top_line_from_scroll, window_max_top_line,
    HugeFileUi, SaveJob, SearchJob, SearchKind, WindowJump, WindowRequest, WindowTarget,
    EDGE_LINES, PREFETCH_LINES,
};
use crate::huge_gutter::{self, HugeGutterProps};
use crate::huge_scrollbar::{self, HugeScrollbarProps};
use crate::i18n::{active_language_name, tr, tr_args};
use crate::large_file::{LargeFilePanel, RangeField};
use crate::logging;
use crate::macro_ui::MacroUiState;
use crate::navigation::{BreadcrumbAction, NavigationEntry, NavigationHistory};
use crate::outline::OutlineState;
use crate::palette;
use crate::path_prompt::{NamePromptKind, NamePromptState};
use crate::preferences::{matching_fonts, FontPickerState, PreferencesState};
use crate::quick_open::{self, QuickRow};
use crate::settings_search;
use crate::syntax_theme::{self, THEME_OPTIONS};
use crate::tab_groups::{self, SplitDirection};
use crate::theme::apply_theme;
use crate::util::{self, language_key};
use crate::window_registry;
use crate::workspace_replace::{
    self, FilePreview, ReplacePreview as WorkspaceReplacePreview, ReplaceUndo, SkippedFile,
    UndoEntry,
};

/// What a freshly opened window starts with.
#[derive(Clone, Debug, Default)]
pub enum WindowInit {
    /// One untitled document.
    #[default]
    Blank,
    /// Restore the saved window layout.
    Session(Box<WindowSession>),
    /// Open these tabs (used by "move tab to new window").
    Tabs(Vec<SessionTab>),
}

pub struct LaunchOptions {
    pub files: Vec<PathBuf>,
    pub smoke_test: bool,
    /// The platform has no client-decorated shadow margin to blend against, so
    /// the application removes the window manager titlebar itself and draws the
    /// chrome for every window state.
    pub manual_window_chrome: bool,
    /// Disk-engine documents left behind by a crash, in display order.
    pub recoveries: Vec<RecoveryInfo>,
    /// Initial content for this window.
    pub init: WindowInit,
    /// Restore the queued crash-recovery entry immediately (used when a dirty
    /// disk-engine tab moves to a new window).
    pub auto_recovery: bool,
    /// Process-local window identifier used by the session registry.
    pub window_id: u64,
}

/// A file on disk no longer matches the buffer's recorded identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalNotice {
    Changed,
    Missing,
}

/// What happens after every queued tab has been saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatchSaveKind {
    SaveAll,
    CloseAll,
    Exit,
}

/// A crash-recovery offer shown at startup.
#[derive(Clone, Debug)]
pub struct RecoveryPrompt {
    pub info: RecoveryInfo,
    /// Set when the source changed or disappeared; restoring is refused.
    pub problem: Option<String>,
}

/// Local-history overlay for the active file.
struct HistoryDialogState {
    path: PathBuf,
    entries: Vec<HistoryEntry>,
    error: Option<String>,
}

/// Command palette or quick-open overlay.
struct PaletteOverlay {
    kind: PaletteKind,
    state: Entity<CommandState>,
    /// Flat command IDs for the command palette.
    command_groups: Vec<Vec<&'static str>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaletteKind {
    Commands,
    QuickOpen,
}

/// Quick-open rows and the current query error.
#[derive(Default)]
struct QuickOpenState {
    rows: Vec<QuickRow>,
    error: Option<String>,
}

/// Workspace replace preview, pending state and the one-level undo record.
#[derive(Default)]
pub(crate) struct WorkspaceReplaceState {
    preview: Option<WorkspaceReplacePreview>,
    running: bool,
    cancel: Option<Arc<AtomicBool>>,
    undo: Option<ReplaceUndo>,
    /// Query and replacement captured by the preview so apply uses the same
    /// options.
    query: String,
    replacement: String,
    options: Option<luban_core::search::SearchOptions>,
    filter: GlobFilter,
    error: Option<String>,
    status: String,
    generation: u64,
    /// Whether the buffered preview has been confirmed once.
    confirmed: bool,
    /// Query/options signature the preview was computed for.
    preview_signature: String,
}

/// A tab closed during this session, available through Ctrl+Shift+T.
#[derive(Clone)]
struct ClosedTab {
    path: Option<PathBuf>,
    name: String,
    text: Option<String>,
    encoding: TextEncoding,
    eol: Eol,
    language: String,
    cursor: usize,
    anchor: usize,
    modified: bool,
    huge: bool,
    huge_line: Option<u64>,
    huge_column: Option<u64>,
    /// Byte range of a partially opened tab.
    range: Option<[u64; 2]>,
    non_text: bool,
    file_size: u64,
    last_modified: Option<SystemTime>,
}

/// Keyboard shortcut editor overlay state.
#[derive(Default)]
struct ShortcutEditorState {
    /// Command waiting for its next keystroke.
    waiting_for: Option<String>,
    error: Option<String>,
}

struct SortDialogState {
    column: Entity<InputState>,
    ascending: bool,
    ignore_case: bool,
    error: Option<String>,
}

struct FilterDialogState {
    needle: Entity<InputState>,
    keep: bool,
    match_case: bool,
    regex: bool,
    error: Option<String>,
}

struct ReplacePreviewState {
    preview: ReplacePreview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BookmarkAction {
    Cut,
    Copy,
    Delete,
}

/// Sequential close-other/close-right plan. Each modified tab goes through the
/// normal per-tab save/discard confirmation before the next one is attempted.
struct TrimPlan {
    keep_id: u64,
    /// Tabs that will be closed after the save phase.
    queue: Vec<u64>,
    /// Modified tabs among the queue, saved before anything is closed.
    save_queue: Vec<u64>,
    /// Remove this workspace root once the queue is closed.
    remove_root: Option<PathBuf>,
}

/// Drag payload for reordering tabs.
#[derive(Clone)]
struct TabDrag {
    id: u64,
    label: SharedString,
}

struct TabDragPreview {
    label: SharedString,
}

impl Render for TabDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .rounded_sm()
            .bg(_cx.theme().colors.secondary)
            .text_sm()
            .child(self.label.clone())
    }
}

pub struct Tab {
    pub id: u64,
    pub doc: Document,
    pub editor: Entity<EditorState>,
    pub decorations: RangeDecorationCollection,
    pub bookmark_decorations: RangeDecorationCollection,
    /// Bracket-colour text decorations.
    pub bracket_decorations: TextDecorationCollection,
    /// Tab group (0 or 1).
    pub group: usize,
    pub bookmarks: BookmarkSet,
    pub bookmark_revision: u64,
    pub fold_generation: u64,
    pub needs_sync: bool,
    /// The editor emitted a real text change that has not been synced into
    /// the document yet. Selection and focus events do not set this.
    pub pending_edit: bool,
    /// Cached word count and the document revision it was computed for.
    word_count: Option<(u64, usize)>,
    /// Present when the tab is backed by the disk engine.
    pub huge: Option<HugeFileUi>,
    pub highlighted_revision: u64,
    /// External change/deletion discovered by the file watcher.
    pub external: Option<ExternalNotice>,
    /// Files at or above `Settings::large_file_threshold` open in a reduced
    /// mode: no syntax highlighting, wrapping, bracket matching or live find.
    pub large_file: bool,
    decorations_key: Option<DecorationKey>,
    /// Revision and enable flag the bracket decorations were computed for.
    bracket_key: Option<(u64, bool)>,
    _subscription: Subscription,
    /// Re-renders the app when the editor notifies without an `InputEvent`
    /// (wheel scrolling, cursor motion), so the file scrollbar and the global
    /// gutter follow the viewport.
    _notification: Subscription,
}

type DecorationKey = (u64, usize, u64, bool, bool, bool);

struct PendingOpenAt {
    path: PathBuf,
    line: usize,
    column: usize,
    match_len: usize,
}

/// A background load started by the large-file controller.
struct LargeLoadJob {
    path: PathBuf,
    generation: u64,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    /// Total bytes to read, used for the status percentage.
    size: u64,
    /// Tab that should be replaced after the load succeeds.
    replace_tab: Option<u64>,
    /// The load indexes a disk-engine document.
    huge: bool,
}

/// A session tab prepared for loading on the background executor.
struct RestoreSpec {
    path: Option<PathBuf>,
    text: Option<String>,
    encoding: TextEncoding,
    eol: Eol,
    language: String,
    cursor: usize,
    anchor: usize,
    modified: bool,
    fallback_name: String,
    range: Option<[u64; 2]>,
    huge: bool,
    huge_line: Option<u64>,
    huge_column: Option<u64>,
    bookmarks: Vec<usize>,
    /// Editor group the tab belongs to.
    group: usize,
}

impl RestoreSpec {
    fn load(self, max_size: u64) -> Document {
        if self.huge {
            if let Some(path) = &self.path {
                let cancel = AtomicBool::new(false);
                let progress = AtomicU64::new(0);
                let line = self.huge_line.unwrap_or(1);
                let restored = match self.range {
                    Some([from, to]) => {
                        Document::open_huge_range(path, from, to, line, &cancel, &progress)
                    }
                    None => Document::open_huge(path, line, &cancel, &progress),
                };
                let restored = restored.or_else(|err| {
                    log::warn!(
                        "could not restore huge range {}: {err}; opening the whole file",
                        path.display()
                    );
                    Document::open_huge(path, 1, &cancel, &progress)
                });
                match restored {
                    Ok(mut doc) => {
                        let column = self.huge_column.unwrap_or(1);
                        if let Some(huge) = doc.huge_mut() {
                            let (offset, window) = {
                                let engine = huge.read();
                                let offset = engine.offset_for_line(line, column).unwrap_or(0);
                                let window = engine.read_window(offset, WINDOW_BYTES).ok();
                                (offset, window)
                            };
                            if let Some(window) = window {
                                huge.initial_window = Some(window.clone());
                                huge.cursor = offset.min(window.start.saturating_add(window.len));
                                huge.anchor = huge.cursor;
                                huge.scroll_line = window.line;
                            }
                        }
                        return doc;
                    }
                    Err(err) => log::warn!("could not restore huge {}: {err}", path.display()),
                }
            }
        }
        let mut doc = if let Some(path) = &self.path {
            let opened = Self::open_restored(path, self.range, max_size).or_else(|err| {
                log::warn!("could not restore {}: {err}", path.display());
                Document::open_with_limit(path, max_size)
            });
            match opened {
                Ok(mut doc) => {
                    if let Some(text) = &self.text {
                        // Ordinary buffers are stored LF-normalized; non-text
                        // buffers are stored raw and must keep their bytes.
                        doc.attach_text(if doc.is_non_text() {
                            text.clone()
                        } else {
                            eol::to_lf(text)
                        });
                        doc.encoding = self.encoding;
                        doc.eol = self.eol;
                        doc.language = self.language.clone();
                        if self.modified {
                            doc.touch();
                        } else {
                            doc.mark_saved();
                        }
                    }
                    doc
                }
                Err(err) => {
                    log::warn!("could not restore {}: {err}", path.display());
                    Self::untitled(
                        self.fallback_name,
                        self.text,
                        self.encoding,
                        self.eol,
                        self.language,
                        self.modified,
                    )
                }
            }
        } else {
            Self::untitled(
                self.fallback_name,
                self.text,
                self.encoding,
                self.eol,
                self.language,
                self.modified,
            )
        };
        let total = doc.char_count();
        doc.cursor = self.cursor.min(total);
        doc.anchor = self.anchor.min(total);
        doc
    }

    fn open_restored(
        path: &Path,
        range: Option<[u64; 2]>,
        max_size: u64,
    ) -> luban_core::Result<Document> {
        let Some([from, to]) = range else {
            return Document::open_with_limit(path, max_size);
        };
        let cancel = AtomicBool::new(false);
        let progress = std::sync::atomic::AtomicU64::new(0);
        let result = Document::open_range(path, from, to, max_size, &cancel, &progress);
        if matches!(result, Err(luban_core::Error::InvalidRange { .. })) {
            if let Ok(metadata) = fs::metadata(path) {
                let size = metadata.len();
                if size > 0 {
                    let from = from.min(size - 1);
                    let to = to.min(size).max(from + 1);
                    return Document::open_range(path, from, to, max_size, &cancel, &progress);
                }
            }
        }
        result
    }

    fn untitled(
        name: String,
        text: Option<String>,
        encoding: TextEncoding,
        eol: Eol,
        language: String,
        modified: bool,
    ) -> Document {
        let mut doc = Document::new_untitled(name);
        doc.attach_text(text.as_deref().map(eol::to_lf).unwrap_or_default());
        doc.encoding = encoding;
        doc.eol = eol;
        doc.language = language;
        if modified {
            doc.touch();
        }
        doc
    }
}

pub struct LubanApp {
    pub settings: Settings,
    pub tabs: Vec<Tab>,
    pub active: usize,
    next_tab_id: u64,
    untitled_counter: usize,
    pending_opens: Vec<PathBuf>,
    pending_open_at: Option<PendingOpenAt>,
    pub find: FindPanel,
    pub find_in_files: FindInFilesPanel,
    pub explorer: ExplorerState,
    /// Focus target for the file tree so arrow keys act on rows.
    explorer_focus: FocusHandle,
    /// Focus target and selection for the symbol outline.
    outline_focus: FocusHandle,
    outline_selected: Option<usize>,
    /// Additional workspace roots beyond `workspace_root`.
    pub workspace_roots: Vec<PathBuf>,
    pub workspace_file: Option<PathBuf>,
    pub workspace_name: String,
    pub workspace_settings: WorkspaceSettings,
    /// Settings loaded from disk, without workspace overrides.
    global_settings: Settings,
    pub workspace_index: Option<WorkspaceIndex>,
    pub indexing: bool,
    index_cancel: Option<Arc<AtomicBool>>,
    index_generation: u64,
    pub outline: OutlineState,
    pub nav: NavigationHistory,
    /// Active editor group (0 or 1).
    pub active_group: usize,
    active_in_group: [Option<u64>; 2],
    /// Active tab positions restored from the session, per group.
    restore_group_active: [usize; 2],
    pub split: SplitDirection,
    quick: QuickOpenState,
    /// Query string mirrored from the quick-open palette for filtering.
    quick_query: String,
    pub(crate) replace_state: WorkspaceReplaceState,
    pub name_prompt: Option<NamePromptState>,
    pending_settings_import: Option<PathBuf>,
    /// Process-local window id and whether the session registry knows it.
    window_id: u64,
    window_registered: bool,
    /// Recover the moved disk-engine tab as soon as the window exists.
    auto_recovery: bool,
    pub large_file: LargeFilePanel,
    large_loads: Vec<LargeLoadJob>,
    next_load_generation: u64,
    pub preferences: PreferencesState,
    pub file_dialog: Option<FileDialogState>,
    file_path_input: Entity<InputState>,
    file_name_input: Entity<InputState>,
    pub confirm: Option<ConfirmState>,
    overwrite_save: Option<SaveAsRequest>,
    pending_export: Option<ExportFormat>,
    pub go_to_line: Option<GoToLineState>,
    pub language_picker: Option<LanguagePickerState>,
    pub about_open: bool,
    pub shortcuts_open: bool,
    history_dialog: Option<HistoryDialogState>,
    palette: Option<PaletteOverlay>,
    shortcut_editor: Option<ShortcutEditorState>,
    pub column_editor: ColumnEditorState,
    pub macro_ui: MacroUiState,
    macro_recording: bool,
    macro_recorded: Vec<String>,
    macro_playback: bool,
    macro_keymap: Vec<(commands::Binding, String)>,
    replace_preview: Option<ReplacePreviewState>,
    /// Last match end reached by Select Next Occurrence, keyed by needle and tab.
    occurrence_anchor: Option<(String, usize, u64)>,
    sort_dialog: Option<SortDialogState>,
    filter_dialog: Option<FilterDialogState>,
    /// Snapshot waiting for the user to confirm replacing a modified buffer.
    pending_history_restore: Option<HistoryEntry>,
    /// Tabs closed during this session, newest last.
    closed_tabs: Vec<ClosedTab>,
    /// Active close-other/close-right sequence.
    trim_plan: Option<TrimPlan>,
    /// Effective command shortcuts, rebuilt whenever settings change.
    keymap: Keymap,
    history: HistoryStore,
    pub status: String,
    pub workspace_root: Option<PathBuf>,
    last_session_save: Instant,
    allow_close: bool,
    focus_editor: bool,
    smoke_test: bool,
    manual_window_chrome: bool,
    pending_exit_saves: Vec<u64>,
    pending_close_saves: Vec<u64>,
    pending_find_next: bool,
    pending_replace: bool,
    pending_file_confirm: bool,
    pending_name_confirm: bool,
    pending_go_to_line: bool,
    pending_close_all_after_saves: bool,
    /// Huge tabs queued by Save All / Close All / Exit.
    pending_huge_saves: Vec<u64>,
    pending_huge_all: bool,
    pending_huge_exit: bool,
    /// Normal tabs with paths queued by Save All / Close All / Exit. They are
    /// saved one at a time so an external-change confirmation can pause the
    /// batch instead of closing the tab behind the dialog.
    pending_normal_saves: Vec<u64>,
    pending_normal_all: bool,
    pending_normal_close_all: bool,
    pending_normal_exit: bool,
    pending_normal_skipped_partial: bool,
    /// Documents that failed during a Save All run and are reported together
    /// after the queue drains.
    pending_save_failures: Vec<String>,
    /// The running batch ends by closing all tabs or quitting.
    pending_batch_is_close: bool,
    /// A save waiting for the user to confirm overwriting an externally
    /// modified file.
    pending_huge_overwrite: Option<(u64, Option<PathBuf>, SaveThen)>,
    /// A normal document save waiting for the same confirmation.
    pending_normal_overwrite: Option<(u64, Option<PathBuf>, SaveThen)>,
    /// Directory watcher for every open file.
    watch: FileWatcher,
    /// Crash recovery offers not yet shown.
    recovery_queue: VecDeque<RecoveryInfo>,
    /// The recovery offer currently displayed.
    recovery_prompt: Option<RecoveryPrompt>,
    /// Recovery entry currently being rebuilt in the background.
    recovery_restoring: Option<RecoveryInfo>,
    /// Line/column to apply once a disk-engine open started from a search hit
    /// has finished indexing.
    pending_huge_at: Option<(PathBuf, u64, u64)>,
    window_title: String,
    window_width: f32,
    window_height: f32,
    window_maximized: bool,
    _subscriptions: Vec<Subscription>,
}

/// Maximum width of a recent-file menu label, counted in display columns
/// (one per half-width character, two per CJK or other wide character).
///
/// The popup menu caps its own width at 500px but does not clip item text, so
/// a full path would otherwise paint past the menu background. Counting
/// columns instead of chars keeps CJK-heavy paths within the same budget.
const RECENT_PATH_COLUMNS: usize = 48;

/// Tabs size to their title so short file names do not leave dead space,
/// while the range keeps tiny and very long labels usable.
const TAB_MIN_WIDTH: f32 = 80.0;
const TAB_MAX_WIDTH: f32 = 180.0;

/// Elide the middle of a string that is wider than `max_columns`, keeping
/// both ends readable. Paths benefit from the trailing component, so the
/// second half of the budget goes to the tail.
fn truncate_middle(text: &str, max_columns: usize) -> String {
    if max_columns == 0 {
        return String::new();
    }
    let total: usize = text.chars().map(char_columns).sum();
    if total <= max_columns {
        return text.to_string();
    }

    let budget = max_columns.saturating_sub(1); // room for the ellipsis
    let head_budget = budget / 2;
    let tail_budget = budget - head_budget;

    let mut head = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let width = char_columns(ch);
        if used + width > head_budget {
            break;
        }
        used += width;
        head.push(ch);
    }

    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for ch in text.chars().rev() {
        let width = char_columns(ch);
        if used + width > tail_budget {
            break;
        }
        used += width;
        tail.push(ch);
    }
    tail.reverse();

    head.push('…');
    head.extend(tail);
    head
}

/// Approximate display width of a character in terminal-style columns.
fn char_columns(ch: char) -> usize {
    let code = ch as u32;
    let wide = matches!(
        code,
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x1F900..=0x1F9FF
            | 0x20000..=0x3FFFD
    );
    if wide {
        2
    } else {
        1
    }
}

/// Destination index after moving one item, or `None` for a no-op move.
fn reorder_index(len: usize, from: usize, target: usize) -> Option<usize> {
    if from >= len || target > len || from == target || from + 1 == target {
        return None;
    }
    Some((if from < target { target - 1 } else { target }).min(len.saturating_sub(1)))
}

/// Indices closed by "Close Others" or "Close to the Right".
fn trim_queue(len: usize, keep: usize, right: bool) -> Vec<usize> {
    if keep >= len {
        return Vec::new();
    }
    if right {
        (keep + 1..len).collect()
    } else {
        (0..len).filter(|index| *index != keep).collect()
    }
}

fn count_words(text: &str) -> usize {
    text.split_whitespace().count()
}

fn session_tab_from(tab: &Tab) -> SessionTab {
    let huge = tab.doc.is_huge();
    let (huge_line, huge_column, huge_scroll) = match tab.doc.huge() {
        Some(state) => {
            let (line, column) = state
                .engine
                .try_read()
                .map(|engine| engine.line_col_at(state.cursor).unwrap_or((1, 1)))
                .unwrap_or((1, 1));
            (Some(line), Some(column), Some(state.scroll_line as f32))
        }
        None => (None, None, None),
    };
    SessionTab {
        path: tab.doc.path.clone(),
        // Large buffers are never written to the session: the file is restored
        // from disk instead.
        text: if !tab.large_file
            && !huge
            && tab.doc.partial_range().is_none()
            && (tab.doc.is_modified() || tab.doc.is_untitled())
        {
            Some(tab.doc.text.clone())
        } else {
            None
        },
        encoding: tab.doc.encoding,
        eol: tab.doc.eol,
        language: tab.doc.language.clone(),
        cursor: if huge { 0 } else { tab.doc.cursor },
        anchor: if huge { 0 } else { tab.doc.anchor },
        scroll_offset: 0.0,
        modified: !tab.large_file
            && !huge
            && tab.doc.partial_range().is_none()
            && tab.doc.is_modified(),
        range: tab
            .doc
            .partial_range()
            .map(|range| [range.start, range.end]),
        source_size: tab.doc.partial_range().map(|range| range.total),
        huge,
        huge_line,
        huge_column,
        huge_scroll,
        huge_modified: huge && tab.doc.is_modified(),
        recovery_id: tab.doc.huge().and_then(|state| {
            state
                .engine
                .try_read()
                .ok()
                .and_then(|engine| engine.recovery_id().map(str::to_string))
        }),
        bookmarks: if huge {
            Vec::new()
        } else {
            tab.bookmarks.as_vec()
        },
    }
}

impl LubanApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, mut options: LaunchOptions) -> Self {
        let mut settings = Settings::load();
        // Drop entries for commands this build no longer knows about, and
        // entries that cannot be parsed. Conflicts stay visible in the editor
        // rather than silently changing which command wins.
        settings.keybindings.retain(|id, text| {
            let valid = commands::spec(id).is_some()
                && commands::parse_binding(text)
                    .is_ok_and(|binding| commands::binding_allowed(&binding));
            if !valid {
                log::warn!("ignoring invalid shortcut override for {id}");
            }
            valid
        });
        let keybindings = settings.keybindings.clone();
        let global_settings = settings.clone();
        let init = std::mem::take(&mut options.init);
        let window_id = options.window_id;
        let options_auto_recovery = options.auto_recovery;
        crate::i18n::apply_language(&settings.ui_language);
        apply_theme(&settings, Some(window), cx);
        if let Err(error) = syntax_theme::apply(&settings, cx) {
            log::warn!("could not apply syntax theme: {error}");
        }

        let mut app = Self {
            settings,
            global_settings,
            workspace_roots: Vec::new(),
            workspace_file: None,
            workspace_name: String::new(),
            workspace_settings: WorkspaceSettings::default(),
            workspace_index: None,
            indexing: false,
            index_cancel: None,
            index_generation: 0,
            outline: OutlineState::default(),
            nav: NavigationHistory::new(),
            active_group: 0,
            active_in_group: [None, None],
            restore_group_active: [0, 0],
            split: SplitDirection::None,
            quick: QuickOpenState::default(),
            quick_query: String::new(),
            replace_state: WorkspaceReplaceState::default(),
            name_prompt: None,
            pending_settings_import: None,
            window_id,
            window_registered: true,
            auto_recovery: options_auto_recovery,
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 1,
            untitled_counter: 0,
            pending_opens: Vec::new(),
            pending_open_at: None,
            find: FindPanel::new(window, cx),
            find_in_files: FindInFilesPanel::new(window, cx),
            explorer: ExplorerState::new(),
            explorer_focus: cx.focus_handle(),
            outline_focus: cx.focus_handle(),
            outline_selected: None,
            large_file: LargeFilePanel::new(window, cx),
            large_loads: Vec::new(),
            next_load_generation: 1,
            preferences: PreferencesState::new(window, cx),
            file_dialog: None,
            file_path_input: cx
                .new(|cx| InputState::new(window, cx).placeholder(tr("dialog.path_placeholder"))),
            file_name_input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr("dialog.file_name_placeholder"))
            }),
            confirm: None,
            overwrite_save: None,
            pending_export: None,
            go_to_line: None,
            language_picker: None,
            about_open: false,
            shortcuts_open: false,
            history_dialog: None,
            palette: None,
            shortcut_editor: None,
            column_editor: ColumnEditorState::new(window, cx),
            macro_ui: MacroUiState::new(window, cx),
            macro_recording: false,
            macro_recorded: Vec::new(),
            macro_playback: false,
            macro_keymap: Vec::new(),
            replace_preview: None,
            occurrence_anchor: None,
            sort_dialog: None,
            filter_dialog: None,
            pending_history_restore: None,
            closed_tabs: Vec::new(),
            trim_plan: None,
            keymap: Keymap::build(&keybindings),
            history: HistoryStore::system(),
            status: String::new(),
            workspace_root: None,
            last_session_save: Instant::now(),
            allow_close: false,
            focus_editor: true,
            smoke_test: options.smoke_test,
            manual_window_chrome: options.manual_window_chrome,
            pending_exit_saves: Vec::new(),
            pending_close_saves: Vec::new(),
            pending_find_next: false,
            pending_replace: false,
            pending_file_confirm: false,
            pending_name_confirm: false,
            pending_go_to_line: false,
            pending_close_all_after_saves: false,
            pending_huge_saves: Vec::new(),
            pending_huge_all: false,
            pending_huge_exit: false,
            pending_normal_saves: Vec::new(),
            pending_normal_all: false,
            pending_normal_close_all: false,
            pending_normal_exit: false,
            pending_normal_skipped_partial: false,
            pending_save_failures: Vec::new(),
            pending_batch_is_close: false,
            pending_huge_overwrite: None,
            pending_normal_overwrite: None,
            watch: FileWatcher::new(),
            recovery_queue: options.recoveries.into_iter().collect(),
            recovery_prompt: None,
            recovery_restoring: None,
            pending_huge_at: None,
            window_title: "Luban Notepad".to_string(),
            window_width: 1280.0,
            window_height: 820.0,
            window_maximized: false,
            _subscriptions: Vec::new(),
        };

        app.rebuild_macro_keymap();

        // The shortcut editor captures keystrokes even when no input owns the
        // focus; the disk-engine history shortcuts must run before the
        // editor's own bound actions. Both are handled globally here.
        let weak_shortcuts = cx.weak_entity();
        app._subscriptions
            .push(cx.intercept_keystrokes(move |event, window, cx| {
                if event.keystroke.key.eq_ignore_ascii_case("escape") {
                    let handled = weak_shortcuts
                        .update(cx, |app, cx| {
                            if app.overwrite_save.is_some() {
                                app.cancel_overwrite(cx);
                                true
                            } else if app.file_dialog.is_some() {
                                app.cancel_file_dialog(cx);
                                true
                            } else if app.name_prompt.is_some() {
                                app.name_prompt = None;
                                app.focus_editor = true;
                                cx.notify();
                                true
                            } else if app.confirm.is_some() {
                                app.confirm_cancel(cx);
                                true
                            } else if app.palette.is_some() {
                                app.close_palette(window, cx);
                                true
                            } else if app.history_dialog.is_some() {
                                app.history_dialog = None;
                                app.pending_history_restore = None;
                                app.focus_editor = true;
                                cx.notify();
                                true
                            } else if app.trim_plan.is_some() {
                                app.trim_plan = None;
                                true
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false);
                    if handled {
                        cx.stop_propagation();
                    }
                    return;
                }
                let waiting = weak_shortcuts
                    .update(cx, |app, _| {
                        app.shortcut_editor
                            .as_ref()
                            .is_some_and(|editor| editor.waiting_for.is_some())
                    })
                    .unwrap_or(false);
                if waiting {
                    let handled = weak_shortcuts
                        .update(cx, |app, cx| {
                            app.capture_shortcut(&event.keystroke, window, cx)
                        })
                        .unwrap_or(false);
                    if handled {
                        cx.stop_propagation();
                    }
                    return;
                }
                let modifiers = event.keystroke.modifiers;
                if !(modifiers.secondary() || modifiers.control) {
                    return;
                }
                let key = event.keystroke.key.to_ascii_lowercase();
                if !matches!(key.as_str(), "z" | "y" | "home" | "end") {
                    return;
                }
                let handled = weak_shortcuts
                    .update(cx, |app, cx| {
                        if !app.active_doc().is_some_and(Document::is_huge) {
                            return false;
                        }
                        match (key.as_str(), modifiers.shift) {
                            ("z", false) => app.huge_history(false, window, cx),
                            ("y", false) | ("z", true) => app.huge_history(true, window, cx),
                            ("home", false) => app.huge_jump_line(1, 1, window, cx),
                            ("end", false) => app.huge_jump_end(window, cx),
                            _ => return false,
                        }
                        true
                    })
                    .unwrap_or(false);
                if handled {
                    cx.stop_propagation();
                }
            }));

        app._subscriptions.push(
            cx.subscribe(&app.find.query, |this, _, event, cx| match event {
                InputEvent::Change => {
                    this.find.dirty = true;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    this.pending_find_next = true;
                    cx.notify();
                }
                _ => {}
            }),
        );
        app._subscriptions.push(cx.subscribe(
            &app.find.replacement,
            |this, _, event, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } => {
                    this.pending_replace = true;
                    cx.notify();
                }
                _ => {}
            },
        ));
        for entity in [
            app.find_in_files.query.clone(),
            app.find_in_files.folder.clone(),
            app.find_in_files.masks.clone(),
            app.find_in_files.include.clone(),
            app.find_in_files.exclude.clone(),
            app.find_in_files.replacement.clone(),
            app.preferences.search.clone(),
        ] {
            app._subscriptions
                .push(cx.subscribe(&entity, |_, _, event, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                }));
        }
        for entity in [app.file_path_input.clone(), app.file_name_input.clone()] {
            app._subscriptions
                .push(cx.subscribe(&entity, |this, _, event, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.pending_file_confirm = true;
                        cx.notify();
                    }
                }));
        }
        for (entity, field) in [
            (app.large_file.from_input.clone(), RangeField::From),
            (app.large_file.to_input.clone(), RangeField::To),
            (app.large_file.size_input.clone(), RangeField::Size),
        ] {
            app._subscriptions
                .push(cx.subscribe(&entity, move |this, _, event, cx| {
                    let normalize =
                        matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. });
                    if normalize || matches!(event, InputEvent::Change) {
                        this.large_file.queue_input(field, normalize);
                        cx.notify();
                    }
                }));
        }
        for (entity, field) in [
            (app.large_file.from_slider.clone(), RangeField::From),
            (app.large_file.to_slider.clone(), RangeField::To),
            (app.large_file.size_slider.clone(), RangeField::Size),
        ] {
            app._subscriptions
                .push(cx.subscribe(&entity, move |this, _, _: &SliderEvent, cx| {
                    this.large_file.queue_slider(field);
                    cx.notify();
                }));
        }

        // Open command-line files first; they take precedence over the session.
        if !options.files.is_empty() {
            let mut opened = 0;
            for path in options.files {
                if app.open_path(&path, window, cx) {
                    opened += 1;
                }
            }
            if opened == 0 {
                // Keep the open error visible while still giving the user an
                // editable window.
                let status = std::mem::take(&mut app.status);
                app.new_file(window, cx);
                app.status = status;
            }
        } else {
            match init {
                WindowInit::Session(window_session) => {
                    app.restore_window_session(*window_session, window, cx);
                }
                WindowInit::Tabs(tabs) => {
                    app.restore_tabs(tabs, window, cx);
                }
                WindowInit::Blank => app.new_file(window, cx),
            }
        }
        app.advance_recovery_prompt();
        if app.auto_recovery && app.recovery_prompt.is_some() {
            app.auto_recovery = false;
            app.start_recovery_restore(window, cx);
        }

        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |this, cx| this.on_should_close(window, cx))
                .unwrap_or(true)
        });

        // Autosave the session on a low-frequency timer. The task exits when
        // the app entity is dropped.
        cx.spawn(async move |this, cx| loop {
            let interval = this
                .update(cx, |app, _| app.settings.session_interval_secs.max(5))
                .unwrap_or(20);
            cx.background_executor()
                .timer(Duration::from_secs(interval))
                .await;
            if this
                .update(cx, |app, cx| {
                    app.autosave_session(cx);
                })
                .is_err()
            {
                break;
            }
        })
        .detach();

        // Poll the watcher even while the window is idle so an external change
        // is noticed without waiting for the next mouse or keyboard event.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            if this
                .update(cx, |app, cx| {
                    app.sync_file_watch(cx);
                })
                .is_err()
            {
                break;
            }
        })
        .detach();

        if app.smoke_test {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let _ = this.update(cx, |app, cx| {
                    app.allow_close = true;
                    cx.quit();
                });
            })
            .detach();
        }

        // A System theme follows live appearance changes; explicit Light/Dark
        // choices ignore them.
        app._subscriptions
            .push(cx.observe_window_appearance(window, |this, window, cx| {
                if matches!(this.settings.theme, ThemeChoice::System) {
                    apply_theme(&this.settings, Some(window), cx);
                    let _ = syntax_theme::apply(&this.settings, cx);
                    cx.notify();
                }
            }));

        app
    }

    // ------------------------------------------------------------------
    // Document / tab plumbing
    // ------------------------------------------------------------------

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    pub fn active_doc(&self) -> Option<&Document> {
        self.tabs.get(self.active).map(|t| &t.doc)
    }

    fn tab_index_by_id(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// Whether per-language indentation applies to this document.
    fn language_indent_applies(doc: &Document, large_file: bool) -> bool {
        !large_file && !doc.is_huge() && !doc.is_non_text()
    }

    /// Indentation for an editor: per-language overrides only apply to
    /// ordinary text documents, never to reduced, non-text or disk-engine views.
    fn editor_tab_size(&self, doc: &Document, large_file: bool) -> TabSize {
        let (tab_size, use_spaces) = if Self::language_indent_applies(doc, large_file) {
            (
                self.settings.tab_size_for(&doc.language),
                self.settings.use_spaces_for(&doc.language),
            )
        } else {
            (self.settings.tab_size, self.settings.use_spaces)
        };
        TabSize {
            tab_size,
            hard_tabs: !use_spaces,
        }
    }

    fn new_editor(
        &self,
        doc: &mut Document,
        large_file: bool,
        huge_window: Option<WindowText>,
        huge_cursor: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (
        Entity<EditorState>,
        RangeDecorationCollection,
        RangeDecorationCollection,
        TextDecorationCollection,
        Subscription,
        Subscription,
    ) {
        // Large documents are mounted by moving the decoded buffer into the
        // editor; the document then releases its own copy. Disk-engine
        // documents mount only their bounded window.
        let (text, window_start) = if let Some(window_text) = huge_window {
            (window_text.text, window_text.start)
        } else if large_file {
            (doc.detach_text().0, 0)
        } else {
            (doc.text.clone(), 0)
        };
        let lang = if large_file || doc.is_huge() {
            "text".to_string()
        } else {
            language_key(&doc.language).to_string()
        };
        // Disk-engine tabs render a document-global gutter in the app, so the
        // editor's window-local gutter stays off.
        let line_numbers = self.settings.show_line_numbers && !doc.is_huge();
        let wrap = !large_file && self.settings.word_wrap;
        let editor_tab = self.editor_tab_size(doc, large_file);
        let tab_size = editor_tab.tab_size;
        let use_spaces = !editor_tab.hard_tabs;

        let editor = {
            let window_ref = &mut *window;
            cx.new(move |cx| {
                EditorState::new(window_ref, cx)
                    .default_value(text)
                    .language(lang)
                    .line_number(line_numbers)
                    .soft_wrap(wrap)
            })
        };
        editor.update(cx, |state, cx| {
            state.set_tab_size(
                TabSize {
                    tab_size,
                    hard_tabs: !use_spaces,
                },
                cx,
            );
            // The application owns Ctrl+F / Ctrl+H and the find UI.
            state.set_searchable(false, cx);
            let plain = !large_file && !doc.is_huge() && !doc.is_non_text();
            state.set_show_whitespaces(plain && self.settings.show_whitespace, window, cx);
            state.set_indent_guides(plain && self.settings.show_indent_guides, window, cx);
        });
        let decorations = editor.update(cx, |state, cx| {
            state.create_range_decorations_collection(Vec::new(), cx)
        });
        let bookmark_decorations = editor.update(cx, |state, cx| {
            state.create_range_decorations_collection(Vec::new(), cx)
        });
        let bracket_decorations = editor.update(cx, |state, cx| {
            state.create_decorations_collection(Vec::new(), cx)
        });

        if doc.is_huge() {
            let start = huge_cursor
                .unwrap_or(window_start)
                .saturating_sub(window_start);
            editor.update(cx, |state, cx| {
                let total = state.text().len();
                let start = usize::try_from(start).unwrap_or(total).min(total);
                state.set_selected_range(start..start, cx);
                if doc.read_only {
                    state.set_readonly(true, cx);
                }
            });
        } else {
            let (anchor, cursor) = (doc.anchor, doc.cursor);
            editor.update(cx, |state, cx| {
                let total = state.text().len_chars();
                let b0 = state.text().char_to_byte_idx(anchor.min(total));
                let b1 = state.text().char_to_byte_idx(cursor.min(total));
                let (start, end) = if b0 <= b1 { (b0, b1) } else { (b1, b0) };
                state.set_selected_range(start..end, cx);
            });
        }
        if self.focus_editor {
            editor.update(cx, |state, cx| state.focus(window, cx));
        }

        let subscription = cx.subscribe(&editor, |this, editor, event, cx| {
            if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.editor == editor) {
                if matches!(event, InputEvent::Change) {
                    tab.needs_sync = true;
                    tab.pending_edit = true;
                    // Large documents never run the whole-buffer diff; the
                    // editor event is enough to track the modified state.
                    if tab.large_file {
                        tab.doc.touch();
                    }
                }
            }
            cx.notify();
        });

        // Wheel and keyboard scrolling only notify the editor entity. A huge
        // tab must re-render the app so the file-global scrollbar and gutter
        // are recomputed from the fresh scroll offset.
        let notification = cx.observe(&editor, |this, editor, cx| {
            let follows_editor = this.tabs.get(this.active).is_some_and(|tab| {
                tab.editor == editor
                    && (tab.huge.is_some()
                        || this.settings.show_document_map
                        || (this.settings.show_ruler && !this.settings.word_wrap))
            });
            if follows_editor {
                cx.notify();
            }
        });

        (
            editor,
            decorations,
            bookmark_decorations,
            bracket_decorations,
            subscription,
            notification,
        )
    }

    fn add_document(&mut self, doc: Document, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let huge = doc.is_huge();
        let large_file = huge
            || (doc.path.is_some() && doc.file_size >= self.settings.large_file_threshold as u64);
        let mut doc = doc;
        let initial = doc.take_initial_window();
        let huge_ui = initial
            .as_ref()
            .map(|window| HugeFileUi::new(window.clone()));
        let huge_cursor = doc.huge().map(|huge| huge.cursor);
        let (
            editor,
            decorations,
            bookmark_decorations,
            bracket_decorations,
            subscription,
            notification,
        ) = self.new_editor(&mut doc, large_file, initial, huge_cursor, window, cx);
        self.tabs.push(Tab {
            id,
            doc,
            editor,
            decorations,
            bookmark_decorations,
            bracket_decorations,
            group: self.active_group,
            bookmarks: BookmarkSet::default(),
            bookmark_revision: 0,
            fold_generation: 0,
            needs_sync: false,
            pending_edit: false,
            word_count: None,
            huge: huge_ui,
            highlighted_revision: u64::MAX,
            external: None,
            large_file,
            decorations_key: None,
            bracket_key: None,
            _subscription: subscription,
            _notification: notification,
        });
        self.active = self.tabs.len() - 1;
        self.active_in_group[self.active_group] = Some(id);
        self.focus_editor = true;
        self.status.clear();
        self.find.clear();
        self.apply_fold_candidates(self.active, cx);
        self.schedule_fold_candidates(self.active, window, cx);
        self.refresh_bookmark_decorations(self.active, cx);
        self.record_navigation(cx);
        cx.notify();
    }

    fn restore_tab_bookmarks(&mut self, bookmarks: Vec<usize>, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.len().checked_sub(1) else {
            return;
        };
        {
            let Some(tab) = self.tabs.get_mut(index) else {
                return;
            };
            if tab.doc.is_huge() || tab.doc.is_non_text() {
                return;
            }
            let text = if tab.large_file {
                tab.editor.read(cx).text().to_string()
            } else {
                tab.doc.text.clone()
            };
            let count = textutil::LineIndex::new(&text).len();
            tab.bookmarks = BookmarkSet::from_lines(bookmarks);
            tab.bookmarks.clamp(count);
            tab.bookmark_revision = tab.bookmark_revision.wrapping_add(1);
        }
        self.refresh_bookmark_decorations(index, cx);
        cx.notify();
    }

    fn apply_fold_candidates(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.doc.is_huge()
            || tab.large_file
            || tab.doc.is_non_text()
            || tab.doc.partial_range().is_some()
        {
            return;
        }
        let language = tab.doc.language.as_str();
        if !matches!(
            language,
            "Python" | "YAML" | "Makefile" | "Ruby" | "Haskell" | "TOML"
        ) {
            return;
        }
        if tab.doc.text.len() > 8 * 1024 * 1024 {
            return;
        }
        let candidates = textutil::indent_fold_candidates(&tab.doc.text, self.settings.tab_size);
        let ranges: Vec<FoldRange> = candidates
            .into_iter()
            .map(|(start, end)| FoldRange::new(start, end))
            .collect();
        let editor = tab.editor.clone();
        editor.update(cx, |state, cx| {
            state.apply_highlighter_fold_candidates(ranges, cx);
        });
    }

    fn schedule_fold_candidates(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        tab.fold_generation = tab.fold_generation.wrapping_add(1);
        let generation = tab.fold_generation;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(260))
                .await;
            let _ = this.update_in(cx, |app, _window, cx| {
                let current = app
                    .tabs
                    .get(index)
                    .map(|tab| tab.fold_generation)
                    .unwrap_or_default();
                if current == generation {
                    app.apply_fold_candidates(index, cx);
                }
            });
        })
        .detach();
    }

    pub fn new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.untitled_counter += 1;
        let mut doc = Document::new_untitled(tr_args(
            "untitled.name",
            &[("number", self.untitled_counter.to_string())],
        ));
        doc.encoding = self.settings.default_encoding;
        doc.eol = self.settings.eol_for_new_document(language::PLAIN_TEXT);
        self.add_document(doc, window, cx);
    }

    pub fn open_path(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let path = util::absolutize(path);
        if let Some(index) = self.tabs.iter().position(|t| {
            t.doc
                .path
                .as_ref()
                .is_some_and(|p| util::same_path(p, &path))
        }) {
            self.record_navigation(cx);
            self.active = index;
            self.active_group = self.tabs[index].group;
            self.active_in_group[self.active_group] = Some(self.tabs[index].id);
            self.focus_editor = true;
            self.find.dirty = true;
            self.record_navigation(cx);
            cx.notify();
            return true;
        }
        if self
            .pending_opens
            .iter()
            .any(|pending| util::same_path(pending, &path))
        {
            self.set_status(
                tr_args("status.opening", &[("path", path.display().to_string())]),
                cx,
            );
            return true;
        }
        let max_size = self.settings.max_file_size_bytes();
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) => {
                let err = luban_core::Error::Io(err);
                self.set_status(
                    tr_args(
                        "status.open_failed",
                        &[
                            ("path", path.display().to_string()),
                            ("error", err.to_string()),
                        ],
                    ),
                    cx,
                );
                return false;
            }
        };
        if metadata.is_dir() {
            let err = luban_core::Error::InvalidPath(path.clone());
            self.set_status(
                tr_args(
                    "status.open_failed",
                    &[
                        ("path", path.display().to_string()),
                        ("error", err.to_string()),
                    ],
                ),
                cx,
            );
            return false;
        }
        let size = metadata.len();
        let controller = self.settings.large_file_controller
            && size >= self.settings.large_file_controller_threshold_bytes();
        if size > max_size {
            let probe = match DiskEngine::probe(&path) {
                Ok(probe) => probe,
                Err(err) => {
                    self.set_status(
                        tr_args(
                            "status.open_failed",
                            &[
                                ("path", path.display().to_string()),
                                ("error", err.to_string()),
                            ],
                        ),
                        cx,
                    );
                    return false;
                }
            };
            if !probe.non_text {
                if controller {
                    self.start_controller_open(path, size, true, window, cx);
                } else {
                    self.start_huge_open(path, 1, None, false, window, cx);
                }
                return true;
            }
            if controller {
                self.start_controller_open(path, size, false, window, cx);
                return true;
            }
            let err = luban_core::Error::FileTooLarge {
                size,
                limit: max_size,
            };
            self.set_status(
                tr_args(
                    "status.open_failed",
                    &[
                        ("path", path.display().to_string()),
                        ("error", err.to_string()),
                    ],
                ),
                cx,
            );
            return false;
        }
        if controller {
            self.start_controller_open(path, size, false, window, cx);
            return true;
        }
        if metadata.len() >= self.settings.large_file_threshold as u64 {
            self.start_background_open(path, max_size, window, cx);
            return true;
        }
        match Document::open_with_limit(&path, max_size) {
            Ok(doc) => {
                let non_text = doc.is_non_text();
                self.settings.add_recent(path.clone());
                self.commit_settings();
                self.add_document(doc, window, cx);
                let status = if non_text {
                    tr_args(
                        "status.opened_non_text",
                        &[("path", path.display().to_string())],
                    )
                } else {
                    tr_args("status.opened", &[("path", path.display().to_string())])
                };
                self.set_status(status, cx);
                true
            }
            Err(err) => {
                self.set_status(
                    tr_args(
                        "status.open_failed",
                        &[
                            ("path", path.display().to_string()),
                            ("error", err.to_string()),
                        ],
                    ),
                    cx,
                );
                false
            }
        }
    }

    /// Open files dropped from the desktop onto the window. Dropping a folder
    /// is not "Open Folder": replacing the workspace root as a side effect of
    /// a stray drop would be surprising, so directories are ignored.
    fn open_dropped_paths(
        &mut self,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let files: Vec<&PathBuf> = paths.iter().filter(|path| !path.is_dir()).collect();
        if files.is_empty() {
            self.set_status(tr("status.drop_no_files"), cx);
            return;
        }
        let total = files.len();
        let mut opened = 0;
        let mut failed = 0;
        for path in files {
            if self.open_path(path, window, cx) {
                opened += 1;
            } else {
                failed += 1;
            }
        }
        if total > 1 {
            if failed == 0 {
                self.set_status(
                    tr_args("status.batch_opened", &[("count", opened.to_string())]),
                    cx,
                );
            } else {
                self.set_status(
                    tr_args(
                        "status.batch_open_partial",
                        &[
                            ("opened", opened.to_string()),
                            ("failed", failed.to_string()),
                        ],
                    ),
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// Read and decode a large file on the background executor, then mount it
    /// on the main thread. The window stays interactive while this runs.
    fn start_background_open(
        &mut self,
        path: PathBuf,
        max_size: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_opens.push(path.clone());
        self.set_status(
            tr_args("status.opening", &[("path", path.display().to_string())]),
            cx,
        );
        let task_path = path.clone();
        let task = cx
            .background_executor()
            .spawn(async move { Document::open_with_limit(&task_path, max_size) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_background_open(path, result, window, cx);
            });
        })
        .detach();
    }

    fn finish_background_open(
        &mut self,
        path: PathBuf,
        result: luban_core::Result<Document>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_opens
            .retain(|pending| !util::same_path(pending, &path));
        match result {
            Ok(doc) => {
                let non_text = doc.is_non_text();
                self.settings.add_recent(path.clone());
                self.commit_settings();
                self.add_document(doc, window, cx);
                let status = if non_text {
                    tr_args(
                        "status.opened_non_text",
                        &[("path", path.display().to_string())],
                    )
                } else {
                    tr_args("status.opened", &[("path", path.display().to_string())])
                };
                self.set_status(status, cx);
                let pending_at = self
                    .pending_open_at
                    .as_ref()
                    .is_some_and(|request| util::same_path(&request.path, &path));
                if pending_at {
                    if let Some(request) = self.pending_open_at.take() {
                        let index = self.active;
                        let line_count = self
                            .tabs
                            .get(index)
                            .map(|tab| tab.editor.read(cx).text().lines_len())
                            .unwrap_or(0);
                        if request.line > line_count {
                            self.set_status(tr("status.line_outside_partial"), cx);
                        } else {
                            let (start, end) = self.line_selection_in_tab(
                                index,
                                request.line,
                                request.column,
                                request.match_len,
                                cx,
                            );
                            self.select_char_range(start, end, window, cx);
                        }
                    }
                }
            }
            Err(err) => {
                if self
                    .pending_open_at
                    .as_ref()
                    .is_some_and(|request| util::same_path(&request.path, &path))
                {
                    self.pending_open_at = None;
                }
                let status = tr_args(
                    "status.open_failed",
                    &[
                        ("path", path.display().to_string()),
                        ("error", err.to_string()),
                    ],
                );
                if self.tabs.is_empty() {
                    self.new_file(window, cx);
                }
                self.set_status(status, cx);
            }
        }
        cx.notify();
    }

    /// Start a controller-driven open. Files within the size limit load whole;
    /// larger files load the beginning portion by default.
    fn start_controller_open(
        &mut self,
        path: PathBuf,
        size: u64,
        text: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let max_size = self.settings.max_file_size_bytes();
        let default_size = self.settings.large_file_partial_size_bytes();
        let engine = size > max_size && text;
        let whole = size <= max_size || engine;
        self.large_file.open_for(
            crate::large_file::PanelBinding {
                path: path.clone(),
                size,
                tab_id: None,
                default_size,
                unit: self.settings.large_file_display_unit,
                loaded: 0,
                whole,
                engine,
                partial: false,
            },
            window,
            cx,
        );
        if engine {
            self.start_huge_open(path, 1, None, true, window, cx);
            return;
        }
        let range = if whole {
            None
        } else {
            Some((0, default_size.min(size)))
        };
        self.start_controller_load(path, range, None, window, cx);
    }

    /// Index a large text file in the background and mount its first window.
    /// `target_line` positions the first window; `controller` keeps the
    /// large-file panel's progress and stop button wired up.
    fn start_huge_open(
        &mut self,
        path: PathBuf,
        target_line: u64,
        replace_tab: Option<u64>,
        controller: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_huge_job(path, target_line, None, replace_tab, controller, window, cx);
    }

    /// Index only the line-aligned `from..to` range of a text file and mount a
    /// windowed view of that range.
    #[allow(clippy::too_many_arguments)]
    fn start_huge_range_open(
        &mut self,
        path: PathBuf,
        from: u64,
        to: u64,
        target_line: u64,
        replace_tab: Option<u64>,
        controller: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_huge_job(
            path,
            target_line,
            Some((from, to)),
            replace_tab,
            controller,
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn start_huge_job(
        &mut self,
        path: PathBuf,
        target_line: u64,
        range: Option<(u64, u64)>,
        replace_tab: Option<u64>,
        controller: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_controller_load_for_path(&path);
        let generation = self.next_load_generation;
        self.next_load_generation = self.next_load_generation.wrapping_add(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let size = match range {
            Some((from, to)) => to.saturating_sub(from),
            None => fs::metadata(&path)
                .map(|metadata| metadata.len())
                .unwrap_or(0),
        };
        self.large_loads.push(LargeLoadJob {
            path: path.clone(),
            generation,
            cancel: cancel.clone(),
            progress: progress.clone(),
            size,
            replace_tab,
            huge: true,
        });
        self.pending_opens.push(path.clone());
        if controller {
            self.large_file
                .begin_loading(progress.clone(), cancel.clone(), cx);
        }
        self.set_status(
            tr_args("status.opening", &[("path", path.display().to_string())]),
            cx,
        );

        let task_path = path.clone();
        let task_cancel = cancel.clone();
        let task_progress = progress.clone();
        let task = cx.background_executor().spawn(async move {
            match range {
                Some((from, to)) => Document::open_huge_range(
                    &task_path,
                    from,
                    to,
                    target_line,
                    &task_cancel,
                    &task_progress,
                ),
                None => Document::open_huge(&task_path, target_line, &task_cancel, &task_progress),
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_huge_open(generation, path, result, window, cx);
            });
        })
        .detach();

        // Keep the status bar and controller progress moving while indexing.
        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let keep = this
                .update(cx, |app, cx| {
                    let Some(job) = app
                        .large_loads
                        .iter()
                        .find(|job| job.generation == generation)
                    else {
                        return false;
                    };
                    let total = job.size.max(1);
                    let percent =
                        (job.progress.load(Ordering::Relaxed).saturating_mul(100) / total).min(100);
                    app.status =
                        tr_args("status.huge_indexing", &[("percent", percent.to_string())]);
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        })
        .detach();
    }

    fn finish_huge_open(
        &mut self,
        generation: u64,
        path: PathBuf,
        result: luban_core::Result<Document>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .large_loads
            .iter()
            .position(|job| job.generation == generation)
        else {
            return;
        };
        let job = self.large_loads.remove(index);
        let has_newer = self
            .large_loads
            .iter()
            .any(|candidate| util::same_path(&candidate.path, &path));
        if !has_newer {
            self.pending_opens
                .retain(|pending| !util::same_path(pending, &path));
        }
        let is_current_target = self
            .large_file
            .target
            .as_ref()
            .is_some_and(|target| util::same_path(&target.path, &path));
        let result = if job.cancel.load(Ordering::Relaxed) {
            Err(luban_core::Error::Cancelled)
        } else {
            result
        };

        match result {
            Ok(doc) => {
                if let Some(tab_id) = job.replace_tab {
                    if let Some(index) = self.tab_index_by_id(tab_id) {
                        if self.tabs[index].doc.is_modified() {
                            if is_current_target {
                                let loaded = self.large_file.loaded;
                                self.large_file.error = Some(tr("lfc.error.target_modified"));
                                self.large_file.finish_loading(loaded);
                            }
                            self.set_status(tr("lfc.error.target_modified"), cx);
                            if self.tabs.is_empty() {
                                self.new_file(window, cx);
                            }
                            cx.notify();
                            return;
                        }
                        self.tabs.remove(index);
                        if self.tabs.is_empty() {
                            self.active = 0;
                        } else if self.active >= self.tabs.len() {
                            self.active = self.tabs.len() - 1;
                        } else if self.active > index {
                            self.active -= 1;
                        }
                    }
                }
                let size = doc.file_size;
                let partial = doc.partial_range();
                let loaded = partial
                    .map(|range| range.end.saturating_sub(range.start))
                    .unwrap_or(size);
                self.settings.add_recent(path.clone());
                self.commit_settings();
                self.add_document(doc, window, cx);
                let tab_id = self.active_tab().map(|tab| tab.id);
                if is_current_target {
                    self.large_file.clear_pin();
                    self.large_file.bound_tab = tab_id;
                    self.large_file.available_space =
                        crate::large_file::available_space(&std::env::temp_dir());
                    if let Some(target) = &mut self.large_file.target {
                        target.size = size;
                        target.tab_id = tab_id;
                        target.partial = partial.is_some();
                    }
                    if let Some(range) = partial {
                        // Show the line-aligned range that was actually
                        // indexed, not the raw field values.
                        let mode = self.large_file.selection.mode();
                        let mut selection = luban_core::large_file::RangeSelection::new(
                            size,
                            self.settings.large_file_partial_size_bytes(),
                        );
                        selection.set_mode(mode);
                        let _ = selection.set_range(range.start, range.end);
                        self.large_file.set_selection(selection, size, window, cx);
                    }
                    self.large_file.finish_loading(loaded);
                }
                self.set_status(
                    tr_args(
                        "status.opened_huge",
                        &[("path", path.display().to_string())],
                    ),
                    cx,
                );
                if self
                    .pending_huge_at
                    .as_ref()
                    .is_some_and(|(pending, _, _)| util::same_path(pending, &path))
                {
                    if let Some((_, line, column)) = self.pending_huge_at.take() {
                        self.huge_jump_line(line, column, window, cx);
                    }
                }
            }
            Err(err) => {
                if self
                    .pending_huge_at
                    .as_ref()
                    .is_some_and(|(pending, _, _)| util::same_path(pending, &path))
                {
                    self.pending_huge_at = None;
                }
                let cancelled = matches!(err, luban_core::Error::Cancelled);
                if is_current_target && !has_newer {
                    let loaded = self.large_file.loaded;
                    self.large_file.finish_loading(loaded);
                    if cancelled {
                        self.large_file.status = Some(tr("lfc.stopped"));
                    } else {
                        self.large_file.error = Some(err.to_string());
                    }
                }
                if cancelled {
                    self.set_status(tr("lfc.stopped"), cx);
                } else {
                    self.set_status(
                        tr_args(
                            "status.open_failed",
                            &[
                                ("path", path.display().to_string()),
                                ("error", err.to_string()),
                            ],
                        ),
                        cx,
                    );
                    if self.tabs.is_empty() {
                        self.new_file(window, cx);
                    }
                }
            }
        }
        cx.notify();
    }

    fn start_controller_load(
        &mut self,
        path: PathBuf,
        range: Option<(u64, u64)>,
        replace_tab: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_controller_load_for_path(&path);
        let generation = self.next_load_generation;
        self.next_load_generation = self.next_load_generation.wrapping_add(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        self.large_loads.push(LargeLoadJob {
            path: path.clone(),
            generation,
            cancel: cancel.clone(),
            progress: progress.clone(),
            size: 0,
            replace_tab,
            huge: false,
        });
        self.pending_opens.push(path.clone());
        self.large_file
            .begin_loading(progress.clone(), cancel.clone(), cx);
        self.set_status(
            tr_args("status.opening", &[("path", path.display().to_string())]),
            cx,
        );

        let max_size = self.settings.max_file_size_bytes();
        let task_path = path.clone();
        let task_cancel = cancel.clone();
        let task_progress = progress.clone();
        let task = cx.background_executor().spawn(async move {
            match range {
                Some((from, to)) => Document::open_range(
                    &task_path,
                    from,
                    to,
                    max_size,
                    &task_cancel,
                    &task_progress,
                ),
                None => {
                    Document::open_with_progress(&task_path, max_size, &task_cancel, &task_progress)
                }
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_controller_load(generation, path, result, window, cx);
            });
        })
        .detach();

        // Repaint while the read is in flight so the "opened" byte count moves.
        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            let keep = this
                .update(cx, |app, cx| {
                    if app.large_file.loading {
                        cx.notify();
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        })
        .detach();
    }

    fn cancel_controller_load_for_path(&mut self, path: &Path) {
        for job in &self.large_loads {
            if util::same_path(&job.path, path) {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
        self.large_loads
            .retain(|job| !util::same_path(&job.path, path));
        self.pending_opens
            .retain(|pending| !util::same_path(pending, path));
    }

    fn finish_controller_load(
        &mut self,
        generation: u64,
        path: PathBuf,
        result: luban_core::Result<Document>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .large_loads
            .iter()
            .position(|job| job.generation == generation)
        else {
            // A newer load replaced this one; ignore the stale result.
            return;
        };
        let job = self.large_loads.remove(index);
        let has_newer = self
            .large_loads
            .iter()
            .any(|candidate| util::same_path(&candidate.path, &path));
        if !has_newer {
            self.pending_opens
                .retain(|pending| !util::same_path(pending, &path));
        }
        let is_current_target = self
            .large_file
            .target
            .as_ref()
            .is_some_and(|target| util::same_path(&target.path, &path));
        // A Stop that arrived just before the read completed must still win:
        // discard the finished document instead of mounting it.
        let result = if job.cancel.load(Ordering::Relaxed) {
            Err(luban_core::Error::Cancelled)
        } else {
            result
        };

        match result {
            Ok(doc) => {
                if let Some(tab_id) = job.replace_tab {
                    if let Some(index) = self.tab_index_by_id(tab_id) {
                        if self.tabs[index].doc.is_modified() {
                            if is_current_target {
                                let loaded = self.large_file.loaded;
                                self.large_file.error = Some(tr("lfc.error.target_modified"));
                                self.large_file.finish_loading(loaded);
                            }
                            self.set_status(tr("lfc.error.target_modified"), cx);
                            if self.tabs.is_empty() {
                                self.new_file(window, cx);
                            }
                            cx.notify();
                            return;
                        }
                        self.tabs.remove(index);
                        if self.tabs.is_empty() {
                            self.active = 0;
                        } else if self.active >= self.tabs.len() {
                            self.active = self.tabs.len() - 1;
                        } else if self.active > index {
                            self.active -= 1;
                        }
                    }
                }
                let non_text = doc.is_non_text();
                let total = doc.file_size;
                let partial = doc.partial_range();
                self.settings.add_recent(path.clone());
                self.commit_settings();
                self.add_document(doc, window, cx);
                let tab_id = self.active_tab().map(|tab| tab.id);
                if is_current_target {
                    self.large_file.clear_pin();
                    self.large_file.bound_tab = tab_id;
                    let loaded = partial
                        .map(|range| range.end.saturating_sub(range.start))
                        .unwrap_or(total);
                    self.large_file.finish_loading(loaded);
                    self.large_file.available_space =
                        crate::large_file::available_space(&std::env::temp_dir());
                    if let Some(target) = &mut self.large_file.target {
                        target.size = total;
                        target.tab_id = tab_id;
                    }
                }
                let status = if let Some(range) = partial {
                    let unit = self.settings.large_file_display_unit;
                    tr_args(
                        "status.partial_opened",
                        &[
                            ("path", path.display().to_string()),
                            (
                                "loaded",
                                format_bytes(range.end.saturating_sub(range.start), unit),
                            ),
                            ("total", format_bytes(total, unit)),
                            ("start", format_bytes(range.start, unit)),
                        ],
                    )
                } else if non_text {
                    tr_args(
                        "status.opened_non_text",
                        &[("path", path.display().to_string())],
                    )
                } else {
                    tr_args("status.opened", &[("path", path.display().to_string())])
                };
                self.set_status(status, cx);

                let pending_at = self
                    .pending_open_at
                    .as_ref()
                    .is_some_and(|request| util::same_path(&request.path, &path));
                if pending_at {
                    if let Some(request) = self.pending_open_at.take() {
                        let index = self.active;
                        let line_count = self
                            .tabs
                            .get(index)
                            .map(|tab| tab.editor.read(cx).text().lines_len())
                            .unwrap_or(0);
                        if request.line > line_count {
                            self.set_status(tr("status.line_outside_partial"), cx);
                        } else {
                            let (start, end) = self.line_selection_in_tab(
                                index,
                                request.line,
                                request.column,
                                request.match_len,
                                cx,
                            );
                            self.select_char_range(start, end, window, cx);
                        }
                    }
                }
            }
            Err(err) => {
                if self
                    .pending_huge_at
                    .as_ref()
                    .is_some_and(|(pending, _, _)| util::same_path(pending, &path))
                {
                    self.pending_huge_at = None;
                }
                let cancelled = matches!(err, luban_core::Error::Cancelled);
                if is_current_target && !has_newer {
                    let loaded = self.large_file.loaded;
                    self.large_file.finish_loading(loaded);
                    if cancelled {
                        self.large_file.status = Some(tr("lfc.stopped"));
                    } else {
                        self.large_file.error = Some(err.to_string());
                    }
                }
                if cancelled {
                    self.set_status(tr("lfc.stopped"), cx);
                } else {
                    let status = tr_args(
                        "status.open_failed",
                        &[
                            ("path", path.display().to_string()),
                            ("error", err.to_string()),
                        ],
                    );
                    self.set_status(status, cx);
                    if self.tabs.is_empty() {
                        self.new_file(window, cx);
                    }
                }
                if cancelled
                    && is_current_target
                    && !has_newer
                    && !self.tabs.iter().any(|tab| {
                        tab.doc
                            .path
                            .as_ref()
                            .is_some_and(|open| util::same_path(open, &path))
                    })
                {
                    let active = self.tabs.get(self.active).map(|tab| tab.id);
                    self.large_file.pin_to_tab(active);
                }
            }
        }
        cx.notify();
    }

    /// Apply the controller's current range, replacing an unmodified tab for
    /// the same file when one exists.
    pub(crate) fn apply_large_file_range(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.large_file.target.clone() else {
            self.large_file.error = Some(tr("lfc.error.no_target"));
            cx.notify();
            return;
        };
        let raw_range = match self.large_file.current_range(cx) {
            Ok(range) => range,
            Err(message) => {
                self.large_file.error = Some(message);
                cx.notify();
                return;
            }
        };
        let whole = raw_range.is_none();
        let max_size = self.settings.max_file_size_bytes();
        let default_size = self.settings.large_file_partial_size_bytes();

        let metadata = match fs::metadata(&target.path) {
            Ok(metadata) => metadata,
            Err(err) => {
                self.large_file.error = Some(err.to_string());
                cx.notify();
                return;
            }
        };
        if metadata.is_dir() {
            self.large_file.error =
                Some(luban_core::Error::InvalidPath(target.path.clone()).to_string());
            cx.notify();
            return;
        }
        let size = metadata.len();
        if size == 0 {
            self.large_file.error = Some(tr("lfc.error.empty_file"));
            cx.notify();
            return;
        }
        let text = DiskEngine::probe(&target.path)
            .map(|probe| !probe.non_text)
            .unwrap_or(false);
        if whole && size > max_size && text {
            let mut replace_tab = None;
            if let Some(index) = self.tabs.iter().position(|tab| {
                tab.doc
                    .path
                    .as_ref()
                    .is_some_and(|path| util::same_path(path, &target.path))
            }) {
                if self.tabs[index].doc.is_modified() {
                    self.large_file.error = Some(tr("lfc.error.target_modified"));
                    cx.notify();
                    return;
                }
                replace_tab = Some(self.tabs[index].id);
            }
            let selection = luban_core::large_file::RangeSelection::new(size, default_size);
            self.large_file.set_selection(selection, size, window, cx);
            self.large_file.error = None;
            self.start_huge_open(target.path, 1, replace_tab, true, window, cx);
            return;
        }
        if whole && size > max_size {
            self.large_file.error = Some(tr_args(
                "lfc.error.whole_too_large",
                &[
                    ("size", format_bytes(size, SizeUnit::Bytes)),
                    ("limit", format_bytes(max_size, SizeUnit::Bytes)),
                ],
            ));
            cx.notify();
            return;
        }

        let (from, to) = match raw_range {
            Some((from, to)) => {
                let from = from.min(size - 1);
                let to = to.min(size).max(from + 1);
                (from, to)
            }
            None => (0, size),
        };
        if !whole {
            if from >= to {
                self.large_file.error = Some(tr("lfc.error.start_end"));
                cx.notify();
                return;
            }
            if text {
                // Text partial opens are engine range views and are not capped
                // by max_file_size_mb: the engine indexes only this range.
                let mut replace_tab = None;
                if let Some(index) = self.tabs.iter().position(|tab| {
                    tab.doc
                        .path
                        .as_ref()
                        .is_some_and(|path| util::same_path(path, &target.path))
                }) {
                    if self.tabs[index].doc.is_modified() {
                        self.large_file.error = Some(tr("lfc.error.target_modified"));
                        cx.notify();
                        return;
                    }
                    replace_tab = Some(self.tabs[index].id);
                }
                let mode = self.large_file.selection.mode();
                let mut selection = luban_core::large_file::RangeSelection::new(size, default_size);
                selection.set_mode(mode);
                let _ = selection.set_range(from, to);
                self.large_file.set_selection(selection, size, window, cx);
                self.large_file.error = None;
                self.start_huge_range_open(target.path, from, to, 1, replace_tab, true, window, cx);
                return;
            }
            if to - from > max_size {
                self.large_file.error = Some(
                    luban_core::Error::FileTooLarge {
                        size: to - from,
                        limit: max_size,
                    }
                    .to_string(),
                );
                cx.notify();
                return;
            }
        }
        let mode = self.large_file.selection.mode();
        let mut selection = luban_core::large_file::RangeSelection::new(size, default_size);
        selection.set_mode(if whole { RangeMode::Whole } else { mode });
        if !whole {
            let _ = selection.set_range(from, to);
        }

        let mut replace_tab = None;
        if let Some(index) = self.tabs.iter().position(|tab| {
            tab.doc
                .path
                .as_ref()
                .is_some_and(|path| util::same_path(path, &target.path))
        }) {
            if self.tabs[index].doc.is_modified() {
                self.large_file.error = Some(tr("lfc.error.target_modified"));
                cx.notify();
                return;
            }
            replace_tab = Some(self.tabs[index].id);
        }

        self.large_file
            .set_selection(selection.clone(), size, window, cx);
        self.large_file.error = None;
        let range = if whole {
            None
        } else {
            Some((selection.from(), selection.to()))
        };
        self.start_controller_load(target.path, range, replace_tab, window, cx);
    }

    pub fn toggle_large_file_controller(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.large_file.open {
            self.large_file.open = false;
            self.large_file.bound_tab = None;
        } else {
            self.large_file.open = true;
            self.large_file.bound_tab = None;
            self.sync_large_file_panel(window, cx);
        }
        cx.notify();
    }

    /// Keep the controller pointed at the active tab when it is not loading.
    fn sync_large_file_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.large_file.open || self.large_file.loading {
            return;
        }
        let active_tab_id = self.tabs.get(self.active).map(|tab| tab.id);
        if self.large_file.should_hold_target(active_tab_id) {
            return;
        }
        self.large_file.clear_pin();
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let tab_id = tab.id;
        let size = tab.doc.source_size();
        let same_target = self.large_file.bound_tab == Some(tab_id)
            && self.large_file.target.as_ref().is_some_and(|target| {
                target.size == size && target.partial == tab.doc.partial_range().is_some()
            })
            && tab
                .doc
                .path
                .as_ref()
                .zip(self.large_file.target.as_ref().map(|target| &target.path))
                .is_some_and(|(open, bound)| util::same_path(open, bound));
        if same_target {
            return;
        }
        let Some(path) = tab.doc.path.clone() else {
            self.large_file.clear_target(Some(tab_id), window, cx);
            return;
        };
        let huge = tab.doc.is_huge();
        let partial = tab.doc.partial_range();
        let loaded = partial
            .map(|range| range.end.saturating_sub(range.start))
            .unwrap_or(size);
        self.large_file.bind(
            crate::large_file::PanelBinding {
                path,
                size,
                tab_id: Some(tab_id),
                default_size: self.settings.large_file_partial_size_bytes(),
                unit: self.settings.large_file_display_unit,
                loaded,
                whole: if huge {
                    partial.is_none()
                } else {
                    size <= self.settings.max_file_size_bytes()
                },
                engine: huge,
                partial: partial.is_some(),
            },
            window,
            cx,
        );
        if let Some(range) = partial {
            let mode = self.large_file.selection.mode();
            let mut selection = luban_core::large_file::RangeSelection::new(
                size,
                self.settings.large_file_partial_size_bytes(),
            );
            selection.set_mode(mode);
            let _ = selection.set_range(range.start, range.end);
            self.large_file.set_selection(selection, size, window, cx);
        }
    }

    /// Character range for a 1-based line/column and match length, read from
    /// the editor rope so no line index or full-text scan is needed.
    fn line_selection_in_tab(
        &self,
        index: usize,
        line: usize,
        column: usize,
        match_len: usize,
        cx: &gpui_kit::App,
    ) -> (usize, usize) {
        let Some(tab) = self.tabs.get(index) else {
            return (0, 0);
        };
        let state = tab.editor.read(cx);
        let rope = state.text();
        let rows = rope.lines_len();
        let row = line.saturating_sub(1).min(rows.saturating_sub(1));
        let line_start = rope.line_start_offset(row);
        let line_slice = rope.slice_line(row);
        let column = column.saturating_sub(1).min(line_slice.len_chars());
        let byte = line_start + line_slice.char_to_byte_idx(column);
        let start = rope.byte_to_char_idx(byte.min(rope.len()));
        let end = (start + match_len).min(rope.len_chars());
        (start, end)
    }

    pub fn open_path_at(
        &mut self,
        path: &Path,
        line: usize,
        column: usize,
        match_len: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = util::absolutize(path);
        if let Some(index) = self.tabs.iter().position(|t| {
            t.doc
                .path
                .as_ref()
                .is_some_and(|open| util::same_path(open, &path))
        }) {
            self.record_navigation(cx);
            self.active = index;
            self.active_group = self.tabs[index].group;
            self.active_in_group[self.active_group] = Some(self.tabs[index].id);
            if self.tabs[index].doc.is_huge() {
                if self.tabs[index].doc.is_partial() {
                    // Range views use local line numbers; a file-global line
                    // cannot be mapped back without scanning the prefix.
                    self.set_status(tr("status.line_outside_partial"), cx);
                    self.focus_editor = true;
                    self.find.dirty = true;
                    cx.notify();
                    return;
                }
                self.huge_jump_line(line as u64, column as u64, window, cx);
                self.focus_editor = true;
                self.find.dirty = true;
                cx.notify();
                return;
            }
            let (start, end) = self.line_selection_in_tab(index, line, column, match_len, cx);
            self.select_char_range(start, end, window, cx);
            self.focus_editor = true;
            self.find.dirty = true;
            cx.notify();
            return;
        }
        let already_open = false;
        let metadata = fs::metadata(&path).ok();
        let size = metadata
            .as_ref()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let text = size > self.settings.max_file_size_bytes()
            && DiskEngine::probe(&path)
                .map(|probe| !probe.non_text)
                .unwrap_or(false);
        if text {
            let controller = self.settings.large_file_controller
                && size >= self.settings.large_file_controller_threshold_bytes();
            self.pending_huge_at = Some((path.clone(), line as u64, column as u64));
            self.start_huge_open(path, line as u64, None, controller, window, cx);
            return;
        }
        let large = metadata
            .map(|metadata| {
                metadata.len() >= self.settings.large_file_threshold as u64
                    || (self.settings.large_file_controller
                        && metadata.len() >= self.settings.large_file_controller_threshold_bytes())
            })
            .unwrap_or(false);
        if !self.open_path(&path, window, cx) {
            return;
        }
        if large && !already_open {
            // The background loader will apply the selection when the tab is
            // mounted.
            self.pending_open_at = Some(PendingOpenAt {
                path,
                line,
                column,
                match_len,
            });
            return;
        }
        let index = self
            .tabs
            .iter()
            .position(|t| {
                t.doc
                    .path
                    .as_ref()
                    .is_some_and(|open| util::same_path(open, &path))
            })
            .unwrap_or(self.active);
        self.active = index;
        let (start, end) = self.line_selection_in_tab(index, line, column, match_len, cx);
        self.select_char_range(start, end, window, cx);
        self.focus_editor = true;
        self.find.dirty = true;
        cx.notify();
    }

    /// Restore one saved window: workspace, roots, split layout and tab groups.
    fn restore_window_session(
        &mut self,
        session: WindowSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.window_width = session.width;
        self.window_height = session.height;
        window.resize(size(px(self.window_width), px(self.window_height)));
        self.set_workspace_environment(
            session.workspace_file.clone(),
            session.workspace_roots.clone(),
            window,
            cx,
        );
        self.rebuild_index(cx);
        self.split = match session.split {
            SplitLayout::Right => SplitDirection::Right,
            SplitLayout::Down => SplitDirection::Down,
            SplitLayout::None => SplitDirection::None,
        };
        self.active_group = session
            .active_group
            .min(session.groups.len().saturating_sub(1));
        for (index, group) in session.groups.iter().take(2).enumerate() {
            self.restore_group_active[index] = group.active_tab;
        }
        let specs = self.session_specs(session);
        if specs.is_empty() {
            self.new_file(window, cx);
            return;
        }
        self.begin_restore(specs, window, cx);
    }

    /// Restore tabs moved into a new window. Every tab starts in group 0.
    fn restore_tabs(&mut self, tabs: Vec<SessionTab>, window: &mut Window, cx: &mut Context<Self>) {
        let group_count = self.active_group + 1;
        let groups = (0..group_count)
            .map(|index| GroupSession {
                active_tab: if index == self.active_group {
                    tabs.len().saturating_sub(1)
                } else {
                    0
                },
                tabs: if index == self.active_group {
                    tabs.clone()
                } else {
                    Vec::new()
                },
            })
            .collect();
        let session = WindowSession {
            id: self.window_id,
            width: self.window_width,
            height: self.window_height,
            split: match self.split {
                SplitDirection::Right => SplitLayout::Right,
                SplitDirection::Down => SplitLayout::Down,
                SplitDirection::None => SplitLayout::None,
            },
            active_group: self.active_group,
            groups,
            workspace_file: self.workspace_file.clone(),
            workspace_roots: self.workspace_roots.clone(),
            ..WindowSession::default()
        };
        let specs = self.session_specs(session);
        if specs.is_empty() {
            self.new_file(window, cx);
            return;
        }
        self.begin_restore(specs, window, cx);
    }

    /// Convert session groups into flat restore specs with group membership.
    fn session_specs(&mut self, session: WindowSession) -> Vec<RestoreSpec> {
        let pending_recovery_ids: HashSet<&str> = if self.auto_recovery {
            HashSet::new()
        } else {
            self.recovery_queue
                .iter()
                .chain(self.recovery_prompt.iter().map(|prompt| &prompt.info))
                .map(|info| info.id.as_str())
                .collect()
        };
        let mut specs = Vec::new();
        for (group, group_session) in session.groups.iter().enumerate() {
            for tab in &group_session.tabs {
                if tab
                    .recovery_id
                    .as_deref()
                    .is_some_and(|id| pending_recovery_ids.contains(id))
                {
                    continue;
                }
                let fallback_name = if let Some(path) = &tab.path {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string())
                } else {
                    self.untitled_counter += 1;
                    tr_args(
                        "untitled.name",
                        &[("number", self.untitled_counter.to_string())],
                    )
                };
                specs.push(RestoreSpec {
                    path: tab.path.clone(),
                    text: tab.text.clone(),
                    encoding: tab.encoding,
                    eol: tab.eol,
                    language: tab.language.clone(),
                    cursor: tab.cursor,
                    anchor: tab.anchor,
                    modified: tab.modified,
                    fallback_name,
                    range: tab.range,
                    huge: tab.huge,
                    huge_line: tab.huge_line,
                    huge_column: tab.huge_column,
                    bookmarks: tab.bookmarks.clone(),
                    group,
                });
            }
        }
        specs
    }

    fn begin_restore(
        &mut self,
        specs: Vec<RestoreSpec>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let max_size = self.settings.max_file_size_bytes();
        let any_large = specs.iter().any(|spec| {
            spec.huge
                || spec.path.as_ref().is_some_and(|path| {
                    fs::metadata(path)
                        .map(|metadata| metadata.len() >= self.settings.large_file_threshold as u64)
                        .unwrap_or(false)
                })
        });
        if !any_large {
            for spec in specs {
                let group = spec.group;
                let bookmarks = spec.bookmarks.clone();
                let doc = spec.load(max_size);
                self.add_document(doc, window, cx);
                let group_limit = self.group_count().saturating_sub(1);
                if let Some(tab) = self.tabs.last_mut() {
                    tab.group = group.min(group_limit);
                }
                self.restore_tab_bookmarks(bookmarks, cx);
            }
            self.finish_restore_groups(cx);
            return;
        }
        self.set_status(tr("status.restoring_session"), cx);
        let task = cx.background_executor().spawn(async move {
            specs
                .into_iter()
                .map(|spec| {
                    let group = spec.group;
                    let bookmarks = spec.bookmarks.clone();
                    (group, bookmarks, spec.load(max_size))
                })
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let docs = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_restore_groups_with(docs, window, cx);
            });
        })
        .detach();
    }

    fn finish_restore_groups_with(
        &mut self,
        docs: Vec<(usize, Vec<usize>, Document)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if docs.is_empty() {
            self.new_file(window, cx);
            return;
        }
        for (group, bookmarks, doc) in docs {
            self.add_document(doc, window, cx);
            let group_limit = self.group_count().saturating_sub(1);
            if let Some(tab) = self.tabs.last_mut() {
                tab.group = group.min(group_limit);
            }
            self.restore_tab_bookmarks(bookmarks, cx);
        }
        self.finish_restore_groups(cx);
    }

    /// Select the restored group's active tab and refresh derived state.
    fn finish_restore_groups(&mut self, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            self.active = 0;
            return;
        }
        self.active_group = self.active_group.min(self.group_count().saturating_sub(1));
        self.active_in_group = [None, None];
        let memberships: Vec<usize> = self.tabs.iter().map(|tab| tab.group).collect();
        for group in 0..self.group_count() {
            let indices = tab_groups::group_tabs(&memberships, group);
            if indices.is_empty() {
                continue;
            }
            let position = self.restore_group_active[group].min(indices.len() - 1);
            let index = indices[position];
            self.active_in_group[group] = Some(self.tabs[index].id);
        }
        if let Some(index) =
            self.active_in_group[self.active_group].and_then(|id| self.tab_index_by_id(id))
        {
            self.active = index;
        } else if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.group == self.active_group)
        {
            self.active = index;
        }
        self.focus_editor = true;
        self.nav.clear();
        self.refresh_outline(cx);
        cx.notify();
    }

    /// Number of visible editor groups.
    fn group_count(&self) -> usize {
        if self.split.is_split() {
            2
        } else {
            1
        }
    }

    pub fn split_editor(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.split.is_split() {
            self.split = direction;
            cx.notify();
            return;
        }
        if !self.tabs.iter().any(|tab| tab.group == 0) {
            self.split = direction;
            self.active_group = 1;
            cx.notify();
            return;
        }
        self.sync_active(window, cx);
        self.split = direction;
        let memberships: Vec<usize> = self.tabs.iter().map(|tab| tab.group).collect();
        let first_group = tab_groups::group_tabs(&memberships, 0);
        if first_group.len() > 1 {
            let index = self.active;
            let id = self.tabs[index].id;
            self.tabs[index].group = 1;
            self.active_group = 1;
            self.active_in_group[1] = Some(id);
            let remaining: Vec<usize> = self
                .tabs
                .iter()
                .enumerate()
                .filter_map(|(index, tab)| (tab.group == 0).then_some(index))
                .collect();
            self.active_in_group[0] = remaining.last().map(|index| self.tabs[*index].id);
            self.active = self
                .tabs
                .iter()
                .position(|tab| tab.id == id)
                .unwrap_or(self.active);
        } else {
            self.active_group = 1;
            self.active_in_group[1] = None;
        }
        self.save_session_now(cx);
        cx.notify();
    }

    pub fn unsplit_editor(&mut self, cx: &mut Context<Self>) {
        if !self.split.is_split() {
            return;
        }
        let memberships: Vec<usize> = self.tabs.iter().map(|tab| tab.group).collect();
        let order = tab_groups::merge_groups(&memberships);
        let active_id = self.tabs.get(self.active).map(|tab| tab.id);
        let mut old: Vec<Option<Tab>> = std::mem::take(&mut self.tabs)
            .into_iter()
            .map(Some)
            .collect();
        self.tabs = order
            .into_iter()
            .filter_map(|index| old.get_mut(index).and_then(Option::take))
            .collect();
        for tab in &mut self.tabs {
            tab.group = 0;
        }
        self.split = SplitDirection::None;
        self.active_group = 0;
        self.active = active_id
            .and_then(|id| self.tab_index_by_id(id))
            .unwrap_or(0);
        self.active_in_group = [self.tabs.get(self.active).map(|tab| tab.id), None];
        self.save_session_now(cx);
        cx.notify();
    }

    pub fn switch_group(
        &mut self,
        target: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.split.is_split() {
            return;
        }
        let target = target.unwrap_or(usize::from(self.active_group == 0));
        let target = target.min(1);
        if target == self.active_group {
            return;
        }
        self.sync_active(window, cx);
        if let Some(tab) = self.tabs.get(self.active) {
            self.active_in_group[self.active_group] = Some(tab.id);
        }
        self.active_group = target;
        if let Some(index) = self.active_in_group[target]
            .and_then(|id| self.tab_index_by_id(id))
            .filter(|index| self.tabs[*index].group == target)
        {
            self.active = index;
        } else if let Some(index) = self.tabs.iter().position(|tab| tab.group == target) {
            self.active = index;
            self.active_in_group[target] = Some(self.tabs[index].id);
        }
        self.focus_editor = true;
        cx.notify();
    }

    /// Move a tab to the other group, creating a split when needed.
    pub fn move_tab_to_group(
        &mut self,
        id: u64,
        target: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if target >= tab_groups::MAX_GROUPS {
            return;
        }
        if !self.split.is_split() && target == 1 {
            self.split_editor(SplitDirection::Right, window, cx);
        }
        let Some(from) = self.tab_index_by_id(id) else {
            return;
        };
        if self.tabs[from].group == target {
            self.activate_tab_index(from, window, cx);
            return;
        }
        self.sync_active(window, cx);
        let mut tab = self.tabs.remove(from);
        tab.group = target;
        self.tabs.push(tab);
        let new_index = self.tabs.len() - 1;
        self.active_group = target;
        self.active = new_index;
        self.active_in_group[target] = Some(id);
        self.save_session_now(cx);
        cx.notify();
    }

    pub fn move_active_tab_to_other_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let target = if tab.group == 0 { 1 } else { 0 };
        let id = tab.id;
        self.move_tab_to_group(id, target, window, cx);
    }

    pub fn new_window(&mut self, cx: &mut Context<Self>) {
        if let Err(error) =
            window_registry::open_window(WindowInit::Blank, self.manual_window_chrome, cx)
        {
            self.set_status(
                tr_args("window.open_failed", &[("error", error.to_string())]),
                cx,
            );
        }
    }

    /// Move the active tab into a new window without a save prompt.
    pub fn move_tab_to_new_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let mut snapshot = session_tab_from(tab);
        let id = tab.id;
        if tab.large_file && tab.doc.is_modified() {
            snapshot.text = Some(tab.editor.read(cx).text().to_string());
            snapshot.modified = true;
        } else if tab.doc.partial_range().is_some() && tab.doc.is_modified() && !tab.doc.is_huge() {
            snapshot.text = Some(tab.doc.text.clone());
            snapshot.modified = true;
        }
        let mut recovery = None;
        if snapshot.huge && snapshot.huge_modified {
            let Some(recovery_id) = snapshot.recovery_id.clone() else {
                self.set_status(tr("window.move_unsaved_huge_failed"), cx);
                return;
            };
            recovery = luban_core::disk_engine::scan_recovery(luban_core::disk_engine::RETENTION)
                .into_iter()
                .find(|info| info.id == recovery_id);
            if recovery.is_none() {
                self.set_status(tr("window.move_unsaved_huge_failed"), cx);
                return;
            }
        }
        match window_registry::open_window_for_tab(
            snapshot,
            recovery,
            self.manual_window_chrome,
            cx,
        ) {
            Ok(_) => {
                if let Some(index) = self.tab_index_by_id(id) {
                    self.cancel_tab_jobs(index);
                    self.tabs.remove(index);
                    self.nav.remove_tab(id);
                    if self.active >= self.tabs.len() {
                        self.active = self.tabs.len().saturating_sub(1);
                    }
                    self.active_in_group = [None, None];
                    if let Some(tab) = self.tabs.get(self.active) {
                        self.active_group = tab.group;
                        self.active_in_group[tab.group] = Some(tab.id);
                    }
                    if self.tabs.is_empty() {
                        self.new_file(window, cx);
                    }
                }
                self.save_session_now(cx);
                cx.notify();
            }
            Err(error) => self.set_status(
                tr_args("window.open_failed", &[("error", error.to_string())]),
                cx,
            ),
        }
    }

    /// Activate the workspace roots and apply the workspace file's overrides.
    fn set_workspace_environment(
        &mut self,
        file: Option<PathBuf>,
        roots: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_file = file.clone();
        self.workspace_roots = roots;
        self.workspace_root = self.workspace_roots.first().cloned();
        self.workspace_name.clear();
        self.workspace_settings = WorkspaceSettings::default();
        self.settings = self.global_settings.clone();
        if let Some(path) = file {
            match WorkspaceFile::load(&path) {
                Ok(loaded) => {
                    let resolved = loaded.workspace.resolved_roots(&path);
                    if !resolved.is_empty() {
                        self.workspace_roots = resolved;
                        self.workspace_root = self.workspace_roots.first().cloned();
                    }
                    self.workspace_name = loaded.workspace.display_name(&path);
                    self.workspace_settings = loaded.workspace.settings.clone();
                    self.workspace_settings.apply_to(&mut self.settings);
                    self.settings.validate();
                    for warning in &loaded.warnings {
                        log::warn!("{warning}");
                    }
                    if let Some(warning) = loaded.warnings.first() {
                        self.status = warning.clone();
                    }
                }
                Err(error) => {
                    self.set_status(
                        tr_args("workspace.open_failed", &[("error", error.to_string())]),
                        cx,
                    );
                }
            }
        }
        if self.workspace_name.is_empty() {
            self.workspace_name = match (&self.workspace_file, self.workspace_roots.first()) {
                (Some(file), _) => file
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| file.display().to_string()),
                (None, Some(root)) => self.explorer.root_name_for(root),
                (None, None) => String::new(),
            };
        }
        self.explorer.set_roots(self.workspace_roots.clone());
        self.apply_settings_impl(window, cx, false);
    }

    /// Rebuild the workspace index in the background.
    fn rebuild_index(&mut self, cx: &mut Context<Self>) {
        self.index_generation = self.index_generation.wrapping_add(1);
        if let Some(cancel) = self.index_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        let roots = self.workspace_roots.clone();
        if roots.is_empty() {
            self.workspace_index = None;
            self.indexing = false;
            cx.notify();
            return;
        }
        let options = IndexOptions {
            respect_gitignore: self.settings.respect_gitignore,
            include_hidden: self.settings.show_hidden_files,
            ..IndexOptions::default()
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.index_cancel = Some(cancel.clone());
        self.indexing = true;
        let generation = self.index_generation;
        let task = cx
            .background_executor()
            .spawn(async move { build_index(&roots, options, &cancel) });
        cx.spawn(async move |this, cx| {
            let index = task.await;
            let _ = this.update(cx, |app, cx| {
                if app.index_generation != generation {
                    return;
                }
                app.indexing = false;
                app.workspace_index = Some(index);
                if app.palette.is_some() {
                    app.rebuild_quick_rows(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn stop_index(&mut self) {
        self.index_generation = self.index_generation.wrapping_add(1);
        if let Some(cancel) = self.index_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.workspace_index = None;
        self.indexing = false;
    }

    /// Refresh the symbol outline for the active tab.
    fn refresh_outline(&mut self, _cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            self.outline.clear();
            return;
        };
        if tab.doc.is_huge()
            || tab.large_file
            || tab.doc.partial_range().is_some()
            || tab.doc.is_non_text()
        {
            self.outline.clear();
            self.outline.error = Some(tr("outline.unavailable"));
            return;
        }
        if tab.doc.text.len() > 8 * 1024 * 1024 {
            self.outline.clear();
            self.outline.error = Some(tr("outline.too_large"));
            return;
        }
        let revision = tab.doc.revision();
        let language = tab.doc.language.clone();
        self.outline.refresh(&language, &tab.doc.text, revision);
    }

    /// Open a workspace file and replace the current roots.
    pub fn open_workspace(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let path = util::absolutize(path);
        let roots = match WorkspaceFile::load(&path) {
            Ok(loaded) => loaded.workspace.resolved_roots(&path),
            Err(error) => {
                self.set_status(
                    tr_args("workspace.open_failed", &[("error", error.to_string())]),
                    cx,
                );
                return;
            }
        };
        self.set_workspace_environment(Some(path.clone()), roots, window, cx);
        self.settings.show_file_explorer = true;
        self.global_settings.add_recent_workspace(path.clone());
        self.settings.add_recent_workspace(path);
        let _ = self.global_settings.save();
        self.apply_settings(window, cx);
        self.rebuild_index(cx);
        self.save_session_now(cx);
        self.set_status(
            tr_args("workspace.opened", &[("name", self.workspace_name.clone())]),
            cx,
        );
    }

    /// Open a folder as an in-memory single-root workspace.
    pub fn open_folder_workspace(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = util::absolutize(path);
        if !path.is_dir() {
            self.set_status(
                tr_args(
                    "status.open_failed",
                    &[
                        ("path", path.display().to_string()),
                        ("error", tr("workspace.not_a_folder")),
                    ],
                ),
                cx,
            );
            return;
        }
        self.set_workspace_environment(None, vec![path], window, cx);
        self.settings.show_file_explorer = true;
        self.apply_settings(window, cx);
        self.rebuild_index(cx);
        self.save_session_now(cx);
    }

    /// Write the workspace definition to its file, or the given new path.
    fn write_workspace(&mut self, path: &Path, cx: &mut Context<Self>) -> bool {
        let mut global = self.global_settings.clone();
        global.merge_global_fields_from(&self.settings);
        self.workspace_settings = WorkspaceSettings::capture(&self.settings, &global);
        let workspace = WorkspaceFile {
            version: luban_core::workspace_file::WORKSPACE_VERSION,
            name: self.workspace_name.clone(),
            roots: self
                .workspace_roots
                .iter()
                .map(|root| util::absolutize(root))
                .collect(),
            settings: self.workspace_settings.clone(),
        };
        match workspace.save_to(path) {
            Ok(()) => {
                self.workspace_file = Some(path.to_path_buf());
                self.settings.add_recent_workspace(path.to_path_buf());
                self.global_settings
                    .add_recent_workspace(path.to_path_buf());
                let _ = self.global_settings.save();
                self.set_status(
                    tr_args("workspace.saved", &[("path", path.display().to_string())]),
                    cx,
                );
                true
            }
            Err(error) => {
                self.set_status(
                    tr_args("workspace.save_failed", &[("error", error.to_string())]),
                    cx,
                );
                false
            }
        }
    }

    pub fn save_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace_roots.is_empty() {
            self.set_status(tr("workspace.none"), cx);
            return;
        }
        if let Some(path) = self.workspace_file.clone() {
            self.write_workspace(&path, cx);
            self.save_session_now(cx);
            return;
        }
        let root = self
            .workspace_roots
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("."));
        let suggested = WorkspaceFile::suggested_path(&root);
        self.show_file_dialog(
            FileDialogMode::Save,
            FileDialogPurpose::SaveWorkspace,
            root,
            suggested
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "new.luban-workspace".to_string()),
            window,
            cx,
        );
    }

    pub fn save_workspace_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace_roots.is_empty() {
            self.set_status(tr("workspace.none"), cx);
            return;
        }
        let root = self.workspace_roots.first().cloned().unwrap_or_default();
        let directory = self
            .workspace_file
            .as_ref()
            .and_then(|path| path.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.clone());
        let name = self
            .workspace_file
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                WorkspaceFile::suggested_path(&root)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "new.luban-workspace".to_string())
            });
        self.show_file_dialog(
            FileDialogMode::Save,
            FileDialogPurpose::SaveWorkspaceAs,
            directory,
            name,
            window,
            cx,
        );
    }

    pub fn close_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace_file = None;
        self.workspace_roots.clear();
        self.workspace_root = None;
        self.workspace_name.clear();
        self.workspace_settings = WorkspaceSettings::default();
        self.settings = self.global_settings.clone();
        self.stop_index();
        self.explorer.clear();
        self.apply_settings_impl(window, cx, false);
        self.save_session_now(cx);
        cx.notify();
    }

    pub fn add_workspace_root(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path = util::absolutize(path);
        if !path.is_dir()
            || self
                .workspace_roots
                .iter()
                .any(|root| util::same_path(root, &path))
        {
            return;
        }
        self.workspace_roots.push(path);
        self.workspace_root = self.workspace_roots.first().cloned();
        self.explorer.set_roots(self.workspace_roots.clone());
        self.rebuild_index(cx);
        if let Some(file) = self.workspace_file.clone() {
            self.write_workspace(&file, cx);
        }
    }

    /// Remove a root from the workspace. Tabs below it are closed when clean;
    /// dirty tabs keep the root until the user decides.
    fn remove_selected_workspace_root(&mut self, cx: &mut Context<Self>) {
        let selected = self.explorer.selected.clone();
        let Some(selected) = selected else {
            self.set_status(tr("explorer.no_selection"), cx);
            return;
        };
        let root = self
            .workspace_roots
            .iter()
            .find(|root| selected.starts_with(root))
            .cloned();
        match root {
            Some(root) => self.remove_workspace_root(&root, cx),
            None => self.set_status(tr("workspace.not_in_workspace"), cx),
        }
    }

    pub fn remove_workspace_root(&mut self, path: &Path, cx: &mut Context<Self>) {
        let dirty: Vec<u64> = self
            .tabs
            .iter()
            .filter(|tab| {
                tab.doc
                    .path
                    .as_ref()
                    .is_some_and(|file| file.starts_with(path) && tab.doc.is_modified())
            })
            .map(|tab| tab.id)
            .collect();
        if !dirty.is_empty() {
            let queue: Vec<u64> = self
                .tabs
                .iter()
                .filter(|tab| {
                    tab.doc
                        .path
                        .as_ref()
                        .is_some_and(|file| file.starts_with(path))
                })
                .map(|tab| tab.id)
                .collect();
            let keep_id = self
                .tabs
                .iter()
                .find(|tab| {
                    tab.doc
                        .path
                        .as_ref()
                        .is_none_or(|file| !file.starts_with(path))
                })
                .map(|tab| tab.id)
                .unwrap_or(0);
            self.trim_plan = Some(TrimPlan {
                keep_id,
                queue,
                save_queue: dirty,
                remove_root: Some(path.to_path_buf()),
            });
            self.confirm = Some(ConfirmState::new(
                CloseIntent::TrimTabs,
                tr("workspace.remove_root_title"),
                tr_args(
                    "workspace.remove_root_dirty",
                    &[(
                        "count",
                        self.trim_plan
                            .as_ref()
                            .map(|p| p.save_queue.len())
                            .unwrap_or(0)
                            .to_string(),
                    )],
                ),
            ));
            cx.notify();
            return;
        }
        self.remove_workspace_root_now(path, cx);
    }

    pub fn remove_workspace_root_now(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.workspace_roots
            .retain(|root| !util::same_path(root, path));
        if self
            .workspace_root
            .as_deref()
            .is_some_and(|root| util::same_path(root, path))
        {
            self.workspace_root = self.workspace_roots.first().cloned();
        }
        let closing: Vec<u64> = self
            .tabs
            .iter()
            .filter(|tab| {
                tab.doc
                    .path
                    .as_ref()
                    .is_some_and(|file| file.starts_with(path))
            })
            .map(|tab| tab.id)
            .collect();
        for id in closing {
            self.close_tab_by_id_without_prompt(id, cx);
        }
        self.explorer.set_roots(self.workspace_roots.clone());
        self.rebuild_index(cx);
        if let Some(file) = self.workspace_file.clone() {
            self.write_workspace(&file, cx);
        } else if self.workspace_roots.is_empty() {
            self.close_workspace_no_window(cx);
        }
        self.save_session_now(cx);
        cx.notify();
    }

    fn close_workspace_no_window(&mut self, cx: &mut Context<Self>) {
        self.workspace_file = None;
        self.workspace_roots.clear();
        self.workspace_root = None;
        self.workspace_name.clear();
        self.workspace_settings = WorkspaceSettings::default();
        self.settings = self.global_settings.clone();
        self.stop_index();
        self.explorer.clear();
        cx.notify();
    }

    fn close_tab_by_id_without_prompt(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(index) = self.tab_index_by_id(id) {
            self.cancel_tab_jobs(index);
            self.tabs.remove(index);
            self.nav.remove_tab(id);
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len().saturating_sub(1);
            }
            self.active_in_group[self.active_group] = None;
            self.find.dirty = true;
            cx.notify();
        }
    }

    /// Pull the active editor's text/cursor into the document model. The
    /// editor is the source of truth while typing.
    fn sync_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.get(self.active).map(|_| self.active) else {
            return;
        };
        if self.tabs[index].doc.is_huge() {
            self.sync_huge_tab(index, window, cx);
            return;
        }
        // Selection changes do not emit InputEvent, so they are synced from the
        // editor Rope every frame. This is O(log n) and keeps the document's
        // cursor/selection correct for replace-one, selection scope and session
        // metadata; only text changes need the whole-buffer diff below.
        let large_file = self.tabs[index].large_file;
        if large_file || !self.tabs[index].needs_sync {
            self.sync_editor_selection(index, large_file, cx);
            return;
        }
        let tab = &self.tabs[index];
        let (raw, cursor_byte, range) = {
            let state = tab.editor.read(cx);
            (
                state.text().to_string(),
                state.cursor(),
                state.selected_range(),
            )
        };
        let had_cr = raw.contains('\r');
        // Non-text buffers keep their original line-ending bytes; rewriting
        // CR as LF here would break the byte-preserving save guarantee.
        let normalize_cr = had_cr && !tab.doc.is_non_text();
        let normalized = if normalize_cr { eol::to_lf(&raw) } else { raw };
        let char_count = textutil::char_count(&normalized);
        let cursor = textutil::byte_to_char(&normalized, cursor_byte.min(normalized.len()));
        let start =
            textutil::byte_to_char(&normalized, range.start.min(normalized.len())).min(char_count);
        let end =
            textutil::byte_to_char(&normalized, range.end.min(normalized.len())).min(char_count);
        let (anchor, cursor) = if cursor <= start {
            (end, start)
        } else {
            (start, end)
        };

        let changed = normalized != self.tabs[index].doc.text;
        let tab = &mut self.tabs[index];
        if changed {
            self.occurrence_anchor = None;
            tab.bookmarks.adjust(&tab.doc.text, &normalized);
            tab.doc.set_text_from_widget(&normalized, cursor, anchor);
            tab.needs_sync = false;
            tab.pending_edit = false;
            tab.highlighted_revision = u64::MAX;
            tab.bookmark_revision = tab.bookmark_revision.wrapping_add(1);
        } else {
            tab.doc.set_selection(anchor, cursor);
            tab.needs_sync = false;
            tab.pending_edit = false;
        }
        if changed {
            self.find.dirty = true;
            self.apply_fold_candidates(index, cx);
            self.schedule_fold_candidates(index, window, cx);
            self.refresh_bookmark_decorations(index, cx);
        }

        if normalize_cr {
            let b0 = textutil::char_to_byte(&normalized, start);
            let b1 = textutil::char_to_byte(&normalized, end);
            let editor = self.tabs[index].editor.clone();
            editor.update(cx, |state, cx| {
                state.set_value(normalized, window, cx);
                state.set_selected_range(b0..b1.max(b0), cx);
            });
        }
    }

    /// Copy the editor's caret and selection into the document without
    /// materializing or diffing the buffer.
    fn sync_editor_selection(&mut self, index: usize, clear_pending: bool, cx: &gpui_kit::App) {
        let (cursor_byte, range) = {
            let state = self.tabs[index].editor.read(cx);
            (state.cursor(), state.selected_range())
        };
        let (anchor, cursor) = {
            let state = self.tabs[index].editor.read(cx);
            let rope = state.text();
            let len = rope.len();
            let cursor = rope.byte_to_char_idx(cursor_byte.min(len));
            let start = rope.byte_to_char_idx(range.start.min(len));
            let end = rope.byte_to_char_idx(range.end.min(len));
            if cursor <= start {
                (end, start)
            } else {
                (start, end)
            }
        };
        let tab = &mut self.tabs[index];
        tab.doc.set_selection(anchor, cursor);
        if clear_pending {
            // Large-file buffers live in the editor; the Change event already
            // marked the document dirty, so this flag has served its purpose.
            tab.needs_sync = false;
            tab.pending_edit = false;
        }
    }

    /// Materialize a detached document buffer from its editor. Returns false
    /// when the tab does not exist.
    fn ensure_tab_text(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        if self.tabs.get(index).is_some_and(|tab| tab.doc.is_huge()) {
            self.set_status(tr("status.huge_op_unavailable"), cx);
            return false;
        }
        let Some(tab) = self.tabs.get_mut(index) else {
            return false;
        };
        if !tab.doc.is_text_detached() {
            return true;
        }
        let (text, char_count) = {
            let state = tab.editor.read(cx);
            let rope = state.text();
            (rope.to_string(), rope.len_chars())
        };
        tab.doc.attach_text_counted(text, char_count);
        true
    }

    /// Release a large document's full-text snapshot after a transient
    /// operation; the editor keeps the only copy.
    fn release_tab_text(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index) {
            if tab.large_file {
                let _ = tab.doc.detach_text();
            }
        }
    }

    fn ensure_active_text(&mut self, cx: &mut Context<Self>) -> bool {
        self.ensure_tab_text(self.active, cx)
    }

    fn release_active_text(&mut self) {
        self.release_tab_text(self.active);
    }

    fn select_char_range(
        &mut self,
        start: usize,
        end: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let total = tab.doc.char_count();
        let (start, end) = if start <= end {
            (start.min(total), end.min(total))
        } else {
            (end.min(total), start.min(total))
        };
        tab.doc.set_selection(start, end);
        let editor = tab.editor.clone();
        editor.update(cx, |state, cx| {
            let char_count = state.text().len_chars();
            let b0 = state.text().char_to_byte_idx(start.min(char_count));
            let b1 = state.text().char_to_byte_idx(end.min(char_count));
            state.set_selected_range(b0..b1, cx);
            state.focus(window, cx);
        });
    }

    fn replace_document_text(
        &mut self,
        new_text: String,
        select_start: usize,
        select_end: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tabs.get(self.active).map(|_| self.active) else {
            return;
        };
        let tab = &mut self.tabs[index];
        let char_count = textutil::char_count(&new_text);
        let anchor = select_start.min(char_count);
        let cursor = select_end.min(char_count);
        let large_file = tab.large_file;
        if !large_file && !tab.doc.is_huge() && !tab.doc.is_non_text() {
            tab.bookmarks.adjust(&tab.doc.text, &new_text);
            tab.bookmark_revision = tab.bookmark_revision.wrapping_add(1);
        }
        tab.highlighted_revision = u64::MAX;
        tab.decorations_key = None;
        let b0 = textutil::char_to_byte(&new_text, anchor);
        let b1 = textutil::char_to_byte(&new_text, cursor);
        if large_file {
            // Free the document snapshot before handing the new buffer to the
            // editor, so a replace never holds three full copies.
            let _ = tab.doc.detach_text();
            tab.doc.set_selection(anchor, cursor);
            tab.editor.update(cx, |state, cx| {
                state.replace_all(new_text, window, cx);
                state.set_selected_range(b0..b1, cx);
                state.focus(window, cx);
            });
            tab.doc
                .record_external_replacement(char_count, anchor, cursor);
        } else {
            tab.doc.replace_all_text(&new_text, EditKind::Other);
            tab.doc.set_selection(anchor, cursor);
            tab.editor.update(cx, |state, cx| {
                // Preserve the editor's undo history across programmatic edits.
                state.replace_all(new_text, window, cx);
                state.set_selected_range(b0..b1, cx);
                state.focus(window, cx);
            });
        }
        tab.needs_sync = false;
        cx.notify();
    }

    pub(crate) fn set_status(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.status = message.into();
        cx.notify();
    }

    fn focus_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab() {
            tab.editor.update(cx, |state, cx| state.focus(window, cx));
        }
    }

    fn editor_action(
        &mut self,
        action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_editor(window, cx);
        window.dispatch_action(action, cx);
        cx.notify();
    }

    fn undo_or_editor(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_doc().is_some_and(Document::is_huge) {
            self.huge_history(redo, window, cx);
            return;
        }
        if redo {
            self.editor_action(Box::new(Redo), window, cx);
        } else {
            self.editor_action(Box::new(Undo), window, cx);
        }
    }

    fn active_selection(&self) -> (usize, usize) {
        self.active_doc()
            .map(|doc| doc.selection())
            .unwrap_or((0, 0))
    }

    // ------------------------------------------------------------------
    // Disk-engine (huge) documents
    // ------------------------------------------------------------------

    /// Rows the editor's text area can actually display. `visible_row_range`
    /// is clipped to the mounted window and is therefore too small near a
    /// window or range end.
    fn huge_viewport_lines(&self, index: usize, cx: &gpui_kit::App) -> Option<u64> {
        let tab = self.tabs.get(index)?;
        let state = tab.editor.read(cx);
        let line_height = state.line_height()?.as_f32();
        if line_height <= 0.0 {
            return None;
        }
        let height = state.input_bounds().size.height.as_f32();
        if height <= 0.0 {
            return None;
        }
        Some((height / line_height).floor().max(1.0) as u64)
    }

    /// Synchronise the editor window with the engine: apply text changes,
    /// update the global cursor and request a new window near an edge.
    fn sync_huge_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ui) = self.tabs.get_mut(index).and_then(|tab| tab.huge.as_mut()) {
            if let Some(request) = ui.scroll.take_request() {
                ui.scroll_target = Some(request);
                // The pointer position supersedes the top line of a window
                // that is still being laid out.
                ui.pending_top_line = None;
                // A newer pointer position wins over a jump that is still
                // waiting for the in-flight read to finish.
                ui.queued_window = None;
            }
        }
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(ui) = tab.huge.as_ref() else {
            return;
        };
        let busy = ui.is_busy();
        let wants_text = tab.needs_sync;
        let apply_text = !busy;
        let (text, cursor_byte, range, scroll_offset_y, editor_line_height) = {
            let state = tab.editor.read(cx);
            let rope = state.text();
            let cursor = state.cursor().min(rope.len());
            (
                if wants_text {
                    Some(rope.to_string())
                } else {
                    None
                },
                cursor,
                state.selected_range(),
                state.scroll_offset().y.as_f32(),
                state.line_height().map(|height| height.as_f32()),
            )
        };
        let local_anchor = if cursor_byte <= range.start {
            range.end
        } else {
            range.start
        };

        if !busy {
            if let Some(text) = text.as_ref() {
                if *text != ui.last_text {
                    let (start, removed, inserted) = diff_bytes(&ui.last_text, text);
                    let global = ui.window_start.saturating_add(start as u64);
                    let inserted = inserted.to_string();
                    let result = self.tabs[index]
                        .doc
                        .huge()
                        .map(|huge| {
                            let mut engine = huge.write();
                            let result = engine
                                .apply_edit(global, removed as u64, &inserted)
                                .map(|_| ());
                            if result.is_ok() {
                                huge.refresh_metadata(&engine);
                            }
                            result
                        })
                        .unwrap_or(Ok(()));
                    if let Err(error) = result {
                        // Keep the editor and engine in agreement; the
                        // committed text is the authoritative window.
                        let revert = self.tabs[index]
                            .huge
                            .as_ref()
                            .map(|ui| ui.last_text.clone())
                            .unwrap_or_default();
                        let local = cursor_byte.min(revert.len());
                        let editor = self.tabs[index].editor.clone();
                        editor.update(cx, |state, cx| {
                            state.set_value(revert, window, cx);
                            state.set_selected_range(local..local, cx);
                        });
                        self.set_status(error.to_string(), cx);
                        return;
                    }
                    let newlines = text.bytes().filter(|byte| *byte == b'\n').count() as u64;
                    if let Some(ui) = self.tabs[index].huge.as_mut() {
                        ui.last_text = text.clone();
                        ui.window_len = ui.last_text.len() as u64;
                        ui.window_newlines = newlines;
                        ui.cancel_pending();
                    }
                    self.find.huge_results.clear();
                    self.find.huge_truncated = false;
                    self.tabs[index].needs_sync = false;
                    self.tabs[index].highlighted_revision = u64::MAX;
                    let trimmed = self.tabs[index]
                        .doc
                        .huge()
                        .map(|huge| huge.read().history_trimmed())
                        .unwrap_or(false);
                    if trimmed {
                        let notify = self.tabs[index]
                            .huge
                            .as_mut()
                            .map(|ui| {
                                if ui.history_trimmed_notified {
                                    false
                                } else {
                                    ui.history_trimmed_notified = true;
                                    true
                                }
                            })
                            .unwrap_or(false);
                        if notify {
                            self.set_status(tr("status.huge_history_trimmed"), cx);
                        }
                    }
                }
            }
        }

        let (position_line, position_character) = {
            let state = self.tabs[index].editor.read(cx);
            let rope = state.text();
            let cursor = state.cursor().min(rope.len());
            let position = rope.offset_to_position(cursor);
            (position.line, position.character)
        };
        let (cursor, anchor, update_line, scroll_line, window_line, window_column, scroll_top) = {
            let Some(ui) = self.tabs[index].huge.as_mut() else {
                return;
            };
            let (cursor, anchor, update_line) = match ui.offscreen_cursor {
                Some((local, global)) if local == cursor_byte as u64 => (global, global, false),
                Some(_) => {
                    ui.offscreen_cursor = None;
                    (
                        ui.window_start.saturating_add(cursor_byte as u64),
                        ui.window_start.saturating_add(local_anchor as u64),
                        true,
                    )
                }
                None => (
                    ui.window_start.saturating_add(cursor_byte as u64),
                    ui.window_start.saturating_add(local_anchor as u64),
                    true,
                ),
            };
            // A window swap installs a deferred scroll offset that the editor
            // applies on its next layout. Until then the scroll handle still
            // holds the previous window's offset, which would point at a very
            // different global line; report the installed top line instead.
            let scroll_top = match (ui.pending_top_line, editor_line_height) {
                (Some(pending), Some(line_height)) => {
                    let pending = pending.max(ui.window_line);
                    let expected = -((pending - ui.window_line) as f32 * line_height);
                    let applied = (scroll_offset_y - expected).abs() <= 0.5;
                    let stale = ui.loaded_at.elapsed() >= Duration::from_millis(500);
                    if applied || stale {
                        ui.pending_top_line = None;
                    }
                    if applied {
                        Some(top_line_from_scroll(
                            scroll_offset_y,
                            line_height,
                            ui.window_line,
                            ui.window_newlines,
                        ))
                    } else {
                        Some(pending)
                    }
                }
                (Some(pending), None) => Some(pending),
                (None, Some(line_height)) => Some(top_line_from_scroll(
                    scroll_offset_y,
                    line_height,
                    ui.window_line,
                    ui.window_newlines,
                )),
                (None, None) => None,
            };
            let scroll_line = scroll_top.unwrap_or(ui.window_line);
            (
                cursor,
                anchor,
                update_line,
                scroll_line,
                ui.window_line,
                ui.window_column,
                scroll_top,
            )
        };
        if let Some(huge) = self.tabs[index].doc.huge_mut() {
            huge.cursor = cursor;
            huge.anchor = anchor;
            huge.scroll_line = scroll_line;
            huge.write().set_recovery_position(cursor, scroll_line);
            if update_line {
                huge.cursor_line = window_line.saturating_add(position_line as u64);
                huge.cursor_column = if position_line == 0 {
                    window_column.saturating_add(position_character as u64)
                } else {
                    position_character as u64 + 1
                };
            }
        }
        if let (Some(top), Some(ui)) = (scroll_top, self.tabs[index].huge.as_mut()) {
            ui.scroll_top_line = top;
            // A pending window may still be targeting an older position; keep
            // the newest target until that request finishes and is checked.
            if ui.scroll_target == Some(top) && ui.pending_window.is_none() {
                ui.scroll_target = None;
            }
        }
        if !wants_text || apply_text {
            self.tabs[index].needs_sync = false;
            self.tabs[index].pending_edit = false;
        }
        self.maybe_schedule_huge_window(index, window, cx);
    }

    fn maybe_schedule_huge_window(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(ui) = tab.huge.as_ref() else {
            return;
        };
        if ui.pending_window.is_some() || ui.is_busy() {
            return;
        }
        let (cursor_byte, position_line, scroll_offset_y, line_height) = {
            let state = tab.editor.read(cx);
            let rope = state.text();
            let cursor = state.cursor().min(rope.len());
            let position = rope.offset_to_position(cursor);
            (
                cursor,
                position.line as u64,
                state.scroll_offset().y.as_f32(),
                state.line_height().map(|height| height.as_f32()),
            )
        };
        let Some(line_height) = line_height.filter(|height| *height > 0.0) else {
            return;
        };
        let viewport_lines = self.huge_viewport_lines(index, cx).unwrap_or(1);
        let scroll_target = ui.scroll_target;
        let dragging = ui.scroll.is_dragging();
        if let Some(target) = scroll_target {
            self.huge_scroll_to(index, target, viewport_lines, line_height, window, cx);
            return;
        }
        // While the thumb is held, a window edge must not start its own
        // prefetch and pull the viewport away from the pointer.
        if dragging || !ui.settle_elapsed() {
            return;
        }
        let total_lines = tab.doc.line_count() as u64;
        let window_line = ui.window_line;
        let window_end = ui.window_end_line();
        // `visible_row_range` comes from the previous layout, so a fast wheel
        // gesture can reach the window edge before it catches up. The scroll
        // offset is current, and rows are a uniform grid.
        let top_global = top_line_from_scroll(
            scroll_offset_y,
            line_height,
            window_line,
            ui.window_newlines,
        );
        let bottom_global = top_global.saturating_add(viewport_lines.saturating_sub(1));
        let cursor_line = window_line.saturating_add(position_line);
        let local_cursor = cursor_byte as u64;
        // Byte-edge tracking only matters when one line fills the window;
        // normal windows swap by line.
        let long_line_window = ui.window_newlines <= 8;
        let edge_bytes = (WINDOW_BYTES / 8).max(64 * 1024);
        let near_bytes_start =
            long_line_window && ui.window_start > 0 && local_cursor <= edge_bytes;
        let near_bytes_end = long_line_window
            && !ui.at_eof
            && local_cursor.saturating_add(edge_bytes) >= ui.window_len;
        let left_edge = top_global.saturating_sub(window_line) <= EDGE_LINES && ui.window_start > 0;
        let right_edge =
            window_end.saturating_sub(bottom_global) <= EDGE_LINES && bottom_global < total_lines;
        // The editor only holds a local sentinel for an offscreen caret. Do not
        // treat that sentinel as a reason to chase the caret while the user is
        // scrolling the viewport somewhere else.
        let cursor_offscreen = ui.offscreen_cursor.is_some();
        let near_cursor_start = !cursor_offscreen
            && cursor_line.saturating_sub(window_line) <= EDGE_LINES
            && window_line > 1;
        let near_cursor_end = !cursor_offscreen
            && window_end.saturating_sub(cursor_line) <= EDGE_LINES
            && window_end < total_lines;
        let cursor_outside =
            !cursor_offscreen && (cursor_line < window_line || cursor_line > window_end);
        if !near_bytes_start
            && !near_bytes_end
            && !left_edge
            && !right_edge
            && !near_cursor_start
            && !near_cursor_end
            && !cursor_outside
        {
            return;
        }
        // An offscreen caret keeps its real global position; the editor only
        // holds a local sentinel for it, which must not become the cursor.
        let cursor = ui
            .offscreen_cursor
            .map(|(_, global)| global)
            .unwrap_or_else(|| ui.window_start.saturating_add(local_cursor));
        let top_line = top_global.max(1);
        let target = if near_bytes_start {
            WindowTarget::Offset(ui.window_start.saturating_sub(WINDOW_BYTES / 2))
        } else if near_bytes_end {
            WindowTarget::Offset(cursor)
        } else {
            let base = if cursor_outside {
                cursor_line
            } else {
                top_global
            };
            WindowTarget::Line(base.saturating_sub(PREFETCH_LINES).max(1))
        };
        match target {
            WindowTarget::Line(line) if line == window_line => return,
            WindowTarget::Offset(offset)
                if offset >= ui.window_start
                    && offset.saturating_add(WINDOW_BYTES / 2)
                        <= ui.window_start.saturating_add(ui.window_len) =>
            {
                return
            }
            _ => {}
        }
        self.request_huge_window(index, target, cursor, top_line, None, window, cx);
    }

    /// Move the viewport to a global line requested by the file scrollbar.
    ///
    /// The tab keeps the target until the viewport reaches it, so a window
    /// load that is already running cannot overwrite a newer pointer position.
    /// Inside the mounted window the offset is applied directly; otherwise a
    /// new window is read around the target.
    fn huge_scroll_to(
        &mut self,
        index: usize,
        target: u64,
        viewport_lines: u64,
        line_height: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(ui) = tab.huge.as_ref() else {
            return;
        };
        if ui.pending_window.is_some() || ui.is_busy() {
            return;
        }
        let editor = tab.editor.clone();
        let total_lines = tab.doc.line_count() as u64;
        let max_top = max_top_line(total_lines, viewport_lines);
        let target = target.clamp(1, max_top);
        let window_line = ui.window_line;
        let window_max_top = window_max_top_line(window_line, ui.window_newlines, viewport_lines);
        let cursor = ui
            .offscreen_cursor
            .map(|(_, global)| global)
            .unwrap_or_else(|| {
                ui.window_start
                    .saturating_add(editor.read(cx).cursor() as u64)
            });
        if let Some(ui) = self.tabs[index].huge.as_mut() {
            ui.scroll_target = Some(target);
            ui.pending_top_line = None;
        }
        if target >= window_line && target <= window_max_top {
            let local = target.saturating_sub(window_line) as f32;
            let desired = point(px(0.), px(-(local * line_height)));
            // Avoid a notify/render loop when the viewport already sits on the
            // requested line.
            if editor.read(cx).scroll_offset() != desired {
                editor.update(cx, |state, cx| {
                    state.set_scroll_offset(desired, cx);
                });
            }
            return;
        }
        // Leave some scrollback above the target and keep the target off the
        // window's top edge, which would otherwise trigger an edge prefetch
        // immediately after the load.
        let read_line = target.saturating_sub(PREFETCH_LINES).max(1);
        self.request_huge_window(
            index,
            WindowTarget::Line(read_line),
            cursor,
            target,
            None,
            window,
            cx,
        );
    }

    /// Ask the executor for another window. Stale results are discarded by
    /// `generation`.
    #[allow(clippy::too_many_arguments)]
    fn request_huge_window(
        &mut self,
        index: usize,
        target: WindowTarget,
        cursor: u64,
        top_line: u64,
        select: Option<(u64, u64)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(ui) = tab.huge.as_ref() else {
            return;
        };
        if ui.is_busy() {
            return;
        }
        let Some(engine) = tab.doc.huge().map(|huge| huge.engine.clone()) else {
            return;
        };
        let tab_id = tab.id;
        let pending = ui.pending_window.is_some();
        // The file scrollbar passes its own target as `top_line`, so matching
        // values identify a scrollbar request. Any other jump supersedes a
        // drag target that has not been reached yet.
        let from_scrollbar = ui.scroll_target == Some(top_line);
        if let Some(ui) = self.tabs[index].huge.as_mut() {
            if !from_scrollbar {
                ui.scroll_target = None;
            }
            if pending {
                // One disk read per tab: keep only the newest jump and issue
                // it when the current read finishes.
                log::debug!("huge window jump queued target={target:?}");
                ui.queued_window = Some(WindowJump {
                    target,
                    cursor,
                    top_line,
                    select,
                });
                cx.notify();
                return;
            }
        }
        let generation = self.tabs[index]
            .huge
            .as_mut()
            .map(|ui| ui.next_generation())
            .unwrap_or(0);
        if let Some(ui) = self.tabs[index].huge.as_mut() {
            ui.pending_window = Some(WindowRequest {
                generation,
                cursor,
                top_line,
                select,
                requested_at: Instant::now(),
            });
        }
        let task = cx.background_executor().spawn(async move {
            let engine = engine.read().unwrap_or_else(|error| error.into_inner());
            match target {
                WindowTarget::Line(line) => engine.read_window_from_line(line, WINDOW_BYTES),
                WindowTarget::Offset(offset) => engine.read_window(offset, WINDOW_BYTES),
            }
        });
        log::debug!("huge window read start gen={generation} target={target:?}");
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_huge_window(tab_id, generation, result, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_huge_window(
        &mut self,
        tab_id: u64,
        generation: u64,
        result: luban_core::Result<WindowText>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let Some(ui) = self.tabs[index].huge.as_ref() else {
            return;
        };
        let Some(request) = ui.pending_window.as_ref() else {
            return;
        };
        if request.generation != generation {
            return;
        }
        let request = self.tabs[index]
            .huge
            .as_mut()
            .and_then(|ui| ui.pending_window.take());
        let Some(request) = request else {
            return;
        };
        match result {
            Ok(loaded) => {
                let desired_top = self.tabs[index]
                    .huge
                    .as_ref()
                    .and_then(|ui| ui.scroll_target)
                    .unwrap_or(request.top_line);
                let viewport_lines = self.huge_viewport_lines(index, cx).unwrap_or(1);
                // `visible_row_range` is clipped to the mounted window; use the
                // real viewport capacity so the last screen of the window or
                // range is filled instead of scrolled past its end.
                let top_line =
                    clamp_top_line(desired_top, loaded.line, loaded.newlines, viewport_lines);
                let top_local = top_line.saturating_sub(loaded.line);
                let target_inside =
                    desired_top >= loaded.line && desired_top <= loaded.line + loaded.newlines;
                let target_visible = target_inside
                    && desired_top >= top_line
                    && desired_top < top_line.saturating_add(viewport_lines);
                let cursor_local =
                    request.cursor.saturating_sub(loaded.start).min(loaded.len) as usize;
                let line_height = self.tabs[index]
                    .editor
                    .read(cx)
                    .line_height()
                    .map(|height| height.as_f32())
                    .unwrap_or(18.0);
                let select = request.select.map(|(start, end)| {
                    (
                        start.saturating_sub(loaded.start).min(loaded.len) as usize,
                        end.saturating_sub(loaded.start).min(loaded.len) as usize,
                    )
                });
                log::debug!(
                    "huge window line={} newlines={} top={top_line} desired={desired_top} viewport={viewport_lines} cursor_local={cursor_local} elapsed_ms={}",
                    loaded.line,
                    loaded.newlines,
                    request.requested_at.elapsed().as_millis()
                );
                let text = loaded.text.clone();
                let editor = self.tabs[index].editor.clone();
                editor.update(cx, |state, cx| {
                    state.set_value(text, window, cx);
                    state.set_selected_range(cursor_local..cursor_local, cx);
                    if let Some((start, end)) = select {
                        state.set_selected_range(start..end.max(start), cx);
                    }
                    let y = px(-(top_local as f32 * line_height));
                    state.set_scroll_offset(point(px(0.), y), cx);
                    state.focus(window, cx);
                });
                let inside = request.cursor >= loaded.start
                    && request.cursor <= loaded.start.saturating_add(loaded.len);
                if let Some(ui) = self.tabs[index].huge.as_mut() {
                    ui.set_window(loaded.clone());
                    ui.offscreen_cursor = if inside {
                        None
                    } else {
                        Some((cursor_local as u64, request.cursor))
                    };
                    ui.scroll_top_line = top_line;
                    // The editor applies the new scroll offset on its next
                    // layout; keep the scrollbar on this top line until then.
                    ui.pending_top_line = Some(top_line);
                    if target_visible && ui.scroll_target == Some(desired_top) {
                        ui.scroll_target = None;
                    }
                }
                if let Some(huge) = self.tabs[index].doc.huge_mut() {
                    huge.cursor = request.cursor;
                    huge.anchor = select
                        .map(|(start, _)| loaded.start.saturating_add(start as u64))
                        .unwrap_or(huge.cursor);
                    huge.scroll_line = top_line;
                }
                self.tabs[index].needs_sync = false;
                self.focus_editor = true;
                cx.notify();
            }
            Err(err) => {
                self.set_status(err.to_string(), cx);
            }
        }
        self.start_queued_huge_window(tab_id, window, cx);
    }

    /// Issue the newest jump that arrived while a window read was in flight.
    fn start_queued_huge_window(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let Some(jump) = self.tabs[index]
            .huge
            .as_mut()
            .and_then(|ui| ui.queued_window.take())
        else {
            return;
        };
        self.request_huge_window(
            index,
            jump.target,
            jump.cursor,
            jump.top_line,
            jump.select,
            window,
            cx,
        );
    }

    fn huge_history(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let index = self.active;
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.huge.as_ref().is_some_and(|ui| ui.is_busy()) {
            return;
        }
        let Some(huge) = tab.doc.huge() else {
            return;
        };
        let outcome = {
            let mut engine = huge.write();
            let outcome = if redo { engine.redo() } else { engine.undo() };
            huge.refresh_metadata(&engine);
            outcome
        };
        match outcome {
            Some(outcome) => {
                let total_lines = tab.doc.line_count() as u64;
                let viewport_lines = self.huge_viewport_lines(index, cx).unwrap_or(1);
                let target_line = outcome.line.saturating_sub(PREFETCH_LINES / 2).max(1);
                let top_line = outcome
                    .line
                    .saturating_sub(1)
                    .max(1)
                    .min(max_top_line(total_lines, viewport_lines));
                self.request_huge_window(
                    index,
                    WindowTarget::Line(target_line),
                    outcome.cursor,
                    top_line,
                    None,
                    window,
                    cx,
                );
                self.set_status(
                    tr(if redo {
                        "status.huge_redo"
                    } else {
                        "status.huge_undo"
                    }),
                    cx,
                );
            }
            None => self.set_status(tr("status.huge_no_history"), cx),
        }
    }

    fn huge_jump_line(
        &mut self,
        line: u64,
        column: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_active(window, cx);
        let index = self.active;
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(huge) = tab.doc.huge() else {
            return;
        };
        let offset = {
            let engine = huge.read();
            engine.offset_for_line(line, column)
        };
        let offset = match offset {
            Ok(offset) => offset,
            Err(err) => {
                self.set_status(err.to_string(), cx);
                return;
            }
        };
        let total_lines = tab.doc.line_count() as u64;
        let line = line.clamp(1, total_lines.max(1));
        let viewport_lines = self.huge_viewport_lines(index, cx).unwrap_or(1);
        let target_line = line.saturating_sub(PREFETCH_LINES / 2).max(1);
        let top_line = line
            .saturating_sub(1)
            .max(1)
            .min(max_top_line(total_lines, viewport_lines));
        self.request_huge_window(
            index,
            WindowTarget::Line(target_line),
            offset,
            top_line,
            Some((offset, offset)),
            window,
            cx,
        );
        self.focus_editor = true;
        cx.notify();
    }

    fn huge_jump_end(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let index = self.active;
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(huge) = tab.doc.huge() else {
            return;
        };
        let (end, last_line) = {
            let engine = huge.read();
            (engine.total_len(), engine.total_lines())
        };
        let viewport_lines = self.huge_viewport_lines(index, cx).unwrap_or(1);
        let top_line = max_top_line(last_line, viewport_lines);
        self.request_huge_window(
            index,
            WindowTarget::Offset(end.saturating_sub(WINDOW_BYTES / 2)),
            end,
            top_line,
            Some((end, end)),
            window,
            cx,
        );
        self.focus_editor = true;
        cx.notify();
    }

    fn huge_find_next(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let query = self.find.query_text(cx);
        if query.is_empty() {
            self.open_find(false, window, cx);
            return;
        }
        let index = self.active;
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(ui) = tab.huge.as_ref() else {
            return;
        };
        if ui.is_busy() {
            return;
        }
        let search = self.find.engine(&query);
        if let Some(error) = search.error() {
            self.find.error = Some(error.to_string());
            cx.notify();
            return;
        }
        let Some(huge_state) = tab.doc.huge() else {
            return;
        };
        let (cursor, anchor) = (huge_state.cursor, huge_state.anchor);
        let from = if forward {
            cursor.max(anchor)
        } else {
            cursor.min(anchor)
        };
        let total_len = huge_state.read().total_len();
        // "In selection" is served from the current window; a selection can
        // never span windows.
        if self.find.in_selection && cursor != anchor {
            let start = cursor.min(anchor);
            let end = cursor.max(anchor);
            if let (Some(local_start), Some(local_end)) =
                (ui.local_of_global(start), ui.local_of_global(end))
            {
                let found = if forward {
                    search.find_first_in(&ui.last_text, local_start as usize)
                } else {
                    search.find_last_in(&ui.last_text, local_end as usize)
                };
                match found {
                    Some(found)
                        if found.start as u64 >= local_start && found.end as u64 <= local_end =>
                    {
                        let editor = tab.editor.clone();
                        editor.update(cx, |state, cx| {
                            state.set_selected_range(found.start..found.end, cx);
                            state.focus(window, cx);
                        });
                        self.find.status = tr("find.match_found");
                    }
                    _ => self.find.status = tr("find.no_matches"),
                }
                cx.notify();
                return;
            }
        }

        let engine = huge_state.engine.clone();
        let tab_id = tab.id;
        let editor = tab.editor.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let generation = self.tabs[index]
            .huge
            .as_mut()
            .map(|ui| ui.next_generation())
            .unwrap_or(0);
        if let Some(ui) = self.tabs[index].huge.as_mut() {
            ui.cancel_pending();
            ui.search = Some(SearchJob {
                generation,
                kind: SearchKind::FindNext,
                cancel: cancel.clone(),
                progress: progress.clone(),
                total: total_len,
            });
        }
        if !self.tabs[index].doc.read_only {
            editor.update(cx, |state, cx| state.set_readonly(true, cx));
        }
        self.find.status = tr("find.huge_searching");
        self.find.error = None;
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            let engine = engine.read().unwrap_or_else(|error| error.into_inner());
            engine.find(&search, from, forward, &cancel, &progress)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_huge_find(tab_id, generation, forward, result, window, cx);
            });
        })
        .detach();
    }

    fn finish_huge_find(
        &mut self,
        tab_id: u64,
        generation: u64,
        forward: bool,
        result: luban_core::Result<Option<luban_core::disk_engine::SearchHit>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let matching = self.tabs[index]
            .huge
            .as_ref()
            .and_then(|ui| ui.search.as_ref())
            .is_some_and(|job| job.generation == generation);
        if !matching {
            return;
        }
        self.tabs[index].huge.as_mut().unwrap().search = None;
        let read_only = self.tabs[index].doc.read_only;
        let editor = self.tabs[index].editor.clone();
        editor.update(cx, |state, cx| state.set_readonly(read_only, cx));
        match result {
            Ok(Some(hit)) => {
                let target_line = hit.line.saturating_sub(PREFETCH_LINES / 2).max(1);
                self.request_huge_window(
                    index,
                    WindowTarget::Line(target_line),
                    hit.start,
                    hit.line.saturating_sub(1).max(1),
                    Some((hit.start, hit.end)),
                    window,
                    cx,
                );
                self.find.current = None;
                self.find.status = tr_args("find.huge_match_at", &[("line", hit.line.to_string())]);
            }
            Ok(None) => {
                self.find.status = tr("find.no_matches");
            }
            Err(err) => {
                if matches!(err, luban_core::Error::Cancelled) {
                    self.find.status = tr("find.stopped");
                } else {
                    self.find.error = Some(err.to_string());
                }
            }
        }
        let _ = forward;
        cx.notify();
    }

    fn huge_find_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let query = self.find.query_text(cx);
        if query.is_empty() {
            self.find.status = tr("find.empty_query");
            cx.notify();
            return;
        }
        let index = self.active;
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(ui) = tab.huge.as_ref() else {
            return;
        };
        if ui.is_busy() {
            return;
        }
        let search = self.find.engine(&query);
        if let Some(error) = search.error() {
            self.find.error = Some(error.to_string());
            cx.notify();
            return;
        }
        let Some(huge_state) = tab.doc.huge() else {
            return;
        };
        let total_len = huge_state.read().total_len();
        let engine = huge_state.engine.clone();
        let tab_id = tab.id;
        let editor = tab.editor.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let generation = self.tabs[index]
            .huge
            .as_mut()
            .map(|ui| ui.next_generation())
            .unwrap_or(0);
        let limit = self.find.huge_limit;
        if let Some(ui) = self.tabs[index].huge.as_mut() {
            ui.cancel_pending();
            ui.search = Some(SearchJob {
                generation,
                kind: SearchKind::FindAll,
                cancel: cancel.clone(),
                progress: progress.clone(),
                total: total_len,
            });
        }
        if !self.tabs[index].doc.read_only {
            editor.update(cx, |state, cx| state.set_readonly(true, cx));
        }
        self.find.dirty = false;
        self.find.huge_results.clear();
        self.find.status = tr("find.huge_searching");
        self.find.error = None;
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            let engine = engine.read().unwrap_or_else(|error| error.into_inner());
            engine.count_matches(&search, limit, &cancel, &progress)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_huge_find_all(tab_id, generation, limit, result, window, cx);
            });
        })
        .detach();
    }

    fn finish_huge_find_all(
        &mut self,
        tab_id: u64,
        generation: u64,
        limit: usize,
        result: luban_core::Result<Vec<luban_core::disk_engine::SearchHit>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let matching = self.tabs[index]
            .huge
            .as_ref()
            .and_then(|ui| ui.search.as_ref())
            .is_some_and(|job| job.generation == generation);
        if !matching {
            return;
        }
        if let Some(ui) = self.tabs[index].huge.as_mut() {
            ui.search = None;
        }
        let read_only = self.tabs[index].doc.read_only;
        let editor = self.tabs[index].editor.clone();
        editor.update(cx, |state, cx| state.set_readonly(read_only, cx));
        let _ = window;
        match result {
            Ok(hits) => {
                self.find.huge_truncated = hits.len() >= limit;
                self.find.status = if self.find.huge_truncated {
                    tr_args(
                        "find.match_count_limited",
                        &[
                            ("count", hits.len().to_string()),
                            ("first", limit.to_string()),
                        ],
                    )
                } else if hits.len() == 1 {
                    tr("find.match_count.one")
                } else {
                    tr_args(
                        "find.match_count.other",
                        &[("count", hits.len().to_string())],
                    )
                };
                self.find.huge_results = hits;
            }
            Err(err) => {
                self.find.huge_results.clear();
                if matches!(err, luban_core::Error::Cancelled) {
                    self.find.status = tr("find.stopped");
                } else {
                    self.find.error = Some(err.to_string());
                }
            }
        }
        cx.notify();
    }

    fn huge_find_jump(
        &mut self,
        hit: luban_core::disk_engine::SearchHit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = self.active;
        if !self.tabs.get(index).is_some_and(|tab| tab.doc.is_huge()) {
            return;
        }
        let target_line = hit.line.saturating_sub(PREFETCH_LINES / 2).max(1);
        self.request_huge_window(
            index,
            WindowTarget::Line(target_line),
            hit.start,
            hit.line.saturating_sub(1).max(1),
            Some((hit.start, hit.end)),
            window,
            cx,
        );
        self.focus_editor = true;
        cx.notify();
    }

    fn huge_replace_one(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let query = self.find.query_text(cx);
        let replacement = self.find.replacement_text(cx);
        if query.is_empty() {
            return;
        }
        let index = self.active;
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(ui) = tab.huge.as_ref() else {
            return;
        };
        if ui.is_busy() {
            return;
        }
        let search = self.find.engine(&query);
        if let Some(error) = search.error() {
            self.find.error = Some(error.to_string());
            cx.notify();
            return;
        }
        let Some(huge_state) = tab.doc.huge() else {
            return;
        };
        let from = huge_state.cursor.max(huge_state.anchor);
        let total_len = huge_state.read().total_len();
        // Replace the selection when it is exactly one match.
        let selection = (huge_state.cursor != huge_state.anchor).then(|| {
            (
                huge_state.cursor.min(huge_state.anchor),
                huge_state.cursor.max(huge_state.anchor),
            )
        });
        if let Some((start, end)) = selection {
            if let Some(local_start) = ui.local_of_global(start) {
                if let Some(found) = search.find_first_in(&ui.last_text, local_start as usize) {
                    if found.start as u64 == local_start
                        && found.end as u64 == local_start + (end - start)
                    {
                        let matched_text = ui.last_text[found.start..found.end].to_string();
                        let expansion = search.replacement_for_bytes(
                            &matched_text,
                            ByteMatch {
                                start: 0,
                                end: matched_text.len(),
                            },
                            &replacement,
                        );
                        let mut engine = huge_state.write();
                        let cursor = start + expansion.len() as u64;
                        let applied = engine.apply_edit(start, end - start, &expansion);
                        huge_state.refresh_metadata(&engine);
                        match applied {
                            Ok(_) => {
                                drop(engine);
                                let target_line = huge_state
                                    .read()
                                    .line_at(cursor)
                                    .unwrap_or(1)
                                    .saturating_sub(PREFETCH_LINES / 2)
                                    .max(1);
                                self.request_huge_window(
                                    index,
                                    WindowTarget::Line(target_line),
                                    cursor,
                                    target_line,
                                    Some((cursor, cursor)),
                                    window,
                                    cx,
                                );
                                self.find.status = tr("find.replaced_one");
                            }
                            Err(err) => self.find.error = Some(err.to_string()),
                        }
                        cx.notify();
                        return;
                    }
                }
            }
        }
        let engine = huge_state.engine.clone();
        let tab_id = tab.id;
        let editor = tab.editor.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let generation = self.tabs[index]
            .huge
            .as_mut()
            .map(|ui| ui.next_generation())
            .unwrap_or(0);
        if let Some(ui) = self.tabs[index].huge.as_mut() {
            ui.cancel_pending();
            ui.search = Some(SearchJob {
                generation,
                kind: SearchKind::ReplaceOne,
                cancel: cancel.clone(),
                progress: progress.clone(),
                total: total_len,
            });
        }
        if !self.tabs[index].doc.read_only {
            editor.update(cx, |state, cx| state.set_readonly(true, cx));
        }
        self.find.status = tr("find.huge_searching");
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            let engine = engine.read().unwrap_or_else(|error| error.into_inner());
            engine.find(&search, from, true, &cancel, &progress)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_huge_replace_one(tab_id, generation, result, replacement, window, cx);
            });
        })
        .detach();
    }

    fn finish_huge_replace_one(
        &mut self,
        tab_id: u64,
        generation: u64,
        result: luban_core::Result<Option<luban_core::disk_engine::SearchHit>>,
        replacement: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let matching = self.tabs[index]
            .huge
            .as_ref()
            .and_then(|ui| ui.search.as_ref())
            .is_some_and(|job| job.generation == generation);
        if !matching {
            return;
        }
        self.tabs[index].huge.as_mut().unwrap().search = None;
        let read_only = self.tabs[index].doc.read_only;
        let editor = self.tabs[index].editor.clone();
        editor.update(cx, |state, cx| state.set_readonly(read_only, cx));
        let hit = match result {
            Ok(Some(hit)) => hit,
            Ok(None) => {
                self.find.status = tr("find.no_matches");
                cx.notify();
                return;
            }
            Err(err) => {
                if matches!(err, luban_core::Error::Cancelled) {
                    self.find.status = tr("find.stopped");
                } else {
                    self.find.error = Some(err.to_string());
                }
                cx.notify();
                return;
            }
        };
        let query = self.find.query_text(cx);
        let search = self.find.engine(&query);
        let matched_text = {
            let Some(huge) = self.tabs[index].doc.huge() else {
                return;
            };
            huge.read()
                .read_text(hit.start, hit.end.saturating_sub(hit.start))
        };
        let matched_text = match matched_text {
            Ok(text) => text,
            Err(err) => {
                self.find.error = Some(err.to_string());
                cx.notify();
                return;
            }
        };
        let expansion = search.replacement_for_bytes(
            &matched_text,
            ByteMatch {
                start: 0,
                end: matched_text.len(),
            },
            &replacement,
        );
        let cursor = hit.start.saturating_add(expansion.len() as u64);
        let line = {
            let Some(huge) = self.tabs[index].doc.huge() else {
                return;
            };
            let mut engine = huge.write();
            let applied =
                engine.apply_edit(hit.start, hit.end.saturating_sub(hit.start), &expansion);
            huge.refresh_metadata(&engine);
            match applied {
                Ok(_) => engine.line_at(cursor).unwrap_or(hit.line),
                Err(err) => {
                    self.find.error = Some(err.to_string());
                    cx.notify();
                    return;
                }
            }
        };
        let target_line = line.saturating_sub(PREFETCH_LINES / 2).max(1);
        self.request_huge_window(
            index,
            WindowTarget::Line(target_line),
            cursor,
            line.saturating_sub(1).max(1),
            Some((cursor, cursor)),
            window,
            cx,
        );
        self.find.status = tr("find.replaced_one");
        cx.notify();
    }

    fn huge_replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let query = self.find.query_text(cx);
        let replacement = self.find.replacement_text(cx);
        if query.is_empty() {
            return;
        }
        let index = self.active;
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(ui) = tab.huge.as_ref() else {
            return;
        };
        if ui.is_busy() {
            return;
        }
        let search = self.find.engine(&query);
        if let Some(error) = search.error() {
            self.find.error = Some(error.to_string());
            cx.notify();
            return;
        }
        let Some(huge_state) = tab.doc.huge() else {
            return;
        };
        let engine = huge_state.engine.clone();
        let cursor = huge_state.cursor;
        let tab_id = tab.id;
        let editor = tab.editor.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let generation = self.tabs[index]
            .huge
            .as_mut()
            .map(|ui| ui.next_generation())
            .unwrap_or(0);
        {
            let mut engine = engine.write().unwrap_or_else(|error| error.into_inner());
            engine.set_editing_locked(true);
        }
        editor.update(cx, |state, cx| state.set_readonly(true, cx));
        if let Some(ui) = self.tabs[index].huge.as_mut() {
            ui.cancel_pending();
            ui.search = Some(SearchJob {
                generation,
                kind: SearchKind::ReplaceAll,
                cancel: cancel.clone(),
                progress: progress.clone(),
                total: engine.read().map(|e| e.total_len()).unwrap_or(0),
            });
        }
        self.find.status = tr("find.huge_replace_all_running");
        self.find.error = None;
        cx.notify();

        let task = cx.background_executor().spawn(async move {
            let mut engine = engine.write().unwrap_or_else(|error| error.into_inner());
            engine.replace_all(&search, &replacement, &cancel, &progress)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_huge_replace_all(tab_id, generation, cursor, result, window, cx);
            });
        })
        .detach();

        // Repaint the status bar while the replacement streams.
        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let keep = this
                .update(cx, |app, cx| {
                    let Some(index) = app.tab_index_by_id(tab_id) else {
                        return false;
                    };
                    let Some(ui) = app.tabs[index].huge.as_ref() else {
                        return false;
                    };
                    let Some(job) = ui.search.as_ref() else {
                        return false;
                    };
                    if job.generation != generation {
                        return false;
                    }
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        })
        .detach();
    }

    fn finish_huge_replace_all(
        &mut self,
        tab_id: u64,
        generation: u64,
        cursor: u64,
        result: luban_core::Result<luban_core::disk_engine::ReplaceReport>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let matching = self.tabs[index]
            .huge
            .as_ref()
            .and_then(|ui| ui.search.as_ref())
            .is_some_and(|job| job.generation == generation);
        if !matching {
            return;
        }
        self.tabs[index].huge.as_mut().unwrap().search = None;
        let read_only = self.tabs[index].doc.read_only;
        {
            if let Some(huge) = self.tabs[index].doc.huge() {
                let mut engine = huge.write();
                engine.set_editing_locked(false);
                huge.refresh_metadata(&engine);
            }
        }
        let editor = self.tabs[index].editor.clone();
        editor.update(cx, |state, cx| state.set_readonly(read_only, cx));
        match result {
            Ok(report) => {
                if report.cancelled {
                    self.find.status = tr("find.stopped");
                } else {
                    self.find.status = tr_args(
                        "find.replaced_other",
                        &[("count", report.count.to_string())],
                    );
                }
                // The huge result list is separate from `matches`; leaving
                // dirty set would overwrite the count with the hint.
                self.find.dirty = false;
                let line = {
                    let Some(huge) = self.tabs[index].doc.huge() else {
                        cx.notify();
                        return;
                    };
                    huge.read().line_at(cursor).unwrap_or(1)
                };
                let target_line = line.saturating_sub(PREFETCH_LINES / 2).max(1);
                self.request_huge_window(
                    index,
                    WindowTarget::Line(target_line),
                    cursor,
                    line.saturating_sub(1).max(1),
                    Some((cursor, cursor)),
                    window,
                    cx,
                );
            }
            Err(err) => {
                if matches!(err, luban_core::Error::Cancelled) {
                    self.find.status = tr("find.stopped");
                } else {
                    self.find.error = Some(err.to_string());
                }
            }
        }
        cx.notify();
    }

    fn cancel_tab_jobs(&self, index: usize) {
        if let Some(ui) = self.tabs.get(index).and_then(|tab| tab.huge.as_ref()) {
            if let Some(job) = &ui.save {
                job.cancel.store(true, Ordering::Relaxed);
            }
            if let Some(job) = &ui.search {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    fn cancel_huge_search(&mut self) {
        for tab in &self.tabs {
            if let Some(job) = tab.huge.as_ref().and_then(|ui| ui.search.as_ref()) {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Stop background disk-engine index scans.
    fn cancel_huge_opens(&mut self, cx: &mut Context<Self>) -> bool {
        let mut cancelled = false;
        for job in &self.large_loads {
            if job.huge {
                job.cancel.store(true, Ordering::Relaxed);
                cancelled = true;
            }
        }
        if cancelled {
            self.set_status(tr("lfc.stopped"), cx);
        }
        cancelled
    }

    /// Stop a running disk-engine save and any in-flight full-file search.
    pub(crate) fn cancel_huge_save(&mut self, cx: &mut Context<Self>) {
        let mut save_cancelled = false;
        let mut search_cancelled = false;
        for tab in &self.tabs {
            if let Some(ui) = &tab.huge {
                if let Some(job) = &ui.save {
                    job.cancel.store(true, Ordering::Relaxed);
                    save_cancelled = true;
                }
                if let Some(job) = &ui.search {
                    job.cancel.store(true, Ordering::Relaxed);
                    search_cancelled = true;
                }
            }
        }
        if save_cancelled {
            self.set_status(tr("status.save_cancelled"), cx);
        } else if search_cancelled {
            self.set_status(tr("find.stopped"), cx);
        }
    }

    /// Start a background save. `then` continues a close/exit flow after the
    /// file has been replaced.
    fn start_huge_save(
        &mut self,
        index: usize,
        target: Option<PathBuf>,
        then: SaveThen,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.tabs.len() || !self.tabs[index].doc.is_huge() {
            return;
        }
        self.sync_active(window, cx);
        if self.tabs[index]
            .huge
            .as_ref()
            .is_some_and(|ui| ui.save.is_some())
        {
            // The running save already contains every edit (editing is
            // locked); remember what the caller wanted to do afterwards.
            if let Some(ui) = self.tabs[index].huge.as_mut() {
                ui.queued_save_then = Some(then);
            }
            return;
        }
        if self.tabs[index]
            .huge
            .as_ref()
            .is_some_and(|ui| ui.search.is_some())
        {
            self.set_status(tr("status.huge_busy"), cx);
            return;
        }
        let Some(path) = self.tabs[index].doc.path.clone() else {
            return;
        };
        let save_to_source = target.is_none();
        if save_to_source && self.tabs[index].doc.is_partial() {
            // A range view never writes back to its source.
            self.set_status(tr("status.save_partial_as"), cx);
            return;
        }
        if save_to_source && self.tabs[index].doc.read_only {
            self.set_status(tr("status.document_read_only"), cx);
            return;
        }
        let target = target.unwrap_or(path);
        if !force && self.tabs[index].doc.changed_on_disk() {
            self.pending_huge_overwrite = Some((self.tabs[index].id, Some(target.clone()), then));
            let name = self.tabs[index].doc.display_name();
            self.confirm = Some(ConfirmState::new(
                CloseIntent::OverwriteExternal(self.tabs[index].id),
                tr_args("confirm.overwrite_external_title", &[("name", name)]),
                tr("confirm.overwrite_external_detail"),
            ));
            cx.notify();
            return;
        }
        // The save writes a complete copy next to the target; refuse when the
        // filesystem cannot hold it.
        let directory = target
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let (engine_arc, snapshot, estimated) = {
            let Some(huge) = self.tabs[index].doc.huge() else {
                return;
            };
            let engine = huge.read();
            (
                huge.engine.clone(),
                engine.snapshot(),
                engine.estimated_save_size(),
            )
        };
        if let Some(free) = crate::large_file::available_space(&directory) {
            if free < estimated.saturating_add(16 * 1024 * 1024) {
                self.set_status(
                    tr_args(
                        "status.huge_no_space",
                        &[
                            (
                                "needed",
                                format_bytes(estimated, self.settings.large_file_display_unit),
                            ),
                            (
                                "free",
                                format_bytes(free, self.settings.large_file_display_unit),
                            ),
                        ],
                    ),
                    cx,
                );
                return;
            }
        }
        let restore_read_only = self.tabs[index].doc.read_only;
        let should_lock_editor = !restore_read_only;
        let tab_id = self.tabs[index].id;
        let editor = self.tabs[index].editor.clone();
        if let Some(huge) = self.tabs[index].doc.huge() {
            let mut engine = huge.write();
            engine.set_editing_locked(true);
        }
        if should_lock_editor {
            editor.update(cx, |state, cx| state.set_readonly(true, cx));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let total = self.tabs[index]
            .doc
            .huge()
            .map(|huge| huge.read().estimated_save_size())
            .unwrap_or(0);
        let generation = self.next_load_generation;
        self.next_load_generation = self.next_load_generation.wrapping_add(1);
        if let Some(ui) = self.tabs[index].huge.as_mut() {
            ui.cancel_pending();
            ui.save = Some(SaveJob {
                generation,
                then,
                cancel: cancel.clone(),
                progress: progress.clone(),
                total,
                restore_read_only,
            });
        }
        self.set_status(tr("status.huge_saving"), cx);
        let task_target = target.clone();
        let task = cx
            .background_executor()
            .spawn(async move { snapshot.write(&task_target, force, &progress, &cancel) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_huge_save(tab_id, generation, target, result, window, cx);
            });
        })
        .detach();
        let _ = engine_arc;

        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let keep = this
                .update(cx, |app, cx| {
                    let Some(index) = app.tab_index_by_id(tab_id) else {
                        return false;
                    };
                    let Some(ui) = app.tabs[index].huge.as_ref() else {
                        return false;
                    };
                    let Some(job) = ui.save.as_ref() else {
                        return false;
                    };
                    if job.generation != generation {
                        return false;
                    }
                    let total = job.total.max(1);
                    let percent =
                        (job.progress.load(Ordering::Relaxed).saturating_mul(100) / total).min(100);
                    app.status = tr_args(
                        "status.huge_saving_percent",
                        &[("percent", percent.to_string())],
                    );
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        })
        .detach();
        cx.notify();
    }

    fn finish_huge_save(
        &mut self,
        tab_id: u64,
        generation: u64,
        target: PathBuf,
        result: luban_core::Result<luban_core::disk_engine::SaveReport>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let matching = self.tabs[index]
            .huge
            .as_ref()
            .and_then(|ui| ui.save.as_ref())
            .is_some_and(|job| job.generation == generation);
        if !matching {
            return;
        }
        let job = self.tabs[index].huge.as_mut().and_then(|ui| ui.save.take());
        let Some(job) = job else {
            return;
        };
        let restore_read_only = job.restore_read_only;
        let was_partial = self.tabs[index].doc.is_partial();
        match result {
            Ok(report) => {
                let lossy = report.lossy();
                let adopted = {
                    if let Some(huge) = self.tabs[index].doc.huge() {
                        let mut engine = huge.write();
                        let result = engine.adopt_saved(report, target.clone());
                        if result.is_err() {
                            engine.set_editing_locked(false);
                        }
                        huge.refresh_metadata(&engine);
                        result
                    } else {
                        Ok(())
                    }
                };
                if let Err(err) = adopted {
                    self.abort_pending_saves();
                    let editor = self.tabs[index].editor.clone();
                    editor.update(cx, |state, cx| state.set_readonly(restore_read_only, cx));
                    self.set_status(err.to_string(), cx);
                    return;
                }
                let tab = &mut self.tabs[index];
                tab.doc.path = Some(target.clone());
                if let Ok(metadata) = fs::metadata(&target) {
                    tab.doc.file_size = metadata.len();
                    tab.doc.last_modified = metadata.modified().ok();
                    tab.doc.read_only = metadata.permissions().readonly();
                }
                tab.external = None;
                self.watch
                    .note_saved(&target, tab.doc.file_size, tab.doc.last_modified);
                // Saving a range view as a new file turns the tab into a
                // whole-file engine over the target; the partial marker must
                // not survive the transition.
                if was_partial {
                    tab.doc.set_partial_range(None);
                }
                let read_only = tab.doc.read_only;
                let tab_name = tab.doc.display_name();
                let tab_editor = tab.editor.clone();
                if let Some(ui) = tab.huge.as_mut() {
                    ui.history_trimmed_notified = false;
                }
                tab_editor.update(cx, |state, cx| state.set_readonly(read_only, cx));
                if was_partial {
                    let window_text = self.tabs[index]
                        .doc
                        .huge()
                        .and_then(|huge| huge.read().read_window(0, WINDOW_BYTES).ok());
                    if let Some(window_text) = window_text {
                        let line = window_text.line;
                        let start = window_text.start;
                        let text = window_text.text.clone();
                        let editor = self.tabs[index].editor.clone();
                        editor.update(cx, |state, cx| {
                            state.set_value(text, window, cx);
                            state.set_selected_range(0..0, cx);
                            state.set_scroll_offset(point(px(0.), px(0.)), cx);
                            state.focus(window, cx);
                        });
                        if let Some(huge) = self.tabs[index].doc.huge_mut() {
                            huge.cursor = start;
                            huge.anchor = start;
                            huge.scroll_line = line;
                            huge.cursor_line = line;
                            huge.cursor_column = 1;
                            huge.initial_window = None;
                            let guard = huge.read();
                            huge.refresh_metadata(&guard);
                        }
                        if let Some(ui) = self.tabs[index].huge.as_mut() {
                            ui.set_window(window_text);
                            ui.scroll_top_line = line;
                            ui.scroll_target = None;
                            ui.queued_window = None;
                            ui.offscreen_cursor = None;
                        }
                        self.tabs[index].needs_sync = false;
                    }
                }
                if self
                    .large_file
                    .target
                    .as_ref()
                    .is_some_and(|panel| util::same_path(&panel.path, &target))
                {
                    let size = self.tabs[index].doc.source_size();
                    let selection = luban_core::large_file::RangeSelection::new(
                        size,
                        self.settings.large_file_partial_size_bytes(),
                    );
                    self.large_file.set_selection(selection, size, window, cx);
                    self.large_file.loaded = size;
                    self.large_file.bound_tab = Some(tab_id);
                }
                self.settings.add_recent(target.clone());
                self.commit_settings();
                if lossy {
                    let encoding = self.tabs[index].doc.encoding;
                    self.set_status(
                        tr_args(
                            "status.saved_lossy",
                            &[("name", tab_name), ("encoding", encoding.label())],
                        ),
                        cx,
                    );
                } else {
                    self.set_status(
                        tr_args(
                            "status.saved_path",
                            &[("path", target.display().to_string())],
                        ),
                        cx,
                    );
                }
                self.finish_save_flow(job.then, window, cx);
                let queued = self.tabs[index]
                    .huge
                    .as_mut()
                    .and_then(|ui| ui.queued_save_then.take());
                if let Some(queued) = queued {
                    self.finish_save_flow(queued, window, cx);
                }
            }
            Err(err) => {
                if let Some(huge) = self.tabs[index].doc.huge() {
                    let mut engine = huge.write();
                    engine.set_editing_locked(false);
                }
                let editor = self.tabs[index].editor.clone();
                editor.update(cx, |state, cx| state.set_readonly(restore_read_only, cx));
                let cancelled = matches!(err, luban_core::Error::Cancelled);
                let name = self.tabs[index].doc.display_name();
                let queued_then = self.tabs[index]
                    .huge
                    .as_ref()
                    .and_then(|ui| ui.queued_save_then);
                if cancelled {
                    self.set_status(tr("status.save_cancelled"), cx);
                } else {
                    self.pending_save_failures.push(format!("{name}: {err}"));
                    self.set_status(
                        tr_args("status.save_failed", &[("error", err.to_string())]),
                        cx,
                    );
                }
                if let Some(ui) = self.tabs[index].huge.as_mut() {
                    ui.queued_save_then = None;
                }
                // Save All keeps going after a failure and reports the failed
                // tabs at the end. Close All / Exit must stop so they never
                // close the remaining tabs with an unresolved failure.
                let save_all_continues = !cancelled
                    && (job.then == SaveThen::NextHugeSave
                        || queued_then == Some(SaveThen::NextHugeSave))
                    && !self.pending_huge_all
                    && !self.pending_huge_exit;
                if save_all_continues {
                    self.finish_save_flow(SaveThen::NextHugeSave, window, cx);
                } else {
                    self.abort_pending_saves();
                }
            }
        }
        cx.notify();
    }

    fn finish_save_flow(&mut self, then: SaveThen, window: &mut Window, cx: &mut Context<Self>) {
        match then {
            SaveThen::Nothing => {
                // A standalone save must never drain a paused batch: doing so
                // could close tabs or quit with an unresolved conflict.
                self.focus_editor(window, cx);
            }
            SaveThen::NextHugeSave => {
                if self.pending_huge_saves.is_empty() {
                    // An in-flight save from an earlier batch can finish while
                    // a new batch is waiting on an external-change dialog.
                    // Never close or quit until that dialog is resolved.
                    if !self.batch_can_finalize() {
                        return;
                    }
                    if self.unsaved_blocks_close(cx) {
                        return;
                    }
                    if self.pending_huge_exit {
                        self.pending_huge_exit = false;
                        if self.pending_exit_saves.is_empty() {
                            self.request_quit(window, cx);
                        } else {
                            self.prompt_next_batch_save(window, cx);
                        }
                    } else if self.pending_huge_all {
                        self.pending_huge_all = false;
                        if self.pending_close_saves.is_empty() {
                            self.close_all_now(window, cx);
                        } else {
                            self.prompt_next_batch_save(window, cx);
                        }
                    } else {
                        self.report_save_all_result(cx);
                    }
                } else {
                    self.save_next_huge(window, cx);
                }
            }
            SaveThen::NextNormalSave => {
                self.save_next_normal(window, cx);
            }
            SaveThen::NextTrimSave => {
                self.save_next_trim(window, cx);
            }
            SaveThen::ReloadTab(id) => {
                if let Some(index) = self.tab_index_by_id(id) {
                    if self.tabs[index].doc.is_huge() {
                        self.start_huge_reload(index, window, cx);
                    } else {
                        self.reload_tab_now(index, window, cx);
                    }
                }
            }
            SaveThen::CloseTab(id) => {
                if let Some(index) = self.tab_index_by_id(id) {
                    self.close_tab_at(index, window, cx);
                }
            }
            SaveThen::Exit => {
                if self.pending_exit_saves.is_empty() {
                    if self.unsaved_blocks_close(cx) {
                        return;
                    }
                    self.request_quit(window, cx);
                } else {
                    self.prompt_next_batch_save(window, cx);
                }
            }
            SaveThen::CloseAll => {
                if self.pending_close_saves.is_empty() {
                    if self.unsaved_blocks_close(cx) {
                        return;
                    }
                    self.close_all_now(window, cx);
                } else {
                    self.prompt_next_batch_save(window, cx);
                }
            }
        }
    }

    fn save_next_huge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while let Some(id) = self.pending_huge_saves.first().copied() {
            self.pending_huge_saves.remove(0);
            let Some(index) = self.tab_index_by_id(id) else {
                continue;
            };
            if !self.tabs[index].doc.is_modified() {
                continue;
            }
            self.start_huge_save(index, None, SaveThen::NextHugeSave, false, window, cx);
            return;
        }
        if !self.batch_can_finalize() {
            return;
        }
        if self.unsaved_blocks_close(cx) {
            return;
        }
        if self.pending_huge_exit {
            self.pending_huge_exit = false;
            if self.pending_exit_saves.is_empty() {
                self.request_quit(window, cx);
            } else {
                self.prompt_next_batch_save(window, cx);
            }
        } else if self.pending_huge_all {
            self.pending_huge_all = false;
            if self.pending_close_saves.is_empty() {
                self.close_all_now(window, cx);
            } else {
                self.prompt_next_batch_save(window, cx);
            }
        } else {
            self.report_save_all_result(cx);
        }
    }

    /// Reload a disk-engine tab. The engine is replaced by a freshly indexed
    /// one after the background scan completes.
    fn start_huge_reload(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(path) = tab.doc.path.clone() else {
            return;
        };
        let Some(huge) = tab.doc.huge() else {
            return;
        };
        let (line, column) = {
            let engine = huge.read();
            engine.line_col_at(huge.cursor).unwrap_or((1, 1))
        };
        let range = tab
            .doc
            .partial_range()
            .map(|range| (range.start, range.end));
        let tab_id = tab.id;
        let editor = tab.editor.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        if let Some(ui) = self.tabs[index].huge.as_mut() {
            ui.cancel_pending();
            ui.search = Some(SearchJob {
                generation: 0,
                kind: SearchKind::FindNext,
                cancel: cancel.clone(),
                progress: progress.clone(),
                total: range
                    .map(|(from, to)| to.saturating_sub(from))
                    .unwrap_or_else(|| path.metadata().map(|meta| meta.len()).unwrap_or(0)),
            });
        }
        editor.update(cx, |state, cx| state.set_readonly(true, cx));
        self.set_status(tr("status.huge_reloading"), cx);
        let task_path = path.clone();
        let task_cancel = cancel.clone();
        let task_progress = progress.clone();
        let task = cx.background_executor().spawn(async move {
            let engine = match range {
                Some((from, to)) => {
                    DiskEngine::open_range(&task_path, from, to, &task_cancel, &task_progress)?
                }
                None => DiskEngine::open(&task_path, &task_cancel, &task_progress)?,
            };
            let offset = engine.offset_for_line(line, column).unwrap_or(0);
            let window = engine.read_window(offset, WINDOW_BYTES)?;
            Ok::<_, luban_core::Error>((engine, window, offset))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_huge_reload(tab_id, path, result, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_huge_reload(
        &mut self,
        tab_id: u64,
        path: PathBuf,
        result: luban_core::Result<(DiskEngine, WindowText, u64)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        if let Some(ui) = self.tabs[index].huge.as_mut() {
            ui.search = None;
        }
        match result {
            Ok((engine, loaded, offset)) => {
                let encoding = engine.encoding();
                let eol = engine.eol();
                let read_only = engine.is_read_only();
                let size = engine.source_size();
                let range = engine.range();
                let last_modified = engine.last_modified();
                let total_chars = engine.total_chars();
                let (cursor_line, cursor_column) =
                    engine.line_col_at(offset).unwrap_or((loaded.line, 1));
                let cursor_local = offset.saturating_sub(loaded.start) as usize;
                if let Some(huge) = self.tabs[index].doc.huge_mut() {
                    huge.engine = Arc::new(std::sync::RwLock::new(engine));
                    huge.cursor = offset;
                    huge.anchor = offset;
                    huge.scroll_line = loaded.line;
                    huge.cursor_line = cursor_line;
                    huge.cursor_column = cursor_column;
                    huge.initial_window = None;
                    let guard = huge.read();
                    huge.refresh_metadata(&guard);
                }
                {
                    let tab = &mut self.tabs[index];
                    tab.doc.encoding = encoding;
                    tab.doc.eol = eol;
                    tab.doc.read_only = read_only;
                    tab.doc.file_size = size;
                    tab.doc.last_modified = last_modified;
                    tab.external = None;
                    self.watch.note_saved(&path, size, last_modified);
                    tab.doc.set_partial_range(range.map(|range| PartialRange {
                        start: range.start,
                        end: range.end,
                        total: size,
                    }));
                    let _ = total_chars;
                    let editor = tab.editor.clone();
                    let text = loaded.text.clone();
                    editor.update(cx, |state, cx| {
                        state.set_value(text, window, cx);
                        let total = state.text().len();
                        let cursor = cursor_local.min(total);
                        state.set_selected_range(cursor..cursor, cx);
                        state.set_readonly(read_only, cx);
                        state.focus(window, cx);
                    });
                    if let Some(ui) = tab.huge.as_mut() {
                        ui.set_window(loaded.clone());
                        ui.scroll_top_line = loaded.line;
                        ui.scroll_target = None;
                        ui.queued_window = None;
                        ui.history_trimmed_notified = false;
                    }
                }
                self.tabs[index].needs_sync = false;
                let _ = path;
                self.set_status(tr("status.reloaded"), cx);
            }
            Err(err) => {
                let read_only = self.tabs[index].doc.read_only;
                let editor = self.tabs[index].editor.clone();
                editor.update(cx, |state, cx| state.set_readonly(read_only, cx));
                self.set_status(
                    tr_args("status.reload_failed", &[("error", err.to_string())]),
                    cx,
                );
            }
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // File operations
    // ------------------------------------------------------------------

    pub fn save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let index = self.active;
        let Some(tab) = self.active_tab() else { return };
        // Range views are engine documents too, but they must never write back
        // to their source; Ctrl+S opens the save-as flow.
        if tab.doc.is_partial() {
            self.save_active_as(window, cx);
            return;
        }
        if tab.doc.is_huge() {
            self.start_huge_save(index, None, SaveThen::Nothing, false, window, cx);
            return;
        }
        if tab.doc.path.is_none() {
            self.save_active_as(window, cx);
            return;
        }
        self.save_tab_at(index, window, cx);
    }

    fn save_tab_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab_at_with(index, SaveThen::Nothing, false, window, cx);
    }

    /// Save a tab, optionally resuming a close/exit flow afterwards.
    ///
    /// Unless `force` is set, a file whose size or modification time changed
    /// since it was opened shows an Overwrite/Reload/Cancel confirmation
    /// instead of being overwritten silently.
    fn save_tab_at_with(
        &mut self,
        index: usize,
        then: SaveThen,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs[index].doc.is_partial() {
            self.set_status(tr("status.save_partial_as"), cx);
            return;
        }
        if self.tabs[index].doc.is_huge() {
            self.start_huge_save(index, None, then, force, window, cx);
            return;
        }
        let path = self.tabs[index].doc.path.clone();
        if !force && path.is_some() {
            let status = self.tabs[index].doc.disk_status();
            if matches!(status, DiskStatus::Changed | DiskStatus::Missing) {
                let id = self.tabs[index].id;
                let name = self.tabs[index].doc.display_name();
                self.pending_normal_overwrite = Some((id, path, then));
                self.confirm = Some(ConfirmState::new(
                    CloseIntent::SaveConflict(id),
                    tr_args("confirm.overwrite_external_title", &[("name", name)]),
                    tr("confirm.overwrite_external_detail"),
                ));
                cx.notify();
                return;
            }
        }
        if !self.ensure_tab_text(index, cx) {
            self.abort_pending_saves();
            return;
        }
        let large_file = self.tabs[index].large_file;
        let non_text = self.tabs[index].doc.is_non_text();
        if self.settings.trim_trailing_whitespace_on_save && !large_file && !non_text {
            let tab = &self.tabs[index];
            let trimmed = textutil::trim_trailing_whitespace(&tab.doc.text);
            if trimmed != tab.doc.text {
                let cursor = tab.doc.cursor.min(textutil::char_count(&trimmed));
                let anchor = tab.doc.anchor.min(textutil::char_count(&trimmed));
                let b0 = textutil::char_to_byte(&trimmed, anchor);
                let b1 = textutil::char_to_byte(&trimmed, cursor);
                let tab = &mut self.tabs[index];
                tab.doc.replace_all_text(&trimmed, EditKind::Other);
                tab.editor.update(cx, |state, cx| {
                    state.replace_all(trimmed, window, cx);
                    state.set_selected_range(b0..b1, cx);
                });
                tab.needs_sync = false;
            }
        }
        if let Some(target) = path.as_deref() {
            if !non_text {
                self.snapshot_before_save(target, cx);
            }
        }
        let (name, encoding, saved, path, error) = {
            let tab = &mut self.tabs[index];
            let name = tab.doc.display_name();
            let encoding = tab.doc.encoding;
            match tab.doc.save() {
                Ok(lossy) => {
                    tab.highlighted_revision = u64::MAX;
                    (name, encoding, Some(lossy), tab.doc.path.clone(), None)
                }
                Err(err) => (name, encoding, None, None, Some(err.to_string())),
            }
        };
        self.release_tab_text(index);
        if let Some(path) = path {
            self.settings.add_recent(path.clone());
            if let Some(tab) = self.tabs.get_mut(index) {
                tab.external = None;
                self.watch
                    .note_saved(&path, tab.doc.file_size, tab.doc.last_modified);
            }
        }
        self.commit_settings();
        match (saved, error) {
            (Some(lossy), _) if lossy => self.set_status(
                tr_args(
                    "status.saved_lossy",
                    &[("name", name), ("encoding", encoding.label())],
                ),
                cx,
            ),
            (Some(_), _) => self.set_status(tr_args("status.saved", &[("name", name)]), cx),
            (None, Some(err)) => {
                if self.pending_normal_all {
                    // Save All continues with the remaining tabs and reports
                    // the failures at the end, matching the existing flow.
                    self.pending_save_failures.push(format!("{name}: {err}"));
                    self.set_status(tr_args("status.save_failed", &[("error", err)]), cx);
                    self.save_next_normal(window, cx);
                } else {
                    // Close All / Exit must stop: continuing could close tabs
                    // while this document is still unsaved.
                    self.abort_pending_saves();
                    self.set_status(tr_args("status.save_failed", &[("error", err)]), cx);
                }
            }
            (None, None) => {}
        }
        if saved.is_some() && then != SaveThen::Nothing {
            self.finish_save_flow(then, window, cx);
        }
        cx.notify();
    }

    pub fn save_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        self.start_batch_save(BatchSaveKind::SaveAll, window, cx);
    }

    /// Queue every modified tab for a Save All / Close All / Exit flow.
    ///
    /// Normal documents with paths are saved one at a time. A save conflict
    /// pauses the batch instead of letting the caller close the tab or quit,
    /// and only an explicit Overwrite/Save resumes it.
    fn start_batch_save(
        &mut self,
        kind: BatchSaveKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.abort_pending_saves();
        self.pending_batch_is_close = matches!(kind, BatchSaveKind::CloseAll | BatchSaveKind::Exit);
        let ids = self.modified_tabs();
        let mut huge = Vec::new();
        for id in ids {
            let Some(index) = self.tab_index_by_id(id) else {
                continue;
            };
            let tab = &self.tabs[index];
            if tab.doc.is_partial() {
                match kind {
                    BatchSaveKind::SaveAll => self.pending_normal_skipped_partial = true,
                    BatchSaveKind::CloseAll => {
                        self.pending_close_saves.push(id);
                        self.pending_close_all_after_saves = true;
                    }
                    BatchSaveKind::Exit => self.pending_exit_saves.push(id),
                }
                continue;
            }
            if tab.doc.is_huge() {
                huge.push(id);
                continue;
            }
            if tab.doc.path.is_some() {
                self.pending_normal_saves.push(id);
            } else {
                match kind {
                    BatchSaveKind::SaveAll => {}
                    BatchSaveKind::CloseAll => {
                        self.pending_close_saves.push(id);
                        self.pending_close_all_after_saves = true;
                    }
                    BatchSaveKind::Exit => self.pending_exit_saves.push(id),
                }
            }
        }
        match kind {
            BatchSaveKind::SaveAll => self.pending_normal_all = true,
            BatchSaveKind::CloseAll => self.pending_normal_close_all = true,
            BatchSaveKind::Exit => self.pending_normal_exit = true,
        }
        if !huge.is_empty() {
            self.pending_huge_saves = huge;
            match kind {
                BatchSaveKind::SaveAll => {
                    self.pending_huge_all = false;
                    self.pending_huge_exit = false;
                }
                BatchSaveKind::CloseAll => self.pending_huge_all = true,
                BatchSaveKind::Exit => self.pending_huge_exit = true,
            }
        }
        self.save_next_normal(window, cx);
    }

    /// Save the next queued normal document, or finish the batch.
    fn save_next_normal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while !self.pending_normal_saves.is_empty() {
            let id = self.pending_normal_saves.remove(0);
            let Some(index) = self.tab_index_by_id(id) else {
                continue;
            };
            let tab = &self.tabs[index];
            if !tab.doc.is_modified() || tab.doc.path.is_none() || tab.doc.is_partial() {
                continue;
            }
            self.save_tab_at_with(index, SaveThen::NextNormalSave, false, window, cx);
            return;
        }
        self.finish_normal_batch(window, cx);
    }

    fn finish_normal_batch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_normal_all {
            self.pending_normal_all = false;
            if !self.pending_huge_saves.is_empty() {
                self.save_next_huge(window, cx);
            } else if self.pending_save_failures.is_empty() && self.pending_normal_skipped_partial {
                self.pending_normal_skipped_partial = false;
                self.set_status(tr("status.save_partial_as"), cx);
            } else {
                self.pending_normal_skipped_partial = false;
                self.report_save_all_result(cx);
            }
            return;
        }
        if self.pending_normal_close_all {
            self.pending_normal_close_all = false;
            if !self.pending_huge_saves.is_empty() {
                self.save_next_huge(window, cx);
            } else if self.pending_close_saves.is_empty() {
                if self.unsaved_blocks_close(cx) {
                    return;
                }
                self.close_all_now(window, cx);
            } else {
                self.prompt_next_batch_save(window, cx);
            }
            return;
        }
        if self.pending_normal_exit {
            self.pending_normal_exit = false;
            if !self.pending_huge_saves.is_empty() {
                self.save_next_huge(window, cx);
            } else if self.pending_exit_saves.is_empty() {
                if self.unsaved_blocks_close(cx) {
                    return;
                }
                self.request_quit(window, cx);
            } else {
                self.prompt_next_batch_save(window, cx);
            }
        }
    }

    /// A disk-engine save job is still writing a tab.
    fn huge_save_in_flight(&self) -> bool {
        self.tabs
            .iter()
            .any(|tab| tab.huge.as_ref().is_some_and(|ui| ui.save.is_some()))
    }

    fn has_unsaved_tabs(&self) -> bool {
        self.tabs
            .iter()
            .any(|tab| tab.doc.is_modified() || tab.pending_edit)
    }

    /// Refuse to close or quit when a document was edited after the batch
    /// started. The user must retry so the new edits enter the queue.
    fn unsaved_blocks_close(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.pending_batch_is_close || !self.has_unsaved_tabs() {
            return false;
        }
        self.abort_pending_saves();
        self.set_status(tr("status.batch_unsaved"), cx);
        true
    }

    /// Whether no batch is waiting on a user decision, an unsaved normal tab,
    /// or an in-flight disk-engine save.
    fn batch_can_finalize(&self) -> bool {
        self.confirm.is_none()
            && self.pending_normal_overwrite.is_none()
            && self.pending_huge_overwrite.is_none()
            && !self.pending_normal_all
            && !self.pending_normal_close_all
            && !self.pending_normal_exit
            && self.pending_normal_saves.is_empty()
            && !self.huge_save_in_flight()
    }

    fn report_save_all_result(&mut self, cx: &mut Context<Self>) {
        if self.pending_save_failures.is_empty() {
            self.set_status(tr("status.saved_modified"), cx);
            return;
        }
        let count = self.pending_save_failures.len();
        self.pending_save_failures.clear();
        self.set_status(
            tr_args("status.save_all_failed", &[("count", count.to_string())]),
            cx,
        );
    }

    /// Drop every queued save/close/exit continuation. Used when the user
    /// cancels a confirmation, so no background flow closes tabs or quits.
    fn abort_pending_saves(&mut self) {
        self.confirm = None;
        self.pending_normal_saves.clear();
        self.pending_normal_all = false;
        self.pending_normal_close_all = false;
        self.pending_normal_exit = false;
        self.pending_normal_skipped_partial = false;
        self.pending_exit_saves.clear();
        self.pending_close_saves.clear();
        self.pending_close_all_after_saves = false;
        self.pending_huge_saves.clear();
        self.pending_huge_all = false;
        self.pending_huge_exit = false;
        self.pending_huge_overwrite = None;
        self.pending_normal_overwrite = None;
        self.pending_save_failures.clear();
        self.pending_batch_is_close = false;
        self.trim_plan = None;
    }

    pub fn save_active_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else { return };
        let tab_id = tab.id;
        let dir = util::default_save_directory(tab.doc.path.as_deref());
        let suggested = if tab.doc.is_partial() {
            tab.doc
                .path
                .as_deref()
                .map(luban_core::large_file::partial_save_name)
                .unwrap_or_else(|| tab.doc.display_name())
        } else {
            tab.doc.display_name()
        };
        self.show_file_dialog(
            FileDialogMode::Save,
            FileDialogPurpose::SaveAs {
                tab_id,
                then: SaveThen::Nothing,
            },
            dir,
            suggested,
            window,
            cx,
        );
    }

    fn language_override(&self) -> Option<&LanguageSettings> {
        self.settings
            .language_settings
            .get(&self.preferences.language)
    }

    fn update_language_override(&mut self, update: impl FnOnce(&mut LanguageSettings)) {
        let language = self.preferences.language.clone();
        let mut entry = self
            .settings
            .language_settings
            .get(&language)
            .cloned()
            .unwrap_or_default();
        update(&mut entry);
        if entry.is_empty() {
            self.settings.language_settings.remove(&language);
        } else {
            self.settings.language_settings.insert(language, entry);
        }
        self.commit_settings();
    }

    fn apply_language_indent_to_open_tabs(&mut self, cx: &mut Context<Self>) {
        let language = self.preferences.language.clone();
        let tab_size = self.settings.tab_size_for(&language);
        let use_spaces = self.settings.use_spaces_for(&language);
        for tab in &self.tabs {
            if tab.doc.language != language
                || !Self::language_indent_applies(&tab.doc, tab.large_file)
            {
                continue;
            }
            tab.editor.update(cx, |state, cx| {
                state.set_tab_size(
                    TabSize {
                        tab_size,
                        hard_tabs: !use_spaces,
                    },
                    cx,
                );
            });
        }
    }

    fn set_language_use_spaces(&mut self, value: Option<bool>, cx: &mut Context<Self>) {
        self.update_language_override(|entry| entry.use_spaces = value);
        self.apply_language_indent_to_open_tabs(cx);
        cx.notify();
    }

    fn set_language_eol(&mut self, value: Option<Eol>, cx: &mut Context<Self>) {
        self.update_language_override(|entry| entry.eol = value);
        cx.notify();
    }

    fn apply_language_tab_size(&mut self, cx: &mut Context<Self>) {
        let value = self
            .preferences
            .language_tab_size
            .read(cx)
            .value()
            .trim()
            .to_string();
        if value.is_empty() {
            self.update_language_override(|entry| entry.tab_size = None);
        } else {
            match value.parse::<usize>() {
                Ok(size) if (1..=16).contains(&size) => {
                    self.update_language_override(|entry| entry.tab_size = Some(size));
                }
                _ => {
                    self.set_status(tr("prefs.language_tab_invalid"), cx);
                    return;
                }
            }
        }
        self.apply_language_indent_to_open_tabs(cx);
        self.set_status(tr("prefs.language_saved"), cx);
        cx.notify();
    }

    fn import_syntax_theme(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        match syntax_theme::load_custom(path) {
            Ok(_) => {
                let previous = self.settings.syntax_theme.clone();
                let previous_path = self.settings.syntax_theme_path.clone();
                self.settings.syntax_theme = syntax_theme::CUSTOM_ID.to_string();
                self.settings.syntax_theme_path = Some(util::absolutize(path));
                if let Err(error) = syntax_theme::apply(&self.settings, cx) {
                    self.settings.syntax_theme = previous;
                    self.settings.syntax_theme_path = previous_path;
                    self.set_status(
                        tr_args("status.theme_import_failed", &[("error", error)]),
                        cx,
                    );
                    return;
                }
                self.commit_settings();
                self.set_status(
                    tr_args(
                        "status.theme_imported",
                        &[("path", path.display().to_string())],
                    ),
                    cx,
                );
                self.focus_editor(window, cx);
            }
            Err(error) => self.set_status(
                tr_args("status.theme_import_failed", &[("error", error)]),
                cx,
            ),
        }
        cx.notify();
    }

    fn open_syntax_theme_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = dirs::home_dir()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.show_file_dialog(
            FileDialogMode::Open,
            FileDialogPurpose::ImportTheme,
            dir,
            String::new(),
            window,
            cx,
        );
    }

    fn set_syntax_theme(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self.settings.syntax_theme.clone();
        let previous_path = self.settings.syntax_theme_path.clone();
        self.settings.syntax_theme = id.to_string();
        match syntax_theme::apply(&self.settings, cx) {
            Ok(()) => {
                self.commit_settings();
                self.set_status(tr("status.theme_applied"), cx);
            }
            Err(error) => {
                self.settings.syntax_theme = previous;
                self.settings.syntax_theme_path = previous_path;
                self.set_status(
                    tr_args("status.theme_import_failed", &[("error", error)]),
                    cx,
                );
            }
        }
        self.refresh_placeholders(window, cx);
        cx.notify();
    }

    fn export_document(
        &mut self,
        format: ExportFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_active(window, cx);
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.doc.is_huge() || tab.doc.partial_range().is_some() || tab.doc.is_non_text() {
            self.set_status(tr("status.export_unavailable"), cx);
            return;
        }
        let dir = util::default_save_directory(tab.doc.path.as_deref());
        let stem = tab
            .doc
            .path
            .as_ref()
            .and_then(|path| path.file_stem())
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled".to_string());
        let suggested = format!("{stem}.{}", format.extension());
        self.pending_export = None;
        self.show_file_dialog(
            FileDialogMode::Save,
            FileDialogPurpose::Export { format },
            dir,
            suggested,
            window,
            cx,
        );
    }

    fn write_export(
        &mut self,
        path: &Path,
        format: ExportFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_active(window, cx);
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.doc.is_huge() || tab.doc.partial_range().is_some() || tab.doc.is_non_text() {
            self.set_status(tr("status.export_unavailable"), cx);
            return;
        }
        if !self.ensure_active_text(cx) {
            return;
        }
        let (text, language) = {
            let Some(tab) = self.active_tab() else { return };
            (tab.doc.text.clone(), tab.doc.language.clone())
        };
        let token = if self.active_tab().is_some_and(|tab| tab.large_file) {
            "text".to_string()
        } else {
            util::language_key(&language).to_string()
        };
        let (_, theme) = syntax_theme::export_theme(&self.settings);
        let title = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Luban Notepad".to_string());
        let content = match format {
            ExportFormat::Html => export::to_html(&text, &token, &theme, &title),
            ExportFormat::Rtf => export::to_rtf(&text, &token, &theme),
        };
        self.release_active_text();
        match write_atomic(path, content.as_bytes()) {
            Ok(()) => self.set_status(
                tr_args("status.exported", &[("path", path.display().to_string())]),
                cx,
            ),
            Err(err) => self.set_status(
                tr_args("status.export_failed", &[("error", err.to_string())]),
                cx,
            ),
        }
    }

    fn save_as_request(
        &mut self,
        request: SaveAsRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(request.tab_id) else {
            return;
        };
        if self.tabs[index].doc.is_huge() {
            self.start_huge_save(index, Some(request.path), request.then, false, window, cx);
            return;
        }
        if !self.ensure_tab_text(index, cx) {
            self.abort_pending_saves();
            return;
        }
        if !self.tabs[index].doc.is_non_text() {
            self.snapshot_before_save(&request.path, cx);
        }
        let lossy = match self.tabs[index].doc.save_as(&request.path) {
            Ok(lossy) => lossy,
            Err(err) => {
                self.release_tab_text(index);
                self.set_status(
                    tr_args("status.save_failed", &[("error", err.to_string())]),
                    cx,
                );
                self.abort_pending_saves();
                return;
            }
        };
        self.release_tab_text(index);
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.external = None;
        }
        if let Some(tab) = self.tabs.get(index) {
            self.watch
                .note_saved(&request.path, tab.doc.file_size, tab.doc.last_modified);
        }
        self.settings.add_recent(request.path.clone());
        self.commit_settings();
        if lossy {
            self.set_status(
                tr_args(
                    "status.saved_path_lossy",
                    &[("path", request.path.display().to_string())],
                ),
                cx,
            );
        } else {
            self.set_status(
                tr_args(
                    "status.saved_path",
                    &[("path", request.path.display().to_string())],
                ),
                cx,
            );
        }
        self.finish_save_flow(request.then, window, cx);
        cx.notify();
    }

    pub fn open_file_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self
            .workspace_root
            .clone()
            .or_else(|| {
                self.active_doc()
                    .and_then(|d| d.path.as_ref())
                    .and_then(|p| p.parent())
                    .map(Path::to_path_buf)
            })
            .or_else(dirs::home_dir)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.show_file_dialog(
            FileDialogMode::Open,
            FileDialogPurpose::OpenFiles,
            dir,
            String::new(),
            window,
            cx,
        );
    }

    pub fn open_folder_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self
            .workspace_root
            .clone()
            .or_else(dirs::home_dir)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.show_file_dialog(
            FileDialogMode::Folder,
            FileDialogPurpose::OpenFolder,
            dir,
            String::new(),
            window,
            cx,
        );
    }

    fn choose_find_in_files_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.find_in_files.folder_path(cx);
        self.show_file_dialog(
            FileDialogMode::Folder,
            FileDialogPurpose::FindInFilesFolder,
            dir,
            String::new(),
            window,
            cx,
        );
    }

    pub fn open_workspace_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self
            .workspace_file
            .as_ref()
            .and_then(|path| path.parent())
            .map(Path::to_path_buf)
            .or_else(|| self.workspace_root.clone())
            .or_else(dirs::home_dir)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.show_file_dialog(
            FileDialogMode::Open,
            FileDialogPurpose::OpenWorkspace,
            dir,
            String::new(),
            window,
            cx,
        );
    }

    pub fn open_add_folder_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self
            .workspace_root
            .clone()
            .or_else(dirs::home_dir)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.show_file_dialog(
            FileDialogMode::Folder,
            FileDialogPurpose::AddFolder,
            dir,
            String::new(),
            window,
            cx,
        );
    }

    pub fn open_import_settings_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = dirs::config_dir()
            .or_else(dirs::home_dir)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.show_file_dialog(
            FileDialogMode::Open,
            FileDialogPurpose::ImportSettings,
            dir,
            String::new(),
            window,
            cx,
        );
    }

    pub fn open_export_settings_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = dirs::config_dir()
            .or_else(dirs::home_dir)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.show_file_dialog(
            FileDialogMode::Save,
            FileDialogPurpose::ExportSettings,
            dir,
            "luban-settings.json".to_string(),
            window,
            cx,
        );
    }

    fn export_settings(&mut self, path: &Path, cx: &mut Context<Self>) {
        let json = match self.global_settings.to_json() {
            Ok(json) => json,
            Err(error) => {
                self.set_status(tr_args("settings.export_failed", &[("error", error)]), cx);
                return;
            }
        };
        match write_atomic(path, json.as_bytes()) {
            Ok(()) => self.set_status(
                tr_args("settings.exported", &[("path", path.display().to_string())]),
                cx,
            ),
            Err(error) => self.set_status(
                tr_args("settings.export_failed", &[("error", error.to_string())]),
                cx,
            ),
        }
    }

    fn apply_imported_settings(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let json = match fs::read_to_string(path) {
            Ok(json) => json,
            Err(error) => {
                self.set_status(
                    tr_args("settings.import_failed", &[("error", error.to_string())]),
                    cx,
                );
                return;
            }
        };
        let imported = match Settings::from_json(&json) {
            Ok(settings) => settings,
            Err(error) => {
                self.set_status(tr_args("settings.import_failed", &[("error", error)]), cx);
                return;
            }
        };
        if let Err(error) = imported.save() {
            self.set_status(
                tr_args("settings.import_failed", &[("error", error.to_string())]),
                cx,
            );
            return;
        }
        self.global_settings = imported;
        // A workspace keeps its overrides; the imported global settings become
        // the inherited base.
        self.settings = self.global_settings.clone();
        self.workspace_settings.apply_to(&mut self.settings);
        self.settings.validate();
        crate::i18n::apply_language(&self.settings.ui_language);
        self.apply_settings_impl(window, cx, false);
        self.set_status(
            tr_args("settings.imported", &[("path", path.display().to_string())]),
            cx,
        );
        cx.notify();
    }

    fn reset_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut defaults = Settings::default();
        defaults.validate();
        if let Err(error) = defaults.save() {
            self.set_status(
                tr_args("settings.reset_failed", &[("error", error.to_string())]),
                cx,
            );
            return;
        }
        self.global_settings = defaults;
        self.settings = self.global_settings.clone();
        self.workspace_settings.apply_to(&mut self.settings);
        self.settings.validate();
        crate::i18n::apply_language(&self.settings.ui_language);
        self.apply_settings_impl(window, cx, false);
        self.set_status(tr("settings.reset_done"), cx);
        cx.notify();
    }

    fn show_file_dialog(
        &mut self,
        mode: FileDialogMode,
        purpose: FileDialogPurpose,
        dir: PathBuf,
        suggested_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dir = util::absolutize(&dir);
        self.file_dialog = Some(
            FileDialogState::new(mode, purpose, dir.clone(), Vec::new())
                .with_list_focus(cx.focus_handle().tab_stop(true)),
        );
        let path_value = dir.display().to_string();
        self.file_path_input
            .update(cx, |input, cx| input.set_value(path_value, window, cx));
        self.file_name_input
            .update(cx, |input, cx| input.set_value(suggested_name, window, cx));
        match mode {
            FileDialogMode::Save => {
                self.file_name_input
                    .update(cx, |input, cx| input.focus(window, cx));
            }
            _ => {
                self.file_path_input
                    .update(cx, |input, cx| input.focus(window, cx));
            }
        }
        self.start_file_dialog_listing(dir, window, cx);
    }

    /// List a directory on the background executor. Every listing takes a new
    /// process-wide generation so a stale task from a closed dialog cannot
    /// match a reopened dialog listing the same directory.
    fn start_file_dialog_listing(
        &mut self,
        dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &mut self.file_dialog else {
            return;
        };
        dialog.dir = dir.clone();
        dialog.generation = next_listing_generation();
        let generation = dialog.generation;
        dialog.loading = true;
        dialog.entries.clear();
        dialog.selected.clear();
        dialog.active = None;
        dialog.anchor = None;
        dialog.last_click = None;
        dialog.error = None;
        dialog.list_scroll.set_offset(point(px(0.), px(0.)));

        let show_hidden = self.settings.show_hidden_files;
        let task_dir = dir.clone();
        let task = cx
            .background_executor()
            .spawn(async move { list_directory(&task_dir, show_hidden) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, _window, cx| {
                app.finish_file_dialog_listing(dir, generation, result, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_file_dialog_listing(
        &mut self,
        dir: PathBuf,
        generation: u64,
        result: luban_core::Result<Vec<FileEntry>>,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &mut self.file_dialog else {
            return;
        };
        if !dialog.accepts_listing(generation, &dir) {
            return;
        }
        dialog.loading = false;
        match result {
            Ok(entries) => {
                dialog.entries = entries;
                dialog.error = None;
            }
            Err(err) => {
                dialog.entries.clear();
                dialog.error = Some(tr_args("dialog.list_failed", &[("error", err.to_string())]));
            }
        }
        dialog.prune_selection();
        cx.notify();
    }

    fn file_dialog_navigate(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let path = util::absolutize(&path);
        if !path.is_dir() {
            if let Some(dialog) = &mut self.file_dialog {
                dialog.error = Some(tr_args(
                    "dialog.not_a_folder",
                    &[("path", path.display().to_string())],
                ));
            }
            cx.notify();
            return;
        }
        if self.file_dialog.is_none() {
            return;
        }
        let value = path.display().to_string();
        self.file_path_input
            .update(cx, |input, cx| input.set_value(value, window, cx));
        self.start_file_dialog_listing(path, window, cx);
    }

    fn file_dialog_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.file_dialog else {
            return;
        };
        if let Some(parent) = dialog.dir.parent() {
            let parent = parent.to_path_buf();
            self.file_dialog_navigate(parent, window, cx);
        }
    }

    fn file_dialog_set_filter(&mut self, filter: FileFilter, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.file_dialog {
            dialog.set_filter(filter);
        }
        cx.notify();
    }

    fn file_dialog_click(
        &mut self,
        row: usize,
        path: PathBuf,
        modifiers: Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (fill_name, focus) = {
            let Some(dialog) = &mut self.file_dialog else {
                return;
            };
            dialog.last_click = Some(path.clone());
            let command = modifiers.secondary() || modifiers.control;
            match dialog.mode {
                FileDialogMode::Open => {
                    if modifiers.shift {
                        dialog.extend_selection_to(row);
                    } else if command {
                        dialog.toggle_selection(&path);
                        dialog.set_active(row);
                        dialog.anchor = Some(row);
                    } else {
                        dialog.select_row(row);
                    }
                }
                FileDialogMode::Save | FileDialogMode::Folder => {
                    dialog.select_only(&path);
                    dialog.set_active(row);
                    dialog.anchor = Some(row);
                }
            }
            let fill_name = dialog.mode != FileDialogMode::Open;
            let focus = dialog.list_focus.clone();
            let active_row = dialog.active.unwrap_or(row);
            dialog.list_scroll.scroll_to_item(active_row);
            (fill_name, focus)
        };

        if fill_name {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.file_name_input
                .update(cx, |input, cx| input.set_value(name, window, cx));
        }
        if let Some(focus) = focus {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// The second click of a double click opens the file in the Open dialog.
    /// Save and Folder dialogs keep their click behavior, and a click sequence
    /// that moved to another row (for example after entering a directory) is
    /// ignored instead of opening whatever now sits under the cursor.
    fn file_dialog_double_click(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let opens = self
            .file_dialog
            .as_mut()
            .is_some_and(|dialog| dialog.prepare_double_click_open(&path));
        if opens {
            self.file_dialog_confirm(window, cx);
        }
    }

    fn file_dialog_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.file_dialog {
            dialog.error = Some(message.into());
        }
        cx.notify();
    }

    fn file_dialog_list_focused(&self, window: &Window) -> bool {
        self.file_dialog
            .as_ref()
            .and_then(|dialog| dialog.list_focus.as_ref())
            .is_some_and(|focus| focus.is_focused(window))
    }

    fn focus_file_dialog_list(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let focus = if let Some(dialog) = &mut self.file_dialog {
            dialog.set_active(row);
            dialog
                .list_scroll
                .scroll_to_item(dialog.active.unwrap_or(row));
            dialog.list_focus.clone()
        } else {
            None
        };
        if let Some(focus) = focus {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    fn file_dialog_list_key(
        &mut self,
        key: &str,
        modifiers: Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (row_count, current, page) = {
            let Some(dialog) = &self.file_dialog else {
                return false;
            };
            let page = (f32::from(dialog.list_scroll.bounds().size.height) / 26.0).floor() as usize;
            (
                dialog.visible_row_count(),
                dialog.active.unwrap_or(0),
                page.max(1),
            )
        };
        let command = modifiers.secondary() || modifiers.control;

        if key == "space" && !command {
            if let Some(dialog) = &mut self.file_dialog {
                dialog.toggle_active();
                let row = dialog.active.unwrap_or(0);
                dialog.list_scroll.scroll_to_item(row);
            }
            cx.notify();
            return true;
        }
        if key == "a" && command {
            if let Some(dialog) = &mut self.file_dialog {
                dialog.select_all_visible();
            }
            cx.notify();
            return true;
        }
        if key == "enter" {
            self.file_dialog_confirm(window, cx);
            return true;
        }

        let target = match key {
            "up" => Some(current.saturating_sub(1)),
            "down" => Some((current + 1).min(row_count - 1)),
            "home" => Some(0),
            "end" => Some(row_count - 1),
            "pageup" => Some(current.saturating_sub(page)),
            "pagedown" => Some((current + page).min(row_count - 1)),
            _ => None,
        };
        let Some(target) = target else {
            return false;
        };
        if let Some(dialog) = &mut self.file_dialog {
            if modifiers.shift {
                dialog.extend_selection_to(target);
            } else if command {
                dialog.set_active(target);
            } else {
                dialog.select_row(target);
            }
            dialog.list_scroll.scroll_to_item(target);
        }
        cx.notify();
        true
    }

    fn file_dialog_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.file_dialog.take() else {
            return;
        };
        let typed = self.file_path_input.read(cx).value().to_string();
        let typed = typed.trim().to_string();
        let name = self.file_name_input.read(cx).value().to_string();
        let name = name.trim().to_string();

        match dialog.mode {
            FileDialogMode::Open => {
                let selected = dialog.selected_in_visible_order();
                if matches!(dialog.purpose, FileDialogPurpose::ImportTheme) {
                    let candidate = selected.first().cloned().or_else(|| {
                        if typed.is_empty() {
                            None
                        } else {
                            Some(util::absolutize(Path::new(&typed)))
                        }
                    });
                    match candidate {
                        Some(path) if path.is_file() => {
                            self.import_syntax_theme(&path, window, cx);
                        }
                        Some(_) => {
                            self.file_dialog = Some(dialog);
                            self.file_dialog_error(tr("dialog.not_a_file"), cx);
                        }
                        None => {
                            self.file_dialog = Some(dialog);
                            self.file_dialog_error(tr("dialog.select_file"), cx);
                        }
                    }
                    cx.notify();
                    return;
                }
                if !selected.is_empty() {
                    let total = selected.len();
                    let mut opened = 0usize;
                    let mut failed = 0usize;
                    for path in selected {
                        if self.open_path(&path, window, cx) {
                            opened += 1;
                        } else {
                            failed += 1;
                        }
                    }
                    if total > 1 {
                        if failed == 0 {
                            self.set_status(
                                tr_args("status.batch_opened", &[("count", opened.to_string())]),
                                cx,
                            );
                        } else {
                            self.set_status(
                                tr_args(
                                    "status.batch_open_partial",
                                    &[
                                        ("opened", opened.to_string()),
                                        ("failed", failed.to_string()),
                                    ],
                                ),
                                cx,
                            );
                        }
                    }
                    cx.notify();
                    return;
                }
                let dir_text = dialog.dir.display().to_string();
                let candidate = if !typed.is_empty() && typed != dir_text {
                    PathBuf::from(&typed)
                } else {
                    dialog.dir.clone()
                };
                let candidate = util::absolutize(&candidate);
                if candidate.is_dir() {
                    self.file_dialog = Some(dialog);
                    self.file_dialog_navigate(candidate, window, cx);
                } else if candidate.is_file() {
                    match dialog.purpose {
                        FileDialogPurpose::ImportTheme => {
                            self.import_syntax_theme(&candidate, window, cx);
                        }
                        FileDialogPurpose::OpenWorkspace => {
                            self.open_workspace(&candidate, window, cx);
                        }
                        FileDialogPurpose::ImportSettings => {
                            self.pending_settings_import = Some(candidate);
                            self.confirm = Some(ConfirmState::new(
                                CloseIntent::ImportSettings,
                                tr("settings.import_title"),
                                tr("settings.import_detail"),
                            ));
                        }
                        _ => {
                            self.open_path(&candidate, window, cx);
                        }
                    }
                } else {
                    self.file_dialog = Some(dialog);
                    self.file_dialog_error(
                        tr_args(
                            "dialog.file_not_found",
                            &[("path", candidate.display().to_string())],
                        ),
                        cx,
                    );
                }
            }
            FileDialogMode::Save => {
                let name = if name.is_empty() {
                    name
                } else {
                    dialog.with_default_extension(&name)
                };
                let mut candidate = if !name.is_empty() {
                    PathBuf::from(&name)
                } else if !typed.is_empty() {
                    PathBuf::from(&typed)
                } else {
                    self.file_dialog = Some(dialog);
                    self.file_dialog_error(tr("dialog.enter_name"), cx);
                    return;
                };
                if !candidate.is_absolute() {
                    candidate = dialog.dir.join(candidate);
                }
                let candidate = util::absolutize(&candidate);
                if candidate.is_dir() {
                    self.file_dialog = Some(dialog);
                    self.file_dialog_error(tr("dialog.target_is_folder"), cx);
                    return;
                }
                match dialog.purpose.clone() {
                    FileDialogPurpose::SaveAs { tab_id, then } => {
                        let request = SaveAsRequest {
                            tab_id,
                            path: candidate,
                            then,
                        };
                        if request.path.exists() {
                            self.overwrite_save = Some(request);
                        } else {
                            self.save_as_request(request, window, cx);
                        }
                    }
                    FileDialogPurpose::Export { format } => {
                        if candidate.exists() {
                            self.pending_export = Some(format);
                            self.overwrite_save = Some(SaveAsRequest {
                                tab_id: 0,
                                path: candidate,
                                then: SaveThen::Nothing,
                            });
                        } else {
                            self.write_export(&candidate, format, window, cx);
                        }
                    }
                    FileDialogPurpose::SaveWorkspace | FileDialogPurpose::SaveWorkspaceAs => {
                        let candidate = if candidate.extension().is_none() {
                            candidate
                                .with_extension(luban_core::workspace_file::WORKSPACE_EXTENSION)
                        } else {
                            candidate
                        };
                        self.write_workspace(&candidate, cx);
                        self.save_session_now(cx);
                    }
                    FileDialogPurpose::ExportSettings => {
                        self.export_settings(&candidate, cx);
                    }
                    _ => {}
                }
            }
            FileDialogMode::Folder => {
                let mut candidate = if !typed.is_empty() {
                    PathBuf::from(&typed)
                } else {
                    dialog
                        .selected
                        .iter()
                        .next()
                        .cloned()
                        .unwrap_or_else(|| dialog.dir.clone())
                };
                if !candidate.is_absolute() {
                    candidate = dialog.dir.join(candidate);
                }
                let candidate = util::absolutize(&candidate);
                if !candidate.is_dir() {
                    self.file_dialog = Some(dialog);
                    self.file_dialog_error(
                        tr_args(
                            "dialog.not_a_folder",
                            &[("path", candidate.display().to_string())],
                        ),
                        cx,
                    );
                    return;
                }
                match dialog.purpose {
                    FileDialogPurpose::OpenFolder => {
                        self.settings.show_file_explorer = true;
                        self.open_folder_workspace(&candidate, window, cx);
                        self.apply_settings(window, cx);
                    }
                    FileDialogPurpose::AddFolder => {
                        self.add_workspace_root(&candidate, cx);
                        self.settings.show_file_explorer = true;
                        self.apply_settings(window, cx);
                    }
                    FileDialogPurpose::FindInFilesFolder => {
                        let value = candidate.display().to_string();
                        self.find_in_files.folder.update(cx, |input, cx| {
                            input.set_value(value, window, cx);
                        });
                    }
                    _ => {}
                }
            }
        }
        cx.notify();
    }

    pub fn reload_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let index = self.active;
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.doc.path.is_none() {
            self.set_status(tr("status.reload_untitled"), cx);
            return;
        }
        if tab.doc.is_huge() {
            let id = tab.id;
            if tab.doc.is_modified() {
                self.confirm = Some(ConfirmState::new(
                    CloseIntent::ReloadTab(id),
                    tr("confirm.reload_title"),
                    tr("confirm.reload_detail"),
                ));
                cx.notify();
            } else {
                self.start_huge_reload(index, window, cx);
            }
            return;
        }
        if tab.doc.is_modified() {
            self.confirm = Some(ConfirmState::new(
                CloseIntent::ReloadTab(tab.id),
                tr("confirm.reload_title"),
                tr("confirm.reload_normal_detail"),
            ));
            cx.notify();
            return;
        }
        self.reload_tab_now(index, window, cx);
    }

    /// Reload a normal document from disk without asking (the caller has
    /// already confirmed discarding unsaved changes).
    fn reload_tab_now(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let path = tab.doc.path.clone();
        let tab = &mut self.tabs[index];
        match tab.doc.reload() {
            Ok(()) => {
                let (text, cursor, anchor) = if tab.large_file {
                    let (text, _) = tab.doc.detach_text();
                    (text, tab.doc.cursor, tab.doc.anchor)
                } else {
                    (tab.doc.text.clone(), tab.doc.cursor, tab.doc.anchor)
                };
                tab.highlighted_revision = u64::MAX;
                tab.decorations_key = None;
                tab.external = None;
                tab.editor.update(cx, |state, cx| {
                    state.set_value(text, window, cx);
                    let total = state.text().len_chars();
                    let b0 = state.text().char_to_byte_idx(anchor.min(total));
                    let b1 = state.text().char_to_byte_idx(cursor.min(total));
                    state.set_selected_range(b0..b1, cx);
                });
                tab.needs_sync = false;
                if let Some(path) = path {
                    self.watch
                        .note_saved(&path, tab.doc.file_size, tab.doc.last_modified);
                }
                self.set_status(tr("status.reloaded"), cx);
            }
            Err(err) => self.set_status(
                tr_args("status.reload_failed", &[("error", err.to_string())]),
                cx,
            ),
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Close / quit
    // ------------------------------------------------------------------

    fn modified_tabs(&self) -> Vec<u64> {
        self.tabs
            .iter()
            .filter(|t| t.doc.is_modified())
            .map(|t| t.id)
            .collect()
    }

    pub fn request_close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let Some(tab) = self.active_tab() else { return };
        let id = tab.id;
        let name = tab.doc.display_name();
        if tab.doc.is_modified() {
            self.confirm = Some(ConfirmState::new(
                CloseIntent::Tab(id),
                tr_args("confirm.save_changes_title", &[("name", name)]),
                tr("confirm.save_changes_detail"),
            ));
            cx.notify();
        } else if let Some(index) = self.tab_index_by_id(id) {
            self.close_tab_at(index, window, cx);
        }
    }

    pub fn close_tab_by_id(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let Some(index) = self.tab_index_by_id(id) else {
            return;
        };
        let tab = &self.tabs[index];
        if tab.doc.is_modified() {
            let name = tab.doc.display_name();
            self.confirm = Some(ConfirmState::new(
                CloseIntent::Tab(id),
                tr_args("confirm.save_changes_title", &[("name", name)]),
                tr("confirm.save_changes_detail"),
            ));
            cx.notify();
        } else {
            self.close_tab_at(index, window, cx);
        }
    }

    fn close_tab_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab_at_inner(index, window, cx);
    }

    fn close_tab_at_inner(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        let closed = self.tabs[index].id;
        let closed_group = self.tabs[index].group;
        let closed_position = self.tabs[..index]
            .iter()
            .filter(|tab| tab.group == closed_group)
            .count();
        let closed_active = index == self.active;
        self.record_closed_tab(index, cx);
        self.cancel_tab_jobs(index);
        self.tabs.remove(index);
        self.nav.remove_tab(closed);
        if self.active_in_group[closed_group] == Some(closed) {
            self.active_in_group[closed_group] = None;
        }
        if self.tabs.is_empty() {
            self.active = 0;
            self.active_group = 0;
            self.active_in_group = [None, None];
            self.new_file(window, cx);
            return;
        }
        if !closed_active {
            if self.active > index {
                self.active -= 1;
            } else if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            }
        } else {
            let memberships: Vec<usize> = self.tabs.iter().map(|tab| tab.group).collect();
            let members = tab_groups::group_tabs(&memberships, closed_group);
            if let Some(next) = members
                .get(closed_position.min(members.len().saturating_sub(1)))
                .copied()
            {
                self.active = next;
                self.active_group = closed_group;
            } else if let Some(other) = tab_groups::fallback_group(closed_group, self.split) {
                if let Some(first) = self.tabs.iter().position(|tab| tab.group == other) {
                    self.active = first;
                    self.active_group = other;
                }
            } else {
                self.active = self.active.min(self.tabs.len() - 1);
            }
            let group = self.tabs[self.active].group;
            self.active_group = group;
            self.active_in_group[group] = Some(self.tabs[self.active].id);
        }
        self.focus_editor = true;
        self.find.clear();
        self.find.dirty = true;
        cx.notify();
    }

    pub fn request_close_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let modified = self.modified_tabs();
        if modified.is_empty() {
            self.close_all_now(window, cx);
        } else {
            let detail = if modified.len() == 1 {
                tr("confirm.close_all_detail.one")
            } else {
                tr_args(
                    "confirm.close_all_detail.other",
                    &[("count", modified.len().to_string())],
                )
            };
            self.confirm = Some(ConfirmState::new(
                CloseIntent::AllTabs,
                tr("confirm.close_all_title"),
                detail,
            ));
            cx.notify();
        }
    }

    fn close_all_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_batch_is_close = false;
        for index in 0..self.tabs.len() {
            self.cancel_tab_jobs(index);
        }
        self.tabs.clear();
        self.active = 0;
        self.active_group = 0;
        self.active_in_group = [None, None];
        self.nav.clear();
        self.find.clear();
        self.new_file(window, cx);
    }

    pub fn request_exit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        if self.modified_tabs().is_empty() {
            self.request_quit(window, cx);
        } else {
            self.confirm = Some(ConfirmState::new(
                CloseIntent::Exit,
                tr("confirm.quit_title"),
                tr("confirm.quit_detail"),
            ));
            cx.notify();
        }
    }

    fn on_should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.allow_close {
            return true;
        }
        self.sync_active(window, cx);
        if self.modified_tabs().is_empty() {
            let last = cx.windows().len() <= 1;
            self.persist_state(window, cx);
            self.allow_close = true;
            if self.window_registered {
                window_registry::unregister(self.window_id);
                self.window_registered = false;
            }
            if last {
                cx.quit();
            }
            return true;
        }
        self.request_exit(window, cx);
        false
    }

    fn prompt_next_batch_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        loop {
            let next = if !self.pending_exit_saves.is_empty() {
                self.pending_exit_saves.remove(0)
            } else if !self.pending_close_saves.is_empty() {
                self.pending_close_saves.remove(0)
            } else {
                return;
            };
            let Some(index) = self.tab_index_by_id(next) else {
                continue;
            };
            let then = if self.pending_close_all_after_saves {
                SaveThen::CloseAll
            } else {
                SaveThen::Exit
            };
            let (dir, name) = {
                let tab = &self.tabs[index];
                let name = if tab.doc.is_partial() {
                    tab.doc
                        .path
                        .as_deref()
                        .map(luban_core::large_file::partial_save_name)
                        .unwrap_or_else(|| tab.doc.display_name())
                } else {
                    tab.doc.display_name()
                };
                (util::default_save_directory(tab.doc.path.as_deref()), name)
            };
            self.show_file_dialog(
                FileDialogMode::Save,
                FileDialogPurpose::SaveAs { tab_id: next, then },
                dir,
                name,
                window,
                cx,
            );
            return;
        }
    }

    fn confirm_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        match confirm.intent {
            CloseIntent::OverwriteExternal(id) => {
                let Some((tab_id, target, then)) = self.pending_huge_overwrite.take() else {
                    return;
                };
                if tab_id != id {
                    return;
                }
                let Some(index) = self.tab_index_by_id(id) else {
                    return;
                };
                self.start_huge_save(index, target, then, true, window, cx);
            }
            CloseIntent::SaveConflict(id) => {
                let Some((tab_id, target, then)) = self.pending_normal_overwrite.take() else {
                    return;
                };
                if tab_id != id {
                    return;
                }
                let Some(index) = self.tab_index_by_id(id) else {
                    return;
                };
                if let Some(target) = target {
                    self.save_as_request(
                        SaveAsRequest {
                            tab_id: id,
                            path: target,
                            then,
                        },
                        window,
                        cx,
                    );
                } else {
                    self.save_tab_at_with(index, then, true, window, cx);
                }
            }
            CloseIntent::ReloadTab(id) => {
                let Some(index) = self.tab_index_by_id(id) else {
                    return;
                };
                if !self.tabs[index].doc.is_huge() {
                    self.save_tab_at_with(index, SaveThen::ReloadTab(id), false, window, cx);
                    return;
                }
                if self.tabs[index].doc.is_partial() {
                    let (dir, name) = {
                        let tab = &self.tabs[index];
                        (
                            util::default_save_directory(tab.doc.path.as_deref()),
                            tab.doc
                                .path
                                .as_deref()
                                .map(luban_core::large_file::partial_save_name)
                                .unwrap_or_else(|| tab.doc.display_name()),
                        )
                    };
                    self.show_file_dialog(
                        FileDialogMode::Save,
                        FileDialogPurpose::SaveAs {
                            tab_id: id,
                            then: SaveThen::ReloadTab(id),
                        },
                        dir,
                        name,
                        window,
                        cx,
                    );
                    return;
                }
                self.start_huge_save(index, None, SaveThen::ReloadTab(id), false, window, cx);
            }
            CloseIntent::Tab(id) => {
                let Some(index) = self.tab_index_by_id(id) else {
                    return;
                };
                if self.tabs[index].doc.is_huge() && !self.tabs[index].doc.is_partial() {
                    self.start_huge_save(index, None, SaveThen::CloseTab(id), false, window, cx);
                    return;
                }
                if self.tabs[index].doc.path.is_some() && !self.tabs[index].doc.is_partial() {
                    self.save_tab_at_with(index, SaveThen::CloseTab(id), false, window, cx);
                } else {
                    let (dir, name) = {
                        let tab = &self.tabs[index];
                        let name = if tab.doc.is_partial() {
                            tab.doc
                                .path
                                .as_deref()
                                .map(luban_core::large_file::partial_save_name)
                                .unwrap_or_else(|| tab.doc.display_name())
                        } else {
                            tab.doc.display_name()
                        };
                        (util::default_save_directory(tab.doc.path.as_deref()), name)
                    };
                    self.show_file_dialog(
                        FileDialogMode::Save,
                        FileDialogPurpose::SaveAs {
                            tab_id: id,
                            then: SaveThen::CloseTab(id),
                        },
                        dir,
                        name,
                        window,
                        cx,
                    );
                }
            }
            CloseIntent::AllTabs => {
                self.start_batch_save(BatchSaveKind::CloseAll, window, cx);
            }
            CloseIntent::Exit => {
                self.start_batch_save(BatchSaveKind::Exit, window, cx);
            }
            CloseIntent::RestoreHistory => {
                self.apply_pending_history_restore(window, cx);
            }
            CloseIntent::TrimTabs => {
                self.save_next_trim(window, cx);
            }
            CloseIntent::DeletePath { path, permanent } => {
                if !self.delete_path_now(&path, permanent, cx) && !permanent {
                    self.confirm = Some(ConfirmState::new(
                        CloseIntent::DeletePath {
                            path,
                            permanent: true,
                        },
                        tr("explorer.delete_failed_title"),
                        tr("explorer.delete_permanent_detail"),
                    ));
                    cx.notify();
                }
            }
            CloseIntent::ImportSettings => {
                if let Some(path) = self.pending_settings_import.take() {
                    self.apply_imported_settings(&path, window, cx);
                }
            }
            CloseIntent::ResetSettings => {
                self.reset_settings(window, cx);
            }
            CloseIntent::WorkspaceReplace => {
                self.apply_workspace_replace(window, cx);
            }
        }
    }

    fn confirm_discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        // Discarding one document must not let a queued Close All / Exit
        // continuation run with a different decision. A trim plan is the
        // decision itself and is executed below.
        if confirm.intent != CloseIntent::TrimTabs {
            self.abort_pending_saves();
        }
        match confirm.intent {
            CloseIntent::OverwriteExternal(id) => {
                self.pending_huge_overwrite = None;
                let Some(index) = self.tab_index_by_id(id) else {
                    return;
                };
                if self.tabs[index].doc.is_modified() {
                    self.confirm = Some(ConfirmState::new(
                        CloseIntent::ReloadTab(id),
                        tr("confirm.reload_title"),
                        tr("confirm.reload_detail"),
                    ));
                    cx.notify();
                } else {
                    self.start_huge_reload(index, window, cx);
                }
            }
            CloseIntent::SaveConflict(id) => {
                self.pending_normal_overwrite = None;
                let Some(index) = self.tab_index_by_id(id) else {
                    return;
                };
                if self.tabs[index].doc.is_modified() {
                    self.confirm = Some(ConfirmState::new(
                        CloseIntent::ReloadTab(id),
                        tr("confirm.reload_title"),
                        tr("confirm.reload_normal_detail"),
                    ));
                    cx.notify();
                } else {
                    self.reload_tab_now(index, window, cx);
                }
            }
            CloseIntent::ReloadTab(id) => {
                if let Some(index) = self.tab_index_by_id(id) {
                    if self.tabs[index].doc.is_huge() {
                        self.start_huge_reload(index, window, cx);
                    } else {
                        self.reload_tab_now(index, window, cx);
                    }
                }
            }
            CloseIntent::Tab(id) => {
                if let Some(index) = self.tab_index_by_id(id) {
                    self.close_tab_at(index, window, cx);
                }
            }
            CloseIntent::AllTabs => self.close_all_now(window, cx),
            CloseIntent::Exit => self.request_quit(window, cx),
            CloseIntent::RestoreHistory => {
                self.pending_history_restore = None;
                cx.notify();
            }
            CloseIntent::TrimTabs => {
                self.close_trim_now(true, window, cx);
            }
            CloseIntent::DeletePath { path, .. } => {
                if self.delete_path_now(&path, false, cx) {
                    self.confirm = None;
                }
            }
            CloseIntent::ImportSettings => {
                self.pending_settings_import = None;
            }
            CloseIntent::ResetSettings => {}
            CloseIntent::WorkspaceReplace => {}
        }
    }

    fn confirm_cancel(&mut self, cx: &mut Context<Self>) {
        self.confirm = None;
        self.pending_history_restore = None;
        self.abort_pending_saves();
        cx.notify();
    }

    fn persist_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let size = window.bounds().size;
        self.window_width = size.width.into();
        self.window_height = size.height.into();
        self.window_maximized = window.is_maximized();
        self.save_session_now(cx);
        self.commit_settings();
    }

    fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_batch_is_close = false;
        for index in 0..self.tabs.len() {
            self.cancel_tab_jobs(index);
        }
        self.persist_state(window, cx);
        self.allow_close = true;
        if self.window_registered {
            window_registry::unregister(self.window_id);
            self.window_registered = false;
        }
        if cx.windows().len() > 1 {
            // File > Exit closes the current window; the application keeps
            // running while another window remains.
            window.remove_window();
        } else {
            cx.quit();
        }
    }

    // ------------------------------------------------------------------
    // Session
    // ------------------------------------------------------------------

    pub(crate) fn build_window_session(&self) -> WindowSession {
        let memberships: Vec<usize> = self.tabs.iter().map(|tab| tab.group).collect();
        let mut groups = Vec::new();
        for group in 0..self.group_count() {
            let indices = tab_groups::group_tabs(&memberships, group);
            let active_id = self.active_in_group[group].or_else(|| {
                (group == self.active_group)
                    .then(|| self.tabs.get(self.active).map(|tab| tab.id))
                    .flatten()
            });
            let active_tab = active_id
                .and_then(|id| self.tab_index_by_id(id))
                .and_then(|index| indices.iter().position(|candidate| *candidate == index))
                .unwrap_or(0);
            groups.push(GroupSession {
                active_tab,
                tabs: indices
                    .iter()
                    .filter_map(|index| self.tabs.get(*index))
                    .map(session_tab_from)
                    .collect(),
            });
        }
        WindowSession {
            id: self.window_id,
            x: None,
            y: None,
            width: self.window_width,
            height: self.window_height,
            maximized: self.window_maximized,
            workspace_file: self.workspace_file.clone(),
            workspace_roots: self.workspace_roots.clone(),
            split: match self.split {
                SplitDirection::Right => SplitLayout::Right,
                SplitDirection::Down => SplitLayout::Down,
                SplitDirection::None => SplitLayout::None,
            },
            active_group: self.active_group,
            groups,
        }
    }

    fn save_session_now(&mut self, cx: &mut Context<Self>) {
        let current = self.build_window_session();
        let windows = window_registry::collect(cx, current);
        let session = Session {
            version: luban_core::session::SESSION_VERSION,
            windows,
        };
        match session.save() {
            Ok(()) => {}
            Err(err) => {
                log::warn!("could not save session: {err}");
                self.set_status(
                    tr_args("status.session_save_failed", &[("error", err.to_string())]),
                    cx,
                );
            }
        }
    }

    fn autosave_session(&mut self, cx: &mut Context<Self>) {
        if !self.settings.autosave_session {
            return;
        }
        let elapsed = self.last_session_save.elapsed();
        if elapsed < Duration::from_secs(self.settings.session_interval_secs.max(5)) {
            return;
        }
        self.last_session_save = Instant::now();
        self.save_session_now(cx);
    }

    // ------------------------------------------------------------------
    // Pending async work
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Editing commands
    // ------------------------------------------------------------------

    pub fn duplicate_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        if !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let (start, end) = tab.doc.selection();
        let (new_text, sel_start, sel_end) = textutil::duplicate_lines(&tab.doc.text, start, end);
        self.replace_document_text(new_text, sel_start, sel_end, window, cx);
        self.set_status(tr("status.duplicated_line"), cx);
    }

    pub fn delete_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        if !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let (start, end) = tab.doc.selection();
        let (new_text, pos) = textutil::delete_lines(&tab.doc.text, start, end);
        self.replace_document_text(new_text, pos, pos, window, cx);
        self.set_status(tr("status.deleted_line"), cx);
    }

    pub fn trim_trailing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        if !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let trimmed = textutil::trim_trailing_whitespace(&tab.doc.text);
        if trimmed == tab.doc.text {
            self.set_status(tr("status.no_trailing_whitespace"), cx);
            return;
        }
        let (start, end) = tab.doc.selection();
        let total = textutil::char_count(&trimmed);
        self.replace_document_text(trimmed, start.min(total), end.min(total), window, cx);
        self.set_status(tr("status.trimmed_whitespace"), cx);
    }

    pub fn toggle_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        if !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let Some(prefix) = language::line_comment(&tab.doc.language) else {
            self.set_status(
                tr_args(
                    "status.no_line_comment",
                    &[("language", language_display_name(&tab.doc.language))],
                ),
                cx,
            );
            return;
        };
        let (start, end) = tab.doc.selection();
        let (new_text, sel_start, sel_end) =
            textutil::toggle_line_comments(&tab.doc.text, start, end, prefix);
        self.replace_document_text(new_text, sel_start, sel_end, window, cx);
        self.set_status(tr("status.toggled_comment"), cx);
    }

    pub fn sort_selection(&mut self, ascending: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.sort_selection_mode(SortMode::Text, ascending, false, 1, window, cx);
    }

    /// Whether a full-text editing command may run on the active tab.
    fn full_text_editing_available(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.sync_active(window, cx);
        let Some(tab) = self.active_tab() else {
            return false;
        };
        if tab.doc.is_huge() {
            self.set_status(tr("status.huge_op_unavailable"), cx);
            return false;
        }
        if tab.large_file || tab.doc.partial_range().is_some() || tab.doc.is_non_text() {
            self.set_status(tr("status.op_needs_full_text"), cx);
            return false;
        }
        true
    }

    fn multi_cursor_available(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.sync_active(window, cx);
        let Some(tab) = self.active_tab() else {
            return false;
        };
        if tab.doc.is_huge()
            || tab.large_file
            || tab.doc.partial_range().is_some()
            || tab.doc.is_non_text()
        {
            self.set_status(tr("status.op_needs_full_text"), cx);
            return false;
        }
        true
    }

    fn sort_selection_mode(
        &mut self,
        mode: SortMode,
        ascending: bool,
        ignore_case: bool,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.full_text_editing_available(window, cx) || !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let (start, end) = tab.doc.selection();
        let (new_text, sel_start, sel_end) = line_ops::sort_lines(
            &tab.doc.text,
            start,
            end,
            mode,
            ascending,
            ignore_case,
            column,
        );
        self.replace_document_text(new_text, sel_start, sel_end, window, cx);
        self.set_status(tr("status.sorted"), cx);
    }

    fn move_lines(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.full_text_editing_available(window, cx) || !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let (start, end) = tab.doc.selection();
        let (new_text, sel_start, sel_end) = line_ops::move_lines(&tab.doc.text, start, end, up);
        if new_text == tab.doc.text {
            self.set_status(tr("status.line_move_boundary"), cx);
            return;
        }
        self.replace_document_text(new_text, sel_start, sel_end, window, cx);
        self.set_status(
            tr(if up {
                "status.line_moved_up"
            } else {
                "status.line_moved_down"
            }),
            cx,
        );
    }

    fn join_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.full_text_editing_available(window, cx) || !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let (start, end) = tab.doc.selection();
        let (new_text, sel_start, sel_end) = line_ops::join_lines(&tab.doc.text, start, end);
        if new_text == tab.doc.text {
            self.set_status(tr("status.line_join_boundary"), cx);
            return;
        }
        self.replace_document_text(new_text, sel_start, sel_end, window, cx);
        self.set_status(tr("status.lines_joined"), cx);
    }

    fn split_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.full_text_editing_available(window, cx) || !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let (start, end) = tab.doc.selection();
        let (_, column) = textutil::LineIndex::new(&tab.doc.text).line_col(tab.doc.cursor);
        if column <= 1 && start == end {
            self.set_status(tr("status.line_split_no_column"), cx);
            return;
        }
        let column = column.saturating_sub(1);
        let (new_text, sel_start, sel_end) =
            line_ops::split_lines_at(&tab.doc.text, start, end, column);
        if new_text == tab.doc.text {
            self.set_status(tr("status.line_split_no_column"), cx);
            return;
        }
        self.replace_document_text(new_text, sel_start, sel_end, window, cx);
        self.set_status(tr("status.lines_split"), cx);
    }

    fn remove_duplicate_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.full_text_editing_available(window, cx) || !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let (start, end) = tab.doc.selection();
        let (new_text, sel_start, sel_end) =
            line_ops::remove_duplicate_lines(&tab.doc.text, start, end);
        self.replace_document_text(new_text, sel_start, sel_end, window, cx);
        self.set_status(tr("status.duplicates_removed"), cx);
    }

    fn remove_empty_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.full_text_editing_available(window, cx) || !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let (start, end) = tab.doc.selection();
        let (new_text, sel_start, sel_end) =
            line_ops::remove_empty_lines(&tab.doc.text, start, end);
        self.replace_document_text(new_text, sel_start, sel_end, window, cx);
        self.set_status(tr("status.empty_lines_removed"), cx);
    }

    fn open_sort_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.full_text_editing_available(window, cx) {
            return;
        }
        let column = cx.new(|cx| InputState::new(window, cx).default_value("1"));
        let focus = column.clone();
        cx.defer_in(window, move |_this, window, cx| {
            focus.update(cx, |input, cx| input.focus(window, cx));
        });
        self.sort_dialog = Some(SortDialogState {
            column,
            ascending: true,
            ignore_case: true,
            error: None,
        });
        cx.notify();
    }

    fn apply_sort_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.sort_dialog.take() else {
            return;
        };
        let column = dialog.column.read(cx).value().trim().parse::<usize>();
        let Ok(column) = column else {
            self.sort_dialog = Some(SortDialogState {
                error: Some(tr("sort.invalid_column")),
                ..dialog
            });
            cx.notify();
            return;
        };
        if column == 0 {
            self.sort_dialog = Some(SortDialogState {
                error: Some(tr("sort.invalid_column")),
                ..dialog
            });
            cx.notify();
            return;
        }
        self.sort_selection_mode(
            SortMode::Column,
            dialog.ascending,
            dialog.ignore_case,
            column,
            window,
            cx,
        );
        self.focus_editor(window, cx);
    }

    fn open_filter_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.full_text_editing_available(window, cx) {
            return;
        }
        let needle =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("filter.needle_placeholder")));
        let focus = needle.clone();
        cx.defer_in(window, move |_this, window, cx| {
            focus.update(cx, |input, cx| input.focus(window, cx));
        });
        self.filter_dialog = Some(FilterDialogState {
            needle,
            keep: true,
            match_case: false,
            regex: false,
            error: None,
        });
        cx.notify();
    }

    fn apply_filter_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.filter_dialog.take() else {
            return;
        };
        let needle = dialog.needle.read(cx).value().to_string();
        if needle.is_empty() {
            self.filter_dialog = Some(FilterDialogState {
                error: Some(tr("filter.empty_needle")),
                ..dialog
            });
            cx.notify();
            return;
        }
        if !self.full_text_editing_available(window, cx) || !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let (start, end) = tab.doc.selection();
        match line_ops::filter_lines(
            &tab.doc.text,
            start,
            end,
            &needle,
            dialog.keep,
            dialog.match_case,
            dialog.regex,
        ) {
            Ok(outcome) => {
                if outcome.matched == 0 {
                    self.set_status(tr("status.filter_no_match"), cx);
                    return;
                }
                if outcome.removed == 0 {
                    self.set_status(tr("status.filter_no_match"), cx);
                    return;
                }
                self.replace_document_text(
                    outcome.text,
                    outcome.selection.0,
                    outcome.selection.1,
                    window,
                    cx,
                );
                self.set_status(
                    tr_args(
                        "status.filtered_lines",
                        &[("count", outcome.removed.to_string())],
                    ),
                    cx,
                );
                self.focus_editor(window, cx);
            }
            Err(err) => {
                self.filter_dialog = Some(FilterDialogState {
                    error: Some(err),
                    ..dialog
                });
                cx.notify();
            }
        }
    }

    fn open_column_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.full_text_editing_available(window, cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let (_, column) = textutil::LineIndex::new(&tab.doc.text).line_col(tab.doc.cursor);
        self.column_editor.open_at(column, window, cx);
        let input = self.column_editor.column.clone();
        cx.defer_in(window, move |_this, window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx));
        });
        cx.notify();
    }

    fn apply_column_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let payload = match self.column_editor.payload(cx) {
            Ok(payload) => payload,
            Err(err) => {
                self.column_editor.error = Some(err);
                cx.notify();
                return;
            }
        };
        if !self.full_text_editing_available(window, cx) || !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let (start, end) = tab.doc.selection();
        let (new_text, sel_start, _sel_end) =
            line_ops::column_edit(&tab.doc.text, start, end, payload.0, &payload.1);
        self.column_editor.open = false;
        self.column_editor.error = None;
        self.replace_document_text(new_text, sel_start, sel_start, window, cx);
        self.focus_editor(window, cx);
        self.set_status(tr("status.column_edited"), cx);
    }

    /// Text used for "select next/all occurrences": the selection or the word
    /// at the cursor.
    fn occurrence_target(&self, cx: &gpui_kit::App) -> Option<(String, usize, usize)> {
        let tab = self.active_tab()?;
        if tab.doc.is_huge()
            || tab.large_file
            || tab.doc.partial_range().is_some()
            || tab.doc.is_non_text()
        {
            return None;
        }
        let state = tab.editor.read(cx);
        let range = state.selected_range();
        let text = state.text();
        if range.start != range.end {
            let selected = text.slice(range.clone()).to_string();
            return Some((selected, range.start, range.end));
        }
        let offset = state.cursor().min(text.len());
        let full = text.to_string();
        let char_index = text.byte_to_char_idx(offset);
        let (word_start, word_end) = textutil::word_range_at(&full, char_index);
        if word_start == word_end {
            return None;
        }
        let b0 = text.char_to_byte_idx(word_start);
        let b1 = text.char_to_byte_idx(word_end);
        Some((text.slice(b0..b1).to_string(), b0, b1))
    }

    fn select_next_occurrence(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let Some((needle, _, _)) = self.occurrence_target(cx) else {
            self.set_status(tr("status.occurrence_needs_selection"), cx);
            return;
        };
        if needle.contains(['\n', '\r']) || needle.chars().count() > 200 {
            self.set_status(tr("status.occurrence_needs_single_line"), cx);
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let tab_id = tab.id;
        let text = tab.editor.read(cx).text().to_string();
        let options = luban_core::search::SearchOptions {
            match_case: self.find.match_case,
            wrap_around: false,
            ..Default::default()
        };
        let engine = luban_core::search::SearchEngine::new(needle.clone(), options);
        let from = {
            let state = tab.editor.read(cx);
            match &self.occurrence_anchor {
                Some((anchor_needle, end, anchor_tab))
                    if anchor_needle == &needle && *anchor_tab == tab_id =>
                {
                    state.text().byte_to_char_idx(*end)
                }
                _ => state.text().byte_to_char_idx(state.selected_range().end),
            }
        };
        let next = engine.find_from(&text, from, true, None);
        let editor = tab.editor.clone();
        match next {
            Some(matched) => {
                let (b0, b1) = {
                    let state = tab.editor.read(cx);
                    (
                        state.text().char_to_byte_idx(matched.start),
                        state.text().char_to_byte_idx(matched.end),
                    )
                };
                editor.update(cx, |state, cx| {
                    state.add_selection(b0..b1, cx);
                    state.focus(window, cx);
                });
                self.occurrence_anchor = Some((needle, b1, tab_id));
                self.set_status(tr("status.occurrence_added"), cx);
            }
            None => self.set_status(tr("status.occurrence_none"), cx),
        }
    }

    fn select_all_occurrences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.occurrence_anchor = None;
        self.sync_active(window, cx);
        let Some((needle, _, _)) = self.occurrence_target(cx) else {
            self.set_status(tr("status.occurrence_needs_selection"), cx);
            return;
        };
        if needle.contains(['\n', '\r']) || needle.chars().count() > 200 {
            self.set_status(tr("status.occurrence_needs_single_line"), cx);
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let text = tab.editor.read(cx).text().to_string();
        let options = luban_core::search::SearchOptions {
            match_case: self.find.match_case,
            wrap_around: false,
            ..Default::default()
        };
        let engine = luban_core::search::SearchEngine::new(needle, options);
        let matches = engine.find_all(&text, None, 20_001);
        if matches.is_empty() {
            self.set_status(tr("status.occurrence_none"), cx);
            return;
        }
        if matches.len() > 20_000 {
            self.set_status(tr("status.occurrence_too_many"), cx);
            return;
        }
        let ranges: Vec<(usize, usize)> = {
            let state = tab.editor.read(cx);
            matches
                .iter()
                .map(|matched| {
                    (
                        state.text().char_to_byte_idx(matched.start),
                        state.text().char_to_byte_idx(matched.end),
                    )
                })
                .collect()
        };
        let editor = tab.editor.clone();
        editor.update(cx, |state, cx| {
            state.set_selected_range(ranges[0].0..ranges[0].1, cx);
            for range in ranges.iter().skip(1) {
                state.add_selection(range.0..range.1, cx);
            }
            state.focus(window, cx);
        });
        self.set_status(
            tr_args(
                "status.occurrences_added",
                &[("count", ranges.len().to_string())],
            ),
            cx,
        );
    }

    fn unfold_current(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else { return };
        if tab.doc.is_huge() || tab.large_file {
            self.set_status(tr("status.folding_unavailable"), cx);
            return;
        }
        let editor = tab.editor.clone();
        let unfolded = editor.update(cx, |state, cx| {
            let line = state
                .text()
                .offset_to_point(state.cursor().min(state.text().len()))
                .row;
            state.unfold_at(Position::new(line as u32, 0), cx)
        });
        if unfolded {
            self.set_status(tr("status.unfolded"), cx);
        } else {
            self.set_status(tr("status.no_fold_at_cursor"), cx);
        }
    }

    fn unfold_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else { return };
        if tab.doc.is_huge() || tab.large_file {
            self.set_status(tr("status.folding_unavailable"), cx);
            return;
        }
        let editor = tab.editor.clone();
        editor.update(cx, |state, cx| {
            state.set_folding(false, window, cx);
            state.set_folding(true, window, cx);
        });
        self.set_status(tr("status.unfolded_all"), cx);
    }

    fn bookmark_line(&self) -> Option<usize> {
        let tab = self.active_tab()?;
        if tab.doc.is_huge() || tab.doc.is_non_text() {
            return None;
        }
        Some(line_ops::line_of_char(&tab.doc.text, tab.doc.cursor))
    }

    fn toggle_bookmark(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.full_text_editing_available(window, cx) {
            return;
        }
        let Some(line) = self.bookmark_line() else {
            return;
        };
        let index = self.active;
        let set = {
            let Some(tab) = self.tabs.get_mut(index) else {
                return;
            };
            let set = tab.bookmarks.toggle(line);
            tab.bookmark_revision = tab.bookmark_revision.wrapping_add(1);
            set
        };
        self.refresh_bookmark_decorations(index, cx);
        self.set_status(
            tr(if set {
                "status.bookmark_added"
            } else {
                "status.bookmark_removed"
            }),
            cx,
        );
    }

    fn goto_bookmark(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.full_text_editing_available(window, cx) {
            return;
        }
        let Some(current) = self.bookmark_line() else {
            return;
        };
        let target = {
            let Some(tab) = self.active_tab() else { return };
            if forward {
                tab.bookmarks.next_after(current)
            } else {
                tab.bookmarks.previous_before(current)
            }
        };
        let Some(line) = target else {
            self.set_status(
                tr(if forward {
                    "status.no_next_bookmark"
                } else {
                    "status.no_previous_bookmark"
                }),
                cx,
            );
            return;
        };
        let offset = {
            let Some(tab) = self.active_tab() else { return };
            line_ops::line_start(&tab.doc.text, line)
        };
        self.select_char_range(offset, offset, window, cx);
        self.set_status(
            tr_args("status.jumped_bookmark", &[("line", line.to_string())]),
            cx,
        );
    }

    fn clear_bookmarks(&mut self, cx: &mut Context<Self>) {
        let index = self.active;
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        tab.bookmarks.clear();
        tab.bookmark_revision = tab.bookmark_revision.wrapping_add(1);
        self.refresh_bookmark_decorations(index, cx);
        self.set_status(tr("status.bookmarks_cleared"), cx);
    }

    fn bookmark_lines(
        &mut self,
        action: BookmarkAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.full_text_editing_available(window, cx) || !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        if tab.bookmarks.is_empty() {
            self.set_status(tr("status.no_bookmarks"), cx);
            return;
        }
        let text = tab.doc.text.clone();
        let cursor = tab.doc.cursor;
        let mut bookmarks = tab.bookmarks.clone();
        let index = self.active;
        match action {
            BookmarkAction::Copy => {
                let selected = bookmarks.selected_text(&text);
                cx.write_to_clipboard(ClipboardItem::new_string(selected));
                self.set_status(tr("status.bookmarked_lines_copied"), cx);
            }
            BookmarkAction::Cut => {
                let (clipboard, new_text) = bookmarks.cut_lines(&text);
                cx.write_to_clipboard(ClipboardItem::new_string(clipboard));
                let new_len = textutil::char_count(&new_text);
                let cursor = cursor.min(new_len);
                if let Some(tab) = self.tabs.get_mut(index) {
                    tab.bookmarks.clear();
                    tab.bookmark_revision = tab.bookmark_revision.wrapping_add(1);
                }
                self.replace_document_text(new_text, cursor, cursor, window, cx);
                self.refresh_bookmark_decorations(index, cx);
                self.set_status(tr("status.bookmarked_lines_cut"), cx);
            }
            BookmarkAction::Delete => {
                let new_text = bookmarks.delete_lines(&text);
                let new_len = textutil::char_count(&new_text);
                let cursor = cursor.min(new_len);
                if let Some(tab) = self.tabs.get_mut(index) {
                    tab.bookmarks.clear();
                    tab.bookmark_revision = tab.bookmark_revision.wrapping_add(1);
                }
                self.replace_document_text(new_text, cursor, cursor, window, cx);
                self.refresh_bookmark_decorations(index, cx);
                self.set_status(tr("status.bookmarked_lines_deleted"), cx);
            }
        }
    }

    fn refresh_bookmark_decorations(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.doc.is_huge() || tab.doc.is_non_text() || tab.bookmarks.is_empty() {
            tab.bookmark_decorations.clear(cx);
            return;
        }
        let text = if tab.large_file {
            std::borrow::Cow::Owned(tab.editor.read(cx).text().to_string())
        } else {
            std::borrow::Cow::Borrowed(tab.doc.text.as_str())
        };
        let index_lines = textutil::LineIndex::new(&text);
        let mut color = cx.theme().colors.yellow;
        color.a = 0.22;
        let decorations: Vec<RangeDecoration> = tab
            .bookmarks
            .lines()
            .filter_map(|line| {
                let line = line.saturating_sub(1);
                if line >= index_lines.len() {
                    return None;
                }
                let start = index_lines.line_start(line);
                let end = index_lines.line_end_including_newline(line);
                let b0 = textutil::char_to_byte(&text, start);
                let b1 = textutil::char_to_byte(&text, end);
                (b1 > b0).then(|| {
                    RangeDecoration::new(b0..b1)
                        .with_style(RangeDecorationStyle::Fill)
                        .with_color(color)
                })
            })
            .collect();
        tab.bookmark_decorations.set(decorations, cx);
    }

    pub fn transform_case(
        &mut self,
        mode: textutil::CaseMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_active(window, cx);
        if !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let (start, end) = tab.doc.selection();
        if start == end {
            self.set_status(tr("status.select_for_case"), cx);
            return;
        }
        let (new_text, sel_start, sel_end) =
            textutil::transform_case(&tab.doc.text, start, end, mode);
        self.replace_document_text(new_text, sel_start, sel_end, window, cx);
        self.set_status(tr("status.changed_case"), cx);
    }

    pub fn convert_eol(&mut self, new_eol: Eol, cx: &mut Context<Self>) {
        if self.active_doc().is_some_and(Document::is_non_text) {
            self.set_status(tr("status.eol_non_text"), cx);
            return;
        }
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        if tab.doc.convert_eol(new_eol) {
            self.set_status(
                tr_args("status.eol_set", &[("eol", new_eol.label().to_string())]),
                cx,
            );
        }
    }

    pub fn set_encoding(&mut self, encoding: TextEncoding, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        if tab.doc.set_encoding(encoding) {
            self.set_status(
                tr_args("status.encoding_set", &[("encoding", encoding.label())]),
                cx,
            );
        }
    }

    pub fn set_language(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let language_eol = self.settings.eol_for_new_document(&name);
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        if !tab.doc.set_language(name.clone()) {
            return;
        }
        if tab.doc.path.is_none() && tab.doc.text.is_empty() && !tab.doc.is_non_text() {
            tab.doc.eol = language_eol;
        }
        let key = language_key(&name).to_string();
        tab.highlighted_revision = u64::MAX;
        tab.decorations_key = None;
        if !tab.large_file {
            tab.editor
                .update(cx, |state, cx| state.set_highlighter(key, cx));
        }
        let index = self.active;
        let tab_size = self.settings.tab_size_for(&name);
        let use_spaces = self.settings.use_spaces_for(&name);
        let editor = self.tabs[index].editor.clone();
        if !self.tabs[index].large_file
            && !self.tabs[index].doc.is_huge()
            && !self.tabs[index].doc.is_non_text()
        {
            editor.update(cx, |state, cx| {
                state.set_tab_size(
                    TabSize {
                        tab_size,
                        hard_tabs: !use_spaces,
                    },
                    cx,
                );
            });
            self.apply_fold_candidates(index, cx);
            self.schedule_fold_candidates(index, window, cx);
        }
        self.set_status(
            tr_args(
                "status.language_set",
                &[("language", language_display_name(&name))],
            ),
            cx,
        );
        self.focus_editor(window, cx);
    }

    pub fn zoom(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.settings.font_size = (self.settings.font_size + delta).clamp(6.0, 48.0);
        self.commit_settings();
        cx.notify();
    }

    pub fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.settings.font_size = 14.0;
        self.commit_settings();
        cx.notify();
    }

    pub fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_settings_impl(window, cx, true);
    }

    fn apply_settings_impl(&mut self, window: &mut Window, cx: &mut Context<Self>, persist: bool) {
        apply_theme(&self.settings, Some(window), cx);
        if let Err(error) = syntax_theme::apply(&self.settings, cx) {
            log::warn!("could not apply syntax theme: {error}");
        }
        self.keymap = Keymap::build(&self.settings.keybindings);
        self.rebuild_macro_keymap();
        let line_numbers = self.settings.show_line_numbers;
        let wrap = self.settings.word_wrap;
        let whitespace = self.settings.show_whitespace;
        let indent_guides = self.settings.show_indent_guides;
        for index in 0..self.tabs.len() {
            let wrap = !self.tabs[index].large_file && wrap;
            // Disk-engine tabs draw document-global numbers in the app.
            let line_numbers = line_numbers && !self.tabs[index].doc.is_huge();
            let tab = self.editor_tab_size(&self.tabs[index].doc, self.tabs[index].large_file);
            let plain = !self.tabs[index].doc.is_huge() && !self.tabs[index].large_file;
            self.tabs[index].editor.update(cx, |state, cx| {
                state.set_line_number(line_numbers, window, cx);
                state.set_soft_wrap(wrap, window, cx);
                state.set_tab_size(tab, cx);
                state.set_show_whitespaces(plain && whitespace, window, cx);
                state.set_indent_guides(plain && indent_guides, window, cx);
            });
            self.tabs[index].decorations_key = None;
            self.tabs[index].bracket_key = None;
        }
        if persist {
            self.commit_settings();
        }
        self.explorer.refresh();
        cx.notify();
    }

    /// Persist global-only fields globally and overridable fields either to
    /// the workspace file or to the global settings.
    fn commit_settings(&mut self) {
        let mut global = self.global_settings.clone();
        global.merge_global_fields_from(&self.settings);
        if let Some(path) = self.workspace_file.clone() {
            self.workspace_settings = WorkspaceSettings::capture(&self.settings, &global);
            let workspace = WorkspaceFile {
                version: luban_core::workspace_file::WORKSPACE_VERSION,
                name: self.workspace_name.clone(),
                roots: self.workspace_roots.clone(),
                settings: self.workspace_settings.clone(),
            };
            if let Err(error) = workspace.save_to(&path) {
                log::warn!("could not save workspace {}: {error}", path.display());
            }
            self.global_settings = global;
        } else {
            self.global_settings = self.settings.clone();
        }
        if let Err(err) = self.global_settings.save() {
            log::warn!("could not save settings: {err}");
        }
    }

    fn mark_find_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        if !self.full_text_editing_available(window, cx) {
            return;
        }
        if self.find.matches.is_empty() {
            self.set_status(tr("status.no_matches_to_mark"), cx);
            return;
        }
        let index = self.active;
        let mut lines = HashSet::new();
        {
            let Some(tab) = self.tabs.get(index) else {
                return;
            };
            for matched in &self.find.matches {
                lines.insert(line_ops::line_of_char(&tab.doc.text, matched.start));
            }
        }
        let added = {
            let Some(tab) = self.tabs.get_mut(index) else {
                return;
            };
            let mut added = 0usize;
            for line in lines.iter().copied() {
                if tab.bookmarks.toggle(line) {
                    added += 1;
                }
            }
            tab.bookmark_revision = tab.bookmark_revision.wrapping_add(1);
            added
        };
        self.refresh_bookmark_decorations(index, cx);
        self.set_status(
            tr_args("status.marked_matches", &[("count", added.to_string())]),
            cx,
        );
    }

    fn clear_search_history(&mut self, cx: &mut Context<Self>) {
        self.settings.search_history.clear();
        self.settings.replace_history.clear();
        self.commit_settings();
        self.set_status(tr("status.search_history_cleared"), cx);
        cx.notify();
    }

    fn toggle_macro_recording(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.macro_recording {
            self.macro_recording = false;
            let count = self.macro_recorded.len();
            if count == 0 {
                self.set_status(tr("status.macro_empty"), cx);
            } else {
                self.set_status(
                    tr_args("status.macro_stopped", &[("count", count.to_string())]),
                    cx,
                );
            }
        } else {
            self.macro_recording = true;
            self.macro_recorded.clear();
            self.set_status(tr("status.macro_recording"), cx);
        }
        cx.notify();
    }

    fn save_macro_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.macro_recorded.is_empty() {
            self.set_status(tr("status.macro_empty"), cx);
            return;
        }
        self.macro_ui.reset(window, cx);
        let input = self.macro_ui.name_input.clone();
        cx.defer_in(window, move |_this, window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx));
        });
        cx.notify();
    }

    fn confirm_save_macro(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.macro_ui.name_input.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.macro_ui.error = Some(tr("macro.name_empty"));
            cx.notify();
            return;
        }
        if self.settings.macros.find(&name).is_some()
            && self.macro_ui.overwrite_pending.as_deref() != Some(name.as_str())
        {
            self.macro_ui.overwrite_pending = Some(name.clone());
            self.macro_ui.error = Some(tr_args("macro.overwrite_confirm", &[("name", name)]));
            cx.notify();
            return;
        }
        match self
            .settings
            .macros
            .upsert(&name, self.macro_recorded.clone())
        {
            Ok(()) => {
                self.commit_settings();
                self.macro_ui.open = false;
                self.macro_ui.error = None;
                self.macro_ui.overwrite_pending = None;
                self.macro_recorded.clear();
                self.set_status(
                    tr_args("status.macro_saved", &[("name", name.trim().to_string())]),
                    cx,
                );
                self.rebuild_macro_keymap();
                self.focus_editor(window, cx);
                cx.notify();
            }
            Err(error) => {
                self.macro_ui.error = Some(error);
                cx.notify();
            }
        }
    }

    fn open_macro_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.macro_ui.reset(window, cx);
        cx.notify();
    }

    fn play_last_macro(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = self.settings.macros.macros.last().cloned() else {
            self.set_status(tr("status.no_macros"), cx);
            return;
        };
        self.play_macro(&saved, window, cx);
    }

    fn play_macro(&mut self, saved: &SavedMacro, window: &mut Window, cx: &mut Context<Self>) {
        if self.macro_playback {
            return;
        }
        let commands = saved.commands.clone();
        let name = saved.name.clone();
        self.macro_playback = true;
        let mut failed = 0usize;
        for command in &commands {
            if !self.run_command(command, window, cx) {
                failed += 1;
            }
        }
        self.macro_playback = false;
        self.set_status(
            tr_args(
                "status.macro_played",
                &[
                    ("name", name),
                    ("count", commands.len().to_string()),
                    ("failed", failed.to_string()),
                ],
            ),
            cx,
        );
        cx.notify();
    }

    fn delete_macro(&mut self, name: &str, cx: &mut Context<Self>) {
        self.settings.macros.remove(name);
        self.commit_settings();
        self.rebuild_macro_keymap();
        self.macro_ui.selected = None;
        self.set_status(
            tr_args("status.macro_deleted", &[("name", name.to_string())]),
            cx,
        );
        cx.notify();
    }

    fn rename_macro(&mut self, cx: &mut Context<Self>) {
        let Some(old) = self.macro_ui.selected.clone() else {
            self.macro_ui.error = Some(tr("macro.select_first"));
            cx.notify();
            return;
        };
        let new = self.macro_ui.name_input.read(cx).value().trim().to_string();
        match self.settings.macros.rename(&old, &new) {
            Ok(()) => {
                self.commit_settings();
                self.macro_ui.selected = Some(new.clone());
                self.macro_ui.error = None;
                self.rebuild_macro_keymap();
                self.set_status(tr_args("status.macro_renamed", &[("name", new)]), cx);
                cx.notify();
            }
            Err(error) => {
                self.macro_ui.error = Some(error);
                cx.notify();
            }
        }
    }

    fn assign_macro_shortcut(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.macro_ui.selected.clone() else {
            self.macro_ui.error = Some(tr("macro.select_first"));
            cx.notify();
            return;
        };
        let text = self.macro_ui.shortcut_input.read(cx).value().to_string();
        if text.trim().is_empty() {
            self.settings.macros.remove_binding(&name);
            self.commit_settings();
            self.rebuild_macro_keymap();
            self.macro_ui.error = None;
            self.set_status(tr("status.macro_shortcut_removed"), cx);
            cx.notify();
            return;
        }
        match commands::validate_macro_binding(
            &name,
            &text,
            &self.settings.keybindings,
            &self.settings.macros.bindings,
        ) {
            Ok(formatted) => {
                self.settings.macros.set_binding(&name, formatted);
                self.commit_settings();
                self.rebuild_macro_keymap();
                self.macro_ui.error = None;
                self.set_status(tr("status.macro_shortcut_saved"), cx);
                cx.notify();
            }
            Err(error) => {
                self.macro_ui.error = Some(error.to_string());
                cx.notify();
            }
        }
    }

    fn rebuild_macro_keymap(&mut self) {
        self.macro_keymap = commands::macro_keymap(&self.settings.macros.bindings);
    }

    fn refresh_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inputs = [
            (self.file_path_input.clone(), "dialog.path_placeholder"),
            (self.file_name_input.clone(), "dialog.file_name_placeholder"),
            (self.find.query.clone(), "find.query_placeholder"),
            (
                self.find.replacement.clone(),
                "find.replacement_placeholder",
            ),
            (self.find_in_files.query.clone(), "ff.query_placeholder"),
            (self.find_in_files.folder.clone(), "ff.folder_placeholder"),
            (self.find_in_files.masks.clone(), "ff.masks_placeholder"),
        ];
        for (input, key) in inputs {
            input.update(cx, |state, cx| {
                state.set_placeholder(tr(key), window, cx);
            });
        }
        if let Some(picker) = &self.preferences.font_picker {
            picker.filter.update(cx, |state, cx| {
                state.set_placeholder(tr("prefs.font_search_placeholder"), window, cx);
            });
        }
        if let Some(state) = &self.go_to_line {
            state.input.update(cx, |state, cx| {
                state.set_placeholder(tr("dialog.line_placeholder"), window, cx);
            });
        }
        if let Some(picker) = &self.language_picker {
            picker.filter.update(cx, |state, cx| {
                state.set_placeholder(tr("dialog.filter_languages"), window, cx);
            });
        }
    }

    fn set_ui_language(&mut self, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.ui_language = value.to_string();
        crate::i18n::apply_language(value);
        self.commit_settings();
        for tab in &mut self.tabs {
            if tab.doc.path.is_some() {
                continue;
            }
            let number = tab
                .doc
                .untitled_name
                .rsplit_once(' ')
                .map(|(_, number)| number)
                .filter(|number| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
                .map(str::to_string);
            if let Some(number) = number {
                tab.doc.untitled_name = tr_args("untitled.name", &[("number", number)]);
            }
        }
        self.refresh_placeholders(window, cx);
        // These are pre-formatted snapshots; drop them so the next render
        // starts from the newly selected language.
        self.status.clear();
        self.find.status.clear();
        self.find.error = None;
        self.find_in_files.status.clear();
        if let Some(dialog) = self.file_dialog.as_mut() {
            dialog.error = None;
        }
        self.window_title.clear();
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Find / replace
    // ------------------------------------------------------------------

    pub fn open_find(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.find.open = true;
        self.find.show_replace = replace;
        self.find.dirty = true;
        if self.find.in_selection && self.find.scope.is_none() {
            let (a, b) = self.active_selection();
            if a != b {
                self.find.scope = Some((a, b));
            }
        }
        self.find
            .query
            .update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    pub fn find_next(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_doc().is_some_and(Document::is_huge) {
            self.huge_find_next(forward, window, cx);
            return;
        }
        self.sync_active(window, cx);
        let query = self.find.query_text(cx);
        if query.is_empty() {
            self.open_find(false, window, cx);
            return;
        }
        self.settings.record_search(&query);
        self.commit_settings();
        self.record_navigation(cx);
        if self.find.in_selection && self.find.scope.is_none() {
            let (a, b) = self.active_selection();
            if a != b {
                self.find.scope = Some((a, b));
            } else {
                self.find.status = tr("find.select_scope_first");
                cx.notify();
                return;
            }
        }
        let large = self.active_tab().is_some_and(|tab| tab.large_file);
        if self.find.dirty {
            if large {
                // Typing in the find box never scans a large document.
                self.find.dirty = false;
                self.find.matches.clear();
                self.find.current = None;
                self.find.generation = self.find.generation.wrapping_add(1);
            } else {
                let text = self
                    .tabs
                    .get(self.active)
                    .map(|tab| tab.doc.text.as_str())
                    .unwrap_or("");
                self.find.refresh(text, &query);
                self.find.dirty = false;
            }
        }
        if !self.ensure_active_text(cx) {
            return;
        }
        let (a, b) = self
            .active_doc()
            .map(|doc| doc.selection())
            .unwrap_or((0, 0));
        let scope = if self.find.in_selection {
            self.find.scope
        } else {
            None
        };
        let from = if a != b {
            if forward {
                b
            } else {
                a
            }
        } else {
            b
        };
        let found = {
            let Some(tab) = self.tabs.get(self.active) else {
                return;
            };
            self.find
                .engine(&query)
                .find_from(&tab.doc.text, from, forward, scope)
        };
        self.release_active_text();
        match found {
            Some(matched) => {
                if large {
                    self.find.current = None;
                    self.find.status = tr("find.match_found");
                } else {
                    self.find.current = self.find.matches.iter().position(|m| *m == matched);
                    let count = self.find.matches.len();
                    let position = self.find.current.map(|i| i + 1).unwrap_or(0);
                    self.find.status = if count > 0 {
                        tr_args(
                            "find.position",
                            &[
                                ("position", position.to_string()),
                                ("count", count.to_string()),
                            ],
                        )
                    } else {
                        tr("find.match_found")
                    };
                }
                self.find.generation = self.find.generation.wrapping_add(1);
                self.select_char_range(matched.start, matched.end, window, cx);
                self.focus_editor = true;
                self.record_navigation(cx);
                cx.notify();
            }
            None => {
                self.find.status = tr("find.no_matches");
                cx.notify();
            }
        }
    }

    pub fn replace_one(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_doc().is_some_and(Document::is_huge) {
            self.huge_replace_one(window, cx);
            return;
        }
        self.sync_active(window, cx);
        let query = self.find.query_text(cx);
        let replacement = self.find.replacement_text(cx);
        if query.is_empty() {
            return;
        }
        self.settings.record_search(&query);
        self.settings.record_replacement(&replacement);
        self.commit_settings();
        if !self.ensure_active_text(cx) {
            return;
        }
        let large = self.active_tab().is_some_and(|tab| tab.large_file);
        let (a, b) = self
            .active_doc()
            .map(|doc| doc.selection())
            .unwrap_or((0, 0));
        let scope = self.find.effective_scope(a != b, (a, b));
        let target = {
            let Some(tab) = self.tabs.get(self.active) else {
                return;
            };
            let text = tab.doc.text.as_str();
            let engine = self.find.engine(&query);
            if a != b {
                // Replace the selection when it is itself a match; otherwise
                // advance to the next match. `find_from` stops at the first
                // match, so this stays allocation-free for large files.
                match engine.find_from(text, a, true, scope) {
                    Some(m) if m.start == a && m.end == b => Some(m),
                    _ => engine.find_from(text, b, true, scope),
                }
            } else {
                engine.find_from(text, b, true, scope)
            }
        };
        let Some(matched) = target else {
            self.release_active_text();
            self.find.status = tr("find.no_matches");
            cx.notify();
            return;
        };
        let (expanded, new_cursor, old_total) = {
            let text = self
                .tabs
                .get(self.active)
                .map(|tab| tab.doc.text.as_str())
                .unwrap_or("");
            let expanded = self
                .find
                .engine(&query)
                .replacement_for(text, matched, &replacement);
            let new_cursor = matched.start + expanded.chars().count();
            (
                expanded,
                new_cursor,
                self.tabs[self.active].doc.char_count(),
            )
        };
        if large {
            self.release_active_text();
            self.apply_large_edits(&[(matched, expanded)], new_cursor, old_total, window, cx);
        } else {
            let new_text = {
                let text = self
                    .tabs
                    .get(self.active)
                    .map(|tab| tab.doc.text.as_str())
                    .unwrap_or("");
                self.find.replace_match(text, &query, matched, &replacement)
            };
            self.find.dirty = true;
            self.replace_document_text(new_text, new_cursor, new_cursor, window, cx);
        }
        self.find.status = tr("find.replaced_one");
    }

    /// Apply small targeted replacements directly to a large editor buffer.
    /// Edits run back to front so earlier character positions stay valid.
    #[allow(clippy::too_many_arguments)]
    fn apply_large_edits(
        &mut self,
        edits: &[(Match, String)],
        cursor_after: usize,
        old_total: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let Some(index) = self.tabs.get(self.active).map(|_| self.active) else {
            return old_total;
        };
        let mut new_total = old_total;
        let editor = self.tabs[index].editor.clone();
        for (matched, expanded) in edits.iter().rev() {
            new_total = new_total + expanded.chars().count() - matched.len();
            let (b0, b1) = {
                let state = editor.read(cx);
                let rope = state.text();
                let total = rope.len_chars();
                (
                    rope.char_to_byte_idx(matched.start.min(total)),
                    rope.char_to_byte_idx(matched.end.min(total)),
                )
            };
            editor.update(cx, |state, cx| {
                state.set_selected_range(b0..b1, cx);
                state.replace(expanded.clone(), window, cx);
            });
        }
        let cursor_byte = {
            let state = editor.read(cx);
            state
                .text()
                .char_to_byte_idx(cursor_after.min(state.text().len_chars()))
        };
        editor.update(cx, |state, cx| {
            state.set_selected_range(cursor_byte..cursor_byte, cx);
            state.focus(window, cx);
        });
        let tab = &mut self.tabs[index];
        tab.doc
            .record_external_replacement(new_total, cursor_after, cursor_after);
        tab.highlighted_revision = u64::MAX;
        tab.decorations_key = None;
        tab.needs_sync = false;
        new_total
    }

    pub fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_doc().is_some_and(Document::is_huge) {
            self.huge_replace_all(window, cx);
            return;
        }
        self.sync_active(window, cx);
        let query = self.find.query_text(cx);
        let replacement = self.find.replacement_text(cx);
        if query.is_empty() {
            return;
        }
        self.settings.record_search(&query);
        self.settings.record_replacement(&replacement);
        self.commit_settings();
        if !self.ensure_active_text(cx) {
            return;
        }
        let preview = {
            let text = self
                .tabs
                .get(self.active)
                .map(|tab| tab.doc.text.as_str())
                .unwrap_or("");
            self.find
                .engine(&query)
                .preview_replace(text, &replacement, self.find.scope, 5)
        };
        self.release_active_text();
        match preview {
            Ok(preview) => {
                self.replace_preview = Some(ReplacePreviewState { preview });
                cx.notify();
            }
            Err(err) => {
                self.find.error = Some(err);
                cx.notify();
            }
        }
    }

    fn confirm_replace_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let empty = self
            .replace_preview
            .as_ref()
            .is_some_and(|state| state.preview.count == 0);
        self.replace_preview = None;
        if empty {
            cx.notify();
            return;
        }
        self.apply_replace_all(window, cx);
    }

    fn apply_replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_doc().is_some_and(Document::is_huge) {
            self.huge_replace_all(window, cx);
            return;
        }
        self.sync_active(window, cx);
        let query = self.find.query_text(cx);
        let replacement = self.find.replacement_text(cx);
        if query.is_empty() {
            return;
        }
        self.settings.record_search(&query);
        self.settings.record_replacement(&replacement);
        self.commit_settings();
        if !self.ensure_active_text(cx) {
            return;
        }
        let large = self.active_tab().is_some_and(|tab| tab.large_file);
        if large {
            let (edits, old_total, cursor) = {
                let text = self
                    .tabs
                    .get(self.active)
                    .map(|tab| tab.doc.text.as_str())
                    .unwrap_or("");
                let engine = self.find.engine(&query);
                let edits: Vec<(Match, String)> = engine
                    .find_all(text, self.find.scope, usize::MAX)
                    .into_iter()
                    .map(|matched| (matched, engine.replacement_for(text, matched, &replacement)))
                    .collect();
                let cursor = edits.first().map(|(matched, _)| matched.start).unwrap_or(0);
                (edits, self.tabs[self.active].doc.char_count(), cursor)
            };
            if edits.is_empty() {
                self.release_active_text();
                self.find.status = tr_args("find.replaced_other", &[("count", "0".to_string())]);
                cx.notify();
                return;
            }
            let count = edits.len();
            self.release_active_text();
            self.apply_large_edits(&edits, cursor, old_total, window, cx);
            self.find.status = tr_args("find.replaced_other", &[("count", count.to_string())]);
            cx.notify();
            return;
        }
        let replaced = {
            let text = self
                .tabs
                .get(self.active)
                .map(|tab| tab.doc.text.as_str())
                .unwrap_or("");
            self.find.replace_all(text, &query, &replacement)
        };
        match replaced {
            Ok((new_text, count)) => {
                let cursor = self
                    .find
                    .matches
                    .first()
                    .map(|m| m.start)
                    .unwrap_or(0)
                    .min(textutil::char_count(&new_text));
                self.find.dirty = true;
                self.replace_document_text(new_text, cursor, cursor, window, cx);
                self.find.status = tr_args("find.replaced_other", &[("count", count.to_string())]);
                cx.notify();
            }
            Err(err) => {
                self.release_active_text();
                self.find.error = Some(err);
                cx.notify();
            }
        }
    }

    // ------------------------------------------------------------------
    // Find in files
    // ------------------------------------------------------------------

    pub fn open_workspace_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find_in_files.workspace = !self.workspace_roots.is_empty();
        self.find_in_files.show_replace = true;
        self.find_in_files.respect_gitignore = self.settings.respect_gitignore;
        self.find_in_files.open = true;
        self.find_in_files
            .replacement
            .update(cx, |input, cx| input.focus(window, cx));
        self.find_in_files
            .query
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// Signature of every input the replace preview depends on.
    fn replace_signature(&self, cx: &gpui_kit::App) -> String {
        let panel = &self.find_in_files;
        format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            panel.query_text(cx),
            panel.replacement_text(cx),
            panel.include_text(cx),
            panel.exclude_text(cx),
            panel.match_case,
            panel.whole_word,
            panel.regex,
            panel.enhanced_regex,
            panel.workspace,
            panel.respect_gitignore,
        )
    }

    /// Drop a preview whose inputs changed after it was computed.
    fn invalidate_stale_preview(&mut self, cx: &mut Context<Self>) {
        if self.replace_state.preview.is_some()
            && self.replace_state.preview_signature != self.replace_signature(cx)
        {
            self.replace_state.preview = None;
            self.replace_state.status = tr("replace.preview_stale");
            cx.notify();
        }
    }

    /// Compute the workspace replace preview in the background.
    pub fn start_workspace_replace_preview(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.replace_state.running {
            return;
        }
        let query = self.find_in_files.query_text(cx);
        let replacement = self.find_in_files.replacement_text(cx);
        if query.is_empty() {
            self.replace_state.error = Some(tr("ff.enter_query"));
            self.replace_state.status = tr("ff.enter_query");
            cx.notify();
            return;
        }
        let filter = match self.find_in_files.filter(cx) {
            Ok(filter) => filter,
            Err(error) => {
                self.replace_state.error = Some(error.clone());
                self.replace_state.status = error;
                cx.notify();
                return;
            }
        };
        if self.workspace_roots.is_empty() {
            self.replace_state.error = Some(tr("ff.no_workspace"));
            self.replace_state.status = tr("ff.no_workspace");
            cx.notify();
            return;
        }
        let roots = self.workspace_roots.clone();
        let search_options = self.find_in_files.search_options();
        let max_file_size = self.settings.max_file_size_bytes();
        let respect_gitignore = self.find_in_files.respect_gitignore;
        let include_hidden = self.settings.show_hidden_files;
        let mut skipped_open: Vec<(PathBuf, String)> = Vec::new();
        for tab in &self.tabs {
            let Some(path) = &tab.doc.path else { continue };
            let reason = if tab.doc.is_huge() {
                Some(tr("replace.skip_disk_engine"))
            } else if tab.doc.partial_range().is_some() {
                Some(tr("replace.skip_partial"))
            } else if tab.doc.is_non_text() {
                Some(tr("replace.skip_non_text"))
            } else if tab.large_file {
                Some(tr("replace.skip_large"))
            } else {
                None
            };
            if let Some(reason) = reason {
                skipped_open.push((path.clone(), reason));
            }
        }
        self.replace_state.running = true;
        self.replace_state.error = None;
        self.replace_state.preview = None;
        self.replace_state.confirmed = false;
        self.replace_state.generation = self.replace_state.generation.wrapping_add(1);
        let generation = self.replace_state.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.replace_state.cancel = Some(cancel.clone());
        self.replace_state.query = query.clone();
        self.replace_state.replacement = replacement.clone();
        self.replace_state.options = Some(search_options);
        self.replace_state.filter = filter.clone();
        self.replace_state.preview_signature = self.replace_signature(cx);
        self.replace_state.status = tr("replace.previewing");
        cx.notify();

        let task = cx.background_executor().spawn(async move {
            let mut progress = |_files: usize| {};
            let options = WorkspaceSearchOptions {
                roots: roots.clone(),
                query: query.clone(),
                options: search_options,
                filter,
                respect_gitignore,
                include_hidden,
                max_results: 20_000,
                max_file_size,
            };
            let outcome = match search_workspace(&options, &cancel, &mut progress) {
                Ok(outcome) => outcome,
                Err(error) => return Err(error.to_string()),
            };
            let mut files: Vec<FilePreview> = Vec::new();
            let mut skipped: Vec<SkippedFile> = Vec::new();
            let mut seen: Vec<PathBuf> = Vec::new();
            for matched in &outcome.matches {
                if seen.iter().any(|path| path == &matched.path) {
                    continue;
                }
                seen.push(matched.path.clone());
                if let Some((_, reason)) =
                    skipped_open.iter().find(|(path, _)| path == &matched.path)
                {
                    skipped.push(SkippedFile {
                        path: matched.path.clone(),
                        reason: reason.clone(),
                    });
                    continue;
                }
                let bytes = match std::fs::read(&matched.path) {
                    Ok(bytes) => bytes,
                    Err(_) => continue,
                };
                if encoding::looks_binary(&bytes) {
                    skipped.push(SkippedFile {
                        path: matched.path.clone(),
                        reason: crate::i18n::tr("replace.skip_binary"),
                    });
                    continue;
                }
                let decoded = encoding::decode(&bytes);
                let engine = luban_core::search::SearchEngine::new(query.clone(), search_options);
                let (replaced, count) = match engine.replace_all(&decoded.text, &replacement, None)
                {
                    Ok(result) => result,
                    Err(_) => continue,
                };
                if count == 0 || replaced == decoded.text {
                    continue;
                }
                let samples: Vec<(String, String)> = outcome
                    .matches
                    .iter()
                    .filter(|candidate| candidate.path == matched.path)
                    .take(5)
                    .filter_map(|candidate| {
                        workspace_replace::replacement_sample(
                            &candidate.line_text,
                            &query,
                            search_options,
                            &replacement,
                        )
                    })
                    .collect();
                files.push(FilePreview {
                    path: matched.path.clone(),
                    relative: workspace_replace::relative_path(&roots, &matched.path),
                    matches: count,
                    samples,
                    fingerprint: workspace_replace::fingerprint(&bytes),
                    size: bytes.len() as u64,
                });
            }
            let total = files.iter().map(|file| file.matches).sum();
            Ok::<WorkspaceReplacePreview, String>(WorkspaceReplacePreview {
                files,
                skipped,
                total_matches: total,
                truncated: outcome.truncated,
            })
        });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let _ = this.update(cx, |app, cx| {
                if app.replace_state.generation != generation {
                    return;
                }
                app.replace_state.running = false;
                app.replace_state.cancel = None;
                match outcome {
                    Ok(preview) => {
                        app.replace_state.status = tr_args(
                            "replace.preview_ready",
                            &[
                                ("files", preview.file_count().to_string()),
                                ("matches", preview.total_matches.to_string()),
                                ("skipped", preview.skipped.len().to_string()),
                            ],
                        );
                        if preview.confirmable() {
                            app.replace_state.error = None;
                        } else {
                            app.replace_state.error = Some(tr("replace.no_matches"));
                        }
                        app.replace_state.preview = Some(preview);
                    }
                    Err(error) => {
                        app.replace_state.error = Some(error.clone());
                        app.replace_state.status = error;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn confirm_workspace_replace(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.invalidate_stale_preview(cx);
        let Some(preview) = &self.replace_state.preview else {
            self.replace_state.error = Some(tr("replace.preview_required"));
            cx.notify();
            return;
        };
        if !preview.confirmable() {
            return;
        }
        self.confirm = Some(ConfirmState::new(
            CloseIntent::WorkspaceReplace,
            tr("replace.confirm_title"),
            tr_args(
                "replace.confirm_detail",
                &[
                    ("files", preview.file_count().to_string()),
                    ("matches", preview.total_matches.to_string()),
                ],
            ),
        ));
        cx.notify();
    }

    /// Apply the computed preview after re-reading every file and comparing
    /// its fingerprint. In-memory buffers only; nothing is written to disk.
    fn apply_workspace_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preview) = self.replace_state.preview.clone() else {
            return;
        };
        let query = self.replace_state.query.clone();
        let replacement = self.replace_state.replacement.clone();
        let options = self
            .replace_state
            .options
            .unwrap_or_else(|| self.find_in_files.search_options());
        let engine = luban_core::search::SearchEngine::new(query, options);
        if engine.error().is_some() {
            return;
        }
        let mut undo = ReplaceUndo::default();
        let mut modified = 0usize;
        let mut skipped = preview.skipped.clone();
        for file in &preview.files {
            let bytes = match std::fs::read(&file.path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    skipped.push(SkippedFile {
                        path: file.path.clone(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            if bytes.len() as u64 != file.size
                || workspace_replace::fingerprint(&bytes) != file.fingerprint
            {
                skipped.push(SkippedFile {
                    path: file.path.clone(),
                    reason: tr("replace.skip_changed"),
                });
                continue;
            }
            let decoded = encoding::decode(&bytes);
            let (replaced, count) = match engine.replace_all(&decoded.text, &replacement, None) {
                Ok(result) => result,
                Err(_) => continue,
            };
            if count == 0 {
                continue;
            }
            let new_text = eol::to_lf_owned(replaced);
            if let Some(index) = self.tabs.iter().position(|tab| {
                tab.doc
                    .path
                    .as_ref()
                    .is_some_and(|path| util::same_path(path, &file.path))
            }) {
                let tab = &self.tabs[index];
                if tab.doc.is_huge()
                    || tab.large_file
                    || tab.doc.partial_range().is_some()
                    || tab.doc.is_non_text()
                {
                    skipped.push(SkippedFile {
                        path: file.path.clone(),
                        reason: tr("replace.skip_open_mode"),
                    });
                    continue;
                }
                undo.entries.push(UndoEntry {
                    tab_id: Some(tab.id),
                    path: tab.doc.path.clone(),
                    text: tab.doc.text.clone(),
                    modified: tab.doc.is_modified(),
                    cursor: tab.doc.cursor,
                    anchor: tab.doc.anchor,
                    was_open: true,
                });
                self.apply_replacement_to_tab(index, new_text, window, cx);
                modified += 1;
            } else {
                let mut doc = match Document::open_with_limit(&file.path, u64::MAX) {
                    Ok(doc) => doc,
                    Err(error) => {
                        skipped.push(SkippedFile {
                            path: file.path.clone(),
                            reason: error.to_string(),
                        });
                        continue;
                    }
                };
                if doc.is_non_text() {
                    skipped.push(SkippedFile {
                        path: file.path.clone(),
                        reason: tr("replace.skip_non_text"),
                    });
                    continue;
                }
                doc.attach_text(new_text);
                doc.touch();
                let path = file.path.clone();
                self.add_document(doc, window, cx);
                undo.entries.push(UndoEntry {
                    tab_id: self.tabs.last().map(|tab| tab.id),
                    path: Some(path),
                    text: String::new(),
                    modified: false,
                    cursor: 0,
                    anchor: 0,
                    was_open: false,
                });
                modified += 1;
            }
        }
        self.replace_state.undo = Some(undo);
        self.replace_state.preview = None;
        self.replace_state.status = tr_args(
            "replace.applied",
            &[
                ("count", modified.to_string()),
                ("skipped", skipped.len().to_string()),
            ],
        );
        self.replace_state.error = None;
        self.set_status(self.replace_state.status.clone(), cx);
        cx.notify();
    }

    fn apply_replacement_to_tab(
        &mut self,
        index: usize,
        new_text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        let total = tab.doc.char_count();
        let cursor = tab.doc.cursor.min(total);
        let anchor = tab.doc.anchor.min(total);
        tab.doc
            .apply_edit(0, total, &new_text, EditKind::Other, cursor, anchor);
        tab.highlighted_revision = u64::MAX;
        tab.decorations_key = None;
        tab.bracket_key = None;
        tab.needs_sync = false;
        tab.pending_edit = false;
        let char_count = textutil::char_count(&new_text);
        let cursor = cursor.min(char_count);
        let anchor = anchor.min(char_count);
        let b0 = textutil::char_to_byte(&new_text, anchor.min(cursor));
        let b1 = textutil::char_to_byte(&new_text, anchor.max(cursor));
        let editor = tab.editor.clone();
        let doc_cursor = cursor;
        let doc_anchor = anchor;
        self.tabs[index].doc.set_selection(doc_anchor, doc_cursor);
        editor.update(cx, |state, cx| {
            state.replace_all(new_text, window, cx);
            state.set_selected_range(b0..b1, cx);
        });
    }

    pub fn undo_workspace_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(undo) = self.replace_state.undo.take() else {
            self.set_status(tr("replace.nothing_to_undo"), cx);
            return;
        };
        for entry in undo.entries.into_iter().rev() {
            if !entry.was_open {
                if let Some(path) = &entry.path {
                    if let Some(index) = self.tabs.iter().position(|tab| {
                        tab.doc
                            .path
                            .as_ref()
                            .is_some_and(|open| util::same_path(open, path))
                    }) {
                        self.close_tab_by_id_without_prompt(self.tabs[index].id, cx);
                    }
                }
                continue;
            }
            let index = entry
                .tab_id
                .and_then(|id| self.tab_index_by_id(id))
                .or_else(|| {
                    entry.path.as_ref().and_then(|path| {
                        self.tabs.iter().position(|tab| {
                            tab.doc
                                .path
                                .as_ref()
                                .is_some_and(|open| util::same_path(open, path))
                        })
                    })
                });
            let index = match index {
                Some(index) => index,
                None => {
                    let mut doc = Document::with_text(entry.text.clone());
                    doc.path = entry.path.clone();
                    doc.touch();
                    self.add_document(doc, window, cx);
                    self.tabs.len() - 1
                }
            };
            let total = self.tabs[index].doc.char_count();
            let old_modified = entry.modified;
            {
                let tab = &mut self.tabs[index];
                tab.doc.apply_edit(
                    0,
                    total,
                    &entry.text,
                    EditKind::Other,
                    entry.cursor.min(textutil::char_count(&entry.text)),
                    entry.anchor.min(textutil::char_count(&entry.text)),
                );
                if !old_modified {
                    tab.doc.mark_saved();
                }
                tab.highlighted_revision = u64::MAX;
                tab.decorations_key = None;
                tab.bracket_key = None;
                tab.needs_sync = false;
                tab.pending_edit = false;
            }
            let text = entry.text.clone();
            let total_chars = textutil::char_count(&text);
            let cursor = entry.cursor.min(total_chars);
            let anchor = entry.anchor.min(total_chars);
            let b0 = textutil::char_to_byte(&text, anchor.min(cursor));
            let b1 = textutil::char_to_byte(&text, anchor.max(cursor));
            let editor = self.tabs[index].editor.clone();
            self.tabs[index].doc.set_selection(anchor, cursor);
            editor.update(cx, |state, cx| {
                state.replace_all(text, window, cx);
                state.set_selected_range(b0..b1, cx);
            });
        }
        self.replace_state.status = tr("replace.undone");
        self.set_status(tr("replace.undone"), cx);
        cx.notify();
    }

    pub fn open_find_in_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find_in_files.open = true;
        self.find_in_files.respect_gitignore = self.settings.respect_gitignore;
        if self.find_in_files.folder.read(cx).value().is_empty() {
            let folder = self
                .workspace_root
                .clone()
                .or_else(|| {
                    self.active_doc()
                        .and_then(|d| d.path.as_ref())
                        .and_then(|p| p.parent())
                        .map(Path::to_path_buf)
                })
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            let value = folder.display().to_string();
            self.find_in_files
                .folder
                .update(cx, |state, cx| state.set_value(value, window, cx));
        }
        self.find_in_files
            .query
            .update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    pub fn start_find_in_files(&mut self, cx: &mut Context<Self>) {
        let query = self.find_in_files.query_text(cx);
        if query.is_empty() {
            self.find_in_files.status = tr("ff.enter_query");
            cx.notify();
            return;
        }
        self.settings.record_search(&query);
        self.commit_settings();
        if self.find_in_files.open_documents {
            self.search_open_documents(cx);
            return;
        }
        let filter = match self.find_in_files.filter(cx) {
            Ok(filter) => filter,
            Err(error) => {
                self.find_in_files.error = Some(error.clone());
                self.find_in_files.status = error;
                cx.notify();
                return;
            }
        };
        self.find_in_files.error = None;
        self.find_in_files.generation += 1;
        let generation = self.find_in_files.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.find_in_files.cancel = Some(cancel.clone());
        self.find_in_files.running = true;
        self.find_in_files.clear_results();
        self.find_in_files.status = tr("ff.searching");
        cx.notify();

        if self.find_in_files.workspace {
            if self.workspace_roots.is_empty() {
                self.find_in_files.running = false;
                self.find_in_files.cancel = None;
                self.find_in_files.status = tr("ff.no_workspace");
                cx.notify();
                return;
            }
            let options = WorkspaceSearchOptions {
                roots: self.workspace_roots.clone(),
                query,
                options: self.find_in_files.search_options(),
                filter,
                respect_gitignore: self.find_in_files.respect_gitignore,
                include_hidden: self.settings.show_hidden_files,
                max_results: 20_000,
                max_file_size: self.settings.max_file_size_bytes(),
            };
            let cancel_for_task = cancel.clone();
            let task = cx.background_executor().spawn(async move {
                let mut progress = |_files: usize| {};
                search_workspace(&options, &cancel_for_task, &mut progress)
            });
            cx.spawn(async move |this, cx| {
                let outcome = task.await;
                this.update(cx, |this, cx| {
                    this.finish_find_in_files(outcome, generation, cx);
                })
                .ok();
            })
            .detach();
            return;
        }
        let folder = self.find_in_files.folder_path(cx);
        if !folder.is_dir() {
            self.find_in_files.running = false;
            self.find_in_files.cancel = None;
            self.find_in_files.status =
                tr_args("ff.not_a_folder", &[("path", folder.display().to_string())]);
            cx.notify();
            return;
        }
        let options = FindInFilesOptions {
            query,
            folder,
            masks: self.find_in_files.masks_text(cx),
            options: self.find_in_files.search_options(),
            max_results: 2000,
            max_file_size: 16 * 1024 * 1024,
            include_hidden: false,
            skip_common_ignored_folders: true,
        };
        let cancel_for_task = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            let mut progress = |_files: usize| {};
            luban_core::workspace::search_in_files(&options, &cancel_for_task, &mut progress)
        });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| {
                this.finish_find_in_files(outcome, generation, cx);
            })
            .ok();
        })
        .detach();
    }

    fn search_open_documents(&mut self, cx: &mut Context<Self>) {
        let query = self.find_in_files.query_text(cx);
        let engine =
            luban_core::search::SearchEngine::new(query, self.find_in_files.search_options());
        if let Some(error) = engine.error() {
            self.find_in_files.clear_results();
            self.find_in_files.status = error.to_string();
            cx.notify();
            return;
        }
        let mut matches = Vec::new();
        let mut scanned = 0usize;
        let mut files_with_matches = HashSet::new();
        let mut truncated = false;
        for tab in &self.tabs {
            if tab.doc.is_huge() || tab.doc.partial_range().is_some() || tab.doc.is_non_text() {
                continue;
            }
            let Some(path) = tab.doc.path.clone() else {
                continue;
            };
            scanned += 1;
            let before = matches.len();
            let text = if tab.large_file {
                tab.editor.read(cx).text().to_string()
            } else {
                tab.doc.text.clone()
            };
            let index = textutil::LineIndex::new(&text);
            for matched in engine.find_all(&text, None, 2000 - matches.len()) {
                let (line, column) = index.line_col(matched.start);
                matches.push(FileMatch {
                    path: path.clone(),
                    line,
                    column,
                    line_text: index.line_text(&text, line - 1),
                    match_text: textutil::slice_chars(&text, matched.start, matched.end),
                    match_len: matched.len(),
                });
                if matches.len() >= 2000 {
                    truncated = true;
                    break;
                }
            }
            if matches.len() > before {
                files_with_matches.insert(path);
            }
            if truncated {
                break;
            }
        }
        self.find_in_files.generation += 1;
        let generation = self.find_in_files.generation;
        self.find_in_files.running = false;
        self.find_in_files.cancel = None;
        self.finish_find_in_files(
            Ok(FindInFilesOutcome {
                matches,
                files_scanned: scanned,
                files_with_matches: files_with_matches.len(),
                truncated,
                cancelled: false,
            }),
            generation,
            cx,
        );
    }

    fn finish_find_in_files(
        &mut self,
        outcome: Result<FindInFilesOutcome, luban_core::Error>,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        if generation != self.find_in_files.generation {
            return;
        }
        self.find_in_files.running = false;
        self.find_in_files.cancel = None;
        match outcome {
            Ok(outcome) => {
                self.find_in_files.files_scanned = outcome.files_scanned;
                self.find_in_files.truncated = outcome.truncated;
                self.find_in_files.cancelled = outcome.cancelled;
                let roots = self.workspace_roots.clone();
                self.find_in_files.set_results(outcome.matches, &roots);
                self.find_in_files.status = if outcome.cancelled {
                    tr("ff.cancelled")
                } else if outcome.truncated {
                    tr_args(
                        "ff.matches_in_files_limited",
                        &[
                            ("matches", self.find_in_files.results.len().to_string()),
                            ("files", outcome.files_with_matches.to_string()),
                        ],
                    )
                } else {
                    tr_args(
                        "ff.matches_in_files",
                        &[
                            ("matches", self.find_in_files.results.len().to_string()),
                            ("files", outcome.files_with_matches.to_string()),
                            ("scanned", outcome.files_scanned.to_string()),
                        ],
                    )
                };
            }
            Err(err) => {
                self.find_in_files.results.clear();
                self.find_in_files.status =
                    tr_args("ff.search_failed", &[("error", err.to_string())]);
            }
        }
        cx.notify();
    }

    pub fn cancel_find_in_files(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = &self.find_in_files.cancel {
            cancel.store(true, Ordering::Relaxed);
            self.find_in_files.status = tr("ff.cancelling");
            cx.notify();
        }
    }

    fn open_file_match(&mut self, matched: FileMatch, window: &mut Window, cx: &mut Context<Self>) {
        self.open_path_at(
            &matched.path,
            matched.line,
            matched.column,
            matched.match_len,
            window,
            cx,
        );
    }

    // ------------------------------------------------------------------
    // Decorations
    // ------------------------------------------------------------------

    fn refresh_decorations(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.get(self.active).map(|_| self.active) else {
            return;
        };
        let large = self.tabs[index].large_file;
        let key = {
            let tab = &self.tabs[index];
            (
                tab.doc.revision(),
                tab.doc.cursor,
                self.find.generation,
                self.find.open,
                self.settings.highlight_matching_brackets,
                large,
            )
        };
        if self.tabs[index].decorations_key == Some(key) {
            return;
        }
        let current = self.find.current;
        let matches = if self.find.open && !large {
            self.find.matches.clone()
        } else {
            Vec::new()
        };
        // Bracket matching walks the whole buffer; large files skip it.
        let bracket = if self.settings.highlight_matching_brackets && !large {
            textutil::matching_bracket(&self.tabs[index].doc.text, self.tabs[index].doc.cursor)
        } else {
            None
        };
        let editor = self.tabs[index].editor.clone();
        let mut decorations: Vec<RangeDecoration> = Vec::new();
        {
            let state = editor.read(cx);
            let rope = state.text();
            let total = rope.len_chars();
            for (match_index, matched) in matches.iter().enumerate() {
                let b0 = rope.char_to_byte_idx(matched.start.min(total));
                let b1 = rope.char_to_byte_idx(matched.end.min(total));
                if b0 == b1 {
                    continue;
                }
                let color = if current == Some(match_index) {
                    rgba(0x2F81F7AA)
                } else {
                    rgba(0xF2C94C55)
                };
                decorations.push(
                    RangeDecoration::new(b0..b1)
                        .with_style(RangeDecorationStyle::Fill)
                        .with_color(color.into()),
                );
            }
            if let Some((a, b)) = bracket {
                let b0 = rope.char_to_byte_idx(a.min(total));
                let b1 = rope.char_to_byte_idx((b + 1).min(total));
                decorations.push(
                    RangeDecoration::new(b0..b1)
                        .with_style(RangeDecorationStyle::Frame)
                        .with_color(rgba(0x2F81F799).into()),
                );
            }
        }
        let tab = &mut self.tabs[index];
        tab.decorations.set(decorations, cx);
        tab.decorations_key = Some(key);
        tab.highlighted_revision = tab.doc.revision();
        self.refresh_bracket_decorations(index, cx);
    }

    /// Recolour paired brackets when the document or the setting changed.
    fn refresh_bracket_decorations(&mut self, index: usize, cx: &mut Context<Self>) {
        let enabled = self.settings.bracket_colors;
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let plain = !tab.doc.is_huge()
            && !tab.large_file
            && tab.doc.partial_range().is_none()
            && !tab.doc.is_non_text();
        let supported = enabled
            && plain
            && tab.doc.text.len() <= luban_core::bracket_colors::MAX_BRACKET_COLOR_BYTES;
        let key = (tab.doc.revision(), supported);
        if tab.bracket_key == Some(key) {
            return;
        }
        if !supported {
            tab.bracket_decorations.clear(cx);
            if let Some(tab) = self.tabs.get_mut(index) {
                tab.bracket_key = Some(key);
            }
            return;
        }
        let colors = [
            rgba(0xE0A458FF),
            rgba(0x6CB6FFFF),
            rgba(0x8BD49CFF),
            rgba(0xE57BB8FF),
        ];
        let pairs = luban_core::bracket_colors::scan_brackets(&tab.doc.text);
        let mut decorations = Vec::with_capacity(pairs.len() * 2);
        for pair in pairs {
            let color =
                colors[luban_core::bracket_colors::depth_color_index(pair.depth, colors.len())];
            for offset in [pair.open, pair.close] {
                decorations.push(TextDecoration::new(
                    offset..offset + 1,
                    HighlightStyle {
                        color: Some(color.into()),
                        ..HighlightStyle::default()
                    },
                ));
            }
        }
        tab.bracket_decorations.set(decorations, cx);
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.bracket_key = Some(key);
        }
    }

    fn process_enter_actions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_go_to_line {
            self.pending_go_to_line = false;
            self.apply_go_to_line(window, cx);
        }
        if self.pending_file_confirm {
            self.pending_file_confirm = false;
            self.file_dialog_confirm(window, cx);
        }
        if self.pending_replace {
            self.pending_replace = false;
            self.replace_one(window, cx);
        }
        if self.pending_find_next {
            self.pending_find_next = false;
            self.find_next(true, window, cx);
        }
        if self.pending_name_confirm {
            self.pending_name_confirm = false;
            self.confirm_name_prompt(window, cx);
        }
    }

    fn refresh_find(&mut self, cx: &mut Context<Self>) {
        if !self.find.open || !self.find.dirty {
            return;
        }
        let query = self.find.query_text(cx);
        if self.active_tab().is_some_and(|tab| tab.large_file) {
            // A query change invalidates an in-flight scan.
            if let Some(ui) = self.tabs.get(self.active).and_then(|tab| tab.huge.as_ref()) {
                if let Some(job) = &ui.search {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
            // Large files only search when the user asks for it.
            self.find.dirty = false;
            self.find.matches.clear();
            self.find.huge_results.clear();
            self.find.huge_truncated = false;
            self.find.current = None;
            self.find.error = None;
            self.find.generation = self.find.generation.wrapping_add(1);
            if query.is_empty() {
                self.find.status.clear();
            } else {
                self.find.status = tr("find.large_file_hint");
            }
            return;
        }
        let text = self
            .tabs
            .get(self.active)
            .map(|tab| tab.doc.text.as_str())
            .unwrap_or("");
        self.find.refresh(text, &query);
        self.find.dirty = false;
    }

    // ------------------------------------------------------------------
    // Commands, local history and shortcuts
    // ------------------------------------------------------------------

    fn command_context(&self) -> CommandContext {
        let tab = self.active_tab();
        let (can_undo, can_redo) = match tab {
            Some(tab) => match tab.doc.huge() {
                Some(state) => state
                    .engine
                    .try_read()
                    .map(|engine| (engine.can_undo(), engine.can_redo()))
                    .unwrap_or((false, false)),
                None => (tab.doc.undo.can_undo(), tab.doc.undo.can_redo()),
            },
            None => (false, false),
        };
        CommandContext {
            has_document: tab.is_some(),
            has_path: tab.is_some_and(|tab| tab.doc.path.is_some()),
            can_save: tab.is_some(),
            can_undo,
            can_redo,
            can_edit_full_text: tab.is_some_and(|tab| {
                !tab.doc.is_huge()
                    && !tab.large_file
                    && tab.doc.partial_range().is_none()
                    && !tab.doc.is_non_text()
            }),
            can_go_back: self.nav.can_back(),
            can_go_forward: self.nav.can_forward(),
        }
    }

    /// A cheap fingerprint of state a macro can reasonably replay.
    ///
    /// Tab revisions catch document edits, and the serialized settings catch
    /// view/theme/preference changes. Commands that only move a caret or open
    /// a transient overlay do not add a macro step.
    fn macro_state_signature(&self) -> (usize, usize, Vec<u64>, Vec<bool>, String) {
        (
            self.tabs.len(),
            self.active,
            self.tabs.iter().map(|tab| tab.doc.revision()).collect(),
            self.tabs.iter().map(|tab| tab.doc.is_modified()).collect(),
            serde_json::to_string(&self.settings).unwrap_or_default(),
        )
    }

    /// Execute a registered command. Menu items, the toolbar, shortcuts and
    /// the command palette all end up here.
    pub fn run_command(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(spec) = commands::spec(id) else {
            log::warn!("unknown command {id}");
            return false;
        };
        if !spec.enabled(&self.command_context()) {
            return false;
        }
        if id != "edit.select-next-occurrence" {
            self.occurrence_anchor = None;
        }
        let recording_before =
            (self.macro_recording && !self.macro_playback && luban_core::macros::recordable(id))
                .then(|| self.macro_state_signature());
        match id {
            "file.new" => self.new_file(window, cx),
            "file.new-window" => self.new_window(cx),
            "file.open" => self.open_file_dialog(window, cx),
            "file.open-folder" => self.open_folder_dialog(window, cx),
            "file.open-workspace" => self.open_workspace_dialog(window, cx),
            "file.save-workspace" => self.save_workspace(window, cx),
            "file.save-workspace-as" => self.save_workspace_as(window, cx),
            "file.add-folder" => self.open_add_folder_dialog(window, cx),
            "file.close-workspace" => self.close_workspace(window, cx),
            "file.open-recent-workspace" => {
                if let Some(path) = self.settings.recent_workspaces.first().cloned() {
                    self.open_workspace(&path, window, cx);
                } else {
                    self.set_status(tr("workspace.no_recent"), cx);
                }
            }
            "file.remove-workspace-root" => self.remove_selected_workspace_root(cx),
            "navigation.goto-symbol" => {
                self.open_quick_open_with("@".to_string(), window, cx);
            }
            "file.clear-recent-workspaces" => {
                self.settings.recent_workspaces.clear();
                self.global_settings.recent_workspaces.clear();
                self.commit_settings();
                self.set_status(tr("workspace.recent_cleared"), cx);
            }
            "tree.new-file" | "tree.new-folder" | "tree.rename" | "tree.delete"
            | "tree.refresh" | "tree.copy-path" | "tree.reveal" => {
                self.explorer_command(id, window, cx)
            }
            "file.save" => self.save_active(window, cx),
            "file.save-as" => self.save_active_as(window, cx),
            "file.save-all" => self.save_all(window, cx),
            "file.reload" => self.reload_active(window, cx),
            "file.local-history" => self.open_local_history(window, cx),
            "file.export-html" => self.export_document(ExportFormat::Html, window, cx),
            "file.export-rtf" => self.export_document(ExportFormat::Rtf, window, cx),
            "file.close" => self.request_close_active(window, cx),
            "file.close-all" => self.request_close_all(window, cx),
            "file.exit" => self.request_exit(window, cx),
            "edit.undo" => self.undo_or_editor(false, window, cx),
            "edit.redo" => self.undo_or_editor(true, window, cx),
            "edit.cut" => self.editor_action(Box::new(Cut), window, cx),
            "edit.copy" => self.editor_action(Box::new(Copy), window, cx),
            "edit.paste" => self.editor_action(Box::new(Paste), window, cx),
            "edit.select-all" => self.editor_action(Box::new(SelectAll), window, cx),
            "edit.indent" => self.editor_action(Box::new(Indent), window, cx),
            "edit.outdent" => self.editor_action(Box::new(Outdent), window, cx),
            "edit.duplicate-line" => self.duplicate_line(window, cx),
            "edit.delete-line" => self.delete_line(window, cx),
            "edit.toggle-comment" => self.toggle_comment(window, cx),
            "edit.trim-whitespace" => self.trim_trailing(window, cx),
            "edit.uppercase" => self.transform_case(textutil::CaseMode::Upper, window, cx),
            "edit.lowercase" => self.transform_case(textutil::CaseMode::Lower, window, cx),
            "edit.title-case" => self.transform_case(textutil::CaseMode::Title, window, cx),
            "edit.sort-asc" => self.sort_selection(true, window, cx),
            "edit.sort-desc" => self.sort_selection(false, window, cx),
            "edit.sort-ignore-case-asc" => {
                self.sort_selection_mode(SortMode::Text, true, true, 1, window, cx)
            }
            "edit.sort-ignore-case-desc" => {
                self.sort_selection_mode(SortMode::Text, false, true, 1, window, cx)
            }
            "edit.sort-numeric-asc" => {
                self.sort_selection_mode(SortMode::Numeric, true, false, 1, window, cx)
            }
            "edit.sort-numeric-desc" => {
                self.sort_selection_mode(SortMode::Numeric, false, false, 1, window, cx)
            }
            "edit.sort-by-column" => self.open_sort_dialog(window, cx),
            "edit.move-line-up" => self.move_lines(true, window, cx),
            "edit.move-line-down" => self.move_lines(false, window, cx),
            "edit.join-lines" => self.join_lines(window, cx),
            "edit.split-lines" => self.split_lines(window, cx),
            "edit.remove-duplicate-lines" => self.remove_duplicate_lines(window, cx),
            "edit.remove-empty-lines" => self.remove_empty_lines(window, cx),
            "edit.filter-lines" => self.open_filter_dialog(window, cx),
            "edit.column-editor" => self.open_column_editor(window, cx),
            "edit.add-cursor-above" => {
                if self.multi_cursor_available(window, cx) {
                    self.editor_action(Box::new(AddCursorAbove), window, cx);
                }
            }
            "edit.add-cursor-below" => {
                if self.multi_cursor_available(window, cx) {
                    self.editor_action(Box::new(AddCursorBelow), window, cx);
                }
            }
            "edit.select-next-occurrence" => self.select_next_occurrence(window, cx),
            "edit.select-all-occurrences" => self.select_all_occurrences(window, cx),
            "edit.unfold-current" => self.unfold_current(window, cx),
            "edit.unfold-all" => self.unfold_all(window, cx),
            "search.find" => self.open_find(false, window, cx),
            "search.replace" => self.open_find(true, window, cx),
            "search.find-next" => self.find_next(true, window, cx),
            "search.find-previous" => self.find_next(false, window, cx),
            "search.go-to-line" => self.open_go_to_line(window, cx),
            "search.find-in-files" => self.open_find_in_files(window, cx),
            "search.workspace-replace" => self.open_workspace_replace(window, cx),
            "search.undo-workspace-replace" => self.undo_workspace_replace(window, cx),
            "search.clear-highlights" => {
                self.find.clear();
                cx.notify();
            }
            "search.mark-all" => self.mark_find_all(window, cx),
            "search.clear-history" => self.clear_search_history(cx),
            "bookmarks.toggle" => self.toggle_bookmark(window, cx),
            "bookmarks.next" => self.goto_bookmark(true, window, cx),
            "bookmarks.previous" => self.goto_bookmark(false, window, cx),
            "bookmarks.clear" => self.clear_bookmarks(cx),
            "bookmarks.cut-lines" => self.bookmark_lines(BookmarkAction::Cut, window, cx),
            "bookmarks.copy-lines" => self.bookmark_lines(BookmarkAction::Copy, window, cx),
            "bookmarks.delete-lines" => self.bookmark_lines(BookmarkAction::Delete, window, cx),
            "macro.toggle-recording" => self.toggle_macro_recording(window, cx),
            "macro.play-last" => self.play_last_macro(window, cx),
            "macro.play-saved" => self.open_macro_dialog(window, cx),
            "macro.save-recording" => self.save_macro_recording(window, cx),
            "view.word-wrap" => {
                self.settings.word_wrap = !self.settings.word_wrap;
                self.apply_settings(window, cx);
            }
            "view.line-numbers" => {
                self.settings.show_line_numbers = !self.settings.show_line_numbers;
                self.apply_settings(window, cx);
            }
            "view.toolbar" => {
                self.settings.show_toolbar = !self.settings.show_toolbar;
                self.apply_settings(window, cx);
            }
            "view.status-bar" => {
                self.settings.show_status_bar = !self.settings.show_status_bar;
                self.apply_settings(window, cx);
            }
            "view.explorer" => {
                self.settings.show_file_explorer = !self.settings.show_file_explorer;
                self.apply_settings(window, cx);
            }
            "view.large-file-controller" => self.toggle_large_file_controller(window, cx),
            "view.zoom-in" => self.zoom(1.0, cx),
            "view.zoom-out" => self.zoom(-1.0, cx),
            "view.zoom-reset" => self.reset_zoom(cx),
            "view.theme-light" => {
                self.settings.theme = ThemeChoice::Light;
                self.apply_settings(window, cx);
            }
            "view.theme-dark" => {
                self.settings.theme = ThemeChoice::Dark;
                self.apply_settings(window, cx);
            }
            "view.theme-system" => {
                self.settings.theme = ThemeChoice::System;
                self.apply_settings(window, cx);
            }
            "view.toggle-theme" => {
                self.settings.theme = if self.settings.theme == ThemeChoice::Light {
                    ThemeChoice::Dark
                } else {
                    ThemeChoice::Light
                };
                self.apply_settings(window, cx);
            }
            "view.fullscreen" => {
                log::info!("toggling fullscreen");
                window.toggle_fullscreen();
                cx.notify();
            }
            "view.choose-language" => self.open_language_picker(window, cx),
            "view.whitespace" => {
                self.settings.show_whitespace = !self.settings.show_whitespace;
                self.apply_settings(window, cx);
            }
            "view.indent-guides" => {
                self.settings.show_indent_guides = !self.settings.show_indent_guides;
                self.apply_settings(window, cx);
            }
            "view.ruler" => {
                self.settings.show_ruler = !self.settings.show_ruler;
                self.apply_settings(window, cx);
            }
            "view.document-map" => {
                self.settings.show_document_map = !self.settings.show_document_map;
                self.apply_settings(window, cx);
            }
            "view.bracket-colors" => {
                self.settings.bracket_colors = !self.settings.bracket_colors;
                if self.settings.bracket_colors
                    && self.active_tab().is_some_and(|tab| {
                        tab.doc.is_huge()
                            || tab.large_file
                            || tab.doc.partial_range().is_some()
                            || tab.doc.is_non_text()
                    })
                {
                    self.set_status(tr("status.bracket_colors_unavailable"), cx);
                }
                self.apply_settings(window, cx);
            }
            "navigation.toggle-outline" => {
                self.settings.show_outline = !self.settings.show_outline;
                self.apply_settings(window, cx);
            }
            "navigation.toggle-breadcrumbs" => {
                self.settings.show_breadcrumbs = !self.settings.show_breadcrumbs;
                self.apply_settings(window, cx);
            }
            "navigation.back" => self.navigate_back(window, cx),
            "navigation.forward" => self.navigate_forward(window, cx),
            "window.move-tab-to-new-window" => self.move_tab_to_new_window(window, cx),
            "window.split-right" => {
                self.split_editor(SplitDirection::Right, window, cx);
            }
            "window.split-down" => {
                self.split_editor(SplitDirection::Down, window, cx);
            }
            "window.unsplit" => self.unsplit_editor(cx),
            "window.switch-group" | "window.switch-group-back" => {
                self.switch_group(None, window, cx);
            }
            "window.move-tab-to-other-group" => {
                self.move_active_tab_to_other_group(window, cx);
            }
            "tabs.close-others" => self.request_trim_tabs(false, window, cx),
            "tabs.close-right" => self.request_trim_tabs(true, window, cx),
            "tabs.copy-path" => self.copy_active_path(cx),
            "tabs.reveal" => self.reveal_active_in_file_manager(cx),
            "tabs.reopen-closed" => self.reopen_closed_tab(window, cx),
            "tabs.next" => self.next_tab(1, window, cx),
            "tabs.previous" => self.next_tab(-1, window, cx),
            "palette.commands" => self.open_command_palette(window, cx),
            "palette.quick-open" => self.open_quick_open(window, cx),
            "settings.preferences" => self.open_preferences(window, cx),
            "settings.import" => self.open_import_settings_dialog(window, cx),
            "settings.export" => self.open_export_settings_dialog(window, cx),
            "settings.reset" => {
                self.confirm = Some(ConfirmState::new(
                    CloseIntent::ResetSettings,
                    tr("settings.reset_title"),
                    tr("settings.reset_detail"),
                ));
                cx.notify();
            }
            "help.shortcuts" => {
                self.shortcuts_open = true;
                cx.notify();
            }
            "help.about" => {
                self.about_open = true;
                cx.notify();
            }
            "help.open-log-dir" => {
                let directory = logging::log_dir();
                match util::open_directory_command(&directory).spawn() {
                    Ok(()) => {}
                    Err(error) => self.set_status(
                        tr_args(
                            "status.open_path_failed",
                            &[("error", format!("{}: {error}", directory.display()))],
                        ),
                        cx,
                    ),
                }
            }
            other => {
                log::warn!("command {other} has no implementation");
                return false;
            }
        }
        if let Some(before) = recording_before {
            if before != self.macro_state_signature() {
                self.macro_recorded.push(id.to_string());
                cx.notify();
            }
        }
        true
    }

    /// Take a local-history snapshot of the file a save is about to replace.
    fn snapshot_before_save(&mut self, target: &Path, cx: &mut Context<Self>) {
        if !self.settings.local_history_enabled {
            return;
        }
        match self.history.snapshot(target) {
            Ok(Some(_)) => {
                let keep = Duration::from_secs(self.settings.local_history_days.max(1) * 24 * 3600);
                let limit = self.settings.local_history_max_mb.max(1) * 1024 * 1024;
                if let Err(error) = self.history.prune(keep, limit) {
                    log::warn!("could not prune local history: {error}");
                }
            }
            Ok(None) => {}
            Err(error) => {
                log::warn!("could not snapshot {}: {error}", target.display());
                self.set_status(
                    tr_args("status.history_failed", &[("error", error.to_string())]),
                    cx,
                );
            }
        }
    }

    fn open_local_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let Some(path) = tab.doc.path.clone() else {
            return;
        };
        if tab.doc.is_huge() || tab.doc.is_partial() || tab.doc.is_non_text() {
            self.set_status(tr("status.history_unavailable"), cx);
            return;
        }
        match self.history.list(&path) {
            Ok(entries) => {
                self.history_dialog = Some(HistoryDialogState {
                    path,
                    entries,
                    error: None,
                });
            }
            Err(error) => {
                self.history_dialog = Some(HistoryDialogState {
                    path,
                    entries: Vec::new(),
                    error: Some(error.to_string()),
                });
            }
        }
        let _ = window;
        cx.notify();
    }

    fn restore_history_entry(
        &mut self,
        entry: HistoryEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tabs.get(self.active).map(|_| self.active) else {
            return;
        };
        let bytes = match self.history.read(&entry) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.set_status(
                    tr_args("status.history_failed", &[("error", error.to_string())]),
                    cx,
                );
                return;
            }
        };
        let tab = &self.tabs[index];
        let decoded = encoding::decode_with(&bytes, tab.doc.encoding);
        let text = if tab.doc.is_non_text() {
            decoded
        } else {
            eol::to_lf(&decoded)
        };
        if tab.doc.is_modified() {
            self.pending_history_restore = Some(entry);
            self.confirm = Some(ConfirmState::new(
                CloseIntent::RestoreHistory,
                tr("confirm.history_restore_title"),
                tr("confirm.history_restore_detail"),
            ));
            cx.notify();
            return;
        }
        self.apply_history_restore(index, text, window, cx);
    }

    fn apply_pending_history_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.pending_history_restore.take() else {
            return;
        };
        let Some(index) = self.tabs.get(self.active).map(|_| self.active) else {
            return;
        };
        let bytes = match self.history.read(&entry) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.set_status(
                    tr_args("status.history_failed", &[("error", error.to_string())]),
                    cx,
                );
                return;
            }
        };
        let tab = &self.tabs[index];
        let decoded = encoding::decode_with(&bytes, tab.doc.encoding);
        let text = if tab.doc.is_non_text() {
            decoded
        } else {
            eol::to_lf(&decoded)
        };
        self.apply_history_restore(index, text, window, cx);
    }

    fn apply_history_restore(
        &mut self,
        index: usize,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        tab.doc.replace_all_text(&text, EditKind::Other);
        tab.doc.touch();
        tab.doc.cursor = 0;
        tab.doc.anchor = 0;
        tab.highlighted_revision = u64::MAX;
        tab.needs_sync = false;
        tab.pending_edit = false;
        let editor = tab.editor.clone();
        editor.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.set_selected_range(0..0, cx);
            state.focus(window, cx);
        });
        self.history_dialog = None;
        self.focus_editor = true;
        self.set_status(tr("status.history_restored"), cx);
        cx.notify();
    }

    fn clear_local_history(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.history_dialog.as_mut() else {
            return;
        };
        match self.history.clear(&dialog.path) {
            Ok(()) => {
                dialog.entries.clear();
                dialog.error = None;
            }
            Err(error) => dialog.error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            return;
        }
        let groups = palette::command_groups();
        let command_groups: Vec<Vec<&'static str>> = groups
            .iter()
            .map(|(_, commands)| commands.iter().map(|command| command.id).collect())
            .collect();
        let state = cx.new(|cx| CommandState::new(window, cx));
        state.update(cx, |state, cx| state.focus(window, cx));
        self.palette = Some(PaletteOverlay {
            kind: PaletteKind::Commands,
            state,
            command_groups,
        });
        cx.notify();
    }

    fn open_quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_quick_open_with(String::new(), window, cx);
    }

    fn open_quick_open_with(&mut self, query: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            return;
        }
        let state = cx.new(|cx| CommandState::new(window, cx));
        state.update(cx, |state, cx| state.focus(window, cx));
        self.quick_query = query;
        self.rebuild_quick_rows(cx);
        self.palette = Some(PaletteOverlay {
            kind: PaletteKind::QuickOpen,
            state,
            command_groups: Vec::new(),
        });
        cx.notify();
    }

    /// Line count of a tab for Goto Anything and the document map. Reduced
    /// large-file tabs keep their text in the editor, not the document.
    fn tab_line_count(&self, index: usize, cx: &gpui_kit::App) -> usize {
        let Some(tab) = self.tabs.get(index) else {
            return 0;
        };
        if tab.doc.is_huge() {
            tab.doc.line_count().max(1)
        } else if tab.large_file {
            tab.editor.read(cx).text().lines_len().max(1)
        } else {
            tab.doc.line_count().max(1)
        }
    }

    /// Recompute quick-open rows for the current query.
    fn rebuild_quick_rows(&mut self, cx: &mut Context<Self>) {
        let query = self.quick_query.clone();
        let parsed = luban_core::fuzzy::parse_quick_query(&query);
        self.quick.error = None;
        let active_tab = self
            .tabs
            .get(self.active)
            .map(|tab| (tab.id, self.tab_line_count(self.active, cx)));
        match parsed {
            luban_core::fuzzy::QuickQuery::Line(line) => {
                match quick_open::line_row(
                    line,
                    active_tab.map(|(id, _)| id),
                    active_tab.map(|(_, lines)| lines).unwrap_or(0),
                ) {
                    Ok(row) => self.quick.rows = vec![row],
                    Err(error) => {
                        self.quick.rows.clear();
                        self.quick.error = Some(error);
                    }
                }
            }
            luban_core::fuzzy::QuickQuery::Symbol(symbol_query) => {
                let Some((tab_id, _)) = active_tab else {
                    self.quick.rows.clear();
                    self.quick.error = Some(tr("command.goto.no_document"));
                    cx.notify();
                    return;
                };
                if self.outline.symbols.is_empty() {
                    self.quick.rows.clear();
                    self.quick.error = Some(if self.outline.error.is_some() {
                        tr("outline.unavailable")
                    } else {
                        tr("command.goto.no_symbols")
                    });
                } else {
                    self.quick.rows =
                        quick_open::symbol_rows(&symbol_query, &self.outline.symbols, tab_id);
                    if self.quick.rows.is_empty() {
                        self.quick.error = Some(tr("command.quick_open.no_matches"));
                    }
                }
            }
            luban_core::fuzzy::QuickQuery::Files { text, line } => {
                let mut open = Vec::new();
                let mut workspace_files = Vec::new();
                if let Some(index) = &self.workspace_index {
                    for tab in &self.tabs {
                        if let Some(path) = &tab.doc.path {
                            let relative = index
                                .relative_of(path)
                                .unwrap_or_else(|| path.display().to_string());
                            open.push(quick_open::OpenFile {
                                path: path.clone(),
                                relative,
                                name: tab.doc.display_name(),
                                modified: tab.doc.is_modified(),
                            });
                        }
                    }
                    for file in index.files() {
                        workspace_files.push((file.relative.clone(), file.path.clone()));
                    }
                } else {
                    for tab in &self.tabs {
                        if let Some(path) = &tab.doc.path {
                            open.push(quick_open::OpenFile {
                                path: path.clone(),
                                relative: path.display().to_string(),
                                name: tab.doc.display_name(),
                                modified: tab.doc.is_modified(),
                            });
                        }
                    }
                    for path in &self.settings.recent_files {
                        if path.is_file() {
                            workspace_files.push((path.display().to_string(), path.clone()));
                        }
                    }
                }
                self.quick.rows = quick_open::file_rows(
                    &text,
                    line,
                    &open,
                    &workspace_files,
                    quick_open::MAX_QUICK_ROWS,
                );
                if self.quick.rows.is_empty() {
                    self.quick.error = Some(tr("command.quick_open.no_matches"));
                }
            }
        }
        cx.notify();
    }

    /// Palette query callback: filters the quick-open list.
    fn set_quick_query(&mut self, query: &str, cx: &mut Context<Self>) {
        if self.quick_query == query {
            return;
        }
        self.quick_query = query.to_string();
        self.rebuild_quick_rows(cx);
    }

    fn confirm_palette(
        &mut self,
        kind: PaletteKind,
        index: gpui_kit::component::IndexPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(overlay) = self.palette.take() else {
            return;
        };
        match kind {
            PaletteKind::Commands => {
                let id = overlay
                    .command_groups
                    .get(index.section)
                    .and_then(|group| group.get(index.row))
                    .copied();
                if let Some(id) = id {
                    self.run_command(id, window, cx);
                }
            }
            PaletteKind::QuickOpen => {
                let target = self.quick.rows.get(index.row).map(|row| row.target.clone());
                self.quick.rows.clear();
                match target {
                    Some(quick_open::QuickTarget::File { path, line }) => match line {
                        Some(line) => {
                            self.open_path_at(&path, line as usize, 1, 0, window, cx);
                        }
                        None => {
                            self.open_path(&path, window, cx);
                        }
                    },
                    Some(quick_open::QuickTarget::Line { tab_id, line })
                    | Some(quick_open::QuickTarget::Symbol { tab_id, line }) => {
                        if let Some(index) = self.tab_index_by_id(tab_id) {
                            self.activate_tab_index(index, window, cx);
                            self.jump_to_line_in_tab(index, line, window, cx);
                        }
                    }
                    None => {}
                }
            }
        }
        self.focus_editor(window, cx);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        self.quick.rows.clear();
        self.quick_query.clear();
        self.focus_editor(window, cx);
        cx.notify();
    }

    /// Snapshot the current location for the navigation history.
    fn current_navigation_entry(&self, cx: &gpui_kit::App) -> Option<NavigationEntry> {
        let tab = self.tabs.get(self.active)?;
        Some(NavigationEntry {
            tab_id: tab.id,
            cursor: tab.doc.cursor,
            anchor: tab.doc.anchor,
            scroll: tab.editor.read(cx).scroll_offset().y.as_f32(),
        })
    }

    fn record_navigation(&mut self, cx: &gpui_kit::App) {
        if let Some(entry) = self.current_navigation_entry(cx) {
            self.nav.record(entry);
        }
    }

    pub fn navigate_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let alive: HashSet<u64> = self.tabs.iter().map(|tab| tab.id).collect();
        let target = self.nav.back_while(|entry| alive.contains(&entry.tab_id));
        if let Some(entry) = target {
            self.apply_navigation_entry(entry, window, cx);
        }
    }

    pub fn navigate_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let alive: HashSet<u64> = self.tabs.iter().map(|tab| tab.id).collect();
        let target = self
            .nav
            .forward_while(|entry| alive.contains(&entry.tab_id));
        if let Some(entry) = target {
            self.apply_navigation_entry(entry, window, cx);
        }
    }

    fn apply_navigation_entry(
        &mut self,
        entry: NavigationEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_by_id(entry.tab_id) else {
            return;
        };
        self.sync_active(window, cx);
        self.active = index;
        let group = self.tabs[index].group;
        self.active_group = group;
        self.active_in_group[group] = Some(entry.tab_id);
        let huge = self.tabs[index].doc.is_huge();
        if huge {
            self.huge_jump_line(entry.cursor as u64 + 1, entry.anchor as u64 + 1, window, cx);
        } else {
            let total = self.tabs[index].editor.read(cx).text().len_chars();
            let anchor = entry.anchor.min(total);
            let cursor = entry.cursor.min(total);
            let editor = self.tabs[index].editor.clone();
            let rope = editor.read(cx).text().clone();
            let b0 = rope.char_to_byte_idx(anchor);
            let b1 = rope.char_to_byte_idx(cursor);
            let (start, end) = if b0 <= b1 { (b0, b1) } else { (b1, b0) };
            editor.update(cx, |state, cx| {
                state.set_selected_range(start..end, cx);
                state.set_scroll_offset(point(px(0.), px(entry.scroll)), cx);
                state.focus(window, cx);
            });
            self.tabs[index].doc.set_selection(anchor, cursor);
        }
        self.focus_editor = true;
        self.find.dirty = true;
        cx.notify();
    }

    /// Move the caret to a 1-based line of a tab and focus the editor.
    fn jump_to_line_in_tab(
        &mut self,
        index: usize,
        line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (huge, group) = match self.tabs.get(index) {
            Some(tab) => (tab.doc.is_huge(), tab.group),
            None => return,
        };
        self.record_navigation(cx);
        if huge {
            self.active = index;
            self.active_group = group;
            self.active_in_group[group] = Some(self.tabs[index].id);
            self.huge_jump_line(line as u64, 1, window, cx);
            return;
        }
        self.active = index;
        self.active_group = group;
        self.active_in_group[group] = Some(self.tabs[index].id);
        let (start, end) = self.line_selection_in_tab(index, line, 1, 0, cx);
        self.select_char_range(start, end, window, cx);
        self.focus_editor = true;
        self.record_navigation(cx);
        cx.notify();
    }

    fn open_shortcut_editor(&mut self, cx: &mut Context<Self>) {
        self.shortcut_editor = Some(ShortcutEditorState::default());
        self.shortcuts_open = false;
        cx.notify();
    }

    /// Capture the next keystroke for a command waiting in the shortcut
    /// editor. Returns true when the event was consumed.
    fn capture_shortcut(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(mut editor) = self.shortcut_editor.take() else {
            return false;
        };
        let Some(id) = editor.waiting_for.clone() else {
            self.shortcut_editor = Some(editor);
            return false;
        };
        let key = keystroke.key.to_ascii_lowercase();
        if key == "escape" {
            editor.waiting_for = None;
            self.shortcut_editor = Some(editor);
            cx.notify();
            return true;
        }
        let modifiers = keystroke.modifiers;
        let mut parts = Vec::new();
        if modifiers.secondary() || modifiers.control {
            parts.push("ctrl".to_string());
        }
        if modifiers.shift {
            parts.push("shift".to_string());
        }
        if modifiers.alt {
            parts.push("alt".to_string());
        }
        let key = match key.as_str() {
            " " => "space".to_string(),
            "+" => "plus".to_string(),
            "-" => "minus".to_string(),
            "/" => "slash".to_string(),
            other => other.to_string(),
        };
        parts.push(key);
        let text = parts.join("+");
        match commands::validate_override(&id, &text, &self.settings.keybindings) {
            Ok(binding) => {
                self.settings.keybindings.insert(id, binding);
                editor.waiting_for = None;
                editor.error = None;
                self.apply_settings(window, cx);
            }
            Err(error) => {
                editor.waiting_for = None;
                editor.error = Some(error.to_string());
                cx.notify();
            }
        }
        self.shortcut_editor = Some(editor);
        true
    }

    fn reset_shortcut(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.keybindings.remove(id);
        if let Some(editor) = self.shortcut_editor.as_mut() {
            editor.error = None;
        }
        self.apply_settings(window, cx);
    }

    fn reset_all_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.keybindings.clear();
        if let Some(editor) = self.shortcut_editor.as_mut() {
            editor.error = None;
        }
        self.apply_settings(window, cx);
    }

    fn record_closed_tab(&mut self, index: usize, cx: &Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let huge = tab.doc.is_huge();
        let (huge_line, huge_column) = if huge {
            tab.doc
                .huge()
                .and_then(|state| {
                    let cursor = state.cursor;
                    state
                        .engine
                        .try_read()
                        .ok()
                        .and_then(|engine| engine.line_col_at(cursor).ok())
                })
                .unwrap_or((1, 1))
        } else {
            (1, 1)
        };
        let modified = tab.doc.is_modified();
        // Keep the exact buffer for every normal tab so reopening restores
        // what was closed even when the file changes afterwards.
        let range = tab
            .doc
            .partial_range()
            .map(|range| [range.start, range.end]);
        let non_text = tab.doc.is_non_text();
        let text = if huge || range.is_some() {
            None
        } else {
            Some(tab.editor.read(cx).text().to_string())
        };
        let name = tab.doc.display_name();
        let closed = ClosedTab {
            path: tab.doc.path.clone(),
            name,
            text,
            encoding: tab.doc.encoding,
            eol: tab.doc.eol,
            language: tab.doc.language.clone(),
            cursor: tab.doc.cursor,
            anchor: tab.doc.anchor,
            modified,
            huge,
            huge_line: huge.then_some(huge_line),
            huge_column: huge.then_some(huge_column),
            range,
            non_text,
            file_size: tab.doc.file_size,
            last_modified: tab.doc.last_modified,
        };
        self.closed_tabs.push(closed);
        if self.closed_tabs.len() > 20 {
            self.closed_tabs.remove(0);
        }
    }

    fn reopen_closed_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(closed) = self.closed_tabs.last().cloned() else {
            self.set_status(tr("status.no_closed_tabs"), cx);
            return;
        };
        if let Some(path) = &closed.path {
            if let Some(index) = self.tabs.iter().position(|tab| {
                tab.doc
                    .path
                    .as_ref()
                    .is_some_and(|open| util::same_path(open, path))
            }) {
                self.closed_tabs.pop();
                self.activate_tab_index(index, window, cx);
                return;
            }
            if let Some([from, to]) = closed.range {
                let cancel = AtomicBool::new(false);
                let progress = AtomicU64::new(0);
                match Document::open_range(
                    path,
                    from,
                    to,
                    self.settings.max_file_size_bytes(),
                    &cancel,
                    &progress,
                ) {
                    Ok(mut doc) => {
                        doc.cursor = closed.cursor.min(doc.char_count());
                        doc.anchor = closed.anchor.min(doc.char_count());
                        self.closed_tabs.pop();
                        self.add_document(doc, window, cx);
                        self.set_status(tr("status.tab_restored"), cx);
                    }
                    Err(error) => {
                        self.set_status(
                            tr_args("status.reload_failed", &[("error", error.to_string())]),
                            cx,
                        );
                    }
                }
                return;
            }
            if closed.text.is_none() {
                if self.open_path(path, window, cx) {
                    if closed.huge {
                        self.pending_huge_at = Some((
                            path.clone(),
                            closed.huge_line.unwrap_or(1),
                            closed.huge_column.unwrap_or(1),
                        ));
                    }
                    // The entry is consumed once the open request is accepted;
                    // a failed open leaves it on the stack.
                    self.closed_tabs.pop();
                }
                return;
            }
        }
        let mut doc = Document::new_untitled(closed.name);
        doc.attach_text(closed.text.unwrap_or_default());
        doc.encoding = closed.encoding;
        doc.eol = closed.eol;
        doc.language = closed.language;
        doc.path = closed.path;
        doc.file_size = closed.file_size;
        doc.last_modified = closed.last_modified;
        doc.cursor = closed.cursor;
        doc.anchor = closed.anchor;
        if closed.non_text {
            doc.mark_non_text();
        }
        if closed.modified {
            doc.touch();
        } else {
            doc.mark_saved();
        }
        self.closed_tabs.pop();
        self.add_document(doc, window, cx);
        self.set_status(tr("status.tab_restored"), cx);
    }

    fn copy_active_path(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.active_doc().and_then(|doc| doc.path.clone()) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
        self.set_status(tr("status.path_copied"), cx);
    }

    fn reveal_active_in_file_manager(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.active_doc().and_then(|doc| doc.path.clone()) else {
            return;
        };
        if let Err(error) = util::reveal_file_command(&path).spawn() {
            self.set_status(
                tr_args("status.open_path_failed", &[("error", error.to_string())]),
                cx,
            );
        }
    }

    /// Close every tab except the active one, or every tab to its right.
    ///
    /// Modified targets are saved before any tab is closed, so cancelling the
    /// save phase leaves the whole tab set intact.
    fn request_trim_tabs(&mut self, right: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.get(self.active).map(|_| self.active) else {
            return;
        };
        let keep_id = self.tabs[index].id;
        let memberships: Vec<usize> = self.tabs.iter().map(|tab| tab.group).collect();
        let members = tab_groups::group_tabs(&memberships, self.tabs[index].group);
        let keep_position = members
            .iter()
            .position(|member| *member == index)
            .unwrap_or(0);
        let queue: Vec<u64> = trim_queue(members.len(), keep_position, right)
            .into_iter()
            .filter_map(|position| members.get(position))
            .filter_map(|index| self.tabs.get(*index).map(|tab| tab.id))
            .collect();
        if queue.is_empty() {
            return;
        }
        let save_queue: Vec<u64> = queue
            .iter()
            .filter(|id| {
                self.tab_index_by_id(**id)
                    .is_some_and(|index| self.tabs[index].doc.is_modified())
            })
            .copied()
            .collect();
        self.trim_plan = Some(TrimPlan {
            keep_id,
            queue,
            save_queue,
            remove_root: None,
        });
        if self
            .trim_plan
            .as_ref()
            .is_some_and(|plan| plan.save_queue.is_empty())
        {
            self.close_trim_now(false, window, cx);
            return;
        }
        self.confirm = Some(ConfirmState::new(
            CloseIntent::TrimTabs,
            tr("confirm.trim_title"),
            tr("confirm.trim_detail"),
        ));
        cx.notify();
    }

    /// Save the modified targets of a trim plan, one at a time.
    fn save_next_trim(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        loop {
            let id = {
                let Some(plan) = self.trim_plan.as_mut() else {
                    return;
                };
                match plan.save_queue.first().copied() {
                    Some(id) => {
                        plan.save_queue.remove(0);
                        id
                    }
                    None => break,
                }
            };
            let Some(index) = self.tab_index_by_id(id) else {
                continue;
            };
            if !self.tabs[index].doc.is_modified() {
                continue;
            }
            if self.tabs[index].doc.is_huge() && !self.tabs[index].doc.is_partial() {
                self.start_huge_save(index, None, SaveThen::NextTrimSave, false, window, cx);
                return;
            }
            if self.tabs[index].doc.path.is_some() && !self.tabs[index].doc.is_partial() {
                self.save_tab_at_with(index, SaveThen::NextTrimSave, false, window, cx);
                return;
            }
            let (dir, name) = {
                let tab = &self.tabs[index];
                let name = if tab.doc.is_partial() {
                    tab.doc
                        .path
                        .as_deref()
                        .map(luban_core::large_file::partial_save_name)
                        .unwrap_or_else(|| tab.doc.display_name())
                } else {
                    tab.doc.display_name()
                };
                (util::default_save_directory(tab.doc.path.as_deref()), name)
            };
            self.show_file_dialog(
                FileDialogMode::Save,
                FileDialogPurpose::SaveAs {
                    tab_id: id,
                    then: SaveThen::NextTrimSave,
                },
                dir,
                name,
                window,
                cx,
            );
            return;
        }
        self.close_trim_now(false, window, cx);
    }

    /// Close every tab in the plan. `discard` allows modified buffers to be
    /// dropped; otherwise a late edit aborts instead of losing data.
    fn close_trim_now(&mut self, discard: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(plan) = self.trim_plan.take() else {
            return;
        };
        if !discard {
            let changed = plan.queue.iter().any(|id| {
                self.tab_index_by_id(*id)
                    .is_some_and(|index| self.tabs[index].doc.is_modified())
            });
            if changed {
                self.abort_pending_saves();
                self.set_status(tr("status.batch_unsaved"), cx);
                return;
            }
        }
        for id in plan.queue {
            if let Some(index) = self.tab_index_by_id(id) {
                self.close_tab_at_inner(index, window, cx);
            }
        }
        self.finish_trim(plan.keep_id, cx);
        if let Some(path) = plan.remove_root {
            self.remove_workspace_root_now(&path, cx);
            self.save_session_now(cx);
        }
    }

    fn finish_trim(&mut self, keep_id: u64, cx: &mut Context<Self>) {
        if let Some(index) = self.tab_index_by_id(keep_id) {
            self.active = index;
            self.focus_editor = true;
        }
        self.find.clear();
        self.find.dirty = true;
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Keyboard
    // ------------------------------------------------------------------

    fn shortcut(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let modifiers = event.keystroke.modifiers;
        let key = event.keystroke.key.to_ascii_lowercase();
        let secondary = modifiers.secondary();
        let shift = modifiers.shift;
        let alt = modifiers.alt;
        let command = secondary || modifiers.control;
        // A waiting shortcut editor claims the next keystroke verbatim.
        if self
            .shortcut_editor
            .as_ref()
            .is_some_and(|editor| editor.waiting_for.is_some())
        {
            return self.capture_shortcut(&event.keystroke, window, cx);
        }
        if key == "escape" {
            if self.replace_preview.is_some() {
                self.replace_preview = None;
                cx.notify();
                return true;
            }
            if self.column_editor.open {
                self.column_editor.open = false;
                self.column_editor.error = None;
                cx.notify();
                return true;
            }
            if self.sort_dialog.is_some() {
                self.sort_dialog = None;
                cx.notify();
                return true;
            }
            if self.filter_dialog.is_some() {
                self.filter_dialog = None;
                cx.notify();
                return true;
            }
            if self.macro_ui.open {
                self.macro_ui.open = false;
                self.macro_ui.error = None;
                cx.notify();
                return true;
            }
            if self.palette.is_some() {
                self.close_palette(window, cx);
                return true;
            }
            if self.history_dialog.is_some() {
                self.history_dialog = None;
                self.pending_history_restore = None;
                self.focus_editor = true;
                cx.notify();
                return true;
            }
            if self.shortcut_editor.is_some() {
                self.shortcut_editor = None;
                cx.notify();
                return true;
            }
            if self.overwrite_save.is_some() {
                self.cancel_overwrite(cx);
                return true;
            }
            if self.file_dialog.is_some() {
                self.cancel_file_dialog(cx);
                return true;
            }
            if self.name_prompt.is_some() {
                self.name_prompt = None;
                self.focus_editor(window, cx);
                cx.notify();
                return true;
            }
            if self.confirm.is_some() {
                self.confirm_cancel(cx);
                return true;
            }
            if self.about_open || self.shortcuts_open {
                self.about_open = false;
                self.shortcuts_open = false;
                cx.notify();
                return true;
            }
            if self.language_picker.is_some() {
                self.language_picker = None;
                cx.notify();
                return true;
            }
            if self.go_to_line.is_some() {
                self.go_to_line = None;
                cx.notify();
                return true;
            }
            if self.preferences.font_picker.is_some() {
                self.preferences.font_picker = None;
                self.focus_editor(window, cx);
                cx.notify();
                return true;
            }
            if self.preferences.open {
                self.close_preferences(window, cx);
                return true;
            }
            if self.find_in_files.open {
                self.cancel_find_in_files(cx);
                self.find_in_files.open = false;
                cx.notify();
                return true;
            }
            if self.find.open {
                self.find.open = false;
                self.cancel_huge_search();
                self.find.clear();
                cx.notify();
                return true;
            }
            if self.cancel_huge_opens(cx) {
                return true;
            }
            return false;
        }

        if key == "enter" {
            if self.replace_preview.is_some() {
                self.confirm_replace_preview(window, cx);
                return true;
            }
            if self.sort_dialog.is_some() {
                self.apply_sort_dialog(window, cx);
                return true;
            }
            if self.filter_dialog.is_some() {
                self.apply_filter_dialog(window, cx);
                return true;
            }
            if self.column_editor.open {
                self.apply_column_editor(window, cx);
                return true;
            }
            if self.macro_ui.open {
                self.confirm_save_macro(window, cx);
                return true;
            }
        }

        if self.column_editor.open
            || self.sort_dialog.is_some()
            || self.filter_dialog.is_some()
            || self.macro_ui.open
        {
            // Text fields inside these dialogs own ordinary typing. Only
            // command/alt chords are left for the application.
            return command || alt;
        }

        // The focused file tree owns arrows, Enter, F2, Delete and Ctrl+N.
        if self.explorer_key(&key, modifiers, window, cx) {
            return true;
        }
        if self.outline_key(&key, modifiers, window, cx) {
            return true;
        }

        // Registered commands take precedence over the legacy inline
        // handlers below. Editing commands only apply while the editor owns
        // the focus, so a find field or dialog keeps Ctrl+C/V/A. A rebound
        // command consumes its old default instead of running twice. Modal
        // quick-open surfaces own their keystrokes entirely.
        if self.palette.is_none() && self.history_dialog.is_none() && self.shortcut_editor.is_none()
        {
            let editor_focused = self
                .tabs
                .get(self.active)
                .is_some_and(|tab| tab.editor.read(cx).focus_handle(cx).is_focused(window));
            // Layout-dependent keys arrive as their unshifted character.
            let lookup_key = match key.as_str() {
                "=" => "plus",
                "-" => "minus",
                "/" => "slash",
                "\\" => "backslash",
                "[" => "bracketleft",
                "]" => "bracketright",
                "," => "comma",
                "." => "period",
                ";" => "semicolon",
                "'" => "quote",
                "`" => "backquote",
                other => other,
            };
            if let Some(id) = self.keymap.lookup(lookup_key, command, shift, alt) {
                if !id.starts_with("edit.") || editor_focused {
                    return self.run_command(id, window, cx);
                }
            }
            if let Some(id) = self
                .keymap
                .suppressed_binding(lookup_key, command, shift, alt)
            {
                if !id.starts_with("edit.") || editor_focused {
                    return true;
                }
            }
            let macro_name = self
                .macro_keymap
                .iter()
                .find(|(binding, _)| binding.matches(lookup_key, command, shift, alt))
                .map(|(_, name)| name.clone());
            if let Some(name) = macro_name {
                if let Some(saved) = self.settings.macros.find(&name).cloned() {
                    self.play_macro(&saved, window, cx);
                    return true;
                }
            }
        }

        // A quick-open surface with a query field keeps its text-editing and
        // navigation keys; every other command/chord stays with the overlay.
        if self.palette.is_some() {
            let text_edit = command && matches!(key.as_str(), "c" | "v" | "x" | "a" | "z" | "y");
            if text_edit {
                return false;
            }
            let function_key = key.len() > 1
                && key.starts_with('f')
                && key[1..].chars().all(|ch| ch.is_ascii_digit());
            return command || alt || function_key;
        }
        if self.history_dialog.is_some() || self.shortcut_editor.is_some() {
            let function_key = key.len() > 1
                && key.starts_with('f')
                && key[1..].chars().all(|ch| ch.is_ascii_digit());
            return command || alt || function_key;
        }

        if self.file_dialog.is_some() {
            if self.file_dialog_list_focused(window) {
                if self.file_dialog_list_key(&key, modifiers, window, cx) {
                    return true;
                }
            } else if key == "down"
                && self
                    .file_path_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            {
                self.focus_file_dialog_list(0, window, cx);
                return true;
            }
        }

        if key == "f3" {
            self.find_next(!shift, window, cx);
            return true;
        }

        if !command {
            return false;
        }

        if self.active_doc().is_some_and(Document::is_huge) {
            match (key.as_str(), shift) {
                ("z", false) => {
                    self.huge_history(false, window, cx);
                    return true;
                }
                ("y", false) => {
                    self.huge_history(true, window, cx);
                    return true;
                }
                ("z", true) => {
                    self.huge_history(true, window, cx);
                    return true;
                }
                ("home", false) => {
                    self.huge_jump_line(1, 1, window, cx);
                    return true;
                }
                ("end", false) => {
                    self.huge_jump_end(window, cx);
                    return true;
                }
                _ => {}
            }
        }

        match (key.as_str(), shift) {
            ("n", false) => self.new_file(window, cx),
            ("o", false) => self.open_file_dialog(window, cx),
            ("o", true) => self.open_folder_dialog(window, cx),
            ("s", false) => self.save_active(window, cx),
            ("s", true) => self.save_active_as(window, cx),
            ("w", false) => self.request_close_active(window, cx),
            ("w", true) => self.request_close_all(window, cx),
            ("f", false) => self.open_find(false, window, cx),
            ("h", false) => self.open_find(true, window, cx),
            ("f", true) => self.open_find_in_files(window, cx),
            ("g", false) => self.open_go_to_line(window, cx),
            ("d", false) => self.duplicate_line(window, cx),
            ("k", true) => self.delete_line(window, cx),
            ("/", false) => self.toggle_comment(window, cx),
            ("=", false) | ("+", false) => self.zoom(1.0, cx),
            ("-", false) => self.zoom(-1.0, cx),
            ("0", false) => self.reset_zoom(cx),
            ("tab", false) | ("pagedown", false) => self.next_tab(1, window, cx),
            ("tab", true) | ("pageup", false) => self.next_tab(-1, window, cx),
            ("q", false) => self.request_exit(window, cx),
            ("1", false) => self.activate_group_position(0, window, cx),
            ("2", false) => self.activate_group_position(1, window, cx),
            ("3", false) => self.activate_group_position(2, window, cx),
            ("4", false) => self.activate_group_position(3, window, cx),
            ("5", false) => self.activate_group_position(4, window, cx),
            ("6", false) => self.activate_group_position(5, window, cx),
            ("7", false) => self.activate_group_position(6, window, cx),
            ("8", false) => self.activate_group_position(7, window, cx),
            ("9", false) => self.activate_group_position(8, window, cx),
            _ => return false,
        }
        true
    }

    fn activate_tab_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.tabs.len() {
            if index != self.active {
                self.record_navigation(cx);
            }
            self.sync_active(window, cx);
            self.active = index;
            let group = self.tabs[index].group;
            self.active_group = group;
            self.active_in_group[group] = Some(self.tabs[index].id);
            self.focus_editor = true;
            self.find.clear();
            self.find.dirty = true;
            self.record_navigation(cx);
            cx.notify();
        }
    }

    /// Activate a tab by its position inside the active group.
    fn activate_group_position(
        &mut self,
        position: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let memberships: Vec<usize> = self.tabs.iter().map(|tab| tab.group).collect();
        let members = tab_groups::group_tabs(&memberships, self.active_group);
        if let Some(index) = members.get(position).copied() {
            self.activate_tab_index(index, window, cx);
        }
    }

    fn next_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let memberships: Vec<usize> = self.tabs.iter().map(|tab| tab.group).collect();
        let members = tab_groups::group_tabs(&memberships, self.active_group);
        if members.len() < 2 {
            return;
        }
        self.sync_active(window, cx);
        let current = members
            .iter()
            .position(|index| *index == self.active)
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(members.len() as isize) as usize;
        self.active = members[next];
        self.active_in_group[self.active_group] = Some(self.tabs[self.active].id);
        self.focus_editor = true;
        self.find.clear();
        self.find.dirty = true;
        cx.notify();
    }

    fn open_go_to_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = &self.go_to_line {
            state.input.update(cx, |state, cx| state.focus(window, cx));
            return;
        }
        let mut state = GoToLineState::new(window, cx);
        let subscription = cx.subscribe(&state.input, |this, _, event, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.pending_go_to_line = true;
                cx.notify();
            }
        });
        state.subscription = Some(subscription);
        state.input.update(cx, |state, cx| state.focus(window, cx));
        self.go_to_line = Some(state);
        cx.notify();
    }

    fn apply_go_to_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.go_to_line.take() else {
            return;
        };
        let value = state.input.read(cx).value().to_string();
        let mut parts = value.split([':', ',']).map(|p| p.trim().parse::<usize>());
        let line = parts.next().and_then(Result::ok).unwrap_or(1).max(1);
        let column = parts.next().and_then(Result::ok).unwrap_or(1).max(1);
        let index = self.active;
        if self.tabs.get(index).is_some_and(|tab| tab.doc.is_huge()) {
            self.huge_jump_line(line as u64, column as u64, window, cx);
            self.focus_editor = true;
            cx.notify();
            return;
        }
        self.record_navigation(cx);
        let (position, position_end) = self.line_selection_in_tab(index, line, column, 0, cx);
        let _ = position_end;
        self.select_char_range(position, position, window, cx);
        self.focus_editor = true;
        self.record_navigation(cx);
        cx.notify();
    }

    fn open_language_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = &self.language_picker {
            state.filter.update(cx, |state, cx| {
                state.set_value(String::new(), window, cx);
                state.focus(window, cx);
            });
            return;
        }
        let state = LanguagePickerState::new(window, cx);
        state
            .filter
            .update(cx, |filter, cx| filter.focus(window, cx));
        self.language_picker = Some(state);
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Menus
    // ------------------------------------------------------------------

    /// Label for a command, sourced from the registry so menus and the
    /// command palette cannot drift.
    fn command_label(id: &str) -> String {
        commands::spec(id)
            .map(|spec| tr(spec.label_key))
            .unwrap_or_else(|| id.to_string())
    }

    fn menu_item<F>(
        weak: &WeakEntity<Self>,
        label: impl Into<SharedString>,
        disabled: bool,
        checked: Option<bool>,
        action: F,
    ) -> PopupMenuItem
    where
        F: Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    {
        let weak = weak.clone();
        let mut item = PopupMenuItem::new(label.into()).disabled(disabled);
        if let Some(checked) = checked {
            item = item.checked(checked);
        }
        item.on_click(move |_, window, cx| {
            let _ = weak.update(cx, |this, cx| action(this, window, cx));
        })
    }

    /// Bound a menu to the height that fits below the menu row and let the
    /// rest scroll. Without a cap, a menu taller than the window is clamped to
    /// the top edge, covering the menu bar, while its last entries still run
    /// past the bottom edge and become unreachable.
    fn fit_menu(menu: PopupMenu, window: &Window) -> PopupMenu {
        // Title bar/menu row (~40px), dropdown offset (6px) and the positioner's
        // viewport margin (8px), rounded up.
        let available = (window.viewport_size().height - px(64.)).max(px(120.));
        menu.scrollable(true).max_h(available)
    }

    fn render_menu_buttons(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let settings = self.settings.clone();
        let recent = self.settings.recent_files.clone();
        let recent_workspaces = self.settings.recent_workspaces.clone();
        let large_file_open = self.large_file.open;
        let tab_count = self.tabs.len();
        let has_doc = tab_count > 0;
        let encoded = self
            .active_doc()
            .map(|d| d.encoding)
            .unwrap_or(TextEncoding::Utf8);
        let current_eol = self.active_doc().map(|d| d.eol).unwrap_or(Eol::Lf);
        let current_language = self
            .active_doc()
            .map(|d| d.language.clone())
            .unwrap_or_else(|| language::PLAIN_TEXT.to_string());
        let active_partial = self.active_doc().is_some_and(Document::is_partial);
        let active_huge = self.active_doc().is_some_and(Document::is_huge);
        let active_large = self.active_tab().is_some_and(|tab| tab.large_file);
        let active_non_text = self.active_doc().is_some_and(Document::is_non_text);
        let can_edit_full_text =
            has_doc && !active_huge && !active_partial && !active_large && !active_non_text;
        let closed_tab_count = self.closed_tabs.len();
        let can_go_back = self.nav.can_back();
        let can_go_forward = self.nav.can_forward();

        let weak_file = weak.clone();
        let weak_edit = weak.clone();
        let weak_search = weak.clone();
        let weak_view = weak.clone();
        let weak_settings = weak.clone();
        let weak_help = weak.clone();
        let recent_items = recent.clone();

        let file_menu = Button::new("menu-file")
            .label(tr("menu.file"))
            .ghost()
            .compact()
            .dropdown_menu(move |mut menu, window, _cx| {
                menu = menu
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.new"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.new", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.new-window"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.new-window", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.open"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.open", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.open-folder"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.open-folder", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.open-workspace"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.open-workspace", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.save-workspace"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.save-workspace", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.save-workspace-as"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.save-workspace-as", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.add-folder"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.add-folder", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.close-workspace"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.close-workspace", window, cx);
                        },
                    ))
                    .separator();
                if recent_items.is_empty() {
                    menu = menu.item(Self::menu_item(
                        &weak_file,
                        tr("menu.file.no_recent"),
                        true,
                        None,
                        |_, _, _| {},
                    ));
                } else {
                    for path in &recent_items {
                        let path = path.clone();
                        let label = tr_args(
                            "menu.file.recent",
                            &[(
                                "path",
                                truncate_middle(&path.display().to_string(), RECENT_PATH_COLUMNS),
                            )],
                        );
                        menu = menu.item(Self::menu_item(
                            &weak_file,
                            label,
                            false,
                            None,
                            move |this, window, cx| {
                                this.open_path(&path, window, cx);
                            },
                        ));
                    }
                }
                if !recent_workspaces.is_empty() {
                    menu = menu.separator();
                    for path in &recent_workspaces {
                        let path = path.clone();
                        let label = tr_args(
                            "menu.file.recent_workspace",
                            &[(
                                "path",
                                truncate_middle(&path.display().to_string(), RECENT_PATH_COLUMNS),
                            )],
                        );
                        menu = menu.item(Self::menu_item(
                            &weak_file,
                            label,
                            false,
                            None,
                            move |this, window, cx| {
                                this.open_workspace(&path, window, cx);
                            },
                        ));
                    }
                    menu = menu.item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.clear-recent-workspaces"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.clear-recent-workspaces", window, cx);
                        },
                    ));
                }
                menu = menu
                    .separator()
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.save"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("file.save", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.save-as"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("file.save-as", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.save-all"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("file.save-all", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.reload"),
                        !has_doc || (active_partial && !active_huge),
                        None,
                        |this, window, cx| {
                            this.run_command("file.reload", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.local-history"),
                        !has_doc || active_huge || active_partial,
                        None,
                        |this, window, cx| {
                            this.run_command("file.local-history", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.export-html"),
                        !has_doc || active_huge || active_partial,
                        None,
                        |this, window, cx| {
                            this.run_command("file.export-html", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.export-rtf"),
                        !has_doc || active_huge || active_partial,
                        None,
                        |this, window, cx| {
                            this.run_command("file.export-rtf", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.close"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("file.close", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.close-all"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("file.close-all", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("tabs.reopen-closed"),
                        closed_tab_count == 0,
                        None,
                        |this, window, cx| {
                            this.run_command("tabs.reopen-closed", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_file,
                        Self::command_label("file.exit"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("file.exit", window, cx);
                        },
                    ));
                Self::fit_menu(menu, window)
            });

        let edit_menu = Button::new("menu-edit")
            .label(tr("menu.edit"))
            .ghost()
            .compact()
            .dropdown_menu(move |menu, window, _cx| {
                let mut menu = menu
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.undo"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.undo", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.redo"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.redo", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.cut"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.cut", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.copy"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.copy", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.paste"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.paste", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.select-all"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.select-all", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.indent"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.indent", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.outdent"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.outdent", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.duplicate-line"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.duplicate-line", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.delete-line"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.delete-line", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.toggle-comment"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.toggle-comment", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.trim-whitespace"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.trim-whitespace", window, cx);
                        },
                    ))
                    .separator();
                menu = menu
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.uppercase"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.uppercase", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.lowercase"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.lowercase", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.title-case"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.title-case", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-asc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-asc", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-desc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-desc", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-ignore-case-asc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-ignore-case-asc", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-ignore-case-desc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-ignore-case-desc", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-numeric-asc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-numeric-asc", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-numeric-desc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-numeric-desc", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.sort-by-column"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.sort-by-column", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.filter-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.filter-lines", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.move-line-up"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.move-line-up", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.move-line-down"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.move-line-down", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.join-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.join-lines", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.split-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.split-lines", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.remove-duplicate-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.remove-duplicate-lines", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.remove-empty-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.remove-empty-lines", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("bookmarks.cut-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("bookmarks.cut-lines", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("bookmarks.copy-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("bookmarks.copy-lines", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("bookmarks.delete-lines"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("bookmarks.delete-lines", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.column-editor"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.column-editor", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.select-next-occurrence"),
                        !can_edit_full_text,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.select-next-occurrence", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.select-all-occurrences"),
                        !can_edit_full_text,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.select-all-occurrences", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.add-cursor-above"),
                        !can_edit_full_text,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.add-cursor-above", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.add-cursor-below"),
                        !can_edit_full_text,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.add-cursor-below", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.unfold-current"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.unfold-current", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("edit.unfold-all"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("edit.unfold-all", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("macro.toggle-recording"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("macro.toggle-recording", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("macro.play-last"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("macro.play-last", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("macro.save-recording"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("macro.save-recording", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        Self::command_label("macro.play-saved"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("macro.play-saved", window, cx);
                        },
                    ))
                    .separator();
                for eol in Eol::ALL {
                    let label = tr_args("menu.edit.eol", &[("eol", eol.long_label())]);
                    menu = menu.item(Self::menu_item(
                        &weak_edit,
                        label,
                        !has_doc,
                        Some(current_eol == eol),
                        move |this, _, cx| this.convert_eol(eol, cx),
                    ));
                }
                menu = menu.separator();
                for encoding in TextEncoding::ALL {
                    let label = tr_args("menu.edit.encoding", &[("encoding", encoding.label())]);
                    menu = menu.item(Self::menu_item(
                        &weak_edit,
                        label,
                        !has_doc,
                        Some(encoded == encoding),
                        move |this, _, cx| this.set_encoding(encoding, cx),
                    ));
                }
                Self::fit_menu(menu, window)
            });

        let search_menu = Button::new("menu-search")
            .label(tr("menu.search"))
            .ghost()
            .compact()
            .dropdown_menu(move |menu, _window, _cx| {
                menu.item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.find"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("search.find", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.replace"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("search.replace", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.find-next"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("search.find-next", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.find-previous"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("search.find-previous", window, cx);
                    },
                ))
                .separator()
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.go-to-line"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("search.go-to-line", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.find-in-files"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("search.find-in-files", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.workspace-replace"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("search.workspace-replace", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.undo-workspace-replace"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("search.undo-workspace-replace", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.clear-highlights"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("search.clear-highlights", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.mark-all"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("search.mark-all", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("search.clear-history"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("search.clear-history", window, cx);
                    },
                ))
                .separator()
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("bookmarks.toggle"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("bookmarks.toggle", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("bookmarks.next"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("bookmarks.next", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("bookmarks.previous"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("bookmarks.previous", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    Self::command_label("bookmarks.clear"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.run_command("bookmarks.clear", window, cx);
                    },
                ))
            });

        let view_menu = Button::new("menu-view")
            .label(tr("menu.view"))
            .ghost()
            .compact()
            .dropdown_menu(move |menu, window, _cx| {
                let mut menu = menu
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.word-wrap"),
                        false,
                        Some(settings.word_wrap),
                        |this, window, cx| {
                            this.settings.word_wrap = !this.settings.word_wrap;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.line-numbers"),
                        false,
                        Some(settings.show_line_numbers),
                        |this, window, cx| {
                            this.settings.show_line_numbers = !this.settings.show_line_numbers;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.toolbar"),
                        false,
                        Some(settings.show_toolbar),
                        |this, _window, cx| {
                            this.settings.show_toolbar = !this.settings.show_toolbar;
                            this.commit_settings();
                            cx.notify();
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.status-bar"),
                        false,
                        Some(settings.show_status_bar),
                        |this, _window, cx| {
                            this.settings.show_status_bar = !this.settings.show_status_bar;
                            this.commit_settings();
                            cx.notify();
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.explorer"),
                        false,
                        Some(settings.show_file_explorer),
                        |this, _window, cx| {
                            this.settings.show_file_explorer = !this.settings.show_file_explorer;
                            this.commit_settings();
                            cx.notify();
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.large-file-controller"),
                        !has_doc,
                        Some(large_file_open),
                        |this, window, cx| {
                            this.toggle_large_file_controller(window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.whitespace"),
                        false,
                        Some(settings.show_whitespace),
                        |this, window, cx| {
                            this.run_command("view.whitespace", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.indent-guides"),
                        false,
                        Some(settings.show_indent_guides),
                        |this, window, cx| {
                            this.run_command("view.indent-guides", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.ruler"),
                        false,
                        Some(settings.show_ruler),
                        |this, window, cx| {
                            this.run_command("view.ruler", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.document-map"),
                        false,
                        Some(settings.show_document_map),
                        |this, window, cx| {
                            this.run_command("view.document-map", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.bracket-colors"),
                        false,
                        Some(settings.bracket_colors),
                        |this, window, cx| {
                            this.run_command("view.bracket-colors", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("navigation.toggle-outline"),
                        false,
                        Some(settings.show_outline),
                        |this, window, cx| {
                            this.run_command("navigation.toggle-outline", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("navigation.toggle-breadcrumbs"),
                        false,
                        Some(settings.show_breadcrumbs),
                        |this, window, cx| {
                            this.run_command("navigation.toggle-breadcrumbs", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("navigation.back"),
                        !can_go_back,
                        None,
                        |this, window, cx| {
                            this.run_command("navigation.back", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("navigation.forward"),
                        !can_go_forward,
                        None,
                        |this, window, cx| {
                            this.run_command("navigation.forward", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("window.split-right"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("window.split-right", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("window.split-down"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("window.split-down", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("window.unsplit"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("window.unsplit", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("window.switch-group"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("window.switch-group", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("window.move-tab-to-other-group"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("window.move-tab-to-other-group", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("window.move-tab-to-new-window"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.run_command("window.move-tab-to-new-window", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.zoom-in"),
                        !has_doc,
                        None,
                        |this, _, cx| this.zoom(1.0, cx),
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.zoom-out"),
                        !has_doc,
                        None,
                        |this, _, cx| this.zoom(-1.0, cx),
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.zoom-reset"),
                        !has_doc,
                        None,
                        |this, _, cx| this.reset_zoom(cx),
                    ))
                    .separator();
                menu = menu
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.theme-light"),
                        false,
                        Some(settings.theme == ThemeChoice::Light),
                        |this, window, cx| {
                            this.settings.theme = ThemeChoice::Light;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.theme-dark"),
                        false,
                        Some(settings.theme == ThemeChoice::Dark),
                        |this, window, cx| {
                            this.settings.theme = ThemeChoice::Dark;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.theme-system"),
                        false,
                        Some(settings.theme == ThemeChoice::System),
                        |this, window, cx| {
                            this.settings.theme = ThemeChoice::System;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("palette.commands"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("palette.commands", window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("palette.quick-open"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("palette.quick-open", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.fullscreen"),
                        false,
                        None,
                        |this, window, cx| {
                            this.run_command("view.fullscreen", window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        Self::command_label("view.choose-language"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.open_language_picker(window, cx);
                        },
                    ));
                for name in language::names() {
                    let selected = current_language == name;
                    let name = name.to_string();
                    menu = menu.item(Self::menu_item(
                        &weak_view,
                        tr_args(
                            "menu.view.language",
                            &[("language", language_display_name(&name))],
                        ),
                        !has_doc,
                        Some(selected),
                        move |this, window, cx| this.set_language(name.clone(), window, cx),
                    ));
                }
                Self::fit_menu(menu, window)
            });

        let settings_menu = Button::new("menu-settings")
            .label(tr("menu.settings"))
            .ghost()
            .compact()
            .dropdown_menu(move |menu, _window, _cx| {
                menu.item(Self::menu_item(
                    &weak_settings,
                    Self::command_label("settings.preferences"),
                    false,
                    None,
                    |this, window, cx| this.open_preferences(window, cx),
                ))
                .separator()
                .item(Self::menu_item(
                    &weak_settings,
                    Self::command_label("settings.import"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("settings.import", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_settings,
                    Self::command_label("settings.export"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("settings.export", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_settings,
                    Self::command_label("settings.reset"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("settings.reset", window, cx);
                    },
                ))
            });

        let help_menu = Button::new("menu-help")
            .label(tr("menu.help"))
            .ghost()
            .compact()
            .dropdown_menu(move |menu, _window, _cx| {
                menu.item(Self::menu_item(
                    &weak_help,
                    Self::command_label("help.shortcuts"),
                    false,
                    None,
                    |this, _, cx| {
                        this.shortcuts_open = true;
                        cx.notify();
                    },
                ))
                .item(Self::menu_item(
                    &weak_help,
                    Self::command_label("help.open-log-dir"),
                    false,
                    None,
                    |this, window, cx| {
                        this.run_command("help.open-log-dir", window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_help,
                    Self::command_label("help.about"),
                    false,
                    None,
                    |this, _, cx| {
                        this.about_open = true;
                        cx.notify();
                    },
                ))
            });

        div()
            .flex()
            .items_center()
            .gap_1()
            .child(file_menu)
            .child(edit_menu)
            .child(search_menu)
            .child(view_menu)
            .child(settings_menu)
            .child(help_menu)
            .into_any_element()
    }

    /// Standalone menu row used when the platform falls back to server-side
    /// decorations and the application must not draw its own title bar.
    fn render_menu_bar(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let menus = self.render_menu_buttons(cx);
        div()
            .flex()
            .items_center()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.background)
            .child(menus)
            .into_any_element()
    }

    /// The application title bar owns the window chrome: application name,
    /// menus, and (on Windows/Linux) the window control buttons. The document
    /// name stays in the tab bar; the native window title is set separately.
    fn render_title_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let menus = self.render_menu_buttons(cx);
        let weak = cx.weak_entity();

        #[cfg(target_os = "linux")]
        let manual_controls = self
            .manual_window_chrome
            .then(|| Self::render_manual_window_controls(&weak, window, cx));
        #[cfg(not(target_os = "linux"))]
        let manual_controls: Option<AnyElement> = None;

        TitleBar::new()
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w(px(0.))
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child("Luban Notepad"),
                    )
                    .child(div().flex_shrink_0().child(menus)),
            )
            .when_some(manual_controls, |title_bar, controls| {
                title_bar.child(controls)
            })
            // `on_close_window` only takes effect on Linux, where the custom
            // close button must go through the same unsaved-changes flow as
            // the window manager's close request.
            .on_close_window(move |_, window, cx| {
                let should_close = weak
                    .update(cx, |this, cx| this.on_should_close(window, cx))
                    .unwrap_or(true);
                if should_close {
                    cx.quit();
                }
            })
            .into_any_element()
    }

    /// Window controls for the manually decorated X11 window. `TitleBar` omits
    /// its own controls whenever the platform reports server decorations, which
    /// is the state the window is created in to avoid the shadow margin, so the
    /// application draws them with the same sizes and hover colours.
    #[cfg(target_os = "linux")]
    fn render_manual_window_controls(
        weak: &gpui::WeakEntity<Self>,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        use gpui_kit::component::{
            Icon, IconName as ComponentIcon, Sizable as _, TITLE_BAR_HEIGHT,
        };

        #[derive(Clone, Copy)]
        enum Action {
            Minimize,
            Maximize,
            Close,
        }

        let supported = window.window_controls();
        let theme = cx.theme();
        let foreground = theme.colors.foreground;

        let control = |id: &'static str, icon: ComponentIcon, action: Action, close: bool| {
            let weak = weak.clone();
            let hover_bg = if close {
                theme.danger
            } else {
                theme.colors.secondary_hover
            };
            let hover_fg = if close {
                theme.danger_foreground
            } else {
                theme.colors.secondary_foreground
            };

            div()
                .id(id)
                .flex()
                .w(TITLE_BAR_HEIGHT)
                .h_full()
                .flex_shrink_0()
                .justify_center()
                .items_center()
                .text_color(foreground)
                .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    match action {
                        Action::Minimize => window.minimize_window(),
                        Action::Maximize => window.zoom_window(),
                        Action::Close => {
                            let should_close = weak
                                .update(cx, |this, cx| this.on_should_close(window, cx))
                                .unwrap_or(true);
                            if should_close {
                                cx.quit();
                            }
                        }
                    }
                })
                .child(Icon::new(icon).small())
        };

        let mut controls = div().flex().flex_shrink_0().items_center().h_full();
        if supported.minimize {
            controls = controls.child(control(
                "window-control-minimize",
                ComponentIcon::WindowMinimize,
                Action::Minimize,
                false,
            ));
        }
        if supported.maximize {
            let icon = if window.is_maximized() {
                ComponentIcon::WindowRestore
            } else {
                ComponentIcon::WindowMaximize
            };
            controls = controls.child(control(
                "window-control-maximize",
                icon,
                Action::Maximize,
                false,
            ));
        }
        controls
            .child(control(
                "window-control-close",
                ComponentIcon::WindowClose,
                Action::Close,
                true,
            ))
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // Toolbar / tabs / status
    // ------------------------------------------------------------------

    fn render_toolbar(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let wrap_enabled =
            self.settings.word_wrap && !self.active_tab().is_some_and(|tab| tab.large_file);
        let (theme_icon, theme_label) = if self.settings.theme == ThemeChoice::Light {
            (IconName::Moon, Self::command_label("view.theme-dark"))
        } else {
            (IconName::Sun, Self::command_label("view.theme-light"))
        };

        let mut file_group = ToolbarGroup::new("tb-file");
        let mut history_group = ToolbarGroup::new("tb-history");
        let mut search_group = ToolbarGroup::new("tb-search");
        let mut view_group = ToolbarGroup::new("tb-view");
        macro_rules! tool {
            ($group:ident, $id:literal, $icon:expr, $label:expr, $action:expr) => {{
                let weak = weak.clone();
                let label = $label;
                $group = $group.child(
                    Button::new($id)
                        .icon($icon)
                        .tooltip(label.clone())
                        .accessibility_label(label)
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| $action(this, window, cx));
                        }),
                );
            }};
        }
        tool!(
            file_group,
            "tb-new",
            IconName::FilePlus,
            Self::command_label("file.new"),
            |this: &mut Self, window, cx| {
                this.run_command("file.new", window, cx);
            }
        );
        tool!(
            file_group,
            "tb-open",
            IconName::FolderOpen,
            Self::command_label("file.open"),
            |this: &mut Self, window, cx| {
                this.run_command("file.open", window, cx);
            }
        );
        tool!(
            file_group,
            "tb-save",
            IconName::Save,
            Self::command_label("file.save"),
            |this: &mut Self, window, cx| {
                this.run_command("file.save", window, cx);
            }
        );
        tool!(
            history_group,
            "tb-undo",
            IconName::Undo,
            Self::command_label("edit.undo"),
            |this: &mut Self, window, cx| {
                this.run_command("edit.undo", window, cx);
            }
        );
        tool!(
            history_group,
            "tb-redo",
            IconName::Redo,
            Self::command_label("edit.redo"),
            |this: &mut Self, window, cx| {
                this.run_command("edit.redo", window, cx);
            }
        );
        tool!(
            search_group,
            "tb-find",
            IconName::Search,
            Self::command_label("search.find"),
            |this: &mut Self, window, cx| {
                this.run_command("search.find", window, cx);
            }
        );
        tool!(
            search_group,
            "tb-replace",
            IconName::Replace,
            Self::command_label("search.replace"),
            |this: &mut Self, window, cx| {
                this.run_command("search.replace", window, cx);
            }
        );
        tool!(
            search_group,
            "tb-findfiles",
            IconName::FolderSearch,
            Self::command_label("search.find-in-files"),
            |this: &mut Self, window, cx| {
                this.run_command("search.find-in-files", window, cx);
            }
        );
        let weak_wrap = weak.clone();
        let wrap_label = Self::command_label("view.word-wrap");
        view_group = view_group.child(
            Button::new("tb-wrap")
                .icon(IconName::TextWrap)
                .selected(wrap_enabled)
                .toggled(wrap_enabled)
                .tooltip(wrap_label.clone())
                .accessibility_label(wrap_label)
                .on_click(move |_, window, cx| {
                    let _ = weak_wrap.update(cx, |this, cx| {
                        this.run_command("view.word-wrap", window, cx);
                    });
                }),
        );
        tool!(
            view_group,
            "tb-theme",
            theme_icon,
            theme_label,
            |this: &mut Self, window, cx| {
                this.run_command("view.toggle-theme", window, cx);
            }
        );

        let border = cx.theme().colors.border;
        let background = cx.theme().colors.background;
        let separator = || {
            div()
                .w(px(1.))
                .h(px(20.))
                .bg(border)
                .mx_1()
                .into_any_element()
        };
        Toolbar::new("app-toolbar")
            .child(file_group)
            .content(separator())
            .child(history_group)
            .content(separator())
            .child(search_group)
            .content(separator())
            .child(view_group)
            .border_b_1()
            .border_color(border)
            .bg(background)
            .px_2()
            .into_any_element()
    }

    fn group_active_index(&self, group: usize) -> Option<usize> {
        if let Some(index) = self.active_in_group[group]
            .and_then(|id| self.tab_index_by_id(id))
            .filter(|index| self.tabs[*index].group == group)
        {
            return Some(index);
        }
        if self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.group == group)
        {
            return Some(self.active);
        }
        self.tabs.iter().position(|tab| tab.group == group)
    }

    fn render_tabs(
        &mut self,
        group: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.weak_entity();
        let group_active = self.group_active_index(group);
        let mut tabs_row = div()
            .flex()
            .items_center()
            .h(px(34.))
            .border_b_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.background);
        let mut tab_elements = Vec::new();
        let members: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| (tab.group == group).then_some(index))
            .collect();
        for index in members.iter().copied() {
            let tab = &self.tabs[index];
            let position = members
                .iter()
                .position(|member| *member == index)
                .unwrap_or(0);
            let active = group_active == Some(index);
            let id = tab.id;
            let has_path = tab.doc.path.is_some();
            let label = if tab.doc.is_modified() {
                format!("● {}", tab.doc.display_name())
            } else {
                tab.doc.display_name()
            };
            let weak_click = weak.clone();
            let weak_close = weak.clone();
            let weak_drop = weak.clone();
            let weak_drag_move = weak.clone();
            let weak_menu_close = weak.clone();
            let weak_menu_others = weak.clone();
            let weak_menu_right = weak.clone();
            let weak_menu_copy = weak.clone();
            let weak_menu_reveal = weak.clone();
            let weak_menu_reload = weak.clone();
            let weak_menu_other_group = weak.clone();
            let weak_menu_new_window = weak.clone();
            let drag_label: SharedString = label.clone().into();
            let interaction_id = ("tab", id);
            let tab_el = div()
                .id(interaction_id)
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .min_w(px(TAB_MIN_WIDTH))
                .max_w(px(TAB_MAX_WIDTH))
                .h_full()
                .overflow_hidden()
                .border_r_1()
                .border_color(cx.theme().colors.border)
                .cursor_pointer()
                .when(active, |this| this.bg(cx.theme().colors.secondary))
                .hover(|this| this.bg(cx.theme().colors.secondary_hover))
                .child(
                    div()
                        .text_sm()
                        .truncate()
                        .min_w(px(0.))
                        .when(active, |this| this.font_weight(FontWeight::MEDIUM))
                        .child(label),
                )
                .child(
                    div()
                        .id(format!("tab-close:{id}"))
                        .flex_shrink_0()
                        .px_1()
                        .rounded_sm()
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .hover(|this| {
                            this.bg(cx.theme().colors.danger)
                                .text_color(cx.theme().colors.danger_foreground)
                        })
                        .child("×")
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            let _ = weak_close.update(cx, |this, cx| {
                                this.close_tab_by_id(id, window, cx);
                            });
                        }),
                );
            let tab_el =
                tab_el
                    .on_click(move |_, window, cx| {
                        let _ = weak_click.update(cx, |this, cx| {
                            this.activate_tab_index(index, window, cx);
                        });
                    })
                    .on_drag(
                        TabDrag {
                            id,
                            label: drag_label,
                        },
                        |drag, _, _, cx| {
                            cx.new(|_| TabDragPreview {
                                label: drag.label.clone(),
                            })
                        },
                    )
                    .on_drag_move::<TabDrag>(move |event, window, cx| {
                        if event.drag(cx).id != id {
                            return;
                        }
                        let x = event.event.position.x;
                        let left = event.bounds.origin.x;
                        let right = left + event.bounds.size.width;
                        let _ = weak_drag_move.update(cx, |this, cx| {
                            if x > right {
                                this.move_tab_by_id(id, group, position + 2, window, cx);
                            } else if x < left && position > 0 {
                                this.move_tab_by_id(id, group, position - 1, window, cx);
                            }
                        });
                    })
                    .on_drop(move |drag: &TabDrag, window, cx| {
                        let _ = weak_drop.update(cx, |this, cx| {
                            this.move_tab_by_id(drag.id, group, position, window, cx);
                        });
                    })
                    .context_menu(move |menu, _, _| {
                        let weak_close = weak_menu_close.clone();
                        let weak_others = weak_menu_others.clone();
                        let weak_right = weak_menu_right.clone();
                        let weak_copy = weak_menu_copy.clone();
                        let weak_reveal = weak_menu_reveal.clone();
                        let weak_reload = weak_menu_reload.clone();
                        let weak_other_group = weak_menu_other_group.clone();
                        let weak_new_window = weak_menu_new_window.clone();
                        let mut menu = menu
                            .item(
                                PopupMenuItem::new(Self::command_label("file.close")).on_click(
                                    move |_, window, cx| {
                                        let _ = weak_close.update(cx, |this, cx| {
                                            this.close_tab_by_id(id, window, cx);
                                        });
                                    },
                                ),
                            )
                            .item(
                                PopupMenuItem::new(Self::command_label("tabs.close-others"))
                                    .on_click(move |_, window, cx| {
                                        let _ = weak_others.update(cx, |this, cx| {
                                            this.activate_tab_index(index, window, cx);
                                            this.request_trim_tabs(false, window, cx);
                                        });
                                    }),
                            )
                            .item(
                                PopupMenuItem::new(Self::command_label("tabs.close-right"))
                                    .on_click(move |_, window, cx| {
                                        let _ = weak_right.update(cx, |this, cx| {
                                            this.activate_tab_index(index, window, cx);
                                            this.request_trim_tabs(true, window, cx);
                                        });
                                    }),
                            )
                            .separator()
                            .item(
                                PopupMenuItem::new(Self::command_label("tabs.copy-path"))
                                    .disabled(!has_path)
                                    .on_click(move |_, window, cx| {
                                        let _ = weak_copy.update(cx, |this, cx| {
                                            this.activate_tab_index(index, window, cx);
                                            this.copy_active_path(cx);
                                        });
                                    }),
                            )
                            .item(
                                PopupMenuItem::new(Self::command_label("tabs.reveal"))
                                    .disabled(!has_path)
                                    .on_click(move |_, window, cx| {
                                        let _ = weak_reveal.update(cx, |this, cx| {
                                            this.activate_tab_index(index, window, cx);
                                            this.reveal_active_in_file_manager(cx);
                                        });
                                    }),
                            );
                        if has_path {
                            menu = menu.item(
                                PopupMenuItem::new(Self::command_label("file.reload")).on_click(
                                    move |_, window, cx| {
                                        let _ = weak_reload.update(cx, |this, cx| {
                                            this.activate_tab_index(index, window, cx);
                                            this.run_command("file.reload", window, cx);
                                        });
                                    },
                                ),
                            );
                        }
                        menu = menu
                            .separator()
                            .item(
                                PopupMenuItem::new(Self::command_label(
                                    "window.move-tab-to-other-group",
                                ))
                                .on_click(move |_, window, cx| {
                                    let _ = weak_other_group.update(cx, |this, cx| {
                                        this.activate_tab_index(index, window, cx);
                                        this.move_active_tab_to_other_group(window, cx);
                                    });
                                }),
                            )
                            .item(
                                PopupMenuItem::new(Self::command_label(
                                    "window.move-tab-to-new-window",
                                ))
                                .on_click(move |_, window, cx| {
                                    let _ = weak_new_window.update(cx, |this, cx| {
                                        this.activate_tab_index(index, window, cx);
                                        this.move_tab_to_new_window(window, cx);
                                    });
                                }),
                            );
                        menu
                    });
            tab_elements.push(tab_el.into_any_element());
        }
        tabs_row = tabs_row.children(tab_elements);
        let weak_new = weak.clone();
        let weak_drop_end = weak.clone();
        tabs_row = tabs_row.child(
            div()
                .id(format!("tab-new:{group}"))
                .px_3()
                .h_full()
                .flex()
                .items_center()
                .cursor_pointer()
                .text_lg()
                .hover(|this| this.bg(cx.theme().colors.secondary))
                .child("+")
                .on_click(move |_, window, cx| {
                    let _ = weak_new.update(cx, |this, cx| {
                        this.active_group = group;
                        this.new_file(window, cx);
                    });
                })
                .on_drop(move |drag: &TabDrag, window, cx| {
                    let _ = weak_drop_end.update(cx, |this, cx| {
                        this.move_tab_by_id(drag.id, group, usize::MAX, window, cx);
                    });
                }),
        );
        tabs_row.into_any_element()
    }

    /// Move a tab to `target_position` inside `target_group`. `usize::MAX`
    /// appends at the end. Moving to a group that does not exist yet opens a
    /// right split.
    fn move_tab_by_id(
        &mut self,
        dragged_id: u64,
        target_group: usize,
        target_position: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(from) = self.tab_index_by_id(dragged_id) else {
            return;
        };
        if target_group >= tab_groups::MAX_GROUPS {
            return;
        }
        if !self.split.is_split() && target_group == 1 {
            self.split_editor(SplitDirection::Right, window, cx);
        }
        let source_group = self.tabs[from].group;
        let memberships: Vec<usize> = self.tabs.iter().map(|tab| tab.group).collect();
        if source_group == target_group {
            let members = tab_groups::group_tabs(&memberships, target_group);
            let from_position = members.iter().position(|index| *index == from).unwrap_or(0);
            let target_position = target_position.min(members.len());
            if target_position == from_position || target_position == from_position + 1 {
                return;
            }
            let insertion = if target_position < members.len() {
                members[target_position]
            } else {
                members
                    .last()
                    .map(|index| index + 1)
                    .unwrap_or(self.tabs.len())
            };
            let Some(to) = reorder_index(self.tabs.len(), from, insertion) else {
                return;
            };
            let active_id = self.tabs.get(self.active).map(|tab| tab.id);
            let tab = self.tabs.remove(from);
            self.tabs.insert(to, tab);
            if let Some(active_id) = active_id {
                if let Some(index) = self.tab_index_by_id(active_id) {
                    self.active = index;
                }
            }
            self.save_session_now(cx);
            cx.notify();
            return;
        }

        // Cross-group move: insert at the requested slot in the target group.
        let target_members: Vec<usize> = memberships
            .iter()
            .enumerate()
            .filter_map(|(index, group)| (*group == target_group && index != from).then_some(index))
            .collect();
        let target_position = target_position.min(target_members.len());
        let insertion = if target_position < target_members.len() {
            target_members[target_position]
        } else {
            target_members
                .last()
                .map(|index| index + 1)
                .unwrap_or(self.tabs.len())
        };
        self.sync_active(window, cx);
        let mut tab = self.tabs.remove(from);
        tab.group = target_group;
        let insertion = if from < insertion {
            insertion - 1
        } else {
            insertion
        };
        let insertion = insertion.min(self.tabs.len());
        self.tabs.insert(insertion, tab);
        let id = self.tabs[insertion].id;
        self.active_group = target_group;
        self.active = insertion;
        self.active_in_group[target_group] = Some(id);
        self.save_session_now(cx);
        cx.notify();
    }

    fn render_status_bar(&mut self, cx: &gpui_kit::App) -> AnyElement {
        let (mut line, mut column) = (1, 1);
        let (mut sel_len, mut chars, mut lines) = (0, 0, 0);
        let mut large_file = false;
        let mut huge_label = String::new();
        let mut window_label = String::new();
        let mut save_label = String::new();
        if let Some(tab) = self.active_tab() {
            large_file = tab.large_file;
            if let Some(ui) = &tab.huge {
                let state = tab.editor.read(cx);
                let rope = state.text();
                chars = tab.doc.char_count();
                lines = tab.doc.line_count();
                let _ = rope;
                let (global_line, global_column) = tab
                    .doc
                    .huge()
                    .map(|huge| (huge.cursor_line, huge.cursor_column))
                    .unwrap_or((1, 1));
                line = usize::try_from(global_line).unwrap_or(usize::MAX);
                column = usize::try_from(global_column).unwrap_or(usize::MAX);
                let range = state.selected_range();
                let start = rope.byte_to_char_idx(range.start.min(rope.len()));
                let end = rope.byte_to_char_idx(range.end.min(rope.len()));
                sel_len = end.saturating_sub(start);
                huge_label = tr("status.huge_file");
                window_label = tr_args(
                    "status.huge_window",
                    &[
                        ("start", ui.window_line.to_string()),
                        ("end", ui.window_end_line().to_string()),
                        ("total", lines.to_string()),
                    ],
                );
                if ui.starts_mid_line {
                    window_label.push_str(&tr("status.huge_continuation"));
                }
                if let Some(save) = &ui.save {
                    let total = save.total.max(1);
                    let percent = (save
                        .progress
                        .load(std::sync::atomic::Ordering::Relaxed)
                        .saturating_mul(100)
                        / total)
                        .min(100);
                    save_label = tr_args(
                        "status.huge_saving_percent",
                        &[("percent", percent.to_string())],
                    );
                } else if let Some(search) = &ui.search {
                    let percent = search
                        .progress
                        .load(std::sync::atomic::Ordering::Relaxed)
                        .min(search.total)
                        .saturating_mul(100)
                        .checked_div(search.total)
                        .unwrap_or(0);
                    save_label = match search.kind {
                        SearchKind::ReplaceAll => tr("find.huge_replace_all_running"),
                        _ => tr_args(
                            "find.huge_searching_percent",
                            &[("percent", percent.to_string())],
                        ),
                    };
                }
            } else {
                let state = tab.editor.read(cx);
                let rope = state.text();
                chars = rope.len_chars();
                lines = rope.lines_len();
                let position = rope.offset_to_position(state.cursor());
                line = position.line as usize + 1;
                column = position.character as usize + 1;
                let range = state.selected_range();
                let start = rope.byte_to_char_idx(range.start.min(rope.len()));
                let end = rope.byte_to_char_idx(range.end.min(rope.len()));
                sel_len = end.saturating_sub(start);
            }
        }
        let eol_label = self.active_doc().map(|d| d.eol.label()).unwrap_or("LF");
        let encoding = self
            .active_doc()
            .map(|d| d.encoding.short_label())
            .unwrap_or("UTF-8");
        let language = self
            .active_doc()
            .map(|d| d.language.clone())
            .unwrap_or_else(|| language::PLAIN_TEXT.to_string());
        let language = language_display_name(&language);
        let zoom = (self.settings.font_size / 14.0 * 100.0).round() as i32;
        let modified = self.active_doc().is_some_and(Document::is_modified);
        let non_text = self.active_doc().is_some_and(Document::is_non_text);
        let partial_label = self
            .active_doc()
            .and_then(Document::partial_range)
            .map(|range| {
                let unit = self.settings.large_file_display_unit;
                tr_args(
                    "status.partial_file",
                    &[
                        (
                            "loaded",
                            format_bytes(range.end.saturating_sub(range.start), unit),
                        ),
                        ("total", format_bytes(range.total, unit)),
                        ("start", format_bytes(range.start, unit)),
                    ],
                )
            })
            .unwrap_or_default();
        let line_col = tr_args(
            "status.line_col",
            &[("line", line.to_string()), ("column", column.to_string())],
        );
        let selection = tr_args("status.selection", &[("count", sel_len.to_string())]);
        let chars_label = if chars == 1 {
            tr("status.chars.one")
        } else {
            tr_args("status.chars.other", &[("count", chars.to_string())])
        };
        let lines_label = if lines == 1 {
            tr("status.lines.one")
        } else {
            tr_args("status.lines.other", &[("count", lines.to_string())])
        };
        let state_label = if modified {
            tr("status.modified")
        } else {
            tr("status.saved_state")
        };
        let zoom_label = tr_args("status.zoom", &[("percent", zoom.to_string())]);
        let unit = self.settings.large_file_display_unit;
        let size_label = match self.active_doc() {
            Some(doc) if doc.path.is_some() => tr_args(
                "status.file_size",
                &[("size", format_bytes(doc.file_size, unit))],
            ),
            Some(_) => tr("status.file_size_unavailable"),
            None => String::new(),
        };
        let words = self.active_tab_mut().and_then(|tab| {
            if tab.large_file || tab.doc.is_huge() || tab.doc.is_partial() || tab.doc.is_non_text()
            {
                return None;
            }
            let revision = tab.doc.revision();
            match tab.word_count {
                Some((cached, count)) if cached == revision => Some(count),
                _ => {
                    let count = count_words(&tab.doc.text);
                    tab.word_count = Some((revision, count));
                    Some(count)
                }
            }
        });
        let words_label = match words {
            Some(count) => tr_args("status.words", &[("count", count.to_string())]),
            None if self.active_doc().is_some() => tr("status.words_unavailable"),
            None => String::new(),
        };
        let (indent_size, indent_spaces) = self
            .active_tab()
            .map(|tab| {
                if Self::language_indent_applies(&tab.doc, tab.large_file) {
                    (
                        self.settings.tab_size_for(&tab.doc.language),
                        self.settings.use_spaces_for(&tab.doc.language),
                    )
                } else {
                    (self.settings.tab_size, self.settings.use_spaces)
                }
            })
            .unwrap_or((self.settings.tab_size, self.settings.use_spaces));
        let indent_label = if indent_spaces {
            tr_args("status.indent_spaces", &[("size", indent_size.to_string())])
        } else {
            tr_args("status.indent_tab", &[("size", indent_size.to_string())])
        };
        let bookmark_label = self
            .active_tab()
            .filter(|tab| !tab.bookmarks.is_empty())
            .map(|tab| {
                tr_args(
                    "status.bookmarks",
                    &[("count", tab.bookmarks.len().to_string())],
                )
            })
            .unwrap_or_default();
        let macro_label = if self.macro_recording {
            tr_args(
                "status.macro_recording_count",
                &[("count", self.macro_recorded.len().to_string())],
            )
        } else {
            String::new()
        };
        let read_only_label = if self.active_doc().is_some_and(|doc| doc.read_only) {
            tr("status.read_only")
        } else {
            String::new()
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(24.))
            .px_3()
            .border_t_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.secondary)
            .text_xs()
            .text_color(cx.theme().colors.muted_foreground)
            .child(div().flex().items_center().gap_4().children([
                div().child(line_col),
                div().child(selection),
                div().child(chars_label),
                div().child(lines_label),
                div().child(size_label),
                div().child(words_label),
                div().child(indent_label),
                div().child(bookmark_label),
                div().child(macro_label),
                div().child(read_only_label),
                div().child(if large_file {
                    tr("status.large_file")
                } else {
                    String::new()
                }),
                div().child(huge_label),
                div().child(window_label),
                div().child(save_label),
                div().child(partial_label),
                div().child(if non_text {
                    tr("status.non_text")
                } else {
                    String::new()
                }),
                div().child(state_label),
                div().child(if self.status.is_empty() {
                    String::new()
                } else {
                    self.status.clone()
                }),
            ]))
            .child(div().flex().items_center().gap_4().children([
                div().child(eol_label),
                div().child(encoding),
                div().child(language),
                div().child(zoom_label),
            ]))
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // Editor area
    // ------------------------------------------------------------------

    fn render_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if !self.split.is_split() {
            return self.render_group(0, true, window, cx);
        }
        let first_active = self.active_group == 0;
        let first = self.render_group(0, first_active, window, cx);
        let second = self.render_group(1, !first_active, window, cx);
        let divider = div()
            .bg(cx.theme().colors.border)
            .when(self.split == SplitDirection::Right, |this| {
                this.w(px(1.)).h_full()
            })
            .when(self.split == SplitDirection::Down, |this| {
                this.h(px(1.)).w_full()
            });
        let layout = div().flex_1().min_w(px(0.)).min_h(px(0.)).flex();
        match self.split {
            SplitDirection::Right => layout.flex_row().child(first).child(divider).child(second),
            SplitDirection::Down => layout.flex_col().child(first).child(divider).child(second),
            SplitDirection::None => layout.child(first),
        }
        .into_any_element()
    }

    fn render_group(
        &mut self,
        group: usize,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tabs = self.render_tabs(group, window, cx);
        let editor = match self.group_active_index(group) {
            Some(index) => self.render_editor_area(index, window, cx),
            None => div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().colors.muted_foreground)
                .child(tr("split.empty_group"))
                .into_any_element(),
        };
        let weak_group = cx.weak_entity();
        div()
            .id(format!("editor-group:{group}"))
            .role(Role::Group)
            .aria_label(tr_args(
                "split.group_label",
                &[("group", (group + 1).to_string())],
            ))
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .when(active && self.split.is_split(), |this| {
                this.border_t_1().border_color(cx.theme().colors.border)
            })
            .child(tabs)
            .child(editor)
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                let _ = weak_group.update(cx, |this, cx| {
                    if this.active_group != group {
                        this.switch_group(Some(group), window, cx);
                    }
                });
            })
            .into_any_element()
    }

    fn render_editor_area(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.weak_entity();
        let Some(tab) = self.tabs.get(index) else {
            return div().size_full().into_any_element();
        };
        let editor = tab.editor.clone();
        let font_size = self.settings.font_size;
        let font_family = self.settings.font_family.clone();
        let show_line_numbers = self.settings.show_line_numbers;
        let _ = weak;

        // Disk-engine tabs mount only a bounded window, so the editor's own
        // scrollbar spans that window. Overlay a file-global scrollbar that
        // targets global lines and takes precedence over the window-local one.
        //
        // The editor's built-in scrollbar is laid out on the padded container,
        // while `input_bounds` is the text area inside that padding. Derive the
        // padding from the size tier passed to the editor so the overlay keeps
        // sharing the native track position.
        const EDITOR_SIZE: Size = Size::Medium;
        let padding_x = EDITOR_SIZE.input_px();
        let padding_y = EDITOR_SIZE.input_py();
        let global_scrollbar = tab.huge.as_ref().and_then(|ui| {
            let state = tab.editor.read(cx);
            let total_lines = tab.doc.line_count() as u64;
            let line_height = state
                .line_height()
                .map(|height| height.as_f32())
                .unwrap_or(0.0);
            let mut viewport = state.input_bounds();
            // `visible_row_range` is clipped to the mounted window, which can
            // be a single long line. The viewport's row capacity is the right
            // denominator for the whole-file mapping and for deciding whether
            // a line scrollbar is meaningful.
            let viewport_lines = if line_height > 0.0 {
                (viewport.size.height.as_f32() / line_height)
                    .floor()
                    .max(1.0) as u64
            } else {
                0
            };
            let view_bytes = tab
                .doc
                .partial_range()
                .map(|range| range.end.saturating_sub(range.start))
                .unwrap_or(tab.doc.file_size);
            let average_line_bytes = view_bytes / total_lines.max(1);
            viewport.origin.y -= padding_y;
            viewport.size.height += padding_y * 2.;
            viewport.size.width += padding_x;
            if !huge_scrollbar::line_scrollbar_is_meaningful(
                total_lines,
                viewport_lines,
                average_line_bytes,
            ) || line_height <= 0.0
                || viewport.size.width <= px(0.0)
                || viewport.size.height <= px(0.0)
            {
                return None;
            }
            Some(huge_scrollbar::render(
                HugeScrollbarProps {
                    top_line: ui.scroll_target.unwrap_or(ui.scroll_top_line),
                    total_lines,
                    viewport_lines: viewport_lines.max(1),
                    line_height,
                    viewport,
                    shared: ui.scroll.clone(),
                },
                cx.theme().colors.background,
            ))
        });

        // Disk-engine tabs hide the editor's window-local gutter and render a
        // document-global one next to the editor. It is positioned from the
        // same scroll offset as the file scrollbar, so both follow wheel
        // scrolling and window swaps.
        let global_gutter = tab.huge.as_ref().and_then(|ui| {
            if !show_line_numbers {
                return None;
            }
            let state = tab.editor.read(cx);
            let line_height = state
                .line_height()
                .map(|height| height.as_f32())
                .unwrap_or(0.0);
            if line_height <= 0.0 {
                return None;
            }
            let input = state.input_bounds();
            let viewport_lines = (input.size.height.as_f32() / line_height).floor().max(1.0) as u64;
            Some(huge_gutter::render(
                HugeGutterProps {
                    offset_y: state.scroll_offset().y.as_f32(),
                    line_height,
                    top_padding: padding_y.as_f32(),
                    viewport_lines,
                    window_line: ui.window_line,
                    window_newlines: ui.window_newlines,
                    total_lines: tab.doc.line_count() as u64,
                    font_family: &font_family,
                    font_size,
                },
                window,
                cx,
            ))
        });

        let breadcrumb = self.render_breadcrumbs(index, cx);
        let ruler = self.render_ruler(index, cx);
        let map = self.render_document_map(index, cx);
        div()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .children(breadcrumb)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .p_1()
                            .child(
                                div()
                                    .flex()
                                    .h_full()
                                    .min_h(px(0.))
                                    .children(global_gutter)
                                    .child(
                                        div().flex_1().min_w(px(0.)).h_full().child(
                                            Editor::new(&editor)
                                                .with_size(EDITOR_SIZE)
                                                .appearance(false)
                                                .bordered(false)
                                                .size_full()
                                                .text_size(px(font_size))
                                                .when(!font_family.is_empty(), |this| {
                                                    this.font_family(font_family.clone())
                                                }),
                                        ),
                                    ),
                            )
                            .children(ruler)
                            .children(global_scrollbar),
                    )
                    .children(map),
            )
            .into_any_element()
    }

    /// Breadcrumb row for the tab: workspace ─ directories ─ file ─ symbol.
    fn render_breadcrumbs(&mut self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.settings.show_breadcrumbs {
            return None;
        }
        let tab = self.tabs.get(index)?;
        let simple = tab.doc.is_huge()
            || tab.large_file
            || tab.doc.partial_range().is_some()
            || tab.doc.is_non_text();
        let file_name = tab.doc.display_name();
        let relative = tab
            .doc
            .path
            .as_ref()
            .and_then(|path| {
                self.workspace_roots
                    .iter()
                    .find_map(|root| path.strip_prefix(root).ok())
            })
            .filter(|_| !simple);
        let root_label = if simple {
            None
        } else if !self.workspace_name.is_empty() {
            Some(self.workspace_name.clone())
        } else if let Some(path) = &tab.doc.path {
            path.parent()
                .and_then(|parent| parent.file_name())
                .map(|name| name.to_string_lossy().into_owned())
        } else {
            None
        };
        let symbol = if simple || tab.doc.is_untitled() || index != self.active {
            None
        } else {
            let cursor_line = line_ops::line_of_char(&tab.doc.text, tab.doc.cursor) + 1;
            luban_core::symbols::containing_symbol(&self.outline.symbols, cursor_line)
                .map(|symbol| (symbol.name.clone(), symbol.line))
        };
        let symbol_name = symbol.as_ref().map(|(name, _)| name.as_str());
        let mut segments = crate::navigation::breadcrumbs(
            root_label.as_deref(),
            relative,
            &file_name,
            symbol_name,
        );
        if let Some((_, line)) = &symbol {
            if let Some(last) = segments.last_mut() {
                last.action = BreadcrumbAction::Symbol(*line);
            }
        }
        let weak = cx.weak_entity();
        let mut row = div()
            .id("breadcrumbs")
            .role(Role::Navigation)
            .aria_label(tr("breadcrumbs.label"))
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .py_1()
            .text_xs()
            .text_color(cx.theme().colors.muted_foreground)
            .border_b_1()
            .border_color(cx.theme().colors.border);
        for (position, segment) in segments.into_iter().enumerate() {
            if position > 0 {
                row = row.child("›");
            }
            let label = segment.label.clone();
            let action = segment.action.clone();
            let weak_segment = weak.clone();
            row = row.child(
                div()
                    .id(format!("breadcrumb:{position}"))
                    .cursor_pointer()
                    .rounded_sm()
                    .px_1()
                    .hover(|this| this.bg(cx.theme().colors.secondary))
                    .child(label)
                    .on_click(move |_, window, cx| {
                        let action = action.clone();
                        let _ = weak_segment.update(cx, |this, cx| match action {
                            BreadcrumbAction::Root => {}
                            BreadcrumbAction::Directory(prefix) => {
                                let query = crate::quick_open::directory_query(&prefix);
                                this.open_quick_open_with(query, window, cx);
                            }
                            BreadcrumbAction::File(name) => {
                                this.open_quick_open_with(name, window, cx);
                            }
                            BreadcrumbAction::Symbol(line) => {
                                this.jump_to_line_in_tab(index, line, window, cx);
                            }
                        });
                    }),
            );
        }
        Some(row.into_any_element())
    }

    /// 1px ruler lines at the configured character columns.
    fn render_ruler(&mut self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.settings.show_ruler || self.settings.word_wrap {
            return None;
        }
        let tab = self.tabs.get(index)?;
        if tab.doc.is_huge()
            || tab.large_file
            || tab.doc.partial_range().is_some()
            || tab.doc.is_non_text()
        {
            return None;
        }
        let font_size = self.settings.font_size;
        let char_width = (font_size * 0.6).max(1.0);
        let scroll_x = tab.editor.read(cx).scroll_offset().x.as_f32();
        let columns = self.settings.ruler_columns.clone();
        if columns.is_empty() {
            return None;
        }
        let lines: Vec<AnyElement> = columns
            .into_iter()
            .map(|column| {
                let x = (column as f32 - 1.0) * char_width - scroll_x;
                div()
                    .absolute()
                    .top(px(0.))
                    .left(px(x))
                    .w(px(1.))
                    .h_full()
                    .bg(cx.theme().colors.border)
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .absolute()
                .inset_0()
                .overflow_hidden()
                .children(lines)
                .into_any_element(),
        )
    }

    /// Map on the right edge. Ordinary documents show sampled line widths;
    /// huge/partial documents show only the viewport proportion.
    fn render_document_map(&mut self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.settings.show_document_map {
            return None;
        }
        let tab = self.tabs.get(index)?;
        let state = tab.editor.read(cx);
        let total_lines = self.tab_line_count(index, cx);
        let line_height = state
            .line_height()
            .map(|height| height.as_f32())
            .unwrap_or(0.0);
        let input = state.input_bounds();
        if !document_map::should_show(total_lines, input.size.height.as_f32()) {
            return None;
        }
        if tab.doc.is_non_text() {
            return Some(
                div()
                    .id("document-map-empty")
                    .w(px(document_map::MAP_WIDTH))
                    .h_full()
                    .border_l_1()
                    .border_color(cx.theme().colors.border)
                    .bg(cx.theme().colors.secondary)
                    .role(Role::List)
                    .aria_label(tr("view.document_map"))
                    .into_any_element(),
            );
        }
        let viewport_lines = if line_height > 0.0 {
            (input.size.height.as_f32() / line_height).floor().max(1.0)
        } else {
            1.0
        };
        let start_line = if tab.doc.is_huge() {
            tab.huge
                .as_ref()
                .map(|ui| ui.window_line.saturating_sub(1) as f32)
                .unwrap_or(0.0)
        } else {
            let scroll = state.scroll_offset().y.as_f32();
            (-scroll / line_height.max(1.0)).max(0.0)
        };
        let (top, height) = document_map::viewport_rect(total_lines, start_line, viewport_lines);
        let mut map = div()
            .id("document-map")
            .relative()
            .w(px(document_map::MAP_WIDTH))
            .h_full()
            .border_l_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.secondary)
            .overflow_hidden()
            .role(Role::List)
            .aria_label(tr("view.document_map"));
        if !tab.doc.is_huge()
            && tab.doc.partial_range().is_none()
            && !tab.large_file
            && tab.doc.text.len() <= 8 * 1024 * 1024
        {
            let samples = document_map::sample(&tab.doc.text, document_map::MAX_MAP_ROWS);
            let bins = 160usize.min(samples.len());
            if bins > 0 {
                let per_bin = (samples.len() as f32 / bins as f32).ceil().max(1.0) as usize;
                let mut bars = Vec::new();
                for bin in 0..bins {
                    let start = bin * per_bin;
                    if start >= samples.len() {
                        break;
                    }
                    let end = (start + per_bin).min(samples.len());
                    let width = samples[start..end]
                        .iter()
                        .map(|row| row.width)
                        .fold(0.0f32, f32::max);
                    bars.push(
                        div()
                            .flex_1()
                            .w_full()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .h(px(1.))
                                    .w(relative(width.clamp(0.03, 1.0)))
                                    .bg(cx.theme().colors.muted_foreground),
                            )
                            .into_any_element(),
                    );
                }
                map = map.child(
                    div()
                        .absolute()
                        .inset_0()
                        .px_2()
                        .flex()
                        .flex_col()
                        .children(bars),
                );
            }
        }
        map = map.child(
            div()
                .absolute()
                .left(px(0.))
                .w_full()
                .top(relative(top))
                .h(relative(height.max(0.02)))
                .bg(cx.theme().colors.selection)
                .into_any_element(),
        );
        let weak_map = cx.weak_entity();
        let weak_map_move = weak_map.clone();
        let map_top = input.origin.y.as_f32();
        let map_height = input.size.height.as_f32().max(1.0);
        let map_line_height = line_height.max(1.0);
        let map_total = total_lines;
        let map = map
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                let ratio = (event.position.y.as_f32() - map_top) / map_height;
                let line = document_map::line_at_ratio(ratio, map_total);
                let shift = event.modifiers.shift;
                let _ = weak_map.update(cx, |this, cx| {
                    this.map_scroll_to(index, line, shift, map_line_height, window, cx);
                });
            })
            .on_mouse_move(move |event: &MouseMoveEvent, window, cx| {
                if event.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                let ratio = (event.position.y.as_f32() - map_top) / map_height;
                let line = document_map::line_at_ratio(ratio, map_total);
                let _ = weak_map_move.update(cx, |this, cx| {
                    this.map_scroll_to(index, line, false, map_line_height, window, cx);
                });
            });
        Some(map.into_any_element())
    }

    /// Scroll the viewport to a document-map line. Shift also moves the caret.
    fn map_scroll_to(
        &mut self,
        index: usize,
        line: usize,
        move_caret: bool,
        line_height: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index != self.active {
            return;
        }
        if self.tabs.get(index).is_some_and(|tab| tab.doc.is_huge()) {
            let viewport_lines = self
                .tabs
                .get(index)
                .map(|tab| {
                    let state = tab.editor.read(cx);
                    let height = state.input_bounds().size.height.as_f32();
                    (height / line_height).floor().max(1.0) as u64
                })
                .unwrap_or(1);
            self.huge_scroll_to(index, line as u64, viewport_lines, line_height, window, cx);
            if move_caret {
                self.huge_jump_line(line as u64, 1, window, cx);
            }
            return;
        }
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let editor = tab.editor.clone();
        let y = -((line.saturating_sub(1) as f32) * line_height);
        editor.update(cx, |state, cx| {
            state.set_scroll_offset(point(px(0.), px(y)), cx);
        });
        if move_caret {
            self.jump_to_line_in_tab(index, line, window, cx);
        }
        cx.notify();
    }

    fn render_side_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let explorer = self.render_explorer(window, cx);
        let outline = if self.settings.show_outline {
            Some(self.render_outline(cx))
        } else {
            None
        };
        div()
            .w(px(self.explorer.width))
            .h_full()
            .flex()
            .flex_col()
            .child(explorer)
            .children(outline)
            .into_any_element()
    }

    /// Symbol outline below the file tree.
    fn render_outline(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.outline.rows();
        let truncated = self.outline.truncated;
        let error = self.outline.error.clone();
        let supported = self.outline.supported;
        let has_document = !self.tabs.is_empty();
        let weak = cx.weak_entity();
        let mut panel = div()
            .h(px(190.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.background)
            .id("outline-panel")
            .track_focus(&self.outline_focus)
            .tab_stop(true)
            .focus_visible(|style| style.border_color(cx.theme().colors.ring))
            .role(Role::List)
            .aria_label(tr("outline.title"));
        panel = panel.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py_2()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(cx.theme().colors.muted_foreground)
                .child(tr("outline.title"))
                .when(truncated, |this| this.child(tr("outline.truncated"))),
        );
        if rows.is_empty() {
            let message = if !has_document {
                tr("outline.no_document")
            } else if let Some(error) = error {
                error
            } else if supported {
                tr("outline.empty")
            } else {
                tr("outline.unsupported")
            };
            panel = panel.child(
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(message),
            );
            return panel.into_any_element();
        }
        let mut list = Vec::new();
        for row in rows {
            let symbol = self.outline.symbols[row.index].clone();
            let line = symbol.line;
            let weak_row = weak.clone();
            let weak_toggle = weak.clone();
            let selected = self.outline_selected == Some(line);
            list.push(
                div()
                    .id(format!("outline-row:{}", symbol.line))
                    .role(Role::ListItem)
                    .aria_label(symbol.name.clone())
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl(px(8.0 + row.depth as f32 * 12.0))
                    .pr_2()
                    .py_1()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(cx.theme().colors.secondary))
                    .hover(|this| this.bg(cx.theme().colors.secondary_hover))
                    .when(row.has_children, |this| {
                        this.child(
                            div()
                                .id(format!("outline-toggle:{}", symbol.line))
                                .px_1()
                                .cursor_pointer()
                                .child(if row.collapsed { "▸" } else { "▾" })
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    let _ = weak_toggle.update(cx, |this, cx| {
                                        this.outline.toggle(line);
                                        cx.notify();
                                    });
                                }),
                        )
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(symbol.kind.clone()),
                    )
                    .child(div().truncate().child(symbol.name.clone()))
                    .on_click(move |_, window, cx| {
                        let _ = weak_row.update(cx, |this, cx| {
                            this.outline_focus.focus(window, cx);
                            this.outline_selected = Some(line);
                            this.jump_to_line_in_tab(this.active, line, window, cx);
                        });
                    })
                    .into_any_element(),
            );
        }
        panel
            .child(
                div()
                    .id("outline-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(list),
            )
            .into_any_element()
    }

    fn render_explorer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let show_hidden = self.settings.show_hidden_files;
        let rows = self
            .explorer
            .visible_rows(self.workspace_index.as_ref(), show_hidden);
        let selected = self.explorer.selected.clone();
        let indexing = self.indexing;
        let truncated = self
            .workspace_index
            .as_ref()
            .is_some_and(|index| !index.truncated_roots.is_empty());
        let mut panel = div()
            .w_full()
            .flex_1()
            .min_h(px(120.))
            .flex()
            .flex_col()
            .id("explorer-panel")
            .border_r_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.background)
            .track_focus(&self.explorer_focus)
            .tab_stop(true)
            .focus_visible(|style| style.border_color(cx.theme().colors.ring))
            .role(Role::Tree)
            .aria_label(tr("explorer.title"));
        panel = panel.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_1()
                .px_3()
                .py_2()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(cx.theme().colors.muted_foreground)
                .child(div().truncate().child(if self.workspace_name.is_empty() {
                    tr("explorer.title")
                } else {
                    self.workspace_name.clone()
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .id("explorer-open-folder")
                                .px_1()
                                .rounded_sm()
                                .cursor_pointer()
                                .hover(|this| this.bg(cx.theme().colors.secondary))
                                .child(tr("explorer.open_folder"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.run_command("file.open-folder", window, cx);
                                })),
                        )
                        .child(
                            div()
                                .id("explorer-refresh")
                                .px_1()
                                .rounded_sm()
                                .cursor_pointer()
                                .hover(|this| this.bg(cx.theme().colors.secondary))
                                .child("⟳")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.refresh_workspace(cx);
                                })),
                        ),
                ),
        );
        if indexing {
            panel = panel.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr("explorer.indexing")),
            );
        }
        if truncated {
            panel = panel.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().colors.warning)
                    .child(tr("explorer.index_truncated")),
            );
        }
        if rows.is_empty() {
            panel = panel.child(
                div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_sm()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr("explorer.empty"))
                    .child(
                        div()
                            .id("explorer-empty-open-workspace")
                            .cursor_pointer()
                            .child(tr("workspace.open"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.open_workspace_dialog(window, cx);
                            })),
                    ),
            );
        } else {
            let mut list = Vec::new();
            for row in rows {
                let path = row.path.clone();
                let name = row.name.clone();
                let is_dir = row.is_dir;
                let depth = row.depth;
                let is_root = depth == 0;
                let expanded = self.explorer.is_expanded(&path);
                let is_selected = selected
                    .as_ref()
                    .is_some_and(|selected| util::same_path(selected, &path));
                let weak_click = weak.clone();
                let weak_menu_new_file = weak.clone();
                let weak_menu_new_folder = weak.clone();
                let weak_menu_rename = weak.clone();
                let weak_menu_delete = weak.clone();
                let weak_menu_refresh = weak.clone();
                let weak_menu_copy = weak.clone();
                let weak_menu_reveal = weak.clone();
                let weak_menu_remove = weak.clone();
                let weak_menu_add = weak.clone();
                let row_context_path = path.clone();
                let context_is_dir = is_dir;
                let context_is_root = is_root;
                let context_root = row.root.clone();
                let mut element = div()
                    .id(format!("explorer-row:{}", path.display()))
                    .role(if is_dir {
                        Role::TreeItem
                    } else {
                        Role::ListItem
                    })
                    .aria_label(name.clone())
                    .aria_selected(is_selected)
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl(px(8.0 + depth as f32 * 12.0))
                    .pr_2()
                    .py_1()
                    .text_sm()
                    .cursor_pointer()
                    .when(is_selected, |this| this.bg(cx.theme().colors.secondary))
                    .hover(|this| this.bg(cx.theme().colors.secondary_hover))
                    .child(match (is_dir, expanded) {
                        (false, _) => "·".to_string(),
                        (true, true) => "▾".to_string(),
                        (true, false) => "▸".to_string(),
                    })
                    .child(div().truncate().child(name.clone()))
                    .on_click(move |_, window, cx| {
                        let _ = weak_click.update(cx, |this, cx| {
                            this.explorer_focus.focus(window, cx);
                            this.explorer.select(path.clone());
                            if context_is_dir {
                                this.explorer
                                    .toggle_directory(&path, this.settings.show_hidden_files);
                            } else {
                                this.open_path(&path, window, cx);
                            }
                            cx.notify();
                        });
                    })
                    .context_menu(move |menu, _, _| {
                        let mut menu = menu
                            .item(PopupMenuItem::new(tr("explorer.new_file")).on_click({
                                let weak = weak_menu_new_file.clone();
                                let dir = if context_is_dir {
                                    row_context_path.clone()
                                } else {
                                    row_context_path
                                        .parent()
                                        .map(Path::to_path_buf)
                                        .unwrap_or_else(|| context_root.clone())
                                };
                                move |_, window, cx| {
                                    let dir = dir.clone();
                                    let _ = weak.update(cx, |this, cx| {
                                        this.start_name_prompt(
                                            NamePromptKind::NewFile,
                                            dir,
                                            None,
                                            String::new(),
                                            window,
                                            cx,
                                        );
                                    });
                                }
                            }))
                            .item(PopupMenuItem::new(tr("explorer.new_folder")).on_click({
                                let weak = weak_menu_new_folder.clone();
                                let dir = if context_is_dir {
                                    row_context_path.clone()
                                } else {
                                    row_context_path
                                        .parent()
                                        .map(Path::to_path_buf)
                                        .unwrap_or_else(|| context_root.clone())
                                };
                                move |_, window, cx| {
                                    let dir = dir.clone();
                                    let _ = weak.update(cx, |this, cx| {
                                        this.start_name_prompt(
                                            NamePromptKind::NewFolder,
                                            dir,
                                            None,
                                            String::new(),
                                            window,
                                            cx,
                                        );
                                    });
                                }
                            }));
                        if !context_is_root {
                            menu = menu
                                .separator()
                                .item(PopupMenuItem::new(tr("explorer.rename")).on_click({
                                    let weak = weak_menu_rename.clone();
                                    let path = row_context_path.clone();
                                    let name = name.clone();
                                    move |_, window, cx| {
                                        let path = path.clone();
                                        let name = name.clone();
                                        let _ = weak.update(cx, |this, cx| {
                                            let parent = path
                                                .parent()
                                                .map(Path::to_path_buf)
                                                .unwrap_or_default();
                                            this.start_name_prompt(
                                                NamePromptKind::Rename,
                                                parent,
                                                Some(path),
                                                name,
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                }))
                                .item(PopupMenuItem::new(tr("explorer.delete")).on_click({
                                    let weak = weak_menu_delete.clone();
                                    let path = row_context_path.clone();
                                    move |_, _, cx| {
                                        let path = path.clone();
                                        let _ = weak.update(cx, |this, cx| {
                                            this.request_delete_path(path, cx);
                                        });
                                    }
                                }));
                        }
                        menu = menu
                            .separator()
                            .item(PopupMenuItem::new(tr("explorer.refresh")).on_click({
                                let weak = weak_menu_refresh.clone();
                                move |_, _, cx| {
                                    let _ = weak.update(cx, |this, cx| {
                                        this.refresh_workspace(cx);
                                    });
                                }
                            }))
                            .item(PopupMenuItem::new(tr("explorer.copy_path")).on_click({
                                let weak = weak_menu_copy.clone();
                                let path = row_context_path.clone();
                                move |_, _, cx| {
                                    let path = path.clone();
                                    let _ = weak.update(cx, |this, cx| {
                                        this.copy_path(&path, cx);
                                    });
                                }
                            }))
                            .item(PopupMenuItem::new(tr("explorer.reveal")).on_click({
                                let weak = weak_menu_reveal.clone();
                                let path = row_context_path.clone();
                                move |_, _, cx| {
                                    let path = path.clone();
                                    let _ = weak.update(cx, |this, cx| {
                                        this.reveal_path(&path, cx);
                                    });
                                }
                            }));
                        if context_is_root {
                            menu = menu
                                .separator()
                                .item(PopupMenuItem::new(tr("workspace.add_folder")).on_click({
                                    let weak = weak_menu_add.clone();
                                    move |_, window, cx| {
                                        let _ = weak.update(cx, |this, cx| {
                                            this.open_add_folder_dialog(window, cx);
                                        });
                                    }
                                }))
                                .item(PopupMenuItem::new(tr("workspace.remove_root")).on_click({
                                    let weak = weak_menu_remove.clone();
                                    let root = context_root.clone();
                                    move |_, _, cx| {
                                        let root = root.clone();
                                        let _ = weak.update(cx, |this, cx| {
                                            this.remove_workspace_root(&root, cx);
                                        });
                                    }
                                }));
                        }
                        menu
                    });
                // The file row has no expander; keep its label aligned with
                // directory names by inserting the same leading cell.
                if !is_dir {
                    element = element.child("");
                }
                list.push(element.into_any_element());
            }
            panel = panel.child(
                div()
                    .id("explorer-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(list),
            );
        }
        let _ = window;
        panel.into_any_element()
    }

    /// Run a file-tree command against the current selection.
    fn explorer_command(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.explorer.selected.clone();
        match id {
            "tree.new-file" | "tree.new-folder" => {
                let Some(parent) = self
                    .explorer
                    .target_directory()
                    .or_else(|| self.workspace_roots.first().cloned())
                else {
                    self.set_status(tr("explorer.no_workspace"), cx);
                    return;
                };
                let kind = if id == "tree.new-file" {
                    NamePromptKind::NewFile
                } else {
                    NamePromptKind::NewFolder
                };
                self.start_name_prompt(kind, parent, None, String::new(), window, cx);
            }
            "tree.rename" => {
                let Some(path) = selected else {
                    self.set_status(tr("explorer.no_selection"), cx);
                    return;
                };
                if self
                    .workspace_roots
                    .iter()
                    .any(|root| util::same_path(root, &path))
                {
                    self.set_status(tr("explorer.cannot_rename_root"), cx);
                    return;
                }
                let Some(parent) = path.parent().map(Path::to_path_buf) else {
                    return;
                };
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.start_name_prompt(
                    NamePromptKind::Rename,
                    parent,
                    Some(path),
                    name,
                    window,
                    cx,
                );
            }
            "tree.delete" => {
                let Some(path) = selected else {
                    self.set_status(tr("explorer.no_selection"), cx);
                    return;
                };
                self.request_delete_path(path, cx);
            }
            "tree.refresh" => self.refresh_workspace(cx),
            "tree.copy-path" => {
                let Some(path) = selected else {
                    self.set_status(tr("explorer.no_selection"), cx);
                    return;
                };
                self.copy_path(&path, cx);
            }
            "tree.reveal" => {
                let Some(path) = selected else {
                    self.set_status(tr("explorer.no_selection"), cx);
                    return;
                };
                self.reveal_path(&path, cx);
            }
            _ => {}
        }
    }

    /// Keyboard handling for the focused symbol outline.
    fn outline_key(
        &mut self,
        key: &str,
        modifiers: gpui_kit::Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if modifiers.control || modifiers.secondary() || modifiers.alt || modifiers.shift {
            return false;
        }
        if !self.settings.show_outline
            || !self.outline_focus.is_focused(window)
            || self.palette.is_some()
            || self.name_prompt.is_some()
        {
            return false;
        }
        let rows = self.outline.rows();
        if rows.is_empty() {
            return false;
        }
        let selected_position = self.outline_selected.and_then(|line| {
            rows.iter()
                .position(|row| self.outline.symbols[row.index].line == line)
        });
        match key {
            "up" => {
                let position = selected_position
                    .map(|position| position.saturating_sub(1))
                    .unwrap_or(rows.len() - 1);
                self.outline_selected = Some(self.outline.symbols[rows[position].index].line);
                cx.notify();
                true
            }
            "down" => {
                let position = selected_position
                    .map(|position| (position + 1).min(rows.len() - 1))
                    .unwrap_or(0);
                self.outline_selected = Some(self.outline.symbols[rows[position].index].line);
                cx.notify();
                true
            }
            "left" | "right" => {
                if let Some(position) = selected_position {
                    let row = &rows[position];
                    if row.has_children {
                        let line = self.outline.symbols[row.index].line;
                        let collapsed = self.outline.collapsed.contains(&line);
                        if (key == "left" && !collapsed) || (key == "right" && collapsed) {
                            self.outline.toggle(line);
                            cx.notify();
                        }
                    }
                }
                true
            }
            "enter" | "space" => {
                if let Some(line) = self.outline_selected {
                    self.jump_to_line_in_tab(self.active, line, window, cx);
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    /// Keyboard handling for the focused file tree. Returns whether the key
    /// was consumed.
    fn explorer_key(
        &mut self,
        key: &str,
        modifiers: gpui_kit::Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.settings.show_file_explorer
            || !self.explorer_focus.is_focused(window)
            || self.palette.is_some()
            || self.confirm.is_some()
            || self.name_prompt.is_some()
        {
            return false;
        }
        let rows = self.explorer.visible_rows(
            self.workspace_index.as_ref(),
            self.settings.show_hidden_files,
        );
        if rows.is_empty() {
            return false;
        }
        let plain =
            !modifiers.control && !modifiers.secondary() && !modifiers.alt && !modifiers.shift;
        if !plain {
            if key == "n"
                && (modifiers.control || modifiers.secondary())
                && !modifiers.shift
                && !modifiers.alt
            {
                self.explorer_command("tree.new-file", window, cx);
                return true;
            }
            return false;
        }
        let current = self.explorer.selected.as_ref().and_then(|selected| {
            rows.iter()
                .position(|row| util::same_path(&row.path, selected))
        });
        match key {
            "up" => {
                let next = current.map(|index| index.saturating_sub(1)).unwrap_or(0);
                self.explorer.select(rows[next].path.clone());
                cx.notify();
                true
            }
            "down" => {
                let next = current
                    .map(|index| (index + 1).min(rows.len() - 1))
                    .unwrap_or(0);
                self.explorer.select(rows[next].path.clone());
                cx.notify();
                true
            }
            "left" => {
                let Some(index) = current else { return false };
                let row = &rows[index];
                if row.is_dir && self.explorer.is_expanded(&row.path) {
                    self.explorer.collapse(&row.path);
                } else if let Some(parent) = row.path.parent() {
                    self.explorer.select(parent.to_path_buf());
                }
                cx.notify();
                true
            }
            "right" => {
                let Some(index) = current else { return false };
                let row = &rows[index];
                if row.is_dir {
                    self.explorer.expand(&row.path);
                }
                cx.notify();
                true
            }
            "enter" => {
                let Some(index) = current else { return false };
                let row = rows[index].clone();
                if row.is_dir {
                    self.explorer
                        .toggle_directory(&row.path, self.settings.show_hidden_files);
                } else {
                    self.open_path(&row.path, window, cx);
                }
                cx.notify();
                true
            }
            "f2" => {
                self.explorer_command("tree.rename", window, cx);
                true
            }
            "delete" => {
                self.explorer_command("tree.delete", window, cx);
                true
            }
            _ => false,
        }
    }

    fn start_name_prompt(
        &mut self,
        kind: NamePromptKind,
        parent: PathBuf,
        original: Option<PathBuf>,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = NamePromptState::new(window, cx, kind, parent, original, initial);
        self._subscriptions
            .push(cx.subscribe(&prompt.input, |this, _, event, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.pending_name_confirm = true;
                    cx.notify();
                }
            }));
        self.name_prompt = Some(prompt);
        cx.notify();
    }

    pub fn confirm_name_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.name_prompt.take() else {
            return;
        };
        let name = prompt.input.read(cx).value().to_string();
        let result = match prompt.kind {
            NamePromptKind::NewFile => file_ops::create_file(&prompt.parent, &name).map(Some),
            NamePromptKind::NewFolder => file_ops::create_dir(&prompt.parent, &name).map(|_| None),
            NamePromptKind::Rename => {
                let Some(original) = &prompt.original else {
                    return;
                };
                let name = file_ops::validate_name(&name)
                    .map_err(|error| luban_core::Error::Other(error.to_string()));
                match name {
                    Ok(name) => {
                        let target = prompt.parent.join(name);
                        file_ops::rename_path(original, &target).map(|_| Some(target))
                    }
                    Err(error) => Err(error),
                }
            }
        };
        match result {
            Ok(created) => {
                if let Some(path) = &created {
                    if let Some(original) = prompt.original.as_deref() {
                        self.rename_open_tabs(original, path);
                        self.explorer.rename_path(original, path);
                    } else {
                        self.explorer.select(path.clone());
                    }
                }
                if prompt.kind == NamePromptKind::NewFolder {
                    self.explorer.expand(&prompt.parent);
                }
                if prompt.kind == NamePromptKind::Rename {
                    self.explorer.expand(&prompt.parent);
                }
                self.refresh_workspace(cx);
                match prompt.kind {
                    NamePromptKind::NewFile => {
                        if let Some(path) = created.clone() {
                            self.open_path(&path, window, cx);
                        }
                        if let Some(path) = created {
                            self.set_status(
                                tr_args(
                                    "explorer.created",
                                    &[("name", path.display().to_string())],
                                ),
                                cx,
                            );
                        }
                    }
                    NamePromptKind::NewFolder => {
                        if let Some(path) = created {
                            self.set_status(
                                tr_args(
                                    "explorer.created",
                                    &[("name", path.display().to_string())],
                                ),
                                cx,
                            );
                        }
                    }
                    NamePromptKind::Rename => {
                        if let Some(path) = created {
                            self.set_status(
                                tr_args(
                                    "explorer.renamed",
                                    &[("name", path.display().to_string())],
                                ),
                                cx,
                            );
                        }
                    }
                }
                cx.notify();
            }
            Err(error) => {
                let message = error.to_string();
                self.name_prompt = Some(NamePromptState {
                    error: Some(message.clone()),
                    ..prompt
                });
                self.set_status(message, cx);
            }
        }
    }

    fn rename_open_tabs(&mut self, from: &Path, to: &Path) {
        let mut changed = Vec::new();
        for tab in &mut self.tabs {
            if let Some(path) = &tab.doc.path {
                if path == from {
                    tab.doc.path = Some(to.to_path_buf());
                    changed.push(to.to_path_buf());
                } else if let Ok(suffix) = path.strip_prefix(from) {
                    let new_path = to.join(suffix);
                    tab.doc.path = Some(new_path.clone());
                    changed.push(new_path);
                }
            }
        }
        if let Some(path) = self.find_in_files.results.iter().find_map(|matched| {
            matched
                .path
                .strip_prefix(from)
                .ok()
                .map(|suffix| to.join(suffix))
        }) {
            let _ = path;
        }
        for path in changed {
            self.settings.add_recent(path);
        }
        self.watch.remap(from, to);
    }

    pub fn request_delete_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self
            .workspace_roots
            .iter()
            .any(|root| util::same_path(root, &path))
        {
            self.set_status(tr("explorer.cannot_delete_root"), cx);
            return;
        }
        let has_open = self.tabs.iter().any(|tab| {
            tab.doc
                .path
                .as_ref()
                .is_some_and(|open| open.starts_with(&path))
        });
        let dirty = self.tabs.iter().any(|tab| {
            tab.doc
                .path
                .as_ref()
                .is_some_and(|open| open.starts_with(&path) && tab.doc.is_modified())
        });
        if dirty {
            self.set_status(tr("explorer.delete_dirty"), cx);
            return;
        }
        let _ = has_open;
        self.confirm = Some(ConfirmState::new(
            CloseIntent::DeletePath {
                path: path.clone(),
                permanent: false,
            },
            tr_args(
                "explorer.delete_title",
                &[("name", path.display().to_string())],
            ),
            tr("explorer.delete_detail"),
        ));
        cx.notify();
    }

    pub fn delete_path_now(
        &mut self,
        path: &Path,
        permanent: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let result = if permanent {
            file_ops::delete_permanently(path)
        } else {
            file_ops::delete_to_trash(path)
        };
        match result {
            Ok(()) => {
                let closing: Vec<u64> = self
                    .tabs
                    .iter()
                    .filter(|tab| {
                        tab.doc
                            .path
                            .as_ref()
                            .is_some_and(|open| open.starts_with(path))
                    })
                    .map(|tab| tab.id)
                    .collect();
                for id in closing {
                    self.close_tab_by_id_without_prompt(id, cx);
                }
                self.explorer.remove_path(path);
                self.refresh_workspace(cx);
                self.set_status(
                    tr_args("explorer.deleted", &[("name", path.display().to_string())]),
                    cx,
                );
                true
            }
            Err(error) => {
                self.set_status(
                    tr_args("explorer.delete_failed", &[("error", error.to_string())]),
                    cx,
                );
                false
            }
        }
    }

    pub fn refresh_workspace(&mut self, cx: &mut Context<Self>) {
        self.explorer.refresh();
        self.rebuild_index(cx);
        self.sync_file_watch(cx);
        cx.notify();
    }

    fn copy_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
        self.set_status(
            tr_args(
                "status.copied_path",
                &[("path", path.display().to_string())],
            ),
            cx,
        );
    }

    fn reveal_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Err(error) = util::reveal_file_command(path).spawn() {
            self.set_status(
                tr_args(
                    "status.open_path_failed",
                    &[("error", format!("{}: {error}", path.display()))],
                ),
                cx,
            );
        }
    }

    // ------------------------------------------------------------------
    // Overlays
    // ------------------------------------------------------------------

    fn render_find_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let query_input = self.find.query.clone();
        let replacement_input = self.find.replacement.clone();
        let error = self.find.error.clone();
        let status = self.find.status.clone();
        let show_replace = self.find.show_replace;
        let weak_prev = weak.clone();
        let weak_next = weak.clone();
        let weak_all_find = weak.clone();
        let weak_mark_all = weak.clone();
        let weak_close = weak.clone();
        let mut bar = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.secondary)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(Input::new(&query_input).w_full().h(px(28.))),
                    )
                    .child(
                        Button::new("find-prev")
                            .label(tr("find.previous"))
                            .ghost()
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_prev
                                    .update(cx, |this, cx| this.find_next(false, window, cx));
                            }),
                    )
                    .child(
                        Button::new("find-next")
                            .label(tr("find.next"))
                            .ghost()
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_next
                                    .update(cx, |this, cx| this.find_next(true, window, cx));
                            }),
                    )
                    .child(
                        Button::new("find-all")
                            .label(tr("find.find_all"))
                            .ghost()
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_all_find.update(cx, |this, cx| {
                                    if this.active_doc().is_some_and(Document::is_huge) {
                                        this.huge_find_all(window, cx);
                                    } else {
                                        let query = this.find.query_text(cx);
                                        if this.ensure_active_text(cx) {
                                            let text = std::mem::take(
                                                &mut this.tabs[this.active].doc.text,
                                            );
                                            this.find.refresh(&text, &query);
                                            this.tabs[this.active].doc.text = text;
                                            this.release_active_text();
                                        }
                                        this.find.dirty = false;
                                        cx.notify();
                                    }
                                });
                            }),
                    )
                    .child(
                        Button::new("find-mark-all")
                            .label(tr("find.mark_all"))
                            .ghost()
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_mark_all.update(cx, |this, cx| {
                                    this.mark_find_all(window, cx);
                                });
                            }),
                    )
                    .child({
                        let weak_history = weak.clone();
                        let query = query_input.clone();
                        let history = self.settings.search_history.clone();
                        Button::new("find-history")
                            .label(tr("find.history"))
                            .ghost()
                            .compact()
                            .dropdown_menu(move |mut menu, window, _cx| {
                                if history.is_empty() {
                                    menu = menu.item(
                                        PopupMenuItem::new(tr("find.no_history")).disabled(true),
                                    );
                                }
                                for entry in &history {
                                    let value = entry.clone();
                                    let query = query.clone();
                                    let weak = weak_history.clone();
                                    menu = menu.item(PopupMenuItem::new(entry.clone()).on_click(
                                        move |_, window, cx| {
                                            let value = value.clone();
                                            let _ = weak.update(cx, |this, cx| {
                                                query.update(cx, |input, cx| {
                                                    input.set_value(value, window, cx)
                                                });
                                                this.find.dirty = true;
                                                cx.notify();
                                            });
                                        },
                                    ));
                                }
                                let weak_clear = weak_history.clone();
                                menu = menu.separator().item(
                                    PopupMenuItem::new(tr("find.clear_history")).on_click(
                                        move |_, _, cx| {
                                            let _ = weak_clear.update(cx, |this, cx| {
                                                this.clear_search_history(cx);
                                            });
                                        },
                                    ),
                                );
                                Self::fit_menu(menu, window)
                            })
                    })
                    .child(
                        Button::new("find-close")
                            .label(tr("common.close"))
                            .ghost()
                            .compact()
                            .on_click(move |_, _, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.find.open = false;
                                    this.cancel_huge_search();
                                    this.find.clear();
                                    cx.notify();
                                });
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Checkbox::new("find-case")
                            .label(tr("find.match_case"))
                            .checked(self.find.match_case)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.find.match_case = *checked;
                                this.find.dirty = true;
                                this.find.scope = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("find-word")
                            .label(tr("find.whole_word"))
                            .checked(self.find.whole_word)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.find.whole_word = *checked;
                                this.find.dirty = true;
                                this.find.scope = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("find-regex")
                            .label(tr("find.regex"))
                            .checked(self.find.regex)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.find.regex = *checked;
                                this.find.dirty = true;
                                this.find.scope = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("find-enhanced-regex")
                            .label(tr("find.enhanced_regex"))
                            .checked(self.find.enhanced_regex)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.find.enhanced_regex = *checked;
                                this.find.dirty = true;
                                this.find.scope = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("find-dotall")
                            .label(tr("find.dot_newline"))
                            .checked(self.find.dot_matches_newline)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.find.dot_matches_newline = *checked;
                                this.find.dirty = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("find-sel")
                            .label(tr("find.in_selection"))
                            .checked(self.find.in_selection)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.find.in_selection = *checked;
                                this.find.scope = if *checked {
                                    let (a, b) = this.active_selection();
                                    (a != b).then_some((a, b))
                                } else {
                                    None
                                };
                                this.find.dirty = true;
                                cx.notify();
                            })),
                    ),
            );
        if show_replace {
            let weak_replace = weak.clone();
            let weak_all = weak.clone();
            bar = bar.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(Input::new(&replacement_input).w_full().h(px(28.))),
                    )
                    .child({
                        let weak_history = weak.clone();
                        let replacement = replacement_input.clone();
                        let history = self.settings.replace_history.clone();
                        Button::new("replace-history")
                            .label(tr("find.history"))
                            .ghost()
                            .compact()
                            .dropdown_menu(move |mut menu, window, _cx| {
                                if history.is_empty() {
                                    menu = menu.item(
                                        PopupMenuItem::new(tr("find.no_history")).disabled(true),
                                    );
                                }
                                for entry in &history {
                                    let value = entry.clone();
                                    let replacement = replacement.clone();
                                    let weak = weak_history.clone();
                                    menu = menu.item(PopupMenuItem::new(entry.clone()).on_click(
                                        move |_, window, cx| {
                                            let value = value.clone();
                                            let _ = weak.update(cx, |this, cx| {
                                                replacement.update(cx, |input, cx| {
                                                    input.set_value(value, window, cx)
                                                });
                                                this.find.dirty = true;
                                                cx.notify();
                                            });
                                        },
                                    ));
                                }
                                let weak_clear = weak_history.clone();
                                menu = menu.separator().item(
                                    PopupMenuItem::new(tr("find.clear_history")).on_click(
                                        move |_, _, cx| {
                                            let _ = weak_clear.update(cx, |this, cx| {
                                                this.clear_search_history(cx);
                                            });
                                        },
                                    ),
                                );
                                Self::fit_menu(menu, window)
                            })
                    })
                    .child(
                        Button::new("replace-one")
                            .label(tr("find.replace"))
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_replace
                                    .update(cx, |this, cx| this.replace_one(window, cx));
                            }),
                    )
                    .child(
                        Button::new("replace-all")
                            .label(tr("find.replace_all"))
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ =
                                    weak_all.update(cx, |this, cx| this.replace_all(window, cx));
                            }),
                    ),
            );
        }
        if !self.find.huge_results.is_empty() {
            let results = self.find.huge_results.clone();
            let mut list = div()
                .id("find-huge-results")
                .max_h(px(150.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_1()
                .text_xs();
            for (index, hit) in results.iter().take(200).enumerate() {
                let hit = *hit;
                let weak_row = weak.clone();
                list = list.child(
                    div()
                        .id(("find-huge-result", index))
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().colors.secondary))
                        .child(tr_args(
                            "find.huge_result",
                            &[("line", hit.line.to_string())],
                        ))
                        .on_click(move |_, window, cx| {
                            let _ = weak_row.update(cx, |this, cx| {
                                this.huge_find_jump(hit, window, cx);
                            });
                        }),
                );
            }
            bar = bar.child(list);
        }
        if let Some(error) = error {
            bar = bar.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.danger)
                    .child(error),
            );
        } else if !status.is_empty() {
            bar = bar.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(status),
            );
        }
        let _ = window;
        bar.into_any_element()
    }

    fn cancel_overwrite(&mut self, cx: &mut Context<Self>) {
        self.pending_export = None;
        if let Some(request) = self.overwrite_save.take() {
            if matches!(
                request.then,
                SaveThen::Exit
                    | SaveThen::CloseAll
                    | SaveThen::NextNormalSave
                    | SaveThen::NextTrimSave
            ) {
                self.abort_pending_saves();
            }
            self.set_status(tr("status.save_cancelled"), cx);
        }
        cx.notify();
    }

    fn confirm_overwrite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request) = self.overwrite_save.take() else {
            return;
        };
        if let Some(format) = self.pending_export.take() {
            self.write_export(&request.path, format, window, cx);
            return;
        }
        self.save_as_request(request, window, cx);
    }

    fn render_replace_preview(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = &self.replace_preview else {
            return div().into_any_element();
        };
        let count = state.preview.count;
        let mut samples = Vec::new();
        for (before, after) in &state.preview.samples {
            samples.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors.border)
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(textutil::preview(before, 160)),
                    )
                    .child(div().text_sm().child(textutil::preview(after, 160)))
                    .into_any_element(),
            );
        }
        let content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("find.preview_title")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr_args(
                        "find.preview_count",
                        &[("count", count.to_string())],
                    )),
            )
            .child(
                div()
                    .id("replace-preview-list")
                    .flex()
                    .flex_col()
                    .max_h(px(260.))
                    .overflow_y_scroll()
                    .children(samples),
            )
            .child(if count == 0 {
                div()
                    .flex()
                    .justify_end()
                    .child(
                        Button::new("replace-preview-cancel")
                            .label(tr("common.close"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.replace_preview = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("replace-preview-cancel")
                            .label(tr("common.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.replace_preview = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("replace-preview-confirm")
                            .label(tr("find.replace_all"))
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_replace_preview(window, cx);
                            })),
                    )
                    .into_any_element()
            })
            .into_any_element();
        self.overlay_card(520.0, None, cx, content)
    }

    fn render_sort_dialog(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.sort_dialog else {
            return div().into_any_element();
        };
        let input = dialog.column.clone();
        let ascending = dialog.ascending;
        let ignore_case = dialog.ignore_case;
        let error = dialog.error.clone();
        let content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("sort.title")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr("sort.column_help")),
            )
            .child(Input::new(&input).h(px(30.)))
            .child(
                Checkbox::new("sort-ascending")
                    .label(tr("sort.ascending"))
                    .checked(ascending)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        if let Some(dialog) = this.sort_dialog.as_mut() {
                            dialog.ascending = *checked;
                        }
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("sort-ignore-case")
                    .label(tr("sort.ignore_case"))
                    .checked(ignore_case)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        if let Some(dialog) = this.sort_dialog.as_mut() {
                            dialog.ignore_case = *checked;
                        }
                        cx.notify();
                    })),
            )
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().colors.danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("sort-cancel")
                            .label(tr("common.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sort_dialog = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("sort-apply")
                            .label(tr("sort.apply"))
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply_sort_dialog(window, cx);
                            })),
                    ),
            )
            .into_any_element();
        self.overlay_card(420.0, None, cx, content)
    }

    fn render_filter_dialog(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.filter_dialog else {
            return div().into_any_element();
        };
        let input = dialog.needle.clone();
        let keep = dialog.keep;
        let match_case = dialog.match_case;
        let regex = dialog.regex;
        let error = dialog.error.clone();
        let content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("filter.title")),
            )
            .child(Input::new(&input).h(px(30.)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Checkbox::new("filter-keep")
                            .label(tr("filter.keep"))
                            .checked(keep)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                if let Some(dialog) = this.filter_dialog.as_mut() {
                                    dialog.keep = *checked;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("filter-case")
                            .label(tr("filter.match_case"))
                            .checked(match_case)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                if let Some(dialog) = this.filter_dialog.as_mut() {
                                    dialog.match_case = *checked;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("filter-regex")
                            .label(tr("filter.regex"))
                            .checked(regex)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                if let Some(dialog) = this.filter_dialog.as_mut() {
                                    dialog.regex = *checked;
                                }
                                cx.notify();
                            })),
                    ),
            )
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().colors.danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("filter-cancel")
                            .label(tr("common.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.filter_dialog = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("filter-apply")
                            .label(tr("filter.apply"))
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply_filter_dialog(window, cx);
                            })),
                    ),
            )
            .into_any_element();
        self.overlay_card(440.0, None, cx, content)
    }

    fn render_column_editor(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if !self.column_editor.open {
            return div().into_any_element();
        }
        let numbers = self.column_editor.numbers;
        let text_input = self.column_editor.text.clone();
        let start_input = self.column_editor.start.clone();
        let step_input = self.column_editor.step.clone();
        let width_input = self.column_editor.width.clone();
        let column_input = self.column_editor.column.clone();
        let error = self.column_editor.error.clone();
        let mut fields = div().flex().flex_col().gap_2();
        fields = fields.child(
            div()
                .flex()
                .gap_2()
                .child(
                    Button::new("column-text-mode")
                        .label(tr("column.text_mode"))
                        .when(!numbers, |button| button.primary())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.column_editor.numbers = false;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("column-number-mode")
                        .label(tr("column.number_mode"))
                        .when(numbers, |button| button.primary())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.column_editor.numbers = true;
                            cx.notify();
                        })),
                ),
        );
        if numbers {
            fields = fields
                .child(div().text_sm().child(tr("column.start")))
                .child(Input::new(&start_input).h(px(30.)))
                .child(div().text_sm().child(tr("column.step")))
                .child(Input::new(&step_input).h(px(30.)))
                .child(div().text_sm().child(tr("column.width")))
                .child(Input::new(&width_input).h(px(30.)));
        } else {
            fields = fields
                .child(div().text_sm().child(tr("column.text")))
                .child(Input::new(&text_input).h(px(30.)));
        }
        fields = fields
            .child(div().text_sm().child(tr("column.column")))
            .child(Input::new(&column_input).h(px(30.)));
        let content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("column.title")),
            )
            .child(fields)
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().colors.danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("column-cancel")
                            .label(tr("common.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.column_editor.open = false;
                                this.column_editor.error = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("column-apply")
                            .label(tr("column.apply"))
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply_column_editor(window, cx);
                            })),
                    ),
            )
            .into_any_element();
        self.overlay_card(440.0, None, cx, content)
    }

    fn render_macro_dialog(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if !self.macro_ui.open {
            return div().into_any_element();
        }
        let selected = self.macro_ui.selected.clone();
        let recorded = self.macro_recorded.len();
        let macros = self.settings.macros.macros.clone();
        let mut rows = Vec::new();
        for saved in macros {
            let name = saved.name.clone();
            let is_selected = selected.as_deref() == Some(name.as_str());
            let shortcut = self
                .settings
                .macros
                .binding(&name)
                .unwrap_or("")
                .to_string();
            let weak = cx.weak_entity();
            rows.push(
                div()
                    .id(format!("macro-row:{name}"))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .when(is_selected, |this| this.bg(cx.theme().colors.accent))
                    .cursor_pointer()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_sm().child(name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().colors.muted_foreground)
                                    .child(format!(
                                        "{} · {}",
                                        saved.commands.len(),
                                        if shortcut.is_empty() {
                                            "—".to_string()
                                        } else {
                                            shortcut
                                        }
                                    )),
                            ),
                    )
                    .on_click(move |_, _, cx| {
                        let name = name.clone();
                        let _ = weak.update(cx, |this, cx| {
                            this.macro_ui.selected = Some(name);
                            cx.notify();
                        });
                    })
                    .into_any_element(),
            );
        }
        let name_input = self.macro_ui.name_input.clone();
        let shortcut_input = self.macro_ui.shortcut_input.clone();
        let error = self.macro_ui.error.clone();
        let content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("macro.title")),
            )
            .child(
                div()
                    .id("macro-list")
                    .flex()
                    .flex_col()
                    .max_h(px(220.))
                    .overflow_y_scrollbar()
                    .children(rows),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("macro-play")
                            .label(tr("macro.play"))
                            .disabled(selected.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                let Some(name) = this.macro_ui.selected.clone() else {
                                    return;
                                };
                                let Some(saved) = this.settings.macros.find(&name).cloned() else {
                                    return;
                                };
                                this.play_macro(&saved, window, cx);
                            })),
                    )
                    .child(
                        Button::new("macro-delete")
                            .label(tr("macro.delete"))
                            .disabled(selected.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                let Some(name) = this.macro_ui.selected.clone() else {
                                    return;
                                };
                                this.delete_macro(&name, cx);
                            })),
                    )
                    .child(
                        Button::new("macro-rename")
                            .label(tr("macro.rename"))
                            .disabled(selected.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rename_macro(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(Input::new(&shortcut_input).h(px(30.)).flex_1())
                    .child(
                        Button::new("macro-assign")
                            .label(tr("macro.assign_shortcut"))
                            .disabled(selected.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.assign_macro_shortcut(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(Input::new(&name_input).h(px(30.)).flex_1())
                    .child(
                        Button::new("macro-save")
                            .label(tr("macro.save_recording"))
                            .primary()
                            .disabled(recorded == 0)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_save_macro(window, cx);
                            })),
                    ),
            )
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().colors.danger)
                        .child(error),
                )
            })
            .child(
                div().flex().justify_end().child(
                    Button::new("macro-close")
                        .label(tr("common.close"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.macro_ui.open = false;
                            cx.notify();
                        })),
                ),
            )
            .into_any_element();
        self.overlay_card(560.0, Some(600.0), cx, content)
    }

    fn render_overwrite_confirm(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(request) = &self.overwrite_save else {
            return div().into_any_element();
        };
        let path = request.path.display().to_string();
        let weak_cancel = cx.weak_entity();
        let weak_overwrite = cx.weak_entity();
        let content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("dialog.overwrite_title")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(path),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("overwrite-cancel")
                            .label(tr("common.cancel"))
                            .on_click(move |_, _, cx| {
                                let _ =
                                    weak_cancel.update(cx, |this, cx| this.cancel_overwrite(cx));
                            }),
                    )
                    .child(
                        Button::new("overwrite-confirm")
                            .label(tr("dialog.overwrite"))
                            .danger()
                            .on_click(move |_, window, cx| {
                                let _ = weak_overwrite
                                    .update(cx, |this, cx| this.confirm_overwrite(window, cx));
                            }),
                    ),
            )
            .into_any_element();
        let _ = window;
        self.overlay_card(520.0, None, cx, content)
    }

    fn cancel_file_dialog(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = self.file_dialog.take() {
            if let FileDialogPurpose::SaveAs {
                then: SaveThen::Exit | SaveThen::CloseAll | SaveThen::NextTrimSave,
                ..
            } = dialog.purpose
            {
                self.abort_pending_saves();
                self.set_status(tr("status.save_cancelled_close"), cx);
            }
        }
        cx.notify();
    }

    fn render_file_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = self.file_dialog.as_ref() else {
            return div().into_any_element();
        };
        let mode = dialog.mode;
        let title = dialog.title();
        let list_title = title.clone();
        let confirm_label = dialog.confirm_label();
        let dir = dialog.dir.clone();
        let filter = dialog.filter;
        let loading = dialog.loading;
        let error = dialog.error.clone();
        let active = dialog.active;
        let selected_count = dialog.selected_count();
        let list_focus = dialog.list_focus.clone();
        let list_scroll = dialog.list_scroll.clone();
        let list_focused = list_focus
            .as_ref()
            .is_some_and(|focus| focus.is_focused(window));
        let list_border = if list_focused {
            cx.theme().colors.ring
        } else {
            cx.theme().colors.border
        };
        let path_input = self.file_path_input.clone();
        let name_input = self.file_name_input.clone();
        let weak = cx.weak_entity();
        let weak_up = weak.clone();
        let weak_refresh = weak.clone();
        let weak_cancel = weak.clone();
        let weak_close = weak.clone();
        let weak_confirm = weak.clone();
        let weak_parent = weak.clone();
        let weak_filter_all = weak.clone();
        let weak_filter_text = weak.clone();

        let mut rows: Vec<AnyElement> = Vec::new();
        let parent_active = active == Some(0);
        rows.push(
            div()
                .id("file-dialog-parent")
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .text_sm()
                .cursor_pointer()
                .when(parent_active, |this| {
                    this.bg(cx.theme().colors.secondary_hover)
                })
                .hover(|this| this.bg(cx.theme().colors.secondary))
                .role(Role::ListBoxOption)
                .aria_label("..")
                .aria_selected(false)
                .when(parent_active, |this| this.aria_active_descendant())
                .child("↰")
                .child("..")
                .on_click(move |event, window, cx| {
                    if event.click_count() > 1 {
                        return;
                    }
                    let _ = weak_parent.update(cx, |this, cx| {
                        this.file_dialog_up(window, cx);
                        this.focus_file_dialog_list(0, window, cx);
                    });
                })
                .into_any_element(),
        );
        {
            let dialog = self.file_dialog.as_ref().expect("dialog checked above");
            for (index, entry) in dialog
                .entries
                .iter()
                .filter(|entry| dialog.entry_is_visible(entry))
                .enumerate()
            {
                let row = index + 1;
                let path = entry.path.clone();
                let name = entry.name.clone();
                let is_dir = entry.is_dir;
                let is_selected = dialog.selected.contains(&path);
                let is_active = active == Some(row);
                if is_dir {
                    let weak_dir = weak.clone();
                    rows.push(
                        div()
                            .id(format!("file-dialog-dir:{}", path.display()))
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_1()
                            .text_sm()
                            .cursor_pointer()
                            .when(is_active, |this| this.bg(cx.theme().colors.secondary_hover))
                            .hover(|this| this.bg(cx.theme().colors.secondary))
                            .role(Role::ListBoxOption)
                            .aria_label(name.clone())
                            .aria_selected(false)
                            .when(is_active, |this| this.aria_active_descendant())
                            .child("▸")
                            .child(name)
                            .on_click(move |event, window, cx| {
                                if event.click_count() > 1 {
                                    return;
                                }
                                let _ = weak_dir.update(cx, |this, cx| {
                                    this.file_dialog_navigate(path.clone(), window, cx);
                                    this.focus_file_dialog_list(0, window, cx);
                                });
                            })
                            .into_any_element(),
                    );
                } else {
                    let weak_file = weak.clone();
                    rows.push(
                        div()
                            .id(format!("file-dialog-file:{}", path.display()))
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_1()
                            .text_sm()
                            .cursor_pointer()
                            .when(is_active, |this| this.bg(cx.theme().colors.secondary_hover))
                            .when(is_selected, |this| this.bg(cx.theme().colors.secondary))
                            .hover(|this| this.bg(cx.theme().colors.secondary_hover))
                            .role(Role::ListBoxOption)
                            .aria_label(name.clone())
                            .aria_selected(is_selected)
                            .when(is_active, |this| this.aria_active_descendant())
                            .child(if is_selected { "✓" } else { " " })
                            .child(name)
                            .on_click(move |event, window, cx| {
                                let modifiers = event.modifiers();
                                let click_count = event.click_count();
                                let _ = weak_file.update(cx, |this, cx| {
                                    if click_count > 1 {
                                        this.file_dialog_double_click(path.clone(), window, cx);
                                    } else {
                                        this.file_dialog_click(
                                            row,
                                            path.clone(),
                                            modifiers,
                                            window,
                                            cx,
                                        );
                                    }
                                });
                            })
                            .into_any_element(),
                    );
                }
            }
        }
        if loading {
            rows.push(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr("dialog.loading"))
                    .into_any_element(),
            );
        }

        let filter_row = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("file-dialog-filter-all")
                    .label(tr("dialog.filter_all"))
                    .compact()
                    .toggled(filter == FileFilter::All)
                    .on_click(move |_, _, cx| {
                        let _ = weak_filter_all.update(cx, |this, cx| {
                            this.file_dialog_set_filter(FileFilter::All, cx);
                        });
                    }),
            )
            .child(
                Button::new("file-dialog-filter-text")
                    .label(tr("dialog.filter_text"))
                    .compact()
                    .toggled(filter == FileFilter::Text)
                    .on_click(move |_, _, cx| {
                        let _ = weak_filter_text.update(cx, |this, cx| {
                            this.file_dialog_set_filter(FileFilter::Text, cx);
                        });
                    }),
            )
            .child(div().flex_1())
            .when(mode == FileDialogMode::Open && selected_count > 0, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(tr_args(
                            "dialog.selected_count",
                            &[("count", selected_count.to_string())],
                        )),
                )
            });

        let scroll_area = div()
            .id("file-dialog-list")
            .size_full()
            .flex()
            .flex_col()
            .pr(px(16.))
            .track_scroll(&list_scroll)
            .when_some(list_focus, |this, focus| this.track_focus(&focus))
            .role(Role::ListBox)
            .aria_label(list_title)
            .overflow_y_scroll()
            .border_1()
            .border_color(list_border)
            .rounded_md()
            .children(rows);
        let list_area = div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .child(scroll_area)
            .child(
                Scrollbar::vertical(&list_scroll)
                    .id("file-dialog-scrollbar")
                    .mode(ScrollbarMode::Always),
            );

        let mut body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .gap_2()
            .p_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        Button::new("file-dialog-close")
                            .label(tr("common.close"))
                            .compact()
                            .on_click(move |_, _, cx| {
                                let _ =
                                    weak_close.update(cx, |this, cx| this.cancel_file_dialog(cx));
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("file-dialog-up")
                            .label(tr("dialog.up"))
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ =
                                    weak_up.update(cx, |this, cx| this.file_dialog_up(window, cx));
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(Input::new(&path_input).w_full().h(px(30.))),
                    )
                    .child(
                        Button::new("file-dialog-refresh")
                            .label(tr("dialog.refresh"))
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_refresh.update(cx, |this, cx| {
                                    let dir = this.file_dialog.as_ref().map(|d| d.dir.clone());
                                    if let Some(dir) = dir {
                                        this.file_dialog_navigate(dir, window, cx);
                                    }
                                });
                            }),
                    ),
            )
            .when(mode != FileDialogMode::Folder, |this| {
                this.child(filter_row)
            })
            .child(list_area);
        if mode == FileDialogMode::Save {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(80.)).text_sm().child(tr("dialog.file_name")))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(Input::new(&name_input).w_full().h(px(30.))),
                    ),
            );
        }
        if mode == FileDialogMode::Folder {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr_args(
                        "dialog.current_folder",
                        &[("path", dir.display().to_string())],
                    )),
            );
        }
        if let Some(error) = error {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.danger)
                    .child(error),
            );
        }
        body = body.child(
            div()
                .flex()
                .justify_end()
                .gap_2()
                .child(
                    Button::new("file-dialog-cancel")
                        .label(tr("common.cancel"))
                        .on_click(move |_, _, cx| {
                            let _ = weak_cancel.update(cx, |this, cx| this.cancel_file_dialog(cx));
                        }),
                )
                .child(
                    Button::new("file-dialog-confirm")
                        .label(confirm_label)
                        .primary()
                        .on_click(move |_, window, cx| {
                            let _ = weak_confirm
                                .update(cx, |this, cx| this.file_dialog_confirm(window, cx));
                        }),
                ),
        );
        let _ = window;
        self.overlay_card(820.0, Some(620.0), cx, body.into_any_element())
    }

    // ------------------------------------------------------------------
    // External change notices and crash recovery
    // ------------------------------------------------------------------

    /// Refresh the watched directories and turn due events into notices.
    fn sync_file_watch(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .tabs
            .iter()
            .filter_map(|tab| tab.doc.path.clone())
            .collect();
        self.watch.sync(&paths);
        let events = self.watch.poll();
        if events.is_empty() {
            return;
        }
        let mut changed = false;
        for event in events {
            for index in 0..self.tabs.len() {
                let matching = self.tabs[index]
                    .doc
                    .path
                    .as_ref()
                    .is_some_and(|path| util::same_path(path, &event.path));
                if !matching {
                    continue;
                }
                // A running save replaces the file itself; its events are not
                // external changes.
                if self.tabs[index]
                    .huge
                    .as_ref()
                    .is_some_and(|ui| ui.is_busy())
                {
                    continue;
                }
                let notice = match event.kind {
                    WatchEventKind::Removed => Some(ExternalNotice::Missing),
                    WatchEventKind::Changed => match self.tabs[index].doc.disk_status() {
                        DiskStatus::Changed => Some(ExternalNotice::Changed),
                        DiskStatus::Missing => Some(ExternalNotice::Missing),
                        DiskStatus::Unchanged | DiskStatus::Untracked => None,
                    },
                };
                if self.tabs[index].external != notice {
                    self.tabs[index].external = notice;
                    changed = true;
                }
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn render_external_banner(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tab = self.active_tab()?;
        let notice = tab.external?;
        let id = tab.id;
        let name = tab.doc.display_name();
        let (message_key, action_key) = match notice {
            ExternalNotice::Changed => ("banner.external_changed", "common.reload"),
            ExternalNotice::Missing => ("banner.external_missing", "menu.file.save_as"),
        };
        let weak_reload = cx.weak_entity();
        let weak_keep = cx.weak_entity();
        let banner = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.secondary)
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .child(tr_args(message_key, &[("name", name)])),
            )
            .child(
                Button::new("external-reload")
                    .label(tr(action_key))
                    .on_click(move |_, window, cx| {
                        let _ = weak_reload.update(cx, |this, cx| {
                            this.external_action(id, window, cx);
                        });
                    }),
            )
            .child(
                Button::new("external-keep")
                    .label(tr("banner.keep"))
                    .on_click(move |_, _, cx| {
                        let _ = weak_keep.update(cx, |this, cx| {
                            if let Some(index) = this.tab_index_by_id(id) {
                                this.tabs[index].external = None;
                            }
                            cx.notify();
                        });
                    }),
            )
            .into_any_element();
        Some(banner)
    }

    /// Reload (for a modification) or open Save As (for a deletion) from the
    /// external-change banner.
    fn external_action(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tab_index_by_id(id) else {
            return;
        };
        self.active = index;
        let missing = self.tabs[index].external == Some(ExternalNotice::Missing);
        if missing {
            self.save_active_as(window, cx);
        } else {
            self.reload_active(window, cx);
        }
    }

    fn advance_recovery_prompt(&mut self) {
        if self.recovery_prompt.is_none() {
            if let Some(info) = self.recovery_queue.pop_front() {
                self.recovery_prompt = Some(RecoveryPrompt {
                    info,
                    problem: None,
                });
            }
        }
    }

    /// Rebuild a disk-engine document from its recovery data on a background
    /// executor so startup stays responsive.
    fn start_recovery_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.recovery_prompt.take() else {
            return;
        };
        if prompt.problem.is_some() {
            self.recovery_prompt = Some(prompt);
            return;
        }
        let info = prompt.info;
        self.recovery_restoring = Some(info.clone());
        self.set_status(tr("status.recovering"), cx);
        let task = cx.background_executor().spawn(async move {
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let progress = std::sync::atomic::AtomicU64::new(0);
            DiskEngine::recover(&info, &cancel, &progress)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_recovery_restore(result, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_recovery_restore(
        &mut self,
        result: luban_core::Result<DiskEngine>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let info = self.recovery_restoring.take();
        match result {
            Ok(engine) => match Document::from_recovered_engine(engine) {
                Ok(doc) => {
                    let path = doc.path.clone();
                    if let Some(index) = self.tabs.iter().position(|tab| {
                        path.as_ref().is_some_and(|path| {
                            tab.doc
                                .path
                                .as_ref()
                                .is_some_and(|open| util::same_path(open, path))
                        })
                    }) {
                        if !self.tabs[index].doc.is_modified() {
                            self.close_tab_at(index, window, cx);
                        }
                    }
                    self.add_document(doc, window, cx);
                    self.set_status(tr("status.recovery_restored"), cx);
                    self.advance_recovery_prompt();
                }
                Err(error) => {
                    self.restore_recovery_problem(info, error.to_string(), cx);
                }
            },
            Err(error) => {
                self.restore_recovery_problem(info, error.to_string(), cx);
            }
        }
        cx.notify();
    }

    fn restore_recovery_problem(
        &mut self,
        info: Option<RecoveryInfo>,
        message: String,
        cx: &mut Context<Self>,
    ) {
        self.set_status(
            tr_args("status.recovery_failed", &[("error", message.clone())]),
            cx,
        );
        if let Some(info) = info {
            self.recovery_prompt = Some(RecoveryPrompt {
                info,
                problem: Some(message),
            });
        }
    }

    /// Discard a recovery entry. When `open_source` is set the current file on
    /// disk is opened afterwards, matching the "Open current file" choice.
    fn discard_recovery_prompt(
        &mut self,
        open_source: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(prompt) = self.recovery_prompt.take() else {
            return;
        };
        let info = prompt.info;
        if let Err(error) = luban_core::disk_engine::discard_recovery_dir(&info.dir) {
            self.recovery_prompt = Some(RecoveryPrompt {
                info,
                problem: Some(error.to_string()),
            });
            self.set_status(
                tr_args(
                    "status.recovery_discard_failed",
                    &[("error", error.to_string())],
                ),
                cx,
            );
            cx.notify();
            return;
        }
        if open_source {
            let path = info.path().to_path_buf();
            if path.exists() {
                let line = info.scroll_line().max(1);
                let range = info.range().map(|[start, end]| (start, end));
                self.start_huge_job(path, line, range, None, false, window, cx);
            } else {
                self.set_status(
                    tr_args(
                        "status.recovery_source_missing",
                        &[("path", info.path().display().to_string())],
                    ),
                    cx,
                );
            }
        }
        self.advance_recovery_prompt();
        cx.notify();
    }

    fn render_recovery_prompt(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(prompt) = &self.recovery_prompt else {
            return div().into_any_element();
        };
        let path = prompt.info.path().display().to_string();
        let problem = prompt.problem.clone();
        let title = tr("recovery.title");
        let detail = match &problem {
            Some(problem) => problem.clone(),
            None => tr_args("recovery.detail", &[("path", path.clone())]),
        };
        let weak_restore = cx.weak_entity();
        let weak_open = cx.weak_entity();
        let weak_discard = cx.weak_entity();
        let weak_later = cx.weak_entity();
        let has_problem = problem.is_some();
        let mut actions = div().flex().justify_end().gap_2().child(
            Button::new("recovery-later")
                .label(tr("recovery.later"))
                .on_click(move |_, _, cx| {
                    let _ = weak_later.update(cx, |this, cx| {
                        this.recovery_prompt = None;
                        this.advance_recovery_prompt();
                        cx.notify();
                    });
                }),
        );
        if has_problem {
            actions = actions
                .child(
                    Button::new("recovery-open-current")
                        .label(tr("recovery.open_current"))
                        .primary()
                        .on_click(move |_, window, cx| {
                            let _ = weak_restore.update(cx, |this, cx| {
                                this.discard_recovery_prompt(true, window, cx);
                            });
                        }),
                )
                .child(
                    Button::new("recovery-discard")
                        .label(tr("recovery.discard"))
                        .danger()
                        .on_click(move |_, window, cx| {
                            let _ = weak_discard.update(cx, |this, cx| {
                                this.discard_recovery_prompt(false, window, cx);
                            });
                        }),
                );
        } else {
            actions = actions
                .child(
                    Button::new("recovery-discard")
                        .label(tr("recovery.discard_open"))
                        .on_click(move |_, window, cx| {
                            let _ = weak_discard.update(cx, |this, cx| {
                                // Discarding a normal recovery offer also opens
                                // the unchanged file on disk.
                                this.discard_recovery_prompt(true, window, cx);
                            });
                        }),
                )
                .child(
                    Button::new("recovery-restore")
                        .label(tr("recovery.restore"))
                        .primary()
                        .on_click(move |_, window, cx| {
                            let _ = weak_open.update(cx, |this, cx| {
                                this.start_recovery_restore(window, cx);
                            });
                        }),
                );
        }
        let content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(detail),
            )
            .child(actions)
            .into_any_element();
        let _ = window;
        self.overlay_card(520.0, None, cx, content)
    }

    fn render_palette(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(overlay) = &self.palette else {
            return div().into_any_element();
        };
        let kind = overlay.kind;
        let state = overlay.state.clone();
        let quick_rows = self.quick.rows.clone();
        let quick_error = self.quick.error.clone();
        let indexing = self.indexing;
        let truncated = self
            .workspace_index
            .as_ref()
            .is_some_and(|index| !index.truncated_roots.is_empty());
        let context = self.command_context();
        let weak_confirm = cx.weak_entity();
        let weak_cancel = cx.weak_entity();
        let weak_query = cx.weak_entity();
        let mut command = Command::new(&state)
            .bordered(false)
            .placeholder(match kind {
                PaletteKind::Commands => tr("command.palette.placeholder"),
                PaletteKind::QuickOpen => tr("command.quick_open.placeholder"),
            })
            .on_confirm(move |index, window, cx| {
                let _ = weak_confirm.update(cx, |this, cx| {
                    this.confirm_palette(kind, index, window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                let _ = weak_cancel.update(cx, |this, cx| this.close_palette(window, cx));
            });
        match kind {
            PaletteKind::Commands => {
                for (category, specs) in palette::command_groups() {
                    let category_label = tr(category.label_key());
                    let items = specs.iter().map(|spec| {
                        let mut item = CommandItem::new()
                            .label(tr(spec.label_key))
                            .keywords([spec.id, category_label.as_str()]);
                        if !spec.enabled(&context) {
                            item = item.disabled(true);
                        }
                        item
                    });
                    command = command.group(
                        CommandGroup::new()
                            .label(tr(category.label_key()))
                            .items(items),
                    );
                }
            }
            PaletteKind::QuickOpen => {
                command = command
                    .filterable(false)
                    .on_query(move |query, _, cx| {
                        let query = query.to_string();
                        let _ = weak_query.update(cx, |this, cx| {
                            this.set_quick_query(&query, cx);
                        });
                    })
                    .footer(move |_, _, cx| {
                        let hint = if indexing {
                            tr("command.quick_open.indexing")
                        } else if truncated {
                            tr("command.quick_open.truncated")
                        } else {
                            tr("command.quick_open.hint")
                        };
                        div()
                            .px_3()
                            .py_2()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(hint)
                            .into_any_element()
                    })
                    .empty(move |_, _, cx| {
                        let message = quick_error
                            .clone()
                            .unwrap_or_else(|| tr("command.quick_open.no_matches"));
                        div()
                            .px_3()
                            .py_4()
                            .text_sm()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(message)
                            .into_any_element()
                    });
                if !quick_rows.is_empty() {
                    let items = quick_rows.into_iter().map(|row| {
                        let label = row.label.clone();
                        let detail = row.detail.clone();
                        CommandItem::new()
                            .label(label.clone())
                            .keywords([detail.clone()])
                            .child(move |_, cx| {
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_3()
                                    .w_full()
                                    .child(div().truncate().text_sm().child(label.clone()))
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_xs()
                                            .text_color(cx.theme().colors.muted_foreground)
                                            .child(detail.clone()),
                                    )
                                    .into_any_element()
                            })
                    });
                    command = command.group(CommandGroup::new().items(items));
                }
            }
        }
        self.overlay_card(640.0, Some(440.0), cx, command.into_any_element())
    }

    fn render_history_dialog(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(dialog) = &self.history_dialog else {
            return div().into_any_element();
        };
        let name = dialog
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| dialog.path.display().to_string());
        let entries = dialog.entries.clone();
        let empty = entries.is_empty();
        let error = dialog.error.clone();
        let has_error = error.is_some();
        let weak_clear = cx.weak_entity();
        let weak_close = cx.weak_entity();
        let unit = self.settings.large_file_display_unit;
        let rows = entries.into_iter().map(|entry| {
            let weak_restore = cx.weak_entity();
            let time = logging::format_timestamp(entry.taken_at);
            let size = format_bytes(entry.size, unit);
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(cx.theme().colors.border)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(div().text_sm().child(time))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().colors.muted_foreground)
                                .child(size),
                        ),
                )
                .child(
                    Button::new(format!("history-restore:{}", entry.path.display()))
                        .label(tr("history.restore"))
                        .compact()
                        .on_click(move |_, window, cx| {
                            let entry = entry.clone();
                            let _ = weak_restore.update(cx, |this, cx| {
                                this.restore_history_entry(entry, window, cx);
                            });
                        }),
                )
                .into_any_element()
        });
        let content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr_args("history.title", &[("name", name)])),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("history-clear")
                                    .label(tr("history.clear"))
                                    .compact()
                                    .on_click(move |_, _, cx| {
                                        let _ = weak_clear
                                            .update(cx, |this, cx| this.clear_local_history(cx));
                                    }),
                            )
                            .child(
                                Button::new("history-close")
                                    .label(tr("common.close"))
                                    .compact()
                                    .on_click(move |_, _, cx| {
                                        let _ = weak_close.update(cx, |this, cx| {
                                            this.history_dialog = None;
                                            cx.notify();
                                        });
                                    }),
                            ),
                    ),
            )
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().colors.danger)
                        .child(error),
                )
            })
            .when(empty && !has_error, |this| {
                this.child(
                    div()
                        .p_4()
                        .text_sm()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(tr("history.empty")),
                )
            })
            .child(
                div()
                    .id("history-entries")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .into_any_element();
        self.overlay_card(560.0, Some(440.0), cx, content)
    }

    fn render_shortcut_editor(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(editor) = &self.shortcut_editor else {
            return div().into_any_element();
        };
        let waiting = editor.waiting_for.clone();
        let error = editor.error.clone();
        let conflicts = commands::conflicts(&self.settings.keybindings);
        let overrides = self.settings.keybindings.clone();
        let keymap = &self.keymap;
        let rows: Vec<_> = commands::COMMANDS
            .iter()
            .map(|spec| {
                (
                    spec.id,
                    tr(spec.label_key),
                    keymap.binding_text(spec.id).unwrap_or("—").to_string(),
                    overrides.get(spec.id).cloned(),
                )
            })
            .collect();
        let weak_all = cx.weak_entity();
        let weak_close = cx.weak_entity();
        let mut list = Vec::new();
        for (id, label, binding, overridden) in rows {
            let weak_edit = cx.weak_entity();
            let weak_reset = cx.weak_entity();
            let is_waiting = waiting.as_deref() == Some(id);
            let mut edit = Button::new(format!("shortcut-edit:{id}"))
                .label(if is_waiting {
                    tr("shortcuts.press_key")
                } else {
                    tr("shortcuts.change")
                })
                .compact();
            if is_waiting {
                edit = edit.primary();
            }
            list.push(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors.border)
                    .child(div().flex_1().text_sm().child(label))
                    .child(
                        div()
                            .w(px(110.))
                            .text_sm()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(binding),
                    )
                    .child(edit.on_click(move |_, _, cx| {
                        let _ = weak_edit.update(cx, |this, cx| {
                            if let Some(editor) = this.shortcut_editor.as_mut() {
                                editor.error = None;
                                editor.waiting_for = Some(id.to_string());
                            }
                            cx.notify();
                        });
                    }))
                    .when(overridden.is_some(), |this| {
                        this.child(
                            Button::new(format!("shortcut-reset:{id}"))
                                .label(tr("shortcuts.reset"))
                                .compact()
                                .on_click(move |_, window, cx| {
                                    let _ = weak_reset.update(cx, |this, cx| {
                                        this.reset_shortcut(id, window, cx);
                                    });
                                }),
                        )
                    })
                    .into_any_element(),
            );
        }
        let content = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("shortcuts.edit_title")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("shortcuts-reset-all")
                                    .label(tr("shortcuts.reset_all"))
                                    .compact()
                                    .on_click(move |_, window, cx| {
                                        let _ = weak_all.update(cx, |this, cx| {
                                            this.reset_all_shortcuts(window, cx);
                                        });
                                    }),
                            )
                            .child(
                                Button::new("shortcuts-editor-close")
                                    .label(tr("common.close"))
                                    .compact()
                                    .on_click(move |_, _, cx| {
                                        let _ = weak_close.update(cx, |this, cx| {
                                            this.shortcut_editor = None;
                                            cx.notify();
                                        });
                                    }),
                            ),
                    ),
            )
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().colors.danger)
                        .child(error),
                )
            })
            .when(!conflicts.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().colors.danger)
                        .child(tr_args(
                            "shortcuts.conflicts",
                            &[("count", conflicts.len().to_string())],
                        )),
                )
            })
            .child(
                div()
                    .id("shortcut-editor-rows")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(list),
            )
            .into_any_element();
        self.overlay_card(680.0, Some(560.0), cx, content)
    }

    fn render_overlays(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut overlays = Vec::new();
        if self.find_in_files.open {
            overlays.push(self.render_find_in_files(window, cx));
        }
        if self.preferences.open {
            overlays.push(self.render_preferences(window, cx));
        }
        if self.preferences.font_picker.is_some() {
            overlays.push(self.render_font_picker(window, cx));
        }
        if self.palette.is_some() {
            overlays.push(self.render_palette(window, cx));
        }
        if self.history_dialog.is_some() {
            overlays.push(self.render_history_dialog(window, cx));
        }
        if self.shortcut_editor.is_some() {
            overlays.push(self.render_shortcut_editor(window, cx));
        }
        if self.confirm.is_some() {
            overlays.push(self.render_confirm(window, cx));
        }
        if self.recovery_prompt.is_some() {
            overlays.push(self.render_recovery_prompt(window, cx));
        }
        if self.go_to_line.is_some() {
            overlays.push(self.render_go_to_line(window, cx));
        }
        if self.name_prompt.is_some() {
            overlays.push(self.render_name_prompt(window, cx));
        }
        if self.language_picker.is_some() {
            overlays.push(self.render_language_picker(window, cx));
        }
        if self.about_open {
            overlays.push(self.render_about(window, cx));
        }
        if self.shortcuts_open {
            overlays.push(self.render_shortcuts(window, cx));
        }
        if self.file_dialog.is_some() {
            overlays.push(self.render_file_dialog(window, cx));
        }
        if self.overwrite_save.is_some() {
            overlays.push(self.render_overwrite_confirm(window, cx));
        }
        if self.replace_preview.is_some() {
            overlays.push(self.render_replace_preview(cx));
        }
        if self.sort_dialog.is_some() {
            overlays.push(self.render_sort_dialog(cx));
        }
        if self.filter_dialog.is_some() {
            overlays.push(self.render_filter_dialog(cx));
        }
        if self.column_editor.open {
            overlays.push(self.render_column_editor(cx));
        }
        if self.macro_ui.open {
            overlays.push(self.render_macro_dialog(cx));
        }
        overlays
    }

    fn overlay_card(
        &self,
        width: f32,
        height: Option<f32>,
        cx: &gpui_kit::App,
        content: AnyElement,
    ) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000066))
            .child(
                div()
                    .w(px(width))
                    .when_some(height, |this, height| this.h(px(height)))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().colors.border)
                    .bg(cx.theme().colors.background)
                    .shadow_lg()
                    .child(content),
            )
            .into_any_element()
    }

    fn render_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(confirm) = &self.confirm else {
            return div().into_any_element();
        };
        let title = confirm.title.clone();
        let detail = confirm.detail.clone();
        let exit = matches!(confirm.intent, CloseIntent::Exit);
        let (save_key, discard_key) = match confirm.intent {
            CloseIntent::OverwriteExternal(_) | CloseIntent::SaveConflict(_) => {
                ("confirm.overwrite_yes", "common.reload")
            }
            CloseIntent::ReloadTab(_) => ("confirm.reload_save", "confirm.reload_discard"),
            CloseIntent::RestoreHistory => ("common.restore", "common.cancel"),
            CloseIntent::TrimTabs => ("confirm.save_close", "confirm.discard"),
            CloseIntent::DeletePath {
                permanent: false, ..
            } => ("explorer.move_to_trash", "explorer.delete_permanently"),
            CloseIntent::DeletePath {
                permanent: true, ..
            } => ("explorer.delete_permanently", "common.cancel"),
            CloseIntent::ImportSettings => ("settings.import_confirm", "settings.keep"),
            CloseIntent::ResetSettings => ("settings.reset_confirm", "settings.keep"),
            CloseIntent::WorkspaceReplace => ("replace.confirm_button", "replace.keep"),
            _ if exit => ("confirm.save_exit", "confirm.discard_exit"),
            _ => ("confirm.save_close", "confirm.discard"),
        };
        let discard_is_danger = !matches!(
            confirm.intent,
            CloseIntent::OverwriteExternal(_)
                | CloseIntent::SaveConflict(_)
                | CloseIntent::RestoreHistory
                | CloseIntent::ImportSettings
                | CloseIntent::ResetSettings
                | CloseIntent::WorkspaceReplace
                | CloseIntent::DeletePath {
                    permanent: false,
                    ..
                }
        );
        let primary_is_danger = matches!(
            confirm.intent,
            CloseIntent::DeletePath {
                permanent: true,
                ..
            }
        );
        let weak_save = cx.weak_entity();
        let weak_discard = cx.weak_entity();
        let weak_cancel = cx.weak_entity();
        let content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(detail),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("confirm-cancel")
                            .label(tr("common.cancel"))
                            .on_click(move |_, _, cx| {
                                let _ = weak_cancel.update(cx, |this, cx| this.confirm_cancel(cx));
                            }),
                    )
                    .child({
                        let button = Button::new("confirm-discard")
                            .label(tr(discard_key))
                            .on_click(move |_, window, cx| {
                                let _ = weak_discard
                                    .update(cx, |this, cx| this.confirm_discard(window, cx));
                            });
                        if discard_is_danger {
                            button.danger()
                        } else {
                            button
                        }
                    })
                    .child({
                        let button = Button::new("confirm-save").label(tr(save_key)).on_click(
                            move |_, window, cx| {
                                let _ =
                                    weak_save.update(cx, |this, cx| this.confirm_save(window, cx));
                            },
                        );
                        if primary_is_danger {
                            button.danger()
                        } else {
                            button.primary()
                        }
                    }),
            )
            .into_any_element();
        let _ = window;
        self.overlay_card(460.0, None, cx, content)
    }

    fn render_name_prompt(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(prompt) = &self.name_prompt else {
            return div().into_any_element();
        };
        let input = prompt.input.clone();
        let title = tr(prompt.kind.title_key());
        let action = tr(prompt.kind.action_key());
        let error = prompt.error.clone();
        let weak_confirm = cx.weak_entity();
        let weak_cancel = cx.weak_entity();
        let content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(Input::new(&input).h(px(30.)))
            .when(error.is_some(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors.danger)
                        .child(error.clone().unwrap_or_default()),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("name-prompt-cancel")
                            .label(tr("common.cancel"))
                            .on_click(move |_, _, cx| {
                                let _ = weak_cancel.update(cx, |this, cx| {
                                    this.name_prompt = None;
                                    this.focus_editor = true;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        Button::new("name-prompt-confirm")
                            .label(action)
                            .primary()
                            .on_click(move |_, window, cx| {
                                let _ = weak_confirm.update(cx, |this, cx| {
                                    this.confirm_name_prompt(window, cx);
                                });
                            }),
                    ),
            )
            .into_any_element();
        self.overlay_card(480.0, None, cx, content)
    }

    fn render_go_to_line(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = &self.go_to_line else {
            return div().into_any_element();
        };
        let input = state.input.clone();
        let weak_go = cx.weak_entity();
        let weak_cancel = cx.weak_entity();
        let content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("dialog.go_to_line")),
            )
            .child(Input::new(&input).h(px(30.)))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("goto-cancel")
                            .label(tr("common.cancel"))
                            .on_click(move |_, _, cx| {
                                let _ = weak_cancel.update(cx, |this, cx| {
                                    this.go_to_line = None;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        Button::new("goto-go")
                            .label(tr("dialog.go"))
                            .primary()
                            .on_click(move |_, window, cx| {
                                let _ = weak_go
                                    .update(cx, |this, cx| this.apply_go_to_line(window, cx));
                            }),
                    ),
            )
            .into_any_element();
        let _ = window;
        self.overlay_card(360.0, None, cx, content)
    }

    fn render_language_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(picker) = &self.language_picker else {
            return div().into_any_element();
        };
        let filter = picker.filter.clone();
        let filter_text = filter.read(cx).value().to_string().to_lowercase();
        let weak = cx.weak_entity();
        let current = self
            .active_doc()
            .map(|d| d.language.clone())
            .unwrap_or_default();
        let mut list = Vec::new();
        for name in language::names() {
            let display = language_display_name(name);
            if !filter_text.is_empty()
                && !name.to_lowercase().contains(&filter_text)
                && !display.to_lowercase().contains(&filter_text)
            {
                continue;
            }
            let selected = name == current;
            let name_owned = name.to_string();
            let weak_item = weak.clone();
            list.push(
                div()
                    .id(format!("lang:{name_owned}"))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(cx.theme().colors.secondary))
                    .hover(|this| this.bg(cx.theme().colors.secondary_hover))
                    .child(if selected { "•" } else { " " })
                    .child(display)
                    .on_click(move |_, window, cx| {
                        let _ = weak_item.update(cx, |this, cx| {
                            this.set_language(name_owned.clone(), window, cx);
                            this.language_picker = None;
                            cx.notify();
                        });
                    })
                    .into_any_element(),
            );
        }
        let weak_close = cx.weak_entity();
        let content = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("dialog.language")),
                    )
                    .child(
                        Button::new("lang-close")
                            .label(tr("common.close"))
                            .compact()
                            .on_click(move |_, _, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.language_picker = None;
                                    cx.notify();
                                });
                            }),
                    ),
            )
            .child(Input::new(&filter).h(px(30.)))
            .child(
                div()
                    .id("language-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(list),
            )
            .into_any_element();
        let _ = window;
        self.overlay_card(420.0, Some(520.0), cx, content)
    }

    fn render_find_in_files(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let query = self.find_in_files.query.clone();
        let folder = self.find_in_files.folder.clone();
        let masks = self.find_in_files.masks.clone();
        let include = self.find_in_files.include.clone();
        let exclude = self.find_in_files.exclude.clone();
        let replacement = self.find_in_files.replacement.clone();
        let show_replace = self.find_in_files.show_replace;
        let workspace = self.find_in_files.workspace;
        let has_workspace = !self.workspace_roots.is_empty();
        let respect_gitignore = self.find_in_files.respect_gitignore;
        let status = self.find_in_files.status.clone();
        let error = self.find_in_files.error.clone();
        let running = self.find_in_files.running;
        let total_groups = self.find_in_files.groups.len();
        let groups: Vec<crate::find::FileGroup> = self
            .find_in_files
            .groups
            .iter()
            .take(200)
            .cloned()
            .collect();
        let collapsed = self.find_in_files.collapsed.clone();
        let count = self.find_in_files.results.len();
        let weak_search = weak.clone();
        let weak_cancel = weak.clone();
        let weak_close = weak.clone();
        let weak_settings_filter = weak.clone();
        let weak_toggle_group = weak.clone();
        let weak_result = weak.clone();
        let weak_preview = weak.clone();
        let weak_replace = weak.clone();
        let weak_undo = weak.clone();
        let has_undo = self.replace_state.undo.is_some();
        let replacing = self.replace_state.running;
        self.invalidate_stale_preview(cx);
        let replace_preview = self.replace_state.preview.clone();
        let mut result_rows = Vec::new();
        for (group_index, group) in groups.iter().enumerate() {
            let is_collapsed = collapsed.contains(&group.path);
            let weak_group = weak_toggle_group.clone();
            let group_path = group.path.clone();
            let group_relative = group.relative.clone();
            let group_count = group.matches.len();
            result_rows.push(
                div()
                    .id(format!("ff-group:{group_index}"))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors.border)
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().colors.secondary))
                    .child(if is_collapsed { "▸" } else { "▾" })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(group_relative),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(group_count.to_string()),
                    )
                    .on_click(move |_, _, cx| {
                        let path = group_path.clone();
                        let _ = weak_group.update(cx, |this, cx| {
                            this.find_in_files.toggle_group(&path);
                            cx.notify();
                        });
                    })
                    .into_any_element(),
            );
            if is_collapsed {
                continue;
            }
            for (match_index, matched) in group.matches.iter().take(200).enumerate() {
                let matched = matched.clone();
                let weak_row = weak_result.clone();
                result_rows.push(
                    div()
                        .id(format!("ff-result:{group_index}:{match_index}"))
                        .flex()
                        .flex_col()
                        .gap_0()
                        .pl(px(28.))
                        .pr_3()
                        .py_1()
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().colors.secondary))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_xs()
                                .text_color(cx.theme().colors.muted_foreground)
                                .child(format!("{}:{}", matched.line, matched.column)),
                        )
                        .child(
                            div()
                                .text_sm()
                                .child(textutil::preview(&matched.line_text, 300)),
                        )
                        .on_click(move |_, window, cx| {
                            let _ = weak_row.update(cx, |this, cx| {
                                this.open_file_match(matched.clone(), window, cx);
                            });
                        })
                        .into_any_element(),
                );
            }
            if group.matches.len() > 200 {
                result_rows.push(
                    div()
                        .pl(px(28.))
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(tr_args(
                            "ff.more_matches",
                            &[("count", (group.matches.len() - 200).to_string())],
                        ))
                        .into_any_element(),
                );
            }
        }
        if total_groups > groups.len() {
            result_rows.push(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr_args(
                        "ff.more_files",
                        &[("count", (total_groups - groups.len()).to_string())],
                    ))
                    .into_any_element(),
            );
        }
        let mut options = div()
            .flex()
            .items_center()
            .gap_3()
            .flex_wrap()
            .child(
                Checkbox::new("ff-workspace")
                    .label(tr("ff.workspace"))
                    .checked(workspace && has_workspace)
                    .disabled(!has_workspace)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.find_in_files.workspace = *checked;
                        if *checked {
                            this.find_in_files.open_documents = false;
                        }
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("ff-gitignore")
                    .label(tr("ff.respect_gitignore"))
                    .checked(respect_gitignore)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.find_in_files.respect_gitignore = *checked;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("ff-use-settings")
                    .label(tr("ff.use_workspace_filter"))
                    .compact()
                    .on_click(move |_, window, cx| {
                        let _ = weak_settings_filter.update(cx, |this, cx| {
                            let include = this.settings.search_include.clone();
                            let exclude = this.settings.search_exclude.clone();
                            this.find_in_files
                                .use_settings_filter(&include, &exclude, window, cx);
                        });
                    }),
            );
        for (id, label_key, checked, field) in [
            (
                "ff-case",
                "ff.match_case",
                self.find_in_files.match_case,
                0u8,
            ),
            ("ff-word", "ff.whole_word", self.find_in_files.whole_word, 1),
            ("ff-regex", "ff.regex", self.find_in_files.regex, 2),
            (
                "ff-enhanced-regex",
                "ff.enhanced_regex",
                self.find_in_files.enhanced_regex,
                3,
            ),
            (
                "ff-open-docs",
                "ff.open_documents",
                self.find_in_files.open_documents,
                4,
            ),
        ] {
            options = options.child(
                Checkbox::new(id)
                    .label(tr(label_key))
                    .checked(checked)
                    .on_click(cx.listener(move |this, checked, _, cx| {
                        match field {
                            0 => this.find_in_files.match_case = *checked,
                            1 => this.find_in_files.whole_word = *checked,
                            2 => this.find_in_files.regex = *checked,
                            3 => this.find_in_files.enhanced_regex = *checked,
                            _ => {
                                this.find_in_files.open_documents = *checked;
                                if *checked {
                                    this.find_in_files.workspace = false;
                                }
                            }
                        }
                        cx.notify();
                    })),
            );
        }
        options = options
            .child(
                Button::new("ff-search")
                    .label(tr(if running { "ff.searching" } else { "ff.search" }))
                    .primary()
                    .compact()
                    .on_click(move |_, _, cx| {
                        let _ = weak_search.update(cx, |this, cx| this.start_find_in_files(cx));
                    }),
            )
            .child(
                Button::new("ff-cancel")
                    .label(tr("common.cancel"))
                    .compact()
                    .on_click(move |_, _, cx| {
                        let _ = weak_cancel.update(cx, |this, cx| this.cancel_find_in_files(cx));
                    }),
            );
        let mut replace_row = div().flex().items_center().gap_2();
        replace_row = replace_row
            .child(Input::new(&replacement).h(px(30.)))
            .child(
                Button::new("ff-toggle-replace")
                    .label(tr(if show_replace {
                        "ff.hide_replace"
                    } else {
                        "ff.show_replace"
                    }))
                    .compact()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.find_in_files.show_replace = !this.find_in_files.show_replace;
                        cx.notify();
                    })),
            );
        if show_replace {
            replace_row = replace_row
                .child(
                    Button::new("ff-preview")
                        .label(tr("ff.preview_replace"))
                        .compact()
                        .on_click(move |_, window, cx| {
                            let _ = weak_preview.update(cx, |this, cx| {
                                this.start_workspace_replace_preview(window, cx);
                            });
                        }),
                )
                .child(
                    Button::new("ff-replace-all")
                        .label(tr("ff.replace_all"))
                        .compact()
                        .on_click(move |_, window, cx| {
                            let _ = weak_replace.update(cx, |this, cx| {
                                this.confirm_workspace_replace(window, cx);
                            });
                        }),
                )
                .child(
                    Button::new("ff-undo-replace")
                        .label(tr("ff.undo_replace"))
                        .compact()
                        .disabled(!has_undo || replacing)
                        .on_click(move |_, window, cx| {
                            let _ = weak_undo.update(cx, |this, cx| {
                                this.undo_workspace_replace(window, cx);
                            });
                        }),
                );
        }
        let content = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("ff.title")),
                    )
                    .child(
                        Button::new("ff-close")
                            .label(tr("common.close"))
                            .compact()
                            .on_click(move |_, _, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.cancel_find_in_files(cx);
                                    this.find_in_files.open = false;
                                    cx.notify();
                                });
                            }),
                    ),
            )
            .child(Input::new(&query).h(px(30.)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&include).h(px(30.))))
                    .child(div().flex_1().child(Input::new(&exclude).h(px(30.)))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&folder).h(px(30.))))
                    .child(
                        Button::new("ff-browse")
                            .label(tr("ff.browse"))
                            .compact()
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.choose_find_in_files_folder(window, cx);
                            })),
                    ),
            )
            .child(Input::new(&masks).h(px(30.)))
            .child(options)
            .child(replace_row)
            .when(error.is_some(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors.danger)
                        .child(error.clone().unwrap_or_default()),
                )
            })
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(if status.is_empty() {
                        if count == 1 {
                            tr("ff.results.one")
                        } else {
                            tr_args("ff.results.other", &[("count", count.to_string())])
                        }
                    } else {
                        status
                    }),
            )
            .when_some(replace_preview, |this, preview| {
                let mut rows = Vec::new();
                for file in preview.files.iter().take(40) {
                    rows.push(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0()
                            .text_xs()
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(format!("{}  ({} →)", file.relative, file.matches)),
                            )
                            .children(file.samples.iter().map(|(before, after)| {
                                div()
                                    .pl(px(12.))
                                    .text_color(cx.theme().colors.muted_foreground)
                                    .child(format!("{before} → {after}"))
                            }))
                            .into_any_element(),
                    );
                }
                for skipped in preview.skipped.iter().take(20) {
                    rows.push(
                        div()
                            .text_xs()
                            .text_color(cx.theme().colors.warning)
                            .child(format!("{}: {}", skipped.path.display(), skipped.reason))
                            .into_any_element(),
                    );
                }
                this.child(
                    div()
                        .id("replace-preview")
                        .flex()
                        .flex_col()
                        .gap_1()
                        .max_h(px(180.))
                        .overflow_y_scroll()
                        .border_1()
                        .border_color(cx.theme().colors.border)
                        .rounded_sm()
                        .p_2()
                        .child(tr_args(
                            "replace.preview_summary",
                            &[
                                ("files", preview.file_count().to_string()),
                                ("matches", preview.total_matches.to_string()),
                                ("skipped", preview.skipped.len().to_string()),
                            ],
                        ))
                        .children(rows),
                )
            })
            .child(
                div()
                    .id("ff-results")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(result_rows),
            )
            .into_any_element();
        let _ = window;
        self.overlay_card(860.0, Some(640.0), cx, content)
    }

    fn render_about(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_6()
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .child("Luban Notepad"),
            )
            .child(div().text_sm().child(tr_args(
                "about.version",
                &[("version", env!("CARGO_PKG_VERSION").to_string())],
            )))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr("about.description")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr("about.engine")),
            )
            .child(
                div().flex().justify_end().child(
                    Button::new("about-close")
                        .label(tr("common.close"))
                        .on_click(move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.about_open = false;
                                cx.notify();
                            });
                        }),
                ),
            )
            .into_any_element();
        let _ = window;
        self.overlay_card(560.0, None, cx, content)
    }

    fn render_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let weak_edit = cx.weak_entity();
        let groups = palette::command_groups();
        let sections = groups.into_iter().map(|(category, specs)| {
            let rows: Vec<_> = specs
                .iter()
                .filter_map(|spec| {
                    let binding = self.keymap.binding_text(spec.id)?;
                    Some((tr(spec.label_key), binding.to_string()))
                })
                .collect();
            (tr(category.label_key()), rows)
        });
        let content = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("shortcuts.title")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("shortcuts-edit")
                                    .label(tr("shortcuts.customize"))
                                    .compact()
                                    .on_click(move |_, _, cx| {
                                        let _ = weak_edit.update(cx, |this, cx| {
                                            this.open_shortcut_editor(cx);
                                        });
                                    }),
                            )
                            .child(
                                Button::new("shortcuts-close")
                                    .label(tr("common.close"))
                                    .compact()
                                    .on_click(move |_, _, cx| {
                                        let _ = weak.update(cx, |this, cx| {
                                            this.shortcuts_open = false;
                                            this.focus_editor = true;
                                            cx.notify();
                                        });
                                    }),
                            ),
                    ),
            )
            .child(
                div()
                    .id("shortcut-help")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(sections.filter(|(_, rows)| !rows.is_empty()).map(
                        |(category, rows)| {
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(cx.theme().colors.muted_foreground)
                                        .child(category),
                                )
                                .children(rows.into_iter().map(|(label, shortcut)| {
                                    div()
                                        .flex()
                                        .justify_between()
                                        .gap_4()
                                        .text_sm()
                                        .child(label)
                                        .child(
                                            div()
                                                .text_color(cx.theme().colors.muted_foreground)
                                                .child(shortcut),
                                        )
                                }))
                        },
                    )),
            )
            .into_any_element();
        let _ = window;
        self.overlay_card(520.0, Some(560.0), cx, content)
    }

    fn close_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preferences.open = false;
        self.preferences.font_picker = None;
        let search = self.preferences.search.clone();
        search.update(cx, |input, cx| input.set_value(String::new(), window, cx));
        self.focus_editor(window, cx);
        cx.notify();
    }

    fn open_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preferences.open = true;
        let value = self
            .settings
            .tab_size_for(&self.preferences.language)
            .to_string();
        self.preferences
            .language_tab_size
            .update(cx, |input, cx| input.set_value(value, window, cx));
        cx.notify();
    }

    fn open_font_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = &self.preferences.font_picker {
            picker.filter.update(cx, |filter, cx| {
                filter.set_value(String::new(), window, cx);
                filter.focus(window, cx);
            });
            return;
        }
        if self.preferences.fonts.is_none() {
            self.preferences.fonts = Some(cx.text_system().all_font_names());
        }
        let fonts = self.preferences.fonts.clone().unwrap_or_default();
        let picker = FontPickerState::new(window, cx, fonts);
        picker
            .filter
            .update(cx, |filter, cx| filter.focus(window, cx));
        self.preferences.font_picker = Some(picker);
        cx.notify();
    }

    fn select_font_family(&mut self, family: String, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.font_family = family;
        self.preferences.font_picker = None;
        // The picker owned focus through its filter input; hand it back so
        // Escape and the global shortcuts keep reaching the app.
        self.focus_editor(window, cx);
        self.apply_settings(window, cx);
    }

    fn render_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let settings = self.settings.clone();
        let search_input = self.preferences.search.clone();
        let search_query = search_input.read(cx).value().to_string();
        let visible = settings_search::visible_group_keys(&search_query);
        let no_matches = !settings_search::has_matches(&search_query);
        let weak_theme_light = cx.weak_entity();
        let weak_theme_dark = cx.weak_entity();
        let weak_close = cx.weak_entity();
        let weak_font_picker = cx.weak_entity();
        let weak_zoom_in = cx.weak_entity();
        let weak_zoom_out = cx.weak_entity();
        let weak_tab_in = cx.weak_entity();
        let weak_tab_out = cx.weak_entity();
        let weak_lang_system = cx.weak_entity();
        let weak_lang_en = cx.weak_entity();
        let weak_lang_zh = cx.weak_entity();
        let weak_theme_system = cx.weak_entity();
        let weak_days_down = cx.weak_entity();
        let weak_days_up = cx.weak_entity();
        let weak_mb_down = cx.weak_entity();
        let weak_mb_up = cx.weak_entity();
        let weak_shortcuts = cx.weak_entity();

        let mut language_system = Button::new("prefs-lang-system")
            .label(tr("prefs.language_system"))
            .compact();
        if settings.ui_language == "system" {
            language_system = language_system.primary();
        }
        let language_system = language_system.on_click(move |_, window, cx| {
            let _ = weak_lang_system.update(cx, |this, cx| {
                this.set_ui_language("system", window, cx);
            });
        });

        let mut language_en = Button::new("prefs-lang-en")
            .label(tr("prefs.language_english"))
            .compact();
        if settings.ui_language == "en" {
            language_en = language_en.primary();
        }
        let language_en = language_en.on_click(move |_, window, cx| {
            let _ = weak_lang_en.update(cx, |this, cx| {
                this.set_ui_language("en", window, cx);
            });
        });

        let mut language_zh = Button::new("prefs-lang-zh")
            .label(tr("prefs.language_chinese"))
            .compact();
        if settings.ui_language == "zh-CN" {
            language_zh = language_zh.primary();
        }
        let language_zh = language_zh.on_click(move |_, window, cx| {
            let _ = weak_lang_zh.update(cx, |this, cx| {
                this.set_ui_language("zh-CN", window, cx);
            });
        });

        let toggle_row =
            |label: String,
             value: bool,
             id: &'static str,
             weak: WeakEntity<Self>,
             set: fn(&mut Self, bool, &mut Window, &mut Context<Self>)| {
                Checkbox::new(id).label(label).checked(value).on_click(
                    move |checked, window, cx| {
                        let checked = *checked;
                        let _ = weak.update(cx, |this, cx| set(this, checked, window, cx));
                    },
                )
            };

        let content = div()
            .flex()
            .flex_col()
            .h_full()
            .min_h(px(0.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .p_5()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("prefs.title")),
                    )
                    .child(
                        Button::new("prefs-close")
                            .label(tr("common.close"))
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.close_preferences(window, cx);
                                });
                            }),
                    ),
            )
            .child(
                div().px_5().pb_2().child(
                    Input::new(&search_input)
                        .h(px(30.))
                        .aria_label(tr("prefs.search_placeholder")),
                ),
            )
            .child(
                div()
                    .id("prefs-scroll")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scrollbar()
                    .px_5()
                    .pb_5()
                    .when(no_matches, |this| {
                        this.child(
                            div()
                                .py_4()
                                .text_sm()
                                .text_color(cx.theme().colors.muted_foreground)
                                .child(tr("prefs.no_matches")),
                        )
                    })
                    .when(visible.contains("language"), |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .child(
                                            div().w(px(140.)).text_sm().child(tr("prefs.language")),
                                        )
                                        .child(language_system)
                                        .child(language_en)
                                        .child(language_zh),
                                )
                                .when(settings.ui_language == "system", |this| {
                                    this.child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().colors.muted_foreground)
                                            .child(tr_args(
                                                "prefs.language_current",
                                                &[("language", active_language_name().to_string())],
                                            )),
                                    )
                                }),
                        )
                    })
                    .when(visible.contains("appearance"), |this| {
                        this.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().w(px(140.)).text_sm().child(tr("prefs.theme")))
                                .child(
                                    Button::new("prefs-light")
                                        .label(tr("prefs.light"))
                                        .on_click(move |_, window, cx| {
                                            let _ = weak_theme_light.update(cx, |this, cx| {
                                                this.settings.theme = ThemeChoice::Light;
                                                this.apply_settings(window, cx);
                                            });
                                        }),
                                )
                                .child(Button::new("prefs-dark").label(tr("prefs.dark")).on_click(
                                    move |_, window, cx| {
                                        let _ = weak_theme_dark.update(cx, |this, cx| {
                                            this.settings.theme = ThemeChoice::Dark;
                                            this.apply_settings(window, cx);
                                        });
                                    },
                                ))
                                .child(
                                    Button::new("prefs-system")
                                        .label(tr("prefs.system_theme"))
                                        .on_click(move |_, window, cx| {
                                            let _ = weak_theme_system.update(cx, |this, cx| {
                                                this.settings.theme = ThemeChoice::System;
                                                this.apply_settings(window, cx);
                                            });
                                        }),
                                ),
                        )
                    })
                    .when(visible.contains("appearance"), |this| {
                        this.child({
                            let syntax_label = if settings.syntax_theme == "custom" {
                                settings
                                    .syntax_theme_path
                                    .as_ref()
                                    .and_then(|path| path.file_stem())
                                    .map(|stem| stem.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| tr("prefs.syntax_custom"))
                            } else if settings.syntax_theme == "default"
                                || settings.syntax_theme.is_empty()
                            {
                                tr("prefs.syntax_default")
                            } else {
                                THEME_OPTIONS
                                    .iter()
                                    .find(|option| option.id == settings.syntax_theme)
                                    .map(|option| option.label.to_string())
                                    .unwrap_or_else(|| settings.syntax_theme.clone())
                            };
                            let weak_theme = weak.clone();
                            let weak_import = weak.clone();
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().w(px(140.)).text_sm().child(tr("prefs.syntax_theme")))
                                .child(
                                    Button::new("prefs-syntax-theme")
                                        .label(syntax_label)
                                        .dropdown_menu(move |mut menu, window, _cx| {
                                            let weak_default = weak_theme.clone();
                                            menu = menu.item(
                                                PopupMenuItem::new(tr("prefs.syntax_default"))
                                                    .on_click(move |_, window, cx| {
                                                        let _ =
                                                            weak_default.update(cx, |this, cx| {
                                                                this.set_syntax_theme(
                                                                    "default", window, cx,
                                                                );
                                                            });
                                                    }),
                                            );
                                            for option in THEME_OPTIONS {
                                                let weak = weak_theme.clone();
                                                let id = option.id;
                                                menu = menu.item(
                                                    PopupMenuItem::new(option.label).on_click(
                                                        move |_, window, cx| {
                                                            let _ = weak.update(cx, |this, cx| {
                                                                this.set_syntax_theme(
                                                                    id, window, cx,
                                                                );
                                                            });
                                                        },
                                                    ),
                                                );
                                            }
                                            Self::fit_menu(menu, window)
                                        }),
                                )
                                .child(
                                    Button::new("prefs-import-theme")
                                        .label(tr("prefs.import_theme"))
                                        .compact()
                                        .on_click(move |_, window, cx| {
                                            let _ = weak_import.update(cx, |this, cx| {
                                                this.open_syntax_theme_dialog(window, cx);
                                            });
                                        }),
                                )
                        })
                    })
                    .when(visible.contains("font"), |this| {
                        this.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().w(px(140.)).text_sm().child(tr("prefs.font_size")))
                                .child(
                                    Button::new("prefs-font-minus")
                                        .label("−")
                                        .compact()
                                        .on_click(move |_, _, cx| {
                                            let _ = weak_zoom_out
                                                .update(cx, |this, cx| this.zoom(-1.0, cx));
                                        }),
                                )
                                .child(div().text_sm().child(tr_args(
                                    "prefs.pixels",
                                    &[("size", format!("{:.0}", settings.font_size))],
                                )))
                                .child(
                                    Button::new("prefs-font-plus")
                                        .label("+")
                                        .compact()
                                        .on_click(move |_, _, cx| {
                                            let _ = weak_zoom_in
                                                .update(cx, |this, cx| this.zoom(1.0, cx));
                                        }),
                                ),
                        )
                    })
                    .when(visible.contains("font"), |this| {
                        this.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().w(px(140.)).text_sm().child(tr("prefs.font_family")))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .truncate()
                                        .text_sm()
                                        .text_color(cx.theme().colors.muted_foreground)
                                        .child(if settings.font_family.is_empty() {
                                            tr("prefs.font_default")
                                        } else {
                                            settings.font_family.clone()
                                        }),
                                )
                                .child(
                                    Button::new("prefs-choose-font")
                                        .label(tr("prefs.choose_font"))
                                        .compact()
                                        .on_click(move |_, window, cx| {
                                            let _ = weak_font_picker.update(cx, |this, cx| {
                                                this.open_font_picker(window, cx);
                                            });
                                        }),
                                ),
                        )
                    })
                    .when(visible.contains("editor"), |this| {
                        this.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().w(px(140.)).text_sm().child(tr("prefs.tab_size")))
                                .child(
                                    Button::new("prefs-tab-minus")
                                        .label("−")
                                        .compact()
                                        .on_click(move |_, window, cx| {
                                            let _ = weak_tab_out.update(cx, |this, cx| {
                                                this.settings.tab_size =
                                                    this.settings.tab_size.saturating_sub(1).max(1);
                                                this.apply_settings(window, cx);
                                            });
                                        }),
                                )
                                .child(div().text_sm().child(format!("{}", settings.tab_size)))
                                .child(Button::new("prefs-tab-plus").label("+").compact().on_click(
                                    move |_, window, cx| {
                                        let _ = weak_tab_in.update(cx, |this, cx| {
                                            this.settings.tab_size =
                                                (this.settings.tab_size + 1).min(16);
                                            this.apply_settings(window, cx);
                                        });
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.insert_spaces"),
                                    settings.use_spaces,
                                    "prefs-spaces",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.use_spaces = checked;
                                        this.apply_settings(window, cx);
                                    },
                                )),
                        )
                    })
                    .when(visible.contains("editor"), |this| {
                        this.child({
                            let language = self.preferences.language.clone();
                            let overrides = self.language_override().cloned().unwrap_or_default();
                            let weak_language = weak.clone();
                            let weak_tab_apply = weak.clone();
                            let weak_spaces_inherit = weak.clone();
                            let weak_spaces_on = weak.clone();
                            let weak_spaces_off = weak.clone();
                            let weak_eol_inherit = weak.clone();
                            let weak_eol_lf = weak.clone();
                            let weak_eol_crlf = weak.clone();
                            let weak_eol_cr = weak.clone();
                            let tab_size_input = self.preferences.language_tab_size.clone();
                            let selected_language = language.clone();
                            let weak_language_menu = weak_language.clone();
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.per_language")),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .child(
                                            div()
                                                .w(px(140.))
                                                .text_sm()
                                                .child(tr("prefs.language_select")),
                                        )
                                        .child(
                                            Button::new("prefs-language-select")
                                                .label(language_display_name(&language))
                                                .dropdown_menu(move |mut menu, window, _cx| {
                                                    for name in language::names() {
                                                        let selected =
                                                            selected_language.as_str() == name;
                                                        let name = name.to_string();
                                                        let weak = weak_language_menu.clone();
                                                        menu = menu.item(
                                                            PopupMenuItem::new(
                                                                language_display_name(&name),
                                                            )
                                                            .checked(selected)
                                                            .on_click(move |_, window, cx| {
                                                                let name = name.clone();
                                                                let _ =
                                                                    weak.update(cx, |this, cx| {
                                                                        this.preferences.language =
                                                                            name.clone();
                                                                        let value = this
                                                                            .settings
                                                                            .tab_size_for(&name)
                                                                            .to_string();
                                                                        this.preferences
                                                                            .language_tab_size
                                                                            .update(
                                                                                cx,
                                                                                |input, cx| {
                                                                                    input.set_value(
                                                                                        value,
                                                                                        window, cx,
                                                                                    )
                                                                                },
                                                                            );
                                                                        cx.notify();
                                                                    });
                                                            }),
                                                        );
                                                    }
                                                    Self::fit_menu(menu, window)
                                                }),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(px(140.))
                                                .text_sm()
                                                .child(tr("prefs.language_tab_size")),
                                        )
                                        .child(Input::new(&tab_size_input).w(px(60.)).h(px(28.)))
                                        .child(
                                            Button::new("prefs-language-tab-apply")
                                                .label(tr("prefs.apply"))
                                                .compact()
                                                .on_click(move |_, _, cx| {
                                                    let _ =
                                                        weak_tab_apply.update(cx, |this, cx| {
                                                            this.apply_language_tab_size(cx);
                                                        });
                                                }),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(px(140.))
                                                .text_sm()
                                                .child(tr("prefs.language_indent")),
                                        )
                                        .child(
                                            Button::new("prefs-language-spaces-inherit")
                                                .label(tr("prefs.inherit"))
                                                .compact()
                                                .when(overrides.use_spaces.is_none(), |button| {
                                                    button.primary()
                                                })
                                                .on_click(move |_, _, cx| {
                                                    let _ = weak_spaces_inherit.update(
                                                        cx,
                                                        |this, cx| {
                                                            this.set_language_use_spaces(None, cx);
                                                        },
                                                    );
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-language-spaces-on")
                                                .label(tr("prefs.spaces"))
                                                .compact()
                                                .when(
                                                    overrides.use_spaces == Some(true),
                                                    |button| button.primary(),
                                                )
                                                .on_click(move |_, _, cx| {
                                                    let _ =
                                                        weak_spaces_on.update(cx, |this, cx| {
                                                            this.set_language_use_spaces(
                                                                Some(true),
                                                                cx,
                                                            );
                                                        });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-language-spaces-off")
                                                .label(tr("prefs.tabs"))
                                                .compact()
                                                .when(
                                                    overrides.use_spaces == Some(false),
                                                    |button| button.primary(),
                                                )
                                                .on_click(move |_, _, cx| {
                                                    let _ =
                                                        weak_spaces_off.update(cx, |this, cx| {
                                                            this.set_language_use_spaces(
                                                                Some(false),
                                                                cx,
                                                            );
                                                        });
                                                }),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(px(140.))
                                                .text_sm()
                                                .child(tr("prefs.language_eol")),
                                        )
                                        .child(
                                            Button::new("prefs-language-eol-inherit")
                                                .label(tr("prefs.inherit"))
                                                .compact()
                                                .when(overrides.eol.is_none(), |button| {
                                                    button.primary()
                                                })
                                                .on_click(move |_, _, cx| {
                                                    let _ =
                                                        weak_eol_inherit.update(cx, |this, cx| {
                                                            this.set_language_eol(None, cx);
                                                        });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-language-eol-lf")
                                                .label("LF")
                                                .compact()
                                                .when(overrides.eol == Some(Eol::Lf), |button| {
                                                    button.primary()
                                                })
                                                .on_click(move |_, _, cx| {
                                                    let _ = weak_eol_lf.update(cx, |this, cx| {
                                                        this.set_language_eol(Some(Eol::Lf), cx);
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-language-eol-crlf")
                                                .label("CRLF")
                                                .compact()
                                                .when(overrides.eol == Some(Eol::CrLf), |button| {
                                                    button.primary()
                                                })
                                                .on_click(move |_, _, cx| {
                                                    let _ = weak_eol_crlf.update(cx, |this, cx| {
                                                        this.set_language_eol(Some(Eol::CrLf), cx);
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-language-eol-cr")
                                                .label("CR")
                                                .compact()
                                                .when(overrides.eol == Some(Eol::Cr), |button| {
                                                    button.primary()
                                                })
                                                .on_click(move |_, _, cx| {
                                                    let _ = weak_eol_cr.update(cx, |this, cx| {
                                                        this.set_language_eol(Some(Eol::Cr), cx);
                                                    });
                                                }),
                                        ),
                                )
                        })
                    })
                    .when(visible.contains("editor"), |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.editor")),
                                )
                                .child(toggle_row(
                                    tr("prefs.line_numbers"),
                                    settings.show_line_numbers,
                                    "prefs-linenumbers",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_line_numbers = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.word_wrap"),
                                    settings.word_wrap,
                                    "prefs-wrap",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.word_wrap = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.match_brackets"),
                                    settings.highlight_matching_brackets,
                                    "prefs-brackets",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.highlight_matching_brackets = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.trim_on_save"),
                                    settings.trim_trailing_whitespace_on_save,
                                    "prefs-trim",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.trim_trailing_whitespace_on_save = checked;
                                        this.apply_settings(window, cx);
                                    },
                                )),
                        )
                    })
                    .when(visible.contains("session"), |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.session")),
                                )
                                .child(toggle_row(
                                    tr("prefs.restore_session"),
                                    settings.restore_session,
                                    "prefs-restore",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.restore_session = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.autosave"),
                                    settings.autosave_session,
                                    "prefs-autosave",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.autosave_session = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.hidden_files"),
                                    settings.show_hidden_files,
                                    "prefs-hidden",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_hidden_files = checked;
                                        this.explorer.refresh();
                                        this.apply_settings(window, cx);
                                    },
                                )),
                        )
                    })
                    .when(
                        visible.contains("history") || visible.contains("keybindings"),
                        |this| {
                            this.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(tr("prefs.local_history")),
                                    )
                                    .child(toggle_row(
                                        tr("prefs.local_history_enabled"),
                                        settings.local_history_enabled,
                                        "prefs-history-enabled",
                                        weak.clone(),
                                        |this, checked, window, cx| {
                                            this.settings.local_history_enabled = checked;
                                            this.apply_settings(window, cx);
                                        },
                                    ))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_3()
                                            .child(
                                                div()
                                                    .w(px(140.))
                                                    .text_sm()
                                                    .child(tr("prefs.local_history_days")),
                                            )
                                            .child(
                                                Button::new("prefs-history-days-down")
                                                    .label("−")
                                                    .compact()
                                                    .on_click(move |_, window, cx| {
                                                        let _ = weak_days_down.update(
                                                            cx,
                                                            |this, cx| {
                                                                this.settings.local_history_days =
                                                                    this.settings
                                                                        .local_history_days
                                                                        .saturating_sub(1)
                                                                        .max(1);
                                                                this.apply_settings(window, cx);
                                                            },
                                                        );
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .child(settings.local_history_days.to_string()),
                                            )
                                            .child(
                                                Button::new("prefs-history-days-up")
                                                    .label("+")
                                                    .compact()
                                                    .on_click(move |_, window, cx| {
                                                        let _ =
                                                            weak_days_up.update(cx, |this, cx| {
                                                                this.settings.local_history_days =
                                                                    (this
                                                                        .settings
                                                                        .local_history_days
                                                                        + 1)
                                                                    .min(3650);
                                                                this.apply_settings(window, cx);
                                                            });
                                                    }),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_3()
                                            .child(
                                                div()
                                                    .w(px(140.))
                                                    .text_sm()
                                                    .child(tr("prefs.local_history_size")),
                                            )
                                            .child(
                                                Button::new("prefs-history-mb-down")
                                                    .label("−")
                                                    .compact()
                                                    .on_click(move |_, window, cx| {
                                                        let _ =
                                                            weak_mb_down.update(cx, |this, cx| {
                                                                this.settings
                                                                    .local_history_max_mb = this
                                                                    .settings
                                                                    .local_history_max_mb
                                                                    .saturating_sub(1)
                                                                    .max(1);
                                                                this.apply_settings(window, cx);
                                                            });
                                                    }),
                                            )
                                            .child(
                                                div().text_sm().child(
                                                    settings.local_history_max_mb.to_string(),
                                                ),
                                            )
                                            .child(
                                                Button::new("prefs-history-mb-up")
                                                    .label("+")
                                                    .compact()
                                                    .on_click(move |_, window, cx| {
                                                        let _ =
                                                            weak_mb_up.update(cx, |this, cx| {
                                                                this.settings
                                                                    .local_history_max_mb = (this
                                                                    .settings
                                                                    .local_history_max_mb
                                                                    + 1)
                                                                .min(4096);
                                                                this.apply_settings(window, cx);
                                                            });
                                                    }),
                                            ),
                                    )
                                    .child(
                                        Button::new("prefs-shortcuts")
                                            .label(tr("shortcuts.customize"))
                                            .compact()
                                            .on_click(move |_, _, cx| {
                                                let _ = weak_shortcuts.update(cx, |this, cx| {
                                                    this.open_shortcut_editor(cx);
                                                });
                                            }),
                                    ),
                            )
                        },
                    )
                    .when(visible.contains("view"), |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.view")),
                                )
                                .child(toggle_row(
                                    tr("view.whitespace"),
                                    settings.show_whitespace,
                                    "prefs-whitespace",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_whitespace = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("view.indent_guides"),
                                    settings.show_indent_guides,
                                    "prefs-indent-guides",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_indent_guides = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("view.ruler"),
                                    settings.show_ruler,
                                    "prefs-ruler",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_ruler = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("view.document_map"),
                                    settings.show_document_map,
                                    "prefs-document-map",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_document_map = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("view.bracket_colors"),
                                    settings.bracket_colors,
                                    "prefs-bracket-colors",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.bracket_colors = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("view.outline"),
                                    settings.show_outline,
                                    "prefs-outline",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_outline = checked;
                                        this.apply_settings(window, cx);
                                    },
                                ))
                                .child(toggle_row(
                                    tr("view.breadcrumbs"),
                                    settings.show_breadcrumbs,
                                    "prefs-breadcrumbs",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_breadcrumbs = checked;
                                        this.apply_settings(window, cx);
                                    },
                                )),
                        )
                    })
                    .when(visible.contains("workspace"), |this| {
                        let weak_open = weak.clone();
                        let weak_save = weak.clone();
                        let weak_save_as = weak.clone();
                        let weak_add = weak.clone();
                        let weak_close_workspace = weak.clone();
                        let roots = if self.workspace_roots.is_empty() {
                            tr("workspace.none")
                        } else {
                            self.workspace_roots
                                .iter()
                                .map(|root| root.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        let name = if self.workspace_name.is_empty() {
                            tr("workspace.none")
                        } else {
                            self.workspace_name.clone()
                        };
                        let file = self
                            .workspace_file
                            .as_ref()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| tr("workspace.memory_only"));
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.workspace")),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .text_xs()
                                        .text_color(cx.theme().colors.muted_foreground)
                                        .child(tr_args("prefs.workspace_name", &[("name", name)]))
                                        .child(tr_args("prefs.workspace_file", &[("path", file)])),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().colors.muted_foreground)
                                        .child(roots),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .flex_wrap()
                                        .child(
                                            Button::new("prefs-workspace-open")
                                                .label(tr("workspace.open"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_open.update(cx, |this, cx| {
                                                        this.open_workspace_dialog(window, cx);
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-workspace-save")
                                                .label(tr("workspace.save"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_save.update(cx, |this, cx| {
                                                        this.save_workspace(window, cx);
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-workspace-save-as")
                                                .label(tr("workspace.save_as"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_save_as.update(cx, |this, cx| {
                                                        this.save_workspace_as(window, cx);
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-workspace-add")
                                                .label(tr("workspace.add_folder"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_add.update(cx, |this, cx| {
                                                        this.open_add_folder_dialog(window, cx);
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-workspace-close")
                                                .label(tr("workspace.close"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_close_workspace.update(
                                                        cx,
                                                        |this, cx| {
                                                            this.close_workspace(window, cx);
                                                        },
                                                    );
                                                }),
                                        ),
                                )
                                .child(toggle_row(
                                    tr("ff.respect_gitignore"),
                                    settings.respect_gitignore,
                                    "prefs-respect-gitignore",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.respect_gitignore = checked;
                                        this.find_in_files.respect_gitignore = checked;
                                        this.apply_settings(window, cx);
                                        this.rebuild_index(cx);
                                        if this.find_in_files.open && this.find_in_files.workspace {
                                            this.start_find_in_files(cx);
                                        }
                                    },
                                ))
                                .child(toggle_row(
                                    tr("prefs.hidden_files"),
                                    settings.show_hidden_files,
                                    "prefs-workspace-hidden",
                                    weak.clone(),
                                    |this, checked, window, cx| {
                                        this.settings.show_hidden_files = checked;
                                        this.apply_settings(window, cx);
                                        this.rebuild_index(cx);
                                    },
                                )),
                        )
                    })
                    .when(visible.contains("files"), |this| {
                        let weak_limit_down = weak.clone();
                        let weak_limit_up = weak.clone();
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.files")),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .child(
                                            div()
                                                .w(px(200.))
                                                .text_sm()
                                                .child(tr("prefs.max_file_size")),
                                        )
                                        .child(
                                            Button::new("prefs-max-size-down")
                                                .label("−")
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ =
                                                        weak_limit_down.update(cx, |this, cx| {
                                                            this.settings.max_file_size_mb = this
                                                                .settings
                                                                .max_file_size_mb
                                                                .saturating_sub(64)
                                                                .max(1);
                                                            this.apply_settings(window, cx);
                                                        });
                                                }),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .child(format!("{} MB", settings.max_file_size_mb)),
                                        )
                                        .child(
                                            Button::new("prefs-max-size-up")
                                                .label("+")
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_limit_up.update(cx, |this, cx| {
                                                        this.settings.max_file_size_mb =
                                                            (this.settings.max_file_size_mb + 64)
                                                                .min(4096);
                                                        this.apply_settings(window, cx);
                                                    });
                                                }),
                                        ),
                                ),
                        )
                    })
                    .when(visible.contains("backup"), |this| {
                        let weak_export = weak.clone();
                        let weak_import = weak.clone();
                        let weak_reset = weak.clone();
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("prefs.import_export")),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .child(
                                            Button::new("prefs-export-settings")
                                                .label(tr("settings.export"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_export.update(cx, |this, cx| {
                                                        this.open_export_settings_dialog(
                                                            window, cx,
                                                        );
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-import-settings")
                                                .label(tr("settings.import"))
                                                .compact()
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak_import.update(cx, |this, cx| {
                                                        this.open_import_settings_dialog(
                                                            window, cx,
                                                        );
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("prefs-reset-settings")
                                                .label(tr("settings.reset"))
                                                .compact()
                                                .on_click(move |_, _, cx| {
                                                    let _ = weak_reset.update(cx, |this, cx| {
                                                        this.confirm = Some(ConfirmState::new(
                                                            CloseIntent::ResetSettings,
                                                            tr("settings.reset_title"),
                                                            tr("settings.reset_detail"),
                                                        ));
                                                        cx.notify();
                                                    });
                                                }),
                                        ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(tr("prefs.footer")),
                    ),
            )
            .into_any_element();
        let _ = window;
        self.overlay_card(680.0, Some(640.0), cx, content)
    }

    fn render_font_picker(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(picker) = &self.preferences.font_picker else {
            return div().into_any_element();
        };
        let filter = picker.filter.clone();
        let filter_text = filter.read(cx).value().to_string();
        let current = self.settings.font_family.clone();
        let weak = cx.weak_entity();
        let default_selected = current.is_empty();
        let weak_default = weak.clone();
        let mut list = vec![div()
            .id("font-default")
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .text_sm()
            .cursor_pointer()
            .when(default_selected, |this| {
                this.bg(cx.theme().colors.secondary)
            })
            .hover(|this| this.bg(cx.theme().colors.secondary_hover))
            .child(if default_selected { "•" } else { " " })
            .child(tr("prefs.font_default"))
            .on_click(move |_, window, cx| {
                let _ = weak_default.update(cx, |this, cx| {
                    this.select_font_family(String::new(), window, cx);
                });
            })
            .into_any_element()];
        for name in matching_fonts(&picker.fonts, &filter_text) {
            let selected = name == current;
            let name_owned = name.to_string();
            let weak_item = weak.clone();
            list.push(
                div()
                    .id(format!("font:{name_owned}"))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(cx.theme().colors.secondary))
                    .hover(|this| this.bg(cx.theme().colors.secondary_hover))
                    .child(if selected { "•" } else { " " })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .child(name_owned.clone()),
                    )
                    .on_click(move |_, window, cx| {
                        let _ = weak_item.update(cx, |this, cx| {
                            this.select_font_family(name_owned.clone(), window, cx);
                        });
                    })
                    .into_any_element(),
            );
        }
        let weak_close = cx.weak_entity();
        let content = div()
            .flex()
            .flex_col()
            .h_full()
            .min_h(px(0.))
            .gap_2()
            .p_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("prefs.font_picker_title")),
                    )
                    .child(
                        Button::new("font-picker-close")
                            .label(tr("common.close"))
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.preferences.font_picker = None;
                                    this.focus_editor(window, cx);
                                    cx.notify();
                                });
                            }),
                    ),
            )
            .child(Input::new(&filter).h(px(30.)))
            .child(
                div()
                    .id("font-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scrollbar()
                    .children(list),
            )
            .into_any_element();
        self.overlay_card(480.0, Some(560.0), cx, content)
    }
}

impl Drop for LubanApp {
    fn drop(&mut self) {
        if self.window_registered {
            window_registry::unregister(self.window_id);
        }
    }
}

impl Render for LubanApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = window.bounds();
        self.window_width = bounds.size.width.as_f32();
        self.window_height = bounds.size.height.as_f32();
        self.window_maximized = window.is_maximized();
        window_registry::update_bounds(self.window_id, bounds, self.window_maximized);
        self.process_enter_actions(window, cx);
        self.sync_active(window, cx);
        self.sync_file_watch(cx);
        self.sync_large_file_panel(window, cx);
        self.autosave_session(cx);
        self.refresh_find(cx);
        self.refresh_outline(cx);
        self.refresh_decorations(cx);

        let title = self
            .active_doc()
            .map(|doc| {
                format!(
                    "{}{} - Luban Notepad",
                    doc.display_name(),
                    if doc.is_modified() { " *" } else { "" }
                )
            })
            .unwrap_or_else(|| "Luban Notepad".to_string());
        if title != self.window_title {
            self.window_title = title;
            window.set_window_title(&self.window_title);
        }

        let show_toolbar = self.settings.show_toolbar;
        let show_status = self.settings.show_status_bar;
        let show_explorer = self.settings.show_file_explorer;
        let show_find = self.find.open;
        let overlays = self.render_overlays(window, cx);

        // Draw our own title bar unless the platform really provides server-side
        // decorations; in that case the window manager already owns the title
        // bar, and the menus keep their own row.
        let use_custom_title_bar = self.manual_window_chrome
            || !cfg!(target_os = "linux")
            || matches!(window.window_decorations(), Decorations::Client { .. });
        let chrome = if use_custom_title_bar {
            self.render_title_bar(window, cx)
        } else {
            self.render_menu_bar(window, cx)
        };
        let toolbar = if show_toolbar {
            Some(self.render_toolbar(window, cx))
        } else {
            None
        };
        let panes = self.render_panes(window, cx);
        let banner = self.render_external_banner(cx);
        let find_bar = if show_find {
            Some(self.render_find_bar(window, cx))
        } else {
            None
        };
        let status_bar = if show_status {
            Some(self.render_status_bar(cx))
        } else {
            None
        };
        let explorer = if show_explorer {
            Some(self.render_side_panel(window, cx))
        } else {
            None
        };
        let can_save_part = self.active_doc().is_some_and(Document::is_partial);
        let huge_saving = self
            .tabs
            .iter()
            .any(|tab| tab.huge.as_ref().is_some_and(|ui| ui.save.is_some()));
        let large_file_panel = if self.large_file.open {
            Some(
                self.large_file
                    .render(can_save_part, huge_saving, window, cx),
            )
        } else {
            None
        };
        let weak_keys = cx.weak_entity();
        let key_listener = move |event: &KeyDownEvent, window: &mut Window, cx: &mut App| {
            let _ = weak_keys.update(cx, |this, cx| {
                if this.shortcut(event, window, cx) {
                    cx.stop_propagation();
                }
            });
        };

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().colors.background)
            .text_color(cx.theme().colors.foreground)
            .on_action(cx.listener(|this, _: &Search, window, cx| {
                this.open_find(false, window, cx);
                cx.stop_propagation();
            }))
            .on_action(
                cx.listener(|this, _: &gpui_kit::base::input::Replace, window, cx| {
                    this.open_find(true, window, cx);
                    cx.stop_propagation();
                }),
            )
            .capture_key_down(key_listener)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.open_dropped_paths(paths.paths(), window, cx);
            }))
            .child(chrome);
        if let Some(toolbar) = toolbar {
            root = root.child(toolbar);
        }
        if let Some(banner) = banner {
            root = root.child(banner);
        }
        root = root.child(
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_row()
                .children(explorer)
                .child(panes)
                .children(large_file_panel),
        );
        if let Some(find_bar) = find_bar {
            root = root.child(find_bar);
        }
        if let Some(status_bar) = status_bar {
            root = root.child(status_bar);
        }
        root = root.children(overlays);

        // The titlebar is drawn by the application on X11 even though the
        // platform reports server decorations: the titlebar was removed with
        // `_MOTIF_WM_HINTS`, and server decorations keep the root view free of
        // the transparent shadow margin that a compositor would have to blend.
        #[cfg(target_os = "linux")]
        if self.manual_window_chrome {
            return client_decorated_frame(
                root.into_any_element(),
                cx.theme().colors.border,
                window.is_maximized(),
            );
        }

        root.into_any_element()
    }
}

/// Wrap a manually decorated Linux window in a hairline border. The window
/// manager keeps its invisible resize border around the undecorated client, so
/// the application only contributes the visible edge; a maximized window needs
/// neither.
#[cfg(target_os = "linux")]
fn client_decorated_frame(root: AnyElement, border: Hsla, maximized: bool) -> AnyElement {
    if maximized {
        return root;
    }

    div()
        .size_full()
        .overflow_hidden()
        .border_1()
        .border_color(border)
        .child(root)
        .into_any_element()
}

#[cfg(test)]
mod tab_ops_tests {
    use super::{count_words, reorder_index, trim_queue};

    #[test]
    fn reorder_moves_before_the_target_and_ignores_no_ops() {
        assert_eq!(reorder_index(4, 0, 2), Some(1));
        assert_eq!(reorder_index(4, 3, 0), Some(0));
        assert_eq!(reorder_index(4, 1, 4), Some(3));
        assert_eq!(reorder_index(4, 1, 2), None);
        assert_eq!(reorder_index(4, 1, 1), None);
        assert_eq!(reorder_index(4, 4, 2), None);
    }

    #[test]
    fn trim_targets_skip_the_kept_tab() {
        assert_eq!(trim_queue(4, 1, false), vec![0, 2, 3]);
        assert_eq!(trim_queue(4, 1, true), vec![2, 3]);
        assert_eq!(trim_queue(4, 3, true), Vec::<usize>::new());
        assert_eq!(trim_queue(1, 0, false), Vec::<usize>::new());
        assert_eq!(trim_queue(3, 3, false), Vec::<usize>::new());
    }

    #[test]
    fn words_use_unicode_whitespace() {
        assert_eq!(count_words("alpha beta\ngamma\t"), 3);
        assert_eq!(count_words("   "), 0);
        assert_eq!(count_words("你好 世界"), 2);
    }
}

#[cfg(test)]
mod menu_label_tests {
    use super::{char_columns, truncate_middle};

    fn columns(text: &str) -> usize {
        text.chars().map(char_columns).sum()
    }

    #[test]
    fn short_labels_are_left_alone() {
        let text = "Recent: src/main.rs";
        assert_eq!(truncate_middle(text, 48), text);
        assert_eq!(truncate_middle("", 48), "");
    }

    #[test]
    fn long_labels_keep_both_ends_and_fit_the_budget() {
        let text = "/home/user/Documents/projects/very-long-file-name.txt";
        let label = truncate_middle(text, 48);
        assert!(label.starts_with("/home/"));
        assert!(label.ends_with(".txt"));
        assert_eq!(label.matches('…').count(), 1);
        assert!(columns(&label) <= 48);
    }

    #[test]
    fn wide_characters_consume_two_columns() {
        assert_eq!(char_columns('a'), 1);
        assert_eq!(char_columns('多'), 2);

        let text = "多".repeat(40);
        let label = truncate_middle(&text, 20);
        assert!(columns(&label) <= 20);
        assert_eq!(label.matches('…').count(), 1);
        assert!(label.starts_with('多'));
        assert!(label.ends_with('多'));
    }

    #[test]
    fn zero_budget_yields_nothing() {
        assert_eq!(truncate_middle("abc", 0), "");
    }
}

#[cfg(test)]
mod restore_tests {
    use super::RestoreSpec;
    use luban_core::encoding::TextEncoding;
    use luban_core::eol::Eol;

    #[test]
    fn partial_range_restore_stays_partial_and_refuses_source_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("range.txt");
        std::fs::write(&path, b"AAAAA\nBBBBB\nCCCCC\n").unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let progress = std::sync::atomic::AtomicU64::new(0);
        let mut doc =
            luban_core::Document::open_range(&path, 0, 6, 1024, &cancel, &progress).unwrap();
        assert!(doc.is_partial());
        assert!(doc.save().is_err());
        assert_eq!(std::fs::read(&path).unwrap().len(), 18);
    }

    #[test]
    fn recreated_non_text_buffer_writes_raw_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin.dat");
        let mut doc = luban_core::Document::new_untitled("bin.dat");
        doc.mark_non_text();
        doc.attach_text("x\r\ny".to_string());
        doc.encoding = TextEncoding::Windows1252;
        doc.eol = Eol::CrLf;
        doc.save_as(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x\r\ny");
    }

    #[test]
    fn per_language_indent_only_applies_to_ordinary_text() {
        let mut doc = luban_core::Document::new_untitled("untitled".to_string());
        assert!(super::LubanApp::language_indent_applies(&doc, false));
        assert!(!super::LubanApp::language_indent_applies(&doc, true));
        doc.mark_non_text();
        assert!(!super::LubanApp::language_indent_applies(&doc, false));
    }

    #[test]
    fn non_text_restore_keeps_raw_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin.dat");
        std::fs::write(&path, [0u8, b'\r', b'\n', 1]).unwrap();
        let spec = RestoreSpec {
            path: Some(path),
            text: Some("x\r\ny".to_string()),
            encoding: TextEncoding::Windows1252,
            eol: Eol::CrLf,
            language: "Plain Text".to_string(),
            cursor: 0,
            anchor: 0,
            modified: true,
            fallback_name: "bin.dat".to_string(),
            range: None,
            huge: false,
            huge_line: None,
            huge_column: None,
            bookmarks: Vec::new(),
            group: 0,
        };
        let doc = spec.load(1024);
        assert!(doc.is_non_text());
        assert_eq!(doc.text, "x\r\ny");
        assert!(doc.is_modified());
    }

    #[test]
    fn partial_restore_applies_unsaved_text_and_keeps_the_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("range.txt");
        std::fs::write(&path, b"AAAAA\nBBBBB\nCCCCC\n").unwrap();
        let spec = RestoreSpec {
            path: Some(path),
            text: Some("BBBBB\nCHANGED\n".to_string()),
            encoding: TextEncoding::Utf8,
            eol: Eol::Lf,
            language: "Plain Text".to_string(),
            cursor: 0,
            anchor: 0,
            modified: true,
            fallback_name: "range.txt".to_string(),
            range: Some([6, 12]),
            huge: false,
            huge_line: None,
            huge_column: None,
            bookmarks: Vec::new(),
            group: 0,
        };
        let doc = spec.load(1024);
        assert!(doc.is_partial());
        assert_eq!(doc.text, "BBBBB\nCHANGED\n");
        assert!(doc.is_modified());
    }

    #[test]
    fn restore_clamps_a_range_beyond_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, b"aaa\nbbb\n").unwrap();
        let spec = RestoreSpec {
            path: Some(path),
            text: None,
            encoding: TextEncoding::Utf8,
            eol: Eol::Lf,
            language: "Plain Text".to_string(),
            cursor: 0,
            anchor: 0,
            modified: false,
            fallback_name: "small.txt".to_string(),
            range: Some([100, 200]),
            huge: false,
            huge_line: None,
            huge_column: None,
            bookmarks: Vec::new(),
            group: 0,
        };
        let doc = spec.load(1024);
        assert!(doc.is_partial());
        let range = doc.partial_range().unwrap();
        assert_eq!(range.total, 8);
        assert!(range.start < 8);
        assert!(range.end <= 8);
        assert_eq!(doc.text, "bbb\n");
    }
}

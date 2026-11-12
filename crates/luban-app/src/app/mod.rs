use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::assets::IconName;
use gpui_kit::base::input::{AddCursorAbove, AddCursorBelow};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::input::{
    BufferPoint, Copy, Cut, Editor, EditorState, FoldRange, HighlightStyleResolver, Indent, Input,
    InputEvent, InputState, Outdent, Paste, Position, RangeDecoration, RangeDecorationCollection,
    RangeDecorationStyle, Redo, RopeExt, Search, SelectAll, TabSize, TextDecoration,
    TextDecorationCollection, Undo,
};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::{ScrollableElement as _, Scrollbar, ScrollbarMode};
use gpui_kit::component::slider::SliderEvent;
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::toolbar::{Toolbar, ToolbarGroup};
use gpui_kit::component::Icon;
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
use luban_core::search::{ByteMatch, Match, SearchOptions};
use luban_core::session::{GroupSession, Session, SessionTab, SplitLayout, WindowSession};
use luban_core::settings::{
    LanguageSettings, Settings, ThemeChoice, PLUGIN_PANEL_MAX_WIDTH, PLUGIN_PANEL_MIN_WIDTH,
};
use luban_core::text as textutil;
use luban_core::undo::EditKind;
use luban_core::workspace::{
    search_workspace, FileMatch, FindInFilesOptions, FindInFilesOutcome, WorkspaceSearchOptions,
};
use luban_core::workspace_file::{WorkspaceFile, WorkspaceSettings};
use luban_core::workspace_index::{build_index, IndexOptions, WorkspaceIndex};
use luban_plugin::manifest::StatusAlignment;

use crate::column_editor::ColumnEditorState;
use crate::commands::{self, CommandContext, Keymap};
use crate::dialogs::{
    CloseIntent, ConfirmState, GoToLineState, LanguagePickerState, SaveAsRequest, SaveThen,
};
use crate::document_map;
use crate::explorer::ExplorerState;
use crate::file_picker::{
    breadcrumbs, build_sections, file_not_found_message, filter_presets, format_modified,
    format_size, load_preview, next_listing_generation, not_a_folder_message, FileDialogMode,
    FileDialogPurpose, FileDialogState, FileFilter, NewFolderState, PickerRow, PickerRowKind,
    PlaceIcon, PlaceSection, PreviewContent, PreviewKind, SortKey,
};
use crate::file_watch::{FileWatcher, WatchEventKind};
use crate::find::{FindInFilesPanel, FindPanel, MarkHighlight, MAX_HIGHLIGHTS};
use crate::huge_file::{
    clamp_top_line, diff_bytes, max_top_line, top_line_from_scroll, window_max_top_line,
    HugeFileUi, SaveJob, SearchJob, SearchKind, WindowJump, WindowRequest, WindowTarget,
    EDGE_LINES, PREFETCH_LINES,
};
use crate::huge_gutter::{self, HugeGutterProps};
use crate::huge_scrollbar::{self, HugeScrollbarProps};
use crate::i18n::{active_language_name, plugin_text, tr, tr_args};
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

/// Shared result of a command run in a newly created IPC window.
pub type CommandResult = std::sync::Arc<std::sync::Mutex<Option<Result<(), String>>>>;

pub struct LaunchOptions {
    pub files: Vec<PathBuf>,
    pub smoke_test: bool,
    /// Disable plugin loading for this run without changing settings.
    pub safe_mode: bool,
    /// Only the primary instance writes `session.json`.
    pub session_writer: bool,
    /// Exit after the tabs opened by this launch have been closed.
    pub wait: bool,
    /// Initial file/line/column request.
    pub goto: Option<(PathBuf, usize, usize)>,
    /// Initial read-only diff request.
    pub diff: Option<(PathBuf, PathBuf)>,
    /// Standard input opened as an untitled buffer.
    pub stdin: Option<String>,
    /// Command to run after the initial files open.
    pub command: Option<String>,
    /// A command failure should terminate this process (CLI launch).
    pub exit_on_command_failure: bool,
    /// Result of a command run in a newly created window (IPC `--new-window`).
    pub command_result: Option<CommandResult>,
    /// IPC endpoint owned by the first window of the primary process.
    pub ipc_queue: Option<crate::ipc::IpcQueue>,
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
    /// Runtime plugin commands, shown as one extra section.
    plugin_commands: Vec<&'static str>,
    /// Section index where plugin commands start.
    plugin_section: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaletteKind {
    Commands,
    QuickOpen,
    Snippets,
}

struct PendingWait {
    id: String,
    tab_ids: Vec<u64>,
    reply: std::sync::mpsc::Sender<crate::ipc::IpcResponse>,
}

struct DiffTask {
    tab_id: u64,
    cancel: Arc<AtomicBool>,
}

struct PluginPanelState {
    plugin_id: String,
    panel_id: String,
    title: luban_plugin::localized::LocalizedText,
    placement: luban_plugin::manifest::PanelPlacement,
    /// Preferred width in points: the width the user dragged to, or the
    /// remembered/manifest width until the panel is resized.
    width: f32,
    lines: Vec<luban_plugin::runtime::PanelLine>,
    /// Optional flat action bar rendered next to the panel title.
    actions: Vec<luban_plugin::runtime::PanelAction>,
    error: Option<String>,
    truncated: bool,
    /// Token identifying the watcher task allowed to refresh this panel.
    watch: u64,
    /// Scroll position, preserved across automatic refreshes.
    scroll: ScrollHandle,
}

/// Width drag in progress on the inner edge of a plugin panel.
#[derive(Clone, Debug)]
struct PluginPanelDrag {
    /// `plugin_id/panel_id` of the panel being resized.
    panel_key: String,
    /// Pointer x when the drag started.
    origin_x: Pixels,
    /// Rendered panel width when the drag started.
    origin_width: f32,
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
    plugin_panel_focus: FocusHandle,
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
    file_search_input: Entity<InputState>,
    file_filter_input: Entity<InputState>,
    file_new_folder_input: Entity<InputState>,
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
    pending_new_folder_create: bool,
    /// A path to select once the picker finishes listing its parent folder.
    pending_file_picker_select: Option<PathBuf>,
    /// Enter was pressed in the picker's editable address field.
    pending_file_picker_address: bool,
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
    pub plugin_host: crate::plugin_host::PluginHost,
    pub safe_mode: bool,
    pub session_writer: bool,
    pub snippets: luban_core::snippet::SnippetSet,
    pub packages: luban_core::user_packages::PackageReport,
    pub package_paths: luban_core::user_packages::PackagePaths,
    pub snippet_session: Option<crate::snippet_ui::SnippetSession>,
    snippet_picker_rows: Vec<crate::snippet_ui::SnippetRow>,
    pub plugin_manager: crate::plugin_manager::PluginManagerState,
    plugin_panel: Option<PluginPanelState>,
    /// Resize drag in progress on the open panel's inner edge.
    plugin_panel_drag: Option<PluginPanelDrag>,
    /// Monotonic token handed to the single live plugin-panel watcher.
    plugin_panel_watch: u64,
    /// How many plugin-originated call applications are in progress. While
    /// this is above zero, document mutations belong to a plugin call's own
    /// side effects; the event pump skips emission so plugin actions cannot
    /// cascade back into the plugin that caused them.
    plugin_event_depth: u32,
    /// Document events queued for the plugin event pump (50 ms tick).
    plugin_event_queue: VecDeque<plugin_events::QueuedPluginEvent>,
    /// Whether the plugin event pump task has been spawned. Only set when the
    /// spawn actually happened, so a later reload that installs the first
    /// runtime retries.
    plugin_pump_started: bool,
    /// Debounces edit generations for the pump's coalesced `on_edit` dispatch.
    plugin_edit_debouncer: plugin_events::EditDebouncer,
    ipc_queue: Option<
        std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<crate::ipc::IpcCommand>>>,
    >,
    pending_waits: Vec<PendingWait>,
    wait: bool,
    wait_exit_scheduled: bool,
    wait_for_tabs: Vec<u64>,
    diff_tasks: Vec<DiffTask>,
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

/// Width of a plugin panel that does not declare one in its manifest.
const PLUGIN_PANEL_DEFAULT_WIDTH: f32 = 380.0;
/// Space the editor keeps when a right-side plugin panel is open.
const PLUGIN_PANEL_EDITOR_RESERVE: f32 = 320.0;
/// Grab band drawn along the inner edge of a right-side plugin panel.
const PLUGIN_PANEL_HANDLE_WIDTH: f32 = 8.0;

/// Widest a right-side plugin panel may get in a window `window_width` wide:
/// the manifest cap, the editor reserve, or the panel minimum when the window
/// is too narrow for both.
fn plugin_panel_max_width(window_width: f32) -> f32 {
    (window_width - PLUGIN_PANEL_EDITOR_RESERVE)
        .clamp(PLUGIN_PANEL_MIN_WIDTH, PLUGIN_PANEL_MAX_WIDTH)
}

/// Fit a preferred panel width into the window without breaking the bounds.
fn fit_plugin_panel_width(preferred: f32, window_width: f32) -> f32 {
    preferred.clamp(PLUGIN_PANEL_MIN_WIDTH, plugin_panel_max_width(window_width))
}

/// Width a drag has reached: the pointer moved the edge by the distance since
/// it went down, fitted into the window. Rounded so a dragged panel never
/// stores fractional points.
fn plugin_panel_drag_width(
    origin_width: f32,
    origin_x: f32,
    position_x: f32,
    window_width: f32,
) -> f32 {
    fit_plugin_panel_width(origin_width + (origin_x - position_x), window_width).round()
}

/// Settings key remembering one panel's dragged width.
fn plugin_panel_width_key(plugin_id: &str, panel_id: &str) -> String {
    format!("{plugin_id}/{panel_id}")
}

/// Width a panel opens with: the width the user dragged for this panel, else
/// the manifest width, else the default. Panels remember their own width.
fn plugin_panel_open_width(
    settings: &Settings,
    plugin_id: &str,
    panel_id: &str,
    manifest_width: Option<u16>,
) -> f32 {
    settings
        .plugin_panel_widths
        .get(&plugin_panel_width_key(plugin_id, panel_id))
        .copied()
        .or_else(|| manifest_width.map(f32::from))
        .unwrap_or(PLUGIN_PANEL_DEFAULT_WIDTH)
        .clamp(PLUGIN_PANEL_MIN_WIDTH, PLUGIN_PANEL_MAX_WIDTH)
}

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

fn count_words(text: &str) -> usize {
    text.split_whitespace().count()
}

fn plugin_permission_summary(permissions: &[String]) -> String {
    if permissions.is_empty() {
        return tr("plugins.no_permissions");
    }
    permissions
        .iter()
        .map(|name| {
            let implemented = luban_plugin::permissions::Permission::ALL
                .iter()
                .any(|permission| permission.name() == name && permission.implemented());
            if implemented {
                name.clone()
            } else {
                format!("{name} ({})", tr("plugins.unavailable"))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn plugin_platform_summary(plugin: &luban_plugin::store::StorePlugin) -> String {
    let targets = plugin.supported_targets();
    if targets.contains(&luban_plugin::store::Target::Universal) {
        return tr("plugins.store.all_platforms");
    }
    if targets.is_empty() {
        return "—".to_string();
    }
    targets
        .iter()
        .map(|target| target.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

mod command_ui;
mod editor_view;
mod file_picker_ui;
mod find_replace_ui;
mod find_window_ui;
mod huge_file_ui;
mod menus;
mod plugin_events;
mod plugin_manager_ui;
mod plugin_ui;
mod preferences_ui;
mod save;
mod session;
mod sidebar;
mod tabs;

use session::session_tab_from;
use tabs::{ClosedTab, Tab};

impl LubanApp {
    // ------------------------------------------------------------------
    // Construction and startup
    // ------------------------------------------------------------------

    pub fn new(window: &mut Window, cx: &mut Context<Self>, mut options: LaunchOptions) -> Self {
        let mut settings = Settings::load();
        let package_paths = luban_core::user_packages::PackagePaths::system();
        if let Err(error) = package_paths.ensure() {
            log::warn!("could not create user package directories: {error}");
        }
        let packages = luban_core::user_packages::load(&package_paths);
        let syntax_build = luban_core::user_syntax::build(&packages.syntax_files);
        for error in &syntax_build.errors {
            log::warn!("user syntax: {error}");
        }
        luban_core::user_syntax::set_active_set(syntax_build.set);
        let user_languages = syntax_build
            .languages
            .iter()
            .map(|info| language::UserLanguageSpec {
                name: info.name.clone(),
                extensions: info.extensions.clone(),
                line_comment: None,
                block_comment: None,
                first_line_match: info.first_line_match.clone(),
            })
            .collect();
        language::set_user_languages(user_languages);
        let plugin_locale = luban_core::i18n::resolve(&settings.ui_language);
        let mut plugin_host =
            crate::plugin_host::PluginHost::load(&settings, options.safe_mode, plugin_locale);
        for (plugin_id, values) in &plugin_host.settings {
            settings
                .plugin_settings
                .insert(plugin_id.clone(), values.clone());
        }
        plugin_host.settings = settings.plugin_settings.clone();
        // Drop entries for commands this build no longer knows about, and
        // entries that cannot be parsed. Disabled/unapproved plugins keep their
        // overrides so safe mode and permission changes do not erase them.
        let known_plugin_commands = plugin_host.known_command_ids();
        settings.keybindings.retain(|id, text| {
            let valid = (commands::spec(id).is_some()
                || plugin_host.has_command(id)
                || known_plugin_commands.contains(id))
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
        let mut startup_notice = None;
        if let Err(error) = syntax_theme::apply_with_fallback(&settings, cx) {
            log::warn!("could not apply syntax theme: {error}");
            startup_notice = Some(tr("theme.invalid_fallback").to_string());
        }

        let plugin_store_url = settings.plugin_store_url.clone();
        let find_placement = settings.find_window;
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
            find: FindPanel::new(&find_placement, window, cx),
            find_in_files: FindInFilesPanel::new(window, cx),
            explorer: ExplorerState::new(),
            explorer_focus: cx.focus_handle(),
            outline_focus: cx.focus_handle(),
            plugin_panel_focus: cx.focus_handle(),
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
            file_search_input: cx
                .new(|cx| InputState::new(window, cx).placeholder(tr("dialog.search_placeholder"))),
            file_filter_input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr("dialog.filter_custom_placeholder"))
            }),
            file_new_folder_input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr("dialog.new_folder_placeholder"))
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
            occurrence_anchor: None,
            sort_dialog: None,
            filter_dialog: None,
            pending_history_restore: None,
            closed_tabs: Vec::new(),
            trim_plan: None,
            keymap: Keymap::build_with(&keybindings, &plugin_host.dynamic_specs()),
            history: HistoryStore::system(),
            status: startup_notice.clone().unwrap_or_default(),
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
            pending_new_folder_create: false,
            pending_file_picker_select: None,
            pending_file_picker_address: false,
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
            safe_mode: options.safe_mode,
            session_writer: options.session_writer,
            snippets: packages.snippets.clone(),
            packages,
            package_paths,
            snippet_session: None,
            snippet_picker_rows: Vec::new(),
            plugin_manager: crate::plugin_manager::PluginManagerState::new(
                window,
                cx,
                &plugin_store_url,
            ),
            plugin_panel: None,
            plugin_panel_drag: None,
            plugin_panel_watch: 0,
            plugin_event_depth: 0,
            plugin_event_queue: VecDeque::new(),
            plugin_pump_started: false,
            plugin_edit_debouncer: plugin_events::EditDebouncer::new(),
            ipc_queue: options.ipc_queue,
            pending_waits: Vec::new(),
            wait: options.wait,
            wait_exit_scheduled: false,
            wait_for_tabs: Vec::new(),
            diff_tasks: Vec::new(),
            plugin_host,
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
                                if app.file_dialog_escape(window, cx) {
                                    true
                                } else {
                                    app.cancel_file_dialog(cx);
                                    app.focus_editor(window, cx);
                                    true
                                }
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
                            } else if app.plugin_manager.open {
                                app.plugin_manager.open = false;
                                app.focus_editor(window, cx);
                                cx.notify();
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
                            } else if app.preferences.open {
                                app.close_preferences(window, cx);
                                true
                            } else if app.snippet_session.is_some() {
                                app.end_snippet(window, cx);
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
                let key = event.keystroke.key.to_ascii_lowercase();
                let command = modifiers.secondary(cx) || modifiers.control;
                // The editor binds Tab, Shift+Tab, Ctrl+F and Ctrl+H to its
                // own actions, so these keys never reach the normal shortcut
                // handler. Resolve snippet navigation and the application find
                // bar here, before those actions run.
                if key == "tab" && !command && !modifiers.alt {
                    let handled = weak_shortcuts
                        .update(cx, |app, cx| {
                            if app.snippet_session.is_none() {
                                return false;
                            }
                            let editor_focused = app.tabs.get(app.active).is_some_and(|tab| {
                                tab.editor.read(cx).focus_handle(cx).is_focused(window)
                            });
                            if !editor_focused {
                                return false;
                            }
                            app.snippet_next(!modifiers.shift, window, cx);
                            true
                        })
                        .unwrap_or(false);
                    if handled {
                        cx.stop_propagation();
                    }
                    return;
                }
                if command
                    && !modifiers.shift
                    && !modifiers.alt
                    && matches!(key.as_str(), "f" | "h")
                {
                    let handled = weak_shortcuts
                        .update(cx, |app, cx| {
                            let editor_focused = app.tabs.get(app.active).is_some_and(|tab| {
                                tab.editor.read(cx).focus_handle(cx).is_focused(window)
                            });
                            if !editor_focused {
                                return false;
                            }
                            app.open_find(key == "h", window, cx);
                            true
                        })
                        .unwrap_or(false);
                    if handled {
                        cx.stop_propagation();
                    }
                    return;
                }
                // Huge-document shortcuts must not act behind the modal file
                // picker; the picker's own handler runs from `shortcut`.
                let picker_open = weak_shortcuts
                    .update(cx, |app, _| app.file_dialog.is_some())
                    .unwrap_or(false);
                if picker_open {
                    return;
                }
                if command && !modifiers.shift && !modifiers.alt && key == "z" {
                    let _ = weak_shortcuts.update(cx, |app, cx| {
                        let editor_focused = app.tabs.get(app.active).is_some_and(|tab| {
                            tab.editor.read(cx).focus_handle(cx).is_focused(window)
                        });
                        if editor_focused {
                            app.snippet_session = None;
                        }
                    });
                }
                if !command {
                    return;
                }
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
                    this.find.status_sticky = false;
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
        app._subscriptions
            .push(cx.subscribe(&app.find.mark_query, |this, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.find.mark_status.clear();
                    cx.notify();
                }
            }));
        app._subscriptions.push(
            cx.subscribe(&app.find_in_files.query, |this, _, event, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.start_find_in_files(cx);
                }
            }),
        );
        for entity in [
            app.find_in_files.query.clone(),
            app.find_in_files.folder.clone(),
            app.find_in_files.masks.clone(),
            app.find_in_files.include.clone(),
            app.find_in_files.exclude.clone(),
            app.find_in_files.replacement.clone(),
            app.preferences.search.clone(),
            app.plugin_manager.store.search.clone(),
            app.plugin_manager.store.url_input.clone(),
        ] {
            app._subscriptions
                .push(cx.subscribe(&entity, |_, _, event, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                }));
        }
        app._subscriptions
            .push(cx.subscribe(&app.file_path_input, |this, _, event, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    if this
                        .file_dialog
                        .as_ref()
                        .is_some_and(|dialog| dialog.address_edit)
                    {
                        this.pending_file_picker_address = true;
                    } else {
                        this.pending_file_confirm = true;
                    }
                    cx.notify();
                }
            }));
        app._subscriptions
            .push(cx.subscribe(&app.file_name_input, |this, _, event, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.pending_file_confirm = true;
                    cx.notify();
                }
            }));
        app._subscriptions
            .push(
                cx.subscribe(&app.file_search_input, |this, _, event, cx| match event {
                    InputEvent::Change => {
                        let value = this.file_search_input.read(cx).value().to_string();
                        if let Some(dialog) = &mut this.file_dialog {
                            dialog.set_search(value);
                        }
                        this.refresh_file_picker_preview(cx);
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => {
                        this.pending_file_confirm = true;
                        cx.notify();
                    }
                    _ => {}
                }),
            );
        app._subscriptions
            .push(cx.subscribe(&app.file_filter_input, |this, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = this.file_filter_input.read(cx).value().to_string();
                    if let Some(dialog) = &mut this.file_dialog {
                        if dialog.custom_filter {
                            dialog.set_custom_filter(&value);
                        }
                    }
                    this.refresh_file_picker_preview(cx);
                    cx.notify();
                }
            }));
        app._subscriptions.push(
            cx.subscribe(
                &app.file_new_folder_input,
                |this, _, event, cx| match event {
                    InputEvent::Change => cx.notify(),
                    InputEvent::PressEnter { .. } => {
                        this.pending_new_folder_create = true;
                        cx.notify();
                    }
                    _ => {}
                },
            ),
        );
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

        let wait_paths: Vec<PathBuf> = options
            .files
            .iter()
            .cloned()
            .chain(options.goto.iter().map(|(path, _, _)| path.clone()))
            .collect();
        let had_files = !options.files.is_empty();
        let mut wait_before: HashSet<u64> = HashSet::new();
        if had_files {
            wait_before.extend(app.tabs.iter().map(|tab| tab.id));
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
        if !had_files {
            wait_before.extend(app.tabs.iter().map(|tab| tab.id));
        }
        if let Some((path, line, column)) = options.goto {
            app.open_path_at(&path, line, column, 0, window, cx);
        }
        if let Some((left, right)) = options.diff {
            app.open_diff(&left, &right, window, cx);
        }
        if let Some(text) = options.stdin {
            app.open_stdin(text, window, cx);
        }
        if let Some(command) = options.command {
            let succeeded = app.run_command(&command, window, cx);
            let message = if succeeded {
                None
            } else {
                Some(tr_args("status.command_failed", &[("command", command)]))
            };
            if let Some(message) = &message {
                if options.exit_on_command_failure {
                    eprintln!("{message}");
                    // `process::exit` skips `IpcServer`'s destructor; release
                    // the lock and socket so the next launch does not read a
                    // stale instance that this process no longer serves.
                    if app.ipc_queue.is_some() {
                        crate::ipc::cleanup(&luban_core::settings::config_dir());
                    }
                    std::process::exit(3);
                }
                app.set_status(message.clone(), cx);
            }
            if let Some(result) = &options.command_result {
                if let Ok(mut result) = result.lock() {
                    *result = Some(match message {
                        Some(message) => Err(message),
                        None => Ok(()),
                    });
                }
            }
        }
        // Opening the initial document clears the status set during startup, so
        // surface the theme fallback again once the window is populated.
        if let Some(notice) = startup_notice {
            app.status = notice;
        }
        if app.wait {
            app.wait_for_tabs = app
                .tabs
                .iter()
                .filter(|tab| !wait_before.contains(&tab.id))
                .map(|tab| tab.id)
                .collect();
            for path in &wait_paths {
                if let Some(tab) = app.tabs.iter().find(|tab| {
                    tab.doc
                        .path
                        .as_ref()
                        .is_some_and(|open| util::same_path(open, path))
                }) {
                    if !app.wait_for_tabs.contains(&tab.id) {
                        app.wait_for_tabs.push(tab.id);
                    }
                }
            }
            if app.wait_for_tabs.is_empty() {
                app.schedule_wait_exit();
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
                    app.poll_plugin_store(cx);
                })
                .is_err()
            {
                break;
            }
        })
        .detach();

        // The earliest reliable pump start: the window handle is available
        // here and any installed+enabled plugin is already loaded. The call is
        // a no-op while no runtime is active (lazy start); a later windowed
        // reload retries, so installing the first plugin starts the pump too.
        app.start_plugin_pump(window, cx);

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
            state.set_highlighter_factory(crate::highlighter::factory(), cx);
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
                if doc.read_only {
                    state.set_readonly(true, cx);
                }
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
                    tab.edit_generation = tab.edit_generation.wrapping_add(1);
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
            mark: None,
            mark_generation: 0,
            fold_generation: 0,
            needs_sync: false,
            pending_edit: false,
            edit_generation: 0,
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
            let changed = index != self.active;
            self.active = index;
            self.active_group = self.tabs[index].group;
            self.active_in_group[self.active_group] = Some(self.tabs[index].id);
            self.focus_editor = true;
            self.find.dirty = true;
            self.record_navigation(cx);
            if changed {
                // Re-opening a tab that is already open activates it rather
                // than opening a second tab: that is an active-document
                // change, not a document-open.
                self.emit_active_document_changed(cx);
            }
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
                // The tab is mounted and active now, so the snapshot reflects
                // the freshly opened document. Huge, large-file, partial,
                // non-text and read-only documents have no plugin snapshot and
                // do not emit (open_path_at, the file picker and drag-and-drop
                // all funnel fresh opens through this path).
                if let Some(snapshot) = self.plugin_document_snapshot(cx) {
                    self.plugin_emit(plugin_events::PluginEvent::DocumentOpened(snapshot));
                }
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

    fn goto_is_clamped(
        &self,
        index: usize,
        line: usize,
        column: usize,
        cx: &gpui_kit::App,
    ) -> bool {
        let Some(tab) = self.tabs.get(index) else {
            return false;
        };
        let rope = tab.editor.read(cx).text();
        let rows = rope.lines_len();
        if line > rows {
            return true;
        }
        let row = line.saturating_sub(1).min(rows.saturating_sub(1));
        column > rope.slice_line(row).len_chars() + 1
    }

    fn note_goto_clamped(&mut self, line: usize, column: usize, cx: &mut Context<Self>) {
        log::info!("goto {line}:{column} was clamped to the document bounds");
        self.set_status(
            tr_args(
                "status.goto_clamped",
                &[("line", line.to_string()), ("column", column.to_string())],
            ),
            cx,
        );
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
            if self.goto_is_clamped(index, line, column, cx) {
                self.note_goto_clamped(line, column, cx);
            }
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
        if self.goto_is_clamped(index, line, column, cx) {
            self.note_goto_clamped(line, column, cx);
        }
        self.focus_editor = true;
        self.find.dirty = true;
        cx.notify();
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

    pub fn new_window(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = window_registry::open_window(
            WindowInit::Blank,
            self.manual_window_chrome,
            self.session_writer,
            self.safe_mode,
            cx,
        ) {
            self.set_status(
                tr_args("window.open_failed", &[("error", error.to_string())]),
                cx,
            );
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

    fn schedule_wait_exit(&mut self) {
        if self.wait_exit_scheduled {
            return;
        }
        self.wait_exit_scheduled = true;
        let owns_ipc = self.ipc_queue.is_some();
        // Give the IPC thread a moment to write the response to the command
        // that closed the last wait target before the process exits.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            if owns_ipc {
                crate::ipc::cleanup(&luban_core::settings::config_dir());
            }
            std::process::exit(0);
        });
    }

    fn process_ipc_requests(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(queue) = self.ipc_queue.clone() else {
            return;
        };
        let commands: Vec<crate::ipc::IpcCommand> = {
            let mut queue = queue.lock().unwrap_or_else(|error| error.into_inner());
            queue.drain(..).collect()
        };
        for command in commands {
            self.handle_ipc_request(command, window, cx);
        }
    }

    fn handle_ipc_request(
        &mut self,
        command: crate::ipc::IpcCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = command.request;
        if request.new_window {
            if request.wait {
                let _ = command.reply.send(crate::ipc::IpcResponse::error(
                    request.id,
                    "--wait cannot be combined with --new-window",
                ));
                return;
            }
            let mut errors = Vec::new();
            for file in &request.files {
                if !crate::ipc::path_is_readable(file) {
                    errors.push(format!("cannot open {}", file.display()));
                }
            }
            if let Some(goto) = &request.goto {
                if !crate::ipc::path_is_readable(&goto.path) {
                    errors.push(format!("cannot open {}", goto.path.display()));
                }
            }
            if let Some((left, right)) = &request.diff {
                if !crate::ipc::path_is_readable(left) {
                    errors.push(format!("cannot open {}", left.display()));
                }
                if !crate::ipc::path_is_readable(right) {
                    errors.push(format!("cannot open {}", right.display()));
                }
            }
            if let Some(command_id) = &request.command {
                if commands::spec(command_id).is_none() && !self.plugin_host.has_command(command_id)
                {
                    errors.push(format!("command '{command_id}' failed"));
                }
            }
            if !errors.is_empty() {
                let _ = command.reply.send(crate::ipc::IpcResponse::error(
                    request.id,
                    errors.join("; "),
                ));
                return;
            }
            let command_result =
                std::sync::Arc::new(std::sync::Mutex::new(None::<Result<(), String>>));
            let command_result_slot = request.command.as_ref().map(|_| command_result.clone());
            let options = LaunchOptions {
                files: request.files.clone(),
                smoke_test: false,
                safe_mode: self.safe_mode,
                session_writer: self.session_writer,
                wait: false,
                goto: request
                    .goto
                    .as_ref()
                    .map(|goto| (goto.path.clone(), goto.line, goto.column)),
                diff: request.diff.clone(),
                stdin: request.stdin.clone(),
                command: request.command.clone(),
                exit_on_command_failure: false,
                command_result: command_result_slot,
                ipc_queue: self.ipc_queue.clone(),
                manual_window_chrome: self.manual_window_chrome,
                recoveries: Vec::new(),
                init: WindowInit::Blank,
                auto_recovery: false,
                window_id: window_registry::next_window_id(),
            };
            let bounds = WindowBounds::Windowed(window_registry::centered_bounds(cx));
            match window_registry::open_window_with(options, bounds, self.manual_window_chrome, cx)
            {
                Ok(_) => {
                    if request.command.is_none() {
                        let _ = command.reply.send(crate::ipc::IpcResponse::ok(request.id));
                    } else {
                        let id = request.id.clone();
                        let reply = command.reply;
                        let slot = command_result.clone();
                        cx.spawn(async move |_, cx| loop {
                            cx.background_executor()
                                .timer(Duration::from_millis(50))
                                .await;
                            let done = slot.lock().ok().and_then(|result| result.clone());
                            if let Some(result) = done {
                                let response = match result {
                                    Ok(()) => crate::ipc::IpcResponse::ok(id),
                                    Err(error) => crate::ipc::IpcResponse::error(id, error),
                                };
                                let _ = reply.send(response);
                                break;
                            }
                        })
                        .detach();
                    }
                }
                Err(error) => {
                    let _ = command.reply.send(crate::ipc::IpcResponse::error(
                        request.id,
                        error.to_string(),
                    ));
                }
            }
            return;
        }
        let before_ids: HashSet<u64> = self.tabs.iter().map(|tab| tab.id).collect();
        let mut errors = Vec::new();
        for path in &request.files {
            if !self.open_path(path, window, cx) {
                errors.push(format!("cannot open {}", path.display()));
            }
        }
        if let Some(goto) = &request.goto {
            if crate::ipc::path_is_readable(&goto.path) {
                self.open_path_at(&goto.path, goto.line, goto.column, 0, window, cx);
            } else {
                errors.push(format!("cannot open {}", goto.path.display()));
            }
        }
        if let Some((left, right)) = &request.diff {
            if !self.open_diff(left, right, window, cx) {
                errors.push("cannot open diff".to_string());
            }
        }
        if let Some(text) = &request.stdin {
            self.open_stdin(text.clone(), window, cx);
        }
        if let Some(command_id) = &request.command {
            if !self.run_command(command_id, window, cx) {
                errors.push(format!("command '{command_id}' failed"));
            }
        }
        let mut tab_ids: Vec<u64> = self
            .tabs
            .iter()
            .filter(|tab| !before_ids.contains(&tab.id))
            .map(|tab| tab.id)
            .collect();
        for path in request
            .files
            .iter()
            .chain(request.goto.iter().map(|goto| &goto.path))
        {
            if let Some(tab) = self.tabs.iter().find(|tab| {
                tab.doc
                    .path
                    .as_ref()
                    .is_some_and(|open| util::same_path(open, path))
            }) {
                if !tab_ids.contains(&tab.id) {
                    tab_ids.push(tab.id);
                }
            }
        }
        let ok = errors.is_empty();
        if ok && request.wait && !tab_ids.is_empty() {
            self.pending_waits.push(PendingWait {
                id: request.id,
                tab_ids,
                reply: command.reply,
            });
        } else {
            let response = if ok {
                crate::ipc::IpcResponse::ok(request.id)
            } else {
                crate::ipc::IpcResponse::error(request.id, errors.join("; "))
            };
            let _ = command.reply.send(response);
        }
        cx.notify();
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

    fn ensure_active_text(&mut self, cx: &mut Context<Self>) -> bool {
        if self.active_doc().is_some_and(|document| document.read_only) {
            self.set_status(tr("status.document_read_only"), cx);
            return false;
        }
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

    /// Select a find result, centring its line when the match is outside the
    /// visible rows. Matches already on screen keep the viewport where it is,
    /// so repeated Find Next does not scroll under the user; off-screen
    /// matches land in the middle of the editor like Notepad--.
    fn select_find_char_range(
        &mut self,
        start: usize,
        end: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let editor = tab.editor.clone();
        let centered = {
            let state = editor.read(cx);
            let text = state.text();
            let total = text.len_chars();
            let (start, end) = if start <= end {
                (start.min(total), end.min(total))
            } else {
                (end.min(total), start.min(total))
            };
            let begin = text.offset_to_position(text.char_to_byte_idx(start));
            // A match that ends at a line boundary ends *after* the newline;
            // measure the last character so the trailing newline does not
            // count as a row below the visible area.
            let last_char = if end > start { end - 1 } else { start };
            let finish = text.offset_to_position(text.char_to_byte_idx(last_char));
            let line_height = state.line_height().map(|height| height.as_f32());
            state
                .visible_row_range()
                .zip(line_height)
                .and_then(|(visible, line_height)| {
                    if line_height <= 0.0 {
                        return None;
                    }
                    let map = state.display_map();
                    let first = map
                        .buffer_pos_to_display_pos(BufferPoint::new(
                            begin.line as usize,
                            begin.character as usize,
                        ))
                        .row;
                    let last = map
                        .buffer_pos_to_display_pos(BufferPoint::new(
                            finish.line as usize,
                            finish.character as usize,
                        ))
                        .row;
                    if first >= visible.start && last < visible.end {
                        // The whole match is on screen: reveal nothing.
                        return None;
                    }
                    let height = state.input_bounds().size.height.as_f32();
                    let rows = (height / line_height).floor().max(1.0);
                    let top = (first as f32 - rows / 2.0).max(0.0);
                    Some(point(state.scroll_offset().x, px(-(top * line_height))))
                })
        };
        self.select_char_range(start, end, window, cx);
        if let Some(offset) = centered {
            editor.update(cx, |state, cx| state.set_scroll_offset(offset, cx));
        }
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
        if self.tabs[index].doc.read_only {
            self.set_status(tr("status.document_read_only"), cx);
            return;
        }
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

    // ------------------------------------------------------------------
    // File operations
    // ------------------------------------------------------------------

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

    fn set_language_use_spaces(&mut self, value: Option<bool>, cx: &mut Context<Self>) {
        self.update_language_override(|entry| entry.use_spaces = value);
        self.apply_language_indent_to_open_tabs(cx);
        cx.notify();
    }

    fn set_language_eol(&mut self, value: Option<Eol>, cx: &mut Context<Self>) {
        self.update_language_override(|entry| entry.eol = value);
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
        if self.tabs[index].doc.read_only {
            self.set_status(tr("status.document_read_only"), cx);
            self.abort_pending_saves();
            return;
        }
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
        // First save of an untitled document and every Save As land here; the
        // snapshot reflects the just-saved tab.
        if let Some(snapshot) = self.plugin_tab_snapshot(index, cx) {
            self.plugin_emit(plugin_events::PluginEvent::DocumentSaved(snapshot));
        }
        self.finish_save_flow(request.then, window, cx);
        cx.notify();
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
        self.plugin_host
            .set_locale(crate::i18n::active_locale().as_str());
        self.refresh_open_plugin_panel(window, cx);
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
        self.plugin_host
            .set_locale(crate::i18n::active_locale().as_str());
        self.refresh_open_plugin_panel(window, cx);
        self.apply_settings_impl(window, cx, false);
        self.set_status(tr("settings.reset_done"), cx);
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

    // ------------------------------------------------------------------
    // Close / quit
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
        if tab.doc.read_only {
            self.set_status(tr("status.document_read_only"), cx);
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
        if self.active_doc().is_some_and(|document| document.read_only) {
            self.set_status(tr("status.document_read_only"), cx);
            return;
        }
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
        if self.active_doc().is_some_and(|document| document.read_only) {
            self.set_status(tr("status.document_read_only"), cx);
            return;
        }
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
        self.rebuild_keymap();
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
            (self.file_search_input.clone(), "dialog.search_placeholder"),
            (
                self.file_filter_input.clone(),
                "dialog.filter_custom_placeholder",
            ),
            (
                self.file_new_folder_input.clone(),
                "dialog.new_folder_placeholder",
            ),
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
        self.plugin_host
            .set_locale(crate::i18n::active_locale().as_str());
        self.refresh_open_plugin_panel(window, cx);
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

    // ------------------------------------------------------------------
    // Find in files
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Decorations
    // ------------------------------------------------------------------

    fn refresh_decorations(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.get(self.active).map(|_| self.active) else {
            return;
        };
        let large = self.tabs[index].large_file;
        self.refresh_mark_highlights(index, cx);
        let key = {
            let tab = &self.tabs[index];
            (
                tab.doc.revision(),
                tab.doc.cursor,
                self.find.generation,
                self.find.open,
                self.settings.highlight_matching_brackets,
                large,
                tab.mark_generation,
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
        // Marks stay visible after the floating window is closed, like
        // Notepad--'s Mark tab. Find matches paint on top of them.
        let marks = if !large {
            self.tabs[index]
                .mark
                .as_ref()
                .map(|mark| mark.matches.clone())
                .unwrap_or_default()
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
            for matched in &marks {
                let b0 = rope.char_to_byte_idx(matched.start.min(total));
                let b1 = rope.char_to_byte_idx(matched.end.min(total));
                if b0 == b1 {
                    continue;
                }
                decorations.push(
                    RangeDecoration::new(b0..b1)
                        .with_style(RangeDecorationStyle::Fill)
                        .with_color(rgba(0x3FB95044).into()),
                );
            }
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

    /// Recompute a document's persistent marks when its text changed. Marks
    /// keep following edits instead of pointing at stale character offsets.
    fn refresh_mark_highlights(&mut self, index: usize, _cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(mark) = &tab.mark else {
            return;
        };
        let revision = tab.doc.revision();
        if mark.revision == revision {
            return;
        }
        let unsupported = tab.large_file
            || tab.doc.is_huge()
            || tab.doc.is_non_text()
            || tab.doc.partial_range().is_some();
        let matches = if unsupported {
            Vec::new()
        } else {
            let engine = luban_core::search::SearchEngine::new(
                mark.query.clone(),
                SearchOptions {
                    in_selection: false,
                    ..mark.options
                },
            );
            engine.find_all(&tab.doc.text, None, MAX_HIGHLIGHTS)
        };
        if let Some(tab) = self.tabs.get_mut(index) {
            if let Some(mark) = tab.mark.as_mut() {
                mark.matches = matches;
                mark.revision = revision;
            }
        }
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
        if self.pending_new_folder_create {
            self.pending_new_folder_create = false;
            self.create_file_picker_folder(window, cx);
        }
        if self.pending_file_picker_address {
            self.pending_file_picker_address = false;
            self.file_picker_apply_address(window, cx);
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

    pub fn open_stdin(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let mut doc = Document::new_untitled("stdin");
        doc.attach_text(eol::to_lf_owned(text));
        doc.language = language::PLAIN_TEXT.to_string();
        doc.touch();
        self.add_document(doc, window, cx);
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
        let plugin_section = command_groups.len();
        let plugin_commands: Vec<&'static str> = self
            .plugin_host
            .commands
            .iter()
            .map(|command| command.id)
            .collect();
        let state = cx.new(|cx| CommandState::new(window, cx));
        state.update(cx, |state, cx| state.focus(window, cx));
        self.palette = Some(PaletteOverlay {
            kind: PaletteKind::Commands,
            state,
            command_groups,
            plugin_commands,
            plugin_section,
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
            plugin_commands: Vec::new(),
            plugin_section: 0,
        });
        cx.notify();
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
        // The palette is gone. Hand focus back to the editor before running the
        // selection so commands that do not open a focused surface keep typing
        // in the document; surfaces opened below set their own focus.
        self.focus_editor(window, cx);
        match kind {
            PaletteKind::Commands => {
                let id = if index.section < overlay.plugin_section {
                    overlay
                        .command_groups
                        .get(index.section)
                        .and_then(|group| group.get(index.row))
                        .copied()
                } else if index.section == overlay.plugin_section {
                    overlay.plugin_commands.get(index.row).copied()
                } else {
                    None
                };
                if let Some(id) = id {
                    self.run_command(id, window, cx);
                }
            }
            PaletteKind::Snippets => {
                let row = self.snippet_picker_rows.get(index.row).cloned();
                self.snippet_picker_rows.clear();
                if let Some(row) = row {
                    self.insert_snippet(&row.snippet, window, cx);
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
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        self.quick.rows.clear();
        self.quick_query.clear();
        self.snippet_picker_rows.clear();
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
        if modifiers.secondary(cx) || modifiers.control {
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
        let dynamic = self.plugin_host.dynamic_specs();
        match commands::validate_override_with(&id, &text, &self.settings.keybindings, &dynamic) {
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

    // ------------------------------------------------------------------
    // Overlays
    // ------------------------------------------------------------------

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
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.sort_dialog = None;
                                this.focus_editor(window, cx);
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
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.filter_dialog = None;
                                this.focus_editor(window, cx);
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
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.column_editor.open = false;
                                this.column_editor.error = None;
                                this.focus_editor(window, cx);
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
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.macro_ui.open = false;
                            this.focus_editor(window, cx);
                            cx.notify();
                        })),
                ),
            )
            .into_any_element();
        self.overlay_card(560.0, Some(600.0), cx, content)
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
        let plugin_items: Vec<(String, bool)> = overlay
            .plugin_commands
            .iter()
            .filter_map(|id| self.plugin_host.command(id))
            .map(|command| (plugin_text(&command.label), command.when.enabled(&context)))
            .collect();
        let snippet_rows = self.snippet_picker_rows.clone();
        let weak_confirm = cx.weak_entity();
        let weak_cancel = cx.weak_entity();
        let weak_query = cx.weak_entity();
        let mut command = Command::new(&state)
            .bordered(false)
            .placeholder(match kind {
                PaletteKind::Commands => tr("command.palette.placeholder"),
                PaletteKind::QuickOpen => tr("command.quick_open.placeholder"),
                PaletteKind::Snippets => tr("snippet.picker_placeholder"),
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
                if !plugin_items.is_empty() {
                    let items = plugin_items.iter().map(|(label, enabled)| {
                        let item = CommandItem::new().label(label.clone());
                        if !enabled {
                            item.disabled(true)
                        } else {
                            item
                        }
                    });
                    command = command.group(
                        CommandGroup::new()
                            .label(tr("command.category.plugins"))
                            .items(items),
                    );
                }
            }
            PaletteKind::Snippets => {
                let items = snippet_rows.iter().map(|row| {
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
        let dynamic = self.plugin_host.dynamic_specs();
        let conflicts = commands::conflicts_with(&self.settings.keybindings, &dynamic);
        let overrides = self.settings.keybindings.clone();
        let keymap = &self.keymap;
        let mut rows: Vec<_> = commands::COMMANDS
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
        rows.extend(self.plugin_host.commands.iter().map(|command| {
            (
                command.id,
                plugin_text(&command.label),
                keymap.binding_text(command.id).unwrap_or("—").to_string(),
                overrides.get(command.id).cloned(),
            )
        }));
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
                                    .on_click(move |_, window, cx| {
                                        let _ = weak_close.update(cx, |this, cx| {
                                            this.shortcut_editor = None;
                                            this.focus_editor(window, cx);
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
        // The plugin panel is modeless: it stays above the editor but below
        // every modal dialog.
        if self.plugin_panel.is_some() {
            overlays.push(self.render_plugin_panel(window, cx));
        }
        if self.find.open {
            overlays.push(self.render_find_window(window, cx));
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
        if self.plugin_manager.open {
            overlays.push(self.render_plugin_manager(window, cx));
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
                            .on_click(move |_, window, cx| {
                                let _ = weak_cancel.update(cx, |this, cx| {
                                    this.go_to_line = None;
                                    this.focus_editor(window, cx);
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
                            this.focus_editor(window, cx);
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
                            .on_click(move |_, window, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.language_picker = None;
                                    this.focus_editor(window, cx);
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
        let plugin_rows: Vec<(String, String)> = self
            .plugin_host
            .commands
            .iter()
            .filter_map(|command| {
                self.keymap
                    .binding_text(command.id)
                    .map(|binding| (plugin_text(&command.label), binding.to_string()))
            })
            .collect();
        let sections = groups
            .into_iter()
            .map(|(category, specs)| {
                let rows: Vec<_> = specs
                    .iter()
                    .filter_map(|spec| {
                        let binding = self.keymap.binding_text(spec.id)?;
                        Some((tr(spec.label_key), binding.to_string()))
                    })
                    .collect();
                (tr(category.label_key()), rows)
            })
            .chain(std::iter::once((
                tr("command.category.plugins"),
                plugin_rows,
            )));
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
        if self
            .snippet_session
            .as_ref()
            .is_some_and(|session| Some(session.tab_id) != self.active_tab().map(|tab| tab.id))
        {
            self.snippet_session = None;
        }
        self.process_enter_actions(window, cx);
        self.sync_active(window, cx);
        self.process_ipc_requests(window, cx);
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
                window.is_fullscreen(),
            );
        }

        root.into_any_element()
    }
}

/// Width of the invisible strip along every window edge that starts a resize.
#[cfg(target_os = "linux")]
const CLIENT_RESIZE_BAND: Pixels = px(5.0);
/// Side length of the corner resize hotspots; larger than the edge bands so
/// diagonal resizing does not require pixel-perfect aim.
#[cfg(target_os = "linux")]
const CLIENT_RESIZE_CORNER: Pixels = px(8.0);

/// Wrap a manually decorated Linux window in a hairline border and add the
/// resize edges the window manager does not provide.
///
/// `_MOTIF_WM_HINTS` with `decorations = 0` makes KWin drop the native frame
/// entirely, including the invisible resize border it puts around decorated
/// windows, so dragging an edge does nothing on its own. The appended strips
/// start a window-manager resize through `_NET_WM_MOVERESIZE`, the same path
/// client-side decorations used before the hint was written pre-map. A
/// maximized window gets neither the border nor the strips; a fullscreen
/// window keeps the border but no strips, since a window manager ignores a
/// resize request while fullscreen.
#[cfg(target_os = "linux")]
fn client_decorated_frame(
    root: AnyElement,
    border: Hsla,
    maximized: bool,
    fullscreen: bool,
) -> AnyElement {
    if maximized {
        return root;
    }

    let mut frame = div().relative().size_full().overflow_hidden().child(
        div()
            .size_full()
            .overflow_hidden()
            .border_1()
            .border_color(border)
            .child(root),
    );
    if !fullscreen {
        for edge in [
            ResizeEdge::Top,
            ResizeEdge::Bottom,
            ResizeEdge::Left,
            ResizeEdge::Right,
            ResizeEdge::TopLeft,
            ResizeEdge::TopRight,
            ResizeEdge::BottomLeft,
            ResizeEdge::BottomRight,
        ] {
            frame = frame.child(client_resize_handle(edge));
        }
    }
    frame.into_any_element()
}

/// One invisible edge or corner that asks the window manager to resize the
/// borderless window. Corners are added after the edges, so they win the
/// overlap and resize on both axes.
#[cfg(target_os = "linux")]
fn client_resize_handle(edge: ResizeEdge) -> AnyElement {
    let hit = div().absolute().bg(transparent_black()).on_mouse_down(
        MouseButton::Left,
        move |_, window, cx| {
            if !window.is_resizable() {
                return;
            }
            window.start_window_resize(edge);
            cx.stop_propagation();
        },
    );

    match edge {
        ResizeEdge::Top => hit
            .top_0()
            .left_0()
            .right_0()
            .h(CLIENT_RESIZE_BAND)
            .cursor(CursorStyle::ResizeUpDown),
        ResizeEdge::Bottom => hit
            .bottom_0()
            .left_0()
            .right_0()
            .h(CLIENT_RESIZE_BAND)
            .cursor(CursorStyle::ResizeUpDown),
        ResizeEdge::Left => hit
            .left_0()
            .top_0()
            .bottom_0()
            .w(CLIENT_RESIZE_BAND)
            .cursor(CursorStyle::ResizeLeftRight),
        ResizeEdge::Right => hit
            .right_0()
            .top_0()
            .bottom_0()
            .w(CLIENT_RESIZE_BAND)
            .cursor(CursorStyle::ResizeLeftRight),
        ResizeEdge::TopLeft => hit
            .top_0()
            .left_0()
            .w(CLIENT_RESIZE_CORNER)
            .h(CLIENT_RESIZE_CORNER)
            .cursor(CursorStyle::ResizeUpLeftDownRight),
        ResizeEdge::TopRight => hit
            .top_0()
            .right_0()
            .w(CLIENT_RESIZE_CORNER)
            .h(CLIENT_RESIZE_CORNER)
            .cursor(CursorStyle::ResizeUpRightDownLeft),
        ResizeEdge::BottomLeft => hit
            .bottom_0()
            .left_0()
            .w(CLIENT_RESIZE_CORNER)
            .h(CLIENT_RESIZE_CORNER)
            .cursor(CursorStyle::ResizeUpRightDownLeft),
        ResizeEdge::BottomRight => hit
            .bottom_0()
            .right_0()
            .w(CLIENT_RESIZE_CORNER)
            .h(CLIENT_RESIZE_CORNER)
            .cursor(CursorStyle::ResizeUpLeftDownRight),
    }
    .into_any_element()
}

/// One config directory for every `#[gpui::test]` in this binary:
/// `LUBAN_NOTEPAD_CONFIG_DIR` is process-wide, so the tests have to agree on
/// its value instead of each installing its own.
#[cfg(test)]
fn test_config_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("test config directory");
        // SAFETY: set once, while this `OnceLock` initializes, before any test
        // creates a window or loads settings.
        unsafe { std::env::set_var("LUBAN_NOTEPAD_CONFIG_DIR", dir.path()) };
        dir
    })
    .path()
}

/// Serializes `#[gpui::test]`s that share the config directory, so one test's
/// `settings.json` write cannot land between another test's save and assert.
#[cfg(test)]
fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod focus_tests {
    use super::{
        test_config_dir, test_lock, LaunchOptions, LubanApp, PaletteKind, PaletteOverlay,
        WindowInit,
    };
    use gpui::TestAppContext;
    use gpui_kit::component::command::CommandState;
    use gpui_kit::component::IndexPath;
    use gpui_kit::{AppContext, Focusable};

    fn options() -> LaunchOptions {
        LaunchOptions {
            files: Vec::new(),
            smoke_test: false,
            safe_mode: true,
            session_writer: false,
            wait: false,
            goto: None,
            diff: None,
            stdin: None,
            command: None,
            exit_on_command_failure: false,
            command_result: None,
            ipc_queue: None,
            manual_window_chrome: false,
            recoveries: Vec::new(),
            init: WindowInit::Blank,
            auto_recovery: false,
            window_id: 1,
        }
    }

    #[gpui::test]
    fn palette_commands_and_escape_restore_editor_focus(cx: &mut TestAppContext) {
        let _guard = test_lock();
        test_config_dir();
        cx.update(gpui_kit::init);
        let (app, cx) = cx.add_window_view(|window, cx| LubanApp::new(window, cx, options()));
        for (command, opened, closed) in [
            (
                "settings.preferences",
                (|app: &LubanApp| app.preferences.open) as fn(&LubanApp) -> bool,
                (|app: &LubanApp| !app.preferences.open) as fn(&LubanApp) -> bool,
            ),
            (
                "plugins.manage",
                (|app: &LubanApp| app.plugin_manager.open) as fn(&LubanApp) -> bool,
                (|app: &LubanApp| !app.plugin_manager.open) as fn(&LubanApp) -> bool,
            ),
        ] {
            cx.update(|window, cx| {
                app.update(cx, |app, cx| {
                    app.focus_editor(window, cx);
                    let state = cx.new(|cx| CommandState::new(window, cx));
                    app.palette = Some(PaletteOverlay {
                        kind: PaletteKind::Commands,
                        state,
                        command_groups: vec![vec![command]],
                        plugin_commands: Vec::new(),
                        plugin_section: 1,
                    });
                    app.confirm_palette(PaletteKind::Commands, IndexPath::new(0), window, cx);
                });
            });
            cx.update(|_, cx| {
                let app = app.read(cx);
                assert!(opened(app), "{command} did not open");
                assert!(app.palette.is_none());
            });

            cx.simulate_keystrokes("escape");

            cx.update(|window, cx| {
                let app = app.read(cx);
                assert!(closed(app), "{command} did not close");
                let editor = app.tabs.get(app.active).expect("active tab").editor.clone();
                assert!(
                    editor.read(cx).focus_handle(cx).is_focused(window),
                    "{command} did not restore editor focus"
                );
            });
        }
    }
}

#[cfg(test)]
mod plugin_roots_tests {
    use super::LubanApp;

    #[test]
    fn workspace_roots_exclude_documents_outside_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let roots = LubanApp::plugin_allowed_roots_for(
            None,
            &[workspace.path().to_path_buf()],
            Some(outside.path()),
        );
        assert_eq!(roots, vec![workspace.path().canonicalize().unwrap()]);
    }

    #[test]
    fn an_open_document_supplies_a_root_without_a_workspace() {
        let outside = tempfile::tempdir().unwrap();
        let roots = LubanApp::plugin_allowed_roots_for(None, &[], Some(outside.path()));
        assert_eq!(roots, vec![outside.path().canonicalize().unwrap()]);
    }
}

#[cfg(test)]
mod tab_ops_tests {
    use super::count_words;
    use super::tabs::{reorder_index, trim_queue};

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
    use super::session::RestoreSpec;
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

#[cfg(test)]
mod plugin_panel_tests {
    use super::{
        fit_plugin_panel_width, plugin_panel_drag_width, plugin_panel_max_width,
        plugin_panel_open_width, test_config_dir, test_lock, LaunchOptions, LubanApp,
        PluginPanelState, WindowInit, PLUGIN_PANEL_DEFAULT_WIDTH, PLUGIN_PANEL_HANDLE_WIDTH,
        PLUGIN_PANEL_MAX_WIDTH, PLUGIN_PANEL_MIN_WIDTH,
    };
    use gpui::{point, px, Modifiers, MouseButton, ScrollHandle, TestAppContext};
    use luban_core::settings::Settings;

    fn options() -> LaunchOptions {
        LaunchOptions {
            files: Vec::new(),
            smoke_test: false,
            safe_mode: true,
            session_writer: false,
            wait: false,
            goto: None,
            diff: None,
            stdin: None,
            command: None,
            exit_on_command_failure: false,
            command_result: None,
            ipc_queue: None,
            manual_window_chrome: false,
            recoveries: Vec::new(),
            init: WindowInit::Blank,
            auto_recovery: false,
            window_id: 1,
        }
    }

    /// Open a right-side panel the way the plugin host would, without loading
    /// a plugin.
    fn open_panel(app: &mut LubanApp, cx: &mut gpui::Context<LubanApp>, width: f32) {
        app.plugin_panel = Some(PluginPanelState {
            plugin_id: "markdown.preview".to_string(),
            panel_id: "markdown.preview.panel".to_string(),
            title: luban_plugin::localized::LocalizedText::new("Preview"),
            placement: luban_plugin::manifest::PanelPlacement::Right,
            width,
            lines: Vec::new(),
            actions: Vec::new(),
            error: None,
            truncated: false,
            watch: 1,
            scroll: ScrollHandle::default(),
        });
        cx.notify();
    }

    /// Width bounds keep the panel inside the manifest range and leave the
    /// editor its reserve, even when the window cannot satisfy both.
    #[test]
    fn width_bounds_clamp_to_window_and_manifest() {
        assert_eq!(plugin_panel_max_width(1600.0), 1200.0);
        assert_eq!(plugin_panel_max_width(1000.0), 680.0);
        assert_eq!(plugin_panel_max_width(500.0), 280.0);
        assert_eq!(fit_plugin_panel_width(640.0, 1600.0), 640.0);
        assert_eq!(fit_plugin_panel_width(900.0, 1000.0), 680.0);
        assert_eq!(fit_plugin_panel_width(120.0, 1600.0), 280.0);
        assert_eq!(fit_plugin_panel_width(900.0, 400.0), 280.0);
    }

    /// A drag stores whole points even when the pointer lands between pixels,
    /// and still respects both bounds.
    #[test]
    fn drag_width_rounds_and_clamps() {
        assert_eq!(plugin_panel_drag_width(640.0, 802.0, 701.6, 1280.0), 740.0);
        assert_eq!(plugin_panel_drag_width(640.0, 802.0, 700.4, 1280.0), 742.0);
        assert_eq!(plugin_panel_drag_width(640.0, 100.0, 900.0, 1280.0), 280.0);
        assert_eq!(plugin_panel_drag_width(640.0, 900.0, 100.0, 1280.0), 960.0);
        assert_eq!(plugin_panel_drag_width(640.0, 900.0, 100.0, 1600.0), 1200.0);
    }

    /// Dragging the panel edge left widens the panel, and the width is
    /// remembered for that panel and restored the next time it opens.
    #[gpui::test]
    fn dragging_the_edge_resizes_and_remembers_the_width(cx: &mut TestAppContext) {
        let _guard = test_lock();
        test_config_dir();
        cx.update(gpui_kit::init);
        let (app, cx) = cx.add_window_view(|window, cx| LubanApp::new(window, cx, options()));
        cx.update(|window, cx| {
            app.update(cx, |app, cx| open_panel(app, cx, 640.0));
            window.draw(cx).clear(cx);
        });

        let bounds = cx.update(|window, _| window.bounds());
        let right = bounds.origin.x + bounds.size.width;
        let edge = right - px(640.0);
        let middle = bounds.origin.y + bounds.size.height / 2.0;

        // Grab the band that straddles the panel's left edge.
        let grab = point(edge, middle);
        assert!(
            grab.x > edge - px(PLUGIN_PANEL_HANDLE_WIDTH / 2.0),
            "handle band starts at the panel edge"
        );
        cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point(grab.x - px(100.0), middle),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| app.read(cx).plugin_panel.as_ref().unwrap().width),
            740.0,
            "dragging left widens the panel while the edge is held"
        );

        cx.simulate_mouse_up(
            point(grab.x - px(100.0), middle),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| app
                .read(cx)
                .settings
                .plugin_panel_widths
                .get("markdown.preview/markdown.preview.panel")
                .copied()),
            Some(740.0),
            "the dragged width is remembered for this panel"
        );

        // The width is written to settings.json, so it survives a restart.
        let reloaded = Settings::load_from(&Settings::path());
        assert_eq!(
            reloaded
                .plugin_panel_widths
                .get("markdown.preview/markdown.preview.panel")
                .copied(),
            Some(740.0),
            "the dragged width is persisted"
        );
        // Opening the same panel again resolves to the remembered width, and
        // other panels keep their own default.
        assert_eq!(
            plugin_panel_open_width(
                &reloaded,
                "markdown.preview",
                "markdown.preview.panel",
                Some(640)
            ),
            740.0
        );
        assert_eq!(
            plugin_panel_open_width(&reloaded, "other.plugin", "other.plugin.panel", Some(500)),
            500.0,
            "other panels are unaffected"
        );
    }

    /// First open uses the manifest width, falling back to 380 points.
    #[test]
    fn first_open_uses_the_manifest_width() {
        let settings = Settings::default();
        assert_eq!(
            plugin_panel_open_width(&settings, "a", "b", None),
            PLUGIN_PANEL_DEFAULT_WIDTH
        );
        assert_eq!(
            plugin_panel_open_width(&settings, "a", "b", Some(640)),
            640.0
        );
        assert_eq!(
            plugin_panel_open_width(&settings, "a", "b", Some(2000)),
            PLUGIN_PANEL_MAX_WIDTH,
            "manifest widths never exceed the draggable range"
        );
        assert_eq!(
            plugin_panel_open_width(&settings, "a", "b", Some(10)),
            PLUGIN_PANEL_MIN_WIDTH
        );
    }

    /// A panel without a remembered width starts at its manifest width, and
    /// dragging cannot push it past the editor reserve.
    #[gpui::test]
    fn dragging_stops_at_the_editor_reserve(cx: &mut TestAppContext) {
        let _guard = test_lock();
        test_config_dir();
        cx.update(gpui_kit::init);
        let (app, cx) = cx.add_window_view(|window, cx| LubanApp::new(window, cx, options()));
        cx.update(|window, cx| {
            app.update(cx, |app, cx| open_panel(app, cx, 640.0));
            window.draw(cx).clear(cx);
        });

        let bounds = cx.update(|window, _| window.bounds());
        let right = bounds.origin.x + bounds.size.width;
        let edge = right - px(640.0);
        let middle = bounds.origin.y + bounds.size.height / 2.0;
        cx.simulate_mouse_down(point(edge, middle), MouseButton::Left, Modifiers::none());
        // Drag far past the widest the window can give the panel.
        cx.simulate_mouse_move(
            point(bounds.origin.x, middle),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| app.read(cx).plugin_panel.as_ref().unwrap().width),
            plugin_panel_max_width(bounds.size.width.as_f32()),
            "the panel never takes the editor's reserve"
        );
    }

    /// Releasing the button outside the window drops its mouse-up; the next
    /// move has to finish the drag instead of leaving the edge highlighted.
    #[gpui::test]
    fn a_move_without_the_button_ends_a_lost_drag(cx: &mut TestAppContext) {
        let _guard = test_lock();
        test_config_dir();
        cx.update(gpui_kit::init);
        let (app, cx) = cx.add_window_view(|window, cx| LubanApp::new(window, cx, options()));
        cx.update(|window, cx| {
            app.update(cx, |app, cx| open_panel(app, cx, 640.0));
            window.draw(cx).clear(cx);
        });

        let bounds = cx.update(|window, _| window.bounds());
        let edge = bounds.origin.x + bounds.size.width - px(640.0);
        let middle = bounds.origin.y + bounds.size.height / 2.0;
        cx.simulate_mouse_down(point(edge, middle), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point(edge - px(60.0), middle),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        assert!(cx.update(|_, cx| app.read(cx).plugin_panel_drag.is_some()));

        // The release happened outside the window, so only this button-less
        // move tells the app the gesture is over.
        cx.simulate_mouse_move(point(edge - px(60.0), middle), None, Modifiers::none());
        cx.run_until_parked();
        assert!(
            cx.update(|_, cx| app.read(cx).plugin_panel_drag.is_none()),
            "a move without the button ends the drag"
        );
        assert_eq!(
            cx.update(|_, cx| app
                .read(cx)
                .settings
                .plugin_panel_widths
                .get("markdown.preview/markdown.preview.panel")
                .copied()),
            Some(700.0),
            "the width dragged before the lost release is still saved"
        );
    }
}

#[cfg(test)]
mod find_selection_tests {
    use super::{test_config_dir, test_lock, LaunchOptions, LubanApp, WindowInit};
    use gpui::TestAppContext;
    use gpui_kit::component::input::RopeExt as _;
    use luban_core::document::Document;
    use luban_core::encoding::TextEncoding;
    use luban_core::eol::Eol;
    use luban_core::language;

    fn options() -> LaunchOptions {
        LaunchOptions {
            files: Vec::new(),
            smoke_test: false,
            safe_mode: true,
            session_writer: false,
            wait: false,
            goto: None,
            diff: None,
            stdin: None,
            command: None,
            exit_on_command_failure: false,
            command_result: None,
            ipc_queue: None,
            manual_window_chrome: false,
            recoveries: Vec::new(),
            init: WindowInit::Blank,
            auto_recovery: false,
            window_id: 1,
        }
    }

    fn add_text(
        app: &mut LubanApp,
        text: &str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<LubanApp>,
    ) {
        let doc = Document::from_text(text, TextEncoding::Utf8, Eol::Lf, language::PLAIN_TEXT);
        app.add_document(doc, window, cx);
    }

    fn select_bytes(
        app: &LubanApp,
        range: std::ops::Range<usize>,
        cx: &mut gpui::Context<LubanApp>,
    ) {
        let editor = app.tabs[app.active].editor.clone();
        editor.update(cx, |state, cx| state.set_selected_range(range, cx));
    }

    #[gpui::test]
    fn opening_find_uses_the_selected_text_as_the_query(cx: &mut TestAppContext) {
        let _guard = test_lock();
        test_config_dir();
        cx.update(gpui_kit::init);
        let (app, cx) = cx.add_window_view(|window, cx| LubanApp::new(window, cx, options()));
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                add_text(app, "alpha beta gamma", window, cx);
                select_bytes(app, 6..10, cx);
                app.open_find(false, window, cx);
                assert_eq!(app.find.query_text(cx), "beta");
            });
        });
    }

    #[gpui::test]
    fn opening_find_without_a_selection_keeps_the_query(cx: &mut TestAppContext) {
        let _guard = test_lock();
        test_config_dir();
        cx.update(gpui_kit::init);
        let (app, cx) = cx.add_window_view(|window, cx| LubanApp::new(window, cx, options()));
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                add_text(app, "alpha beta gamma", window, cx);
                select_bytes(app, 0..0, cx);
                app.find.query.update(cx, |state, cx| {
                    state.set_value("kept", window, cx);
                });
                app.open_find(false, window, cx);
                assert_eq!(app.find.query_text(cx), "kept");
            });
        });
    }

    #[gpui::test]
    fn opening_find_ignores_an_oversized_selection(cx: &mut TestAppContext) {
        let _guard = test_lock();
        test_config_dir();
        cx.update(gpui_kit::init);
        let (app, cx) = cx.add_window_view(|window, cx| LubanApp::new(window, cx, options()));
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let text = "x".repeat(2048);
                add_text(app, &text, window, cx);
                select_bytes(app, 0..text.len(), cx);
                app.find.query.update(cx, |state, cx| {
                    state.set_value("kept", window, cx);
                });
                app.open_find(false, window, cx);
                assert_eq!(app.find.query_text(cx), "kept");
            });
        });
    }

    /// A match below the visible rows scrolls to the middle of the editor;
    /// the next match that is already on screen leaves the viewport alone.
    #[gpui::test]
    fn find_next_centres_an_offscreen_match(cx: &mut TestAppContext) {
        let _guard = test_lock();
        test_config_dir();
        cx.update(gpui_kit::init);
        let (app, cx) = cx.add_window_view(|window, cx| LubanApp::new(window, cx, options()));
        let text: String = (1..=120)
            .map(|line| {
                if line == 90 || line == 91 {
                    format!("line {line} needle\n")
                } else {
                    format!("line {line}\n")
                }
            })
            .collect();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                add_text(app, &text, window, cx);
                app.find.query.update(cx, |state, cx| {
                    state.set_value("needle", window, cx);
                });
                app.find.dirty = true;
            });
            window.draw(cx).clear(cx);
            app.update(cx, |app, cx| app.find_next(true, window, cx));
            window.draw(cx).clear(cx);
        });
        let first_offset = cx.update(|_, cx| {
            let app = app.read(cx);
            let state = app.tabs[app.active].editor.read(cx);
            let visible = state.visible_row_range().expect("laid-out viewport");
            assert!(visible.len() > 2, "the test editor is too short");
            let line = state
                .text()
                .offset_to_position(state.selected_range().start);
            let row = state
                .display_map()
                .buffer_pos_to_display_pos(gpui_kit::component::input::BufferPoint::new(
                    line.line as usize,
                    line.character as usize,
                ))
                .row;
            let center = (visible.start + visible.end) / 2;
            let distance = row.abs_diff(center);
            assert!(
                distance <= 2,
                "match row {row} is not centred in {visible:?}"
            );
            state.scroll_offset().y.as_f32()
        });
        // The next hit is one line away and already visible: finding it must
        // not move the viewport again.
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.find_next(true, window, cx));
            window.draw(cx).clear(cx);
        });
        cx.update(|_, cx| {
            let app = app.read(cx);
            let state = app.tabs[app.active].editor.read(cx);
            assert_eq!(
                state
                    .text()
                    .offset_to_position(state.selected_range().start)
                    .line,
                90,
                "the second hit should be on the following line"
            );
            assert!(
                (state.scroll_offset().y.as_f32() - first_offset).abs() <= 0.5,
                "an on-screen match moved the viewport"
            );
        });
    }
}

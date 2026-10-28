use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{
    Copy, Cut, Editor, EditorState, Indent, Input, InputEvent, InputState, Outdent, Paste,
    RangeDecoration, RangeDecorationCollection, RangeDecorationStyle, Redo, RopeExt, Search,
    SelectAll, TabSize, Undo,
};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::slider::SliderEvent;
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::toolbar::{Toolbar, ToolbarGroup};
use gpui_kit::component::{Selectable as _, Sizable as _, Size, TitleBar};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::*;
use luban_core::disk_engine::{DiskEngine, WindowText, WINDOW_BYTES};
use luban_core::document::{Document, PartialRange};
use luban_core::encoding::TextEncoding;
use luban_core::eol::{self, Eol};
use luban_core::i18n::language_display_name;
use luban_core::language;
use luban_core::large_file::{format_bytes, RangeMode, SizeUnit};
use luban_core::search::{ByteMatch, Match};
use luban_core::session::{Session, SessionTab};
use luban_core::settings::{Settings, ThemeChoice};
use luban_core::text as textutil;
use luban_core::undo::EditKind;
use luban_core::workspace::{
    list_directory, FileEntry, FileMatch, FindInFilesOptions, FindInFilesOutcome,
};

use crate::dialogs::{
    next_listing_generation, CloseIntent, ConfirmState, FileDialogMode, FileDialogPurpose,
    FileDialogState, FileFilter, GoToLineState, LanguagePickerState, SaveAsRequest, SaveThen,
};
use crate::explorer::ExplorerState;
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
use crate::preferences::PreferencesState;
use crate::theme::apply_theme;
use crate::util::{self, language_key};

pub struct LaunchOptions {
    pub files: Vec<PathBuf>,
    pub new_window: bool,
    pub smoke_test: bool,
    /// The platform has no client-decorated shadow margin to blend against, so
    /// the application removes the window manager titlebar itself and draws the
    /// chrome for every window state.
    pub manual_window_chrome: bool,
}

pub struct Tab {
    pub id: u64,
    pub doc: Document,
    pub editor: Entity<EditorState>,
    pub decorations: RangeDecorationCollection,
    pub needs_sync: bool,
    /// Present when the tab is backed by the disk engine.
    pub huge: Option<HugeFileUi>,
    pub highlighted_revision: u64,
    /// Files at or above `Settings::large_file_threshold` open in a reduced
    /// mode: no syntax highlighting, wrapping, bracket matching or live find.
    pub large_file: bool,
    decorations_key: Option<DecorationKey>,
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
                        if self.range.is_none() {
                            // Ordinary buffers are stored LF-normalized;
                            // non-text buffers are stored raw and must keep
                            // their bytes.
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
    pub large_file: LargeFilePanel,
    large_loads: Vec<LargeLoadJob>,
    next_load_generation: u64,
    pub preferences: PreferencesState,
    pub file_dialog: Option<FileDialogState>,
    file_path_input: Entity<InputState>,
    file_name_input: Entity<InputState>,
    pub confirm: Option<ConfirmState>,
    overwrite_save: Option<SaveAsRequest>,
    pub go_to_line: Option<GoToLineState>,
    pub language_picker: Option<LanguagePickerState>,
    pub about_open: bool,
    pub shortcuts_open: bool,
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
    pending_go_to_line: bool,
    pending_close_all_after_saves: bool,
    /// Huge tabs queued by Save All / Close All / Exit.
    pending_huge_saves: Vec<u64>,
    pending_huge_all: bool,
    pending_huge_exit: bool,
    /// A save waiting for the user to confirm overwriting an externally
    /// modified file.
    pending_huge_overwrite: Option<(u64, Option<PathBuf>, SaveThen)>,
    /// Line/column to apply once a disk-engine open started from a search hit
    /// has finished indexing.
    pending_huge_at: Option<(PathBuf, u64, u64)>,
    window_title: String,
    window_width: f32,
    window_height: f32,
    _subscriptions: Vec<Subscription>,
}

/// Maximum width of a recent-file menu label, counted in display columns
/// (one per half-width character, two per CJK or other wide character).
///
/// The popup menu caps its own width at 500px but does not clip item text, so
/// a full path would otherwise paint past the menu background. Counting
/// columns instead of chars keeps CJK-heavy paths within the same budget.
const RECENT_PATH_COLUMNS: usize = 48;

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

impl LubanApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, options: LaunchOptions) -> Self {
        let settings = Settings::load();
        crate::i18n::apply_language(&settings.ui_language);
        apply_theme(&settings, Some(window), cx);

        let mut app = Self {
            settings,
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 1,
            untitled_counter: 0,
            pending_opens: Vec::new(),
            pending_open_at: None,
            find: FindPanel::new(window, cx),
            find_in_files: FindInFilesPanel::new(window, cx),
            explorer: ExplorerState::new(),
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
            go_to_line: None,
            language_picker: None,
            about_open: false,
            shortcuts_open: false,
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
            pending_go_to_line: false,
            pending_close_all_after_saves: false,
            pending_huge_saves: Vec::new(),
            pending_huge_all: false,
            pending_huge_exit: false,
            pending_huge_overwrite: None,
            pending_huge_at: None,
            window_title: "Luban Notepad".to_string(),
            window_width: 1280.0,
            window_height: 820.0,
            _subscriptions: Vec::new(),
        };

        // The editor binds Ctrl+Z/Y and Ctrl+Home/End itself. Intercept them
        // before action dispatch so disk-engine tabs use the engine history
        // and the global document rather than the mounted window.
        let weak_shortcuts = cx.weak_entity();
        app._subscriptions
            .push(cx.intercept_keystrokes(move |event, window, cx| {
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
        app._subscriptions.push(
            cx.subscribe(&app.preferences.font_input, |this, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    if let InputEvent::Change = event {
                        let value = this.preferences.font_input.read(cx).value().to_string();
                        if value != this.settings.font_family {
                            cx.notify();
                        }
                    }
                }
            }),
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
        } else if !options.new_window && app.settings.restore_session {
            let session = Session::load();
            app.restore_session(session, window, cx);
        } else {
            app.new_file(window, cx);
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
                .update(cx, |app, _| {
                    app.autosave_session();
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
        let tab_size = self.settings.tab_size;
        let use_spaces = self.settings.use_spaces;

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
        });
        let decorations = editor.update(cx, |state, cx| {
            state.create_range_decorations_collection(Vec::new(), cx)
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
                tab.needs_sync = true;
                // Large documents never run the whole-buffer diff; the editor
                // event is enough to track the modified state.
                if tab.large_file && matches!(event, InputEvent::Change) {
                    tab.doc.touch();
                }
            }
            cx.notify();
        });

        // Wheel and keyboard scrolling only notify the editor entity. A huge
        // tab must re-render the app so the file-global scrollbar and gutter
        // are recomputed from the fresh scroll offset.
        let notification = cx.observe(&editor, |this, editor, cx| {
            let follows_editor = this
                .tabs
                .get(this.active)
                .is_some_and(|tab| tab.editor == editor && tab.huge.is_some());
            if follows_editor {
                cx.notify();
            }
        });

        (editor, decorations, subscription, notification)
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
        let (editor, decorations, subscription, notification) =
            self.new_editor(&mut doc, large_file, initial, huge_cursor, window, cx);
        self.tabs.push(Tab {
            id,
            doc,
            editor,
            decorations,
            needs_sync: false,
            huge: huge_ui,
            highlighted_revision: u64::MAX,
            large_file,
            decorations_key: None,
            _subscription: subscription,
            _notification: notification,
        });
        self.active = self.tabs.len() - 1;
        self.focus_editor = true;
        self.status.clear();
        self.find.clear();
        cx.notify();
    }

    pub fn new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.untitled_counter += 1;
        let mut doc = Document::new_untitled(tr_args(
            "untitled.name",
            &[("number", self.untitled_counter.to_string())],
        ));
        doc.encoding = self.settings.default_encoding;
        doc.eol = self.settings.default_eol;
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
            self.active = index;
            self.focus_editor = true;
            self.find.dirty = true;
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
                let _ = self.settings.save();
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
                let _ = self.settings.save();
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
                let _ = self.settings.save();
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
                let _ = self.settings.save();
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
            self.active = index;
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

    fn restore_session(&mut self, session: Session, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace_root = session.workspace_root.clone();
        self.explorer.set_root(session.workspace_root.clone());
        self.window_width = session.window_width;
        self.window_height = session.window_height;
        window.resize(size(px(self.window_width), px(self.window_height)));

        if session.tabs.is_empty() {
            self.new_file(window, cx);
            return;
        }

        let active = session.active_tab;
        let max_size = self.settings.max_file_size_bytes();
        let threshold = self.settings.large_file_threshold as u64;
        let mut any_large = false;
        let mut specs = Vec::with_capacity(session.tabs.len());
        for tab in session.tabs {
            let large = tab
                .path
                .as_ref()
                .and_then(|path| fs::metadata(path).ok())
                .map(|metadata| metadata.len() >= threshold)
                .unwrap_or(false);
            any_large |= large || tab.huge;
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
                path: tab.path,
                text: tab.text,
                encoding: tab.encoding,
                eol: tab.eol,
                language: tab.language,
                cursor: tab.cursor,
                anchor: tab.anchor,
                modified: tab.modified,
                fallback_name,
                range: tab.range,
                huge: tab.huge,
                huge_line: tab.huge_line,
                huge_column: tab.huge_column,
            });
        }

        if !any_large {
            for spec in specs {
                let doc = spec.load(max_size);
                self.add_document(doc, window, cx);
            }
            self.active = active.min(self.tabs.len().saturating_sub(1));
            self.focus_editor = true;
            cx.notify();
            return;
        }

        // A large tab in the session must not block startup.
        self.set_status(tr("status.restoring_session"), cx);
        let task = cx.background_executor().spawn(async move {
            specs
                .into_iter()
                .map(|spec| spec.load(max_size))
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let docs = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_restore(docs, active, window, cx);
            });
        })
        .detach();
    }

    fn finish_restore(
        &mut self,
        docs: Vec<Document>,
        active: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if docs.is_empty() {
            self.new_file(window, cx);
            return;
        }
        for doc in docs {
            self.add_document(doc, window, cx);
        }
        self.active = active.min(self.tabs.len().saturating_sub(1));
        self.focus_editor = true;
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
            tab.doc.set_text_from_widget(&normalized, cursor, anchor);
            tab.needs_sync = false;
            tab.highlighted_revision = u64::MAX;
        } else {
            tab.doc.set_selection(anchor, cursor);
            tab.needs_sync = false;
        }
        if changed {
            self.find.dirty = true;
        }

        if normalize_cr {
            let b0 = textutil::char_to_byte(&normalized, start);
            let b1 = textutil::char_to_byte(&normalized, end);
            let editor = tab.editor.clone();
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
            tab.needs_sync = false;
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
        self.tabs[index].needs_sync = false;
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
            self.set_status(tr("core.document.read_only"), cx);
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
                let _ = self.settings.save();
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
                if matches!(err, luban_core::Error::Cancelled) {
                    self.set_status(tr("status.save_cancelled"), cx);
                } else {
                    self.set_status(
                        tr_args("status.save_failed", &[("error", err.to_string())]),
                        cx,
                    );
                }
                self.pending_exit_saves.clear();
                self.pending_close_saves.clear();
                self.pending_huge_saves.clear();
                self.pending_huge_all = false;
                self.pending_huge_exit = false;
                if let Some(ui) = self.tabs[index].huge.as_mut() {
                    ui.queued_save_then = None;
                }
            }
        }
        cx.notify();
    }

    fn finish_save_flow(&mut self, then: SaveThen, window: &mut Window, cx: &mut Context<Self>) {
        match then {
            SaveThen::Nothing => {
                if !self.pending_huge_saves.is_empty() {
                    self.save_next_huge(window, cx);
                } else {
                    self.focus_editor(window, cx);
                }
            }
            SaveThen::NextHugeSave => {
                if self.pending_huge_saves.is_empty() {
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
                        self.set_status(tr("status.saved_modified"), cx);
                    }
                } else {
                    self.save_next_huge(window, cx);
                }
            }
            SaveThen::ReloadTab(id) => {
                if let Some(index) = self.tab_index_by_id(id) {
                    self.start_huge_reload(index, window, cx);
                }
            }
            SaveThen::CloseTab(id) => {
                if let Some(index) = self.tab_index_by_id(id) {
                    self.close_tab_at(index, window, cx);
                }
            }
            SaveThen::Exit => {
                if self.pending_exit_saves.is_empty() {
                    self.request_quit(window, cx);
                } else {
                    self.prompt_next_batch_save(window, cx);
                }
            }
            SaveThen::CloseAll => {
                if self.pending_close_saves.is_empty() {
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
            self.set_status(tr("status.saved_modified"), cx);
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
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs[index].doc.is_partial() {
            self.set_status(tr("status.save_partial_as"), cx);
            return;
        }
        if self.tabs[index].doc.is_huge() {
            self.start_huge_save(index, None, SaveThen::Nothing, false, window, cx);
            return;
        }
        if !self.ensure_tab_text(index, cx) {
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
            self.settings.add_recent(path);
        }
        let _ = self.settings.save();
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
                self.set_status(tr_args("status.save_failed", &[("error", err)]), cx)
            }
            (None, None) => {}
        }
        cx.notify();
    }

    pub fn save_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let mut skipped_partial = false;
        let mut huge = Vec::new();
        for index in 0..self.tabs.len() {
            if !self.tabs[index].doc.is_modified() || self.tabs[index].doc.path.is_none() {
                continue;
            }
            if self.tabs[index].doc.is_partial() {
                skipped_partial = true;
                continue;
            }
            if self.tabs[index].doc.is_huge() {
                huge.push(self.tabs[index].id);
                continue;
            }
            self.save_tab_at(index, window, cx);
        }
        if !huge.is_empty() {
            self.pending_huge_saves = huge;
            self.pending_huge_all = false;
            self.pending_huge_exit = false;
            self.save_next_huge(window, cx);
            return;
        }
        if skipped_partial {
            self.set_status(tr("status.save_partial_as"), cx);
        } else {
            self.set_status(tr("status.saved_modified"), cx);
        }
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
            return;
        }
        let lossy = match self.tabs[index].doc.save_as(&request.path) {
            Ok(lossy) => lossy,
            Err(err) => {
                self.release_tab_text(index);
                self.set_status(
                    tr_args("status.save_failed", &[("error", err.to_string())]),
                    cx,
                );
                self.pending_exit_saves.clear();
                self.pending_close_saves.clear();
                return;
            }
        };
        self.release_tab_text(index);
        self.settings.add_recent(request.path.clone());
        let _ = self.settings.save();
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
                    self.open_path(&candidate, window, cx);
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
                if let FileDialogPurpose::SaveAs { tab_id, then } = dialog.purpose {
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
                        self.workspace_root = Some(candidate.clone());
                        self.explorer.set_root(Some(candidate));
                        self.settings.show_file_explorer = true;
                        let _ = self.settings.save();
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
                tab.editor.update(cx, |state, cx| {
                    state.set_value(text, window, cx);
                    let total = state.text().len_chars();
                    let b0 = state.text().char_to_byte_idx(anchor.min(total));
                    let b1 = state.text().char_to_byte_idx(cursor.min(total));
                    state.set_selected_range(b0..b1, cx);
                });
                tab.needs_sync = false;
                self.set_status(tr("status.reloaded"), cx);
            }
            Err(err) => self.set_status(
                tr_args("status.reload_failed", &[("error", err.to_string())]),
                cx,
            ),
        }
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
        if index >= self.tabs.len() {
            return;
        }
        self.cancel_tab_jobs(index);
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = 0;
            self.new_file(window, cx);
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if self.active > index {
            self.active -= 1;
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
        for index in 0..self.tabs.len() {
            self.cancel_tab_jobs(index);
        }
        self.tabs.clear();
        self.active = 0;
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
            self.persist_state(window, cx);
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
            CloseIntent::ReloadTab(id) => {
                let Some(index) = self.tab_index_by_id(id) else {
                    return;
                };
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
                    self.save_tab_at(index, window, cx);
                    if !self.tabs[index].doc.is_modified() {
                        self.close_tab_at(index, window, cx);
                    }
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
                let ids = self.modified_tabs();
                let mut huge = Vec::new();
                for id in ids {
                    let Some(index) = self.tab_index_by_id(id) else {
                        continue;
                    };
                    if self.tabs[index].doc.is_huge() && !self.tabs[index].doc.is_partial() {
                        huge.push(id);
                    } else if self.tabs[index].doc.path.is_some()
                        && !self.tabs[index].doc.is_partial()
                    {
                        self.save_tab_at(index, window, cx);
                    } else {
                        self.pending_close_saves.push(id);
                    }
                }
                if !huge.is_empty() {
                    self.pending_huge_saves = huge;
                    self.pending_huge_all = true;
                    self.save_next_huge(window, cx);
                } else if self.pending_close_saves.is_empty() {
                    self.close_all_now(window, cx);
                } else {
                    self.pending_close_all_after_saves = true;
                    self.prompt_next_batch_save(window, cx);
                }
            }
            CloseIntent::Exit => {
                let ids = self.modified_tabs();
                let mut huge = Vec::new();
                for id in ids {
                    let Some(index) = self.tab_index_by_id(id) else {
                        continue;
                    };
                    if self.tabs[index].doc.is_huge() && !self.tabs[index].doc.is_partial() {
                        huge.push(id);
                    } else if self.tabs[index].doc.path.is_some()
                        && !self.tabs[index].doc.is_partial()
                    {
                        self.save_tab_at(index, window, cx);
                    } else {
                        self.pending_exit_saves.push(id);
                    }
                }
                if !huge.is_empty() {
                    self.pending_huge_saves = huge;
                    self.pending_huge_exit = true;
                    self.save_next_huge(window, cx);
                } else if self.pending_exit_saves.is_empty() {
                    self.request_quit(window, cx);
                } else {
                    self.prompt_next_batch_save(window, cx);
                }
            }
        }
    }

    fn confirm_discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        match confirm.intent {
            CloseIntent::OverwriteExternal(_) => {
                self.pending_huge_overwrite = None;
                self.set_status(tr("status.save_cancelled"), cx);
            }
            CloseIntent::ReloadTab(id) => {
                if let Some(index) = self.tab_index_by_id(id) {
                    self.start_huge_reload(index, window, cx);
                }
            }
            CloseIntent::Tab(id) => {
                if let Some(index) = self.tab_index_by_id(id) {
                    self.close_tab_at(index, window, cx);
                }
            }
            CloseIntent::AllTabs => self.close_all_now(window, cx),
            CloseIntent::Exit => self.request_quit(window, cx),
        }
    }

    fn confirm_cancel(&mut self, cx: &mut Context<Self>) {
        self.confirm = None;
        self.pending_exit_saves.clear();
        self.pending_close_saves.clear();
        self.pending_close_all_after_saves = false;
        self.pending_huge_saves.clear();
        self.pending_huge_all = false;
        self.pending_huge_exit = false;
        self.pending_huge_overwrite = None;
        cx.notify();
    }

    fn persist_state(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        let size = window.bounds().size;
        self.window_width = size.width.into();
        self.window_height = size.height.into();
        self.save_session_now();
        if let Err(err) = self.settings.save() {
            log::warn!("could not save settings: {err}");
        }
    }

    fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for index in 0..self.tabs.len() {
            self.cancel_tab_jobs(index);
        }
        self.persist_state(window, cx);
        self.allow_close = true;
        cx.quit();
    }

    // ------------------------------------------------------------------
    // Session
    // ------------------------------------------------------------------

    fn build_session(&self) -> Session {
        let tabs = self
            .tabs
            .iter()
            .map(|tab| {
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
                    // Large buffers are never written to the session: the file
                    // is restored from disk instead.
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
                }
            })
            .collect();
        Session {
            version: luban_core::session::SESSION_VERSION,
            tabs,
            active_tab: self.active,
            workspace_root: self.workspace_root.clone(),
            window_width: self.window_width,
            window_height: self.window_height,
        }
    }

    fn save_session_now(&self) {
        let session = self.build_session();
        match session.save() {
            Ok(()) => {}
            Err(err) => log::warn!("could not save session: {err}"),
        }
    }

    fn autosave_session(&mut self) {
        if !self.settings.autosave_session {
            return;
        }
        let elapsed = self.last_session_save.elapsed();
        if elapsed < Duration::from_secs(self.settings.session_interval_secs.max(5)) {
            return;
        }
        self.last_session_save = Instant::now();
        self.save_session_now();
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
        self.sync_active(window, cx);
        if !self.ensure_active_text(cx) {
            return;
        }
        let Some(tab) = self.active_tab() else { return };
        let (start, end) = tab.doc.selection();
        let (new_text, sel_start, sel_end) =
            textutil::sort_lines(&tab.doc.text, start, end, ascending, false);
        self.replace_document_text(new_text, sel_start, sel_end, window, cx);
        self.set_status(
            tr(if ascending {
                "status.sorted_asc"
            } else {
                "status.sorted_desc"
            }),
            cx,
        );
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
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        if !tab.doc.set_language(name.clone()) {
            return;
        }
        let key = language_key(&name).to_string();
        tab.highlighted_revision = u64::MAX;
        tab.decorations_key = None;
        if !tab.large_file {
            tab.editor
                .update(cx, |state, cx| state.set_highlighter(key, cx));
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
        let _ = self.settings.save();
        cx.notify();
    }

    pub fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.settings.font_size = 14.0;
        let _ = self.settings.save();
        cx.notify();
    }

    pub fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        apply_theme(&self.settings, Some(window), cx);
        let line_numbers = self.settings.show_line_numbers;
        let wrap = self.settings.word_wrap;
        let tab = TabSize {
            tab_size: self.settings.tab_size,
            hard_tabs: !self.settings.use_spaces,
        };
        for index in 0..self.tabs.len() {
            let wrap = !self.tabs[index].large_file && wrap;
            // Disk-engine tabs draw document-global numbers in the app.
            let line_numbers = line_numbers && !self.tabs[index].doc.is_huge();
            self.tabs[index].editor.update(cx, |state, cx| {
                state.set_line_number(line_numbers, window, cx);
                state.set_soft_wrap(wrap, window, cx);
                state.set_tab_size(tab, cx);
            });
        }
        if let Err(err) = self.settings.save() {
            log::warn!("could not save settings: {err}");
        }
        self.explorer.refresh();
        cx.notify();
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
            (
                self.preferences.font_input.clone(),
                "prefs.font_placeholder",
            ),
        ];
        for (input, key) in inputs {
            input.update(cx, |state, cx| {
                state.set_placeholder(tr(key), window, cx);
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
        if let Err(err) = self.settings.save() {
            log::warn!("could not save settings: {err}");
        }
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

    pub fn open_find_in_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find_in_files.open = true;
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
        let folder = self.find_in_files.folder_path(cx);
        if !folder.is_dir() {
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
        self.find_in_files.generation += 1;
        let generation = self.find_in_files.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.find_in_files.cancel = Some(cancel.clone());
        self.find_in_files.running = true;
        self.find_in_files.clear_results();
        self.find_in_files.status = tr("ff.searching");
        cx.notify();

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
                self.find_in_files.results = outcome.matches;
                self.find_in_files.truncated = outcome.truncated;
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
        let command = secondary || modifiers.control;
        if key == "escape" {
            if self.overwrite_save.is_some() {
                self.cancel_overwrite(cx);
                return true;
            }
            if self.file_dialog.is_some() {
                self.cancel_file_dialog(cx);
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
            if self.preferences.open {
                self.preferences.open = false;
                cx.notify();
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
            ("1", false) => self.activate_tab_index(0, window, cx),
            ("2", false) => self.activate_tab_index(1, window, cx),
            ("3", false) => self.activate_tab_index(2, window, cx),
            ("4", false) => self.activate_tab_index(3, window, cx),
            ("5", false) => self.activate_tab_index(4, window, cx),
            ("6", false) => self.activate_tab_index(5, window, cx),
            ("7", false) => self.activate_tab_index(6, window, cx),
            ("8", false) => self.activate_tab_index(7, window, cx),
            ("9", false) => self.activate_tab_index(8, window, cx),
            _ => return false,
        }
        true
    }

    fn activate_tab_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.tabs.len() {
            self.sync_active(window, cx);
            self.active = index;
            self.focus_editor = true;
            self.find.clear();
            self.find.dirty = true;
            cx.notify();
        }
    }

    fn next_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        self.sync_active(window, cx);
        let len = self.tabs.len() as isize;
        self.active = ((self.active as isize + delta).rem_euclid(len)) as usize;
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
        let (position, position_end) = self.line_selection_in_tab(index, line, column, 0, cx);
        let _ = position_end;
        self.select_char_range(position, position, window, cx);
        self.focus_editor = true;
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
                        tr("menu.file.new"),
                        false,
                        None,
                        |this, window, cx| {
                            this.new_file(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        tr("menu.file.open"),
                        false,
                        None,
                        |this, window, cx| {
                            this.open_file_dialog(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        tr("menu.file.open_folder"),
                        false,
                        None,
                        |this, window, cx| {
                            this.open_folder_dialog(window, cx);
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
                menu = menu
                    .separator()
                    .item(Self::menu_item(
                        &weak_file,
                        tr("menu.file.save"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.save_active(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        tr("menu.file.save_as"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.save_active_as(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        tr("menu.file.save_all"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.save_all(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        tr("menu.file.reload"),
                        !has_doc || (active_partial && !active_huge),
                        None,
                        |this, window, cx| {
                            this.reload_active(window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_file,
                        tr("menu.file.close"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.request_close_active(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_file,
                        tr("menu.file.close_all"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.request_close_all(window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_file,
                        tr("menu.file.exit"),
                        false,
                        None,
                        |this, window, cx| {
                            this.request_exit(window, cx);
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
                        tr("menu.edit.undo"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.undo_or_editor(false, window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.redo"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.undo_or_editor(true, window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.cut"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.editor_action(Box::new(Cut), window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.copy"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.editor_action(Box::new(Copy), window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.paste"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.editor_action(Box::new(Paste), window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.select_all"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.editor_action(Box::new(SelectAll), window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.indent"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.editor_action(Box::new(Indent), window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.outdent"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.editor_action(Box::new(Outdent), window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.duplicate_line"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.duplicate_line(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.delete_line"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.delete_line(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.toggle_comment"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.toggle_comment(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.trim_whitespace"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.trim_trailing(window, cx);
                        },
                    ))
                    .separator();
                menu = menu
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.uppercase"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.transform_case(textutil::CaseMode::Upper, window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.lowercase"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.transform_case(textutil::CaseMode::Lower, window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.title_case"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.transform_case(textutil::CaseMode::Title, window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.sort_asc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.sort_selection(true, window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_edit,
                        tr("menu.edit.sort_desc"),
                        !has_doc,
                        None,
                        |this, window, cx| {
                            this.sort_selection(false, window, cx);
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
                    tr("menu.search.find"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.open_find(false, window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    tr("menu.search.replace"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.open_find(true, window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    tr("menu.search.find_next"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.find_next(true, window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    tr("menu.search.find_previous"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.find_next(false, window, cx);
                    },
                ))
                .separator()
                .item(Self::menu_item(
                    &weak_search,
                    tr("menu.search.go_to_line"),
                    !has_doc,
                    None,
                    |this, window, cx| {
                        this.open_go_to_line(window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    tr("menu.search.find_in_files"),
                    false,
                    None,
                    |this, window, cx| {
                        this.open_find_in_files(window, cx);
                    },
                ))
                .item(Self::menu_item(
                    &weak_search,
                    tr("menu.search.clear_highlights"),
                    false,
                    None,
                    |this, _, cx| {
                        this.find.clear();
                        cx.notify();
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
                        tr("menu.view.word_wrap"),
                        false,
                        Some(settings.word_wrap),
                        |this, window, cx| {
                            this.settings.word_wrap = !this.settings.word_wrap;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        tr("menu.view.line_numbers"),
                        false,
                        Some(settings.show_line_numbers),
                        |this, window, cx| {
                            this.settings.show_line_numbers = !this.settings.show_line_numbers;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        tr("menu.view.toolbar"),
                        false,
                        Some(settings.show_toolbar),
                        |this, _window, cx| {
                            this.settings.show_toolbar = !this.settings.show_toolbar;
                            let _ = this.settings.save();
                            cx.notify();
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        tr("menu.view.status_bar"),
                        false,
                        Some(settings.show_status_bar),
                        |this, _window, cx| {
                            this.settings.show_status_bar = !this.settings.show_status_bar;
                            let _ = this.settings.save();
                            cx.notify();
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        tr("menu.view.explorer"),
                        false,
                        Some(settings.show_file_explorer),
                        |this, _window, cx| {
                            this.settings.show_file_explorer = !this.settings.show_file_explorer;
                            let _ = this.settings.save();
                            cx.notify();
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        tr("menu.view.large_file_controller"),
                        !has_doc,
                        Some(large_file_open),
                        |this, window, cx| {
                            this.toggle_large_file_controller(window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        tr("menu.view.zoom_in"),
                        !has_doc,
                        None,
                        |this, _, cx| this.zoom(1.0, cx),
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        tr("menu.view.zoom_out"),
                        !has_doc,
                        None,
                        |this, _, cx| this.zoom(-1.0, cx),
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        tr("menu.view.zoom_reset"),
                        !has_doc,
                        None,
                        |this, _, cx| this.reset_zoom(cx),
                    ))
                    .separator();
                menu = menu
                    .item(Self::menu_item(
                        &weak_view,
                        tr("menu.view.light_theme"),
                        false,
                        Some(settings.theme == ThemeChoice::Light),
                        |this, window, cx| {
                            this.settings.theme = ThemeChoice::Light;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        &weak_view,
                        tr("menu.view.dark_theme"),
                        false,
                        Some(settings.theme == ThemeChoice::Dark),
                        |this, window, cx| {
                            this.settings.theme = ThemeChoice::Dark;
                            this.apply_settings(window, cx);
                        },
                    ))
                    .separator()
                    .item(Self::menu_item(
                        &weak_view,
                        tr("menu.view.choose_language"),
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
                    tr("menu.settings.preferences"),
                    false,
                    None,
                    |this, window, cx| this.open_preferences(window, cx),
                ))
            });

        let help_menu = Button::new("menu-help")
            .label(tr("menu.help"))
            .ghost()
            .compact()
            .dropdown_menu(move |menu, _window, _cx| {
                menu.item(Self::menu_item(
                    &weak_help,
                    tr("menu.help.shortcuts"),
                    false,
                    None,
                    |this, _, cx| {
                        this.shortcuts_open = true;
                        cx.notify();
                    },
                ))
                .item(Self::menu_item(
                    &weak_help,
                    tr("menu.help.about"),
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
            (IconName::Moon, tr("menu.view.dark_theme"))
        } else {
            (IconName::Sun, tr("menu.view.light_theme"))
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
            tr("toolbar.new"),
            |this: &mut Self, window, cx| this.new_file(window, cx)
        );
        tool!(
            file_group,
            "tb-open",
            IconName::FolderOpen,
            tr("toolbar.open"),
            |this: &mut Self, window, cx| this.open_file_dialog(window, cx)
        );
        tool!(
            file_group,
            "tb-save",
            IconName::Save,
            tr("toolbar.save"),
            |this: &mut Self, window, cx| this.save_active(window, cx)
        );
        tool!(
            history_group,
            "tb-undo",
            IconName::Undo,
            tr("toolbar.undo"),
            |this: &mut Self, window, cx| this.editor_action(Box::new(Undo), window, cx)
        );
        tool!(
            history_group,
            "tb-redo",
            IconName::Redo,
            tr("toolbar.redo"),
            |this: &mut Self, window, cx| this.editor_action(Box::new(Redo), window, cx)
        );
        tool!(
            search_group,
            "tb-find",
            IconName::Search,
            tr("toolbar.find"),
            |this: &mut Self, window, cx| this.open_find(false, window, cx)
        );
        tool!(
            search_group,
            "tb-replace",
            IconName::Replace,
            tr("toolbar.replace"),
            |this: &mut Self, window, cx| this.open_find(true, window, cx)
        );
        tool!(
            search_group,
            "tb-findfiles",
            IconName::FolderSearch,
            tr("toolbar.find_in_files"),
            |this: &mut Self, window, cx| this.open_find_in_files(window, cx)
        );
        let weak_wrap = weak.clone();
        let wrap_label = tr("toolbar.wrap");
        view_group = view_group.child(
            Button::new("tb-wrap")
                .icon(IconName::TextWrap)
                .selected(wrap_enabled)
                .toggled(wrap_enabled)
                .tooltip(wrap_label.clone())
                .accessibility_label(wrap_label)
                .on_click(move |_, window, cx| {
                    let _ = weak_wrap.update(cx, |this, cx| {
                        this.settings.word_wrap = !this.settings.word_wrap;
                        this.apply_settings(window, cx);
                    });
                }),
        );
        tool!(
            view_group,
            "tb-theme",
            theme_icon,
            theme_label,
            |this: &mut Self, window, cx| {
                this.settings.theme = if this.settings.theme == ThemeChoice::Dark {
                    ThemeChoice::Light
                } else {
                    ThemeChoice::Dark
                };
                this.apply_settings(window, cx);
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

    fn render_tabs(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let mut tabs_row = div()
            .flex()
            .items_center()
            .h(px(34.))
            .border_b_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.background);
        let mut tab_elements = Vec::new();
        for (index, tab) in self.tabs.iter().enumerate() {
            let active = index == self.active;
            let id = tab.id;
            let label = if tab.doc.is_modified() {
                format!("● {}", tab.doc.display_name())
            } else {
                tab.doc.display_name()
            };
            let weak_click = weak.clone();
            let weak_close = weak.clone();
            let interaction_id = ("tab", id);
            let mut tab_el = div()
                .id(interaction_id)
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .h_full()
                .border_r_1()
                .border_color(cx.theme().colors.border)
                .cursor_pointer()
                .when(active, |this| this.bg(cx.theme().colors.secondary))
                .hover(|this| this.bg(cx.theme().colors.secondary_hover))
                .child(
                    div()
                        .text_sm()
                        .when(active, |this| this.font_weight(FontWeight::MEDIUM))
                        .child(label),
                )
                .child(
                    div()
                        .id(format!("tab-close:{id}"))
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
            tab_el = tab_el.on_click(move |_, window, cx| {
                let _ = weak_click.update(cx, |this, cx| {
                    this.activate_tab_index(index, window, cx);
                });
            });
            tab_elements.push(tab_el.into_any_element());
        }
        tabs_row = tabs_row.children(tab_elements);
        let weak_new = weak.clone();
        tabs_row = tabs_row.child(
            div()
                .id("tab-new")
                .px_3()
                .h_full()
                .flex()
                .items_center()
                .cursor_pointer()
                .text_lg()
                .hover(|this| this.bg(cx.theme().colors.secondary))
                .child("+")
                .on_click(move |_, window, cx| {
                    let _ = weak_new.update(cx, |this, cx| this.new_file(window, cx));
                }),
        );
        tabs_row.into_any_element()
    }

    fn render_status_bar(&self, cx: &gpui_kit::App) -> AnyElement {
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

    fn render_editor_area(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let Some(tab) = self.tabs.get(self.active) else {
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

        div()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .relative()
                    .flex_1()
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
                    .children(global_scrollbar),
            )
            .into_any_element()
    }

    fn render_explorer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let width = self.explorer.width;
        let show_hidden = self.settings.show_hidden_files;
        let mut panel = div()
            .w(px(width))
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.background);
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
                .child(tr("explorer.title"))
                .child(
                    div()
                        .id("explorer-open-folder")
                        .px_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().colors.secondary))
                        .child(tr("explorer.open_folder"))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.open_folder_dialog(window, cx);
                        })),
                ),
        );
        if self.explorer.root.is_none() {
            panel = panel.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr("explorer.empty")),
            );
        } else if let Some(root) = self.explorer.root.clone() {
            let root_name = self.explorer.root_name().unwrap_or_default();
            let weak_root = weak.clone();
            let expanded = self.explorer.is_expanded(&root);
            panel = panel.child(
                div()
                    .id("explorer-root")
                    .flex()
                    .items_center()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().colors.secondary))
                    .child(if expanded { "▾" } else { "▸" })
                    .child(div().text_sm().child(root_name))
                    .on_click(move |_, _, cx| {
                        let _ = weak_root.update(cx, |this, cx| {
                            this.explorer.toggle_directory(
                                &this.explorer.root.clone().unwrap_or_default(),
                                this.settings.show_hidden_files,
                            );
                            cx.notify();
                        });
                    }),
            );
            let children = self.render_explorer_children(&root, 1, show_hidden, &weak, cx);
            panel = panel.child(
                div()
                    .id("explorer-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(children),
            );
            let _ = window;
        }
        panel.into_any_element()
    }

    fn render_explorer_children(
        &mut self,
        dir: &Path,
        depth: usize,
        show_hidden: bool,
        weak: &WeakEntity<Self>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if depth > 32 || !self.explorer.is_expanded(dir) {
            return Vec::new();
        }
        let entries = self.explorer.entries(dir, show_hidden).to_vec();
        let mut out = Vec::new();
        for entry in entries {
            let path = entry.path.clone();
            let name = entry.name.clone();
            let is_dir = entry.is_dir;
            let weak_row = weak.clone();
            if is_dir {
                let expanded = self.explorer.is_expanded(&path);
                let path_toggle = path.clone();
                let weak_toggle = weak.clone();
                out.push(
                    div()
                        .id(format!("explorer-dir:{}", path.display()))
                        .flex()
                        .items_center()
                        .gap_1()
                        .pl(px(8.0 + depth as f32 * 12.0))
                        .pr_2()
                        .py_1()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().colors.secondary))
                        .child(if expanded { "▾" } else { "▸" })
                        .child(name)
                        .on_click(move |_, _, cx| {
                            let _ = weak_toggle.update(cx, |this, cx| {
                                this.explorer.toggle_directory(
                                    &path_toggle,
                                    this.settings.show_hidden_files,
                                );
                                cx.notify();
                            });
                        })
                        .into_any_element(),
                );
                if expanded {
                    let nested =
                        self.render_explorer_children(&path, depth + 1, show_hidden, weak, cx);
                    out.extend(nested);
                }
            } else {
                let _ = weak_row;
                let weak_open = weak.clone();
                let path_open = path.clone();
                out.push(
                    div()
                        .id(format!("explorer-file:{}", path.display()))
                        .flex()
                        .items_center()
                        .gap_1()
                        .pl(px(8.0 + depth as f32 * 12.0 + 12.0))
                        .pr_2()
                        .py_1()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().colors.secondary))
                        .child(name)
                        .on_click(move |_, window, cx| {
                            let _ = weak_open.update(cx, |this, cx| {
                                this.open_path(&path_open, window, cx);
                            });
                        })
                        .into_any_element(),
                );
            }
        }
        out
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
        if let Some(request) = self.overwrite_save.take() {
            if matches!(request.then, SaveThen::Exit | SaveThen::CloseAll) {
                self.pending_exit_saves.clear();
                self.pending_close_saves.clear();
                self.pending_close_all_after_saves = false;
            }
            self.set_status(tr("status.save_cancelled"), cx);
        }
        cx.notify();
    }

    fn confirm_overwrite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(request) = self.overwrite_save.take() {
            self.save_as_request(request, window, cx);
        }
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
                then: SaveThen::Exit | SaveThen::CloseAll,
                ..
            } = dialog.purpose
            {
                self.pending_exit_saves.clear();
                self.pending_close_saves.clear();
                self.pending_close_all_after_saves = false;
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

    fn render_overlays(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut overlays = Vec::new();
        if self.find_in_files.open {
            overlays.push(self.render_find_in_files(window, cx));
        }
        if self.preferences.open {
            overlays.push(self.render_preferences(window, cx));
        }
        if self.confirm.is_some() {
            overlays.push(self.render_confirm(window, cx));
        }
        if self.go_to_line.is_some() {
            overlays.push(self.render_go_to_line(window, cx));
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
            CloseIntent::OverwriteExternal(_) => ("confirm.overwrite_yes", "confirm.overwrite_no"),
            CloseIntent::ReloadTab(_) => ("confirm.reload_save", "confirm.reload_discard"),
            _ if exit => ("confirm.save_exit", "confirm.discard_exit"),
            _ => ("confirm.save_close", "confirm.discard"),
        };
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
                    .child(
                        Button::new("confirm-discard")
                            .label(tr(discard_key))
                            .danger()
                            .on_click(move |_, window, cx| {
                                let _ = weak_discard
                                    .update(cx, |this, cx| this.confirm_discard(window, cx));
                            }),
                    )
                    .child(
                        Button::new("confirm-save")
                            .label(tr(save_key))
                            .primary()
                            .on_click(move |_, window, cx| {
                                let _ =
                                    weak_save.update(cx, |this, cx| this.confirm_save(window, cx));
                            }),
                    ),
            )
            .into_any_element();
        let _ = window;
        self.overlay_card(460.0, None, cx, content)
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
        let status = self.find_in_files.status.clone();
        let running = self.find_in_files.running;
        let results = self.find_in_files.results.clone();
        let weak_search = weak.clone();
        let weak_cancel = weak.clone();
        let weak_close = weak.clone();
        let weak_result = weak.clone();
        let count = results.len();
        let mut result_rows = Vec::new();
        for (index, matched) in results.iter().take(2000).enumerate() {
            let matched = matched.clone();
            let weak_row = weak_result.clone();
            result_rows.push(
                div()
                    .id(format!("ff-result:{index}"))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors.border)
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().colors.secondary))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(format!(
                                "{}:{}:{}",
                                matched.path.display(),
                                matched.line,
                                matched.column
                            )),
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
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Checkbox::new("ff-case")
                            .label(tr("ff.match_case"))
                            .checked(self.find_in_files.match_case)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.find_in_files.match_case = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("ff-word")
                            .label(tr("ff.whole_word"))
                            .checked(self.find_in_files.whole_word)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.find_in_files.whole_word = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("ff-regex")
                            .label(tr("ff.regex"))
                            .checked(self.find_in_files.regex)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.find_in_files.regex = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("ff-search")
                            .label(tr(if running { "ff.searching" } else { "ff.search" }))
                            .primary()
                            .compact()
                            .on_click(move |_, _, cx| {
                                let _ =
                                    weak_search.update(cx, |this, cx| this.start_find_in_files(cx));
                            }),
                    )
                    .child(
                        Button::new("ff-cancel")
                            .label(tr("common.cancel"))
                            .compact()
                            .on_click(move |_, _, cx| {
                                let _ = weak_cancel
                                    .update(cx, |this, cx| this.cancel_find_in_files(cx));
                            }),
                    ),
            )
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
        self.overlay_card(820.0, Some(600.0), cx, content)
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
        let rows = [
            (tr("shortcuts.new_open_save"), "Ctrl+N / Ctrl+O / Ctrl+S"),
            (tr("shortcuts.save_as_all"), "Ctrl+Shift+S"),
            (tr("shortcuts.close"), "Ctrl+W / Ctrl+Shift+W"),
            (tr("shortcuts.find_replace"), "Ctrl+F / Ctrl+H"),
            (tr("shortcuts.find_next_prev"), "F3 / Shift+F3"),
            (tr("shortcuts.find_in_files"), "Ctrl+Shift+F"),
            (tr("shortcuts.go_to_line"), "Ctrl+G"),
            (tr("shortcuts.undo_redo"), "Ctrl+Z / Ctrl+Shift+Z"),
            (
                tr("shortcuts.duplicate_delete_line"),
                "Ctrl+D / Ctrl+Shift+K",
            ),
            (tr("shortcuts.toggle_comment"), "Ctrl+/"),
            (tr("shortcuts.next_prev_tab"), "Ctrl+Tab / Ctrl+Shift+Tab"),
            (tr("shortcuts.zoom"), "Ctrl++ / Ctrl+- / Ctrl+0"),
            (tr("shortcuts.quit"), "Ctrl+Q"),
        ];
        let content = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_5()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr("shortcuts.title")),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(rows.map(|(action, shortcut)| {
                        div()
                            .flex()
                            .justify_between()
                            .text_sm()
                            .child(action)
                            .child(
                                div()
                                    .text_color(cx.theme().colors.muted_foreground)
                                    .child(shortcut),
                            )
                    })),
            )
            .child(
                div().flex().justify_end().child(
                    Button::new("shortcuts-close")
                        .label(tr("common.close"))
                        .on_click(move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.shortcuts_open = false;
                                cx.notify();
                            });
                        }),
                ),
            )
            .into_any_element();
        let _ = window;
        self.overlay_card(520.0, None, cx, content)
    }

    fn open_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preferences.open = true;
        let value = self.settings.font_family.clone();
        self.preferences
            .font_input
            .update(cx, |input, cx| input.set_value(value, window, cx));
        cx.notify();
    }

    fn render_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let font_input = self.preferences.font_input.clone();
        let settings = self.settings.clone();
        let weak_theme_light = cx.weak_entity();
        let weak_theme_dark = cx.weak_entity();
        let weak_close = cx.weak_entity();
        let weak_apply_font = cx.weak_entity();
        let weak_zoom_in = cx.weak_entity();
        let weak_zoom_out = cx.weak_entity();
        let weak_tab_in = cx.weak_entity();
        let weak_tab_out = cx.weak_entity();
        let weak_lang_system = cx.weak_entity();
        let weak_lang_en = cx.weak_entity();
        let weak_lang_zh = cx.weak_entity();

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
            .gap_3()
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
                            .child(tr("prefs.title")),
                    )
                    .child(
                        Button::new("prefs-close")
                            .label(tr("common.close"))
                            .compact()
                            .on_click(move |_, _, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.preferences.open = false;
                                    cx.notify();
                                });
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(div().w(px(140.)).text_sm().child(tr("prefs.language")))
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
            .child(
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
                    )),
            )
            .child(
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
                                let _ = weak_zoom_out.update(cx, |this, cx| this.zoom(-1.0, cx));
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
                                let _ = weak_zoom_in.update(cx, |this, cx| this.zoom(1.0, cx));
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().w(px(140.)).text_sm().child(tr("prefs.font_family")))
                    .child(div().flex_1().child(Input::new(&font_input).h(px(30.))))
                    .child(
                        Button::new("prefs-apply-font")
                            .label(tr("prefs.apply"))
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_apply_font.update(cx, |this, cx| {
                                    this.settings.font_family =
                                        this.preferences.font_input.read(cx).value().to_string();
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
                                this.settings.tab_size = (this.settings.tab_size + 1).min(16);
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
            .child(
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
            .child(
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
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr("prefs.footer")),
            )
            .into_any_element();
        let _ = window;
        self.overlay_card(680.0, Some(640.0), cx, content)
    }
}

impl Render for LubanApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.process_enter_actions(window, cx);
        self.sync_active(window, cx);
        self.sync_large_file_panel(window, cx);
        self.autosave_session();
        self.refresh_find(cx);
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
        let tabs = self.render_tabs(window, cx);
        let editor = self.render_editor_area(window, cx);
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
            Some(self.render_explorer(window, cx))
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
        root = root.child(tabs).child(
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_row()
                .children(explorer)
                .child(editor)
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
        };
        let doc = spec.load(1024);
        assert!(doc.is_non_text());
        assert_eq!(doc.text, "x\r\ny");
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

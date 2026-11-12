//! File picker actions and frame. Rendering is split by responsibility into
//! the sibling modules: chrome, sidebar, list and preview.

mod chrome;
mod list;
mod preview;
mod sidebar;

use std::path::{Path, PathBuf};
#[cfg(target_env = "ohos")]
use std::time::{Duration, Instant};

use super::*;

/// Row height shared by the virtual list and keyboard paging.
pub(super) const FILE_PICKER_ROW_HEIGHT: f32 = 30.0;

/// A picker that returns "cancelled" faster than this never presented a UI.
#[cfg(target_env = "ohos")]
const PICKER_START_WINDOW: Duration = Duration::from_secs(2);

/// Saving writes a temporary sibling before renaming, so the directory itself
/// has to accept new files, not just list them.
#[cfg(target_env = "ohos")]
fn directory_is_writable(dir: &Path) -> bool {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let probe = dir.join(format!(".luban-write-probe-{}-{nanos}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

impl LubanApp {
    // ------------------------------------------------------------------
    // Entry points
    // ------------------------------------------------------------------

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

    /// OpenHarmony owns the file-access permission model: user files are only
    /// reachable through the system picker, which also returns a granted path.
    /// Every file, folder and save dialog therefore goes through it. The
    /// in-app browser is only used when the system picker cannot present
    /// itself at all (for example a broken/virtualized FilePicker), never as a
    /// normal alternative.
    #[cfg(target_env = "ohos")]
    fn in_app_picker_directory(&self) -> PathBuf {
        // HOME is `/storage/Users/currentUser`, which the sandbox cannot read.
        // The app files directory is the one path the picker can both list and
        // write, so keep saved documents in a dedicated subdirectory.
        let config = std::env::var_os("LUBAN_NOTEPAD_CONFIG_DIR").map(PathBuf::from);
        if let Some(config) = config.filter(|dir| dir.is_dir()) {
            let documents = config.join("Documents");
            if !documents.is_dir() {
                let _ = std::fs::create_dir(&documents);
            }
            if directory_is_writable(&documents) {
                return documents;
            }
            if directory_is_writable(&config) {
                return config;
            }
        }
        std::env::current_dir()
            .ok()
            .filter(|dir| directory_is_writable(dir))
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    #[cfg(target_env = "ohos")]
    fn fallback_to_in_app_picker(
        &mut self,
        mode: FileDialogMode,
        purpose: FileDialogPurpose,
        dir: PathBuf,
        suggested_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_status(tr("status.picker_fallback"), cx);
        self.show_in_app_file_dialog(mode, purpose, dir, suggested_name, window, cx);
    }

    #[cfg(target_env = "ohos")]
    fn system_open_picker(
        &mut self,
        purpose: FileDialogPurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multiple = matches!(purpose, FileDialogPurpose::OpenFiles);
        let started = Instant::now();
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple,
            prompt: Some(tr("dialog.open_file").into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(Ok(Some(paths))) => {
                    if matches!(purpose, FileDialogPurpose::OpenFiles) {
                        this.open_paths_batch(paths, window, cx);
                    } else if let Some(path) = paths.into_iter().next() {
                        this.apply_open_path(purpose, path, window, cx);
                    }
                }
                Ok(Ok(None)) => {
                    if started.elapsed() < PICKER_START_WINDOW {
                        let dir = this.in_app_picker_directory();
                        this.fallback_to_in_app_picker(
                            FileDialogMode::Open,
                            purpose,
                            dir,
                            String::new(),
                            window,
                            cx,
                        );
                    }
                }
                Ok(Err(error)) => {
                    log::warn!("OHOS file picker failed, using in-app picker: {error}");
                    let dir = this.in_app_picker_directory();
                    this.fallback_to_in_app_picker(
                        FileDialogMode::Open,
                        purpose,
                        dir,
                        String::new(),
                        window,
                        cx,
                    );
                }
                Err(_) => {
                    log::warn!("OHOS file picker channel closed, using in-app picker");
                    let dir = this.in_app_picker_directory();
                    this.fallback_to_in_app_picker(
                        FileDialogMode::Open,
                        purpose,
                        dir,
                        String::new(),
                        window,
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    #[cfg(target_env = "ohos")]
    fn system_folder_picker(
        &mut self,
        purpose: FileDialogPurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let started = Instant::now();
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(tr("dialog.open_folder").into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        this.apply_folder_path(purpose, path, window, cx);
                    }
                }
                Ok(Ok(None)) => {
                    if started.elapsed() < PICKER_START_WINDOW {
                        let dir = this.in_app_picker_directory();
                        this.fallback_to_in_app_picker(
                            FileDialogMode::Folder,
                            purpose,
                            dir,
                            String::new(),
                            window,
                            cx,
                        );
                    }
                }
                Ok(Err(error)) => {
                    log::warn!("OHOS folder picker failed, using in-app picker: {error}");
                    let dir = this.in_app_picker_directory();
                    this.fallback_to_in_app_picker(
                        FileDialogMode::Folder,
                        purpose,
                        dir,
                        String::new(),
                        window,
                        cx,
                    );
                }
                Err(_) => {
                    log::warn!("OHOS folder picker channel closed, using in-app picker");
                    let dir = this.in_app_picker_directory();
                    this.fallback_to_in_app_picker(
                        FileDialogMode::Folder,
                        purpose,
                        dir,
                        String::new(),
                        window,
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    #[cfg(target_env = "ohos")]
    fn system_save_picker(
        &mut self,
        purpose: FileDialogPurpose,
        dir: PathBuf,
        suggested_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The picker needs a URI-addressable default location. HOME is never
        // readable on OpenHarmony, so fall back to the app's own documents
        // directory before the filesystem root.
        let start_dir = if dir.is_dir() {
            dir
        } else {
            std::env::var_os("LUBAN_NOTEPAD_CONFIG_DIR")
                .map(PathBuf::from)
                .map(|config| {
                    let documents = config.join("Documents");
                    if documents.is_dir() {
                        documents
                    } else {
                        config
                    }
                })
                .filter(|dir| dir.is_dir())
                .unwrap_or_else(|| PathBuf::from("/"))
        };
        let cancel_purpose = purpose.clone();
        let fallback_dir = self.in_app_picker_directory();
        let fallback_name = suggested_name.clone();
        let started = Instant::now();
        let receiver = cx.prompt_for_new_path(&start_dir, Some(&suggested_name));
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(Ok(Some(path))) => {
                    // The system save dialog already asks before replacing an
                    // existing file, so do not show a second confirmation.
                    this.apply_save_path(purpose, path, true, window, cx);
                }
                Ok(Ok(None)) => {
                    if started.elapsed() < PICKER_START_WINDOW {
                        // The picker never presented itself; continue the save
                        // flow in the in-app picker instead of aborting it.
                        this.fallback_to_in_app_picker(
                            FileDialogMode::Save,
                            purpose,
                            fallback_dir,
                            fallback_name,
                            window,
                            cx,
                        );
                        return;
                    }
                    if let FileDialogPurpose::SaveAs { then, .. } = cancel_purpose {
                        if matches!(
                            then,
                            SaveThen::Exit | SaveThen::CloseAll | SaveThen::NextTrimSave
                        ) {
                            this.abort_pending_saves();
                            this.set_status(tr("status.save_cancelled_close"), cx);
                        }
                    }
                }
                Ok(Err(error)) => {
                    log::warn!("OHOS save picker failed, using in-app picker: {error}");
                    this.fallback_to_in_app_picker(
                        FileDialogMode::Save,
                        purpose,
                        fallback_dir,
                        fallback_name,
                        window,
                        cx,
                    );
                }
                Err(_) => {
                    log::warn!("OHOS save picker channel closed, using in-app picker");
                    this.fallback_to_in_app_picker(
                        FileDialogMode::Save,
                        purpose,
                        fallback_dir,
                        fallback_name,
                        window,
                        cx,
                    );
                }
            });
        })
        .detach();
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

    pub(super) fn choose_find_in_files_folder(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub(super) fn show_file_dialog(
        &mut self,
        mode: FileDialogMode,
        purpose: FileDialogPurpose,
        dir: PathBuf,
        suggested_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // OpenHarmony has its own permission model: only the system picker can
        // return user files with the required grants. Never open the in-app
        // browser there, not even as a fallback.
        #[cfg(target_env = "ohos")]
        {
            match mode {
                FileDialogMode::Open => self.system_open_picker(purpose, window, cx),
                FileDialogMode::Save => {
                    self.system_save_picker(purpose, dir, suggested_name, window, cx)
                }
                FileDialogMode::Folder => self.system_folder_picker(purpose, window, cx),
            }
        }
        #[cfg(not(target_env = "ohos"))]
        self.show_in_app_file_dialog(mode, purpose, dir, suggested_name, window, cx);
    }

    /// The original path-based picker, used on desktop platforms only.
    #[cfg_attr(target_env = "ohos", allow(dead_code))]
    pub(super) fn show_in_app_file_dialog(
        &mut self,
        mode: FileDialogMode,
        purpose: FileDialogPurpose,
        dir: PathBuf,
        suggested_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dir = util::absolutize(&dir);
        let current_file_dir = self
            .active_doc()
            .and_then(|doc| doc.path.as_ref())
            .and_then(|path| path.parent())
            .map(Path::to_path_buf);
        let mut dialog = FileDialogState::new(mode, purpose, dir.clone(), Vec::new())
            .with_list_focus(cx.focus_handle().tab_stop(true));
        dialog.show_hidden = self.settings.show_hidden_files;
        dialog.recent_dirs = self.settings.recent_dirs.clone();
        dialog.places = self.file_picker_sections(&dialog.recent_dirs, current_file_dir.as_deref());
        dialog.recompute_rows();
        let focus = dialog.list_focus.clone();
        self.file_dialog = Some(dialog);
        self.pending_new_folder_create = false;
        self.pending_file_picker_select = None;

        let path_value = dir.display().to_string();
        self.file_path_input
            .update(cx, |input, cx| input.set_value(path_value, window, cx));
        self.file_name_input
            .update(cx, |input, cx| input.set_value(suggested_name, window, cx));
        for input in [
            self.file_search_input.clone(),
            self.file_filter_input.clone(),
            self.file_new_folder_input.clone(),
        ] {
            input.update(cx, |input, cx| input.set_value(String::new(), window, cx));
        }
        if mode == FileDialogMode::Save {
            self.file_name_input
                .update(cx, |input, cx| input.focus(window, cx));
        } else if let Some(focus) = focus {
            window.focus(&focus, cx);
        }
        self.start_file_picker_listing(dir, false, window, cx);
        self.refresh_file_picker_preview(cx);
    }

    fn file_picker_sections(
        &self,
        recent: &[PathBuf],
        current_file_dir: Option<&Path>,
    ) -> Vec<PlaceSection> {
        let mut roots = self.workspace_roots.clone();
        if let Some(root) = &self.workspace_root {
            if !roots.iter().any(|existing| util::same_path(existing, root)) {
                roots.insert(0, root.clone());
            }
        }
        build_sections(&self.workspace_name, &roots, current_file_dir, recent)
    }

    fn rebuild_file_picker_places(&mut self) {
        let current_file_dir = self
            .active_doc()
            .and_then(|doc| doc.path.as_ref())
            .and_then(|path| path.parent())
            .map(Path::to_path_buf);
        let Some(recent) = self
            .file_dialog
            .as_ref()
            .map(|dialog| dialog.recent_dirs.clone())
        else {
            return;
        };
        let sections = self.file_picker_sections(&recent, current_file_dir.as_deref());
        if let Some(dialog) = &mut self.file_dialog {
            dialog.places = sections;
        }
    }

    // ------------------------------------------------------------------
    // Listing
    // ------------------------------------------------------------------

    fn start_file_picker_listing(
        &mut self,
        dir: PathBuf,
        preserve: bool,
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
        dialog.error = None;
        let preserve_state = preserve.then(|| (dialog.selected.clone(), dialog.active_path()));
        if !preserve {
            dialog.entries.clear();
            dialog.rows = std::sync::Arc::new(Vec::new());
            dialog.selected.clear();
            dialog.active = None;
            dialog.anchor = None;
            dialog
                .list_scroll
                .scroll_to_item(0, gpui_kit::ScrollStrategy::Top);
        }
        let show_hidden = dialog.show_hidden;
        let task_dir = dir.clone();
        let task = cx.background_executor().spawn(async move {
            luban_core::workspace::list_directory_detailed(&task_dir, show_hidden)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, _window, cx| {
                app.finish_file_picker_listing(dir, generation, preserve_state, result, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_file_picker_listing(
        &mut self,
        dir: PathBuf,
        generation: u64,
        preserve: Option<(std::collections::BTreeSet<PathBuf>, Option<PathBuf>)>,
        result: luban_core::Result<Vec<luban_core::workspace::FileEntryInfo>>,
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
                dialog.recompute_rows();
                if let Some((selected, active)) = preserve {
                    dialog.selected = selected;
                    dialog.prune_selection();
                    if let Some(path) = active {
                        dialog.focus_path(&path);
                    }
                }
                if let Some(path) = self.pending_file_picker_select.take() {
                    if dialog.row_for_path(&path).is_some() {
                        if dialog
                            .row(dialog.row_for_path(&path).unwrap_or(0))
                            .is_some_and(|row| row.is_file())
                        {
                            dialog.select_only(&path);
                        }
                        dialog.focus_path(&path);
                    }
                }
            }
            Err(error) => {
                dialog.entries.clear();
                dialog.recompute_rows();
                // OpenHarmony path access is sandboxed: an unreadable path is
                // not a transient I/O failure, and the raw `os error 13` text
                // gives no way forward. Point at the system picker, which is
                // the supported route for files outside the sandbox.
                #[cfg(target_env = "ohos")]
                let message = if matches!(
                    &error,
                    luban_core::Error::Io(io)
                        if io.kind() == std::io::ErrorKind::PermissionDenied
                ) {
                    tr("dialog.list_sandbox_denied")
                } else {
                    tr_args("dialog.list_failed", &[("error", error.to_string())])
                };
                #[cfg(not(target_env = "ohos"))]
                let message = tr_args("dialog.list_failed", &[("error", error.to_string())]);
                dialog.error = Some(message);
            }
        }
        self.refresh_file_picker_preview(cx);
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Navigation
    // ------------------------------------------------------------------

    fn file_picker_navigate(
        &mut self,
        path: PathBuf,
        record: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = util::absolutize(&path);
        if !path.is_dir() {
            self.file_picker_error(not_a_folder_message(&path), cx);
            return;
        }
        let Some(dialog) = &mut self.file_dialog else {
            return;
        };
        dialog.dir = path.clone();
        if record {
            dialog.push_history();
        }
        dialog.remember_recent(&path);
        dialog.address_edit = false;
        dialog.active = None;
        dialog.anchor = None;
        dialog.selected.clear();
        dialog.error = None;
        dialog.search.clear();
        dialog.new_folder = None;
        dialog.preview.clear_current();
        self.file_search_input
            .update(cx, |input, cx| input.set_value(String::new(), window, cx));
        let value = path.display().to_string();
        self.file_path_input
            .update(cx, |input, cx| input.set_value(value, window, cx));
        self.rebuild_file_picker_places();
        self.start_file_picker_listing(path, false, window, cx);
        self.focus_file_picker_list(None, window, cx);
    }

    pub(super) fn file_picker_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .file_dialog
            .as_mut()
            .and_then(|dialog| dialog.go_back());
        if let Some(target) = target {
            self.file_picker_navigate(target, false, window, cx);
        }
    }

    pub(super) fn file_picker_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .file_dialog
            .as_mut()
            .and_then(|dialog| dialog.go_forward());
        if let Some(target) = target {
            self.file_picker_navigate(target, false, window, cx);
        }
    }

    pub(super) fn file_picker_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .file_dialog
            .as_ref()
            .and_then(|dialog| dialog.parent_dir());
        if let Some(target) = target {
            self.file_picker_navigate(target, true, window, cx);
        }
    }

    pub(super) fn file_picker_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.file_dialog.as_ref().map(|dialog| dialog.dir.clone());
        if let Some(dir) = dir {
            self.start_file_picker_listing(dir, true, window, cx);
        }
    }

    pub(super) fn file_picker_toggle_hidden(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &mut self.file_dialog else {
            return;
        };
        dialog.show_hidden = !dialog.show_hidden;
        let dir = dialog.dir.clone();
        self.start_file_picker_listing(dir, true, window, cx);
    }

    pub(super) fn file_picker_toggle_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.file_dialog {
            dialog.show_preview = !dialog.show_preview;
        }
        self.refresh_file_picker_preview(cx);
        cx.notify();
    }

    pub(super) fn file_picker_enter_address(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &mut self.file_dialog else {
            return;
        };
        dialog.address_edit = true;
        let value = dialog.dir.display().to_string();
        self.file_path_input
            .update(cx, |input, cx| input.set_value(value, window, cx));
        self.file_path_input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(super) fn file_picker_cancel_address(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = &mut self.file_dialog {
            dialog.address_edit = false;
        }
        self.focus_file_picker_list(None, window, cx);
    }

    pub(super) fn file_picker_apply_address(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let typed = self.file_path_input.read(cx).value().to_string();
        let typed = typed.trim().to_string();
        if typed.is_empty() {
            self.file_picker_cancel_address(window, cx);
            return;
        }
        let Some(dialog) = &self.file_dialog else {
            return;
        };
        let candidate = util::absolutize_from(&dialog.dir, Path::new(&typed));
        if candidate.is_dir() {
            self.file_picker_navigate(candidate, true, window, cx);
        } else if candidate.is_file() {
            if let Some(parent) = candidate.parent().map(Path::to_path_buf) {
                self.pending_file_picker_select = Some(candidate);
                self.file_picker_navigate(parent, true, window, cx);
            }
        } else {
            self.file_picker_error(file_not_found_message(&candidate), cx);
        }
    }

    // ------------------------------------------------------------------
    // Filtering, sorting, selection
    // ------------------------------------------------------------------

    pub(super) fn file_picker_set_filter(&mut self, filter: FileFilter, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.file_dialog {
            dialog.set_filter(filter);
        }
        self.refresh_file_picker_preview(cx);
        cx.notify();
    }

    pub(super) fn file_picker_select_custom_filter(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &mut self.file_dialog else {
            return;
        };
        dialog.custom_filter = true;
        let value = self.file_filter_input.read(cx).value().to_string();
        dialog.set_custom_filter(&value);
        self.file_filter_input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(super) fn file_picker_set_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.file_dialog {
            dialog.set_sort(key);
        }
        self.refresh_file_picker_preview(cx);
        cx.notify();
    }

    pub(super) fn file_picker_click(
        &mut self,
        row: usize,
        modifiers: Modifiers,
        double_click: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &self.file_dialog else {
            return;
        };
        let Some(entry) = dialog.row(row).cloned() else {
            return;
        };
        match entry.kind {
            PickerRowKind::Parent => {
                self.file_picker_up(window, cx);
                return;
            }
            PickerRowKind::Directory => {
                if double_click {
                    self.file_picker_navigate(entry.path, true, window, cx);
                    return;
                }
                if let Some(dialog) = &mut self.file_dialog {
                    dialog.active = Some(row);
                    dialog.anchor = Some(row);
                    dialog.selected.clear();
                    dialog
                        .list_scroll
                        .scroll_to_item(row, gpui_kit::ScrollStrategy::Nearest);
                }
            }
            PickerRowKind::File => {
                if double_click {
                    if let Some(dialog) = &mut self.file_dialog {
                        dialog.select_only(&entry.path);
                        dialog.active = Some(row);
                    }
                    self.file_dialog_confirm(window, cx);
                    return;
                }
                let command = modifiers.secondary(cx) || modifiers.control;
                if let Some(dialog) = &mut self.file_dialog {
                    match dialog.mode {
                        FileDialogMode::Open => {
                            if modifiers.shift {
                                dialog.extend_selection_to(row);
                            } else if command {
                                dialog.toggle_selection(&entry.path);
                                dialog.set_active(row);
                                dialog.anchor = Some(row);
                            } else {
                                dialog.select_row(row);
                            }
                        }
                        FileDialogMode::Save => {
                            dialog.select_row(row);
                        }
                        FileDialogMode::Folder => {
                            dialog.set_active(row);
                            dialog.anchor = Some(row);
                            dialog.selected.clear();
                        }
                    }
                    dialog
                        .list_scroll
                        .scroll_to_item(row, gpui_kit::ScrollStrategy::Nearest);
                }
                if let Some(dialog) = &self.file_dialog {
                    if dialog.mode == FileDialogMode::Save {
                        let name = entry
                            .path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        self.file_name_input
                            .update(cx, |input, cx| input.set_value(name, window, cx));
                    }
                }
            }
        }
        self.focus_file_picker_list(Some(row), window, cx);
        self.refresh_file_picker_preview(cx);
        cx.notify();
    }

    pub(super) fn file_picker_list_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.file_dialog else {
            return;
        };
        let Some(row) = dialog.active.and_then(|active| dialog.row(active)).cloned() else {
            self.file_dialog_confirm(window, cx);
            return;
        };
        match row.kind {
            PickerRowKind::Parent => self.file_picker_up(window, cx),
            PickerRowKind::Directory => {
                if dialog.mode == FileDialogMode::Folder {
                    self.file_dialog_confirm(window, cx);
                } else {
                    self.file_picker_navigate(row.path, true, window, cx);
                }
            }
            PickerRowKind::File => self.file_dialog_confirm(window, cx),
        }
    }

    pub(super) fn file_picker_list_focused(&self, window: &Window) -> bool {
        self.file_dialog
            .as_ref()
            .and_then(|dialog| dialog.list_focus.as_ref())
            .is_some_and(|focus| focus.is_focused(window))
    }

    pub(super) fn file_picker_input_focused(&self, window: &Window, cx: &App) -> bool {
        [
            &self.file_path_input,
            &self.file_name_input,
            &self.file_search_input,
            &self.file_filter_input,
            &self.file_new_folder_input,
        ]
        .iter()
        .any(|input| input.read(cx).focus_handle(cx).is_focused(window))
    }

    pub(super) fn focus_file_picker_list(
        &mut self,
        row: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = if let Some(dialog) = &mut self.file_dialog {
            if let Some(row) = row {
                dialog.set_active(row);
                dialog.list_scroll.scroll_to_item(
                    dialog.active.unwrap_or(row),
                    gpui_kit::ScrollStrategy::Nearest,
                );
            } else if dialog.active.is_none() {
                dialog.set_active(1.min(dialog.rows.len().saturating_sub(1)));
            }
            dialog.list_focus.clone()
        } else {
            None
        };
        if let Some(focus) = focus {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    pub(super) fn focus_file_picker_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.file_search_input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(super) fn file_picker_list_key(
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
            let height = f32::from(
                dialog
                    .list_scroll
                    .0
                    .borrow()
                    .base_handle
                    .bounds()
                    .size
                    .height,
            );
            let page = (height / FILE_PICKER_ROW_HEIGHT).floor().max(1.0) as usize;
            (dialog.visible_row_count(), dialog.active.unwrap_or(0), page)
        };
        if row_count == 0 {
            return false;
        }
        let command = modifiers.secondary(cx) || modifiers.control;

        if key == "space" && !command {
            if let Some(dialog) = &mut self.file_dialog {
                dialog.toggle_active();
                let row = dialog.active.unwrap_or(0);
                dialog
                    .list_scroll
                    .scroll_to_item(row, gpui_kit::ScrollStrategy::Nearest);
            }
            self.refresh_file_picker_preview(cx);
            cx.notify();
            return true;
        }
        if key == "a" && command {
            if let Some(dialog) = &mut self.file_dialog {
                dialog.select_all_visible();
            }
            self.refresh_file_picker_preview(cx);
            cx.notify();
            return true;
        }
        if key == "enter" {
            self.file_picker_list_enter(window, cx);
            return true;
        }
        if key == "backspace" && !command {
            let buffer = self
                .file_dialog
                .as_mut()
                .map(|dialog| dialog.type_ahead_backspace())
                .unwrap_or_default();
            if !buffer.is_empty() {
                if let Some(dialog) = &mut self.file_dialog {
                    if let Some(row) = dialog.type_ahead_row(&buffer) {
                        dialog.select_row(row);
                        dialog
                            .list_scroll
                            .scroll_to_item(row, gpui_kit::ScrollStrategy::Nearest);
                    }
                }
                self.refresh_file_picker_preview(cx);
                cx.notify();
                return true;
            }
            self.file_picker_up(window, cx);
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
        if let Some(target) = target {
            if let Some(dialog) = &mut self.file_dialog {
                if modifiers.shift {
                    dialog.extend_selection_to(target);
                } else if command {
                    dialog.set_active(target);
                } else {
                    dialog.select_row(target);
                }
                dialog
                    .list_scroll
                    .scroll_to_item(target, gpui_kit::ScrollStrategy::Nearest);
            }
            self.refresh_file_picker_preview(cx);
            cx.notify();
            return true;
        }

        // Type-ahead jumps to the next entry whose name starts with the typed
        // characters. Space stays bound to selection toggling.
        if !command && !modifiers.alt && key.len() == 1 {
            let ch = key.chars().next().unwrap_or_default();
            if ch.is_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                let buffer = self
                    .file_dialog
                    .as_mut()
                    .map(|dialog| dialog.type_ahead_char(ch))
                    .unwrap_or_default();
                if let Some(dialog) = &mut self.file_dialog {
                    if let Some(row) = dialog.type_ahead_row(&buffer) {
                        dialog.select_row(row);
                        dialog
                            .list_scroll
                            .scroll_to_item(row, gpui_kit::ScrollStrategy::Nearest);
                    }
                }
                self.refresh_file_picker_preview(cx);
                cx.notify();
                return true;
            }
        }
        false
    }

    /// Handle the picker's own shortcuts before registered commands. Returns
    /// true when the key was consumed.
    pub(super) fn file_picker_shortcut(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = event.keystroke.key.to_ascii_lowercase();
        let modifiers = event.keystroke.modifiers;
        let command = modifiers.secondary(cx) || modifiers.control;
        let alt = modifiers.alt;
        if command && !modifiers.shift && !alt {
            match key.as_str() {
                "l" => {
                    self.file_picker_enter_address(window, cx);
                    return true;
                }
                "f" => {
                    self.focus_file_picker_search(window, cx);
                    return true;
                }
                "h" => {
                    self.file_picker_toggle_hidden(window, cx);
                    return true;
                }
                _ => {}
            }
        }
        if alt && !command {
            match key.as_str() {
                "left" => {
                    self.file_picker_back(window, cx);
                    return true;
                }
                "right" => {
                    self.file_picker_forward(window, cx);
                    return true;
                }
                "up" => {
                    self.file_picker_up(window, cx);
                    return true;
                }
                _ => {}
            }
        }
        if key == "f5" && !command {
            self.file_picker_refresh(window, cx);
            return true;
        }
        if self.file_picker_list_focused(window) {
            if self.file_picker_list_key(&key, modifiers, window, cx) {
                return true;
            }
        } else if key == "down" && self.file_picker_input_focused(window, cx) {
            self.focus_file_picker_list(None, window, cx);
            return true;
        }
        false
    }

    // ------------------------------------------------------------------
    // New folder
    // ------------------------------------------------------------------

    pub(super) fn file_picker_open_new_folder(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &mut self.file_dialog else {
            return;
        };
        dialog.new_folder = Some(NewFolderState { error: None });
        dialog.address_edit = false;
        self.file_new_folder_input
            .update(cx, |input, cx| input.set_value(String::new(), window, cx));
        self.file_new_folder_input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(super) fn file_picker_cancel_new_folder(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = &mut self.file_dialog {
            dialog.new_folder = None;
        }
        self.focus_file_picker_list(None, window, cx);
    }

    pub(super) fn create_file_picker_folder(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self.file_new_folder_input.read(cx).value().to_string();
        let Some(dialog) = &mut self.file_dialog else {
            return;
        };
        let parent = dialog.dir.clone();
        match file_ops::create_dir(&parent, &name) {
            Ok(path) => {
                dialog.new_folder = None;
                self.file_new_folder_input
                    .update(cx, |input, cx| input.set_value(String::new(), window, cx));
                self.file_picker_navigate(path, true, window, cx);
            }
            Err(error) => {
                if let Some(state) = &mut dialog.new_folder {
                    state.error = Some(error.to_string());
                } else {
                    dialog.new_folder = Some(NewFolderState {
                        error: Some(error.to_string()),
                    });
                }
                cx.notify();
            }
        }
    }

    // ------------------------------------------------------------------
    // Confirm, cancel, errors
    // ------------------------------------------------------------------

    pub(super) fn file_dialog_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut dialog) = self.file_dialog.take() else {
            return;
        };
        let current_dir = dialog.dir.clone();
        dialog.remember_recent(&current_dir);
        self.persist_file_picker_recent(&dialog, cx);
        let typed = self.file_path_input.read(cx).value().to_string();
        let typed = typed.trim().to_string();
        let name = self.file_name_input.read(cx).value().to_string();
        let name = name.trim().to_string();
        let dir_text = dialog.dir.display().to_string();
        let typed_override = (!typed.is_empty() && typed != dir_text)
            .then(|| util::absolutize_from(&dialog.dir, Path::new(&typed)));

        match dialog.mode {
            FileDialogMode::Open => {
                if matches!(dialog.purpose, FileDialogPurpose::ImportTheme) {
                    let candidate = dialog
                        .selected_in_visible_order()
                        .first()
                        .cloned()
                        .or_else(|| typed_override.clone());
                    match candidate {
                        Some(path) if path.is_file() => {
                            self.import_syntax_theme(&path, window, cx);
                        }
                        Some(_) => {
                            self.file_dialog = Some(dialog);
                            self.file_picker_error(tr("dialog.not_a_file"), cx);
                        }
                        None => {
                            self.file_dialog = Some(dialog);
                            self.file_picker_error(tr("dialog.select_file"), cx);
                        }
                    }
                    cx.notify();
                    return;
                }
                let selected = dialog.selected_in_visible_order();
                if !selected.is_empty() {
                    if selected.len() > 1 && !matches!(dialog.purpose, FileDialogPurpose::OpenFiles)
                    {
                        let path = selected[0].clone();
                        self.file_picker_open_path(dialog, path, window, cx);
                        return;
                    }
                    if dialog.purpose_is_open_files() {
                        self.open_paths_batch(selected, window, cx);
                        cx.notify();
                        return;
                    }
                    let path = selected[0].clone();
                    self.file_picker_open_path(dialog, path, window, cx);
                    return;
                }
                if let Some(path) = typed_override {
                    if path.is_dir() {
                        self.file_dialog = Some(dialog);
                        self.file_picker_navigate(path, true, window, cx);
                        return;
                    }
                    if path.is_file() {
                        self.file_picker_open_path(dialog, path, window, cx);
                        return;
                    }
                    self.file_dialog = Some(dialog);
                    self.file_picker_error(file_not_found_message(&path), cx);
                    return;
                }
                if !dialog.search.trim().is_empty() {
                    if let Some(row) = dialog.first_visible_file().cloned() {
                        self.file_picker_open_path(dialog, row.path, window, cx);
                        return;
                    }
                    if let Some(row) = dialog.first_visible_entry().cloned() {
                        if row.is_dir() {
                            self.file_dialog = Some(dialog);
                            self.file_picker_navigate(row.path, true, window, cx);
                            return;
                        }
                    }
                }
                self.file_dialog = Some(dialog);
                self.file_picker_error(tr("dialog.select_open_file"), cx);
            }
            FileDialogMode::Save => {
                let name = if name.is_empty() {
                    name
                } else {
                    dialog.with_default_extension(&name)
                };
                let candidate = if !name.is_empty() {
                    util::absolutize_from(&dialog.dir, Path::new(&name))
                } else if let Some(path) = typed_override {
                    if path.is_dir() {
                        self.file_dialog = Some(dialog);
                        self.file_picker_navigate(path, true, window, cx);
                        return;
                    }
                    path
                } else {
                    self.file_dialog = Some(dialog);
                    self.file_picker_error(tr("dialog.enter_name"), cx);
                    return;
                };
                if candidate.is_dir() {
                    self.file_dialog = Some(dialog);
                    self.file_picker_error(tr("dialog.target_is_folder"), cx);
                    return;
                }
                self.apply_save_path(dialog.purpose.clone(), candidate, false, window, cx);
            }
            FileDialogMode::Folder => {
                let candidate = if let Some(path) = typed_override {
                    path
                } else if let Some(row) = dialog
                    .active
                    .and_then(|active| dialog.row(active))
                    .filter(|row| row.is_dir())
                {
                    row.path.clone()
                } else {
                    dialog.dir.clone()
                };
                if !candidate.is_dir() {
                    self.file_dialog = Some(dialog);
                    self.file_picker_error(not_a_folder_message(&candidate), cx);
                    return;
                }
                self.apply_folder_path(dialog.purpose, candidate, window, cx);
            }
        }
        cx.notify();
    }

    /// Dispatch a confirmed open path through the purpose-specific flow.
    fn file_picker_open_path(
        &mut self,
        dialog: FileDialogState,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !path.is_file() {
            self.file_dialog = Some(dialog);
            self.file_picker_error(tr("dialog.not_a_file"), cx);
            return;
        }
        self.apply_open_path(dialog.purpose, path, window, cx);
    }

    /// Open several files from the system picker, reporting partial failures.
    fn open_paths_batch(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let total = paths.len();
        let mut opened = 0usize;
        let mut failed = 0usize;
        for path in paths {
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
        if opened > 0 {
            self.focus_editor(window, cx);
        }
    }

    /// Dispatch an open path through the purpose-specific flow. Shared by the
    /// in-app picker and the OpenHarmony system picker.
    fn apply_open_path(
        &mut self,
        purpose: FileDialogPurpose,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match purpose {
            FileDialogPurpose::ImportTheme => self.import_syntax_theme(&path, window, cx),
            FileDialogPurpose::OpenWorkspace => self.open_workspace(&path, window, cx),
            FileDialogPurpose::ImportSettings => {
                self.pending_settings_import = Some(path);
                self.confirm = Some(ConfirmState::new(
                    CloseIntent::ImportSettings,
                    tr("settings.import_title"),
                    tr("settings.import_detail"),
                ));
            }
            _ => {
                self.open_path(&path, window, cx);
                // The picker owned focus while it was open. Restore it even
                // when the path merely activates an already-open tab, which
                // does not create a new editor.
                self.focus_editor(window, cx);
            }
        }
    }

    /// Dispatch a save target through the purpose-specific flow.
    fn apply_save_path(
        &mut self,
        purpose: FileDialogPurpose,
        candidate: PathBuf,
        system_picker: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match purpose {
            FileDialogPurpose::SaveAs { tab_id, then } => {
                let request = SaveAsRequest {
                    tab_id,
                    path: candidate,
                    then,
                };
                // The OHOS system picker creates a placeholder for a new file
                // and asks the user before replacing an existing one, so an
                // existence check here would either prompt twice or report a
                // placeholder as an overwrite.
                if request.path.exists() && !system_picker {
                    self.overwrite_save = Some(request);
                } else {
                    self.save_as_request(request, window, cx);
                }
            }
            FileDialogPurpose::Export { format } => {
                // The OHOS system dialog already asked before replacing an
                // existing file, and its sandbox path cannot be stat'ed
                // reliably, so it never reaches the in-app prompt.
                if candidate.exists() && !system_picker {
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
                    candidate.with_extension(luban_core::workspace_file::WORKSPACE_EXTENSION)
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

    /// Dispatch a chosen folder through the purpose-specific flow.
    fn apply_folder_path(
        &mut self,
        purpose: FileDialogPurpose,
        candidate: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match purpose {
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

    pub(super) fn cancel_file_dialog(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = self.file_dialog.take() {
            self.persist_file_picker_recent(&dialog, cx);
            if let FileDialogPurpose::SaveAs {
                then: SaveThen::Exit | SaveThen::CloseAll | SaveThen::NextTrimSave,
                ..
            } = dialog.purpose
            {
                self.abort_pending_saves();
                self.set_status(tr("status.save_cancelled_close"), cx);
            }
        }
        self.pending_new_folder_create = false;
        self.pending_file_picker_select = None;
        cx.notify();
    }

    fn persist_file_picker_recent(&mut self, dialog: &FileDialogState, cx: &mut Context<Self>) {
        if self.settings.recent_dirs != dialog.recent_dirs {
            self.settings.recent_dirs = dialog.recent_dirs.clone();
            self.commit_settings(cx);
        }
    }

    pub(super) fn file_picker_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.file_dialog {
            dialog.error = Some(message.into());
        }
        cx.notify();
    }

    /// Escape inside the picker unwinds transient state before closing it.
    pub(super) fn file_dialog_escape(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(dialog) = &self.file_dialog else {
            return false;
        };
        if dialog.address_edit {
            self.file_picker_cancel_address(window, cx);
            return true;
        }
        if dialog.new_folder.is_some() {
            self.file_picker_cancel_new_folder(window, cx);
            return true;
        }
        let search_focused = self
            .file_search_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let filter_focused = self
            .file_filter_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        if search_focused {
            if !dialog.search.is_empty() {
                self.file_search_input
                    .update(cx, |input, cx| input.set_value(String::new(), window, cx));
                if let Some(dialog) = &mut self.file_dialog {
                    dialog.set_search(String::new());
                }
                self.refresh_file_picker_preview(cx);
                cx.notify();
            } else {
                self.focus_file_picker_list(None, window, cx);
            }
            return true;
        }
        if filter_focused {
            self.focus_file_picker_list(None, window, cx);
            return true;
        }
        false
    }

    // ------------------------------------------------------------------
    // Frame
    // ------------------------------------------------------------------

    pub(super) fn render_file_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.file_dialog.is_none() {
            return div().into_any_element();
        }
        let viewport = window.viewport_size();
        let width = (f32::from(viewport.width) - 48.0).clamp(720.0, 1120.0);
        let height = (f32::from(viewport.height) - 96.0).clamp(460.0, 720.0);
        let show_preview = self
            .file_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.show_preview && width >= 980.0);

        let title = self.render_file_picker_title(cx);
        let toolbar = self.render_file_picker_toolbar(window, cx);
        let sidebar = self.render_file_picker_sidebar(cx);
        let list = self.render_file_picker_list(window, cx);
        let preview = show_preview.then(|| self.render_file_picker_preview(cx));
        let footer = self.render_file_picker_footer(cx);

        let body = div()
            .flex()
            .flex_col()
            .size_full()
            .child(title)
            .child(toolbar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .child(sidebar)
                    .child(list)
                    .children(preview),
            )
            .child(footer);
        self.overlay_card(width, Some(height), cx, body.into_any_element())
    }

    pub(super) fn render_overwrite_confirm(
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
}

impl FileDialogState {
    fn purpose_is_open_files(&self) -> bool {
        matches!(self.purpose, FileDialogPurpose::OpenFiles)
    }
}

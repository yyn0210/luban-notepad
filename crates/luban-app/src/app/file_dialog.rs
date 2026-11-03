use super::*;

impl LubanApp {
    // Built-in file dialog: listing, navigation and rendering

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
    pub(super) fn file_dialog_list_focused(&self, window: &Window) -> bool {
        self.file_dialog
            .as_ref()
            .and_then(|dialog| dialog.list_focus.as_ref())
            .is_some_and(|focus| focus.is_focused(window))
    }
    pub(super) fn focus_file_dialog_list(
        &mut self,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
    pub(super) fn file_dialog_list_key(
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
    pub(super) fn file_dialog_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn cancel_file_dialog(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn render_file_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
                            .on_click(move |_, window, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.cancel_file_dialog(cx);
                                    this.focus_editor(window, cx);
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
                        .on_click(move |_, window, cx| {
                            let _ = weak_cancel.update(cx, |this, cx| {
                                this.cancel_file_dialog(cx);
                                this.focus_editor(window, cx);
                            });
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

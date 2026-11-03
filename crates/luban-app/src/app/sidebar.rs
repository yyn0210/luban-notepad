use super::*;

impl LubanApp {
    // Sidebar UI: outline, explorer and workspace file operations

    pub(super) fn render_side_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
    pub(super) fn explorer_command(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
    pub(super) fn outline_key(
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
    pub(super) fn explorer_key(
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
}

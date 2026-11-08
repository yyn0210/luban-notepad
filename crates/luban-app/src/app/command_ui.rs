use super::*;

impl LubanApp {
    // ------------------------------------------------------------------
    // Commands and shortcuts
    // ------------------------------------------------------------------

    pub(super) fn command_context(&self) -> CommandContext {
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
                    && !tab.doc.read_only
            }),
            can_go_back: self.nav.can_back(),
            can_go_forward: self.nav.can_forward(),
            diff_running: !self.diff_tasks.is_empty(),
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
        let static_spec = commands::spec(id);
        let plugin_command = self.plugin_host.command(id).map(|command| {
            (
                command.plugin_id.clone(),
                command.entry.clone(),
                command.when,
            )
        });
        if static_spec.is_none() && plugin_command.is_none() {
            log::warn!("unknown command {id}");
            return false;
        }
        let context = self.command_context();
        if let Some(spec) = static_spec {
            if !spec.enabled(&context) {
                return false;
            }
        }
        if id != "edit.select-next-occurrence" {
            self.occurrence_anchor = None;
        }
        if id == "edit.undo" {
            self.snippet_session = None;
        }
        let recording_before =
            (self.macro_recording && !self.macro_playback && luban_core::macros::recordable(id))
                .then(|| self.macro_state_signature());
        if let Some((plugin_id, entry, when)) = plugin_command {
            if !when.enabled(&context) {
                return false;
            }
            // Menu, keybinding and CLI paths launch commands without args;
            // panel clicks carry args through run_command_with_args instead.
            let ok = self.run_plugin_command(&plugin_id, &entry, id, None, 0, window, cx);
            if ok {
                if let Some(before) = recording_before {
                    // Plugin commands are recorded even when they only update
                    // plugin-owned state rather than the document.
                    if before != self.macro_state_signature() || static_spec.is_none() {
                        self.macro_recorded.push(id.to_string());
                        cx.notify();
                    }
                }
            }
            return ok;
        }
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
            "diff.cancel" => self.cancel_diff(cx),
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
            "plugins.manage" => {
                self.plugin_manager.open(window, cx);
                cx.notify();
            }
            "plugins.store" => self.open_plugin_store(window, cx),
            "plugins.reload" => self.reload_plugins(window, cx),
            "packages.reload" => self.reload_packages(window, cx),
            "plugins.open-panel" => {
                if let Some(panel) = self.plugin_host.panels.first().cloned() {
                    self.open_plugin_panel(&panel.plugin_id, &panel.id, window, cx);
                } else {
                    self.set_status(tr("plugins.no_panels"), cx);
                }
            }
            "edit.insert-snippet" => self.open_snippet_picker(window, cx),
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

    pub(super) fn rebuild_keymap(&mut self) {
        self.keymap = Keymap::build_with(
            &self.settings.keybindings,
            &self.plugin_host.dynamic_specs(),
        );
        self.rebuild_macro_keymap();
    }

    pub(super) fn shortcut(
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
        if self
            .snippet_session
            .as_ref()
            .is_some_and(|session| Some(session.tab_id) != self.active_tab().map(|tab| tab.id))
        {
            self.snippet_session = None;
        }
        if self.snippet_session.is_some() && command && key == "z" && !shift {
            self.snippet_session = None;
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
                self.focus_editor(window, cx);
                cx.notify();
                return true;
            }
            if self.sort_dialog.is_some() {
                self.sort_dialog = None;
                self.focus_editor(window, cx);
                cx.notify();
                return true;
            }
            if self.filter_dialog.is_some() {
                self.filter_dialog = None;
                self.focus_editor(window, cx);
                cx.notify();
                return true;
            }
            if self.macro_ui.open {
                self.macro_ui.open = false;
                self.macro_ui.error = None;
                self.focus_editor(window, cx);
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
                self.focus_editor(window, cx);
                cx.notify();
                return true;
            }
            if self.overwrite_save.is_some() {
                self.cancel_overwrite(cx);
                return true;
            }
            if self.file_dialog.is_some() {
                self.cancel_file_dialog(cx);
                self.focus_editor(window, cx);
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
                self.focus_editor(window, cx);
                cx.notify();
                return true;
            }
            if self.go_to_line.is_some() {
                self.go_to_line = None;
                self.focus_editor(window, cx);
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
                self.focus_editor(window, cx);
                cx.notify();
                return true;
            }
            if self.find.open {
                self.find.open = false;
                self.cancel_huge_search();
                self.find.clear();
                self.focus_editor(window, cx);
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

        // The file picker is modal: its own shortcuts run first, text editing
        // chords pass through to its inputs, and every other command/chord is
        // swallowed so commands cannot fire behind the dialog.
        if self.file_dialog.is_some() {
            if self.file_picker_shortcut(event, window, cx) {
                return true;
            }
            let input_focused = self.file_picker_input_focused(window, cx);
            if input_focused && command && matches!(key.as_str(), "c" | "v" | "x" | "a" | "z" | "y")
            {
                return false;
            }
            if command || alt {
                return true;
            }
            let function_key = key.len() > 1
                && key.starts_with('f')
                && key[1..].chars().all(|ch| ch.is_ascii_digit());
            if function_key {
                return true;
            }
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

    /// Label for a command, sourced from the registry so menus and the
    /// command palette cannot drift.
    pub(super) fn command_label(id: &str) -> String {
        commands::spec(id)
            .map(|spec| tr(spec.label_key))
            .unwrap_or_else(|| id.to_string())
    }
}

use super::*;

/// A tab closed during this session, available through Ctrl+Shift+T.
#[derive(Clone)]
pub(super) struct ClosedTab {
    pub(super) path: Option<PathBuf>,
    pub(super) name: String,
    pub(super) text: Option<String>,
    pub(super) encoding: TextEncoding,
    pub(super) eol: Eol,
    pub(super) language: String,
    pub(super) cursor: usize,
    pub(super) anchor: usize,
    pub(super) modified: bool,
    pub(super) huge: bool,
    pub(super) huge_line: Option<u64>,
    pub(super) huge_column: Option<u64>,
    /// Byte range of a partially opened tab.
    pub(super) range: Option<[u64; 2]>,
    pub(super) non_text: bool,
    pub(super) file_size: u64,
    pub(super) last_modified: Option<SystemTime>,
}
/// Drag payload for reordering tabs.
#[derive(Clone)]
pub(super) struct TabDrag {
    pub(super) id: u64,
    pub(super) label: SharedString,
}
pub(super) struct TabDragPreview {
    pub(super) label: SharedString,
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
    /// Incremented for every editor text change. Live plugin panels use it to
    /// debounce work without forcing a buffer sync on every key press.
    pub edit_generation: u64,
    /// Cached word count and the document revision it was computed for.
    pub(super) word_count: Option<(u64, usize)>,
    /// Present when the tab is backed by the disk engine.
    pub huge: Option<HugeFileUi>,
    pub highlighted_revision: u64,
    /// External change/deletion discovered by the file watcher.
    pub external: Option<ExternalNotice>,
    /// Files at or above `Settings::large_file_threshold` open in a reduced
    /// mode: no syntax highlighting, wrapping, bracket matching or live find.
    pub large_file: bool,
    pub(super) decorations_key: Option<DecorationKey>,
    /// Revision and enable flag the bracket decorations were computed for.
    pub(super) bracket_key: Option<(u64, bool)>,
    pub(super) _subscription: Subscription,
    /// Re-renders the app when the editor notifies without an `InputEvent`
    /// (wheel scrolling, cursor motion), so the file scrollbar and the global
    /// gutter follow the viewport.
    pub(super) _notification: Subscription,
}
type DecorationKey = (u64, usize, u64, bool, bool, bool);
/// Destination index after moving one item, or `None` for a no-op move.
pub(super) fn reorder_index(len: usize, from: usize, target: usize) -> Option<usize> {
    if from >= len || target > len || from == target || from + 1 == target {
        return None;
    }
    Some((if from < target { target - 1 } else { target }).min(len.saturating_sub(1)))
}
/// Indices closed by "Close Others" or "Close to the Right".
pub(super) fn trim_queue(len: usize, keep: usize, right: bool) -> Vec<usize> {
    if keep >= len {
        return Vec::new();
    }
    if right {
        (keep + 1..len).collect()
    } else {
        (0..len).filter(|index| *index != keep).collect()
    }
}

impl LubanApp {
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

    pub(super) fn tab_index_by_id(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    pub(super) fn close_tab_by_id_without_prompt(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(index) = self.tab_index_by_id(id) {
            self.cancel_diff_for_tab(id);
            self.cancel_tab_jobs(index);
            self.tabs.remove(index);
            self.nav.remove_tab(id);
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len().saturating_sub(1);
            }
            self.active_in_group[self.active_group] = None;
            self.find.dirty = true;
            self.resolve_wait_for_tab(id, cx);
            cx.notify();
        }
    }

    pub fn request_close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        let Some(tab) = self.active_tab() else { return };
        let id = tab.id;
        let name = tab.doc.display_name();
        log::debug!(
            "close request for tab {id}: modified={} wait={} targets={:?}",
            tab.doc.is_modified(),
            self.wait,
            self.wait_for_tabs
        );
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

    pub(super) fn close_tab_at(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_tab_at_inner(index, window, cx);
    }

    pub(super) fn close_tab_at_inner(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.tabs.len() {
            return;
        }
        let active_before = self.tabs.get(self.active).map(|tab| tab.id);
        let closed = self.tabs[index].id;
        self.cancel_diff_for_tab(closed);
        let closed_group = self.tabs[index].group;
        let closed_position = self.tabs[..index]
            .iter()
            .filter(|tab| tab.group == closed_group)
            .count();
        let closed_active = index == self.active;
        self.record_closed_tab(index, cx);
        self.cancel_tab_jobs(index);
        self.tabs.remove(index);
        self.resolve_wait_for_tab(closed, cx);
        self.nav.remove_tab(closed);
        if self.active_in_group[closed_group] == Some(closed) {
            self.active_in_group[closed_group] = None;
        }
        if self.tabs.is_empty() {
            self.active = 0;
            self.active_group = 0;
            self.active_in_group = [None, None];
            self.new_file(window, cx);
            // Closing the last tab lands on a fresh untitled document; that is
            // an active-document change for the plugin pump.
            self.emit_active_document_changed(cx);
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
        // Only a change of the active tab itself is an active-document
        // change; closing a background tab just shifts indices.
        if self.tabs.get(self.active).map(|tab| tab.id) != active_before {
            self.emit_active_document_changed(cx);
        }
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

    pub(super) fn close_all_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_batch_is_close = false;
        for index in 0..self.tabs.len() {
            self.cancel_tab_jobs(index);
        }
        let closed_ids: Vec<u64> = self.tabs.iter().map(|tab| tab.id).collect();
        for id in &closed_ids {
            self.cancel_diff_for_tab(*id);
        }
        let active_before = self.tabs.get(self.active).map(|tab| tab.id);
        self.tabs.clear();
        self.active = 0;
        self.active_group = 0;
        self.active_in_group = [None, None];
        self.nav.clear();
        self.find.clear();
        for id in closed_ids {
            self.resolve_wait_for_tab(id, cx);
        }
        self.new_file(window, cx);
        // Close All lands on a fresh untitled document — same active-document
        // change the last-tab close reports (identity guard mirrors
        // close_tab_at_inner).
        if self.tabs.get(self.active).map(|tab| tab.id) != active_before {
            self.emit_active_document_changed(cx);
        }
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

    pub(super) fn on_should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        log::debug!("on_should_close allow_close={}", self.allow_close);
        if self.allow_close {
            return true;
        }
        self.sync_active(window, cx);
        if self.modified_tabs().is_empty() {
            let last = cx.windows().len() <= 1;
            let closing: Vec<u64> = self.tabs.iter().map(|tab| tab.id).collect();
            for id in &closing {
                self.cancel_diff_for_tab(*id);
            }
            let replied = self.resolve_pending_waits_for_tabs(&closing);
            self.persist_state(window, cx);
            self.allow_close = true;
            if self.window_registered {
                window_registry::unregister(self.window_id);
                self.window_registered = false;
            }
            if last {
                if replied {
                    // Let the IPC thread flush the successful wait responses
                    // before the process exits.
                    self.schedule_wait_exit();
                } else {
                    cx.quit();
                }
            }
            return true;
        }
        self.request_exit(window, cx);
        false
    }

    pub(super) fn prompt_next_batch_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn confirm_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn confirm_discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn confirm_cancel(&mut self, cx: &mut Context<Self>) {
        self.confirm = None;
        self.pending_history_restore = None;
        self.abort_pending_saves();
        cx.notify();
    }

    pub(super) fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_batch_is_close = false;
        for index in 0..self.tabs.len() {
            self.cancel_tab_jobs(index);
        }
        let closing: Vec<u64> = self.tabs.iter().map(|tab| tab.id).collect();
        for id in &closing {
            self.cancel_diff_for_tab(*id);
        }
        let replied = self.resolve_pending_waits_for_tabs(&closing);
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
        } else if replied {
            // Let the IPC thread flush the successful wait responses before
            // the process exits.
            self.schedule_wait_exit();
        } else {
            cx.quit();
        }
    }

    // ------------------------------------------------------------------
    // Session
    // ------------------------------------------------------------------

    /// Line count of a tab for Goto Anything and the document map. Reduced
    /// large-file tabs keep their text in the editor, not the document.
    pub(super) fn tab_line_count(&self, index: usize, cx: &gpui_kit::App) -> usize {
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

    pub(super) fn record_closed_tab(&mut self, index: usize, cx: &Context<Self>) {
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

    pub(super) fn reopen_closed_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                            path.to_path_buf(),
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

    pub(super) fn activate_tab_index(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index < self.tabs.len() {
            let changed = index != self.active;
            if changed {
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
            self.focus_editor(window, cx);
            if changed {
                self.emit_active_document_changed(cx);
            }
            cx.notify();
        }
    }

    /// Activate a tab by its position inside the active group.
    pub(super) fn activate_group_position(
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

    pub(super) fn next_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
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
        // The previously focused editor is no longer mounted, so keyboard
        // focus has to move to the tab that just became active.
        self.focus_editor(window, cx);
        self.emit_active_document_changed(cx);
        cx.notify();
    }

    pub(super) fn render_tabs(
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
    pub(super) fn move_tab_by_id(
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

    // ------------------------------------------------------------------
    // Tab groups, lifecycle and rendering helpers
    // ------------------------------------------------------------------

    /// Character range for a 1-based line/column and match length, read from
    /// the editor rope so no line index or full-text scan is needed.
    pub(super) fn line_selection_in_tab(
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

    /// Number of visible editor groups.
    pub(super) fn group_count(&self) -> usize {
        if self.split.is_split() {
            2
        } else {
            1
        }
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
            self.session_writer,
            self.safe_mode,
            cx,
        ) {
            Ok(_) => {
                if let Some(index) = self.tab_index_by_id(id) {
                    self.cancel_tab_jobs(index);
                    self.tabs.remove(index);
                    self.nav.remove_tab(id);
                    self.resolve_wait_for_tab(id, cx);
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

    /// Resolve forwarded `--wait` requests whose target tabs are closing.
    /// Returns whether any response was sent.
    pub(super) fn resolve_pending_waits_for_tabs(&mut self, ids: &[u64]) -> bool {
        if self.pending_waits.is_empty() || ids.is_empty() {
            return false;
        }
        for wait in &mut self.pending_waits {
            wait.tab_ids.retain(|tab| !ids.contains(tab));
        }
        let ready: Vec<usize> = self
            .pending_waits
            .iter()
            .enumerate()
            .filter(|(_, wait)| wait.tab_ids.is_empty())
            .map(|(index, _)| index)
            .collect();
        let mut replied = false;
        for index in ready.into_iter().rev() {
            let wait = self.pending_waits.remove(index);
            let _ = wait.reply.send(crate::ipc::IpcResponse::ok(wait.id));
            replied = true;
        }
        replied
    }

    pub(super) fn resolve_wait_for_tab(&mut self, id: u64, _cx: &mut Context<Self>) {
        log::debug!(
            "tab {id} closed: wait={} targets={:?} pending={}",
            self.wait,
            self.wait_for_tabs,
            self.pending_waits.len()
        );
        self.resolve_pending_waits_for_tabs(&[id]);
        if !self.wait {
            return;
        }
        self.wait_for_tabs.retain(|tab| *tab != id);
        log::debug!("wait targets after close: {:?}", self.wait_for_tabs);
        if self.wait_for_tabs.is_empty() {
            self.schedule_wait_exit();
        }
    }

    /// Materialize a detached document buffer from its editor. Returns false
    /// when the tab does not exist.
    pub(super) fn ensure_tab_text(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
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
    pub(super) fn release_tab_text(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index) {
            if tab.large_file {
                let _ = tab.doc.detach_text();
            }
        }
    }

    /// Synchronise the editor window with the engine: apply text changes,
    /// update the global cursor and request a new window near an edge.
    pub(super) fn sync_huge_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub(super) fn cancel_tab_jobs(&self, index: usize) {
        if let Some(ui) = self.tabs.get(index).and_then(|tab| tab.huge.as_ref()) {
            if let Some(job) = &ui.save {
                job.cancel.store(true, Ordering::Relaxed);
            }
            if let Some(job) = &ui.search {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    pub(super) fn apply_language_indent_to_open_tabs(&mut self, cx: &mut Context<Self>) {
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

    pub(super) fn apply_language_tab_size(&mut self, cx: &mut Context<Self>) {
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

    /// Reload a normal document from disk without asking (the caller has
    /// already confirmed discarding unsaved changes).
    pub(super) fn reload_tab_now(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub(super) fn modified_tabs(&self) -> Vec<u64> {
        self.tabs
            .iter()
            .filter(|t| t.doc.is_modified())
            .map(|t| t.id)
            .collect()
    }

    pub(super) fn apply_replacement_to_tab(
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

    /// Stop and forget the diff calculation owned by a closing tab.
    fn cancel_diff_for_tab(&mut self, tab_id: u64) {
        for task in &self.diff_tasks {
            if task.tab_id == tab_id {
                task.cancel.store(true, Ordering::Relaxed);
            }
        }
        self.diff_tasks.retain(|task| task.tab_id != tab_id);
    }

    /// Move the caret to a 1-based line of a tab and focus the editor.
    pub(super) fn jump_to_line_in_tab(
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

    /// Close every tab except the active one, or every tab to its right.
    ///
    /// Modified targets are saved before any tab is closed, so cancelling the
    /// save phase leaves the whole tab set intact.
    pub(super) fn request_trim_tabs(
        &mut self,
        right: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub(super) fn rename_open_tabs(&mut self, from: &Path, to: &Path) {
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
}

use super::*;

impl LubanApp {
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

    pub(super) fn save_tab_at(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_tab_at_with(index, SaveThen::Nothing, false, window, cx);
    }

    /// Save a tab, optionally resuming a close/exit flow afterwards.
    ///
    /// Unless `force` is set, a file whose size or modification time changed
    /// since it was opened shows an Overwrite/Reload/Cancel confirmation
    /// instead of being overwritten silently.
    pub(super) fn save_tab_at_with(
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
    pub(super) fn start_batch_save(
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
    pub(super) fn save_next_normal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn finish_normal_batch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn huge_save_in_flight(&self) -> bool {
        self.tabs
            .iter()
            .any(|tab| tab.huge.as_ref().is_some_and(|ui| ui.save.is_some()))
    }

    pub(super) fn has_unsaved_tabs(&self) -> bool {
        self.tabs
            .iter()
            .any(|tab| tab.doc.is_modified() || tab.pending_edit)
    }

    /// Refuse to close or quit when a document was edited after the batch
    /// started. The user must retry so the new edits enter the queue.
    pub(super) fn unsaved_blocks_close(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.pending_batch_is_close || !self.has_unsaved_tabs() {
            return false;
        }
        self.abort_pending_saves();
        self.set_status(tr("status.batch_unsaved"), cx);
        true
    }

    /// Whether no batch is waiting on a user decision, an unsaved normal tab,
    /// or an in-flight disk-engine save.
    pub(super) fn batch_can_finalize(&self) -> bool {
        self.confirm.is_none()
            && self.pending_normal_overwrite.is_none()
            && self.pending_huge_overwrite.is_none()
            && !self.pending_normal_all
            && !self.pending_normal_close_all
            && !self.pending_normal_exit
            && self.pending_normal_saves.is_empty()
            && !self.huge_save_in_flight()
    }

    pub(super) fn report_save_all_result(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn abort_pending_saves(&mut self) {
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
        if tab.doc.read_only {
            self.set_status(tr("status.document_read_only"), cx);
            return;
        }
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

    /// Start a background save. `then` continues a close/exit flow after the
    /// file has been replaced.
    pub(super) fn start_huge_save(
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

    pub(super) fn finish_huge_save(
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

    pub(super) fn finish_save_flow(
        &mut self,
        then: SaveThen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub(super) fn save_next_huge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    /// Take a local-history snapshot of the file a save is about to replace.
    pub(super) fn snapshot_before_save(&mut self, target: &Path, cx: &mut Context<Self>) {
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
}

use super::*;

impl LubanApp {
    // Large-file and disk-engine UI: opens, windows, search, replace and reload

    /// Start a controller-driven open. Files within the size limit load whole;
    /// larger files load the beginning portion by default.
    pub(super) fn start_controller_open(
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
    pub(super) fn start_huge_open(
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
    pub(super) fn start_huge_job(
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
                        let closed_id = self.tabs[index].id;
                        self.tabs.remove(index);
                        self.resolve_wait_for_tab(closed_id, cx);
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
                        let closed_id = self.tabs[index].id;
                        self.tabs.remove(index);
                        self.resolve_wait_for_tab(closed_id, cx);
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
    pub(super) fn sync_large_file_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn maybe_schedule_huge_window(
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
    pub(super) fn huge_scroll_to(
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
    pub(super) fn huge_history(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn huge_jump_line(
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
    pub(super) fn huge_jump_end(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn huge_find_next(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
    /// Reveal a huge-file search hit without moving a viewport that already
    /// shows it. Off-screen hits load a window centred on their line, the same
    /// rule the floating find window applies to normal documents.
    fn reveal_huge_find_hit(
        &mut self,
        index: usize,
        hit: &luban_core::disk_engine::SearchHit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport_lines = self.huge_viewport_lines(index, cx).unwrap_or(1);
        let on_screen = self
            .tabs
            .get(index)
            .and_then(|tab| tab.huge.as_ref())
            .and_then(|ui| {
                let first = ui.scroll_top_line;
                let last = first.saturating_add(viewport_lines);
                if hit.line < first || hit.line >= last {
                    return None;
                }
                Some((ui.local_of_global(hit.start)?, ui.local_of_global(hit.end)?))
            });
        if let Some((start, end)) = on_screen {
            let editor = self.tabs[index].editor.clone();
            editor.update(cx, |state, cx| {
                state.set_selected_range(start as usize..end as usize, cx);
                state.focus(window, cx);
            });
            if let Some(huge) = self.tabs[index].doc.huge_mut() {
                huge.anchor = hit.start;
                huge.cursor = hit.end;
            }
            self.focus_editor = true;
            cx.notify();
            return;
        }
        // Keep the prefetch together with the viewport target: both are
        // measured from the hit line so the match lands mid-window.
        let target_line = hit.line.saturating_sub(PREFETCH_LINES / 2).max(1);
        let top_line = hit.line.saturating_sub(viewport_lines / 2).max(1);
        self.request_huge_window(
            index,
            WindowTarget::Line(target_line),
            hit.start,
            top_line,
            Some((hit.start, hit.end)),
            window,
            cx,
        );
        self.focus_editor = true;
        cx.notify();
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
                self.reveal_huge_find_hit(index, &hit, window, cx);
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
    pub(super) fn huge_find_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn huge_find_jump(
        &mut self,
        hit: luban_core::disk_engine::SearchHit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = self.active;
        if !self.tabs.get(index).is_some_and(|tab| tab.doc.is_huge()) {
            return;
        }
        self.reveal_huge_find_hit(index, &hit, window, cx);
    }
    pub(super) fn huge_replace_one(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn huge_replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn cancel_huge_search(&mut self) {
        for tab in &self.tabs {
            if let Some(job) = tab.huge.as_ref().and_then(|ui| ui.search.as_ref()) {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
    }
    /// Stop background disk-engine index scans.
    pub(super) fn cancel_huge_opens(&mut self, cx: &mut Context<Self>) -> bool {
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
    /// Reload a disk-engine tab. The engine is replaced by a freshly indexed
    /// one after the background scan completes.
    pub(super) fn start_huge_reload(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
}

use super::*;

impl LubanApp {
    // ------------------------------------------------------------------
    // Plugin runtime UI: commands, diffs, snippets and panels
    // ------------------------------------------------------------------

    pub(super) fn plugin_allowed_roots(&self) -> Vec<PathBuf> {
        let document_dir = self
            .active_doc()
            .and_then(|doc| doc.path.as_deref())
            .and_then(Path::parent);
        Self::plugin_allowed_roots_for(
            self.workspace_root.as_ref(),
            &self.workspace_roots,
            document_dir,
        )
    }

    /// File-system roots available to plugins.
    ///
    /// A workspace is the security boundary: when one is open, the directory
    /// of a document outside it is not exposed. Without a workspace the active
    /// document's directory is the only useful root.
    pub(super) fn plugin_allowed_roots_for(
        workspace_root: Option<&PathBuf>,
        workspace_roots: &[PathBuf],
        document_dir: Option<&Path>,
    ) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = workspace_root.into_iter().cloned().collect();
        roots.extend(workspace_roots.iter().cloned());
        if roots.is_empty() {
            if let Some(dir) = document_dir {
                roots.push(dir.to_path_buf());
            }
        }
        roots.sort();
        roots.dedup();
        roots
            .into_iter()
            .filter_map(|root| root.canonicalize().ok())
            .collect()
    }

    pub(super) fn plugin_document_snapshot(
        &self,
        cx: &gpui_kit::App,
    ) -> Option<luban_plugin::runtime::DocumentSnapshot> {
        self.plugin_tab_snapshot(self.active, cx)
    }

    /// Snapshot one tab by index for the plugin runtime. The active tab is the
    /// common case ([`Self::plugin_document_snapshot`]); the event pump also
    /// snapshots an arbitrary saved tab for its `DocumentSaved` batch events.
    /// Huge, partial-range, large-file, non-text and read-only documents are
    /// never exposed to plugins.
    pub(super) fn plugin_tab_snapshot(
        &self,
        index: usize,
        cx: &gpui_kit::App,
    ) -> Option<luban_plugin::runtime::DocumentSnapshot> {
        let tab = self.tabs.get(index)?;
        if tab.large_file
            || tab.doc.is_huge()
            || tab.doc.partial_range().is_some()
            || tab.doc.is_non_text()
            || tab.doc.read_only
        {
            return None;
        }
        let (selection_start, selection_end) = tab.doc.selection();
        let state = tab.editor.read(cx);
        let rope = state.text();
        let char_count = rope.len_chars();
        let cursor = tab.doc.cursor.min(char_count);
        let byte = rope.char_to_byte_idx(cursor);
        let position = rope.offset_to_position(byte);
        Some(luban_plugin::runtime::DocumentSnapshot {
            path: tab.doc.path.clone(),
            text: tab.doc.text.clone(),
            language: tab.doc.language.clone(),
            line: position.line as usize + 1,
            column: position.character as usize + 1,
            selection_start,
            selection_end,
            modified: tab.doc.is_modified(),
        })
    }

    /// Run one plugin command. `context_kind` is the context the command runs
    /// in: `User` for user-initiated runs (palette, menu, keybinding, CLI,
    /// panel click), or the requesting call's own kind when a plugin requested
    /// this command via `HostAction::RunCommand` — context inheritance down
    /// the chain, so a nested command from `on_edit` inherits the read-only
    /// Edit contract and one from an event inherits Event (spec §6's cascade
    /// philosophy applied to the chain, not just direct calls).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_plugin_command(
        &mut self,
        plugin_id: &str,
        entry: &str,
        command_id: &str,
        args: Option<serde_json::Value>,
        command_depth: u8,
        context_kind: luban_plugin::runtime::CallContextKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let settings = self.plugin_host.settings_for(plugin_id);
        let input = luban_plugin::runtime::CallInput {
            plugin: luban_plugin::runtime::PluginIdentity {
                id: plugin_id.to_string(),
                version: self
                    .plugin_host
                    .manifest(plugin_id)
                    .map(|manifest| manifest.version.clone())
                    .unwrap_or_default(),
                data_dir: self.plugin_host.root.join(plugin_id).join("data"),
            },
            document: self.plugin_document_snapshot(cx),
            settings,
            allowed_roots: self.plugin_allowed_roots(),
            command_depth,
            context_kind,
        };
        let Some(call) = self
            .plugin_host
            .run_command(plugin_id, entry, command_id, args, input)
        else {
            self.set_status(
                tr_args(
                    "plugins.not_available",
                    &[("plugin", plugin_id.to_string())],
                ),
                cx,
            );
            return false;
        };
        self.apply_plugin_call(plugin_id, call, context_kind, window, cx)
    }

    /// True while any plugin-originated call's actions are being applied.
    /// The event pump's emit sites check this before emitting their own
    /// events, so a plugin's write-backs cannot cascade back into the
    /// plugin that caused them.
    pub(super) fn plugin_in_event_context(&self) -> bool {
        self.plugin_event_depth > 0
    }

    /// Apply one plugin-originated call's actions. This is the single
    /// write-back point for every `HostAction` the runtime emits from a
    /// command, event or timer call, so depth bookkeeping and the
    /// status/error contract stay uniform. Panel renders keep their own
    /// read-only action match. `context_kind` is the kind the call ran with;
    /// plugin-requested `command.run` actions inherit it unchanged so the
    /// Edit contract (read-only) and the Event contract (cascade-suppressed,
    /// mutable) travel down the whole chain.
    pub(super) fn apply_plugin_call(
        &mut self,
        plugin_id: &str,
        call: luban_plugin::runtime::RuntimeCall<()>,
        context_kind: luban_plugin::runtime::CallContextKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Actions applied inside this call are the plugin's own event-context
        // side effects. The pump sets its own suppression before queueing
        // events; this guard covers nested command.run chains spawned from
        // event contexts. The guard also restores the counter on unwind, so
        // a panic cannot leave event emission suppressed forever.
        self.raise_event_depth_during(move |this| {
            let mut ok = true;
            for action in call.output.actions {
                match action {
                    luban_plugin::runtime::HostAction::SetDocumentText { text, selection } => {
                        let (start, end) = selection.unwrap_or((0, 0));
                        this.replace_document_text(text, start, end, window, cx);
                    }
                    luban_plugin::runtime::HostAction::Notify(message) => {
                        this.set_status(message, cx);
                    }
                    luban_plugin::runtime::HostAction::SetStatus { id, text } => {
                        this.plugin_host.set_status(plugin_id, &id, text);
                        cx.notify();
                    }
                    luban_plugin::runtime::HostAction::OpenPanel { id } => {
                        if this
                            .plugin_host
                            .panels
                            .iter()
                            .any(|panel| panel.plugin_id == plugin_id && panel.id == id)
                        {
                            this.open_plugin_panel(plugin_id, &id, window, cx);
                        } else {
                            this.set_status(
                                format!("plugin '{plugin_id}' requested unknown panel '{id}'"),
                                cx,
                            );
                        }
                    }
                    luban_plugin::runtime::HostAction::RefreshPanel { id } => {
                        // Only a re-render of THIS plugin's already-open panel.
                        // A closed panel is a silent no-op: there is no target
                        // to refresh, and `luban.panel.open` exists for
                        // opening one.
                        let open = this.plugin_panel.as_ref().is_some_and(|panel| {
                            panel.plugin_id == plugin_id && panel.panel_id == id
                        });
                        if open {
                            this.refresh_plugin_panel(plugin_id, &id, window, cx);
                        }
                    }
                    luban_plugin::runtime::HostAction::SetCursor { line, col } => {
                        this.set_plugin_cursor(line, col, window, cx);
                    }
                    luban_plugin::runtime::HostAction::SetSetting { key, value } => {
                        if this.persist_plugin_setting(plugin_id, &key, value, cx) {
                            cx.notify();
                        } else {
                            ok = false;
                            this.set_status(
                                format!(
                                    "plugin '{plugin_id}' cannot write setting '{key}': \
                                     undeclared or invalid value"
                                ),
                                cx,
                            );
                        }
                    }
                    luban_plugin::runtime::HostAction::RunCommand { id, args, depth } => {
                        ok &= this.apply_run_command_action(
                            &id,
                            args,
                            depth,
                            context_kind,
                            window,
                            cx,
                        );
                    }
                }
            }
            if let Some(error) = call.error {
                ok = false;
                this.set_status(error, cx);
            }
            ok
        })
    }

    /// Run `f` with the plugin-event suppression counter raised, restoring it
    /// afterwards — including on unwind, by catching the panic, repairing the
    /// counter and resuming. Panic semantics are unchanged: the hook still
    /// runs once and the unwind still propagates. A drop guard cannot express
    /// this because the guarded region itself needs `&mut self`, so the guard
    /// is spelled catch/resume instead. The event pump dispatches every
    /// plugin call through this so its emit sites stay suppressed.
    pub(super) fn raise_event_depth_during<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.plugin_event_depth = self.plugin_event_depth.saturating_add(1);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self)));
        self.plugin_event_depth = self.plugin_event_depth.saturating_sub(1);
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// Resolve and run one plugin-requested `HostAction::RunCommand`. Plugin
    /// commands resolve first — the same precedence the user-path
    /// [`Self::run_command`] applies — so a plugin command whose id equals a
    /// built-in's cannot reach the built-in fast path and re-enter the host
    /// at depth 0, which would bypass the cascade cap.
    fn apply_run_command_action(
        &mut self,
        id: &str,
        args: Option<serde_json::Value>,
        depth: u8,
        context_kind: luban_plugin::runtime::CallContextKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let plugin_command = self.plugin_host.command(id).cloned();
        match crate::plugin_host::route_run_command(id, depth, plugin_command.is_some()) {
            crate::plugin_host::RunCommandRoute::DepthExceeded => {
                self.set_status(format!("command '{id}' exceeded run depth"), cx);
                false
            }
            crate::plugin_host::RunCommandRoute::Plugin => {
                // route_run_command classified this id as a plugin command.
                // The nested command inherits the requesting call's context
                // kind, so an `on_edit`-spawned command stays read-only and an
                // event-spawned one stays cascade-suppressed — the write
                // feedback loop (on_edit → command.run → edit → on_edit) is
                // broken at the chain, not just at direct dispatches.
                let command = plugin_command.expect("route promised a plugin command");
                self.run_plugin_command(
                    &command.plugin_id,
                    &command.entry,
                    id,
                    args,
                    depth,
                    context_kind,
                    window,
                    cx,
                )
            }
            crate::plugin_host::RunCommandRoute::BuiltIn => {
                if args.is_some() {
                    log::warn!("built-in command '{id}' ignores args");
                }
                // A built-in id routed here has no plugin command behind it
                // (plugin-first routing above), but keep the suppression
                // guard up while it runs. Accepted residual: macro playback
                // (`command.run('macro.play-last')`) replays user-recorded ids
                // through `run_command` at the ambient depth AND with the
                // ambient User document context, so an `on_edit` handler could
                // replay user-recorded edits. Harming this requires a
                // user-recorded, non-idempotent macro and recording is
                // user-driven and out of plugin reach, so the residual is
                // accepted rather than threading the kind through the
                // built-in executor.
                self.raise_event_depth_during(|this| this.run_command(id, window, cx))
            }
            crate::plugin_host::RunCommandRoute::NotFound => {
                self.set_status(format!("command '{id}' not found"), cx);
                false
            }
        }
    }

    /// Run a command by id, forwarding `args` to plugin commands (panel
    /// clicks carry them) and ignoring them for built-ins. This is the
    /// panel-facing counterpart of [`Self::run_command`], which always runs
    /// plugin commands without args.
    pub(super) fn run_command_with_args(
        &mut self,
        id: &str,
        args: Option<serde_json::Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(command) = self.plugin_host.command(id).cloned() else {
            if args.is_some() {
                log::warn!("built-in command '{id}' ignores args");
            }
            return self.run_command(id, window, cx);
        };
        self.run_plugin_command(
            &command.plugin_id,
            &command.entry,
            id,
            args,
            0,
            luban_plugin::runtime::CallContextKind::User,
            window,
            cx,
        )
    }

    /// Move the active document's cursor to a 1-based line/column. Mirrors
    /// the `--goto` semantics: positions clamp to the document bounds (the
    /// `HostAction` contract says the host clamps), the caret is a collapsed
    /// selection, and the editor scrolls the caret into view. Huge documents
    /// jump through the disk engine; range views — including a huge document
    /// narrowed to a partial range, whose line index is range-local — cannot
    /// map a file-global line and report that instead.
    fn set_plugin_cursor(
        &mut self,
        line: usize,
        col: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active >= self.tabs.len() {
            return;
        }
        let index = self.active;
        // The partial check must come first: a huge document WITH a partial
        // range is reachable (huge range view, session restore), and the disk
        // engine's line index is range-local there. Mirrors `open_path_at`,
        // which refuses exactly this state. A `large_file` tab is only in a
        // reduced rendering mode; its full text is loaded, so it does not
        // need the refusal.
        if self.tabs[index].doc.partial_range().is_some() {
            self.set_status(tr("status.line_outside_partial"), cx);
            return;
        }
        if self.tabs[index].doc.is_huge() {
            self.huge_jump_line(line as u64, col as u64, window, cx);
            return;
        }
        // Same rope math as the `--goto` path: clamp the line to the last
        // row, then the column to that row's length. `select_char_range`
        // moves the caret, scrolls it into view and focuses the editor, like
        // Ctrl+G.
        let (position, _) = self.line_selection_in_tab(index, line, col, 0, cx);
        self.select_char_range(position, position, window, cx);
    }

    pub fn reload_packages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let packages = luban_core::user_packages::load(&self.package_paths);
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
        self.snippets = packages.snippets.clone();
        self.packages = packages;
        for index in 0..self.tabs.len() {
            let language = language_key(&self.tabs[index].doc.language).to_string();
            let editor = self.tabs[index].editor.clone();
            editor.update(cx, |state, cx| {
                state.set_highlighter_factory(crate::highlighter::factory(), cx);
                state.set_highlighter(&language, cx);
            });
        }
        if let Err(error) = syntax_theme::apply(&self.settings, cx) {
            log::warn!("could not apply syntax theme: {error}");
        }
        let message = tr_args(
            "packages.reloaded",
            &[
                ("syntax", self.packages.syntax_files.len().to_string()),
                ("themes", self.packages.themes.len().to_string()),
                ("snippets", self.snippets.snippets.len().to_string()),
            ],
        );
        self.set_status(message, cx);
        let _ = window;
        cx.notify();
    }

    pub fn reload_plugins(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_plugins_now(cx);
        // Lazy pump start: runtimes may have appeared (install, enable). The
        // started flag is only set on an actual spawn, so a reload without
        // plugins leaves the pump unstarted and the next one retries.
        self.start_plugin_pump(window, cx);
    }

    /// Reload the plugin host without needing a window handle, so background
    /// store completions can refresh plugins from the idle poll loop.
    pub fn reload_plugins_now(&mut self, cx: &mut Context<Self>) {
        let locale = crate::i18n::active_locale();
        self.plugin_host =
            crate::plugin_host::PluginHost::load(&self.settings, self.safe_mode, &locale);
        for (plugin_id, values) in &self.plugin_host.settings {
            self.settings
                .plugin_settings
                .insert(plugin_id.clone(), values.clone());
        }
        self.global_settings.plugin_settings = self.settings.plugin_settings.clone();
        self.rebuild_keymap();
        self.snippet_session = None;
        self.plugin_panel = None;
        self.plugin_panel_drag = None;
        self.plugin_manager.text_inputs.clear();
        cx.notify();
    }

    pub fn open_diff(
        &mut self,
        left: &Path,
        right: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Missing or unreadable inputs must fail the launch/forward request
        // before a placeholder tab is created.
        if !crate::ipc::path_is_readable(left) {
            self.set_status(
                tr_args("diff.unreadable", &[("path", left.display().to_string())]),
                cx,
            );
            return false;
        }
        if !crate::ipc::path_is_readable(right) {
            self.set_status(
                tr_args("diff.unreadable", &[("path", right.display().to_string())]),
                cx,
            );
            return false;
        }

        let mut doc = Document::new_untitled(tr_args(
            "diff.title",
            &[
                ("left", left.display().to_string()),
                ("right", right.display().to_string()),
            ],
        ));
        doc.attach_text(tr("diff.computing"));
        doc.language = "Diff".to_string();
        doc.read_only = true;
        self.add_document(doc, window, cx);
        let Some(tab_id) = self.active_tab().map(|tab| tab.id) else {
            return false;
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new((0u64, 0u64)));
        self.diff_tasks.push(DiffTask {
            tab_id,
            cancel: cancel.clone(),
        });
        self.set_status(tr("diff.computing"), cx);

        let target_left = left.to_path_buf();
        let target_right = right.to_path_buf();
        let task_cancel = cancel.clone();
        let task_progress = progress.clone();
        let task = cx.background_executor().spawn(async move {
            luban_core::diff::diff_files_with_progress(
                &target_left,
                &target_right,
                &task_cancel,
                &mut |done, total| {
                    if let Ok(mut slot) = task_progress.lock() {
                        *slot = (done, total);
                    }
                },
            )
        });

        let pump_progress = progress.clone();
        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let keep = this
                .update(cx, |app, cx| {
                    if !app.diff_tasks.iter().any(|task| task.tab_id == tab_id) {
                        return false;
                    }
                    let (done, total) = pump_progress.lock().map(|slot| *slot).unwrap_or((0, 0));
                    let percent = done.saturating_mul(100).checked_div(total).unwrap_or(0);
                    app.set_status(
                        tr_args(
                            "diff.computing_progress",
                            &[("percent", percent.to_string())],
                        ),
                        cx,
                    );
                    true
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_diff(tab_id, result, window, cx);
            });
        })
        .detach();
        true
    }

    fn finish_diff(
        &mut self,
        tab_id: u64,
        result: Result<luban_core::diff::DiffOutput, luban_core::diff::DiffError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff_tasks.retain(|task| task.tab_id != tab_id);
        let Some(index) = self.tab_index_by_id(tab_id) else {
            // The tab was closed while the calculation was running.
            return;
        };
        match result {
            Ok(output) => {
                let text = if output.identical {
                    tr("diff.identical")
                } else if output.binary {
                    tr("diff.binary")
                } else if output.too_large {
                    tr("diff.too_large")
                } else {
                    output.text
                };
                let status = if output.identical {
                    tr("diff.ready.identical")
                } else if output.binary {
                    tr("diff.ready.binary")
                } else if output.too_large {
                    tr("diff.ready.too_large")
                } else {
                    tr_args(
                        "diff.ready",
                        &[
                            ("added", output.added.to_string()),
                            ("removed", output.removed.to_string()),
                        ],
                    )
                };
                let tab = &mut self.tabs[index];
                tab.doc.replace_all_text(&text, EditKind::Other);
                tab.doc.mark_saved();
                tab.doc.set_selection(0, 0);
                let editor = tab.editor.clone();
                editor.update(cx, |state, cx| {
                    state.replace_all(text, window, cx);
                    state.set_selected_range(0..0, cx);
                    state.set_readonly(true, cx);
                });
                self.active = index;
                self.active_group = self.tabs[index].group;
                self.active_in_group[self.active_group] = Some(tab_id);
                self.set_status(status, cx);
            }
            Err(luban_core::diff::DiffError::Cancelled) => {
                self.close_tab_at(index, window, cx);
                self.set_status(tr("diff.cancelled"), cx);
            }
            Err(error) => {
                self.close_tab_at(index, window, cx);
                self.set_status(error.to_string(), cx);
            }
        }
    }

    pub(super) fn cancel_diff(&mut self, cx: &mut Context<Self>) {
        if self.diff_tasks.is_empty() {
            return;
        }
        for task in &self.diff_tasks {
            task.cancel.store(true, Ordering::Relaxed);
        }
        self.set_status(tr("diff.cancelling"), cx);
    }

    pub(super) fn open_snippet_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            return;
        }
        let Some(tab) = self.active_tab() else {
            self.set_status(tr("snippet.no_document"), cx);
            return;
        };
        if tab.large_file
            || tab.doc.is_huge()
            || tab.doc.partial_range().is_some()
            || tab.doc.is_non_text()
        {
            self.set_status(tr("snippet.needs_full_text"), cx);
            return;
        }
        let language = tab.doc.language.clone();
        let token = language_key(&language).to_string();
        let mut unsupported = std::collections::BTreeSet::new();
        let mut skipped_scopes = std::collections::BTreeSet::new();
        for snippet in &self.snippets.snippets {
            let Some(scope) = snippet.scope.as_deref() else {
                continue;
            };
            if luban_core::snippet::scope_is_fully_supported(scope) {
                if !snippet.matches_language(&language, &token) {
                    skipped_scopes.insert(scope);
                }
            } else {
                unsupported.insert(scope);
            }
        }
        if !unsupported.is_empty() {
            log::warn!(
                "{} snippet scope(s) use unsupported Sublime selectors; only simple source.<language> forms can match: {}",
                unsupported.len(),
                unsupported.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        let rows = crate::snippet_ui::rows(&self.snippets.snippets, &language, &token);
        if rows.is_empty() {
            log::warn!(
                "no snippets match language '{language}' (token '{token}'); {} snippet(s) loaded; scopes that did not match: {}",
                self.snippets.snippets.len(),
                if skipped_scopes.is_empty() {
                    "-".to_string()
                } else {
                    skipped_scopes.into_iter().collect::<Vec<_>>().join(", ")
                }
            );
            self.set_status(tr("snippet.none"), cx);
            return;
        }
        let state = cx.new(|cx| CommandState::new(window, cx));
        state.update(cx, |state, cx| state.focus(window, cx));
        self.snippet_picker_rows = rows;
        self.palette = Some(PaletteOverlay {
            kind: PaletteKind::Snippets,
            state,
            command_groups: Vec::new(),
            plugin_commands: Vec::new(),
            plugin_section: 0,
        });
        cx.notify();
    }

    pub(super) fn insert_snippet(
        &mut self,
        snippet: &luban_core::snippet::Snippet,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.full_text_editing_available(window, cx) {
            return;
        }
        self.sync_active(window, cx);
        let Some(tab) = self.active_tab() else {
            return;
        };
        let tab_id = tab.id;
        let (start, end) = tab.doc.selection();
        let chars = tab.doc.char_count();
        let start = start.min(chars);
        let end = end.min(chars);
        let expansion = luban_core::snippet::expand(&snippet.body);
        let mut text = String::with_capacity(tab.doc.text.len() + expansion.text.len());
        text.extend(tab.doc.text.chars().take(start));
        text.push_str(&expansion.text);
        text.extend(tab.doc.text.chars().skip(end));
        let (select_start, select_end) = expansion
            .stops
            .first()
            .map(|stop| (start + stop.start, start + stop.end))
            .unwrap_or_else(|| {
                let cursor = start + expansion.text.chars().count();
                (cursor, cursor)
            });
        self.replace_document_text(text, select_start, select_end, window, cx);
        if expansion.stops.is_empty() {
            self.snippet_session = None;
        } else {
            self.snippet_session = Some(crate::snippet_ui::SnippetSession::new(
                tab_id, start, &expansion,
            ));
        }
        if !expansion.warnings.is_empty() {
            log::warn!(
                "snippet '{}': {}",
                snippet.name,
                expansion.warnings.join("; ")
            );
        }
    }

    pub(super) fn snippet_next(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .snippet_session
            .as_ref()
            .is_some_and(|session| Some(session.tab_id) != self.active_tab().map(|tab| tab.id))
        {
            self.snippet_session = None;
            return;
        }
        let selection_end = self
            .active_tab()
            .map(|tab| tab.doc.selection().1)
            .unwrap_or(0);
        let Some(session) = self.snippet_session.as_mut() else {
            return;
        };
        // Shift+Tab on the first field keeps the session and the current
        // selection; only Tab past the last field ends it.
        if !forward && session.current == 0 {
            return;
        }
        // Typing at a stop can change its length. Track the edited stop and
        // shift the remaining stops and the final cursor accordingly.
        session.adjust_for_edit(selection_end);
        match session.advance(forward) {
            Some((start, end)) => self.select_char_range(start, end, window, cx),
            None => self.end_snippet(window, cx),
        }
    }

    pub(super) fn end_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.snippet_session.take() else {
            return;
        };
        let cursor = session.final_cursor;
        self.select_char_range(cursor, cursor, window, cx);
    }

    pub(super) fn open_plugin_panel(
        &mut self,
        plugin_id: &str,
        panel_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let auto_refresh = self
            .plugin_host
            .panels
            .iter()
            .find(|panel| panel.plugin_id == plugin_id && panel.id == panel_id)
            .is_some_and(|panel| panel.refresh == luban_plugin::manifest::PanelRefresh::Edit);
        self.plugin_panel_watch = self.plugin_panel_watch.wrapping_add(1);
        let watch = self.plugin_panel_watch;
        self.render_plugin_panel_content(plugin_id, panel_id, watch, window, cx);
        if auto_refresh
            && self
                .plugin_panel
                .as_ref()
                .is_some_and(|panel| panel.watch == watch)
        {
            self.watch_plugin_panel(watch, window, cx);
        }
    }

    /// Recompute the open panel while keeping its watcher token.
    fn refresh_plugin_panel(
        &mut self,
        plugin_id: &str,
        panel_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let watch = self
            .plugin_panel
            .as_ref()
            .filter(|panel| panel.plugin_id == plugin_id && panel.panel_id == panel_id)
            .map(|panel| panel.watch)
            .unwrap_or_else(|| {
                self.plugin_panel_watch = self.plugin_panel_watch.wrapping_add(1);
                self.plugin_panel_watch
            });
        self.render_plugin_panel_content(plugin_id, panel_id, watch, window, cx);
    }

    /// Recompute the open panel after a UI-language switch so plugin-generated
    /// titles and body text that used `luban.i18n.pick` pick up the new
    /// language.
    pub(super) fn refresh_open_plugin_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &self.plugin_panel else {
            return;
        };
        let plugin_id = panel.plugin_id.clone();
        let panel_id = panel.panel_id.clone();
        self.refresh_plugin_panel(&plugin_id, &panel_id, window, cx);
    }

    /// Pull edits into the document, then recompute a watched panel.
    fn refresh_watched_plugin_panel(
        &mut self,
        watch: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.plugin_panel.as_ref() else {
            return;
        };
        if panel.watch != watch {
            return;
        }
        let plugin_id = panel.plugin_id.clone();
        let panel_id = panel.panel_id.clone();
        if self.active_tab().is_some_and(|tab| {
            !tab.large_file
                && !tab.doc.is_huge()
                && tab.doc.partial_range().is_none()
                && !tab.doc.is_non_text()
                && !tab.doc.read_only
        }) {
            self.sync_active(window, cx);
        }
        self.render_plugin_panel_content(&plugin_id, &panel_id, watch, window, cx);
    }

    /// State that makes a live panel stale. `None` closes the watcher.
    fn plugin_panel_generation(&self, watch: u64) -> Option<(u64, u64, u64, u64)> {
        let panel = self.plugin_panel.as_ref()?;
        if panel.watch != watch {
            return None;
        }
        match self.active_tab() {
            Some(tab) => Some((watch, tab.id, tab.edit_generation, tab.doc.revision())),
            None => Some((watch, 0, 0, 0)),
        }
    }

    fn watch_plugin_panel(&mut self, watch: u64, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let mut last_generation: Option<(u64, u64, u64, u64)> = None;
            let mut changed_at: Option<Instant> = None;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(180))
                    .await;
                let Ok(generation) = this.update(cx, |app, _cx| app.plugin_panel_generation(watch))
                else {
                    break;
                };
                let Some(generation) = generation else {
                    break;
                };
                match last_generation {
                    None => last_generation = Some(generation),
                    Some(previous) if previous != generation => {
                        last_generation = Some(generation);
                        changed_at = Some(Instant::now());
                    }
                    Some(_) => {
                        if changed_at
                            .is_some_and(|since| since.elapsed() >= Duration::from_millis(150))
                        {
                            changed_at = None;
                            let _ = this.update_in(cx, |app, window, cx| {
                                app.refresh_watched_plugin_panel(watch, window, cx);
                            });
                        }
                    }
                }
            }
        })
        .detach();
    }

    /// Start resizing the open panel from its inner edge.
    fn begin_plugin_panel_drag(
        &mut self,
        origin_x: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.plugin_panel.as_ref() else {
            return;
        };
        let origin_width = fit_plugin_panel_width(panel.width, window.bounds().size.width.as_f32());
        self.plugin_panel_drag = Some(PluginPanelDrag {
            panel_key: plugin_panel_width_key(&panel.plugin_id, &panel.panel_id),
            origin_x,
            origin_width,
        });
        cx.notify();
    }

    /// Follow the pointer while the panel edge is held.
    fn drag_plugin_panel_to(
        &mut self,
        position_x: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.plugin_panel_drag.clone() else {
            return;
        };
        let Some(panel) = self.plugin_panel.as_mut() else {
            return;
        };
        if plugin_panel_width_key(&panel.plugin_id, &panel.panel_id) != drag.panel_key {
            return;
        }
        // The edge moves with the pointer, so dragging left widens the panel.
        let width = plugin_panel_drag_width(
            drag.origin_width,
            drag.origin_x.as_f32(),
            position_x.as_f32(),
            window.bounds().size.width.as_f32(),
        );
        if (panel.width - width).abs() > f32::EPSILON {
            panel.width = width;
            cx.notify();
        }
    }

    /// Remember the width the user dragged to and stop following the pointer.
    fn end_plugin_panel_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.plugin_panel_drag.take() else {
            return;
        };
        let width = self
            .plugin_panel
            .as_ref()
            .filter(|panel| {
                plugin_panel_width_key(&panel.plugin_id, &panel.panel_id) == drag.panel_key
            })
            .map(|panel| panel.width);
        if let Some(width) = width.filter(|width| (width - drag.origin_width).abs() > 0.5) {
            self.settings
                .plugin_panel_widths
                .insert(drag.panel_key, width);
            self.commit_settings(cx);
        }
        cx.notify();
    }

    fn render_plugin_panel_content(
        &mut self,
        plugin_id: &str,
        panel_id: &str,
        watch: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = self.plugin_host.settings_for(plugin_id);
        let input = luban_plugin::runtime::CallInput {
            plugin: luban_plugin::runtime::PluginIdentity {
                id: plugin_id.to_string(),
                version: self
                    .plugin_host
                    .manifest(plugin_id)
                    .map(|manifest| manifest.version.clone())
                    .unwrap_or_default(),
                data_dir: self.plugin_host.root.join(plugin_id).join("data"),
            },
            document: self.plugin_document_snapshot(cx),
            settings,
            allowed_roots: self.plugin_allowed_roots(),
            command_depth: 0,
            context_kind: luban_plugin::runtime::CallContextKind::default(),
        };
        let Some(call) = self.plugin_host.panel_content(plugin_id, panel_id, input) else {
            self.set_status(
                tr_args(
                    "plugins.panel_not_available",
                    &[("panel", panel_id.to_string())],
                ),
                cx,
            );
            return;
        };
        let mut error = call.error.clone();
        for action in call.output.actions {
            match action {
                luban_plugin::runtime::HostAction::SetDocumentText { .. } => {
                    log::warn!(
                        "plugin panel '{panel_id}' attempted to modify the document; action ignored"
                    );
                }
                luban_plugin::runtime::HostAction::Notify(message) => self.set_status(message, cx),
                luban_plugin::runtime::HostAction::SetStatus { id, text } => {
                    self.plugin_host.set_status(plugin_id, &id, text);
                }
                luban_plugin::runtime::HostAction::OpenPanel { id } => {
                    log::warn!("plugin panel '{panel_id}' requested panel '{id}'; action ignored");
                }
                luban_plugin::runtime::HostAction::RefreshPanel { id } => {
                    log::debug!(
                        "plugin panel '{panel_id}' requested refresh '{id}' (not wired yet)"
                    );
                }
                luban_plugin::runtime::HostAction::SetCursor { .. } => {
                    log::debug!("plugin cursor action (not wired yet)");
                }
                luban_plugin::runtime::HostAction::SetSetting { key, .. } => {
                    log::debug!("plugin setting write-back '{key}' (not wired yet)");
                }
                luban_plugin::runtime::HostAction::RunCommand { id, .. } => {
                    log::debug!("plugin requested command run '{id}' (not wired yet)");
                }
            }
        }
        if let Some(message) = &error {
            self.set_status(message.clone(), cx);
        }
        let panel_spec = self
            .plugin_host
            .panels
            .iter()
            .find(|panel| panel.plugin_id == plugin_id && panel.id == panel_id)
            .cloned();
        let manifest_title = panel_spec
            .as_ref()
            .map(|panel| panel.title.clone())
            .unwrap_or_else(|| luban_plugin::localized::LocalizedText::new(panel_id));
        let placement = panel_spec
            .as_ref()
            .map(|panel| panel.placement)
            .unwrap_or(luban_plugin::manifest::PanelPlacement::Right);
        // Reuse the width of the panel that is already open so a refresh does
        // not undo a drag; otherwise use the width remembered for this panel,
        // then the manifest default on first open.
        let width = self
            .plugin_panel
            .as_ref()
            .filter(|panel| panel.plugin_id == plugin_id && panel.panel_id == panel_id)
            .map(|panel| panel.width)
            .unwrap_or_else(|| {
                plugin_panel_open_width(
                    &self.settings,
                    plugin_id,
                    panel_id,
                    panel_spec.as_ref().and_then(|panel| panel.width),
                )
            });
        let content_title = call
            .value
            .as_ref()
            .and_then(|panel| panel.title.clone())
            .filter(|title| !title.is_blank());
        let truncated = call.value.as_ref().is_some_and(|panel| panel.truncated);
        let actions = call
            .value
            .as_ref()
            .map(|panel| panel.actions.clone())
            .unwrap_or_default();
        let lines = call.value.map(|panel| panel.lines).unwrap_or_default();
        // Reuse the scroll position for the same panel so live refreshes do
        // not jump back to the top; a newly opened panel starts at the top.
        let scroll = self
            .plugin_panel
            .as_ref()
            .filter(|panel| panel.plugin_id == plugin_id && panel.panel_id == panel_id)
            .map(|panel| panel.scroll.clone())
            .unwrap_or_default();
        self.plugin_panel = Some(PluginPanelState {
            plugin_id: plugin_id.to_string(),
            panel_id: panel_id.to_string(),
            title: content_title.unwrap_or(manifest_title),
            placement,
            width,
            truncated,
            lines,
            actions,
            error: error.take(),
            watch,
            scroll,
        });
        cx.notify();
    }

    pub(super) fn render_plugin_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(panel) = &self.plugin_panel else {
            return div().into_any_element();
        };
        let title = plugin_text(&panel.title);
        let lines = panel.lines.clone();
        let actions = panel.actions.clone();
        let empty = lines.is_empty();
        let error = panel.error.clone();
        let has_error = error.is_some();
        let truncated = panel.truncated;
        let placement = panel.placement;
        let width = panel.width;
        let scroll = panel.scroll.clone();
        let focus = self.plugin_panel_focus.clone();
        let plugin_id = panel.plugin_id.clone();
        let panel_id = panel.panel_id.clone();
        let panel_key = plugin_panel_width_key(&plugin_id, &panel_id);
        let code_font_family = if self.settings.font_family.trim().is_empty() {
            cx.theme().mono_font_family.clone()
        } else {
            SharedString::from(self.settings.font_family.clone())
        };
        let base_font = window.text_style().font();
        let panel_width = match placement {
            luban_plugin::manifest::PanelPlacement::Right => {
                fit_plugin_panel_width(width, window.bounds().size.width.as_f32())
            }
            luban_plugin::manifest::PanelPlacement::Bottom => window.bounds().size.width.as_f32(),
        };
        // The panel content is inset by 20px on each side; keep images clear
        // of the always-visible scrollbar on the right as well.
        let image_max_width = (panel_width - 48.0).max(64.0);
        let document_dir = self
            .active_doc()
            .and_then(|doc| doc.path.as_deref())
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let weak_close = cx.weak_entity();
        let weak_refresh = cx.weak_entity();
        let weak_focus = cx.weak_entity();
        let rows: Vec<AnyElement> = Self::plugin_panel_rows(
            lines,
            document_dir.as_deref(),
            image_max_width,
            &code_font_family,
            &base_font,
            cx,
        );
        let weak_action = cx.weak_entity();
        let action_buttons = actions.into_iter().enumerate().map(move |(index, action)| {
            let weak = weak_action.clone();
            let label = plugin_text(&action.label);
            let command = action.command;
            Button::new(("plugin-panel-action", index))
                .label(label)
                .ghost()
                .compact()
                .on_click(move |_, window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        // Like a line click: the panel stays open with its
                        // scroll position and focus does not move.
                        if !this.run_command_with_args(
                            &command.id,
                            command.args.clone(),
                            window,
                            cx,
                        ) {
                            this.set_status(
                                tr_args(
                                    "status.command_failed",
                                    &[("command", command.id.clone())],
                                ),
                                cx,
                            );
                        }
                    });
                })
        });
        let content = div()
            .flex()
            .flex_col()
            .h_full()
            .gap_2()
            .p_5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .min_w_0()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(title),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap_2()
                            .children(action_buttons)
                            .child(
                                Button::new("plugin-panel-refresh")
                                    .label(tr("plugins.refresh"))
                                    .compact()
                                    .on_click(move |_, window, cx| {
                                        let plugin_id = plugin_id.clone();
                                        let panel_id = panel_id.clone();
                                        let _ = weak_refresh.update(cx, |this, cx| {
                                            this.refresh_plugin_panel(
                                                &plugin_id, &panel_id, window, cx,
                                            );
                                        });
                                    }),
                            )
                            .child(
                                Button::new("plugin-panel-close")
                                    .label(tr("common.close"))
                                    .compact()
                                    .on_click(move |_, window, cx| {
                                        let _ = weak_close.update(cx, |this, cx| {
                                            this.plugin_panel = None;
                                            this.plugin_panel_drag = None;
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
                        .text_xs()
                        .text_color(cx.theme().colors.danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        div()
                            .id("plugin-panel-lines")
                            .size_full()
                            .flex()
                            .flex_col()
                            .track_scroll(&scroll)
                            .track_focus(&focus)
                            .overflow_y_scroll()
                            .children(rows),
                    )
                    .child(
                        Scrollbar::vertical(&scroll)
                            .id("plugin-panel-scrollbar")
                            .mode(ScrollbarMode::Always),
                    ),
            )
            .when(empty && !has_error, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(tr("plugins.panel_empty")),
                )
            })
            .when(truncated, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(tr("plugins.panel_truncated")),
                )
            });
        let card = div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .bg(cx.theme().colors.background)
            // Clicking the panel focuses its scroll area; the editor keeps
            // focus until the user clicks the preview. The panel may have been
            // closed by a row's command before this bubbles up.
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                let _ = weak_focus.update(cx, |this, cx| {
                    if this.plugin_panel.is_some() {
                        this.plugin_panel_focus.focus(window, cx);
                    }
                });
            })
            .child(content);
        match placement {
            luban_plugin::manifest::PanelPlacement::Right => {
                let width = panel_width;
                let dragging = self
                    .plugin_panel_drag
                    .as_ref()
                    .is_some_and(|drag| drag.panel_key == panel_key);
                let handle_hover = cx.theme().colors.secondary;
                let handle_active = cx.theme().colors.selection;
                let weak_drag_start = cx.weak_entity();
                let weak_drag_move = cx.weak_entity();
                let weak_drag_end = cx.weak_entity();
                // The band straddles the panel edge so the visible divider is
                // what the pointer grabs.
                let handle = div()
                    .id("plugin-panel-resize-handle")
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(-PLUGIN_PANEL_HANDLE_WIDTH / 2.0))
                    .w(px(PLUGIN_PANEL_HANDLE_WIDTH))
                    .occlude()
                    .cursor(CursorStyle::ResizeLeftRight)
                    .hover(move |this| this.bg(handle_hover))
                    .when(dragging, move |this| this.bg(handle_active))
                    .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                        let _ = weak_drag_start.update(cx, |this, cx| {
                            this.begin_plugin_panel_drag(event.position.x, window, cx);
                        });
                    })
                    .child(div().size_full().child(canvas(
                        |_, _, _| (),
                        move |_, _, window, _cx| {
                            // A panel edge has no room to follow the pointer, so
                            // the drag is tracked on the window instead.
                            let weak_move = weak_drag_move.clone();
                            window.on_mouse_event(
                                move |event: &MouseMoveEvent, phase, window, cx| {
                                    if !phase.bubble() {
                                        return;
                                    }
                                    let held = event.pressed_button == Some(MouseButton::Left);
                                    let _ = weak_move.update(cx, |this, cx| {
                                        if held {
                                            this.drag_plugin_panel_to(event.position.x, window, cx);
                                        } else {
                                            // The button came up outside the window,
                                            // so its mouse-up never arrived: finish
                                            // the gesture on the next move instead of
                                            // leaving the edge highlighted.
                                            this.end_plugin_panel_drag(cx);
                                        }
                                    });
                                },
                            );
                            window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                                if !phase.bubble() {
                                    return;
                                }
                                let _ = weak_drag_end.update(cx, |this, cx| {
                                    this.end_plugin_panel_drag(cx);
                                });
                            });
                        },
                    )));
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .w(px(width))
                    .occlude()
                    .border_l_1()
                    .border_color(cx.theme().colors.border)
                    .child(card)
                    .child(handle)
                    .into_any_element()
            }
            luban_plugin::manifest::PanelPlacement::Bottom => div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(24.))
                .h(px(260.))
                .occlude()
                .border_t_1()
                .border_color(cx.theme().colors.border)
                .child(card)
                .into_any_element(),
        }
    }

    /// Build one rich panel row. `text` is the plain fallback; `spans` and
    /// `style` carry the presentation added in 1.0.0-preview.2. Consecutive
    /// code lines and table rows are grouped into block widgets here so the
    /// preview reads like an HTML render instead of a list of monospace rows.
    fn plugin_panel_rows(
        lines: Vec<luban_plugin::runtime::PanelLine>,
        document_dir: Option<&Path>,
        image_max_width: f32,
        code_font_family: &SharedString,
        base_font: &Font,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        use luban_plugin::runtime::PanelLineStyle;

        let mut rows = Vec::new();
        let mut iter = lines.into_iter().peekable();
        while let Some(line) = iter.next() {
            if line.style == PanelLineStyle::Image {
                rows.push(Self::plugin_panel_image(
                    &line,
                    document_dir,
                    image_max_width,
                    base_font,
                    cx,
                ));
            } else if line.style == PanelLineStyle::Table && !line.cells.is_empty() {
                let mut table = vec![line];
                while iter.peek().is_some_and(|next| {
                    next.style == PanelLineStyle::Table
                        && !next.cells.is_empty()
                        && !next.break_before
                }) {
                    table.push(iter.next().expect("peeked table row"));
                }
                rows.push(Self::plugin_panel_table(
                    &table,
                    code_font_family,
                    base_font,
                    cx,
                ));
            } else if line.style == PanelLineStyle::Code {
                let mut block = vec![line];
                while iter
                    .peek()
                    .is_some_and(|next| next.style == PanelLineStyle::Code && !next.break_before)
                {
                    block.push(iter.next().expect("peeked code line"));
                }
                rows.push(Self::plugin_panel_code_block(
                    &block,
                    code_font_family,
                    base_font,
                    cx,
                ));
            } else {
                rows.push(Self::plugin_panel_line(
                    line,
                    code_font_family,
                    base_font,
                    cx,
                ));
            }
        }
        rows
    }

    /// Wrap a panel element with the click behavior of its line command. Used
    /// by plain lines and by grouped block widgets (code lines, table rows,
    /// images) so grouping never drops interactions that worked before.
    fn plugin_command_element(
        element: Div,
        command: Option<luban_plugin::runtime::PanelCommand>,
        hover: gpui_kit::Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(command) = command else {
            return element.into_any_element();
        };
        let weak = cx.weak_entity();
        element
            .cursor_pointer()
            .hover(move |this| this.bg(hover))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    // v2: the panel stays open, scroll position is preserved,
                    // editor focus is not stolen.
                    if !this.run_command_with_args(&command.id, command.args.clone(), window, cx) {
                        this.set_status(
                            tr_args("status.command_failed", &[("command", command.id.clone())]),
                            cx,
                        );
                    }
                });
            })
            .into_any_element()
    }

    /// Render consecutive table rows as one bordered, full-width table with a
    /// tinted header row. Column widths follow the per-cell `weight` the
    /// plugin derived from the widest cell in each column.
    fn plugin_panel_table(
        rows: &[luban_plugin::runtime::PanelLine],
        code_font_family: &SharedString,
        base_font: &Font,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use gpui_kit::TextAlign;
        use luban_plugin::runtime::PanelCellAlign;

        let theme = cx.theme().clone();
        let colors = theme.colors;
        let highlight = theme.highlight_theme.clone();
        let row_count = rows.len();
        let mut row_elements = Vec::with_capacity(row_count);
        for (row_index, row) in rows.iter().enumerate() {
            let is_header = row.cells.iter().any(|cell| cell.header);
            let cell_count = row.cells.len();
            let mut cell_elements = Vec::with_capacity(cell_count);
            for (cell_index, cell) in row.cells.iter().enumerate() {
                let text = Self::panel_styled_text(
                    &cell.text,
                    &cell.spans,
                    base_font,
                    code_font_family,
                    colors,
                    &highlight,
                    colors.foreground,
                    cell.header,
                    false,
                );
                let mut cell_element = div()
                    .flex_grow(f32::from(cell.weight.max(1)))
                    .flex_basis(px(0.0))
                    .flex_shrink_1()
                    .min_w(px(0.0))
                    .px(px(12.0))
                    .py(px(7.0))
                    .text_size(px(14.0))
                    .line_height(relative(1.45))
                    .text_align(match cell.align {
                        PanelCellAlign::Left => TextAlign::Left,
                        PanelCellAlign::Center => TextAlign::Center,
                        PanelCellAlign::Right => TextAlign::Right,
                    });
                if cell.header {
                    cell_element = cell_element.font_weight(FontWeight::BOLD);
                }
                if cell_index + 1 < cell_count {
                    cell_element = cell_element.border_r_1().border_color(colors.border);
                }
                cell_elements.push(cell_element.child(text).into_any_element());
            }
            let mut row_element = div()
                .flex()
                .w_full()
                .items_stretch()
                .children(cell_elements);
            if is_header {
                row_element = row_element.bg(colors.secondary.opacity(0.55));
            } else if row_index % 2 == 1 {
                // GitHub/VSCode zebra striping: tint every second body row.
                row_element = row_element.bg(colors.secondary.opacity(0.28));
            }
            if row_index + 1 < row_count {
                row_element = row_element.border_b_1().border_color(colors.border);
            }
            row_elements.push(Self::plugin_command_element(
                row_element,
                row.command.clone(),
                colors.secondary.opacity(0.5),
                cx,
            ));
        }
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .my(px(8.0))
            .border_1()
            .border_color(colors.border)
            .rounded_md()
            .overflow_hidden()
            .children(row_elements)
            .into_any_element()
    }

    /// Render consecutive code lines as one rounded, padded block instead of
    /// one tinted strip per line.
    fn plugin_panel_code_block(
        lines: &[luban_plugin::runtime::PanelLine],
        code_font_family: &SharedString,
        base_font: &Font,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let highlight = theme.highlight_theme.clone();
        let syntax_background = panel_code_background(&highlight, colors);
        let syntax_foreground = highlight
            .style
            .editor_foreground
            .unwrap_or(colors.foreground);
        let mut line_elements = Vec::with_capacity(lines.len());
        for line in lines {
            let text = Self::panel_styled_text(
                &line.text,
                &line.spans,
                base_font,
                code_font_family,
                colors,
                &highlight,
                syntax_foreground,
                false,
                true,
            );
            line_elements.push(Self::plugin_command_element(
                div()
                    .w_full()
                    .min_h(px(17.0))
                    .text_size(px(12.5))
                    .line_height(relative(1.5))
                    .child(text),
                line.command.clone(),
                colors.secondary.opacity(0.5),
                cx,
            ));
        }
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .my(px(8.0))
            .px(px(16.0))
            .py(px(12.0))
            .bg(syntax_background)
            .rounded_md()
            .children(line_elements)
            .into_any_element()
    }

    /// Render a local image block. Remote URLs, missing files and paths that
    /// escape the document's directory fall back to the alt text, matching the
    /// preview's no-network policy without hiding the source's intent.
    fn plugin_panel_image(
        line: &luban_plugin::runtime::PanelLine,
        document_dir: Option<&Path>,
        max_width: f32,
        _base_font: &Font,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let alt = if line.text.trim().is_empty() {
            "image".to_string()
        } else {
            line.text.trim().to_string()
        };
        let fallback = move || {
            div()
                .w_full()
                .px(px(8.0))
                .py(px(2.0))
                .text_size(px(13.0))
                .text_color(colors.muted_foreground)
                .child(format!("🖼 {alt}"))
                .into_any_element()
        };
        let Some(path) = line
            .image
            .as_deref()
            .and_then(|source| resolve_panel_image_source(source, document_dir))
        else {
            return div()
                .w_full()
                .my(px(10.0))
                .child(fallback())
                .into_any_element();
        };
        let alt_for_failure = if line.text.trim().is_empty() {
            "image".to_string()
        } else {
            line.text.trim().to_string()
        };
        let fallback_color = colors.muted_foreground;
        let Some((width, height)) = image::image_dimensions(&path).ok() else {
            return div()
                .w_full()
                .my(px(10.0))
                .child(fallback())
                .into_any_element();
        };
        if width == 0 || height == 0 {
            return div()
                .w_full()
                .my(px(10.0))
                .child(fallback())
                .into_any_element();
        }
        // GPUI's `max_w` clamps the paint but keeps the intrinsic height in
        // the layout, which leaves a large gap under wide images. Scale both
        // dimensions from the file header instead; never upscale.
        let scale = (max_width / width as f32).min(1.0);
        let display_width = (width as f32 * scale).max(1.0);
        let display_height = (height as f32 * scale).max(1.0);
        Self::plugin_command_element(
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .w_full()
                .my(px(8.0))
                .child(
                    img(path)
                        .w(px(display_width))
                        .h(px(display_height))
                        .rounded_md()
                        .with_fallback(move || {
                            div()
                                .px(px(8.0))
                                .text_size(px(13.0))
                                .text_color(fallback_color)
                                .child(format!("🖼 {alt_for_failure}"))
                                .into_any_element()
                        }),
                ),
            line.command.clone(),
            colors.secondary.opacity(0.5),
            cx,
        )
    }

    /// Build the styled text shared by panel lines, code blocks and table
    /// cells. Runs inherit their size and line height from the parent
    /// element; only font family/weight/style and colors live here.
    #[allow(clippy::too_many_arguments)]
    fn panel_styled_text(
        text: &str,
        spans: &[luban_plugin::runtime::PanelSpan],
        base_font: &Font,
        code_font_family: &SharedString,
        colors: gpui_kit::component::theme::ThemeColor,
        highlight: &gpui_kit::component::highlighter::HighlightTheme,
        base_color: gpui_kit::Hsla,
        force_bold: bool,
        code_family: bool,
    ) -> StyledText {
        use luban_plugin::runtime::{PanelSpan, PanelSpanStyle};

        let default_span;
        let spans: &[PanelSpan] = if spans.is_empty() {
            default_span = PanelSpan {
                text: text.to_string(),
                style: PanelSpanStyle::default(),
                role: None,
            };
            std::slice::from_ref(&default_span)
        } else {
            spans
        };
        let mut runs: Vec<TextRun> = Vec::new();
        for span in spans {
            if span.text.is_empty() {
                continue;
            }
            let role = span.role.as_deref().and_then(|role| highlight.style(role));
            let mut font = base_font.clone();
            if code_family || span.style.code() {
                font.family = code_font_family.clone();
            }
            if let Some(style) = &role {
                if let Some(weight) = style.font_weight {
                    font.weight = weight;
                }
                if let Some(style) = style.font_style {
                    font.style = style;
                }
            }
            if force_bold || span.style.bold() {
                font.weight = FontWeight::BOLD;
            }
            if span.style.italic() {
                font.style = FontStyle::Italic;
            }
            let color = if span.style.link() {
                colors.link
            } else if let Some(style) = &role {
                style.color.unwrap_or(base_color)
            } else if span.style.muted() {
                colors.muted_foreground
            } else {
                base_color
            };
            let background_color = span.style.code().then_some(colors.secondary.opacity(0.65));
            let underline =
                (span.style.link() || span.style.underline()).then_some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(color),
                    wavy: false,
                });
            let strikethrough = span.style.strike().then_some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(color),
            });
            runs.push(TextRun {
                len: span.text.len(),
                font,
                color,
                background_color,
                underline,
                strikethrough,
            });
        }
        let mut styled = StyledText::new(text.to_string());
        if !runs.is_empty() {
            styled = styled.with_runs(runs);
        }
        styled
    }

    /// Build one rich panel row. `text` is the plain fallback; `spans` and
    /// `style` carry the presentation. Sizing follows the VSCode preview's
    /// proportions: 14px body text at 1.6 line height, 28/21/17px headings,
    /// rules under H1/H2, and a 6px-radius code surface.
    fn plugin_panel_line(
        line: luban_plugin::runtime::PanelLine,
        code_font_family: &SharedString,
        base_font: &Font,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use luban_plugin::runtime::PanelLineStyle;

        let theme = cx.theme().clone();
        let colors = theme.colors;
        let highlight = theme.highlight_theme.clone();
        let syntax_foreground = highlight
            .style
            .editor_foreground
            .unwrap_or(colors.foreground);

        let tight = line.tight;
        let mut base_color = colors.foreground;
        let mut code_family = false;
        let mut force_bold = false;
        let mut element = div().px(px(8.0)).rounded_sm();
        match line.style {
            PanelLineStyle::Text => {
                element = element.text_size(px(14.0)).line_height(relative(1.6));
                element = if tight {
                    element.py(px(0.0)).mb(px(0.0))
                } else {
                    element.py(px(2.0)).mb(px(10.0))
                };
            }
            PanelLineStyle::Muted => {
                base_color = colors.muted_foreground;
                element = element.text_size(px(13.0)).line_height(relative(1.6));
                element = if tight {
                    element.py(px(0.0)).mb(px(0.0))
                } else {
                    element.py(px(2.0)).mb(px(10.0))
                };
            }
            PanelLineStyle::H1 => {
                force_bold = true;
                element = element
                    .text_size(px(28.0))
                    .line_height(relative(1.25))
                    .font_weight(FontWeight::BOLD)
                    .pt(px(20.0))
                    .pb(px(6.0))
                    .mb(px(14.0))
                    .border_b_1()
                    .border_color(colors.border);
            }
            PanelLineStyle::H2 => {
                force_bold = true;
                element = element
                    .text_size(px(21.0))
                    .line_height(relative(1.3))
                    .font_weight(FontWeight::BOLD)
                    .pt(px(16.0))
                    .pb(px(4.0))
                    .mb(px(10.0))
                    .border_b_1()
                    .border_color(colors.border);
            }
            PanelLineStyle::H3 => {
                force_bold = true;
                element = element
                    .text_size(px(17.0))
                    .line_height(relative(1.35))
                    .font_weight(FontWeight::BOLD)
                    .pt(px(12.0))
                    .pb(px(3.0))
                    .mb(px(6.0));
            }
            PanelLineStyle::H4 => {
                force_bold = true;
                element = element
                    .text_size(px(14.0))
                    .line_height(relative(1.4))
                    .font_weight(FontWeight::BOLD)
                    .pt(px(10.0))
                    .pb(px(2.0))
                    .mb(px(4.0));
            }
            PanelLineStyle::H5 => {
                force_bold = true;
                element = element
                    .text_size(px(12.5))
                    .line_height(relative(1.4))
                    .font_weight(FontWeight::BOLD)
                    .pt(px(8.0))
                    .pb(px(2.0))
                    .mb(px(3.0));
            }
            PanelLineStyle::H6 => {
                base_color = colors.muted_foreground;
                force_bold = true;
                element = element
                    .text_size(px(12.0))
                    .line_height(relative(1.4))
                    .font_weight(FontWeight::BOLD)
                    .pt(px(8.0))
                    .pb(px(2.0))
                    .mb(px(3.0));
            }
            PanelLineStyle::Code => {
                base_color = syntax_foreground;
                code_family = true;
                element = element
                    .text_size(px(12.5))
                    .line_height(relative(1.5))
                    .py(px(1.0));
            }
            PanelLineStyle::Quote => {
                base_color = colors.muted_foreground;
                element = element
                    .text_size(px(14.0))
                    .line_height(relative(1.6))
                    .border_l(px(3.0))
                    .border_color(colors.border)
                    .pl(px(14.0 + f32::from(line.indent) * 16.0))
                    .py(px(2.0));
                if !tight {
                    element = element.mb(px(8.0));
                }
            }
            PanelLineStyle::List => {
                element = element
                    .text_size(px(14.0))
                    .line_height(relative(1.6))
                    .pl(px(18.0 + f32::from(line.indent) * 20.0))
                    .py(px(2.0));
                if !tight {
                    element = element.mb(px(3.0));
                }
            }
            PanelLineStyle::Table => {
                code_family = true;
                element = element
                    .text_size(px(12.5))
                    .line_height(relative(1.45))
                    .py(px(1.0));
            }
            PanelLineStyle::Image => {
                base_color = colors.muted_foreground;
                element = element
                    .text_size(px(13.0))
                    .line_height(relative(1.6))
                    .py(px(2.0))
                    .mb(px(10.0));
            }
            PanelLineStyle::Rule => {
                return div()
                    .my(px(16.0))
                    .child(div().h(px(2.0)).w_full().rounded_full().bg(colors.border))
                    .into_any_element();
            }
        }
        if !matches!(
            line.style,
            PanelLineStyle::Quote | PanelLineStyle::List | PanelLineStyle::Table
        ) {
            element = element.pl(px(8.0 + f32::from(line.indent) * 14.0));
        }

        let text = Self::panel_styled_text(
            &line.text,
            &line.spans,
            base_font,
            code_font_family,
            colors,
            &highlight,
            base_color,
            force_bold,
            code_family,
        );
        let element = element.child(text);
        Self::plugin_command_element(element, line.command, colors.secondary, cx)
    }
}

/// Pick the code-card surface for the active syntax theme.
///
/// Syntax roles are colored for the theme's editor background. When that
/// background clearly differs from the panel (a light syntax theme in a dark
/// UI, or the reverse), using it keeps the roles readable; otherwise a subtle
/// tint keeps light-theme code cards distinct from the page.
fn panel_code_background(
    highlight: &gpui_kit::component::highlighter::HighlightTheme,
    colors: gpui_kit::component::theme::ThemeColor,
) -> gpui_kit::Hsla {
    if let Some(background) = highlight.style.editor_background {
        if (background.l - colors.background.l).abs() > 0.12 {
            return background;
        }
    }
    colors.secondary.opacity(0.6)
}

/// Resolve a Markdown image source to a local file the preview may read.
///
/// Only files inside the active document's directory (after canonicalization,
/// so `..` and symlinks cannot escape it) with an image extension the GPUI
/// image decoder supports are accepted. Remote URLs, `data:` URIs, missing
/// files and out-of-tree paths return `None`, and the caller shows the alt
/// text instead.
fn resolve_panel_image_source(source: &str, document_dir: Option<&Path>) -> Option<PathBuf> {
    let source = source.trim();
    if source.is_empty() {
        return None;
    }
    let lower = source.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("data:")
        || source.starts_with("//")
    {
        return None;
    }
    let root = document_dir?.canonicalize().ok()?;
    let raw = PathBuf::from(source);
    let candidate = if raw.is_absolute() {
        raw
    } else {
        root.join(raw)
    };
    let canonical = candidate.canonicalize().ok()?;
    if !canonical.starts_with(&root) || !canonical.is_file() {
        return None;
    }
    let extension = canonical
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())?;
    if !gpui_kit::Img::extensions()
        .iter()
        .any(|known| *known == extension)
    {
        return None;
    }
    Some(canonical)
}

#[cfg(test)]
mod panel_image_tests {
    use super::resolve_panel_image_source;
    use std::fs;

    #[test]
    fn resolves_files_inside_the_document_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("docs");
        fs::create_dir(&nested).unwrap();
        let image = nested.join("logo.png");
        fs::write(&image, b"not a real png").unwrap();
        let resolved = resolve_panel_image_source("docs/logo.png", Some(dir.path()));
        assert_eq!(resolved, Some(image.canonicalize().unwrap()));
    }

    #[test]
    fn rejects_remote_escape_missing_and_non_image_sources() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_image = outside.path().join("secret.png");
        fs::write(&outside_image, b"x").unwrap();
        assert_eq!(
            resolve_panel_image_source("https://example.com/logo.png", Some(dir.path())),
            None
        );
        assert_eq!(
            resolve_panel_image_source("data:image/png;base64,xx", Some(dir.path())),
            None
        );
        assert_eq!(
            resolve_panel_image_source("../secret.png", Some(dir.path())),
            None
        );
        assert_eq!(
            resolve_panel_image_source("missing.png", Some(dir.path())),
            None
        );
        let text_file = dir.path().join("readme.md");
        fs::write(&text_file, b"text").unwrap();
        assert_eq!(
            resolve_panel_image_source("readme.md", Some(dir.path())),
            None
        );
        assert_eq!(resolve_panel_image_source("logo.png", None), None);
    }
}

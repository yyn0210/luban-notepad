use super::*;

impl LubanApp {
    // Find/replace UI: find bar, replace previews, workspace replace and find-in-files

    pub(super) fn mark_find_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active(window, cx);
        if !self.full_text_editing_available(window, cx) {
            return;
        }
        if self.find.matches.is_empty() {
            self.set_status(tr("status.no_matches_to_mark"), cx);
            return;
        }
        let index = self.active;
        let mut lines = HashSet::new();
        {
            let Some(tab) = self.tabs.get(index) else {
                return;
            };
            for matched in &self.find.matches {
                lines.insert(line_ops::line_of_char(&tab.doc.text, matched.start));
            }
        }
        let added = {
            let Some(tab) = self.tabs.get_mut(index) else {
                return;
            };
            let mut added = 0usize;
            for line in lines.iter().copied() {
                if tab.bookmarks.toggle(line) {
                    added += 1;
                }
            }
            tab.bookmark_revision = tab.bookmark_revision.wrapping_add(1);
            added
        };
        self.refresh_bookmark_decorations(index, cx);
        self.set_status(
            tr_args("status.marked_matches", &[("count", added.to_string())]),
            cx,
        );
    }
    pub(super) fn clear_search_history(&mut self, cx: &mut Context<Self>) {
        self.settings.search_history.clear();
        self.settings.replace_history.clear();
        self.commit_settings();
        self.set_status(tr("status.search_history_cleared"), cx);
        cx.notify();
    }
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
        self.settings.record_search(&query);
        self.commit_settings();
        self.record_navigation(cx);
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
                self.record_navigation(cx);
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
        self.settings.record_search(&query);
        self.settings.record_replacement(&replacement);
        self.commit_settings();
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
        self.settings.record_search(&query);
        self.settings.record_replacement(&replacement);
        self.commit_settings();
        if !self.ensure_active_text(cx) {
            return;
        }
        let preview = {
            let text = self
                .tabs
                .get(self.active)
                .map(|tab| tab.doc.text.as_str())
                .unwrap_or("");
            self.find
                .engine(&query)
                .preview_replace(text, &replacement, self.find.scope, 5)
        };
        self.release_active_text();
        match preview {
            Ok(preview) => {
                self.replace_preview = Some(ReplacePreviewState { preview });
                cx.notify();
            }
            Err(err) => {
                self.find.error = Some(err);
                cx.notify();
            }
        }
    }
    pub(super) fn confirm_replace_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let empty = self
            .replace_preview
            .as_ref()
            .is_some_and(|state| state.preview.count == 0);
        self.replace_preview = None;
        if empty {
            cx.notify();
            return;
        }
        self.apply_replace_all(window, cx);
    }
    fn apply_replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        self.settings.record_search(&query);
        self.settings.record_replacement(&replacement);
        self.commit_settings();
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
    pub fn open_workspace_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find_in_files.workspace = !self.workspace_roots.is_empty();
        self.find_in_files.show_replace = true;
        self.find_in_files.respect_gitignore = self.settings.respect_gitignore;
        self.find_in_files.open = true;
        self.find_in_files
            .replacement
            .update(cx, |input, cx| input.focus(window, cx));
        self.find_in_files
            .query
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }
    /// Signature of every input the replace preview depends on.
    fn replace_signature(&self, cx: &gpui_kit::App) -> String {
        let panel = &self.find_in_files;
        format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            panel.query_text(cx),
            panel.replacement_text(cx),
            panel.include_text(cx),
            panel.exclude_text(cx),
            panel.match_case,
            panel.whole_word,
            panel.regex,
            panel.enhanced_regex,
            panel.workspace,
            panel.respect_gitignore,
        )
    }
    /// Drop a preview whose inputs changed after it was computed.
    fn invalidate_stale_preview(&mut self, cx: &mut Context<Self>) {
        if self.replace_state.preview.is_some()
            && self.replace_state.preview_signature != self.replace_signature(cx)
        {
            self.replace_state.preview = None;
            self.replace_state.status = tr("replace.preview_stale");
            cx.notify();
        }
    }
    /// Compute the workspace replace preview in the background.
    pub fn start_workspace_replace_preview(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.replace_state.running {
            return;
        }
        let query = self.find_in_files.query_text(cx);
        let replacement = self.find_in_files.replacement_text(cx);
        if query.is_empty() {
            self.replace_state.error = Some(tr("ff.enter_query"));
            self.replace_state.status = tr("ff.enter_query");
            cx.notify();
            return;
        }
        let filter = match self.find_in_files.filter(cx) {
            Ok(filter) => filter,
            Err(error) => {
                self.replace_state.error = Some(error.clone());
                self.replace_state.status = error;
                cx.notify();
                return;
            }
        };
        if self.workspace_roots.is_empty() {
            self.replace_state.error = Some(tr("ff.no_workspace"));
            self.replace_state.status = tr("ff.no_workspace");
            cx.notify();
            return;
        }
        let roots = self.workspace_roots.clone();
        let search_options = self.find_in_files.search_options();
        let max_file_size = self.settings.max_file_size_bytes();
        let respect_gitignore = self.find_in_files.respect_gitignore;
        let include_hidden = self.settings.show_hidden_files;
        let mut skipped_open: Vec<(PathBuf, String)> = Vec::new();
        for tab in &self.tabs {
            let Some(path) = &tab.doc.path else { continue };
            let reason = if tab.doc.is_huge() {
                Some(tr("replace.skip_disk_engine"))
            } else if tab.doc.partial_range().is_some() {
                Some(tr("replace.skip_partial"))
            } else if tab.doc.is_non_text() {
                Some(tr("replace.skip_non_text"))
            } else if tab.large_file {
                Some(tr("replace.skip_large"))
            } else {
                None
            };
            if let Some(reason) = reason {
                skipped_open.push((path.clone(), reason));
            }
        }
        self.replace_state.running = true;
        self.replace_state.error = None;
        self.replace_state.preview = None;
        self.replace_state.confirmed = false;
        self.replace_state.generation = self.replace_state.generation.wrapping_add(1);
        let generation = self.replace_state.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.replace_state.cancel = Some(cancel.clone());
        self.replace_state.query = query.clone();
        self.replace_state.replacement = replacement.clone();
        self.replace_state.options = Some(search_options);
        self.replace_state.filter = filter.clone();
        self.replace_state.preview_signature = self.replace_signature(cx);
        self.replace_state.status = tr("replace.previewing");
        cx.notify();

        let task = cx.background_executor().spawn(async move {
            let mut progress = |_files: usize| {};
            let options = WorkspaceSearchOptions {
                roots: roots.clone(),
                query: query.clone(),
                options: search_options,
                filter,
                respect_gitignore,
                include_hidden,
                max_results: 20_000,
                max_file_size,
            };
            let outcome = match search_workspace(&options, &cancel, &mut progress) {
                Ok(outcome) => outcome,
                Err(error) => return Err(error.to_string()),
            };
            let mut files: Vec<FilePreview> = Vec::new();
            let mut skipped: Vec<SkippedFile> = Vec::new();
            let mut seen: Vec<PathBuf> = Vec::new();
            for matched in &outcome.matches {
                if seen.iter().any(|path| path == &matched.path) {
                    continue;
                }
                seen.push(matched.path.clone());
                if let Some((_, reason)) =
                    skipped_open.iter().find(|(path, _)| path == &matched.path)
                {
                    skipped.push(SkippedFile {
                        path: matched.path.clone(),
                        reason: reason.clone(),
                    });
                    continue;
                }
                let bytes = match std::fs::read(&matched.path) {
                    Ok(bytes) => bytes,
                    Err(_) => continue,
                };
                if encoding::looks_binary(&bytes) {
                    skipped.push(SkippedFile {
                        path: matched.path.clone(),
                        reason: crate::i18n::tr("replace.skip_binary"),
                    });
                    continue;
                }
                let decoded = encoding::decode(&bytes);
                let engine = luban_core::search::SearchEngine::new(query.clone(), search_options);
                let (replaced, count) = match engine.replace_all(&decoded.text, &replacement, None)
                {
                    Ok(result) => result,
                    Err(_) => continue,
                };
                if count == 0 || replaced == decoded.text {
                    continue;
                }
                let samples: Vec<(String, String)> = outcome
                    .matches
                    .iter()
                    .filter(|candidate| candidate.path == matched.path)
                    .take(5)
                    .filter_map(|candidate| {
                        workspace_replace::replacement_sample(
                            &candidate.line_text,
                            &query,
                            search_options,
                            &replacement,
                        )
                    })
                    .collect();
                files.push(FilePreview {
                    path: matched.path.clone(),
                    relative: workspace_replace::relative_path(&roots, &matched.path),
                    matches: count,
                    samples,
                    fingerprint: workspace_replace::fingerprint(&bytes),
                    size: bytes.len() as u64,
                });
            }
            let total = files.iter().map(|file| file.matches).sum();
            Ok::<WorkspaceReplacePreview, String>(WorkspaceReplacePreview {
                files,
                skipped,
                total_matches: total,
                truncated: outcome.truncated,
            })
        });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let _ = this.update(cx, |app, cx| {
                if app.replace_state.generation != generation {
                    return;
                }
                app.replace_state.running = false;
                app.replace_state.cancel = None;
                match outcome {
                    Ok(preview) => {
                        app.replace_state.status = tr_args(
                            "replace.preview_ready",
                            &[
                                ("files", preview.file_count().to_string()),
                                ("matches", preview.total_matches.to_string()),
                                ("skipped", preview.skipped.len().to_string()),
                            ],
                        );
                        if preview.confirmable() {
                            app.replace_state.error = None;
                        } else {
                            app.replace_state.error = Some(tr("replace.no_matches"));
                        }
                        app.replace_state.preview = Some(preview);
                    }
                    Err(error) => {
                        app.replace_state.error = Some(error.clone());
                        app.replace_state.status = error;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub fn confirm_workspace_replace(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.invalidate_stale_preview(cx);
        let Some(preview) = &self.replace_state.preview else {
            self.replace_state.error = Some(tr("replace.preview_required"));
            cx.notify();
            return;
        };
        if !preview.confirmable() {
            return;
        }
        self.confirm = Some(ConfirmState::new(
            CloseIntent::WorkspaceReplace,
            tr("replace.confirm_title"),
            tr_args(
                "replace.confirm_detail",
                &[
                    ("files", preview.file_count().to_string()),
                    ("matches", preview.total_matches.to_string()),
                ],
            ),
        ));
        cx.notify();
    }
    /// Apply the computed preview after re-reading every file and comparing
    /// its fingerprint. In-memory buffers only; nothing is written to disk.
    pub(super) fn apply_workspace_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preview) = self.replace_state.preview.clone() else {
            return;
        };
        let query = self.replace_state.query.clone();
        let replacement = self.replace_state.replacement.clone();
        let options = self
            .replace_state
            .options
            .unwrap_or_else(|| self.find_in_files.search_options());
        let engine = luban_core::search::SearchEngine::new(query, options);
        if engine.error().is_some() {
            return;
        }
        let mut undo = ReplaceUndo::default();
        let mut modified = 0usize;
        let mut skipped = preview.skipped.clone();
        for file in &preview.files {
            let bytes = match std::fs::read(&file.path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    skipped.push(SkippedFile {
                        path: file.path.clone(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            if bytes.len() as u64 != file.size
                || workspace_replace::fingerprint(&bytes) != file.fingerprint
            {
                skipped.push(SkippedFile {
                    path: file.path.clone(),
                    reason: tr("replace.skip_changed"),
                });
                continue;
            }
            let decoded = encoding::decode(&bytes);
            let (replaced, count) = match engine.replace_all(&decoded.text, &replacement, None) {
                Ok(result) => result,
                Err(_) => continue,
            };
            if count == 0 {
                continue;
            }
            let new_text = eol::to_lf_owned(replaced);
            if let Some(index) = self.tabs.iter().position(|tab| {
                tab.doc
                    .path
                    .as_ref()
                    .is_some_and(|path| util::same_path(path, &file.path))
            }) {
                let tab = &self.tabs[index];
                if tab.doc.is_huge()
                    || tab.large_file
                    || tab.doc.partial_range().is_some()
                    || tab.doc.is_non_text()
                {
                    skipped.push(SkippedFile {
                        path: file.path.clone(),
                        reason: tr("replace.skip_open_mode"),
                    });
                    continue;
                }
                undo.entries.push(UndoEntry {
                    tab_id: Some(tab.id),
                    path: tab.doc.path.clone(),
                    text: tab.doc.text.clone(),
                    modified: tab.doc.is_modified(),
                    cursor: tab.doc.cursor,
                    anchor: tab.doc.anchor,
                    was_open: true,
                });
                self.apply_replacement_to_tab(index, new_text, window, cx);
                modified += 1;
            } else {
                let mut doc = match Document::open_with_limit(&file.path, u64::MAX) {
                    Ok(doc) => doc,
                    Err(error) => {
                        skipped.push(SkippedFile {
                            path: file.path.clone(),
                            reason: error.to_string(),
                        });
                        continue;
                    }
                };
                if doc.is_non_text() {
                    skipped.push(SkippedFile {
                        path: file.path.clone(),
                        reason: tr("replace.skip_non_text"),
                    });
                    continue;
                }
                doc.attach_text(new_text);
                doc.touch();
                let path = file.path.clone();
                self.add_document(doc, window, cx);
                undo.entries.push(UndoEntry {
                    tab_id: self.tabs.last().map(|tab| tab.id),
                    path: Some(path),
                    text: String::new(),
                    modified: false,
                    cursor: 0,
                    anchor: 0,
                    was_open: false,
                });
                modified += 1;
            }
        }
        self.replace_state.undo = Some(undo);
        self.replace_state.preview = None;
        self.replace_state.status = tr_args(
            "replace.applied",
            &[
                ("count", modified.to_string()),
                ("skipped", skipped.len().to_string()),
            ],
        );
        self.replace_state.error = None;
        self.set_status(self.replace_state.status.clone(), cx);
        cx.notify();
    }
    pub fn undo_workspace_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(undo) = self.replace_state.undo.take() else {
            self.set_status(tr("replace.nothing_to_undo"), cx);
            return;
        };
        for entry in undo.entries.into_iter().rev() {
            if !entry.was_open {
                if let Some(path) = &entry.path {
                    if let Some(index) = self.tabs.iter().position(|tab| {
                        tab.doc
                            .path
                            .as_ref()
                            .is_some_and(|open| util::same_path(open, path))
                    }) {
                        self.close_tab_by_id_without_prompt(self.tabs[index].id, cx);
                    }
                }
                continue;
            }
            let index = entry
                .tab_id
                .and_then(|id| self.tab_index_by_id(id))
                .or_else(|| {
                    entry.path.as_ref().and_then(|path| {
                        self.tabs.iter().position(|tab| {
                            tab.doc
                                .path
                                .as_ref()
                                .is_some_and(|open| util::same_path(open, path))
                        })
                    })
                });
            let index = match index {
                Some(index) => index,
                None => {
                    let mut doc = Document::with_text(entry.text.clone());
                    doc.path = entry.path.clone();
                    doc.touch();
                    self.add_document(doc, window, cx);
                    self.tabs.len() - 1
                }
            };
            let total = self.tabs[index].doc.char_count();
            let old_modified = entry.modified;
            {
                let tab = &mut self.tabs[index];
                tab.doc.apply_edit(
                    0,
                    total,
                    &entry.text,
                    EditKind::Other,
                    entry.cursor.min(textutil::char_count(&entry.text)),
                    entry.anchor.min(textutil::char_count(&entry.text)),
                );
                if !old_modified {
                    tab.doc.mark_saved();
                }
                tab.highlighted_revision = u64::MAX;
                tab.decorations_key = None;
                tab.bracket_key = None;
                tab.needs_sync = false;
                tab.pending_edit = false;
            }
            let text = entry.text.clone();
            let total_chars = textutil::char_count(&text);
            let cursor = entry.cursor.min(total_chars);
            let anchor = entry.anchor.min(total_chars);
            let b0 = textutil::char_to_byte(&text, anchor.min(cursor));
            let b1 = textutil::char_to_byte(&text, anchor.max(cursor));
            let editor = self.tabs[index].editor.clone();
            self.tabs[index].doc.set_selection(anchor, cursor);
            editor.update(cx, |state, cx| {
                state.replace_all(text, window, cx);
                state.set_selected_range(b0..b1, cx);
            });
        }
        self.replace_state.status = tr("replace.undone");
        self.set_status(tr("replace.undone"), cx);
        cx.notify();
    }
    pub fn open_find_in_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find_in_files.open = true;
        self.find_in_files.respect_gitignore = self.settings.respect_gitignore;
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
        self.settings.record_search(&query);
        self.commit_settings();
        if self.find_in_files.open_documents {
            self.search_open_documents(cx);
            return;
        }
        let filter = match self.find_in_files.filter(cx) {
            Ok(filter) => filter,
            Err(error) => {
                self.find_in_files.error = Some(error.clone());
                self.find_in_files.status = error;
                cx.notify();
                return;
            }
        };
        self.find_in_files.error = None;
        self.find_in_files.generation += 1;
        let generation = self.find_in_files.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.find_in_files.cancel = Some(cancel.clone());
        self.find_in_files.running = true;
        self.find_in_files.clear_results();
        self.find_in_files.status = tr("ff.searching");
        cx.notify();

        if self.find_in_files.workspace {
            if self.workspace_roots.is_empty() {
                self.find_in_files.running = false;
                self.find_in_files.cancel = None;
                self.find_in_files.status = tr("ff.no_workspace");
                cx.notify();
                return;
            }
            let options = WorkspaceSearchOptions {
                roots: self.workspace_roots.clone(),
                query,
                options: self.find_in_files.search_options(),
                filter,
                respect_gitignore: self.find_in_files.respect_gitignore,
                include_hidden: self.settings.show_hidden_files,
                max_results: 20_000,
                max_file_size: self.settings.max_file_size_bytes(),
            };
            let cancel_for_task = cancel.clone();
            let task = cx.background_executor().spawn(async move {
                let mut progress = |_files: usize| {};
                search_workspace(&options, &cancel_for_task, &mut progress)
            });
            cx.spawn(async move |this, cx| {
                let outcome = task.await;
                this.update(cx, |this, cx| {
                    this.finish_find_in_files(outcome, generation, cx);
                })
                .ok();
            })
            .detach();
            return;
        }
        let folder = self.find_in_files.folder_path(cx);
        if !folder.is_dir() {
            self.find_in_files.running = false;
            self.find_in_files.cancel = None;
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
    fn search_open_documents(&mut self, cx: &mut Context<Self>) {
        let query = self.find_in_files.query_text(cx);
        let engine =
            luban_core::search::SearchEngine::new(query, self.find_in_files.search_options());
        if let Some(error) = engine.error() {
            self.find_in_files.clear_results();
            self.find_in_files.status = error.to_string();
            cx.notify();
            return;
        }
        let mut matches = Vec::new();
        let mut scanned = 0usize;
        let mut files_with_matches = HashSet::new();
        let mut truncated = false;
        for tab in &self.tabs {
            if tab.doc.is_huge() || tab.doc.partial_range().is_some() || tab.doc.is_non_text() {
                continue;
            }
            let Some(path) = tab.doc.path.clone() else {
                continue;
            };
            scanned += 1;
            let before = matches.len();
            let text = if tab.large_file {
                tab.editor.read(cx).text().to_string()
            } else {
                tab.doc.text.clone()
            };
            let index = textutil::LineIndex::new(&text);
            for matched in engine.find_all(&text, None, 2000 - matches.len()) {
                let (line, column) = index.line_col(matched.start);
                matches.push(FileMatch {
                    path: path.clone(),
                    line,
                    column,
                    line_text: index.line_text(&text, line - 1),
                    match_text: textutil::slice_chars(&text, matched.start, matched.end),
                    match_len: matched.len(),
                });
                if matches.len() >= 2000 {
                    truncated = true;
                    break;
                }
            }
            if matches.len() > before {
                files_with_matches.insert(path);
            }
            if truncated {
                break;
            }
        }
        self.find_in_files.generation += 1;
        let generation = self.find_in_files.generation;
        self.find_in_files.running = false;
        self.find_in_files.cancel = None;
        self.finish_find_in_files(
            Ok(FindInFilesOutcome {
                matches,
                files_scanned: scanned,
                files_with_matches: files_with_matches.len(),
                truncated,
                cancelled: false,
            }),
            generation,
            cx,
        );
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
                self.find_in_files.truncated = outcome.truncated;
                self.find_in_files.cancelled = outcome.cancelled;
                let roots = self.workspace_roots.clone();
                self.find_in_files.set_results(outcome.matches, &roots);
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
    pub(super) fn refresh_find(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn render_find_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.weak_entity();
        let query_input = self.find.query.clone();
        let replacement_input = self.find.replacement.clone();
        let error = self.find.error.clone();
        let status = self.find.status.clone();
        let show_replace = self.find.show_replace;
        let weak_prev = weak.clone();
        let weak_next = weak.clone();
        let weak_all_find = weak.clone();
        let weak_mark_all = weak.clone();
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
                        Button::new("find-mark-all")
                            .label(tr("find.mark_all"))
                            .ghost()
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_mark_all.update(cx, |this, cx| {
                                    this.mark_find_all(window, cx);
                                });
                            }),
                    )
                    .child({
                        let weak_history = weak.clone();
                        let query = query_input.clone();
                        let history = self.settings.search_history.clone();
                        Button::new("find-history")
                            .label(tr("find.history"))
                            .ghost()
                            .compact()
                            .dropdown_menu(move |mut menu, window, _cx| {
                                if history.is_empty() {
                                    menu = menu.item(
                                        PopupMenuItem::new(tr("find.no_history")).disabled(true),
                                    );
                                }
                                for entry in &history {
                                    let value = entry.clone();
                                    let query = query.clone();
                                    let weak = weak_history.clone();
                                    menu = menu.item(PopupMenuItem::new(entry.clone()).on_click(
                                        move |_, window, cx| {
                                            let value = value.clone();
                                            let _ = weak.update(cx, |this, cx| {
                                                query.update(cx, |input, cx| {
                                                    input.set_value(value, window, cx)
                                                });
                                                this.find.dirty = true;
                                                cx.notify();
                                            });
                                        },
                                    ));
                                }
                                let weak_clear = weak_history.clone();
                                menu = menu.separator().item(
                                    PopupMenuItem::new(tr("find.clear_history")).on_click(
                                        move |_, _, cx| {
                                            let _ = weak_clear.update(cx, |this, cx| {
                                                this.clear_search_history(cx);
                                            });
                                        },
                                    ),
                                );
                                Self::fit_menu(menu, window)
                            })
                    })
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
                        Checkbox::new("find-enhanced-regex")
                            .label(tr("find.enhanced_regex"))
                            .checked(self.find.enhanced_regex)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.find.enhanced_regex = *checked;
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
                    .child({
                        let weak_history = weak.clone();
                        let replacement = replacement_input.clone();
                        let history = self.settings.replace_history.clone();
                        Button::new("replace-history")
                            .label(tr("find.history"))
                            .ghost()
                            .compact()
                            .dropdown_menu(move |mut menu, window, _cx| {
                                if history.is_empty() {
                                    menu = menu.item(
                                        PopupMenuItem::new(tr("find.no_history")).disabled(true),
                                    );
                                }
                                for entry in &history {
                                    let value = entry.clone();
                                    let replacement = replacement.clone();
                                    let weak = weak_history.clone();
                                    menu = menu.item(PopupMenuItem::new(entry.clone()).on_click(
                                        move |_, window, cx| {
                                            let value = value.clone();
                                            let _ = weak.update(cx, |this, cx| {
                                                replacement.update(cx, |input, cx| {
                                                    input.set_value(value, window, cx)
                                                });
                                                this.find.dirty = true;
                                                cx.notify();
                                            });
                                        },
                                    ));
                                }
                                let weak_clear = weak_history.clone();
                                menu = menu.separator().item(
                                    PopupMenuItem::new(tr("find.clear_history")).on_click(
                                        move |_, _, cx| {
                                            let _ = weak_clear.update(cx, |this, cx| {
                                                this.clear_search_history(cx);
                                            });
                                        },
                                    ),
                                );
                                Self::fit_menu(menu, window)
                            })
                    })
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
    pub(super) fn render_replace_preview(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = &self.replace_preview else {
            return div().into_any_element();
        };
        let count = state.preview.count;
        let mut samples = Vec::new();
        for (before, after) in &state.preview.samples {
            samples.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors.border)
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(textutil::preview(before, 160)),
                    )
                    .child(div().text_sm().child(textutil::preview(after, 160)))
                    .into_any_element(),
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
                    .child(tr("find.preview_title")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr_args(
                        "find.preview_count",
                        &[("count", count.to_string())],
                    )),
            )
            .child(
                div()
                    .id("replace-preview-list")
                    .flex()
                    .flex_col()
                    .max_h(px(260.))
                    .overflow_y_scroll()
                    .children(samples),
            )
            .child(if count == 0 {
                div()
                    .flex()
                    .justify_end()
                    .child(
                        Button::new("replace-preview-cancel")
                            .label(tr("common.close"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.replace_preview = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("replace-preview-cancel")
                            .label(tr("common.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.replace_preview = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("replace-preview-confirm")
                            .label(tr("find.replace_all"))
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_replace_preview(window, cx);
                            })),
                    )
                    .into_any_element()
            })
            .into_any_element();
        self.overlay_card(520.0, None, cx, content)
    }
    pub(super) fn render_find_in_files(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.weak_entity();
        let query = self.find_in_files.query.clone();
        let folder = self.find_in_files.folder.clone();
        let masks = self.find_in_files.masks.clone();
        let include = self.find_in_files.include.clone();
        let exclude = self.find_in_files.exclude.clone();
        let replacement = self.find_in_files.replacement.clone();
        let show_replace = self.find_in_files.show_replace;
        let workspace = self.find_in_files.workspace;
        let has_workspace = !self.workspace_roots.is_empty();
        let respect_gitignore = self.find_in_files.respect_gitignore;
        let status = self.find_in_files.status.clone();
        let error = self.find_in_files.error.clone();
        let running = self.find_in_files.running;
        let total_groups = self.find_in_files.groups.len();
        let groups: Vec<crate::find::FileGroup> = self
            .find_in_files
            .groups
            .iter()
            .take(200)
            .cloned()
            .collect();
        let collapsed = self.find_in_files.collapsed.clone();
        let count = self.find_in_files.results.len();
        let weak_search = weak.clone();
        let weak_cancel = weak.clone();
        let weak_close = weak.clone();
        let weak_settings_filter = weak.clone();
        let weak_toggle_group = weak.clone();
        let weak_result = weak.clone();
        let weak_preview = weak.clone();
        let weak_replace = weak.clone();
        let weak_undo = weak.clone();
        let has_undo = self.replace_state.undo.is_some();
        let replacing = self.replace_state.running;
        self.invalidate_stale_preview(cx);
        let replace_preview = self.replace_state.preview.clone();
        let mut result_rows = Vec::new();
        for (group_index, group) in groups.iter().enumerate() {
            let is_collapsed = collapsed.contains(&group.path);
            let weak_group = weak_toggle_group.clone();
            let group_path = group.path.clone();
            let group_relative = group.relative.clone();
            let group_count = group.matches.len();
            result_rows.push(
                div()
                    .id(format!("ff-group:{group_index}"))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors.border)
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().colors.secondary))
                    .child(if is_collapsed { "▸" } else { "▾" })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(group_relative),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(group_count.to_string()),
                    )
                    .on_click(move |_, _, cx| {
                        let path = group_path.clone();
                        let _ = weak_group.update(cx, |this, cx| {
                            this.find_in_files.toggle_group(&path);
                            cx.notify();
                        });
                    })
                    .into_any_element(),
            );
            if is_collapsed {
                continue;
            }
            for (match_index, matched) in group.matches.iter().take(200).enumerate() {
                let matched = matched.clone();
                let weak_row = weak_result.clone();
                result_rows.push(
                    div()
                        .id(format!("ff-result:{group_index}:{match_index}"))
                        .flex()
                        .flex_col()
                        .gap_0()
                        .pl(px(28.))
                        .pr_3()
                        .py_1()
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().colors.secondary))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_xs()
                                .text_color(cx.theme().colors.muted_foreground)
                                .child(format!("{}:{}", matched.line, matched.column)),
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
            if group.matches.len() > 200 {
                result_rows.push(
                    div()
                        .pl(px(28.))
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(tr_args(
                            "ff.more_matches",
                            &[("count", (group.matches.len() - 200).to_string())],
                        ))
                        .into_any_element(),
                );
            }
        }
        if total_groups > groups.len() {
            result_rows.push(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr_args(
                        "ff.more_files",
                        &[("count", (total_groups - groups.len()).to_string())],
                    ))
                    .into_any_element(),
            );
        }
        let mut options = div()
            .flex()
            .items_center()
            .gap_3()
            .flex_wrap()
            .child(
                Checkbox::new("ff-workspace")
                    .label(tr("ff.workspace"))
                    .checked(workspace && has_workspace)
                    .disabled(!has_workspace)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.find_in_files.workspace = *checked;
                        if *checked {
                            this.find_in_files.open_documents = false;
                        }
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("ff-gitignore")
                    .label(tr("ff.respect_gitignore"))
                    .checked(respect_gitignore)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.find_in_files.respect_gitignore = *checked;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("ff-use-settings")
                    .label(tr("ff.use_workspace_filter"))
                    .compact()
                    .on_click(move |_, window, cx| {
                        let _ = weak_settings_filter.update(cx, |this, cx| {
                            let include = this.settings.search_include.clone();
                            let exclude = this.settings.search_exclude.clone();
                            this.find_in_files
                                .use_settings_filter(&include, &exclude, window, cx);
                        });
                    }),
            );
        for (id, label_key, checked, field) in [
            (
                "ff-case",
                "ff.match_case",
                self.find_in_files.match_case,
                0u8,
            ),
            ("ff-word", "ff.whole_word", self.find_in_files.whole_word, 1),
            ("ff-regex", "ff.regex", self.find_in_files.regex, 2),
            (
                "ff-enhanced-regex",
                "ff.enhanced_regex",
                self.find_in_files.enhanced_regex,
                3,
            ),
            (
                "ff-open-docs",
                "ff.open_documents",
                self.find_in_files.open_documents,
                4,
            ),
        ] {
            options = options.child(
                Checkbox::new(id)
                    .label(tr(label_key))
                    .checked(checked)
                    .on_click(cx.listener(move |this, checked, _, cx| {
                        match field {
                            0 => this.find_in_files.match_case = *checked,
                            1 => this.find_in_files.whole_word = *checked,
                            2 => this.find_in_files.regex = *checked,
                            3 => this.find_in_files.enhanced_regex = *checked,
                            _ => {
                                this.find_in_files.open_documents = *checked;
                                if *checked {
                                    this.find_in_files.workspace = false;
                                }
                            }
                        }
                        cx.notify();
                    })),
            );
        }
        options = options
            .child(
                Button::new("ff-search")
                    .label(tr(if running { "ff.searching" } else { "ff.search" }))
                    .primary()
                    .compact()
                    .on_click(move |_, _, cx| {
                        let _ = weak_search.update(cx, |this, cx| this.start_find_in_files(cx));
                    }),
            )
            .child(
                Button::new("ff-cancel")
                    .label(tr("common.cancel"))
                    .compact()
                    .on_click(move |_, _, cx| {
                        let _ = weak_cancel.update(cx, |this, cx| this.cancel_find_in_files(cx));
                    }),
            );
        let mut replace_row = div().flex().items_center().gap_2();
        replace_row = replace_row
            .child(Input::new(&replacement).h(px(30.)))
            .child(
                Button::new("ff-toggle-replace")
                    .label(tr(if show_replace {
                        "ff.hide_replace"
                    } else {
                        "ff.show_replace"
                    }))
                    .compact()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.find_in_files.show_replace = !this.find_in_files.show_replace;
                        cx.notify();
                    })),
            );
        if show_replace {
            replace_row = replace_row
                .child(
                    Button::new("ff-preview")
                        .label(tr("ff.preview_replace"))
                        .compact()
                        .on_click(move |_, window, cx| {
                            let _ = weak_preview.update(cx, |this, cx| {
                                this.start_workspace_replace_preview(window, cx);
                            });
                        }),
                )
                .child(
                    Button::new("ff-replace-all")
                        .label(tr("ff.replace_all"))
                        .compact()
                        .on_click(move |_, window, cx| {
                            let _ = weak_replace.update(cx, |this, cx| {
                                this.confirm_workspace_replace(window, cx);
                            });
                        }),
                )
                .child(
                    Button::new("ff-undo-replace")
                        .label(tr("ff.undo_replace"))
                        .compact()
                        .disabled(!has_undo || replacing)
                        .on_click(move |_, window, cx| {
                            let _ = weak_undo.update(cx, |this, cx| {
                                this.undo_workspace_replace(window, cx);
                            });
                        }),
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
                    .child(div().flex_1().child(Input::new(&include).h(px(30.))))
                    .child(div().flex_1().child(Input::new(&exclude).h(px(30.)))),
            )
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
            .child(options)
            .child(replace_row)
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
            .when_some(replace_preview, |this, preview| {
                let mut rows = Vec::new();
                for file in preview.files.iter().take(40) {
                    rows.push(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0()
                            .text_xs()
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(format!("{}  ({} →)", file.relative, file.matches)),
                            )
                            .children(file.samples.iter().map(|(before, after)| {
                                div()
                                    .pl(px(12.))
                                    .text_color(cx.theme().colors.muted_foreground)
                                    .child(format!("{before} → {after}"))
                            }))
                            .into_any_element(),
                    );
                }
                for skipped in preview.skipped.iter().take(20) {
                    rows.push(
                        div()
                            .text_xs()
                            .text_color(cx.theme().colors.warning)
                            .child(format!("{}: {}", skipped.path.display(), skipped.reason))
                            .into_any_element(),
                    );
                }
                this.child(
                    div()
                        .id("replace-preview")
                        .flex()
                        .flex_col()
                        .gap_1()
                        .max_h(px(180.))
                        .overflow_y_scroll()
                        .border_1()
                        .border_color(cx.theme().colors.border)
                        .rounded_sm()
                        .p_2()
                        .child(tr_args(
                            "replace.preview_summary",
                            &[
                                ("files", preview.file_count().to_string()),
                                ("matches", preview.total_matches.to_string()),
                                ("skipped", preview.skipped.len().to_string()),
                            ],
                        ))
                        .children(rows),
                )
            })
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
        self.overlay_card(860.0, Some(640.0), cx, content)
    }
}

use super::*;

/// A session tab prepared for loading on the background executor.
pub(super) struct RestoreSpec {
    pub(super) path: Option<PathBuf>,
    pub(super) text: Option<String>,
    pub(super) encoding: TextEncoding,
    pub(super) eol: Eol,
    pub(super) language: String,
    pub(super) cursor: usize,
    pub(super) anchor: usize,
    pub(super) modified: bool,
    pub(super) fallback_name: String,
    pub(super) range: Option<[u64; 2]>,
    pub(super) huge: bool,
    pub(super) huge_line: Option<u64>,
    pub(super) huge_column: Option<u64>,
    pub(super) bookmarks: Vec<usize>,
    /// Editor group the tab belongs to.
    pub(super) group: usize,
}
impl RestoreSpec {
    pub(super) fn load(self, max_size: u64) -> Document {
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
                        // Ordinary buffers are stored LF-normalized; non-text
                        // buffers are stored raw and must keep their bytes.
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

    pub(super) fn open_restored(
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

    pub(super) fn untitled(
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
pub(super) fn session_tab_from(tab: &Tab) -> SessionTab {
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
        // Large buffers are never written to the session: the file is restored
        // from disk instead.
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
        recovery_id: tab.doc.huge().and_then(|state| {
            state
                .engine
                .try_read()
                .ok()
                .and_then(|engine| engine.recovery_id().map(str::to_string))
        }),
        bookmarks: if huge {
            Vec::new()
        } else {
            tab.bookmarks.as_vec()
        },
    }
}

impl LubanApp {
    /// Restore one saved window: workspace, roots, split layout and tab groups.
    pub(super) fn restore_window_session(
        &mut self,
        session: WindowSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.window_width = session.width;
        self.window_height = session.height;
        window.resize(size(px(self.window_width), px(self.window_height)));
        self.set_workspace_environment(
            session.workspace_file.clone(),
            session.workspace_roots.clone(),
            window,
            cx,
        );
        self.rebuild_index(cx);
        self.split = match session.split {
            SplitLayout::Right => SplitDirection::Right,
            SplitLayout::Down => SplitDirection::Down,
            SplitLayout::None => SplitDirection::None,
        };
        self.active_group = session
            .active_group
            .min(session.groups.len().saturating_sub(1));
        for (index, group) in session.groups.iter().take(2).enumerate() {
            self.restore_group_active[index] = group.active_tab;
        }
        let specs = self.session_specs(session);
        if specs.is_empty() {
            self.new_file(window, cx);
            return;
        }
        self.begin_restore(specs, window, cx);
    }

    /// Restore tabs moved into a new window. Every tab starts in group 0.
    pub(super) fn restore_tabs(
        &mut self,
        tabs: Vec<SessionTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group_count = self.active_group + 1;
        let groups = (0..group_count)
            .map(|index| GroupSession {
                active_tab: if index == self.active_group {
                    tabs.len().saturating_sub(1)
                } else {
                    0
                },
                tabs: if index == self.active_group {
                    tabs.clone()
                } else {
                    Vec::new()
                },
            })
            .collect();
        let session = WindowSession {
            id: self.window_id,
            width: self.window_width,
            height: self.window_height,
            split: match self.split {
                SplitDirection::Right => SplitLayout::Right,
                SplitDirection::Down => SplitLayout::Down,
                SplitDirection::None => SplitLayout::None,
            },
            active_group: self.active_group,
            groups,
            workspace_file: self.workspace_file.clone(),
            workspace_roots: self.workspace_roots.clone(),
            ..WindowSession::default()
        };
        let specs = self.session_specs(session);
        if specs.is_empty() {
            self.new_file(window, cx);
            return;
        }
        self.begin_restore(specs, window, cx);
    }

    /// Convert session groups into flat restore specs with group membership.
    pub(super) fn session_specs(&mut self, session: WindowSession) -> Vec<RestoreSpec> {
        let pending_recovery_ids: HashSet<&str> = if self.auto_recovery {
            HashSet::new()
        } else {
            self.recovery_queue
                .iter()
                .chain(self.recovery_prompt.iter().map(|prompt| &prompt.info))
                .map(|info| info.id.as_str())
                .collect()
        };
        let mut specs = Vec::new();
        for (group, group_session) in session.groups.iter().enumerate() {
            for tab in &group_session.tabs {
                if tab
                    .recovery_id
                    .as_deref()
                    .is_some_and(|id| pending_recovery_ids.contains(id))
                {
                    continue;
                }
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
                    path: tab.path.clone(),
                    text: tab.text.clone(),
                    encoding: tab.encoding,
                    eol: tab.eol,
                    language: tab.language.clone(),
                    cursor: tab.cursor,
                    anchor: tab.anchor,
                    modified: tab.modified,
                    fallback_name,
                    range: tab.range,
                    huge: tab.huge,
                    huge_line: tab.huge_line,
                    huge_column: tab.huge_column,
                    bookmarks: tab.bookmarks.clone(),
                    group,
                });
            }
        }
        specs
    }

    pub(super) fn begin_restore(
        &mut self,
        specs: Vec<RestoreSpec>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let max_size = self.settings.max_file_size_bytes();
        let any_large = specs.iter().any(|spec| {
            spec.huge
                || spec.path.as_ref().is_some_and(|path| {
                    fs::metadata(path)
                        .map(|metadata| metadata.len() >= self.settings.large_file_threshold as u64)
                        .unwrap_or(false)
                })
        });
        if !any_large {
            for spec in specs {
                let group = spec.group;
                let bookmarks = spec.bookmarks.clone();
                let doc = spec.load(max_size);
                self.add_document(doc, window, cx);
                let group_limit = self.group_count().saturating_sub(1);
                if let Some(tab) = self.tabs.last_mut() {
                    tab.group = group.min(group_limit);
                }
                self.restore_tab_bookmarks(bookmarks, cx);
            }
            self.finish_restore_groups(cx);
            return;
        }
        self.set_status(tr("status.restoring_session"), cx);
        let task = cx.background_executor().spawn(async move {
            specs
                .into_iter()
                .map(|spec| {
                    let group = spec.group;
                    let bookmarks = spec.bookmarks.clone();
                    (group, bookmarks, spec.load(max_size))
                })
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let docs = task.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.finish_restore_groups_with(docs, window, cx);
            });
        })
        .detach();
    }

    pub(super) fn finish_restore_groups_with(
        &mut self,
        docs: Vec<(usize, Vec<usize>, Document)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if docs.is_empty() {
            self.new_file(window, cx);
            return;
        }
        for (group, bookmarks, doc) in docs {
            self.add_document(doc, window, cx);
            let group_limit = self.group_count().saturating_sub(1);
            if let Some(tab) = self.tabs.last_mut() {
                tab.group = group.min(group_limit);
            }
            self.restore_tab_bookmarks(bookmarks, cx);
        }
        self.finish_restore_groups(cx);
    }

    /// Select the restored group's active tab and refresh derived state.
    pub(super) fn finish_restore_groups(&mut self, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            self.active = 0;
            return;
        }
        self.active_group = self.active_group.min(self.group_count().saturating_sub(1));
        self.active_in_group = [None, None];
        let memberships: Vec<usize> = self.tabs.iter().map(|tab| tab.group).collect();
        for group in 0..self.group_count() {
            let indices = tab_groups::group_tabs(&memberships, group);
            if indices.is_empty() {
                continue;
            }
            let position = self.restore_group_active[group].min(indices.len() - 1);
            let index = indices[position];
            self.active_in_group[group] = Some(self.tabs[index].id);
        }
        if let Some(index) =
            self.active_in_group[self.active_group].and_then(|id| self.tab_index_by_id(id))
        {
            self.active = index;
        } else if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.group == self.active_group)
        {
            self.active = index;
        }
        self.focus_editor = true;
        self.nav.clear();
        self.refresh_outline(cx);
        cx.notify();
    }

    pub(super) fn persist_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let size = window.bounds().size;
        self.window_width = size.width.into();
        self.window_height = size.height.into();
        self.window_maximized = window.is_maximized();
        self.save_session_now(cx);
        self.commit_settings(cx);
    }

    pub(super) fn save_session_now(&mut self, cx: &mut Context<Self>) {
        if !self.session_writer {
            return;
        }
        let current = self.build_window_session();
        let windows = window_registry::collect(cx, current);
        let session = Session {
            version: luban_core::session::SESSION_VERSION,
            windows,
        };
        match session.save() {
            Ok(()) => {}
            Err(err) => {
                log::warn!("could not save session: {err}");
                self.set_status(
                    tr_args("status.session_save_failed", &[("error", err.to_string())]),
                    cx,
                );
            }
        }
    }

    pub(super) fn autosave_session(&mut self, cx: &mut Context<Self>) {
        if !self.settings.autosave_session {
            return;
        }
        let elapsed = self.last_session_save.elapsed();
        if elapsed < Duration::from_secs(self.settings.session_interval_secs.max(5)) {
            return;
        }
        self.last_session_save = Instant::now();
        self.save_session_now(cx);
    }

    // ------------------------------------------------------------------
    // Pending async work
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Editing commands
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Session restore helpers
    // ------------------------------------------------------------------

    fn restore_tab_bookmarks(&mut self, bookmarks: Vec<usize>, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.len().checked_sub(1) else {
            return;
        };
        {
            let Some(tab) = self.tabs.get_mut(index) else {
                return;
            };
            if tab.doc.is_huge() || tab.doc.is_non_text() {
                return;
            }
            let text = if tab.large_file {
                tab.editor.read(cx).text().to_string()
            } else {
                tab.doc.text.clone()
            };
            let count = textutil::LineIndex::new(&text).len();
            tab.bookmarks = BookmarkSet::from_lines(bookmarks);
            tab.bookmarks.clamp(count);
            tab.bookmark_revision = tab.bookmark_revision.wrapping_add(1);
        }
        self.refresh_bookmark_decorations(index, cx);
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Session collection
    // ------------------------------------------------------------------

    pub(crate) fn build_window_session(&self) -> WindowSession {
        let memberships: Vec<usize> = self.tabs.iter().map(|tab| tab.group).collect();
        let mut groups = Vec::new();
        for group in 0..self.group_count() {
            let indices = tab_groups::group_tabs(&memberships, group);
            let active_id = self.active_in_group[group].or_else(|| {
                (group == self.active_group)
                    .then(|| self.tabs.get(self.active).map(|tab| tab.id))
                    .flatten()
            });
            let active_tab = active_id
                .and_then(|id| self.tab_index_by_id(id))
                .and_then(|index| indices.iter().position(|candidate| *candidate == index))
                .unwrap_or(0);
            groups.push(GroupSession {
                active_tab,
                tabs: indices
                    .iter()
                    .filter_map(|index| self.tabs.get(*index))
                    .map(session_tab_from)
                    .collect(),
            });
        }
        WindowSession {
            id: self.window_id,
            x: None,
            y: None,
            width: self.window_width,
            height: self.window_height,
            maximized: self.window_maximized,
            workspace_file: self.workspace_file.clone(),
            workspace_roots: self.workspace_roots.clone(),
            split: match self.split {
                SplitDirection::Right => SplitLayout::Right,
                SplitDirection::Down => SplitLayout::Down,
                SplitDirection::None => SplitLayout::None,
            },
            active_group: self.active_group,
            groups,
        }
    }
}

use super::*;

impl LubanApp {
    // ------------------------------------------------------------------
    // Editor area
    // ------------------------------------------------------------------

    pub(super) fn render_panes(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.split.is_split() {
            return self.render_group(0, true, window, cx);
        }
        let first_active = self.active_group == 0;
        let first = self.render_group(0, first_active, window, cx);
        let second = self.render_group(1, !first_active, window, cx);
        let divider = div()
            .bg(cx.theme().colors.border)
            .when(self.split == SplitDirection::Right, |this| {
                this.w(px(1.)).h_full()
            })
            .when(self.split == SplitDirection::Down, |this| {
                this.h(px(1.)).w_full()
            });
        let layout = div().flex_1().min_w(px(0.)).min_h(px(0.)).flex();
        match self.split {
            SplitDirection::Right => layout.flex_row().child(first).child(divider).child(second),
            SplitDirection::Down => layout.flex_col().child(first).child(divider).child(second),
            SplitDirection::None => layout.child(first),
        }
        .into_any_element()
    }

    fn render_group(
        &mut self,
        group: usize,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tabs = self.render_tabs(group, window, cx);
        let editor = match self.group_active_index(group) {
            Some(index) => self.render_editor_area(index, window, cx),
            None => div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().colors.muted_foreground)
                .child(tr("split.empty_group"))
                .into_any_element(),
        };
        let weak_group = cx.weak_entity();
        div()
            .id(format!("editor-group:{group}"))
            .role(Role::Group)
            .aria_label(tr_args(
                "split.group_label",
                &[("group", (group + 1).to_string())],
            ))
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .when(active && self.split.is_split(), |this| {
                this.border_t_1().border_color(cx.theme().colors.border)
            })
            .child(tabs)
            .child(editor)
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                let _ = weak_group.update(cx, |this, cx| {
                    if this.active_group != group {
                        this.switch_group(Some(group), window, cx);
                    }
                });
            })
            .into_any_element()
    }

    fn render_editor_area(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.weak_entity();
        let Some(tab) = self.tabs.get(index) else {
            return div().size_full().into_any_element();
        };
        let editor = tab.editor.clone();
        let font_size = self.settings.font_size;
        let font_family = self.settings.font_family.clone();
        let show_line_numbers = self.settings.show_line_numbers;
        let editor_background = cx
            .theme()
            .highlight_theme
            .style
            .editor_background
            .unwrap_or(cx.theme().colors.background);
        let _ = weak;

        // Disk-engine tabs mount only a bounded window, so the editor's own
        // scrollbar spans that window. Overlay a file-global scrollbar that
        // targets global lines and takes precedence over the window-local one.
        //
        // The editor's built-in scrollbar is laid out on the padded container,
        // while `input_bounds` is the text area inside that padding. Derive the
        // padding from the size tier passed to the editor so the overlay keeps
        // sharing the native track position.
        const EDITOR_SIZE: Size = Size::Medium;
        let padding_x = EDITOR_SIZE.input_px();
        let padding_y = EDITOR_SIZE.input_py();
        let global_scrollbar = tab.huge.as_ref().and_then(|ui| {
            let state = tab.editor.read(cx);
            let total_lines = tab.doc.line_count() as u64;
            let line_height = state
                .line_height()
                .map(|height| height.as_f32())
                .unwrap_or(0.0);
            let mut viewport = state.input_bounds();
            // `visible_row_range` is clipped to the mounted window, which can
            // be a single long line. The viewport's row capacity is the right
            // denominator for the whole-file mapping and for deciding whether
            // a line scrollbar is meaningful.
            let viewport_lines = if line_height > 0.0 {
                (viewport.size.height.as_f32() / line_height)
                    .floor()
                    .max(1.0) as u64
            } else {
                0
            };
            let view_bytes = tab
                .doc
                .partial_range()
                .map(|range| range.end.saturating_sub(range.start))
                .unwrap_or(tab.doc.file_size);
            let average_line_bytes = view_bytes / total_lines.max(1);
            viewport.origin.y -= padding_y;
            viewport.size.height += padding_y * 2.;
            viewport.size.width += padding_x;
            if !huge_scrollbar::line_scrollbar_is_meaningful(
                total_lines,
                viewport_lines,
                average_line_bytes,
            ) || line_height <= 0.0
                || viewport.size.width <= px(0.0)
                || viewport.size.height <= px(0.0)
            {
                return None;
            }
            Some(huge_scrollbar::render(
                HugeScrollbarProps {
                    top_line: ui.scroll_target.unwrap_or(ui.scroll_top_line),
                    total_lines,
                    viewport_lines: viewport_lines.max(1),
                    line_height,
                    viewport,
                    shared: ui.scroll.clone(),
                },
                cx.theme().colors.background,
            ))
        });

        // Disk-engine tabs hide the editor's window-local gutter and render a
        // document-global one next to the editor. It is positioned from the
        // same scroll offset as the file scrollbar, so both follow wheel
        // scrolling and window swaps.
        let global_gutter = tab.huge.as_ref().and_then(|ui| {
            if !show_line_numbers {
                return None;
            }
            let state = tab.editor.read(cx);
            let line_height = state
                .line_height()
                .map(|height| height.as_f32())
                .unwrap_or(0.0);
            if line_height <= 0.0 {
                return None;
            }
            let input = state.input_bounds();
            let viewport_lines = (input.size.height.as_f32() / line_height).floor().max(1.0) as u64;
            Some(huge_gutter::render(
                HugeGutterProps {
                    offset_y: state.scroll_offset().y.as_f32(),
                    line_height,
                    top_padding: padding_y.as_f32(),
                    viewport_lines,
                    window_line: ui.window_line,
                    window_newlines: ui.window_newlines,
                    total_lines: tab.doc.line_count() as u64,
                    font_family: &font_family,
                    font_size,
                },
                window,
                cx,
            ))
        });

        let breadcrumb = self.render_breadcrumbs(index, cx);
        let ruler = self.render_ruler(index, cx);
        let map = self.render_document_map(index, cx);
        div()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .children(breadcrumb)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .p_1()
                            .child(
                                div()
                                    .flex()
                                    .h_full()
                                    .min_h(px(0.))
                                    .children(global_gutter)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .h_full()
                                            .bg(editor_background)
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|this, _, _, cx| {
                                                    if this.snippet_session.is_some() {
                                                        this.snippet_session = None;
                                                        cx.notify();
                                                    }
                                                }),
                                            )
                                            .child(
                                                Editor::new(&editor)
                                                    .with_size(EDITOR_SIZE)
                                                    .appearance(false)
                                                    .bordered(false)
                                                    .size_full()
                                                    .text_size(px(font_size))
                                                    .when(!font_family.is_empty(), |this| {
                                                        this.font_family(font_family.clone())
                                                    }),
                                            ),
                                    ),
                            )
                            .children(ruler)
                            .children(global_scrollbar),
                    )
                    .children(map),
            )
            .into_any_element()
    }

    /// Breadcrumb row for the tab: workspace ─ directories ─ file ─ symbol.
    fn render_breadcrumbs(&mut self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.settings.show_breadcrumbs {
            return None;
        }
        let tab = self.tabs.get(index)?;
        let simple = tab.doc.is_huge()
            || tab.large_file
            || tab.doc.partial_range().is_some()
            || tab.doc.is_non_text();
        let file_name = tab.doc.display_name();
        let relative = tab
            .doc
            .path
            .as_ref()
            .and_then(|path| {
                self.workspace_roots
                    .iter()
                    .find_map(|root| path.strip_prefix(root).ok())
            })
            .filter(|_| !simple);
        let root_label = if simple {
            None
        } else if !self.workspace_name.is_empty() {
            Some(self.workspace_name.clone())
        } else if let Some(path) = &tab.doc.path {
            path.parent()
                .and_then(|parent| parent.file_name())
                .map(|name| name.to_string_lossy().into_owned())
        } else {
            None
        };
        let symbol = if simple || tab.doc.is_untitled() || index != self.active {
            None
        } else {
            let cursor_line = line_ops::line_of_char(&tab.doc.text, tab.doc.cursor) + 1;
            luban_core::symbols::containing_symbol(&self.outline.symbols, cursor_line)
                .map(|symbol| (symbol.name.clone(), symbol.line))
        };
        let symbol_name = symbol.as_ref().map(|(name, _)| name.as_str());
        let mut segments = crate::navigation::breadcrumbs(
            root_label.as_deref(),
            relative,
            &file_name,
            symbol_name,
        );
        if let Some((_, line)) = &symbol {
            if let Some(last) = segments.last_mut() {
                last.action = BreadcrumbAction::Symbol(*line);
            }
        }
        let weak = cx.weak_entity();
        let mut row = div()
            .id("breadcrumbs")
            .role(Role::Navigation)
            .aria_label(tr("breadcrumbs.label"))
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .py_1()
            .text_xs()
            .text_color(cx.theme().colors.muted_foreground)
            .border_b_1()
            .border_color(cx.theme().colors.border);
        for (position, segment) in segments.into_iter().enumerate() {
            if position > 0 {
                row = row.child("›");
            }
            let label = segment.label.clone();
            let action = segment.action.clone();
            let weak_segment = weak.clone();
            row = row.child(
                div()
                    .id(format!("breadcrumb:{position}"))
                    .cursor_pointer()
                    .rounded_sm()
                    .px_1()
                    .hover(|this| this.bg(cx.theme().colors.secondary))
                    .child(label)
                    .on_click(move |_, window, cx| {
                        let action = action.clone();
                        let _ = weak_segment.update(cx, |this, cx| match action {
                            BreadcrumbAction::Root => {}
                            BreadcrumbAction::Directory(prefix) => {
                                let query = crate::quick_open::directory_query(&prefix);
                                this.open_quick_open_with(query, window, cx);
                            }
                            BreadcrumbAction::File(name) => {
                                this.open_quick_open_with(name, window, cx);
                            }
                            BreadcrumbAction::Symbol(line) => {
                                this.jump_to_line_in_tab(index, line, window, cx);
                            }
                        });
                    }),
            );
        }
        Some(row.into_any_element())
    }

    /// 1px ruler lines at the configured character columns.
    fn render_ruler(&mut self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.settings.show_ruler || self.settings.word_wrap {
            return None;
        }
        let tab = self.tabs.get(index)?;
        if tab.doc.is_huge()
            || tab.large_file
            || tab.doc.partial_range().is_some()
            || tab.doc.is_non_text()
        {
            return None;
        }
        let font_size = self.settings.font_size;
        let char_width = (font_size * 0.6).max(1.0);
        let scroll_x = tab.editor.read(cx).scroll_offset().x.as_f32();
        let columns = self.settings.ruler_columns.clone();
        if columns.is_empty() {
            return None;
        }
        let lines: Vec<AnyElement> = columns
            .into_iter()
            .map(|column| {
                let x = (column as f32 - 1.0) * char_width - scroll_x;
                div()
                    .absolute()
                    .top(px(0.))
                    .left(px(x))
                    .w(px(1.))
                    .h_full()
                    .bg(cx.theme().colors.border)
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .absolute()
                .inset_0()
                .overflow_hidden()
                .children(lines)
                .into_any_element(),
        )
    }

    /// Map on the right edge. Ordinary documents show sampled line widths;
    /// huge/partial documents show only the viewport proportion.
    fn render_document_map(&mut self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.settings.show_document_map {
            return None;
        }
        let tab = self.tabs.get(index)?;
        let state = tab.editor.read(cx);
        let total_lines = self.tab_line_count(index, cx);
        let line_height = state
            .line_height()
            .map(|height| height.as_f32())
            .unwrap_or(0.0);
        let input = state.input_bounds();
        if !document_map::should_show(total_lines, input.size.height.as_f32()) {
            return None;
        }
        if tab.doc.is_non_text() {
            return Some(
                div()
                    .id("document-map-empty")
                    .w(px(document_map::MAP_WIDTH))
                    .h_full()
                    .border_l_1()
                    .border_color(cx.theme().colors.border)
                    .bg(cx.theme().colors.secondary)
                    .role(Role::List)
                    .aria_label(tr("view.document_map"))
                    .into_any_element(),
            );
        }
        let viewport_lines = if line_height > 0.0 {
            (input.size.height.as_f32() / line_height).floor().max(1.0)
        } else {
            1.0
        };
        let start_line = if tab.doc.is_huge() {
            tab.huge
                .as_ref()
                .map(|ui| ui.window_line.saturating_sub(1) as f32)
                .unwrap_or(0.0)
        } else {
            let scroll = state.scroll_offset().y.as_f32();
            (-scroll / line_height.max(1.0)).max(0.0)
        };
        let (top, height) = document_map::viewport_rect(total_lines, start_line, viewport_lines);
        let mut map = div()
            .id("document-map")
            .relative()
            .w(px(document_map::MAP_WIDTH))
            .h_full()
            .border_l_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.secondary)
            .overflow_hidden()
            .role(Role::List)
            .aria_label(tr("view.document_map"));
        if !tab.doc.is_huge()
            && tab.doc.partial_range().is_none()
            && !tab.large_file
            && tab.doc.text.len() <= 8 * 1024 * 1024
        {
            let samples = document_map::sample(&tab.doc.text, document_map::MAX_MAP_ROWS);
            let bins = 160usize.min(samples.len());
            if bins > 0 {
                let per_bin = (samples.len() as f32 / bins as f32).ceil().max(1.0) as usize;
                let mut bars = Vec::new();
                for bin in 0..bins {
                    let start = bin * per_bin;
                    if start >= samples.len() {
                        break;
                    }
                    let end = (start + per_bin).min(samples.len());
                    let width = samples[start..end]
                        .iter()
                        .map(|row| row.width)
                        .fold(0.0f32, f32::max);
                    bars.push(
                        div()
                            .flex_1()
                            .w_full()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .h(px(1.))
                                    .w(relative(width.clamp(0.03, 1.0)))
                                    .bg(cx.theme().colors.muted_foreground),
                            )
                            .into_any_element(),
                    );
                }
                map = map.child(
                    div()
                        .absolute()
                        .inset_0()
                        .px_2()
                        .flex()
                        .flex_col()
                        .children(bars),
                );
            }
        }
        map = map.child(
            div()
                .absolute()
                .left(px(0.))
                .w_full()
                .top(relative(top))
                .h(relative(height.max(0.02)))
                .bg(cx.theme().colors.selection)
                .into_any_element(),
        );
        let weak_map = cx.weak_entity();
        let weak_map_move = weak_map.clone();
        let map_top = input.origin.y.as_f32();
        let map_height = input.size.height.as_f32().max(1.0);
        let map_line_height = line_height.max(1.0);
        let map_total = total_lines;
        let map = map
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                let ratio = (event.position.y.as_f32() - map_top) / map_height;
                let line = document_map::line_at_ratio(ratio, map_total);
                let shift = event.modifiers.shift;
                let _ = weak_map.update(cx, |this, cx| {
                    this.map_scroll_to(index, line, shift, map_line_height, window, cx);
                });
            })
            .on_mouse_move(move |event: &MouseMoveEvent, window, cx| {
                if event.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                let ratio = (event.position.y.as_f32() - map_top) / map_height;
                let line = document_map::line_at_ratio(ratio, map_total);
                let _ = weak_map_move.update(cx, |this, cx| {
                    this.map_scroll_to(index, line, false, map_line_height, window, cx);
                });
            });
        Some(map.into_any_element())
    }

    /// Scroll the viewport to a document-map line. Shift also moves the caret.
    fn map_scroll_to(
        &mut self,
        index: usize,
        line: usize,
        move_caret: bool,
        line_height: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index != self.active {
            return;
        }
        if self.tabs.get(index).is_some_and(|tab| tab.doc.is_huge()) {
            let viewport_lines = self
                .tabs
                .get(index)
                .map(|tab| {
                    let state = tab.editor.read(cx);
                    let height = state.input_bounds().size.height.as_f32();
                    (height / line_height).floor().max(1.0) as u64
                })
                .unwrap_or(1);
            self.huge_scroll_to(index, line as u64, viewport_lines, line_height, window, cx);
            if move_caret {
                self.huge_jump_line(line as u64, 1, window, cx);
            }
            return;
        }
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let editor = tab.editor.clone();
        let y = -((line.saturating_sub(1) as f32) * line_height);
        editor.update(cx, |state, cx| {
            state.set_scroll_offset(point(px(0.), px(y)), cx);
        });
        if move_caret {
            self.jump_to_line_in_tab(index, line, window, cx);
        }
        cx.notify();
    }
}

use super::*;

use gpui_kit::component::radio::Radio;
use gpui_kit::{MouseMoveEvent, MouseUpEvent, Pixels, Point};
use luban_core::search::{Match, SearchEngine};
use luban_core::settings::{FindWindowPlacement, FIND_WINDOW_MIN_HEIGHT, FIND_WINDOW_MIN_WIDTH};
use luban_core::text::{slice_chars, LineIndex};

use crate::find::{
    FindResultItem, FindTab, MarkHighlight, SearchMode, MAX_ALL_RESULTS, MAX_HIGHLIGHTS,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum OptionsTarget {
    Find,
    Mark,
}

/// Upper bound for rows painted in the in-window results lists. The collected
/// lists are larger; this only keeps a single frame cheap.
const VISIBLE_RESULTS: usize = 500;
const VISIBLE_MARKS: usize = 300;

// Compact metrics shared by every tab. Keeping them here makes it easy to
// tune the density without hunting through each layout.
const FIND_TITLE_HEIGHT: f32 = 30.0;
const FIND_TAB_HEIGHT: f32 = 30.0;
const FIND_LABEL_WIDTH: f32 = 58.0;
const FIND_STATUS_HEIGHT: f32 = 16.0;
const FIND_RESULTS_MIN_HEIGHT: f32 = 56.0;

impl LubanApp {
    // ------------------------------------------------------------------
    // Floating find/replace window
    // ------------------------------------------------------------------

    pub(super) fn render_find_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_find_window_placement(window);
        let title = self.render_find_title_bar(window, cx);
        let tabs = self.render_find_tabs(cx);
        let body = self.render_find_body(window, cx);
        let grip = self.render_find_resize_grip(window, cx);
        let x = self.find.window_x;
        let y = self.find.window_y;
        let width = self.find.window_width;
        let height = self.find.window_height;
        div()
            .id("find-window")
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(width))
            .h(px(height))
            .occlude()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.background)
            .shadow_lg()
            .child(title)
            .child(tabs)
            .child(div().flex_1().min_h(px(0.)).flex().flex_col().child(body))
            .child(grip)
            .into_any_element()
    }

    /// Place the window at the top-right anchor on first open and keep it
    /// inside the main window after the main window shrinks.
    fn ensure_find_window_placement(&mut self, window: &Window) {
        let bounds = window.bounds().size;
        let available_width = (bounds.width.as_f32() - 32.0).max(FIND_WINDOW_MIN_WIDTH);
        let available_height = (bounds.height.as_f32() - 32.0).max(FIND_WINDOW_MIN_HEIGHT);
        self.find.window_width = self
            .find
            .window_width
            .clamp(FIND_WINDOW_MIN_WIDTH, available_width);
        self.find.window_height = self
            .find
            .window_height
            .clamp(FIND_WINDOW_MIN_HEIGHT, available_height);
        if !self.find.window_placed {
            self.find.window_x = (bounds.width.as_f32() - self.find.window_width - 28.0).max(16.0);
            self.find.window_y = 96.0;
            self.find.window_placed = true;
        }
        let max_x = (bounds.width.as_f32() - self.find.window_width - 8.0).max(8.0);
        let max_y = (bounds.height.as_f32() - self.find.window_height - 8.0).max(8.0);
        self.find.window_x = self.find.window_x.clamp(8.0, max_x);
        self.find.window_y = self.find.window_y.clamp(8.0, max_y);
    }

    fn render_find_title_bar(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak_start = cx.weak_entity();
        let weak_move = cx.weak_entity();
        let weak_end = cx.weak_entity();
        let weak_close = cx.weak_entity();
        let title_color = cx.theme().colors.foreground;
        let border = cx.theme().colors.border;
        div()
            .id("find-title-bar")
            .relative()
            .h(px(FIND_TITLE_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(border)
            .bg(cx.theme().colors.secondary)
            .cursor(CursorStyle::OpenHand)
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                let _ = weak_start.update(cx, |this, cx| {
                    this.begin_find_drag(event.position, window, cx);
                });
            })
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _cx| {
                        let weak_move = weak_move.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                            if !phase.bubble() {
                                return;
                            }
                            let held = event.pressed_button == Some(MouseButton::Left);
                            let _ = weak_move.update(cx, |this, cx| {
                                if held {
                                    this.drag_find_window(event.position, window, cx);
                                } else {
                                    this.end_find_drag(cx);
                                }
                            });
                        });
                        let weak_end = weak_end.clone();
                        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                            if !phase.bubble() {
                                return;
                            }
                            let _ = weak_end.update(cx, |this, cx| {
                                this.end_find_drag(cx);
                            });
                        });
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(title_color)
                            .child(tr("find.window_title")),
                    ),
            )
            .child(
                Button::new("find-window-close")
                    .label("✕")
                    .ghost()
                    .xsmall()
                    .tooltip(tr("common.close"))
                    .on_click(move |_, window, cx| {
                        let _ = weak_close.update(cx, |this, cx| {
                            this.close_find_window(window, cx);
                        });
                    }),
            )
            .into_any_element()
    }

    fn render_find_tabs(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let active = self.find.tab;
        let mut row = div()
            .id("find-tabs")
            .h(px(FIND_TAB_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_stretch()
            .gap_1()
            .px_2()
            .border_b_1()
            .border_color(cx.theme().colors.border);
        for tab in FindTab::ALL {
            let weak = cx.weak_entity();
            let is_active = tab == active;
            let color = if is_active {
                cx.theme().colors.foreground
            } else {
                cx.theme().colors.muted_foreground
            };
            let accent = cx.theme().colors.primary;
            let hover = cx.theme().colors.secondary;
            row = row.child(
                div()
                    .id(("find-tab", tab.index()))
                    .h_full()
                    .px_3()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(color)
                    .when(is_active, |this| this.font_weight(FontWeight::MEDIUM))
                    .hover(move |this| this.bg(hover))
                    .child(tr(tab.label_key()))
                    .child(
                        div()
                            .h(px(2.))
                            .w_full()
                            .mt_1()
                            .rounded_sm()
                            .bg(if is_active {
                                accent
                            } else {
                                rgba(0x00000000).into()
                            }),
                    )
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.switch_find_tab(tab, window, cx);
                        });
                    }),
            );
        }
        row.into_any_element()
    }

    fn render_find_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.find.tab {
            FindTab::Find => self.render_find_tab(window, cx),
            FindTab::Replace => self.render_replace_tab(window, cx),
            FindTab::FindInFiles => self.render_find_in_files_body(window, cx),
            FindTab::Mark => self.render_mark_tab(window, cx),
        }
    }

    fn render_find_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let query_input = self.find.query.clone();
        let status = self.find.status.clone();
        let error = self.find.error.clone();
        let scope_active = self.find.in_selection;
        let large = self.active_tab().is_some_and(|tab| tab.large_file);

        let query_row = self.render_find_query_row(
            "find.query",
            "find.find_what",
            &query_input,
            self.settings.search_history.clone(),
            true,
            cx,
        );

        let options = self.render_find_options(OptionsTarget::Find, cx);
        let modes = self.render_search_modes(OptionsTarget::Find, cx);

        let weak_next = weak.clone();
        let weak_prev = weak.clone();
        let weak_count = weak.clone();
        let weak_all = weak.clone();
        let weak_all_open = weak.clone();
        let weak_close = weak.clone();
        let actions = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("find-next")
                    .label(tr("find.find_next_short"))
                    .primary()
                    .small()
                    .on_click(move |_, window, cx| {
                        let _ = weak_next.update(cx, |this, cx| {
                            this.find_next(true, window, cx);
                            this.focus_find_query(window, cx);
                        });
                    }),
            )
            .child(
                Button::new("find-prev")
                    .label(tr("find.find_prev_short"))
                    .small()
                    .on_click(move |_, window, cx| {
                        let _ = weak_prev.update(cx, |this, cx| {
                            this.find_next(false, window, cx);
                            this.focus_find_query(window, cx);
                        });
                    }),
            )
            .child(
                Button::new("find-count")
                    .label(tr("find.count"))
                    .xsmall()
                    .on_click(move |_, window, cx| {
                        let _ = weak_count.update(cx, |this, cx| {
                            this.count_find_matches(window, cx);
                        });
                    }),
            )
            .child(div().flex_1());

        let status_row = div()
            .h(px(FIND_STATUS_HEIGHT))
            .flex_shrink_0()
            .text_xs()
            .when(error.is_some(), |this| {
                this.text_color(cx.theme().colors.danger)
            })
            .when(error.is_none(), |this| {
                this.text_color(cx.theme().colors.muted_foreground)
            })
            .child(error.unwrap_or(status));

        let results = self.render_find_results(window, cx);

        let footer_hint = if large {
            tr("find.large_results_hint")
        } else if scope_active {
            tr("find.in_selection_hint")
        } else {
            String::new()
        };

        div()
            .size_full()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(query_row)
            .child(options)
            .child(modes)
            .child(actions)
            .child(status_row)
            .child(results)
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(footer_hint),
                    )
                    .child(
                        Button::new("find-all-current")
                            .label(tr("find.find_all_current"))
                            .xsmall()
                            .on_click(move |_, window, cx| {
                                let _ = weak_all.update(cx, |this, cx| {
                                    this.find_all_in_current(window, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new("find-all-open")
                            .label(tr("find.find_all_open"))
                            .xsmall()
                            .on_click(move |_, window, cx| {
                                let _ = weak_all_open.update(cx, |this, cx| {
                                    this.find_in_open_documents_from_find(window, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new("find-tab-close")
                            .label(tr("common.close"))
                            .xsmall()
                            .on_click(move |_, window, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.close_find_window(window, cx);
                                });
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_replace_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let query_input = self.find.query.clone();
        let replacement_input = self.find.replacement.clone();
        let query = self.find.query_text(cx);
        let replacement = self.find.replacement_text(cx);
        let error = self.find.error.clone();
        let status = self.find.status.clone();
        let match_count = self.find.matches.len();

        let query_row = self.render_find_query_row(
            "replace.query",
            "find.find_what",
            &query_input,
            self.settings.search_history.clone(),
            true,
            cx,
        );
        let replacement_row = self.render_find_query_row(
            "replace.value",
            "find.replace_with",
            &replacement_input,
            self.settings.replace_history.clone(),
            false,
            cx,
        );
        let options = self.render_find_options(OptionsTarget::Find, cx);
        let modes = self.render_search_modes(OptionsTarget::Find, cx);

        let weak_next = weak.clone();
        let weak_replace = weak.clone();
        let weak_all = weak.clone();
        let weak_all_open = weak.clone();
        let weak_open_replace = weak.clone();
        let weak_undo = weak.clone();
        let weak_close = weak.clone();
        let has_undo = self.replace_state.undo.is_some();
        let actions = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("replace-find-next")
                    .label(tr("find.find_next_short"))
                    .primary()
                    .small()
                    .on_click(move |_, window, cx| {
                        let _ = weak_next.update(cx, |this, cx| {
                            this.find_next(true, window, cx);
                            this.focus_find_query(window, cx);
                        });
                    }),
            )
            .child(
                Button::new("replace-one")
                    .label(tr("find.replace"))
                    .small()
                    .on_click(move |_, window, cx| {
                        let _ = weak_replace.update(cx, |this, cx| {
                            this.replace_one(window, cx);
                        });
                    }),
            )
            .child(
                Button::new("replace-all")
                    .label(tr("find.replace_all"))
                    .small()
                    .on_click(move |_, window, cx| {
                        let _ = weak_all.update(cx, |this, cx| {
                            this.replace_all(window, cx);
                        });
                    }),
            )
            .child(div().flex_1());

        let status_row = div()
            .h(px(FIND_STATUS_HEIGHT))
            .flex_shrink_0()
            .text_xs()
            .when(error.is_some(), |this| {
                this.text_color(cx.theme().colors.danger)
            })
            .when(error.is_none(), |this| {
                this.text_color(cx.theme().colors.muted_foreground)
            })
            .child(error.unwrap_or_else(|| {
                if !status.is_empty() {
                    status
                } else if query.is_empty() {
                    tr("find.empty_query")
                } else if match_count == 0 {
                    tr("find.no_matches")
                } else {
                    tr_args("find.count_result", &[("count", match_count.to_string())])
                }
            }));

        let preview = self.render_replace_preview_list(&query, &replacement, window, cx);

        div()
            .size_full()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(query_row)
            .child(replacement_row)
            .child(options)
            .child(modes)
            .child(actions)
            .child(status_row)
            .child(preview)
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        Button::new("replace-open-docs")
                            .label(tr("find.replace_open_docs"))
                            .xsmall()
                            .on_click(move |_, window, cx| {
                                let _ = weak_open_replace.update(cx, |this, cx| {
                                    this.replace_in_open_documents(window, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new("replace-in-files")
                            .label(tr("find.replace_in_files"))
                            .xsmall()
                            .on_click(move |_, window, cx| {
                                let _ = weak_all_open.update(cx, |this, cx| {
                                    this.find_in_files_from_replace(window, cx);
                                });
                            }),
                    )
                    .when(has_undo, |this| {
                        this.child(
                            Button::new("replace-undo")
                                .label(tr("ff.undo_replace"))
                                .xsmall()
                                .on_click(move |_, window, cx| {
                                    let _ = weak_undo.update(cx, |this, cx| {
                                        this.undo_workspace_replace(window, cx);
                                        this.find.status = this.replace_state.status.clone();
                                        this.find.status_sticky = true;
                                        this.find.dirty = true;
                                        cx.notify();
                                    });
                                }),
                        )
                    })
                    .child(
                        Button::new("replace-tab-close")
                            .label(tr("common.close"))
                            .xsmall()
                            .on_click(move |_, window, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.close_find_window(window, cx);
                                });
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_mark_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.weak_entity();
        let mark_input = self.find.mark_query.clone();
        let marks = self.render_mark_options(cx);
        let modes = self.render_search_modes(OptionsTarget::Mark, cx);
        let status = self.find.mark_status.clone();

        let weak_all = weak.clone();
        let weak_bookmark = weak.clone();
        let weak_clear = weak.clone();
        let weak_clear_all = weak.clone();
        let weak_close = weak.clone();
        let actions = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("mark-all")
                    .label(tr("find.mark_all"))
                    .primary()
                    .small()
                    .on_click(move |_, window, cx| {
                        let _ = weak_all.update(cx, |this, cx| {
                            this.apply_marks(false, window, cx);
                        });
                    }),
            )
            .child(
                Button::new("mark-bookmark")
                    .label(tr("find.mark_bookmark"))
                    .small()
                    .on_click(move |_, window, cx| {
                        let _ = weak_bookmark.update(cx, |this, cx| {
                            this.apply_marks(true, window, cx);
                        });
                    }),
            )
            .child(
                Button::new("mark-clear")
                    .label(tr("find.clear_mark"))
                    .small()
                    .on_click(move |_, window, cx| {
                        let _ = weak_clear.update(cx, |this, cx| {
                            this.clear_active_marks(window, cx);
                        });
                    }),
            )
            .child(
                Button::new("mark-clear-all")
                    .label(tr("find.clear_all_marks"))
                    .small()
                    .on_click(move |_, _window, cx| {
                        let _ = weak_clear_all.update(cx, |this, cx| {
                            this.clear_all_marks(cx);
                        });
                    }),
            )
            .child(div().flex_1())
            .child(
                Button::new("mark-tab-close")
                    .label(tr("common.close"))
                    .xsmall()
                    .on_click(move |_, window, cx| {
                        let _ = weak_close.update(cx, |this, cx| {
                            this.close_find_window(window, cx);
                        });
                    }),
            );

        let status_row = div()
            .h(px(FIND_STATUS_HEIGHT))
            .flex_shrink_0()
            .text_xs()
            .text_color(cx.theme().colors.muted_foreground)
            .child(status);

        let results = self.render_mark_results(window, cx);

        div()
            .size_full()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(FIND_LABEL_WIDTH))
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(tr("find.mark_what")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(Input::new(&mark_input).w_full().small()),
                    ),
            )
            .child(marks)
            .child(modes)
            .child(actions)
            .child(status_row)
            .child(results)
            .into_any_element()
    }

    fn render_find_query_row(
        &self,
        id: &'static str,
        label_key: &'static str,
        input: &Entity<InputState>,
        history: Vec<String>,
        find_query: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.weak_entity();
        let input_entity = input.clone();
        let history_button =
            Button::new(id)
                .label("▾")
                .ghost()
                .xsmall()
                .tooltip(tr("find.history"))
                .dropdown_menu(move |mut menu, window, _cx| {
                    if history.is_empty() {
                        menu = menu.item(PopupMenuItem::new(tr("find.no_history")).disabled(true));
                    }
                    for entry in &history {
                        let value = entry.clone();
                        let input = input_entity.clone();
                        let weak = weak.clone();
                        menu = menu.item(PopupMenuItem::new(entry.clone()).on_click(
                            move |_, window, cx| {
                                let value = value.clone();
                                let _ = weak.update(cx, |this, cx| {
                                    input.update(cx, |state, cx| {
                                        state.set_value(value.clone(), window, cx)
                                    });
                                    if find_query {
                                        this.find.dirty = true;
                                        this.find.status_sticky = false;
                                    }
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    let weak_clear = weak.clone();
                    menu = menu.separator().item(
                        PopupMenuItem::new(tr("find.clear_history")).on_click(move |_, _, cx| {
                            let _ = weak_clear.update(cx, |this, cx| {
                                this.clear_search_history(cx);
                            });
                        }),
                    );
                    Self::fit_menu(menu, window)
                });
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .w(px(FIND_LABEL_WIDTH))
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr(label_key)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(Input::new(input).w_full().small()),
            )
            .child(history_button)
            .into_any_element()
    }

    fn render_find_options(&mut self, target: OptionsTarget, cx: &mut Context<Self>) -> AnyElement {
        let prefix = match target {
            OptionsTarget::Find => "find-option",
            OptionsTarget::Mark => "mark-option",
        };
        let mut entries: Vec<(String, &'static str, bool, u8)> = Vec::new();
        match target {
            OptionsTarget::Find => {
                entries.push((
                    format!("{prefix}-backward"),
                    "find.backward",
                    self.find.backward,
                    3,
                ));
                entries.push((
                    format!("{prefix}-word"),
                    "find.whole_word",
                    self.find.whole_word,
                    1,
                ));
                entries.push((
                    format!("{prefix}-case"),
                    "find.match_case",
                    self.find.match_case,
                    0,
                ));
                entries.push((
                    format!("{prefix}-wrap"),
                    "find.wrap_around",
                    self.find.wrap_around,
                    2,
                ));
                entries.push((
                    format!("{prefix}-selection"),
                    "find.in_selection",
                    self.find.in_selection,
                    4,
                ));
                if self.find.mode() == SearchMode::Regex {
                    entries.push((
                        format!("{prefix}-enhanced"),
                        "find.enhanced_regex",
                        self.find.enhanced_regex,
                        5,
                    ));
                    entries.push((
                        format!("{prefix}-dot"),
                        "find.dot_newline",
                        self.find.dot_matches_newline,
                        6,
                    ));
                }
            }
            OptionsTarget::Mark => {
                entries.push((
                    format!("{prefix}-word"),
                    "find.whole_word",
                    self.find.mark_whole_word,
                    1,
                ));
                entries.push((
                    format!("{prefix}-case"),
                    "find.match_case",
                    self.find.mark_match_case,
                    0,
                ));
                if self.find.mark_mode() == SearchMode::Regex {
                    entries.push((
                        format!("{prefix}-enhanced"),
                        "find.enhanced_regex",
                        self.find.mark_enhanced_regex,
                        5,
                    ));
                }
            }
        }
        let mut row = div().flex().flex_wrap().items_center().gap_x_3().gap_y_1();
        for (id, label, checked, field) in entries {
            row = row.child(
                Checkbox::new(id)
                    .label(tr(label))
                    .checked(checked)
                    .xsmall()
                    .on_click(cx.listener(move |this, checked, _, cx| {
                        let checked = *checked;
                        match (target, field) {
                            (OptionsTarget::Find, 0) => this.find.match_case = checked,
                            (OptionsTarget::Find, 1) => this.find.whole_word = checked,
                            (OptionsTarget::Find, 2) => this.find.wrap_around = checked,
                            (OptionsTarget::Find, 3) => this.find.backward = checked,
                            (OptionsTarget::Find, 4) => {
                                this.find.in_selection = checked;
                                this.find.scope = if checked {
                                    let (a, b) = this.active_selection();
                                    (a != b).then_some((a, b))
                                } else {
                                    None
                                };
                            }
                            (OptionsTarget::Find, 5) => this.find.enhanced_regex = checked,
                            (OptionsTarget::Find, 6) => this.find.dot_matches_newline = checked,
                            (OptionsTarget::Mark, 1) => this.find.mark_whole_word = checked,
                            (OptionsTarget::Mark, 0) => this.find.mark_match_case = checked,
                            (OptionsTarget::Mark, 5) => this.find.mark_enhanced_regex = checked,
                            _ => {}
                        }
                        if target == OptionsTarget::Find {
                            this.find.dirty = true;
                            this.find.status_sticky = false;
                            if field <= 1 || field == 5 {
                                this.find.scope = None;
                            }
                        }
                        cx.notify();
                    })),
            );
        }
        row.into_any_element()
    }

    fn render_mark_options(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.render_find_options(OptionsTarget::Mark, cx)
    }

    fn render_search_modes(&mut self, target: OptionsTarget, cx: &mut Context<Self>) -> AnyElement {
        let current = match target {
            OptionsTarget::Find => self.find.mode(),
            OptionsTarget::Mark => self.find.mark_mode(),
        };
        let prefix = match target {
            OptionsTarget::Find => "find-mode",
            OptionsTarget::Mark => "mark-mode",
        };
        let mut row = div().flex().flex_wrap().items_center().gap_2().child(
            div()
                .w(px(FIND_LABEL_WIDTH))
                .text_xs()
                .text_color(cx.theme().colors.muted_foreground)
                .child(tr("find.mode")),
        );
        for mode in [SearchMode::Normal, SearchMode::Regex, SearchMode::Extend] {
            let weak = cx.weak_entity();
            row = row.child(
                Radio::new((prefix, mode.index()))
                    .label(tr(mode.label_key()))
                    .checked(current == mode)
                    .xsmall()
                    .on_click(move |checked, _, cx| {
                        if !*checked {
                            return;
                        }
                        let _ = weak.update(cx, |this, cx| {
                            match target {
                                OptionsTarget::Find => {
                                    this.find.set_mode(mode);
                                    this.find.dirty = true;
                                    this.find.status_sticky = false;
                                    this.find.scope = None;
                                }
                                OptionsTarget::Mark => this.find.set_mark_mode(mode),
                            }
                            cx.notify();
                        });
                    }),
            );
        }
        row.into_any_element()
    }

    fn render_find_results(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let count = self.find.all_results.len();
        let huge_count = self.find.huge_results.len();
        let limited = self.find.all_results_limited;
        let empty = count == 0 && huge_count == 0;
        let weak_copy = cx.weak_entity();
        let weak_clear = cx.weak_entity();
        let mut rows: Vec<AnyElement> = Vec::new();
        for (index, item) in self
            .find
            .all_results
            .iter()
            .take(VISIBLE_RESULTS)
            .enumerate()
        {
            let matched = item.matched;
            let weak = cx.weak_entity();
            rows.push(
                div()
                    .id(("find-result", index))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().colors.secondary))
                    .child(
                        div()
                            .w(px(72.))
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(format!("{}:{}", item.line, item.column)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_xs()
                            .child(item.preview.clone()),
                    )
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.jump_to_find_result(matched, window, cx);
                        });
                    })
                    .into_any_element(),
            );
        }
        for (index, hit) in self
            .find
            .huge_results
            .iter()
            .take(VISIBLE_RESULTS)
            .enumerate()
        {
            let hit = *hit;
            let weak = cx.weak_entity();
            rows.push(
                div()
                    .id(("find-huge-result", index))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().colors.secondary))
                    .child(div().flex_1().min_w(px(0.)).text_xs().child(tr_args(
                        "find.huge_result",
                        &[("line", hit.line.to_string())],
                    )))
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.huge_find_jump(hit, window, cx);
                        });
                    })
                    .into_any_element(),
            );
        }
        if limited || count > VISIBLE_RESULTS || huge_count > VISIBLE_RESULTS {
            rows.push(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr_args(
                        "find.results_limited",
                        &[("count", VISIBLE_RESULTS.to_string())],
                    ))
                    .into_any_element(),
            );
        }
        let total = count + huge_count;
        let list = div()
            .id("find-results-list")
            .flex_1()
            .min_h(px(FIND_RESULTS_MIN_HEIGHT))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_0()
            .border_1()
            .border_color(cx.theme().colors.border)
            .rounded_sm()
            .when(empty, |this| {
                this.items_center().justify_center().child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(tr("find.results_empty")),
                )
            })
            .children(rows);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(FIND_RESULTS_MIN_HEIGHT))
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(tr("find.results_title")),
                    )
                    .when(total > 0, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().colors.muted_foreground)
                                .child(format!("({total})")),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("find-results-copy")
                            .label(tr("find.copy_results"))
                            .ghost()
                            .xsmall()
                            .disabled(empty)
                            .on_click(move |_, _, cx| {
                                let _ = weak_copy.update(cx, |this, cx| {
                                    this.copy_find_results(cx);
                                });
                            }),
                    )
                    .child(
                        Button::new("find-results-clear")
                            .label(tr("find.clear_results"))
                            .ghost()
                            .xsmall()
                            .disabled(empty)
                            .on_click(move |_, _, cx| {
                                let _ = weak_clear.update(cx, |this, cx| {
                                    this.clear_find_results(cx);
                                });
                            }),
                    ),
            )
            .child(list)
            .into_any_element()
    }

    fn render_replace_preview_list(
        &mut self,
        query: &str,
        replacement: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let large = self.active_tab().is_some_and(|tab| tab.large_file);
        let matches = self.find.matches.clone();
        let mut rows: Vec<AnyElement> = Vec::new();
        if !query.is_empty() && !matches.is_empty() {
            let engine = self.find.engine(query);
            let text = self
                .tabs
                .get(self.active)
                .map(|tab| tab.doc.text.as_str())
                .unwrap_or("");
            for (index, matched) in matches.iter().take(VISIBLE_RESULTS).enumerate() {
                let before = one_line(&slice_chars(text, matched.start, matched.end), 120);
                let after = one_line(&engine.replacement_for(text, *matched, replacement), 120);
                let weak = cx.weak_entity();
                let matched = *matched;
                rows.push(
                    div()
                        .id(("replace-preview-row", index))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().colors.secondary))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_xs()
                                .text_color(cx.theme().colors.muted_foreground)
                                .child(before),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().colors.muted_foreground)
                                .child("→"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_xs()
                                .child(after),
                        )
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.jump_to_find_result(matched, window, cx);
                            });
                        })
                        .into_any_element(),
                );
            }
        }
        let empty = rows.is_empty();
        let total = matches.len();
        let list = div()
            .id("replace-preview-list")
            .flex_1()
            .min_h(px(FIND_RESULTS_MIN_HEIGHT))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_0()
            .border_1()
            .border_color(cx.theme().colors.border)
            .rounded_sm()
            .when(empty, |this| {
                this.items_center().justify_center().child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(if large {
                            tr("find.large_results_hint")
                        } else if query.is_empty() {
                            tr("find.empty_query")
                        } else {
                            tr("find.no_matches")
                        }),
                )
            })
            .children(rows);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(FIND_RESULTS_MIN_HEIGHT))
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(tr("find.replace_preview_title")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(format!("({total})")),
                    ),
            )
            .child(list)
            .into_any_element()
    }

    fn render_mark_results(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.tabs.get(self.active) else {
            return div().into_any_element();
        };
        let unsupported = tab.large_file
            || tab.doc.is_huge()
            || tab.doc.is_non_text()
            || tab.doc.partial_range().is_some();
        let marks = tab
            .mark
            .as_ref()
            .map(|mark| mark.matches.clone())
            .unwrap_or_default();
        let text = tab.doc.text.as_str();
        let mut rows: Vec<AnyElement> = Vec::new();
        if !unsupported && !marks.is_empty() {
            let index = LineIndex::new(text);
            for (row, matched) in marks.iter().take(VISIBLE_MARKS).enumerate() {
                let (line, column) = index.line_col(matched.start);
                let preview = index.line_text(text, line.saturating_sub(1));
                let weak = cx.weak_entity();
                let matched = *matched;
                rows.push(
                    div()
                        .id(("mark-result", row))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().colors.secondary))
                        .child(
                            div()
                                .w(px(72.))
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(cx.theme().colors.muted_foreground)
                                .child(format!("{line}:{column}")),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_xs()
                                .child(textutil::preview(&preview, 200)),
                        )
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.jump_to_find_result(matched, window, cx);
                            });
                        })
                        .into_any_element(),
                );
            }
        }
        let total = marks.len();
        if total > VISIBLE_MARKS {
            rows.push(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr_args(
                        "find.results_limited",
                        &[("count", VISIBLE_MARKS.to_string())],
                    ))
                    .into_any_element(),
            );
        }
        let empty = rows.is_empty();
        let list = div()
            .id("mark-results-list")
            .flex_1()
            .min_h(px(FIND_RESULTS_MIN_HEIGHT))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .border_1()
            .border_color(cx.theme().colors.border)
            .rounded_sm()
            .when(empty, |this| {
                this.items_center().justify_center().child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(if unsupported {
                            tr("find.mark_unsupported")
                        } else {
                            tr("find.mark_empty")
                        }),
                )
            })
            .children(rows);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(FIND_RESULTS_MIN_HEIGHT))
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(tr("find.mark_results_title")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(format!("({total})")),
                    ),
            )
            .child(list)
            .into_any_element()
    }

    fn render_find_resize_grip(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak_start = cx.weak_entity();
        let weak_move = cx.weak_entity();
        let weak_end = cx.weak_entity();
        let mut grip = div()
            .id("find-resize-grip")
            .absolute()
            .right_0()
            .bottom_0()
            .w(px(16.))
            .h(px(16.))
            .occlude()
            .cursor(CursorStyle::ResizeUpLeftDownRight)
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                let _ = weak_start.update(cx, |this, cx| {
                    this.begin_find_resize(event.position, window, cx);
                });
            })
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _cx| {
                        let weak_move = weak_move.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                            if !phase.bubble() {
                                return;
                            }
                            let held = event.pressed_button == Some(MouseButton::Left);
                            let _ = weak_move.update(cx, |this, cx| {
                                if held {
                                    this.resize_find_window(event.position, window, cx);
                                } else {
                                    this.end_find_resize(cx);
                                }
                            });
                        });
                        let weak_end = weak_end.clone();
                        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                            if !phase.bubble() {
                                return;
                            }
                            let _ = weak_end.update(cx, |this, cx| {
                                this.end_find_resize(cx);
                            });
                        });
                    },
                )
                .absolute()
                .inset_0(),
            );
        let color = cx.theme().colors.muted_foreground;
        grip = grip.child(
            div()
                .absolute()
                .right(px(3.))
                .bottom(px(3.))
                .w(px(8.))
                .h(px(8.))
                .border_r_1()
                .border_b_1()
                .border_color(color),
        );
        grip.into_any_element()
    }

    // ------------------------------------------------------------------
    // Window actions
    // ------------------------------------------------------------------

    pub(super) fn switch_find_tab(
        &mut self,
        tab: FindTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.find.tab = tab;
        self.find_in_files.open = tab == FindTab::FindInFiles;
        match tab {
            FindTab::Find | FindTab::Replace => {
                self.find
                    .query
                    .update(cx, |state, cx| state.focus(window, cx));
            }
            FindTab::FindInFiles => {
                self.find_in_files.open = true;
                self.find_in_files
                    .query
                    .update(cx, |state, cx| state.focus(window, cx));
            }
            FindTab::Mark => {
                if self.find.mark_query.read(cx).value().is_empty() {
                    let query = self.find.query_text(cx);
                    if !query.is_empty() {
                        self.find.mark_query.update(cx, |state, cx| {
                            state.set_value(query, window, cx);
                        });
                        self.find.mark_match_case = self.find.match_case;
                        self.find.mark_whole_word = self.find.whole_word;
                        self.find.mark_regex = self.find.regex;
                        self.find.mark_extended = self.find.extended;
                        self.find.mark_enhanced_regex = self.find.enhanced_regex;
                    }
                }
                self.find
                    .mark_query
                    .update(cx, |state, cx| state.focus(window, cx));
            }
        }
        cx.notify();
    }

    pub(super) fn close_find_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find.open = false;
        self.find.drag_offset = None;
        self.find.resize_origin = None;
        self.cancel_huge_search();
        self.cancel_find_in_files(cx);
        self.find_in_files.open = false;
        self.find.clear();
        self.focus_editor(window, cx);
        cx.notify();
    }

    fn focus_find_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find
            .query
            .update(cx, |state, cx| state.focus(window, cx));
    }

    pub(super) fn begin_find_drag(
        &mut self,
        position: Point<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.find.drag_offset = Some((
            position.x.as_f32() - self.find.window_x,
            position.y.as_f32() - self.find.window_y,
        ));
        cx.notify();
    }

    pub(super) fn drag_find_window(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((offset_x, offset_y)) = self.find.drag_offset else {
            return;
        };
        let bounds = window.bounds().size;
        let max_x = (bounds.width.as_f32() - self.find.window_width - 8.0).max(8.0);
        let max_y = (bounds.height.as_f32() - self.find.window_height - 8.0).max(8.0);
        self.find.window_x = (position.x.as_f32() - offset_x).clamp(8.0, max_x);
        self.find.window_y = (position.y.as_f32() - offset_y).clamp(8.0, max_y);
        cx.notify();
    }

    pub(super) fn end_find_drag(&mut self, cx: &mut Context<Self>) {
        if self.find.drag_offset.take().is_some() {
            self.persist_find_geometry();
            cx.notify();
        }
    }

    pub(super) fn begin_find_resize(
        &mut self,
        position: Point<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.find.resize_origin = Some((
            position.x.as_f32(),
            position.y.as_f32(),
            self.find.window_width,
            self.find.window_height,
        ));
        cx.notify();
    }

    pub(super) fn resize_find_window(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((origin_x, origin_y, start_width, start_height)) = self.find.resize_origin else {
            return;
        };
        let bounds = window.bounds().size;
        let max_width = (bounds.width.as_f32() - 16.0).max(FIND_WINDOW_MIN_WIDTH);
        let max_height = (bounds.height.as_f32() - 16.0).max(FIND_WINDOW_MIN_HEIGHT);
        let width =
            (start_width + position.x.as_f32() - origin_x).clamp(FIND_WINDOW_MIN_WIDTH, max_width);
        let height = (start_height + position.y.as_f32() - origin_y)
            .clamp(FIND_WINDOW_MIN_HEIGHT, max_height);
        let max_x = (bounds.width.as_f32() - width - 8.0).max(8.0);
        let max_y = (bounds.height.as_f32() - height - 8.0).max(8.0);
        self.find.window_width = width;
        self.find.window_height = height;
        self.find.window_x = self.find.window_x.min(max_x).max(8.0);
        self.find.window_y = self.find.window_y.min(max_y).max(8.0);
        cx.notify();
    }

    pub(super) fn end_find_resize(&mut self, cx: &mut Context<Self>) {
        if self.find.resize_origin.take().is_some() {
            self.persist_find_geometry();
            cx.notify();
        }
    }

    fn persist_find_geometry(&mut self) {
        self.settings.find_window = FindWindowPlacement {
            x: Some(self.find.window_x),
            y: Some(self.find.window_y),
            width: self.find.window_width,
            height: self.find.window_height,
        };
        self.commit_settings();
    }

    // ------------------------------------------------------------------
    // Find / mark actions
    // ------------------------------------------------------------------

    pub(super) fn count_find_matches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_doc().is_some_and(Document::is_huge) {
            self.find.status = tr("find.count_huge");
            cx.notify();
            return;
        }
        self.sync_active(window, cx);
        let query = self.find.query_text(cx);
        if query.is_empty() {
            self.find.status = tr("find.empty_query");
            cx.notify();
            return;
        }
        if self.find.dirty {
            let text = self
                .tabs
                .get(self.active)
                .map(|tab| tab.doc.text.as_str())
                .unwrap_or("");
            self.find.refresh(text, &query);
            self.find.dirty = false;
        }
        let count = self.find.matches.len();
        self.find.status = if count == 0 {
            tr("find.no_matches")
        } else {
            tr_args("find.count_result", &[("count", count.to_string())])
        };
        self.find.status_sticky = true;
        cx.notify();
    }

    pub(super) fn find_all_in_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_doc().is_some_and(Document::is_huge) {
            self.find.all_results.clear();
            self.find.all_results_limited = false;
            self.huge_find_all(window, cx);
            return;
        }
        self.sync_active(window, cx);
        let query = self.find.query_text(cx);
        if query.is_empty() {
            self.find.status = tr("find.empty_query");
            cx.notify();
            return;
        }
        self.settings.record_search(&query);
        self.commit_settings();
        if self.find.dirty {
            let text = self
                .tabs
                .get(self.active)
                .map(|tab| tab.doc.text.as_str())
                .unwrap_or("");
            self.find.refresh(text, &query);
            self.find.dirty = false;
        }
        let (items, limited, count) = {
            let text = self
                .tabs
                .get(self.active)
                .map(|tab| tab.doc.text.as_str())
                .unwrap_or("");
            let (items, limited) = build_find_results(text, &self.find.matches, MAX_ALL_RESULTS);
            (items, limited, self.find.matches.len())
        };
        self.find.all_results = items;
        self.find.all_results_limited = limited;
        self.find.huge_results.clear();
        self.find.huge_truncated = false;
        self.find.status = if count == 0 {
            tr("find.no_matches")
        } else {
            tr_args("find.count_result", &[("count", count.to_string())])
        };
        self.find.status_sticky = true;
        cx.notify();
    }

    pub(super) fn copy_find_results(&mut self, cx: &mut Context<Self>) {
        let mut text = String::new();
        for item in &self.find.all_results {
            text.push_str(&format!(
                "{}:{}: {}\n",
                item.line, item.column, item.preview
            ));
        }
        for hit in &self.find.huge_results {
            text.push_str(&format!("{}\n", hit.line));
        }
        let count = self.find.all_results.len() + self.find.huge_results.len();
        if text.is_empty() {
            self.find.status = tr("find.no_matches");
        } else {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.find.status = tr_args("find.results_copied", &[("count", count.to_string())]);
            self.find.status_sticky = true;
        }
        cx.notify();
    }

    pub(super) fn clear_find_results(&mut self, cx: &mut Context<Self>) {
        self.find.all_results.clear();
        self.find.all_results_limited = false;
        self.find.huge_results.clear();
        self.find.huge_truncated = false;
        self.find.status = tr("find.results_cleared");
        self.find.status_sticky = true;
        cx.notify();
    }

    pub(super) fn jump_to_find_result(
        &mut self,
        matched: Match,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_active(window, cx);
        self.find.current = self.find.matches.iter().position(|item| *item == matched);
        self.find.generation = self.find.generation.wrapping_add(1);
        self.select_char_range(matched.start, matched.end, window, cx);
        cx.notify();
    }

    /// Hand the current query to the Find in Files tab and search every open
    /// document, mirroring Notepad--'s "Find All in All Opened Documents".
    pub(super) fn find_in_open_documents_from_find(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let query = self.find.query_text(cx);
        if query.is_empty() {
            self.find.status = tr("find.empty_query");
            cx.notify();
            return;
        }
        let replacement = self.find.replacement_text(cx);
        let options = self.find.options();
        self.find_in_files.query.update(cx, |state, cx| {
            state.set_value(query.clone(), window, cx);
        });
        self.find_in_files.replacement.update(cx, |state, cx| {
            state.set_value(replacement.clone(), window, cx);
        });
        self.find_in_files.match_case = options.match_case;
        self.find_in_files.whole_word = options.whole_word;
        self.find_in_files.regex = options.regex;
        self.find_in_files.enhanced_regex = options.enhanced_regex;
        self.find_in_files.open_documents = true;
        self.find_in_files.workspace = false;
        self.find_in_files.show_replace = false;
        self.switch_find_tab(FindTab::FindInFiles, window, cx);
        self.start_find_in_files(cx);
    }

    /// Open the Find in Files tab with the replace row expanded so the user can
    /// run the existing workspace replace flow.
    pub(super) fn find_in_files_from_replace(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.find.query_text(cx).is_empty() {
            self.find.status = tr("find.empty_query");
            cx.notify();
            return;
        }
        let query = self.find.query_text(cx);
        let replacement = self.find.replacement_text(cx);
        self.find_in_files.query.update(cx, |state, cx| {
            state.set_value(query, window, cx);
        });
        self.find_in_files.replacement.update(cx, |state, cx| {
            state.set_value(replacement, window, cx);
        });
        self.find_in_files.show_replace = true;
        self.find_in_files.workspace = !self.workspace_roots.is_empty();
        self.switch_find_tab(FindTab::FindInFiles, window, cx);
        self.find_in_files
            .replacement
            .update(cx, |state, cx| state.focus(window, cx));
    }

    pub(super) fn apply_marks(
        &mut self,
        bookmark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_active(window, cx);
        let Some(index) = self.tabs.get(self.active).map(|_| self.active) else {
            return;
        };
        let unsupported = {
            let tab = &self.tabs[index];
            tab.large_file
                || tab.doc.is_huge()
                || tab.doc.is_non_text()
                || tab.doc.partial_range().is_some()
        };
        if unsupported {
            self.find.mark_status = tr("find.mark_unsupported");
            cx.notify();
            return;
        }
        let query = self.find.mark_query_text(cx);
        if query.is_empty() {
            self.find.mark_status = tr("find.empty_query");
            cx.notify();
            return;
        }
        let options = self.find.mark_options();
        let pattern = if self.find.mark_extended {
            luban_core::search::expand_extended(&query)
        } else {
            query.clone()
        };
        let engine = SearchEngine::new(pattern.clone(), options);
        if let Some(error) = engine.error() {
            self.find.mark_status = error.to_string();
            cx.notify();
            return;
        }
        let (matches, lines, revision) = {
            let tab = &self.tabs[index];
            let matches = engine.find_all(&tab.doc.text, None, MAX_HIGHLIGHTS);
            let index_lines = LineIndex::new(&tab.doc.text);
            let mut lines = Vec::with_capacity(matches.len());
            for matched in &matches {
                lines.push(index_lines.line_of_char(matched.start) + 1);
            }
            (matches, lines, tab.doc.revision())
        };
        let count = matches.len();
        let tab = &mut self.tabs[index];
        tab.mark = Some(MarkHighlight {
            query: pattern,
            options,
            matches,
            revision,
        });
        tab.mark_generation = tab.mark_generation.wrapping_add(1);
        if bookmark {
            for line in lines {
                tab.bookmarks.insert(line);
            }
            tab.bookmark_revision = tab.bookmark_revision.wrapping_add(1);
        }
        self.refresh_bookmark_decorations(index, cx);
        self.find.mark_status = if count == 0 {
            tr("find.no_matches")
        } else {
            tr_args("find.marked_count", &[("count", count.to_string())])
        };
        cx.notify();
    }

    pub(super) fn clear_active_marks(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            if tab.mark.take().is_some() {
                tab.mark_generation = tab.mark_generation.wrapping_add(1);
            }
        }
        self.find.mark_status = tr("find.marks_cleared");
        cx.notify();
    }

    pub(super) fn clear_all_marks(&mut self, cx: &mut Context<Self>) {
        let mut cleared = 0usize;
        for tab in &mut self.tabs {
            if tab.mark.take().is_some() {
                tab.mark_generation = tab.mark_generation.wrapping_add(1);
                cleared += 1;
            }
        }
        self.find.mark_status =
            tr_args("find.marks_cleared_docs", &[("count", cleared.to_string())]);
        cx.notify();
    }
}

fn build_find_results(text: &str, matches: &[Match], limit: usize) -> (Vec<FindResultItem>, bool) {
    let index = LineIndex::new(text);
    let mut items = Vec::with_capacity(matches.len().min(limit));
    for matched in matches.iter().take(limit) {
        let (line, column) = index.line_col(matched.start);
        let preview = index.line_text(text, line.saturating_sub(1));
        items.push(FindResultItem {
            matched: *matched,
            line,
            column,
            preview: textutil::preview(&preview, 200),
        });
    }
    (items, matches.len() > limit)
}

fn one_line(text: &str, max_chars: usize) -> String {
    textutil::preview(text, max_chars)
        .replace('\n', " ↵ ")
        .replace('\r', "")
}

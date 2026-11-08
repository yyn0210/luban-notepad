use super::*;

fn sort_arrow(active: bool, descending: bool) -> &'static str {
    if !active {
        ""
    } else if descending {
        " ↓"
    } else {
        " ↑"
    }
}

fn picker_name(
    row: &PickerRow,
    foreground: gpui_kit::Hsla,
    highlight: gpui_kit::Hsla,
) -> AnyElement {
    if let Some((start, end)) = row.match_range {
        if start <= end && end <= row.name.len() {
            return div()
                .flex()
                .min_w(px(0.))
                .child(row.name[..start].to_string())
                .child(
                    div()
                        .rounded_sm()
                        .bg(highlight)
                        .text_color(foreground)
                        .child(row.name[start..end].to_string()),
                )
                .child(row.name[end..].to_string())
                .into_any_element();
        }
    }
    div().truncate().child(row.name.clone()).into_any_element()
}

fn picker_row_element(
    index: usize,
    row: &PickerRow,
    selected: &std::collections::BTreeSet<PathBuf>,
    active: Option<usize>,
    weak: gpui_kit::WeakEntity<LubanApp>,
    colors: PickerColors,
) -> AnyElement {
    let is_selected = row.is_file() && selected.contains(&row.path);
    let is_active = active == Some(index);
    let parent = row.kind == PickerRowKind::Parent;
    let weak_click = weak.clone();
    let weak_menu_open = weak.clone();
    let weak_menu_copy = weak.clone();
    let weak_menu_reveal = weak.clone();
    let weak_menu_new_folder = weak.clone();
    let weak_menu_refresh = weak.clone();
    let menu_path = row.path.clone();
    let menu_is_dir = row.is_dir();
    let is_file = row.is_file();
    let element = div()
        .id(format!("file-picker-row:{}:{}", index, row.path.display()))
        .h(px(FILE_PICKER_ROW_HEIGHT))
        .w_full()
        .flex()
        .flex_shrink_0()
        .items_center()
        .px_2()
        .gap_2()
        .text_sm()
        .cursor_pointer()
        .role(Role::ListBoxOption)
        .aria_label(row.name.clone())
        .aria_selected(is_selected)
        .when(is_active, |this| this.aria_active_descendant())
        .when(is_selected, |this| this.bg(colors.selected))
        .when(!is_selected && is_active, |this| this.bg(colors.active))
        .hover(move |this| this.bg(colors.hover))
        .child(
            div()
                .w(px(16.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .child(if is_selected {
                    Icon::new(IconName::Check)
                        .with_size(Size::Small)
                        .text_color(colors.primary)
                        .into_any_element()
                } else {
                    div().into_any_element()
                }),
        )
        .child(
            Icon::new(match row.kind {
                PickerRowKind::Parent => IconName::ArrowUp,
                PickerRowKind::Directory => IconName::Folder,
                PickerRowKind::File => IconName::FileText,
            })
            .with_size(Size::Small)
            .text_color(if parent {
                colors.muted
            } else if row.is_dir() {
                colors.primary
            } else {
                colors.muted
            }),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .gap_1()
                .when(parent, |this| this.text_color(colors.muted))
                .child(picker_name(row, colors.foreground, colors.highlight))
                .when(row.is_symlink, |this| {
                    this.child(div().text_xs().text_color(colors.muted).child("↗"))
                }),
        )
        .child(
            div()
                .w(px(96.))
                .flex_shrink_0()
                .text_right()
                .text_xs()
                .text_color(colors.muted)
                .child(format_size(row.size)),
        )
        .child(
            div()
                .w(px(150.))
                .flex_shrink_0()
                .text_right()
                .text_xs()
                .text_color(colors.muted)
                .child(format_modified(row.modified)),
        )
        .on_click(move |event, window, cx| {
            let double_click = event.click_count() > 1;
            let modifiers = event.modifiers();
            let _ = weak_click.update(cx, |this, cx| {
                this.file_picker_click(index, modifiers, double_click, window, cx);
            });
        });

    if parent {
        return element.into_any_element();
    }
    element
        .context_menu(move |menu, _, _| {
            let mut menu = menu.item(PopupMenuItem::new(tr("dialog.open")).on_click({
                let weak = weak_menu_open.clone();
                let path = menu_path.clone();
                move |_, window, cx| {
                    let path = path.clone();
                    let _ = weak.update(cx, |this, cx| {
                        if menu_is_dir {
                            this.file_picker_navigate(path, true, window, cx);
                        } else {
                            if let Some(dialog) = &mut this.file_dialog {
                                dialog.select_only(&path);
                            }
                            this.file_dialog_confirm(window, cx);
                        }
                    });
                }
            }));
            if is_file {
                menu = menu
                    .item(PopupMenuItem::new(tr("explorer.copy_path")).on_click({
                        let weak = weak_menu_copy.clone();
                        let path = menu_path.clone();
                        move |_, _, cx| {
                            let path = path.clone();
                            let _ = weak.update(cx, |this, cx| {
                                this.copy_path(&path, cx);
                            });
                        }
                    }))
                    .item(PopupMenuItem::new(tr("explorer.reveal")).on_click({
                        let weak = weak_menu_reveal.clone();
                        let path = menu_path.clone();
                        move |_, _, cx| {
                            let path = path.clone();
                            let _ = weak.update(cx, |this, cx| {
                                this.reveal_path(&path, cx);
                            });
                        }
                    }));
            }
            menu = menu
                .separator()
                .item(PopupMenuItem::new(tr("explorer.new_folder")).on_click({
                    let weak = weak_menu_new_folder.clone();
                    move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.file_picker_open_new_folder(window, cx);
                        });
                    }
                }))
                .item(PopupMenuItem::new(tr("dialog.refresh")).on_click({
                    let weak = weak_menu_refresh.clone();
                    move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.file_picker_refresh(window, cx);
                        });
                    }
                }));
            menu
        })
        .into_any_element()
}

#[derive(Clone, Copy)]
struct PickerColors {
    foreground: gpui_kit::Hsla,
    muted: gpui_kit::Hsla,
    selected: gpui_kit::Hsla,
    active: gpui_kit::Hsla,
    hover: gpui_kit::Hsla,
    primary: gpui_kit::Hsla,
    highlight: gpui_kit::Hsla,
}

impl LubanApp {
    pub(super) fn render_file_picker_list(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(dialog) = &self.file_dialog else {
            return div().into_any_element();
        };
        let rows = dialog.rows.clone();
        let selected = dialog.selected.clone();
        let active = dialog.active;
        let sort = dialog.sort;
        let filter = dialog.filter.clone();
        let search = dialog.search.clone();
        let loading = dialog.loading;
        let error = dialog.error.clone();
        let empty = rows.len() <= 1;
        let weak = cx.weak_entity();
        let colors = PickerColors {
            foreground: cx.theme().colors.foreground,
            muted: cx.theme().colors.muted_foreground,
            selected: cx.theme().colors.list_active,
            active: cx.theme().colors.list_active,
            hover: cx.theme().colors.list_hover,
            primary: cx.theme().colors.primary,
            highlight: cx.theme().colors.selection,
        };
        let border = cx.theme().colors.border;
        let muted = cx.theme().colors.muted_foreground;
        let weak_sort_name = weak.clone();
        let weak_sort_size = weak.clone();
        let weak_sort_modified = weak.clone();
        let weak_retry = weak.clone();
        let weak_create = weak.clone();
        let weak_cancel_create = weak.clone();
        let focus = dialog.list_focus.clone();
        let scroll = dialog.list_scroll.clone();
        let base_scroll = dialog.list_base_scroll.clone();
        let new_folder = dialog.new_folder.as_ref().map(|state| state.error.clone());

        let header = div()
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .px_2()
            .gap_2()
            .border_b_1()
            .border_color(border)
            .bg(cx.theme().colors.secondary)
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(muted)
            .child(
                div()
                    .id("file-picker-sort-name")
                    .flex_1()
                    .min_w(px(0.))
                    .pl(px(48.))
                    .cursor_pointer()
                    .child(format!(
                        "{}{}",
                        tr("dialog.name_column"),
                        sort_arrow(sort.key == SortKey::Name, sort.descending)
                    ))
                    .on_click(move |_, _, cx| {
                        let _ = weak_sort_name.update(cx, |this, cx| {
                            this.file_picker_set_sort(SortKey::Name, cx);
                        });
                    }),
            )
            .child(
                div()
                    .id("file-picker-sort-size")
                    .w(px(96.))
                    .flex_shrink_0()
                    .text_right()
                    .cursor_pointer()
                    .child(format!(
                        "{}{}",
                        tr("dialog.size_column"),
                        sort_arrow(sort.key == SortKey::Size, sort.descending)
                    ))
                    .on_click(move |_, _, cx| {
                        let _ = weak_sort_size.update(cx, |this, cx| {
                            this.file_picker_set_sort(SortKey::Size, cx);
                        });
                    }),
            )
            .child(
                div()
                    .id("file-picker-sort-modified")
                    .w(px(150.))
                    .flex_shrink_0()
                    .text_right()
                    .cursor_pointer()
                    .child(format!(
                        "{}{}",
                        tr("dialog.modified_column"),
                        sort_arrow(sort.key == SortKey::Modified, sort.descending)
                    ))
                    .on_click(move |_, _, cx| {
                        let _ = weak_sort_modified.update(cx, |this, cx| {
                            this.file_picker_set_sort(SortKey::Modified, cx);
                        });
                    }),
            );

        let row_elements = rows.clone();
        let row_selected = selected.clone();
        let mut list = uniform_list(
            "file-picker-rows",
            rows.len(),
            move |range, _window, _cx| {
                range
                    .map(|index| {
                        let Some(row) = row_elements.get(index) else {
                            return div().into_any_element();
                        };
                        picker_row_element(index, row, &row_selected, active, weak.clone(), colors)
                    })
                    .collect::<Vec<_>>()
            },
        )
        .h_full()
        .w_full()
        .track_scroll(&scroll);
        if let Some(focus) = focus {
            list = list.track_focus(&focus);
        }

        let list_area = div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .child(list)
            .child(
                Scrollbar::vertical(&base_scroll)
                    .id("file-picker-scrollbar")
                    .mode(ScrollbarMode::Always),
            );

        let mut column = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .pr(px(16.))
            .child(header);

        if let Some(new_folder_error) = new_folder {
            column =
                column.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .bg(cx.theme().colors.secondary)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::Folder)
                                        .with_size(Size::Small)
                                        .text_color(cx.theme().colors.primary),
                                )
                                .child(div().flex_1().min_w(px(0.)).child(
                                    Input::new(&self.file_new_folder_input).w_full().h(px(28.)),
                                ))
                                .child(
                                    Button::new("file-picker-create-folder")
                                        .label(tr("dialog.create"))
                                        .compact()
                                        .on_click(move |_, window, cx| {
                                            let _ = weak_create.update(cx, |this, cx| {
                                                this.create_file_picker_folder(window, cx);
                                            });
                                        }),
                                )
                                .child(
                                    Button::new("file-picker-cancel-create")
                                        .label(tr("common.cancel"))
                                        .compact()
                                        .on_click(move |_, window, cx| {
                                            let _ = weak_cancel_create.update(cx, |this, cx| {
                                                this.file_picker_cancel_new_folder(window, cx);
                                            });
                                        }),
                                ),
                        )
                        .when_some(new_folder_error, |this, error| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().colors.danger)
                                    .child(error),
                            )
                        }),
                );
        }

        if let Some(error) = &error {
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().colors.danger)
                    .child(error.clone())
                    .child(
                        Button::new("file-picker-retry")
                            .label(tr("dialog.retry"))
                            .compact()
                            .on_click(move |_, window, cx| {
                                let _ = weak_retry.update(cx, |this, cx| {
                                    this.file_picker_refresh(window, cx);
                                });
                            }),
                    ),
            );
        }

        let status = if loading {
            tr("dialog.loading")
        } else if empty {
            if !search.trim().is_empty() || !filter.is_all() {
                tr("dialog.no_matches")
            } else {
                tr("dialog.empty_folder")
            }
        } else {
            let count = rows.len() - 1;
            if selected.is_empty() {
                tr_args("dialog.items_count", &[("count", count.to_string())])
            } else {
                tr_args(
                    "dialog.items_selected",
                    &[
                        ("count", count.to_string()),
                        ("selected", selected.len().to_string()),
                        (
                            "size",
                            format_size(Some(
                                rows.iter()
                                    .filter(|row| row.is_file() && selected.contains(&row.path))
                                    .filter_map(|row| row.size)
                                    .sum::<u64>(),
                            )),
                        ),
                    ],
                )
            }
        };

        column
            .child(list_area)
            .child(
                div()
                    .h(px(24.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .text_xs()
                    .text_color(muted)
                    .child(status),
            )
            .into_any_element()
    }
}

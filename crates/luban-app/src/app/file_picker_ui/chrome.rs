use super::*;

fn breadcrumb_button(
    label: String,
    path: PathBuf,
    current: bool,
    weak: gpui_kit::WeakEntity<LubanApp>,
    foreground: gpui_kit::Hsla,
    muted: gpui_kit::Hsla,
    hover: gpui_kit::Hsla,
) -> AnyElement {
    div()
        .id(format!("file-picker-crumb:{}", path.display()))
        .px_1()
        .py_1()
        .rounded_sm()
        .text_sm()
        .cursor_pointer()
        .when(current, |this| {
            this.text_color(foreground).font_weight(FontWeight::MEDIUM)
        })
        .when(!current, |this| this.text_color(muted))
        .hover(move |this| this.bg(hover).text_color(foreground))
        .child(label)
        .on_click(move |_, window, cx| {
            let path = path.clone();
            let _ = weak.update(cx, |this, cx| {
                this.file_picker_navigate(path, true, window, cx);
            });
        })
        .into_any_element()
}

impl LubanApp {
    pub(super) fn render_file_picker_title(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.file_dialog else {
            return div().into_any_element();
        };
        let title = dialog.title();
        let weak = cx.weak_entity();
        div()
            .flex()
            .items_center()
            .justify_between()
            .px_4()
            .pt_3()
            .pb_2()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                Button::new("file-picker-close")
                    .icon(IconName::X)
                    .ghost()
                    .compact()
                    .tooltip(tr("common.close"))
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.cancel_file_dialog(cx);
                            this.focus_editor(window, cx);
                        });
                    }),
            )
            .into_any_element()
    }

    pub(super) fn render_file_picker_toolbar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(dialog) = &self.file_dialog else {
            return div().into_any_element();
        };
        let can_back = dialog.can_back();
        let can_forward = dialog.can_forward();
        let can_up = dialog.parent_dir().is_some();
        let address_edit = dialog.address_edit;
        let show_hidden = dialog.show_hidden;
        let show_preview = dialog.show_preview;
        let weak = cx.weak_entity();
        let weak_back = weak.clone();
        let weak_forward = weak.clone();
        let weak_up = weak.clone();
        let weak_path = weak.clone();
        let weak_refresh = weak.clone();
        let weak_hidden = weak.clone();
        let weak_preview = weak.clone();

        let navigation = div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                Button::new("file-picker-back")
                    .icon(IconName::ArrowLeft)
                    .ghost()
                    .compact()
                    .tooltip(tr("dialog.back"))
                    .disabled(!can_back)
                    .on_click(move |_, window, cx| {
                        let _ = weak_back.update(cx, |this, cx| {
                            this.file_picker_back(window, cx);
                        });
                    }),
            )
            .child(
                Button::new("file-picker-forward")
                    .icon(IconName::ArrowRight)
                    .ghost()
                    .compact()
                    .tooltip(tr("dialog.forward"))
                    .disabled(!can_forward)
                    .on_click(move |_, window, cx| {
                        let _ = weak_forward.update(cx, |this, cx| {
                            this.file_picker_forward(window, cx);
                        });
                    }),
            )
            .child(
                Button::new("file-picker-up")
                    .icon(IconName::ArrowUp)
                    .ghost()
                    .compact()
                    .tooltip(tr("dialog.up"))
                    .disabled(!can_up)
                    .on_click(move |_, window, cx| {
                        let _ = weak_up.update(cx, |this, cx| {
                            this.file_picker_up(window, cx);
                        });
                    }),
            );

        let actions = div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                Button::new("file-picker-address")
                    .icon(IconName::Pencil)
                    .ghost()
                    .compact()
                    .tooltip(tr("dialog.edit_path"))
                    .on_click(move |_, window, cx| {
                        let _ = weak_path.update(cx, |this, cx| {
                            this.file_picker_enter_address(window, cx);
                        });
                    }),
            )
            .child(
                Button::new("file-picker-refresh")
                    .icon(IconName::RefreshCw)
                    .ghost()
                    .compact()
                    .tooltip(tr("dialog.refresh"))
                    .on_click(move |_, window, cx| {
                        let _ = weak_refresh.update(cx, |this, cx| {
                            this.file_picker_refresh(window, cx);
                        });
                    }),
            )
            .child(
                Button::new("file-picker-hidden")
                    .icon(if show_hidden {
                        IconName::Eye
                    } else {
                        IconName::EyeOff
                    })
                    .ghost()
                    .compact()
                    .tooltip(if show_hidden {
                        tr("dialog.hide_hidden")
                    } else {
                        tr("dialog.show_hidden")
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak_hidden.update(cx, |this, cx| {
                            this.file_picker_toggle_hidden(window, cx);
                        });
                    }),
            )
            .child(
                Button::new("file-picker-preview-toggle")
                    .icon(if show_preview {
                        IconName::PanelRightOpen
                    } else {
                        IconName::PanelRightClose
                    })
                    .ghost()
                    .compact()
                    .tooltip(tr("dialog.toggle_preview"))
                    .on_click(move |_, _, cx| {
                        let _ = weak_preview.update(cx, |this, cx| {
                            this.file_picker_toggle_preview(cx);
                        });
                    }),
            );

        let location = if address_edit {
            div()
                .flex_1()
                .min_w(px(0.))
                .child(Input::new(&self.file_path_input).w_full().h(px(30.)))
                .into_any_element()
        } else {
            let crumbs = self.render_file_picker_breadcrumbs(cx);
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .child(crumbs)
                .into_any_element()
        };

        let toolbar = div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .pb_2()
            .child(navigation)
            .child(location)
            .child(actions);

        let search = self.render_file_picker_search(cx);
        let custom_filter = if self
            .file_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.custom_filter)
        {
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_4()
                .pb_2()
                .child(
                    div()
                        .w(px(72.))
                        .text_xs()
                        .text_color(cx.theme().colors.muted_foreground)
                        .child(tr("dialog.filter_custom")),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(Input::new(&self.file_filter_input).w_full().h(px(28.))),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };

        let _ = window;
        div()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(search)
            .child(custom_filter)
            .into_any_element()
    }

    fn render_file_picker_breadcrumbs(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.file_dialog else {
            return div().into_any_element();
        };
        let segments = breadcrumbs(&dialog.dir);
        if segments.is_empty() {
            return div().into_any_element();
        }
        let weak = cx.weak_entity();
        let foreground = cx.theme().colors.foreground;
        let muted = cx.theme().colors.muted_foreground;
        let hover = cx.theme().colors.secondary_hover;
        let last_index = segments.len().saturating_sub(1);
        let collapsed = segments.len() > 6;
        let separator = || div().px_1().text_sm().text_color(muted).child("›");
        let push_segment = |row: Div, index: usize| -> Div {
            let (label, path) = segments[index].clone();
            row.child(breadcrumb_button(
                label,
                path,
                index == last_index,
                weak.clone(),
                foreground,
                muted,
                hover,
            ))
        };

        let mut row = div()
            .flex()
            .items_center()
            .gap_0()
            .min_w(px(0.))
            .overflow_hidden();
        if collapsed {
            row = push_segment(row, 0);
            let middle: Vec<(String, PathBuf)> = segments[1..segments.len() - 3].to_vec();
            let weak_more = weak.clone();
            row = row.child(separator()).child(
                Button::new("file-picker-crumb-more")
                    .label("…")
                    .ghost()
                    .compact()
                    .dropdown_menu(move |mut menu, _window, _cx| {
                        for (label, path) in middle.clone() {
                            let weak = weak_more.clone();
                            menu = menu.item(PopupMenuItem::new(label).on_click(
                                move |_, window, cx| {
                                    let path = path.clone();
                                    let _ = weak.update(cx, |this, cx| {
                                        this.file_picker_navigate(path, true, window, cx);
                                    });
                                },
                            ));
                        }
                        menu
                    }),
            );
            for index in segments.len() - 3..segments.len() {
                row = row.child(separator());
                row = push_segment(row, index);
            }
        } else {
            for index in 0..segments.len() {
                if index > 0 {
                    row = row.child(separator());
                }
                row = push_segment(row, index);
            }
        }
        row.into_any_element()
    }

    fn render_file_picker_search(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.file_dialog else {
            return div().into_any_element();
        };
        let presets = filter_presets(&dialog.purpose);
        let current_label = if dialog.custom_filter {
            tr("dialog.filter_custom")
        } else {
            dialog.filter.label()
        };
        let weak = cx.weak_entity();
        let weak_filter = weak.clone();
        let weak_custom = weak.clone();
        let weak_new_folder = weak.clone();
        let filter_button = Button::new("file-picker-filter")
            .label(current_label)
            .icon(IconName::ChevronDown)
            .ghost()
            .compact()
            .dropdown_menu(move |mut menu, _window, _cx| {
                for preset in presets.clone() {
                    let weak = weak_filter.clone();
                    let label = preset.label();
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        let preset = preset.clone();
                        let _ = weak.update(cx, |this, cx| {
                            this.file_picker_set_filter(preset, cx);
                        });
                    }));
                }
                let weak = weak_custom.clone();
                menu =
                    menu.separator()
                        .item(PopupMenuItem::new(tr("dialog.filter_custom")).on_click(
                            move |_, window, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.file_picker_select_custom_filter(window, cx);
                                });
                            },
                        ));
                menu
            });

        div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .pb_2()
            .child(
                Icon::new(IconName::Search)
                    .with_size(Size::Small)
                    .text_color(cx.theme().colors.muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(Input::new(&self.file_search_input).w_full().h(px(30.))),
            )
            .child(filter_button)
            .child(
                Button::new("file-picker-new-folder")
                    .label(tr("dialog.new_folder"))
                    .icon(IconName::Plus)
                    .compact()
                    .on_click(move |_, window, cx| {
                        let _ = weak_new_folder.update(cx, |this, cx| {
                            this.file_picker_open_new_folder(window, cx);
                        });
                    }),
            )
            .into_any_element()
    }

    pub(super) fn render_file_picker_footer(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.file_dialog else {
            return div().into_any_element();
        };
        let mode = dialog.mode;
        let confirm_label = dialog.confirm_label();
        let error = dialog.error.clone();
        let target_folder = dialog
            .active
            .and_then(|active| dialog.row(active))
            .filter(|row| row.is_dir())
            .map(|row| row.path.clone())
            .unwrap_or_else(|| dialog.dir.clone());
        let weak_cancel = cx.weak_entity();
        let weak_confirm = cx.weak_entity();
        let modifier = if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl"
        };
        let hint = tr_args("dialog.shortcut_hint", &[("mod", modifier.to_string())]);

        let mut footer = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(cx.theme().colors.border);

        if mode == FileDialogMode::Save {
            footer = footer.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(72.))
                            .text_sm()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(tr("dialog.file_name")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(Input::new(&self.file_name_input).w_full().h(px(30.))),
                    ),
            );
        }
        if mode == FileDialogMode::Folder {
            footer = footer.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr_args(
                        "dialog.current_folder_selected",
                        &[("path", target_folder.display().to_string())],
                    )),
            );
        }
        if let Some(error) = error {
            footer = footer.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.danger)
                    .child(error),
            );
        }
        footer
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().colors.muted_foreground)
                            .child(hint),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("file-picker-cancel")
                                    .label(tr("common.cancel"))
                                    .on_click(move |_, window, cx| {
                                        let _ = weak_cancel.update(cx, |this, cx| {
                                            this.cancel_file_dialog(cx);
                                            this.focus_editor(window, cx);
                                        });
                                    }),
                            )
                            .child(
                                Button::new("file-picker-confirm")
                                    .label(confirm_label)
                                    .primary()
                                    .on_click(move |_, window, cx| {
                                        let _ = weak_confirm.update(cx, |this, cx| {
                                            this.file_dialog_confirm(window, cx);
                                        });
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }
}

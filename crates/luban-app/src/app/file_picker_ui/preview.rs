use super::*;

fn kind_from_content(content: PreviewContent) -> PreviewKind {
    match content {
        PreviewContent::Text(text) => PreviewKind::Text(text),
        PreviewContent::Binary { size } => PreviewKind::Binary { size },
    }
}

fn preview_target(
    dialog: &FileDialogState,
) -> Option<(PathBuf, Option<u64>, Option<std::time::SystemTime>)> {
    let selected = dialog.selected_in_visible_order();
    let row = if selected.len() == 1 {
        dialog
            .row_for_path(&selected[0])
            .and_then(|index| dialog.row(index))
            .cloned()
    } else {
        dialog.active.and_then(|index| dialog.row(index)).cloned()
    }?;
    row.is_file().then_some((row.path, row.size, row.modified))
}

impl LubanApp {
    pub(crate) fn refresh_file_picker_preview(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.file_dialog else {
            return;
        };
        let selected = dialog.selected_in_visible_order();
        if selected.len() > 1 {
            dialog.preview.generation = dialog.preview.generation.wrapping_add(1);
            dialog.preview.kind = Some(PreviewKind::Multiple {
                count: selected.len(),
                total_size: dialog.selected_total_size(),
            });
            return;
        }
        let target = preview_target(dialog);
        if target.is_none() {
            let active_dir = dialog
                .active
                .and_then(|index| dialog.row(index))
                .and_then(|row| row.is_dir().then(|| (row.name.clone(), row.path.clone())));
            if let Some((name, path)) = active_dir {
                let items = util::same_path(&path, &dialog.dir)
                    .then_some(dialog.rows.len().saturating_sub(1));
                dialog.preview.generation = dialog.preview.generation.wrapping_add(1);
                dialog.preview.kind = Some(PreviewKind::Folder { name, path, items });
                return;
            }
        }
        let Some((path, size, modified)) = target else {
            dialog.preview.generation = dialog.preview.generation.wrapping_add(1);
            dialog.preview.kind = None;
            return;
        };
        if let Some(content) = dialog.preview.cached(&path, size, modified) {
            dialog.preview.generation = dialog.preview.generation.wrapping_add(1);
            dialog.preview.kind = Some(kind_from_content(content));
            return;
        }
        dialog.preview.generation = dialog.preview.generation.wrapping_add(1);
        let generation = dialog.preview.generation;
        dialog.preview.kind = Some(PreviewKind::Loading);
        let executor = cx.background_executor().clone();
        let task_path = path.clone();
        let task = cx.background_executor().spawn(async move {
            executor.timer(std::time::Duration::from_millis(100)).await;
            load_preview(&task_path, size, modified)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |app, cx| {
                let Some(dialog) = &mut app.file_dialog else {
                    return;
                };
                if dialog.preview.generation != generation {
                    return;
                }
                if preview_target(dialog)
                    .map(|(target, _, _)| target)
                    .as_deref()
                    != Some(path.as_path())
                {
                    return;
                }
                match result {
                    Ok(content) => {
                        dialog
                            .preview
                            .store(path.clone(), size, modified, content.clone());
                        dialog.preview.kind = Some(kind_from_content(content));
                    }
                    Err(error) => {
                        dialog.preview.kind = Some(PreviewKind::Error { message: error });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_file_picker_preview(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.file_dialog else {
            return div().into_any_element();
        };
        let kind = dialog.preview.kind.clone().unwrap_or(PreviewKind::None);
        let scroll = dialog.preview_scroll.clone();
        let border = cx.theme().colors.border;
        let muted = cx.theme().colors.muted_foreground;
        let foreground = cx.theme().colors.foreground;
        let danger = cx.theme().colors.danger;
        let mono_font = cx.theme().mono_font_family.clone();

        let content = match kind {
            PreviewKind::None => {
                preview_empty(IconName::FileText, tr("dialog.preview_empty"), muted)
            }
            PreviewKind::Loading => {
                preview_empty(IconName::LoaderCircle, tr("dialog.preview_loading"), muted)
            }
            PreviewKind::Folder { name, path, items } => {
                let detail = items.map_or_else(
                    || path.display().to_string(),
                    |count| {
                        tr_args(
                            "dialog.preview_folder_items",
                            &[
                                ("path", path.display().to_string()),
                                ("count", count.to_string()),
                            ],
                        )
                    },
                );
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
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
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(name)),
                    )
                    .child(div().text_xs().text_color(muted).child(detail))
                    .into_any_element()
            }
            PreviewKind::Multiple { count, total_size } => div()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(tr_args(
                            "dialog.preview_multiple",
                            &[("count", count.to_string())],
                        )),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format_size(Some(total_size))),
                )
                .into_any_element(),
            PreviewKind::Binary { size } => preview_empty(
                IconName::File,
                tr_args(
                    "dialog.preview_binary",
                    &[("size", format_size(Some(size)))],
                ),
                muted,
            ),
            PreviewKind::Error { message } => div()
                .p_3()
                .text_xs()
                .text_color(danger)
                .child(message)
                .into_any_element(),
            PreviewKind::Text(text) => {
                let truncated = text.truncated;
                let mut lines = div()
                    .flex()
                    .flex_col()
                    .py_2()
                    .pr_2()
                    .id("file-picker-preview-scroll")
                    .track_scroll(&scroll);
                for (index, line) in text.lines.iter().enumerate() {
                    lines = lines.child(
                        div()
                            .flex()
                            .gap_2()
                            .px_2()
                            .text_xs()
                            .font_family(mono_font.clone())
                            .child(
                                div()
                                    .w(px(30.))
                                    .flex_shrink_0()
                                    .text_right()
                                    .text_color(muted)
                                    .child((index + 1).to_string()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(foreground)
                                    .child(line.clone()),
                            ),
                    );
                }
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(border)
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .truncate()
                                    .child(text.name.clone()),
                            )
                            .child(div().text_xs().text_color(muted).child(format!(
                                "{} · {}",
                                format_size(Some(text.size)),
                                text.language
                            )))
                            .child(div().text_xs().text_color(muted).child(format!(
                                "{} · {} · {}",
                                text.encoding,
                                text.eol,
                                format_modified(text.modified)
                            ))),
                    )
                    .child(
                        div()
                            .id("file-picker-preview-body")
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scroll()
                            .child(lines),
                    )
                    .when(truncated, |this| {
                        this.child(
                            div()
                                .px_3()
                                .py_1()
                                .text_xs()
                                .text_color(muted)
                                .child(tr("dialog.preview_truncated")),
                        )
                    })
                    .into_any_element()
            }
        };

        div()
            .w(px(288.))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(border)
            .bg(cx.theme().colors.background)
            .child(
                div()
                    .h(px(32.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .px_3()
                    .border_b_1()
                    .border_color(border)
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(muted)
                    .child(tr("dialog.preview")),
            )
            .child(content)
            .into_any_element()
    }
}

fn preview_empty(icon: IconName, text: String, muted: gpui_kit::Hsla) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .items_center()
        .justify_center()
        .gap_2()
        .p_4()
        .child(Icon::new(icon).with_size(Size::Large).text_color(muted))
        .child(div().text_xs().text_color(muted).text_center().child(text))
        .into_any_element()
}

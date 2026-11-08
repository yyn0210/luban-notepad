use super::*;

fn place_icon(icon: PlaceIcon) -> IconName {
    match icon {
        PlaceIcon::Home => IconName::House,
        PlaceIcon::Workspace => IconName::FolderOpen,
        PlaceIcon::Desktop => IconName::Monitor,
        PlaceIcon::Documents => IconName::FileText,
        PlaceIcon::Downloads => IconName::Download,
        PlaceIcon::Recent => IconName::Clock,
        PlaceIcon::Drive => IconName::HardDrive,
        PlaceIcon::Folder => IconName::Folder,
    }
}

impl LubanApp {
    pub(super) fn render_file_picker_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.file_dialog else {
            return div().into_any_element();
        };
        let sections = dialog.places.clone();
        let current = dialog.dir.clone();
        let weak = cx.weak_entity();
        let mut column = div()
            .w(px(176.))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.background)
            .id("file-picker-places")
            .overflow_y_scroll()
            .py_2();

        for section in sections {
            if section.places.is_empty() {
                continue;
            }
            column = column.child(
                div()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(section.title),
            );
            for place in section.places {
                let active = util::same_path(&place.path, &current)
                    || (place.icon == PlaceIcon::Workspace && current.starts_with(&place.path));
                let weak_place = weak.clone();
                let path = place.path.clone();
                column = column.child(
                    div()
                        .id(format!("file-picker-place:{}", path.display()))
                        .mx_1()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_sm()
                        .cursor_pointer()
                        .role(Role::Button)
                        .aria_label(place.label.clone())
                        .when(active, |this| this.bg(cx.theme().colors.secondary))
                        .hover(|this| this.bg(cx.theme().colors.secondary_hover))
                        .child(
                            Icon::new(place_icon(place.icon))
                                .with_size(Size::Small)
                                .text_color(cx.theme().colors.muted_foreground),
                        )
                        .child(div().min_w(px(0.)).truncate().child(place.label))
                        .on_click(move |_, window, cx| {
                            let path = path.clone();
                            let _ = weak_place.update(cx, |this, cx| {
                                this.file_picker_navigate(path, true, window, cx);
                            });
                        }),
                );
            }
        }

        column.into_any_element()
    }
}

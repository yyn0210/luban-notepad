//! File-global line-number gutter for disk-engine documents.
//!
//! The editor's built-in gutter always numbers the mounted window from 1 and
//! sizes its column from the window's own line count, so it cannot show the
//! eight-digit global lines of a huge file. Disk-engine tabs hide it and use
//! this sibling column instead: the numbers come from the window's global
//! first line plus the row offset, and the column is sized for the document's
//! line count. Range views keep their range-local numbering because
//! `window_line` is already range-local there.

use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::*;

use crate::huge_file::{scroll_row_clip, top_line_from_scroll};

/// Digits the gutter reserves. Matches the editor's three-digit minimum and
/// grows with the document's line count.
pub fn line_number_digits(total_lines: u64) -> usize {
    total_lines.to_string().len().max(3)
}

/// 1-based line numbers visible in the mounted window.
///
/// Starts at the viewport top and covers the visible rows plus the partially
/// visible row after them, clamped to the window's last line.
pub fn visible_lines(
    top_line: u64,
    window_line: u64,
    window_newlines: u64,
    viewport_lines: u64,
) -> std::ops::RangeInclusive<u64> {
    let first = top_line.max(window_line);
    let window_end = window_line.saturating_add(window_newlines);
    let last = first
        .saturating_add(viewport_lines.max(1))
        .min(window_end)
        .max(first);
    first..=last
}

/// Everything the gutter needs for one render.
pub struct HugeGutterProps<'a> {
    /// Editor scroll offset; negative when scrolled down.
    pub offset_y: f32,
    pub line_height: f32,
    /// The editor's inner top padding, so the first row lines up with the
    /// text area rather than the editor frame.
    pub top_padding: f32,
    pub viewport_lines: u64,
    /// Global (or range-local) line of the window's first byte.
    pub window_line: u64,
    /// Line breaks inside the mounted window.
    pub window_newlines: u64,
    /// Document total, used for the column width.
    pub total_lines: u64,
    pub font_family: &'a str,
    pub font_size: f32,
}

/// Render the gutter column. The caller places it to the left of the editor
/// and only when the setting is enabled.
pub fn render(props: HugeGutterProps<'_>, window: &mut Window, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let top_line = top_line_from_scroll(
        props.offset_y,
        props.line_height,
        props.window_line,
        props.window_newlines,
    );
    let clip = scroll_row_clip(props.offset_y, props.line_height);
    let digits = line_number_digits(props.total_lines);
    let family: SharedString = if props.font_family.is_empty() {
        theme.mono_font_family.clone()
    } else {
        props.font_family.into()
    };
    let font = font(family.clone());
    let font_id = window.text_system().resolve_font(&font);
    let digit_width = window
        .text_system()
        .layout_width(font_id, px(props.font_size), '0');
    let width = digit_width * digits as f32 + px(12.);
    let lines = visible_lines(
        top_line,
        props.window_line,
        props.window_newlines,
        props.viewport_lines,
    );
    let rows = lines.end().saturating_sub(*lines.start()) + 1;
    let elements = lines.map(|line| {
        div()
            .flex()
            .items_center()
            .justify_end()
            .h(px(props.line_height))
            .pr(px(6.))
            .child(line.to_string())
    });

    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .h_full()
        .w(width)
        .pt(px(props.top_padding))
        .pb(px(props.top_padding))
        .overflow_hidden()
        .bg(theme.colors.background)
        .child(
            div()
                .relative()
                .flex_1()
                .min_h(px(0.))
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top(px(-clip))
                        .left_0()
                        .right_0()
                        .h(px(rows as f32 * props.line_height))
                        .flex()
                        .flex_col()
                        .text_size(px(props.font_size))
                        .font_family(family)
                        .text_color(theme.colors.muted_foreground)
                        .children(elements),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{line_number_digits, visible_lines};

    #[test]
    fn digit_count_follows_the_document() {
        assert_eq!(line_number_digits(0), 3);
        assert_eq!(line_number_digits(1), 3);
        assert_eq!(line_number_digits(999), 3);
        assert_eq!(line_number_digits(1_000), 4);
        assert_eq!(line_number_digits(12_549_113), 8);
    }

    #[test]
    fn visible_lines_start_at_the_viewport_and_stop_at_the_window_end() {
        // A 37-row viewport shows rows 601..=638 of a window starting at 1,
        // with the row after them partly visible.
        assert_eq!(visible_lines(601, 1, 800, 37), 601..=638);
        // Never past the last line of the window.
        assert_eq!(visible_lines(790, 1, 800, 37), 790..=801);
        // A range view keeps its range-local numbering.
        assert_eq!(visible_lines(1, 1, 40, 37), 1..=38);
        assert_eq!(visible_lines(0, 5, 40, 0), 5..=6);
    }
}

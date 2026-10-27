//! File-global vertical scrollbar for disk-engine documents.
//!
//! The editor mounts only a bounded window of a huge file, so its own
//! scrollbar only spans that window. This scrollbar maps the thumb to the
//! whole document instead: dragging or clicking the track targets a global
//! line, and the window loader moves the editor there. The shareable cell
//! state lets the rendered handle publish drag requests to the app without
//! needing an [`App`] reference from [`ScrollbarHandle`].

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::scroll::{Scrollbar, ScrollbarHandle, ScrollbarMode};
use gpui_kit::*;
use luban_core::disk_engine::WINDOW_BYTES;

use crate::huge_file::PREFETCH_LINES;

/// Shared scrollbar state between the rendered handle and the app.
#[derive(Clone, Debug, Default)]
pub struct HugeScrollShared {
    /// Latest global top line requested by a drag or track click.
    request: Rc<Cell<Option<u64>>>,
    /// Whether the user is currently holding the thumb.
    dragging: Rc<Cell<bool>>,
}

impl HugeScrollShared {
    pub fn take_request(&self) -> Option<u64> {
        self.request.take()
    }

    pub fn request(&self, line: u64) {
        self.request.set(Some(line));
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging.get()
    }
}

/// Number of distinct top-line positions the scrollbar can reach.
pub fn viewport_span(total_lines: u64, viewport_lines: u64) -> u64 {
    total_lines.saturating_sub(viewport_lines)
}

/// Convert a normalized scrollbar position to a 1-based global top line.
pub fn line_for_fraction(fraction: f32, total_lines: u64, viewport_lines: u64) -> u64 {
    let span = viewport_span(total_lines, viewport_lines);
    1 + (f64::from(fraction.clamp(0., 1.)) * span as f64).round() as u64
}

/// Convert a 1-based global top line to a normalized scrollbar position.
pub fn fraction_for_line(line: u64, total_lines: u64, viewport_lines: u64) -> f32 {
    let span = viewport_span(total_lines, viewport_lines);
    if span == 0 {
        0.
    } else {
        (line.saturating_sub(1).min(span) as f32) / span as f32
    }
}

/// Whether a line-based whole-file scrollbar has useful resolution.
///
/// Disk-engine documents are far larger than the whole-file editing limit, so
/// a document with only a couple of screens of lines consists of extremely
/// long lines (minified data, one-log-line files). Those keep the editor's own
/// byte-range scrollbar, which can move inside a line; a global line scrollbar
/// would only offer a handful of meaningless positions.
///
/// A line jump also anchors its window read [`PREFETCH_LINES`] above the
/// target. If the average line is long enough that those lines do not fit in
/// [`WINDOW_BYTES`], the target can fall outside the loaded window and the
/// scrollbar could never settle on it. Such long-line documents keep the
/// windowed byte scrolling as well.
pub fn line_scrollbar_is_meaningful(
    total_lines: u64,
    viewport_lines: u64,
    average_line_bytes: u64,
) -> bool {
    if viewport_lines == 0 {
        return false;
    }
    total_lines > viewport_lines.saturating_mul(2)
        && average_line_bytes <= MAX_SCROLLBAR_AVERAGE_LINE_BYTES
}

/// Largest average line length for which the scrollback anchor of a line jump
/// still fits in one window. Half a window leaves room for line-length
/// variance, so the target stays inside the loaded window.
pub const MAX_SCROLLBAR_AVERAGE_LINE_BYTES: u64 = WINDOW_BYTES / (PREFETCH_LINES * 2);

/// Everything the global scrollbar needs for one render.
#[derive(Clone)]
pub struct HugeScrollbarProps {
    /// Global line currently shown at the top, or the line a drag targets.
    pub top_line: u64,
    pub total_lines: u64,
    /// Lines the editor viewport can show; part of the thumb size.
    pub viewport_lines: u64,
    pub line_height: f32,
    /// The editor's viewport in window coordinates.
    pub viewport: Bounds<Pixels>,
    pub shared: HugeScrollShared,
}

#[derive(Clone)]
struct HugeScrollHandle {
    props: HugeScrollbarProps,
}

impl HugeScrollHandle {
    fn span_px(&self) -> f32 {
        viewport_span(self.props.total_lines, self.props.viewport_lines) as f32
            * self.props.line_height
    }
}

impl ScrollbarHandle for HugeScrollHandle {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.props.viewport
    }

    fn offset(&self) -> Point<Pixels> {
        let fraction = fraction_for_line(
            self.props.top_line,
            self.props.total_lines,
            self.props.viewport_lines,
        );
        point(px(0.), px(-(fraction * self.span_px())))
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        let span = self.span_px();
        if span <= 0. {
            return;
        }
        let fraction = (-offset.y.as_f32() / span).clamp(0., 1.);
        let line = line_for_fraction(fraction, self.props.total_lines, self.props.viewport_lines);
        self.props.shared.request(line);
    }

    fn content_size(&self) -> Size<Pixels> {
        size(
            px(1.),
            px(self.props.total_lines as f32 * self.props.line_height),
        )
    }

    fn start_drag(&self) {
        self.props.shared.dragging.set(true);
    }

    fn end_drag(&self) {
        self.props.shared.dragging.set(false);
    }
}

/// Render the overlay scrollbar. The caller must place it in a positioned
/// container above the editor so its mouse handlers take precedence over the
/// editor's window-local scrollbar.
pub fn render(props: HugeScrollbarProps, background: Hsla) -> AnyElement {
    let handle = HugeScrollHandle {
        props: props.clone(),
    };
    let scroll_size = size(px(1.), px(props.total_lines as f32 * props.line_height));

    div()
        .absolute()
        .inset_0()
        .child(
            Scrollbar::vertical(&handle)
                .id("huge-file-scrollbar")
                .mode(ScrollbarMode::Always)
                .viewport_bounds(props.viewport)
                .scroll_size(scroll_size)
                .styles(|styles| {
                    styles
                        .track(|track| track.bg(background))
                        .track_hover(|track| track.bg(background))
                        .track_active(|track| track.bg(background))
                }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    // The crate's `gpui_kit::*` glob re-exports GPUI's `test` attribute, which
    // shadows the built-in one. Import only the functions under test.
    use super::{
        fraction_for_line, line_for_fraction, line_scrollbar_is_meaningful,
        MAX_SCROLLBAR_AVERAGE_LINE_BYTES,
    };

    #[test]
    fn global_scrollbar_needs_more_than_two_screens_of_lines() {
        // The whole document fits, or nearly fits, in the viewport: only a
        // handful of extremely long lines. Keep the editor's byte scrollbar.
        assert!(!line_scrollbar_is_meaningful(10, 50, 100));
        assert!(!line_scrollbar_is_meaningful(80, 40, 100));
        // Not laid out yet.
        assert!(!line_scrollbar_is_meaningful(1_000_000, 0, 100));
        // A normal huge log has far more lines than the viewport.
        assert!(line_scrollbar_is_meaningful(81, 40, 100));
        assert!(line_scrollbar_is_meaningful(12_549_113, 37, 238));
    }

    #[test]
    fn long_lines_keep_the_editors_windowed_scrollbar() {
        // 80 lines pass the two-screen check but average 16 MiB per line, so
        // the 600-line scrollback anchor cannot fit in a 2 MiB window.
        assert!(!line_scrollbar_is_meaningful(80, 37, 16 * 1024 * 1024));
        assert!(!line_scrollbar_is_meaningful(100_000, 37, 12_000));
        assert!(line_scrollbar_is_meaningful(
            100_000,
            37,
            MAX_SCROLLBAR_AVERAGE_LINE_BYTES
        ));
        assert!(!line_scrollbar_is_meaningful(
            100_000,
            37,
            MAX_SCROLLBAR_AVERAGE_LINE_BYTES + 1
        ));
    }

    #[test]
    fn fractions_map_to_the_whole_file() {
        assert_eq!(line_for_fraction(0.0, 10_000, 50), 1);
        assert_eq!(line_for_fraction(1.0, 10_000, 50), 9_951);
        assert_eq!(line_for_fraction(0.5, 10_000, 50), 4_976);
    }

    #[test]
    fn line_and_fraction_round_trip() {
        for (total, viewport) in [(10_000u64, 50u64), (1_000_000, 60), (200, 40)] {
            for step in 0..=100 {
                let fraction = step as f32 / 100.;
                let line = line_for_fraction(fraction, total, viewport);
                let back = fraction_for_line(line, total, viewport);
                assert_eq!(
                    line,
                    line_for_fraction(back, total, viewport),
                    "total={total} viewport={viewport} step={step}"
                );
            }
        }
    }

    #[test]
    fn files_that_fit_the_viewport_have_one_position() {
        assert_eq!(line_for_fraction(0.0, 10, 50), 1);
        assert_eq!(line_for_fraction(1.0, 10, 50), 1);
        assert_eq!(fraction_for_line(5, 10, 50), 0.0);
    }

    #[test]
    fn fractions_are_clamped() {
        assert_eq!(line_for_fraction(-1.0, 100, 10), 1);
        assert_eq!(line_for_fraction(2.0, 100, 10), 91);
        assert_eq!(fraction_for_line(0, 100, 10), 0.0);
        assert_eq!(fraction_for_line(10_000, 100, 10), 1.0);
    }
}

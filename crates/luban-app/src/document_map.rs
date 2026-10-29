//! Document-map sampling and viewport geometry.

/// Maximum number of sampled rows.
pub const MAX_MAP_ROWS: usize = 4_000;
/// The map is hidden below this pixel height.
pub const MIN_MAP_HEIGHT: f32 = 40.0;
/// The map is hidden below this line count.
pub const MIN_MAP_LINES: usize = 20;
/// Rendered width of the map panel.
pub const MAP_WIDTH: f32 = 90.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapRow {
    /// 1-based line number.
    pub line: usize,
    /// Relative width in `0.0..=1.0`.
    pub width: f32,
}

/// Sample up to `max_rows` lines, preserving the first and last lines and
/// evenly spacing the rest. Line widths are normalised to the longest sampled
/// line, so short and long files both produce a readable silhouette.
pub fn sample(text: &str, max_rows: usize) -> Vec<MapRow> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let max_rows = max_rows.clamp(1, MAX_MAP_ROWS);
    let mut rows: Vec<MapRow> = Vec::new();
    if lines.len() <= max_rows {
        for (index, line) in lines.iter().enumerate() {
            rows.push(MapRow {
                line: index + 1,
                width: 0.0,
            });
            let row = rows.last_mut().unwrap();
            row.width = line.chars().count() as f32;
        }
    } else {
        let step = lines.len() as f64 / max_rows as f64;
        for index in 0..max_rows {
            let line_index = ((index as f64) * step).floor() as usize;
            let line_index = line_index.min(lines.len() - 1);
            rows.push(MapRow {
                line: line_index + 1,
                width: 0.0,
            });
            let row = rows.last_mut().unwrap();
            row.width = lines[line_index].chars().count() as f32;
        }
        if let Some(last) = rows.last_mut() {
            last.line = lines.len();
            last.width = lines[lines.len() - 1].chars().count() as f32;
        }
    }
    let longest = rows
        .iter()
        .map(|row| row.width)
        .fold(0.0f32, f32::max)
        .max(1.0);
    for row in &mut rows {
        row.width = (row.width / longest).clamp(0.02, 1.0);
    }
    rows
}

/// Normalised viewport rectangle `(top, height)` in `0.0..=1.0` for a
/// document of `total_lines` showing `visible_lines` starting at
/// `start_line` (both 1-based-ish, `start_line` may be 0).
pub fn viewport_rect(total_lines: usize, start_line: f32, visible_lines: f32) -> (f32, f32) {
    let total = total_lines.max(1) as f32;
    let top = (start_line / total).clamp(0.0, 0.99);
    let height = (visible_lines / total).clamp(0.01, (1.0 - top).max(0.01));
    (top, height)
}

/// Whether the map is meaningful for the current viewport.
pub fn should_show(total_lines: usize, viewport_height: f32) -> bool {
    total_lines >= MIN_MAP_LINES && viewport_height >= MIN_MAP_HEIGHT
}

/// Convert a click position inside the map (`0.0..=1.0`) to a 1-based line.
pub fn line_at_ratio(ratio: f32, total_lines: usize) -> usize {
    let total = total_lines.max(1);
    let line = (ratio.clamp(0.0, 1.0) * total as f32).floor() as usize + 1;
    line.clamp(1, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_every_line_for_small_documents() {
        let text = "a\nbb\nccc\n";
        let rows = sample(text, MAX_MAP_ROWS);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].line, 3);
        assert!((rows[2].width - 1.0).abs() < f32::EPSILON);
        assert!(rows[0].width < rows[1].width);
    }

    #[test]
    fn caps_sampling_and_keeps_the_last_line() {
        let text = (1..=100)
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let rows = sample(&text, 10);
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0].line, 1);
        assert_eq!(rows.last().unwrap().line, 100);
    }

    #[test]
    fn viewport_rect_is_normalised() {
        let (top, height) = viewport_rect(100, 10.0, 20.0);
        assert!((top - 0.1).abs() < 0.001);
        assert!((height - 0.2).abs() < 0.001);
        let (top, height) = viewport_rect(10, 9.0, 100.0);
        assert!(top <= 1.0);
        assert!(top + height <= 1.0);
    }

    #[test]
    fn map_visibility_thresholds() {
        assert!(!should_show(19, 100.0));
        assert!(should_show(20, 100.0));
        assert!(!should_show(100, 39.0));
        assert!(should_show(100, 40.0));
    }

    #[test]
    fn click_ratios_map_to_lines() {
        assert_eq!(line_at_ratio(0.0, 100), 1);
        assert_eq!(line_at_ratio(0.5, 100), 51);
        assert_eq!(line_at_ratio(1.0, 100), 100);
    }

    #[test]
    fn empty_text_has_no_rows() {
        assert!(sample("", MAX_MAP_ROWS).is_empty());
    }
}

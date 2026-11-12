//! Experimental fixed-cell shaping, enabled only by the nondefault `grid` feature.
//! GPUI's ordinary UI layout keeps its own bidi policy.
use super::{font_covers, FontCatalog, LoadedFont};
use gpui::{point, Font, FontId, GlyphId, LineLayout, Pixels, ShapedGlyph, ShapedRun};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug)]
pub struct GridTextCell {
    pub byte_index: u32,
    pub column: u16,
    pub width: u8,
}

pub struct GridLayoutRequest<'a> {
    pub text: &'a str,
    pub cells: &'a [GridTextCell],
    pub runs: &'a [gpui::TextRun],
    /// None preserves every paint-run boundary. Some supplies the authoritative
    /// VT shaping boundaries, allowing equal faces to join across paint runs.
    pub shape_breaks: Option<&'a [u32]>,
    /// Primary grid ascent/descent; their sum is the grid's cell height.
    pub line_metrics: Option<(gpui::Pixels, gpui::Pixels)>,
    pub base_font: &'a Font,
    pub font_size: Pixels,
    pub cell_width: Pixels,
    pub scale_factor: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridLayoutError {
    InvalidGeometry,
    FontUnavailable,
}
impl std::fmt::Display for GridLayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidGeometry => "invalid grid text geometry",
            Self::FontUnavailable => "grid text font unavailable",
        })
    }
}
impl std::error::Error for GridLayoutError {}

impl GridLayoutRequest<'_> {
    fn validate(&self) -> std::result::Result<(), GridLayoutError> {
        if !self.scale_factor.is_finite()
            || self.scale_factor <= 0.
            || self.line_metrics.is_some_and(|(ascent, descent)| {
                !ascent.as_f32().is_finite()
                    || !descent.as_f32().is_finite()
                    || !(ascent + descent).as_f32().is_finite()
                    || ascent + descent <= gpui::px(0.)
            })
            || !self.font_size.as_f32().is_finite()
            || self.font_size <= gpui::px(0.)
            || !self.cell_width.as_f32().is_finite()
            || self.cell_width <= gpui::px(0.)
            || !(self.font_size.as_f32() * self.scale_factor).is_finite()
            || !self.cells.last().is_none_or(|cell| {
                (self.cell_width.as_f32()
                    * (u32::from(cell.column) + u32::from(cell.width)) as f32
                    * self.scale_factor)
                    .is_finite()
            })
            || self.text.len() > u32::MAX as usize
            || self.text.is_empty() != self.cells.is_empty()
            || self.cells.first().is_some_and(|cell| cell.byte_index != 0)
        {
            return Err(GridLayoutError::InvalidGeometry);
        }
        for (index, cell) in self.cells.iter().enumerate() {
            if cell.byte_index as usize >= self.text.len()
                || !self.text.is_char_boundary(cell.byte_index as usize)
                || cell.width > 2
                || (index > 0
                    && (self.cells[index - 1].byte_index >= cell.byte_index
                        || u32::from(self.cells[index - 1].column)
                            + u32::from(self.cells[index - 1].width)
                            != u32::from(cell.column)))
            {
                return Err(GridLayoutError::InvalidGeometry);
            }
        }
        let mut offset = 0usize;
        for run in self.runs {
            offset = offset
                .checked_add(run.len)
                .ok_or(GridLayoutError::InvalidGeometry)?;
            if offset > self.text.len()
                || !self.text.is_char_boundary(offset)
                || (offset != self.text.len()
                    && self
                        .cells
                        .binary_search_by_key(&(offset as u32), |cell| cell.byte_index)
                        .is_err())
            {
                return Err(GridLayoutError::InvalidGeometry);
            }
        }
        if offset != self.text.len() {
            return Err(GridLayoutError::InvalidGeometry);
        }
        if let Some(breaks) = self.shape_breaks {
            for (index, &byte) in breaks.iter().enumerate() {
                if byte == 0
                    || byte as usize >= self.text.len()
                    || (index > 0 && breaks[index - 1] >= byte)
                    || self
                        .cells
                        .binary_search_by_key(&byte, |cell| cell.byte_index)
                        .is_err()
                {
                    return Err(GridLayoutError::InvalidGeometry);
                }
            }
        }
        Ok(())
    }
}

impl FontCatalog {
    /// LTR, character-level clusters and VT-cell positioning, with no word
    /// segmentation. Does not change the platform's normal UI shaping policy.
    pub fn layout_grid(
        &self,
        request: &GridLayoutRequest<'_>,
    ) -> std::result::Result<LineLayout, GridLayoutError> {
        request.validate()?;
        let mut locked = self.0 .0.write();
        let state = &mut *locked;
        let base = state
            .resolve_font(request.base_font)
            .map_err(|_| GridLayoutError::FontUnavailable)?;
        let mut runs: Vec<GridFontRun> = Vec::with_capacity(request.runs.len());
        let breaks = request.shape_breaks.unwrap_or(&[]);
        let mut byte = 0;
        for run in request.runs {
            let visible = run.color.a > 0.;
            let font_id = state
                .resolve_font(&run.font)
                .map_err(|_| GridLayoutError::FontUnavailable)?;
            let end = byte + run.len;
            let start = breaks.partition_point(|&boundary| boundary as usize <= byte);
            for next in breaks[start..]
                .iter()
                .copied()
                .map(|value| value as usize)
                .take_while(|&value| value < end)
                .chain(std::iter::once(end))
            {
                if next > byte {
                    let joins = request.shape_breaks.is_some()
                        && breaks.binary_search(&(byte as u32)).is_err();
                    if let Some(last) = runs
                        .last_mut()
                        .filter(|last| joins && last.font_id == font_id && last.visible == visible)
                    {
                        last.len += next - byte;
                    } else {
                        runs.push(GridFontRun {
                            len: next - byte,
                            font_id,
                            visible,
                        });
                    }
                }
                byte = next;
            }
        }
        let mut layout = state.grid_shaper.layout(
            request,
            GridFonts {
                base,
                fonts: &state.loaded_fonts,
                runs: &runs,
            },
        );
        for run in &mut layout.runs {
            for glyph in &mut run.glyphs {
                glyph.is_emoji =
                    state.glyph_is_emoji(run.font_id, glyph.id.0 as u16, request.font_size);
            }
        }
        Ok(layout)
    }
}

struct GridFonts<'a> {
    base: FontId,
    fonts: &'a [LoadedFont],
    runs: &'a [GridFontRun],
}

struct GridFontRun {
    len: usize,
    font_id: FontId,
    visible: bool,
}

struct Plan {
    font: FontId,
    script: harfrust::Script,
    language: Option<harfrust::Language>,
    shape: harfrust::ShapePlan,
}

#[derive(Default)]
pub(super) struct GridShaper {
    buffer: Option<harfrust::UnicodeBuffer>,
    plans: VecDeque<Plan>,
    features: Vec<harfrust::Feature>,
    codepoints: Vec<Codepoint>,
}

struct Codepoint {
    cell_byte: usize,
    column: u16,
}

impl GridShaper {
    fn layout(&mut self, request: &GridLayoutRequest<'_>, fonts: GridFonts<'_>) -> LineLayout {
        let base = &fonts.fonts[fonts.base.0];
        let metrics = base.font.as_swash().metrics(&base.coordinates);
        let metric_scale = request.font_size.as_f32() / f32::from(metrics.units_per_em);
        let (ascent, descent) = request.line_metrics.unwrap_or((
            gpui::px(metrics.ascent * metric_scale),
            gpui::px(metrics.descent * metric_scale),
        ));
        let columns = request
            .cells
            .last()
            .map_or(0, |cell| usize::from(cell.column) + usize::from(cell.width));
        let mut result = LineLayout {
            font_size: request.font_size,
            width: request.cell_width * columns as f32,
            ascent,
            descent,
            runs: Vec::new(),
            len: request.text.len(),
        };
        let (mut byte, mut cell_index) = (0, 0);
        for run in fonts.runs {
            let end = byte + run.len;
            if byte == end {
                continue;
            }
            if !run.visible {
                while cell_index < request.cells.len()
                    && (request.cells[cell_index].byte_index as usize) < end
                {
                    cell_index += 1;
                }
                byte = end;
                continue;
            }
            let font = &fonts.fonts[run.font_id.0];
            let variable_shaper = font.variable_shaper.as_ref().map(|v| v.shaper(&font.font));
            let shaper = variable_shaper
                .as_ref()
                .unwrap_or_else(|| font.font.shaper());
            let map = font.font.as_swash().charmap();
            let mut buffer = self.buffer.take().unwrap_or_default();
            buffer.set_direction(harfrust::Direction::LeftToRight);
            buffer.set_cluster_level(harfrust::BufferClusterLevel::Characters);
            self.codepoints.clear();
            let origin = request.cells[cell_index].column;
            while cell_index < request.cells.len()
                && (request.cells[cell_index].byte_index as usize) < end
            {
                let cell = request.cells[cell_index];
                let cell_end = request
                    .cells
                    .get(cell_index + 1)
                    .map_or(request.text.len(), |next| next.byte_index as usize);
                let text = &request.text[cell.byte_index as usize..cell_end];
                if font_covers(&font.font, text) {
                    for (index, ch) in text.char_indices() {
                        // run.zig uses presentation selectors for face choice,
                        // then omits them from the shaper's codepoint stream.
                        if index != 0 && matches!(ch, '\u{fe0e}' | '\u{fe0f}') {
                            continue;
                        }
                        buffer.add(ch, self.codepoints.len() as u32);
                        self.codepoints.push(Codepoint {
                            cell_byte: cell.byte_index as usize,
                            column: cell.column - origin,
                        });
                    }
                } else {
                    let replacement = if map.map('\u{fffd}') != 0 {
                        '\u{fffd}'
                    } else {
                        ' '
                    };
                    buffer.add(replacement, self.codepoints.len() as u32);
                    self.codepoints.push(Codepoint {
                        cell_byte: cell.byte_index as usize,
                        column: cell.column - origin,
                    });
                }
                cell_index += 1;
            }
            buffer.guess_segment_properties();
            self.features.clear();
            self.features
                .extend(font.features.features.iter().map(|feature| {
                    harfrust::Feature::new(
                        harfrust::Tag::new(feature.tag.as_bytes()),
                        feature.value,
                        0..usize::MAX,
                    )
                }));
            let script = buffer.script();
            let language = buffer.language();
            // A FontId fixes the face, variable coordinates and feature set.
            // Direction and cluster policy are constant for this shaper.
            let plan_index = self
                .plans
                .iter()
                .position(|plan| {
                    plan.font == run.font_id && plan.script == script && plan.language == language
                })
                .unwrap_or_else(|| {
                    if self.plans.len() == 64 {
                        self.plans.pop_front();
                    }
                    self.plans.push_back(Plan {
                        font: run.font_id,
                        script,
                        language: language.clone(),
                        shape: harfrust::ShapePlan::new(
                            shaper,
                            harfrust::Direction::LeftToRight,
                            Some(script),
                            language.as_ref(),
                            &self.features,
                        ),
                    });
                    self.plans.len() - 1
                });
            let shaped =
                shaper.shape_with_plan(&self.plans[plan_index].shape, buffer, &self.features);
            let scale = request.font_size.as_f32()
                / f32::from(font.font.as_swash().metrics(&font.coordinates).units_per_em);
            let rounded = |value: i32| {
                // The official shaper rounds each 26.6 value to a device pixel.
                ((value as f32 * scale * request.scale_factor + 0.5).floor()) / request.scale_factor
            };
            let (mut x, mut y, mut max_column) = (0., 0., 0u16);
            let (mut anchor_column, mut anchor_x) = (0u16, 0.);
            let mut anchor_byte = byte;
            let mut glyphs = Vec::with_capacity(shaped.len());
            for (info, pos) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
                let index = info.cluster as usize;
                let cp = &self.codepoints[index];
                if anchor_column != cp.column {
                    let first = index == 0 || self.codepoints[index - 1].column != cp.column;
                    if first && cp.column > max_column {
                        anchor_column = cp.column;
                        anchor_x = x;
                        anchor_byte = cp.cell_byte;
                    }
                }
                glyphs.push(ShapedGlyph {
                    id: GlyphId(info.glyph_id),
                    position: point(
                        request.cell_width * f32::from(origin + anchor_column)
                            + gpui::px(x - anchor_x + rounded(pos.x_offset)),
                        gpui::px(-y - rounded(pos.y_offset)),
                    ),
                    // The renderer colors/constrains by Cell.x, not the
                    // original HB codepoint. Keep GPUI's paint index on that
                    // same VT anchor, including reordered marking glyphs.
                    index: anchor_byte,
                    is_emoji: false,
                });
                x += rounded(pos.x_advance);
                y += rounded(pos.y_advance);
                max_column = max_column.max(cp.column);
            }
            self.buffer = Some(shaped.clear());
            result.runs.push(ShapedRun {
                font_id: run.font_id,
                glyphs,
            });
            byte = end;
        }
        result
    }
}

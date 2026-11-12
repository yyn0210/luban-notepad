use gpui::{BorderStyle, Quad};

/// Compile away unused fragment work without changing the general shader's
/// clipping, blending, rounded corners, gradients, or dashed borders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuadPipeline {
    General,
    Solid,
    Rect,
}

impl QuadPipeline {
    pub(crate) fn for_quad(quad: &Quad) -> Self {
        if quad.background.as_solid().is_none() || quad.border_style != BorderStyle::Solid {
            return Self::General;
        }
        let radii = &quad.corner_radii;
        let borders = &quad.border_widths;
        if [
            radii.top_left.0,
            radii.top_right.0,
            radii.bottom_right.0,
            radii.bottom_left.0,
            borders.top.0,
            borders.right.0,
            borders.bottom.0,
            borders.left.0,
        ]
        .into_iter()
        .all(|value| value == 0.0)
        {
            Self::Rect
        } else {
            Self::Solid
        }
    }
}

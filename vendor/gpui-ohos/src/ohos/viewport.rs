use crate::{Pixels, Size};
use std::cell::Cell;

type ResizeCallback = Box<dyn FnMut(Size<Pixels>, f32)>;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Viewport {
    bounds_size: Size<Pixels>,
    effective_size: Size<Pixels>,
    scale: f32,
}

/// Tracks the viewport delivered to GPUI, independently of renderer geometry.
#[derive(Default)]
pub(crate) struct ViewportPublisher {
    published: Cell<Option<Viewport>>,
}

impl ViewportPublisher {
    pub(crate) fn reset(&self) {
        self.published.set(None);
    }

    pub(crate) fn publish(
        &self,
        bounds_size: Size<Pixels>,
        size: Size<Pixels>,
        scale: f32,
        callback: &mut Option<ResizeCallback>,
    ) {
        let viewport = Viewport {
            bounds_size,
            effective_size: size,
            scale,
        };
        if self.published.get() == Some(viewport) {
            return;
        }
        if let Some(callback) = callback {
            self.published.set(Some(viewport));
            callback(size, scale);
        }
    }
}

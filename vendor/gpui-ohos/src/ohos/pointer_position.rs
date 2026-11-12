//! Preserve native pointer precision for clients with device-pixel boundaries.
use gpui::{Pixels, Point, WindowId};
use std::cell::Cell;

thread_local! {
    #[allow(clippy::missing_const_for_thread_local, reason = "OHOS std TLS expansion warns on an already-const initializer")]
    static LATEST: Cell<Option<NativePointerPosition>> = const { Cell::new(None) };
}

/// Last pointer position converted on this UI thread. Matching the window,
/// logical position and scale prevents using another event's coordinates.
/// This value never retains a native window or changes GPUI's event ABI.
#[derive(Clone, Copy)]
pub struct NativePointerPosition {
    window: WindowId,
    logical: Point<Pixels>,
    device: Point<f32>,
    scale: f32,
}

impl NativePointerPosition {
    pub(super) fn record(window: WindowId, logical: Point<Pixels>, device: Point<f32>, scale: f32) {
        let valid = device.x.is_finite()
            && device.y.is_finite()
            && logical.x.as_f32().is_finite()
            && logical.y.as_f32().is_finite()
            && scale.is_finite()
            && scale > 0.;
        LATEST.with(|latest| {
            latest.set(valid.then_some(Self {
                window,
                logical,
                device,
                scale,
            }))
        });
    }

    /// Return exact window-relative native pixels for this GPUI position.
    pub fn get(window: WindowId, logical: Point<Pixels>, scale: f32) -> Option<Point<f32>> {
        LATEST.with(|latest| {
            latest
                .get()
                .filter(|value| {
                    value.window == window && value.logical == logical && value.scale == scale
                })
                .map(|value| value.device)
        })
    }
}

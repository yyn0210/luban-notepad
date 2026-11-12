//! Native wheel ticks/device pixels retained during synchronous GPUI dispatch.
use gpui::{point, Pixels, Point, ScrollDelta, WindowId};
use std::{cell::Cell, marker::PhantomData, rc::Rc};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeScrollDelta {
    WheelTicks(Point<f64>),
    DevicePixels(Point<f64>),
}

#[derive(Clone, Copy)]
struct ScrollContext {
    window: WindowId,
    logical: ScrollDelta,
    scale: f32,
    native: Option<NativeScrollDelta>,
}

thread_local! {
    #[allow(clippy::missing_const_for_thread_local, reason = "OHOS std TLS expansion warns on an already-const initializer")]
    static CURRENT: Cell<Option<ScrollContext>> = const { Cell::new(None) };
}

pub(super) struct NativeScrollScope {
    previous: Option<ScrollContext>,
    _ui_thread: PhantomData<Rc<()>>,
}

impl Drop for NativeScrollScope {
    fn drop(&mut self) {
        CURRENT.with(|current| current.set(self.previous));
    }
}

impl NativeScrollScope {
    pub(super) fn bind(
        window: WindowId,
        logical: ScrollDelta,
        scale: f32,
        native: Option<NativeScrollDelta>,
    ) -> Self {
        let previous = CURRENT.with(|current| {
            current.replace(Some(ScrollContext {
                window,
                logical,
                scale,
                native,
            }))
        });
        Self {
            previous,
            _ui_thread: PhantomData,
        }
    }
}

impl NativeScrollDelta {
    /// Query the exact native delta only while its event is dispatched. A
    /// recognizer may lock one axis to zero; that projection is preserved.
    pub fn get(window: WindowId, delta: ScrollDelta, scale: f32) -> Option<Self> {
        CURRENT.with(|current| {
            let context = current.get()?;
            if context.window != window || context.scale != scale {
                return None;
            }
            let (expected, actual) = match (context.logical, delta) {
                (ScrollDelta::Lines(expected), ScrollDelta::Lines(actual)) => (expected, actual),
                (ScrollDelta::Pixels(expected), ScrollDelta::Pixels(actual)) => {
                    (expected.map(Pixels::as_f32), actual.map(Pixels::as_f32))
                }
                _ => return None,
            };
            if (actual.x != 0. && actual.x != expected.x)
                || (actual.y != 0. && actual.y != expected.y)
            {
                return None;
            }
            let project = |value: Point<f64>| {
                point(
                    if actual.x == 0. && expected.x != 0. {
                        0.
                    } else {
                        value.x
                    },
                    if actual.y == 0. && expected.y != 0. {
                        0.
                    } else {
                        value.y
                    },
                )
            };
            Some(match context.native? {
                Self::WheelTicks(value) => Self::WheelTicks(project(value)),
                Self::DevicePixels(value) => Self::DevicePixels(project(value)),
            })
        })
    }
}

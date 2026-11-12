use super::window_state::NativeWindowState;
use crate::ResizeEdge;

#[derive(Clone, Copy)]
pub(super) struct WindowManipulation {
    pub initial: NativeWindowState,
    pub pointer: (i64, i64),
    pub edge: Option<ResizeEdge>,
}
impl WindowManipulation {
    pub fn update(self, pointer: (i64, i64), minimum: (i64, i64)) -> NativeWindowState {
        let dx = pointer.0.saturating_sub(self.pointer.0);
        let dy = pointer.1.saturating_sub(self.pointer.1);
        let mut result = self.initial;
        match self.edge {
            None => {
                result.left = result.left.saturating_add(dx);
                result.top = result.top.saturating_add(dy);
            }
            Some(edge) => {
                if matches!(
                    edge,
                    ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft
                ) {
                    result.width =
                        (self.initial.width - dx).clamp(minimum.0.max(1), i64::from(i32::MAX));
                    result.left = self.initial.left + self.initial.width - result.width;
                }
                if matches!(
                    edge,
                    ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight
                ) {
                    result.width =
                        (self.initial.width + dx).clamp(minimum.0.max(1), i64::from(i32::MAX));
                }
                if matches!(
                    edge,
                    ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight
                ) {
                    result.height =
                        (self.initial.height - dy).clamp(minimum.1.max(1), i64::from(i32::MAX));
                    result.top = self.initial.top + self.initial.height - result.height;
                }
                if matches!(
                    edge,
                    ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight
                ) {
                    result.height =
                        (self.initial.height + dy).clamp(minimum.1.max(1), i64::from(i32::MAX));
                }
            }
        }
        result.left = result.left.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
        result.top = result.top.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
        result
    }
}

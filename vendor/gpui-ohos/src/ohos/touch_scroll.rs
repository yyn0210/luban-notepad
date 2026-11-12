//! ArkUI pan recognition adapted to GPUI's public platform input interface.
//!
//! Raw XComponent contacts offer control capture, taps and long presses.
//! Only system-recognized Pan events move scroll containers. ArkUI supplies
//! their deltas and release velocity; there is no touch velocity estimator.

use std::time::{Duration, Instant};

use gpui::{
    point, px, DispatchEventResult, GestureTuning, LongPressEvent, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput, Point, ScrollDelta,
    ScrollEvent, ScrollSource, TouchEvent, TouchId, TouchPhase,
};

const MAX_FLING_SPEED: f32 = 8000.;
const STOP_SPEED: f32 = 10.;
// ArkUI's API 12 friction motion: exp(-4.2 * 0.75 * seconds).
const DECELERATION: f32 = 4.2 * 0.75;

#[derive(Clone, Copy)]
enum Axis {
    Horizontal,
    Vertical,
}

impl Axis {
    fn from_delta(delta: Point<Pixels>) -> Self {
        if delta.x.abs() > delta.y.abs() {
            Self::Horizontal
        } else {
            Self::Vertical
        }
    }

    fn lock(self, delta: &mut Point<Pixels>) {
        match self {
            Self::Horizontal => delta.y = px(0.),
            Self::Vertical => delta.x = px(0.),
        }
    }

    fn lock_velocity(self, velocity: &mut Point<f32>) {
        match self {
            Self::Horizontal => velocity.y = 0.,
            Self::Vertical => velocity.x = 0.,
        }
    }
}

pub(crate) struct NativePanInput {
    pub(crate) id: Option<TouchId>,
    pub(crate) phase: TouchPhase,
    /// Already converted from native device pixels to GPUI logical pixels.
    pub(crate) delta: Point<Pixels>,
    /// ArkUI release velocity, converted once from px/s to logical px/s.
    pub(crate) velocity: Point<f32>,
}

enum ContactKind {
    Pending {
        deadline: Instant,
        long_press_offered: bool,
    },
    CoreDrag,
    WindowControl {
        click_count: usize,
        dragged: bool,
    },
    Pan {
        axis: Option<Axis>,
    },
    PanFinished,
    LongPress,
}

struct Contact {
    event: TouchEvent,
    start: Point<Pixels>,
    kind: ContactKind,
    caught: bool,
    // Raw Up and native Pan End may arrive in either order. Retain ownership
    // until both have finished, so neither a click nor a second fling leaks.
    raw_ended: bool,
}

struct Momentum {
    position: Point<Pixels>,
    direction: Point<f32>,
    speed: f32,
    started_at: Instant,
    duration: Duration,
    emitted_distance: f32,
}

struct LastTap {
    position: Point<Pixels>,
    time: Instant,
    count: usize,
}

#[derive(Default)]
pub(crate) struct TouchScroll {
    tuning: GestureTuning,
    contact: Option<Contact>,
    momentum: Option<Momentum>,
    last_tap: Option<LastTap>,
}

impl TouchScroll {
    /// Raw input only offers direct control capture and click/long-press
    /// compatibility. It never synthesizes a pan or computes fling velocity.
    pub(crate) fn relay(
        &mut self,
        event: TouchEvent,
        now: Instant,
        allow_compatibility_tap: bool,
        mut emit: impl FnMut(PlatformInput) -> DispatchEventResult,
    ) -> bool {
        if event.phase == TouchPhase::Started {
            if self
                .contact
                .as_ref()
                .is_some_and(|contact| contact.raw_ended)
            {
                self.cancel_contact(&mut emit);
            }
            // Secondary fingers cannot take over the primary capture.
            if self.contact.is_some() {
                return false;
            }
            let caught = self
                .momentum
                .take()
                .map(|momentum| {
                    emit(scroll(
                        momentum.position,
                        Point::default(),
                        TouchPhase::Ended,
                    ));
                })
                .is_some();
            let claimed = emit(PlatformInput::Touch(event.clone())).default_prevented;
            let kind = if claimed {
                ContactKind::CoreDrag
            } else {
                let mut cancel = event.clone();
                cancel.phase = TouchPhase::Cancelled;
                emit(PlatformInput::Touch(cancel));
                ContactKind::Pending {
                    deadline: now + self.tuning.long_press_duration,
                    long_press_offered: false,
                }
            };
            self.contact = Some(Contact {
                start: event.position,
                event,
                kind,
                caught,
                raw_ended: false,
            });
            return false;
        }

        let Some(mut contact) = self.contact.take() else {
            return false;
        };
        if contact.event.id != event.id {
            self.contact = Some(contact);
            return false;
        }
        let ended = matches!(event.phase, TouchPhase::Ended | TouchPhase::Cancelled);
        let mut tapped = false;
        match contact.kind {
            ContactKind::CoreDrag => {
                emit(PlatformInput::Touch(event.clone()));
                tapped = event.phase == TouchPhase::Ended
                    && !contact.caught
                    && (event.position - contact.start).magnitude()
                        <= f64::from(self.tuning.touch_slop);
            }
            ContactKind::WindowControl {
                click_count,
                mut dragged,
            } => {
                dragged |= (event.position - contact.start).magnitude()
                    > f64::from(self.tuning.touch_slop);
                contact.kind = ContactKind::WindowControl {
                    click_count,
                    dragged,
                };
                if ended {
                    tapped = event.phase == TouchPhase::Ended && !dragged;
                    self.last_tap = tapped.then_some(LastTap {
                        position: event.position,
                        time: now,
                        count: click_count,
                    });
                    release_window_control(&event, click_count, tapped, &mut emit);
                } else {
                    emit(PlatformInput::MouseMove(MouseMoveEvent {
                        position: event.position,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Modifiers::default(),
                    }));
                }
            }
            ContactKind::Pending { .. } => {
                if allow_compatibility_tap
                    && event.phase == TouchPhase::Ended
                    && !contact.caught
                    && (event.position - contact.start).magnitude()
                        <= f64::from(self.tuning.touch_slop)
                {
                    let count = self
                        .last_tap
                        .as_ref()
                        .filter(|tap| {
                            now.saturating_duration_since(tap.time)
                                <= self.tuning.multi_tap_interval
                                && (event.position - tap.position).magnitude()
                                    <= f64::from(self.tuning.multi_tap_slop)
                        })
                        .map_or(1, |tap| tap.count.saturating_add(1));
                    self.last_tap = Some(LastTap {
                        position: event.position,
                        time: now,
                        count,
                    });
                    emit(PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position: event.position,
                        modifiers: Modifiers::default(),
                        click_count: count,
                        first_mouse: false,
                    }));
                    emit(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: event.position,
                        modifiers: Modifiers::default(),
                        click_count: count,
                    }));
                    // Restore touch modality after compatibility mouse events.
                    let mut cancel = event.clone();
                    cancel.phase = TouchPhase::Cancelled;
                    emit(PlatformInput::Touch(cancel));
                    tapped = true;
                }
            }
            ContactKind::Pan { .. } | ContactKind::PanFinished => {}
            ContactKind::LongPress => {
                emit(PlatformInput::LongPress(LongPressEvent {
                    phase: event.phase,
                    start_position: contact.start,
                    position: event.position,
                }));
            }
        }
        contact.event = event;
        if !ended || matches!(contact.kind, ContactKind::Pan { .. }) {
            contact.raw_ended = ended;
            self.contact = Some(contact);
        }
        tapped
    }

    pub(crate) fn capture_window_control(
        &mut self,
        id: TouchId,
        now: Instant,
        mut emit: impl FnMut(PlatformInput) -> DispatchEventResult,
    ) -> bool {
        let Some(contact) = self.contact.as_mut().filter(|contact| {
            contact.event.id == id
                && matches!(contact.kind, ContactKind::Pending { .. })
                && !contact.caught
                && !contact.raw_ended
        }) else {
            return false;
        };
        let click_count = self
            .last_tap
            .as_ref()
            .filter(|tap| {
                now.saturating_duration_since(tap.time) <= self.tuning.multi_tap_interval
                    && (contact.start - tap.position).magnitude()
                        <= f64::from(self.tuning.multi_tap_slop)
            })
            .map_or(1, |tap| tap.count.saturating_add(1));
        contact.kind = ContactKind::WindowControl {
            click_count,
            dragged: false,
        };
        // The press must reach title-bar controls before a native move starts,
        // so their ordinary stop-propagation/default handling still applies.
        let result = emit(PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: contact.start,
            modifiers: Modifiers::default(),
            click_count,
            first_mouse: false,
        }));
        result.propagate && !result.default_prevented
    }

    pub(crate) fn cancel_window_control_click(&mut self) {
        if let Some(Contact {
            kind: ContactKind::WindowControl { dragged, .. },
            ..
        }) = self.contact.as_mut()
        {
            *dragged = true;
        }
    }

    /// ArkUI owns acceptance, displacement and velocity. Its first accepted
    /// delta must move immediately; later packets are already incremental.
    pub(crate) fn native_pan(
        &mut self,
        input: NativePanInput,
        now: Instant,
        mut emit: impl FnMut(PlatformInput) -> DispatchEventResult,
    ) {
        let Some(mut contact) = self.contact.take() else {
            return;
        };
        if input.id.is_some_and(|id| id != contact.event.id) {
            self.contact = Some(contact);
            return;
        }
        match input.phase {
            TouchPhase::Started if matches!(contact.kind, ContactKind::WindowControl { .. }) => {
                if let ContactKind::WindowControl { dragged, .. } = &mut contact.kind {
                    *dragged = true;
                }
            }
            TouchPhase::Started
                if matches!(contact.kind, ContactKind::Pending { .. }) && !contact.raw_ended =>
            {
                let axis = (input.delta != Point::default()).then(|| Axis::from_delta(input.delta));
                contact.kind = ContactKind::Pan { axis };
                let mut delta = input.delta;
                if let Some(axis) = axis {
                    axis.lock(&mut delta);
                }
                // Dropping Start loses movement and delays invalidation until
                // Update, which need not arrive for a short accepted gesture.
                emit(scroll(contact.start, delta, TouchPhase::Started));
            }
            TouchPhase::Moved | TouchPhase::Ended | TouchPhase::Cancelled => {
                if let ContactKind::Pan { axis } = &mut contact.kind {
                    if axis.is_none() && input.delta != Point::default() {
                        *axis = Some(Axis::from_delta(input.delta));
                    }
                    let mut delta = input.delta;
                    if let Some(axis) = axis {
                        axis.lock(&mut delta);
                    }
                    if input.phase == TouchPhase::Cancelled {
                        delta = Point::default();
                    }
                    emit(scroll(contact.start, delta, input.phase));
                    if input.phase == TouchPhase::Ended {
                        let mut velocity = input.velocity;
                        if let Some(axis) = axis {
                            axis.lock_velocity(&mut velocity);
                        }
                        self.start_momentum(contact.start, velocity, now);
                    }
                    if matches!(input.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                        contact.kind = ContactKind::PanFinished;
                    }
                }
            }
            _ => {}
        }
        if !contact.raw_ended || !matches!(contact.kind, ContactKind::PanFinished) {
            self.contact = Some(contact);
        }
    }

    fn start_momentum(&mut self, position: Point<Pixels>, velocity: Point<f32>, now: Instant) {
        let speed = velocity.x.hypot(velocity.y);
        if !speed.is_finite() || speed < self.tuning.min_fling_velocity {
            return;
        }
        let capped_speed = speed.min(MAX_FLING_SPEED);
        self.momentum = Some(Momentum {
            position,
            direction: point(velocity.x / speed, velocity.y / speed),
            speed: capped_speed,
            // Native velocity is already estimated by ArkUI. Start the curve
            // at dispatch time so queued delivery cannot skip its first frame.
            started_at: now,
            duration: Duration::from_secs_f32((capped_speed / STOP_SPEED).ln() / DECELERATION),
            emitted_distance: 0.,
        });
    }

    pub(crate) fn pending_long_press(&self, now: Instant) -> Option<(TouchId, Duration)> {
        let contact = self.contact.as_ref()?;
        if contact.raw_ended
            || (contact.event.position - contact.start).magnitude()
                > f64::from(self.tuning.touch_slop)
        {
            return None;
        }
        match contact.kind {
            ContactKind::Pending {
                deadline,
                long_press_offered: false,
            } => Some((contact.event.id, deadline.saturating_duration_since(now))),
            _ => None,
        }
    }

    pub(crate) fn offer_long_press(
        &mut self,
        id: TouchId,
        mut emit: impl FnMut(PlatformInput) -> DispatchEventResult,
    ) {
        if self.pending_long_press(Instant::now()).is_none() {
            return;
        }
        let Some(contact) = self
            .contact
            .as_mut()
            .filter(|contact| contact.event.id == id)
        else {
            return;
        };
        let ContactKind::Pending {
            long_press_offered, ..
        } = &mut contact.kind
        else {
            return;
        };
        if *long_press_offered {
            return;
        }
        *long_press_offered = true;
        if emit(PlatformInput::LongPress(LongPressEvent {
            phase: TouchPhase::Started,
            start_position: contact.start,
            position: contact.event.position,
        }))
        .default_prevented
        {
            contact.kind = ContactKind::LongPress;
        }
    }

    pub(crate) fn has_momentum(&self) -> bool {
        self.momentum.is_some()
    }

    /// Pan does not emit post-release deltas. GPUI painting still needs one
    /// window-owned inertia curve, advanced once per delivered frame.
    pub(crate) fn tick(&mut self, now: Instant) -> Option<PlatformInput> {
        let momentum = self.momentum.as_mut()?;
        let elapsed = now
            .saturating_duration_since(momentum.started_at)
            .min(momentum.duration);
        let distance =
            momentum.speed / DECELERATION * (1. - (-DECELERATION * elapsed.as_secs_f32()).exp());
        let step = (distance - momentum.emitted_distance).max(0.);
        momentum.emitted_distance = distance.max(momentum.emitted_distance);
        let input = scroll(
            momentum.position,
            point(
                px(momentum.direction.x * step),
                px(momentum.direction.y * step),
            ),
            if elapsed >= momentum.duration {
                TouchPhase::Ended
            } else {
                TouchPhase::Moved
            },
        );
        if elapsed >= momentum.duration {
            self.momentum = None;
        }
        Some(input)
    }

    fn cancel_contact(&mut self, emit: &mut impl FnMut(PlatformInput) -> DispatchEventResult) {
        if let Some(mut contact) = self.contact.take() {
            match contact.kind {
                ContactKind::CoreDrag => {
                    contact.event.phase = TouchPhase::Cancelled;
                    emit(PlatformInput::Touch(contact.event));
                }
                ContactKind::WindowControl { click_count, .. } => {
                    release_window_control(&contact.event, click_count, false, emit);
                }
                ContactKind::Pan { .. } => {
                    emit(scroll(
                        contact.start,
                        Point::default(),
                        TouchPhase::Cancelled,
                    ));
                }
                ContactKind::LongPress => {
                    emit(PlatformInput::LongPress(LongPressEvent {
                        phase: TouchPhase::Cancelled,
                        start_position: contact.start,
                        position: contact.event.position,
                    }));
                }
                ContactKind::Pending { .. } | ContactKind::PanFinished => {}
            }
        }
    }

    pub(crate) fn cancel(&mut self, mut emit: impl FnMut(PlatformInput) -> DispatchEventResult) {
        self.cancel_contact(&mut emit);
        if let Some(momentum) = self.momentum.take() {
            emit(scroll(
                momentum.position,
                Point::default(),
                TouchPhase::Cancelled,
            ));
        }
    }
}

fn release_window_control(
    event: &TouchEvent,
    click_count: usize,
    tapped: bool,
    emit: &mut impl FnMut(PlatformInput) -> DispatchEventResult,
) {
    // GPUI's mouse-up capture clears pressed controls even outside the viewport;
    // releasing there cancels a touch drag without activating the pressed button.
    emit(PlatformInput::MouseUp(MouseUpEvent {
        button: MouseButton::Left,
        position: if tapped {
            event.position
        } else {
            point(px(-1.), px(-1.))
        },
        modifiers: Modifiers::default(),
        click_count,
    }));
    let mut cancel = event.clone();
    cancel.phase = TouchPhase::Cancelled;
    emit(PlatformInput::Touch(cancel));
}

fn scroll(position: Point<Pixels>, delta: Point<Pixels>, touch_phase: TouchPhase) -> PlatformInput {
    PlatformInput::Scroll(ScrollEvent {
        position,
        delta: ScrollDelta::Pixels(delta),
        modifiers: Modifiers::default(),
        touch_phase,
        source: ScrollSource::Touch,
    })
}

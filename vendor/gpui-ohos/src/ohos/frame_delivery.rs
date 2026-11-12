#[cfg(feature = "native-vsync")]
use std::time::Instant;
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use gpui::ForegroundExecutor;
use openharmony_ability::{FrameInputDelivery, OpenHarmonyApp};

#[cfg(feature = "native-vsync")]
use super::native_frames::NativeFrames;
use super::{
    frame_demand::{DeliveryChange, FrameDemand},
    window::OhosWindow,
};

pub(crate) struct FrameCallbackScope {
    active: Rc<Cell<bool>>,
    previous: bool,
}

impl FrameCallbackScope {
    pub(crate) fn enter(active: Rc<Cell<bool>>) -> Self {
        let previous = active.replace(true);
        Self { active, previous }
    }
}

impl Drop for FrameCallbackScope {
    fn drop(&mut self) {
        self.active.set(self.previous);
    }
}

pub(crate) struct WindowFrames {
    app: Rc<RefCell<Option<OpenHarmonyApp>>>,
    window_id: Cell<i64>,
    window: RefCell<Weak<RefCell<OhosWindow>>>,
    demand: RefCell<FrameDemand>,
    surface_available: Cell<bool>,
    foreground: ForegroundExecutor,
    sync_queued: Cell<bool>,
    callback_active: Rc<Cell<bool>>,
    register_warned: Cell<bool>,
    unregister_warned: Cell<bool>,
    #[cfg(feature = "native-vsync")]
    active: Cell<bool>,
    #[cfg(feature = "native-vsync")]
    native: RefCell<Option<NativeFrames>>,
    #[cfg(feature = "native-vsync")]
    native_cleanup_pending: Cell<bool>,
}

impl WindowFrames {
    pub(crate) fn new(
        window_id: i64,
        app: Rc<RefCell<Option<OpenHarmonyApp>>>,
        foreground: ForegroundExecutor,
        callback_active: Rc<Cell<bool>>,
    ) -> Rc<Self> {
        let frames = Rc::new(Self {
            app,
            window_id: Cell::new(window_id),
            window: RefCell::new(Weak::new()),
            demand: RefCell::new(FrameDemand::default()),
            surface_available: Cell::new(false),
            foreground,
            sync_queued: Cell::new(false),
            callback_active,
            register_warned: Cell::new(false),
            unregister_warned: Cell::new(false),
            #[cfg(feature = "native-vsync")]
            active: Cell::new(false),
            #[cfg(feature = "native-vsync")]
            native: RefCell::new(None),
            #[cfg(feature = "native-vsync")]
            native_cleanup_pending: Cell::new(false),
        });
        #[cfg(feature = "native-vsync")]
        super::native_frames::register(&frames);
        frames
    }

    pub(crate) fn set_window(&self, window: Weak<RefCell<OhosWindow>>) {
        *self.window.borrow_mut() = window;
    }

    pub(crate) fn rebind_window(self: &Rc<Self>, window_id: i64) {
        // The frame waker is cached by GPUI and must follow the logical window.
        self.window_id.set(window_id);
        #[cfg(feature = "native-vsync")]
        {
            self.native_cleanup_pending.set(true);
            self.active.set(false);
            super::native_frames::register(self);
        }
        self.demand.borrow_mut().surface_destroyed();
        self.surface_available.set(false);
        self.sync_frame_delivery();
    }

    pub(crate) fn request(self: &Rc<Self>) {
        self.demand.borrow_mut().request();
        self.sync_frame_delivery();
    }

    pub(crate) fn set_active(self: &Rc<Self>, visible: bool, surface_available: bool) {
        #[cfg(feature = "native-vsync")]
        self.active.set(visible && surface_available);
        if self.surface_available.replace(surface_available) && !surface_available {
            self.demand.borrow_mut().surface_destroyed();
        }
        self.demand
            .borrow_mut()
            .set_active(visible && surface_available);
        self.sync_frame_delivery();
    }

    fn sync_frame_delivery(self: &Rc<Self>) {
        let pending = self.demand.borrow().pending().is_some();
        #[cfg(feature = "native-vsync")]
        let pending = pending || self.native_cleanup_pending.get();
        if !pending {
            return;
        }
        if self.callback_active.get() {
            self.defer_sync();
            return;
        }
        #[cfg(feature = "native-vsync")]
        if self.native_cleanup_pending.replace(false) {
            self.native.borrow_mut().take();
        }
        let Some(change) = self.demand.borrow().pending() else {
            return;
        };
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let register = change == DeliveryChange::Register;
        let result = if register {
            self.register_delivery(&app)
        } else {
            #[cfg(feature = "native-vsync")]
            {
                self.native.borrow_mut().take();
                Ok(())
            }
            #[cfg(not(feature = "native-vsync"))]
            app.set_frame_input_delivery_for(self.window_id.get(), FrameInputDelivery::OnDemand)
        };
        self.demand.borrow_mut().complete(change, result.is_ok());
        if let Err(error) = result {
            let warned = if register {
                &self.register_warned
            } else {
                &self.unregister_warned
            };
            if !warned.replace(true) {
                let operation = if register { "register" } else { "unregister" };
                log::warn!(
                    "Cannot {operation} OHOS frame callback for window {}: {error}",
                    self.window_id.get()
                );
            }
        }
    }

    #[cfg(not(feature = "native-vsync"))]
    fn register_delivery(&self, app: &OpenHarmonyApp) -> napi_ohos::Result<()> {
        app.set_frame_input_delivery_for(self.window_id.get(), FrameInputDelivery::Continuous)
    }

    #[cfg(feature = "native-vsync")]
    fn register_delivery(&self, app: &OpenHarmonyApp) -> napi_ohos::Result<()> {
        app.set_frame_input_delivery_for(self.window_id.get(), FrameInputDelivery::OnDemand)?;
        let hz = app.refresh_rate();
        let native = NativeFrames::new(self.window_id.get(), hz, app.main_thread()?)?;
        *self.native.borrow_mut() = Some(native);
        log::info!(
            "native-frame-source window={} target_hz={hz}",
            self.window_id.get()
        );
        Ok(())
    }

    fn defer_sync(self: &Rc<Self>) {
        if self.sync_queued.replace(true) {
            return;
        }
        let frames = self.clone();
        // Keep cleanup alive through window drop; redraw events never drain this queue.
        self.foreground
            .spawn(async move {
                frames.sync_queued.set(false);
                frames.sync_frame_delivery();
            })
            .detach();
    }

    fn on_callback(self: &Rc<Self>) {
        if self.demand.borrow_mut().on_callback().is_some() {
            self.defer_sync();
        }
    }

    #[cfg(not(feature = "native-vsync"))]
    pub(crate) fn on_xcomponent_callback(self: &Rc<Self>) {
        self.on_callback();
    }

    #[cfg(feature = "native-vsync")]
    pub(crate) fn window_id(&self) -> i64 {
        self.window_id.get()
    }

    #[cfg(feature = "native-vsync")]
    pub(crate) fn on_native_callback(self: &Rc<Self>, token: usize) {
        if self
            .native
            .borrow()
            .as_ref()
            .is_none_or(|native| native.token() != token)
        {
            return;
        }
        let _scope = FrameCallbackScope::enter(self.callback_active.clone());
        self.on_callback();
        // Arm before drawing, including the idle-threshold draw, which may
        // create fresh demand. Deferred cleanup releases an idle receiver.
        if self.active.get() {
            let result = self.native.borrow_mut().as_mut().map(NativeFrames::request);
            if let Some(Err(error)) = result {
                log::warn!(
                    "NativeVSync stopped for window {}: {error}",
                    self.window_id.get()
                );
                self.demand
                    .borrow_mut()
                    .complete(DeliveryChange::Register, false);
                self.native_cleanup_pending.set(true);
                self.defer_sync();
            }
        }
        if let Some(window) = self.window.borrow().upgrade() {
            window.borrow().draw_native_frame(Instant::now());
        }
    }
}

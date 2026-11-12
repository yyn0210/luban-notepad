use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::atomic::{AtomicUsize, Ordering},
};

use napi_ohos::{Error, Result};
use ohos_vsync_binding::{OH_NativeVSync_ExpectedRateRange as ExpectedRateRange, Vsync};
use openharmony_ability::MainThreadScheduler;

use super::frame_delivery::WindowFrames;

thread_local! {
    #[allow(clippy::missing_const_for_thread_local, reason = "The initializer is const; OHOS std TLS expansion triggers this lint.")]
    static WINDOWS: RefCell<Vec<Weak<WindowFrames>>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn register(frames: &Rc<WindowFrames>) {
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|window| {
            window
                .upgrade()
                .is_some_and(|window| window.window_id() != frames.window_id())
        });
        windows.push(Rc::downgrade(frames));
    });
}

static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);

/// The binding owns the receiver and its callback liveness gate. A generation
/// token also rejects UI-thread callbacks queued before this owner was dropped.
pub(crate) struct NativeFrames {
    vsync: Vsync,
    token: usize,
    main_thread: MainThreadScheduler,
    window_id: i64,
}

impl NativeFrames {
    pub(crate) fn new(window_id: i64, hz: u32, main_thread: MainThreadScheduler) -> Result<Self> {
        let hz = i32::try_from(hz)
            .ok()
            .filter(|hz| *hz > 0)
            .ok_or_else(|| Error::from_reason(format!("Invalid display refresh rate: {hz}")))?;
        let vsync = Vsync::try_new("gpui-native-render")
            .ok_or_else(|| Error::from_reason("NativeVSync creation failed"))?;
        let result = vsync.set_expected_frame_rate_range(ExpectedRateRange {
            min: hz,
            max: hz,
            expected: hz,
        });
        if result != 0 {
            return Err(Error::from_reason(format!(
                "NativeVSync frame-rate request failed: {result}"
            )));
        }
        let mut connection = Self {
            vsync,
            token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
            main_thread,
            window_id,
        };
        connection.request()?;
        Ok(connection)
    }

    pub(crate) fn token(&self) -> usize {
        self.token
    }

    pub(crate) fn request(&mut self) -> Result<()> {
        let main_thread = self.main_thread.clone();
        let window_id = self.window_id;
        let token = self.token;
        let result = self.vsync.request_frame_once(move |_| {
            // Binding callbacks may run on the VSync thread; GPUI stays on its UI thread.
            if let Err(error) = main_thread.dispatch(move || {
                let frames = WINDOWS.with_borrow(|windows| {
                    windows
                        .iter()
                        .filter_map(Weak::upgrade)
                        .find(|frames| frames.window_id() == window_id)
                });
                if let Some(frames) = frames {
                    frames.on_native_callback(token);
                }
            }) {
                log::warn!("Cannot dispatch NativeVSync for window {window_id}: {error}");
            }
        });
        if result == 0 {
            Ok(())
        } else {
            Err(Error::from_reason(format!(
                "NativeVSync frame request failed: {result}"
            )))
        }
    }
}

impl Drop for NativeFrames {
    fn drop(&mut self) {
        // Withdraw the vote before the binding destroys the receiver. Its
        // liveness gate suppresses late native callbacks without raw userdata.
        self.vsync.set_expected_frame_rate_range(ExpectedRateRange {
            min: 0,
            max: 0,
            expected: 0,
        });
    }
}

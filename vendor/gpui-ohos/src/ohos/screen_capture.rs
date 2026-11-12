use std::{
    ffi::c_void,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    },
};

use anyhow::{bail, ensure, Result};
use futures::channel::oneshot;
use gpui::{
    size, DevicePixels, ForegroundExecutor, ScreenCaptureFrame, ScreenCaptureSource,
    ScreenCaptureStream, SourceMetadata,
};
use image::RgbaImage;
use log::{error, info, warn};

static RECEIVED_VIDEO_BUFFERS: AtomicUsize = AtomicUsize::new(0);

const VIDEO_BUFFER: i32 = 0;
const RGBA_8888: i32 = 12;
const RGBX_8888: i32 = 11;
const BGRA_8888: i32 = 20;
const BGRX_8888: i32 = 19;

#[repr(C)]
struct NativeScreenCapture {
    _private: [u8; 0],
}

#[repr(C)]
struct AvBuffer {
    _private: [u8; 0],
}

#[repr(C)]
struct NativeBuffer {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Default)]
struct NativeBufferConfig {
    width: i32,
    height: i32,
    format: i32,
    usage: i32,
    stride: i32,
}

#[repr(C)]
struct AudioCaptureInfo {
    sample_rate: i32,
    channels: i32,
    source: i32,
}

#[repr(C)]
struct AudioEncInfo {
    bitrate: i32,
    format: i32,
}

#[repr(C)]
struct AudioInfo {
    microphone: AudioCaptureInfo,
    internal: AudioCaptureInfo,
    encoder: AudioEncInfo,
}

#[repr(C)]
struct VideoCaptureInfo {
    display_id: u64,
    mission_ids: *mut i32,
    mission_ids_len: i32,
    width: i32,
    height: i32,
    source: i32,
}

#[repr(C)]
struct VideoEncInfo {
    format: i32,
    bitrate: i32,
    frame_rate: i32,
}

#[repr(C)]
struct VideoInfo {
    capture: VideoCaptureInfo,
    encoder: VideoEncInfo,
}

#[repr(C)]
struct RecorderInfo {
    url: *mut i8,
    url_len: u32,
    format: i32,
}

#[repr(C)]
struct ScreenCaptureConfig {
    capture_mode: i32,
    data_type: i32,
    audio: AudioInfo,
    video: VideoInfo,
    recorder: RecorderInfo,
}

#[link(name = "native_avscreen_capture")]
unsafe extern "C" {
    fn OH_AVScreenCapture_Create() -> *mut NativeScreenCapture;
    fn OH_AVScreenCapture_Init(
        capture: *mut NativeScreenCapture,
        config: ScreenCaptureConfig,
    ) -> i32;
    fn OH_AVScreenCapture_StartScreenCapture(capture: *mut NativeScreenCapture) -> i32;
    fn OH_AVScreenCapture_StopScreenCapture(capture: *mut NativeScreenCapture) -> i32;
    fn OH_AVScreenCapture_Release(capture: *mut NativeScreenCapture) -> i32;
    fn OH_AVScreenCapture_SetDataCallback(
        capture: *mut NativeScreenCapture,
        callback: unsafe extern "C" fn(
            *mut NativeScreenCapture,
            *mut AvBuffer,
            i32,
            i64,
            *mut c_void,
        ),
        user_data: *mut c_void,
    ) -> i32;
    fn OH_AVScreenCapture_SetStateCallback(
        capture: *mut NativeScreenCapture,
        callback: unsafe extern "C" fn(*mut NativeScreenCapture, i32, *mut c_void),
        user_data: *mut c_void,
    ) -> i32;
    fn OH_AVScreenCapture_SetErrorCallback(
        capture: *mut NativeScreenCapture,
        callback: unsafe extern "C" fn(*mut NativeScreenCapture, i32, *mut c_void),
        user_data: *mut c_void,
    ) -> i32;
}

#[link(name = "native_media_core")]
unsafe extern "C" {
    fn OH_AVBuffer_GetNativeBuffer(buffer: *mut AvBuffer) -> *mut NativeBuffer;
}

#[link(name = "native_buffer")]
unsafe extern "C" {
    fn OH_NativeBuffer_GetConfig(buffer: *mut NativeBuffer, config: *mut NativeBufferConfig);
    fn OH_NativeBuffer_Map(buffer: *mut NativeBuffer, address: *mut *mut c_void) -> i32;
    fn OH_NativeBuffer_Unmap(buffer: *mut NativeBuffer) -> i32;
}

struct CaptureCallbacks {
    frame: Mutex<Box<dyn Fn(ScreenCaptureFrame) + Send>>,
    failed: AtomicBool,
}

pub(super) struct DisplayCaptureSource {
    pub display_id: u64,
    pub width: i32,
    pub height: i32,
    pub is_main: bool,
}

struct OhosScreenCaptureSource {
    target: DisplayCaptureSource,
}

impl OhosScreenCaptureSource {
    fn metadata(&self) -> SourceMetadata {
        SourceMetadata {
            id: self.target.display_id,
            label: Some(format!("OpenHarmony display {}", self.target.display_id).into()),
            is_main: Some(self.target.is_main),
            resolution: size(
                DevicePixels(self.target.width),
                DevicePixels(self.target.height),
            ),
        }
    }
}

impl ScreenCaptureSource for OhosScreenCaptureSource {
    fn metadata(&self) -> Result<SourceMetadata> {
        Ok(self.metadata())
    }

    fn stream(
        &self,
        _foreground_executor: &ForegroundExecutor,
        frame_callback: Box<dyn Fn(ScreenCaptureFrame) + Send>,
    ) -> oneshot::Receiver<Result<Box<dyn ScreenCaptureStream>>> {
        let (sender, receiver) = oneshot::channel();
        let stream =
            OhosScreenCaptureStream::start(self.metadata(), self.target.display_id, frame_callback)
                .map(|stream| Box::new(stream) as Box<dyn ScreenCaptureStream>);
        if sender.send(stream).is_err() {
            warn!("OHOS screen capture receiver was dropped before the stream started");
        }
        receiver
    }
}

pub(super) fn sources(
    displays: Vec<DisplayCaptureSource>,
) -> oneshot::Receiver<Result<Vec<Rc<dyn ScreenCaptureSource>>>> {
    let (sender, receiver) = oneshot::channel();
    let sources = displays
        .into_iter()
        .filter(|display| display.width > 0 && display.height > 0)
        .map(|target| Rc::new(OhosScreenCaptureSource { target }) as Rc<dyn ScreenCaptureSource>)
        .collect::<Vec<_>>();
    let result = if sources.is_empty() {
        Err(anyhow::anyhow!("OHOS display size is unavailable"))
    } else {
        Ok(sources)
    };
    if sender.send(result).is_err() {
        warn!("OHOS screen capture source receiver was dropped");
    }
    receiver
}

struct OhosScreenCaptureStream {
    capture: *mut NativeScreenCapture,
    callbacks: *mut CaptureCallbacks,
    metadata: SourceMetadata,
    started: bool,
}

impl OhosScreenCaptureStream {
    fn start(
        metadata: SourceMetadata,
        display_id: u64,
        frame_callback: Box<dyn Fn(ScreenCaptureFrame) + Send>,
    ) -> Result<Self> {
        let capture = unsafe { OH_AVScreenCapture_Create() };
        ensure!(!capture.is_null(), "OHOS AVScreenCapture is unavailable");
        let callbacks = Box::into_raw(Box::new(CaptureCallbacks {
            frame: Mutex::new(frame_callback),
            failed: AtomicBool::new(false),
        }));
        let mut stream = Self {
            capture,
            callbacks,
            metadata,
            started: false,
        };
        let resolution = stream.metadata.resolution;
        let capture_mode = if stream.metadata.is_main == Some(true) {
            0 // OH_CAPTURE_HOME_SCREEN targets the primary display.
        } else {
            1 // OH_CAPTURE_SPECIFIED_SCREEN uses the selected display ID.
        };
        let config = ScreenCaptureConfig {
            capture_mode,
            data_type: 0, // OH_ORIGINAL_STREAM
            audio: AudioInfo {
                microphone: AudioCaptureInfo {
                    sample_rate: 0,
                    channels: 0,
                    source: -1,
                },
                internal: AudioCaptureInfo {
                    sample_rate: 0,
                    channels: 0,
                    source: -1,
                },
                encoder: AudioEncInfo {
                    bitrate: 0,
                    format: 0,
                },
            },
            video: VideoInfo {
                capture: VideoCaptureInfo {
                    display_id,
                    mission_ids: ptr::null_mut(),
                    mission_ids_len: 0,
                    width: resolution.width.0,
                    height: resolution.height.0,
                    source: 2, // OH_VIDEO_SOURCE_SURFACE_RGBA
                },
                encoder: VideoEncInfo {
                    format: 0,
                    bitrate: 0,
                    frame_rate: 0,
                },
            },
            recorder: RecorderInfo {
                url: ptr::null_mut(),
                url_len: 0,
                format: 0,
            },
        };
        check(
            unsafe { OH_AVScreenCapture_Init(capture, config) },
            "initialize capture",
        )?;
        let user_data = callbacks.cast::<c_void>();
        check(
            unsafe { OH_AVScreenCapture_SetDataCallback(capture, on_buffer, user_data) },
            "register video callback",
        )?;
        check(
            unsafe { OH_AVScreenCapture_SetStateCallback(capture, on_state, user_data) },
            "register state callback",
        )?;
        check(
            unsafe { OH_AVScreenCapture_SetErrorCallback(capture, on_error, user_data) },
            "register error callback",
        )?;
        check(
            unsafe { OH_AVScreenCapture_StartScreenCapture(capture) },
            "start capture",
        )?;
        stream.started = true;
        Ok(stream)
    }
}

impl ScreenCaptureStream for OhosScreenCaptureStream {
    fn metadata(&self) -> Result<SourceMetadata> {
        Ok(self.metadata.clone())
    }
}

impl Drop for OhosScreenCaptureStream {
    fn drop(&mut self) {
        if self.started {
            let result = unsafe { OH_AVScreenCapture_StopScreenCapture(self.capture) };
            if result != 0 {
                warn!("Failed to stop OHOS screen capture: {result}");
            }
        }
        let result = unsafe { OH_AVScreenCapture_Release(self.capture) };
        if result != 0 {
            warn!("Failed to release OHOS screen capture: {result}");
        }
        // Release joins native callback execution before callback storage is freed.
        unsafe { drop(Box::from_raw(self.callbacks)) };
    }
}

fn check(code: i32, operation: &str) -> Result<()> {
    if code != 0 {
        bail!("OHOS AVScreenCapture failed to {operation}: {code}");
    }
    Ok(())
}

unsafe extern "C" fn on_state(
    _capture: *mut NativeScreenCapture,
    state: i32,
    _user_data: *mut c_void,
) {
    info!("OHOS screen capture changed state: {state}");
}

unsafe extern "C" fn on_error(
    _capture: *mut NativeScreenCapture,
    code: i32,
    _user_data: *mut c_void,
) {
    error!("OHOS screen capture error: {code}");
}

unsafe extern "C" fn on_buffer(
    _capture: *mut NativeScreenCapture,
    buffer: *mut AvBuffer,
    buffer_type: i32,
    _timestamp: i64,
    user_data: *mut c_void,
) {
    if buffer_type != VIDEO_BUFFER || buffer.is_null() || user_data.is_null() {
        return;
    }
    let count = RECEIVED_VIDEO_BUFFERS.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= 3 {
        info!("OHOS screen capture received video buffer {count}");
    }
    let result = catch_unwind(AssertUnwindSafe(|| unsafe {
        copy_frame(buffer, &*(user_data as *const CaptureCallbacks))
    }));
    if result.is_err() {
        error!("OHOS screen capture frame callback panicked");
    }
}

unsafe fn copy_frame(buffer: *mut AvBuffer, callbacks: &CaptureCallbacks) {
    if callbacks.failed.load(Ordering::Acquire) {
        return;
    }
    let native = unsafe { OH_AVBuffer_GetNativeBuffer(buffer) };
    if native.is_null() {
        return;
    }
    let mut config = NativeBufferConfig::default();
    unsafe { OH_NativeBuffer_GetConfig(native, &mut config) };
    if RECEIVED_VIDEO_BUFFERS.load(Ordering::Relaxed) <= 3 {
        info!(
            "OHOS capture buffer: {}x{}, stride {}, format {}",
            config.width, config.height, config.stride, config.format
        );
    }
    let Ok(width) = usize::try_from(config.width) else {
        return;
    };
    let Ok(height) = usize::try_from(config.height) else {
        return;
    };
    let Ok(stride) = usize::try_from(config.stride) else {
        return;
    };
    let Some(row_bytes) = width.checked_mul(4) else {
        return;
    };
    let Some(frame_bytes) = row_bytes.checked_mul(height) else {
        return;
    };
    if width == 0 || height == 0 || stride < row_bytes || frame_bytes > 256 * 1024 * 1024 {
        return;
    }
    if !matches!(config.format, RGBA_8888 | RGBX_8888 | BGRA_8888 | BGRX_8888) {
        warn!("Unsupported OHOS capture pixel format: {}", config.format);
        return;
    }
    let mut address: *mut c_void = ptr::null_mut();
    if unsafe { OH_NativeBuffer_Map(native, &mut address) } != 0 || address.is_null() {
        warn!("Failed to map OHOS screen capture frame");
        return;
    }
    let format = match config.format {
        RGBA_8888 => super::capture_pixels::CaptureFormat::Rgba,
        RGBX_8888 => super::capture_pixels::CaptureFormat::Rgbx,
        BGRA_8888 => super::capture_pixels::CaptureFormat::Bgra,
        _ => super::capture_pixels::CaptureFormat::Bgrx,
    };
    let Some(mut pixels) = super::capture_pixels::CapturePixels::new(width, height, format) else {
        let _ = unsafe { OH_NativeBuffer_Unmap(native) };
        return;
    };
    for row in 0..height {
        let Some(source_offset) = row.checked_mul(stride) else {
            break;
        };
        let source = unsafe {
            std::slice::from_raw_parts((address as *const u8).add(source_offset), row_bytes)
        };
        if !pixels.push_row(source) {
            break;
        }
    }
    let result = unsafe { OH_NativeBuffer_Unmap(native) };
    if result != 0 {
        warn!("Failed to unmap OHOS screen capture frame: {result}");
        return;
    }
    let Some(pixels) = pixels.finish() else {
        return;
    };
    if RgbaImage::from_raw(config.width as u32, config.height as u32, pixels).is_some() {
        match callbacks.frame.lock() {
            Ok(callback) => {
                // Catch client panics while the guard remains on this stack so
                // a single bad frame callback cannot poison subsequent delivery.
                if catch_unwind(AssertUnwindSafe(|| callback(ScreenCaptureFrame(())))).is_err() {
                    callbacks.failed.store(true, Ordering::Release);
                    error!("OHOS screen capture client callback panicked; stopping frame delivery");
                }
            }
            Err(_) => error!("OHOS screen capture callback lock is poisoned"),
        }
    }
}

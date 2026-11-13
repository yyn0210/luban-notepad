use log::{debug, info, warn};

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    rc::{Rc, Weak},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use accesskit_ohos::Adapter as OhosA11yAdapter;
use anyhow::Result;
use futures::channel::oneshot;
use ohos_accessibility_binding::Provider;
use openharmony_ability::{
    arkui::arkui_input_binding::{UIInputAction, UIInputEvent, UIInputSourceType, UIInputToolType},
    arkui::component::attribute::{ArkUIAttributeBasic, ArkUICommonAttribute},
    ime::{
        Action as ImeAction, Direction as ImeDirection, EnterKey, ImeResult, InputType,
        Rect as ImeCursorRect, TextState, IME,
    },
    xcomponent::{
        Action as KeyAction, MouseAction, MouseButton as OhosMouseButton,
        TouchEvent as OhosTouchEvent, TouchEventData, TouchPointData, TouchPointTool,
    },
    ArkUiInputEvent, AvoidAreaType, Event, ImeEvent, InputEvent, OpenHarmonyApp, PointerInputData,
    XComponentInputEvent,
};
use openharmony_ability_plugin_menu::{MenuClient, MenuCommandItem, MenuPopupCommandsRequest};
use openharmony_ability_plugin_window::{
    on_window_close_requested, WindowClient, WindowCloseSubscription, WindowMoveSubscription,
};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use rustc_hash::FxHashMap;

use super::display::OhosDisplay;
use super::frame_delivery::WindowFrames;
use super::keyboard::OhosKeyState;
use super::platform::appearance_for_color_mode;
use super::presentation::{Presentation, PresentationDemand};
use super::touch_scroll::{NativePanInput, TouchScroll};
use super::wgpu_atlas::WgpuAtlas;
use super::wgpu_context::WgpuContext;
use super::wgpu_renderer::{WgpuRenderer, WgpuSurfaceConfig};
use super::{mouse_buttons::MouseButtons, scroll_packet::ScrollPacket};
use crate::{
    accesskit, point, px, size, A11yCallbacks, AnyWindowHandle, Bounds, Capslock, Corners,
    CursorStyle, DevicePixels, Edges, ForegroundExecutor, FrameRequestSource, GestureTuning,
    GpuSpecs, KeyDownEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseExitEvent,
    MouseMoveEvent, MouseUpEvent, NavigationDirection, Pixels, PlatformAtlas, PlatformDisplay,
    PlatformInput, PlatformInputHandler, PlatformWindow, Point, PromptButton, PromptLevel,
    RequestFrameOptions, ResizeEdge, Scene, ScrollDelta, ScrollEvent, ScrollSource, Size, Task,
    TextInputAction, TextInputConfiguration, TextInputStateChange, TouchEvent, TouchId, TouchPhase,
    WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowControls,
    WindowDecorations, WindowInsets, WindowParams, WindowVisibility,
};

pub(crate) type BackHandler = Rc<RefCell<Option<Box<dyn FnMut()>>>>;
type ResizeCallback = Box<dyn FnMut(Size<Pixels>, f32)>;

type WindowList = RefCell<Vec<Weak<RefCell<OhosWindow>>>>;
type WindowIndex = RefCell<FxHashMap<i64, Weak<RefCell<OhosWindow>>>>;

pub(crate) struct OhosWindowContext {
    pub(crate) form_factor: super::device::DeviceFormFactor,
    pub(crate) native_menus: bool,
    pub(crate) windows: Weak<WindowList>,
    pub(crate) window_index: Weak<WindowIndex>,
    pub(crate) app: Rc<RefCell<Option<OpenHarmonyApp>>>,
    pub(crate) handle: AnyWindowHandle,
    pub(crate) params: WindowParams,
    pub(crate) gpu_context: Arc<WgpuContext>,
    pub(crate) display: OhosDisplay,
    pub(crate) foreground_executor: ForegroundExecutor,
    pub(crate) background_executor: crate::BackgroundExecutor,
    pub(crate) frame_callback_active: Rc<Cell<bool>>,
    pub(crate) clipboard: super::clipboard::OhosClipboard,
    pub(crate) file_access: Arc<super::file_access::FileAccess>,
    pub(crate) cursor_visibility: Rc<RefCell<super::cursor_visibility::CursorVisibility>>,
    pub(crate) cursor_style: Rc<Cell<Option<(i64, CursorStyle)>>>,
    pub(crate) appearance_override: Rc<Cell<Option<WindowAppearance>>>,
    pub(crate) window_id: i64,
    pub(crate) fallback_atlas: Option<Arc<WgpuAtlas>>,
}

pub(crate) struct OhosWindow {
    form_factor: super::device::DeviceFormFactor,
    native_menus: bool,
    windows: Weak<WindowList>,
    window_index: Weak<WindowIndex>,
    released: Rc<Cell<bool>>,
    close_subscription: RefCell<Option<WindowCloseSubscription>>,
    move_subscription: RefCell<Option<WindowMoveSubscription>>,
    app: Rc<RefCell<Option<OpenHarmonyApp>>>,
    pub(crate) handle: AnyWindowHandle,
    bounds: RefCell<Bounds<Pixels>>,
    viewport: super::viewport::ViewportPublisher,
    scale: RefCell<f32>,
    appearance: Cell<WindowAppearance>,
    appearance_override: Rc<Cell<Option<WindowAppearance>>>,
    window_id: i64,
    title: Rc<RefCell<String>>,
    title_generation: Rc<Cell<u64>>,
    restore_state: Rc<RefCell<Option<super::window_state::NativeWindowState>>>,
    restore_pending: Rc<Cell<bool>>,
    mode_operations: Rc<futures::lock::Mutex<()>>,
    floating_state: Cell<Option<super::window_state::NativeWindowState>>,
    is_movable: bool,
    is_resizable: bool,
    is_minimizable: bool,
    min_size: Option<Size<Pixels>>,
    frames: Rc<WindowFrames>,
    closed: Cell<bool>,
    free_window: Cell<bool>,
    maximized: Rc<Cell<bool>>,
    fullscreen: Rc<Cell<bool>>,
    background_appearance: Rc<Cell<WindowBackgroundAppearance>>,
    requested_background: Rc<Cell<Option<WindowBackgroundAppearance>>>,
    background_pending: Rc<Cell<bool>>,
    background_generation: Rc<Cell<u64>>,
    active: Cell<bool>,
    activation_pending: Cell<bool>,
    activation_generation: Cell<u64>,
    hovered: Cell<bool>,
    visibility: Cell<WindowVisibility>,
    show_on_creation: bool,
    decorations: Rc<Cell<WindowDecorations>>,
    requested_decorations: Cell<WindowDecorations>,
    insets: RefCell<WindowInsets>,
    keyboard_overlap_device_px: Cell<i32>,
    input_handler: Rc<RefCell<Option<PlatformInputHandler>>>,
    ime_text_update_pending: Rc<Cell<bool>>,
    edit_generation: Rc<Cell<u64>>,
    clipboard: super::clipboard::OhosClipboard,
    file_access: Arc<super::file_access::FileAccess>,
    callbacks: Rc<RefCell<WindowCallbacks>>,
    renderer: RefCell<Option<WgpuRenderer>>,
    presentation: PresentationDemand,
    defer_presentation: Cell<bool>,
    surface_available: Cell<bool>,
    input_ready: Cell<bool>,
    surface_focused: Cell<bool>,
    force_render: Cell<bool>,
    gpu_context: Arc<WgpuContext>,
    display: OhosDisplay,
    fallback_atlas: RefCell<Option<Arc<WgpuAtlas>>>,
    foreground_executor: ForegroundExecutor,
    background_executor: crate::BackgroundExecutor,
    cursor_visibility: Rc<RefCell<super::cursor_visibility::CursorVisibility>>,
    cursor_style: Rc<Cell<Option<(i64, CursorStyle)>>>,
    keyboard_visible: Rc<Cell<bool>>,
    ime_action: Cell<TextInputAction>,
    ime_retry_generation: Cell<u64>,
    ime_recovery_pending: Cell<bool>,
    ime_retry_pending: Cell<bool>,
    pointer_position: Cell<Option<Point<Pixels>>>,
    pointer_screen_position: Cell<Option<(i64, i64)>>,
    pressed_mouse_button: Cell<MouseButtons>,
    /// When the most recent mouse button transition was delivered. OHOS PCs
    /// also report that mouse as touch points, so a tap arriving around a
    /// button press/release is the compatibility duplicate of that mouse
    /// click. Hover moves must not count: they arrive just before every fast
    /// click and would otherwise hide the click's own touch tap.
    last_mouse_button_at: Cell<Option<Instant>>,
    /// When the current touch contact started, used to tell a duplicate mouse
    /// tap (a mouse button event belongs to the contact) from a real tap.
    touch_started_at: Cell<Option<Instant>>,
    /// When a touch contact was translated into a compatibility click and
    /// where. The authoritative mouse events can arrive after the touch ones,
    /// so that later click is dropped instead of activating the control twice.
    last_compat_tap: Cell<Option<(Instant, Point<Pixels>)>>,
    /// The mouse release paired with a press already covered by
    /// `last_compat_tap`; it must not synthesize a second click.
    suppress_next_release: Cell<Option<(MouseButton, Instant)>>,
    mouse_click: RefCell<Option<MouseClickState>>,
    native_pointer_id: Cell<Option<i32>>,
    native_drag: Rc<RefCell<Option<openharmony_ability::NativeFileDrag>>>,
    native_drag_pending: Rc<Cell<bool>>,
    incoming_drag: Cell<bool>,
    manipulation: Cell<Option<super::window_manipulation::WindowManipulation>>,
    geometry_request: Rc<RefCell<Option<super::window_state::NativeWindowState>>>,
    geometry_pending: Rc<Cell<bool>>,
    back_enabled: Cell<bool>,
    back_handler: BackHandler,
    a11y_callbacks: RefCell<Option<Arc<Mutex<A11yCallbacks>>>>,
    a11y_adapter: RefCell<Option<OhosA11yAdapter<'static>>>,
    key_state: RefCell<OhosKeyState>,
    active_touches: RefCell<HashMap<i32, TouchId>>,
    primary_touch: Cell<Option<i32>>,
    touch_tap_candidates: RefCell<HashMap<i32, TouchTapCandidate>>,
    next_touch_id: Cell<u64>,
    touch_scroll: Rc<RefCell<TouchScroll>>,
    long_press_timer: RefCell<Option<(TouchId, Task<()>)>>,
}

pub(crate) struct OhosWindowHandle {
    released: Rc<Cell<bool>>,
    inner: Rc<RefCell<OhosWindow>>,
    input_handler: Rc<RefCell<Option<PlatformInputHandler>>>,
}

impl Drop for OhosWindowHandle {
    fn drop(&mut self) {
        self.released.set(true);
    }
}

impl Drop for OhosWindow {
    fn drop(&mut self) {
        self.frames.set_active(false, self.surface_available.get());
        self.native_drag.borrow_mut().take();
        self.background_generation
            .set(self.background_generation.get().wrapping_add(1));
        self.invalidate_pending_edit();
        let was_closed = self.closed.replace(true);
        self.title_generation
            .set(self.title_generation.get().wrapping_add(1));
        self.close_subscription.borrow_mut().take();
        self.move_subscription.borrow_mut().take();
        self.release_accessibility();
        if self.window_id == 0 && !was_closed {
            let successor = self.windows.upgrade().and_then(|windows| {
                windows
                    .borrow()
                    .iter()
                    .rev()
                    .filter_map(Weak::upgrade)
                    .find(|window| {
                        let window = window.borrow();
                        !window.closed.get() && !window.released.get()
                    })
            });
            if let Some(successor) = successor {
                successor
                    .borrow_mut()
                    .take_main_surface(self, Rc::downgrade(&successor));
                Self::register_close_handler(&successor);
                return;
            }
        }
        if self.window_id == 0 || was_closed {
            return;
        }
        let Some(client) = self.window_client() else {
            return;
        };
        let window_id = self.window_id;
        self.foreground_executor
            .spawn(async move {
                if let Err(error) = client.destroy_window(window_id).await {
                    warn!("Failed to destroy OHOS sub-window {window_id}: {error}");
                }
            })
            .detach();
    }
}

impl OhosWindowHandle {
    pub(crate) fn new(inner: Rc<RefCell<OhosWindow>>) -> Self {
        inner.borrow().frames.set_window(Rc::downgrade(&inner));
        let input_handler = inner.borrow().input_handler.clone();
        let released = inner.borrow().released.clone();
        Self {
            inner,
            input_handler,
            released,
        }
    }

    fn with_window<R>(&self, f: impl FnOnce(&OhosWindow) -> R) -> R {
        let window = self.inner.borrow();
        f(&window)
    }
}

struct WindowCallbacks {
    request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    input: Option<Box<dyn FnMut(PlatformInput) -> crate::DispatchEventResult>>,
    input_registered: bool,
    pending_inputs: VecDeque<InputEvent>,
    active_status_change: Option<Box<dyn FnMut(bool)>>,
    visibility_change: Option<Box<dyn FnMut(WindowVisibility)>>,
    insets_changed: Option<Box<dyn FnMut(WindowInsets)>>,
    visual_viewport_changed: Option<Box<dyn FnMut()>>,
    hover_status_change: Option<Box<dyn FnMut(bool)>>,
    resize: Option<ResizeCallback>,
    moved: Option<Box<dyn FnMut()>>,
    display_changed: Option<Box<dyn FnMut()>>,
    should_close: Option<Box<dyn FnMut() -> bool>>,
    close: Option<Box<dyn FnOnce()>>,
    appearance_changed: Option<Box<dyn FnMut()>>,
    button_layout_changed: Option<Box<dyn FnMut()>>,
    hit_test_window_control: Option<Box<dyn FnMut() -> Option<WindowControlArea>>>,
}

#[derive(Clone, Copy)]
struct TouchTapCandidate {
    start_position: Point<Pixels>,
    started_in_text_input: bool,
}

struct MouseClickState {
    button: MouseButton,
    position: Point<Pixels>,
    time: Instant,
    count: usize,
}

struct OhosA11yActivation(Arc<Mutex<A11yCallbacks>>);

impl accesskit::ActivationHandler for OhosA11yActivation {
    fn request_initial_tree(&mut self) -> Option<accesskit::TreeUpdate> {
        self.0
            .lock()
            .ok()
            .and_then(|callbacks| (callbacks.activation)())
    }
}

struct OhosA11yAction(Arc<Mutex<A11yCallbacks>>);

impl accesskit::ActionHandler for OhosA11yAction {
    fn do_action(&mut self, request: accesskit::ActionRequest) {
        if let Ok(callbacks) = self.0.lock() {
            (callbacks.action)(request);
        }
    }
}

impl OhosWindow {
    pub(crate) fn register_close_handler(window: &Rc<RefCell<Self>>) {
        let weak_window = Rc::downgrade(window);
        let foreground = window.borrow().foreground_executor.clone();
        let subscription = on_window_close_requested(window.borrow().window_id, move || {
            let Some(window) = weak_window.upgrade() else {
                return false;
            };
            if window.borrow().released.get() || window.borrow().closed.get() {
                return false;
            }
            foreground
                .spawn(async move {
                    Self::request_close(&window);
                })
                .detach();
            true
        });
        *window.borrow().close_subscription.borrow_mut() = Some(subscription);
        *window.borrow().move_subscription.borrow_mut() =
            Some(WindowMoveSubscription::new(window.borrow().window_id));
    }

    fn request_close(window: &Rc<RefCell<Self>>) {
        let callbacks = {
            let window = window.borrow();
            if window.closed.get() || window.released.get() {
                return;
            }
            window.focus_native_surface();
            window.callbacks.clone()
        };
        // Zed can synchronously reenter the platform while starting its async save workflow.
        let mut should_close = callbacks.borrow_mut().should_close.take();
        let allowed = should_close.as_mut().is_none_or(|callback| callback());
        callbacks.borrow_mut().should_close = should_close;
        if allowed {
            let close = callbacks.borrow_mut().close.take();
            if let Some(close) = close {
                close();
            }
        } else {
            let weak_window = Rc::downgrade(window);
            let window = window.borrow();
            if !window.closed.get() && !window.released.get() {
                if let Some(client) = window.window_client() {
                    let window_id = window.window_id;
                    window
                        .foreground_executor
                        .spawn(async move {
                            if let Err(error) = client.show_window(window_id).await {
                                warn!(
                                    "Failed to show OHOS window {window_id} after vetoed close: {error}"
                                );
                            }
                            if let Err(error) = client.focus_window(window_id).await {
                                warn!(
                                    "Failed to focus OHOS window {window_id} after vetoed close: {error}"
                                );
                            }
                            if let Some(window) = weak_window.upgrade() {
                                let window = window.borrow();
                                if !window.closed.get() && !window.released.get() {
                                    window.focus_native_surface();
                                }
                            }
                        })
                        .detach();
                }
            }
        }
    }

    pub(crate) fn dispatch_window_menu_command(window: &Rc<RefCell<Self>>, command: &str) -> bool {
        if window.borrow().closed.get() || window.borrow().released.get() {
            return true;
        }
        match command {
            "gpui-window-minimize" => window.borrow().minimize(),
            "gpui-window-zoom" => window.borrow().zoom(),
            "gpui-window-fullscreen" => window.borrow().toggle_fullscreen(),
            "gpui-window-close" => Self::request_close(window),
            _ => return false,
        }
        true
    }

    fn focus_native_surface(&self) {
        if !self.active.get() || !self.surface_available.get() {
            return;
        }
        let app = self.app.borrow().clone();
        if let Some(app) = app {
            if let Err(error) = app.request_xcomponent_focus_for(self.window_id) {
                warn!("Failed to focus OHOS render surface: {error}");
            }
        }
    }

    fn native_menu_focus_id(&self) -> Option<String> {
        if !self.surface_available.get() || !self.surface_focused.get() {
            return None;
        }
        let component = self
            .app
            .borrow()
            .as_ref()
            .and_then(|app| app.with_xcomponent_for(self.window_id, Clone::clone))?;
        let identifier = format!("gpui-window-{}-render-surface", self.window_id);
        if let Err(error) = component.id(identifier.clone()) {
            warn!("Failed to identify OHOS menu focus target: {error}");
            return None;
        }
        Some(identifier)
    }

    fn cancel_activation(&self) {
        self.activation_pending.set(false);
        self.activation_generation
            .set(self.activation_generation.get().wrapping_add(1));
    }

    fn cancel_other_activations(&self) {
        if let Some(windows) = self.windows.upgrade() {
            for window in windows.borrow().iter().filter_map(Weak::upgrade) {
                let window = window.borrow();
                if window.window_id != self.window_id {
                    window.cancel_activation();
                }
            }
        }
    }

    fn activation_is_pending(&self, generation: u64) -> bool {
        self.activation_pending.get()
            && self.activation_generation.get() == generation
            && !self.closed.get()
            && !self.released.get()
    }

    fn take_main_surface(&mut self, main: &mut Self, successor: Weak<RefCell<Self>>) {
        let rect = self
            .app
            .borrow()
            .as_ref()
            .map(|app| app.window_rect_for(self.window_id))
            .filter(|rect| rect.width > 0 && rect.height > 0);
        self.frames.set_active(false, self.surface_available.get());
        self.a11y_adapter.borrow_mut().take();
        self.close_subscription.borrow_mut().take();
        self.move_subscription.borrow_mut().take();
        self.invalidate_pending_edit();
        self.title_generation
            .set(self.title_generation.get().wrapping_add(1));
        self.background_generation
            .set(self.background_generation.get().wrapping_add(1));
        self.background_pending.set(false);
        // The native surface survives, but queued edits still belong to the closing editor.
        if let Some(app) = self.app.borrow().as_ref() {
            if let Err(error) = app.reset_ime_for(0) {
                warn!("Failed to reset transferred OHOS editor: {error}");
            }
        }
        let child_id = self.window_id;
        std::mem::swap(&mut self.window_id, &mut main.window_id);
        std::mem::swap(&mut self.renderer, &mut main.renderer);
        std::mem::swap(&mut self.fallback_atlas, &mut main.fallback_atlas);
        *self.bounds.borrow_mut() = *main.bounds.borrow();
        *self.scale.borrow_mut() = *main.scale.borrow();
        self.display = main.display.clone();
        self.surface_available.set(main.surface_available.get());
        self.input_ready.set(main.input_ready.get());
        self.surface_focused.set(main.surface_focused.get());
        self.frames.rebind_window(0);
        if let Some(index) = self.window_index.upgrade() {
            let mut index = index.borrow_mut();
            index.remove(&child_id);
            index.insert(0, successor.clone());
        }
        let Some(client) = self.window_client() else {
            return;
        };
        let maximized = self.maximized.get();
        let fullscreen = self.fullscreen.get();
        self.free_window.set(if fullscreen {
            false
        } else if maximized {
            true
        } else {
            main.free_window.get()
        });
        let restore_main = main.maximized.get() || main.fullscreen.get();
        self.foreground_executor
            .spawn(async move {
                let result = async {
                    if restore_main {
                        client.recover_window(0).await?;
                    }
                    if let Some(rect) = rect {
                        client
                            .move_window_to(0, i64::from(rect.left), i64::from(rect.top))
                            .await?;
                        client
                            .resize_window(0, i64::from(rect.width), i64::from(rect.height))
                            .await?;
                    }
                    if fullscreen {
                        client.set_fullscreen(0, true).await?;
                    } else if maximized {
                        client.maximize_window(0).await?;
                    }
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if let Err(error) = result {
                    warn!("Failed to restore transferred OHOS window: {error}");
                }
                // Keep the child visible while the main surface moves into place, otherwise
                // closing the main editor briefly moves the survivor to the closed window.
                // Destroy the child before attaching its logical editor to the main IME.
                if let Err(error) = client.destroy_window(child_id).await {
                    warn!("Failed to destroy transferred OHOS window {child_id}: {error}");
                }
                if let Err(error) = client.focus_window(0).await {
                    warn!("Failed to focus transferred OHOS window: {error}");
                }
                if let Some(successor) = successor.upgrade() {
                    let window = successor.borrow();
                    if !window.closed.get() && !window.released.get() {
                        window.apply_window_options();
                        window.request_decorations(window.requested_decorations.get());
                        window.apply_requested_background();
                        window.initialize_accessibility();
                        window.focus_native_surface();
                        window.apply_ime_action(true);
                        window.schedule_ime_text_update();
                        window.handle_event(&Event::WindowFocusChanged {
                            window_id: 0,
                            focused: true,
                        });
                        window.refresh_insets();
                        window.emit_resize_callback();
                        window.notify_button_layout_changed();
                        window.sync_frame_delivery();
                        window.force_render.set(true);
                        window.schedule_frame();
                    }
                }
            })
            .detach();
    }

    fn initialize_accessibility(&self) {
        if self.a11y_adapter.borrow().is_some() {
            return;
        }
        let Some(callbacks) = self.a11y_callbacks.borrow().clone() else {
            return;
        };
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let Some(provider_info) = app.with_xcomponent_for(self.window_id, |component| {
            let native = component.native_xcomponent();
            let provider = native
                .accessibility_provider()
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            Ok::<_, anyhow::Error>(provider.as_raw() as usize)
        }) else {
            return;
        };
        let provider_raw = match provider_info {
            Ok(info) => info,
            Err(error) => {
                warn!("Failed to find OHOS accessibility provider: {error}");
                return;
            }
        };
        // The adapter is released on SurfaceDestroy, before the ability drops
        // the render XComponent that owns this provider.
        let provider: Provider<'static> =
            match unsafe { Provider::from_raw(provider_raw as *mut _) } {
                Ok(provider) => provider,
                Err(error) => {
                    warn!("Failed to retain OHOS accessibility provider: {error}");
                    return;
                }
            };
        // Single-instance callbacks have one process-wide Rust handler. The
        // API 15 callback table routes by the framework instance ID registered
        // here; native XComponent IDs are not unique across GPUI windows.
        // A new ID also keeps late callbacks from a destroyed surface isolated.
        let instance_id = format!("gpui-{}", uuid::Uuid::new_v4());
        match OhosA11yAdapter::new_with_instance(
            provider,
            &instance_id,
            OhosA11yActivation(callbacks.clone()),
            OhosA11yAction(callbacks),
        ) {
            Ok(adapter) => {
                if let Err(error) = adapter.set_host_focus_state(self.active.get()) {
                    warn!("Failed to set OHOS accessibility focus: {error}");
                }
                *self.a11y_adapter.borrow_mut() = Some(adapter);
                info!(
                    "OHOS accessibility adapter registered window={}",
                    self.window_id
                );
            }
            Err(error) => warn!("Failed to register OHOS accessibility adapter: {error}"),
        }
    }

    fn release_accessibility(&self) {
        let adapter = self.a11y_adapter.borrow_mut().take();
        if adapter.is_some() {
            drop(adapter);
            // Closed GPUI windows have no registry entry to refresh. Surface
            // recreation still deactivates the live window's accessibility.
            if !self.closed.get() {
                if let Some(callbacks) = self.a11y_callbacks.borrow().as_ref() {
                    if let Ok(callbacks) = callbacks.lock() {
                        (callbacks.deactivation)();
                    }
                }
            }
        }
    }

    pub(crate) fn new(context: OhosWindowContext) -> Result<Self> {
        let OhosWindowContext {
            form_factor,
            native_menus,
            windows,
            window_index,
            app,
            handle,
            params,
            gpu_context,
            display,
            foreground_executor,
            background_executor,
            frame_callback_active,
            clipboard,
            file_access,
            cursor_visibility,
            cursor_style,
            appearance_override,
            window_id,
            fallback_atlas,
        } = context;
        let scale = display.scale_factor();
        let appearance = appearance_override.get().unwrap_or_else(|| {
            app.borrow()
                .as_ref()
                .map(|app| appearance_for_color_mode(app.config().color_mode))
                .unwrap_or_default()
        });
        let input_ready = app
            .borrow()
            .as_ref()
            .is_some_and(|app| app.is_text_input_ready_for(window_id));
        let bounds = params.bounds;
        let frames = WindowFrames::new(
            window_id,
            app.clone(),
            foreground_executor.clone(),
            frame_callback_active,
        );

        // Don't create renderer immediately - native_window may not be available yet.
        // Renderer will be initialized lazily in draw() or when SurfaceCreate event is received.
        // At that point, native_window from OpenHarmonyApp will be available.

        Ok(Self {
            form_factor,
            native_menus,
            windows,
            window_index,
            released: Rc::new(Cell::new(false)),
            close_subscription: RefCell::new(None),
            move_subscription: RefCell::new(None),
            app: app.clone(),
            handle,
            bounds: RefCell::new(bounds),
            viewport: super::viewport::ViewportPublisher::default(),
            scale: RefCell::new(scale),
            appearance: Cell::new(appearance),
            appearance_override,
            window_id,
            title: Rc::new(RefCell::new(
                params
                    .titlebar
                    .and_then(|titlebar| titlebar.title)
                    .map(|title| title.to_string())
                    .unwrap_or_default(),
            )),
            title_generation: Rc::new(Cell::new(0)),
            restore_state: Rc::new(RefCell::new(None)),
            restore_pending: Rc::new(Cell::new(false)),
            mode_operations: Rc::new(futures::lock::Mutex::new(())),
            floating_state: Cell::new(None),
            is_movable: params.is_movable,
            is_resizable: params.is_resizable,
            is_minimizable: params.is_minimizable,
            min_size: params.window_min_size,
            frames,
            closed: Cell::new(false),
            free_window: Cell::new(false),
            maximized: Rc::new(Cell::new(false)),
            fullscreen: Rc::new(Cell::new(false)),
            background_appearance: Rc::new(Cell::new(WindowBackgroundAppearance::Opaque)),
            requested_background: Rc::new(Cell::new(None)),
            background_pending: Rc::new(Cell::new(false)),
            background_generation: Rc::new(Cell::new(0)),
            active: Cell::new(window_id == 0),
            activation_pending: Cell::new(false),
            activation_generation: Cell::new(0),
            hovered: Cell::new(false),
            show_on_creation: params.show,
            visibility: Cell::new(if params.show {
                WindowVisibility::Visible
            } else {
                WindowVisibility::Hidden
            }),
            decorations: Rc::new(Cell::new(WindowDecorations::Server)),
            requested_decorations: Cell::new(WindowDecorations::Server),
            insets: RefCell::new(WindowInsets::default()),
            keyboard_overlap_device_px: Cell::new(0),
            input_handler: Rc::new(RefCell::new(None)),
            ime_text_update_pending: Rc::new(Cell::new(false)),
            edit_generation: Rc::new(Cell::new(0)),
            clipboard,
            file_access,
            callbacks: Rc::new(RefCell::new(WindowCallbacks {
                request_frame: None,
                input: None,
                input_registered: false,
                pending_inputs: VecDeque::new(),
                active_status_change: None,
                visibility_change: None,
                insets_changed: None,
                visual_viewport_changed: None,
                hover_status_change: None,
                resize: None,
                moved: None,
                display_changed: None,
                should_close: None,
                close: None,
                appearance_changed: None,
                button_layout_changed: None,
                hit_test_window_control: None,
            })),
            renderer: RefCell::new(None),
            presentation: PresentationDemand::default(),
            defer_presentation: Cell::new(false),
            surface_available: Cell::new(false),
            input_ready: Cell::new(input_ready),
            surface_focused: Cell::new(true),
            force_render: Cell::new(false),
            gpu_context,
            display,
            fallback_atlas: RefCell::new(fallback_atlas),
            foreground_executor,
            background_executor,
            cursor_visibility,
            cursor_style,
            keyboard_visible: Rc::new(Cell::new(false)),
            ime_action: Cell::new(TextInputAction::Unspecified),
            ime_retry_generation: Cell::new(0),
            ime_recovery_pending: Cell::new(false),
            ime_retry_pending: Cell::new(false),
            pointer_position: Cell::new(None),
            pointer_screen_position: Cell::new(None),
            pressed_mouse_button: Cell::new(MouseButtons::default()),
            last_mouse_button_at: Cell::new(None),
            touch_started_at: Cell::new(None),
            last_compat_tap: Cell::new(None),
            suppress_next_release: Cell::new(None),
            mouse_click: RefCell::new(None),
            native_pointer_id: Cell::new(None),
            native_drag: Rc::new(RefCell::new(None)),
            native_drag_pending: Rc::new(Cell::new(false)),
            incoming_drag: Cell::new(false),
            manipulation: Cell::new(None),
            geometry_request: Rc::new(RefCell::new(None)),
            geometry_pending: Rc::new(Cell::new(false)),
            back_enabled: Cell::new(false),
            back_handler: Rc::new(RefCell::new(None)),
            a11y_callbacks: RefCell::new(None),
            a11y_adapter: RefCell::new(None),
            key_state: RefCell::new(OhosKeyState::default()),
            active_touches: RefCell::new(HashMap::new()),
            primary_touch: Cell::new(None),
            touch_tap_candidates: RefCell::new(HashMap::new()),
            next_touch_id: Cell::new(0),
            touch_scroll: Rc::new(RefCell::new(TouchScroll::default())),
            long_press_timer: RefCell::new(None),
        })
    }

    pub(crate) fn atlas(&self) -> Option<Arc<WgpuAtlas>> {
        self.fallback_atlas.borrow().clone()
    }

    pub(crate) fn window_id(&self) -> i64 {
        self.window_id
    }

    pub(crate) fn set_appearance(&self, appearance: WindowAppearance) {
        if self.appearance.replace(appearance) != appearance {
            let mut callback = self.callbacks.borrow_mut().appearance_changed.take();
            if let Some(ref mut callback) = callback {
                callback();
            }
            self.callbacks.borrow_mut().appearance_changed = callback;
        }
    }

    pub(crate) fn refresh_display(&self) {
        let new_scale = self.display.scale_factor();
        let old_scale = self.scale.replace(new_scale);
        if old_scale != new_scale {
            // Native geometry stays in physical pixels when a display snapshot
            // arrives after the configuration event.
            let mut bounds = self.bounds.borrow_mut();
            let ratio = old_scale / new_scale;
            bounds.size = size(bounds.size.width * ratio, bounds.size.height * ratio);
            bounds.origin *= ratio;
        }
        self.refresh_keyboard_overlap_device_px();
        self.emit_resize_callback();
        self.refresh_insets();
        let mut callback = self.callbacks.borrow_mut().display_changed.take();
        if let Some(ref mut callback) = callback {
            callback();
        }
        self.callbacks.borrow_mut().display_changed = callback;
    }

    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        let frames = self.frames.clone();
        Some(Rc::new(move || frames.request()))
    }

    fn sync_frame_delivery(&self) {
        self.frames.set_active(
            !self.closed.get() && self.visibility.get() == WindowVisibility::Visible,
            self.surface_available.get(),
        );
    }

    pub(crate) fn draw_requested_frame(&self, frame_time: Instant) {
        self.draw_frame(frame_time, FrameRequestSource::LocalSchedule);
    }

    pub(crate) fn draw_native_frame(&self, frame_time: Instant) {
        self.draw_frame(frame_time, FrameRequestSource::NativeCallback);
    }

    fn draw_frame(&self, frame_time: Instant, signal_source: FrameRequestSource) {
        if self.closed.get()
            || !self.surface_available.get()
            || self.visibility.get() != WindowVisibility::Visible
            || self.restore_pending.get()
        {
            return;
        }
        let momentum_input = self.touch_scroll.borrow_mut().tick(frame_time);
        if let Some(input) = momentum_input {
            self.dispatch_input(input);
        }
        let mut callback = self.callbacks.borrow_mut().request_frame.take();
        if let Some(ref mut callback) = callback {
            callback(RequestFrameOptions {
                // GPUI clears needs_present after every draw. Retain retries and
                // lifecycle demand to present again without another layout.
                require_presentation: self.presentation.needs_presentation(),
                force_render: self.force_render.replace(false),
                signal_at: None,
                signal_source,
            });
        }
        self.callbacks.borrow_mut().request_frame = callback;
        self.request_touch_momentum_frame();
    }

    pub(crate) fn has_window_controls(&self) -> bool {
        !self.closed.get()
            && !self.released.get()
            && (self.window_id != 0 || self.form_factor.is_desktop() || self.free_window.get())
    }

    pub(crate) fn notify_button_layout_changed(&self) {
        if self.closed.get() || self.released.get() {
            return;
        }
        let window_id = self.window_id;
        let window_index = self.window_index.clone();
        let released = self.released.clone();
        self.foreground_executor
            .spawn(async move {
                if released.get() {
                    return;
                }
                let window = window_index
                    .upgrade()
                    .and_then(|index| index.borrow().get(&window_id).and_then(Weak::upgrade));
                let Some(window) = window else {
                    return;
                };
                let callbacks = {
                    let window = window.borrow();
                    if window.closed.get()
                        || window.released.get()
                        || !Rc::ptr_eq(&window.released, &released)
                    {
                        return;
                    }
                    window.callbacks.clone()
                };
                let mut callback = callbacks.borrow_mut().button_layout_changed.take();
                if let Some(ref mut callback) = callback {
                    callback();
                }
                callbacks.borrow_mut().button_layout_changed = callback;
            })
            .detach();
    }

    pub(crate) fn apply_window_status(&self, status: i32) -> bool {
        // OHOS WindowStatusType: FULL_SCREEN=1, MAXIMIZE=2,
        // MINIMIZE=3, FLOATING=4, SPLIT_SCREEN=5.
        if !(1..=5).contains(&status) {
            warn!(
                "Unknown OHOS window status {status} for window {}",
                self.window_id
            );
            return false;
        }
        // Minimization changes visibility, not the mode to restore or persist.
        if status == 3 {
            self.update_visibility(WindowVisibility::Hidden);
            return false;
        }
        let free_changed = super::device::is_free_window_status(status)
            .is_some_and(|is_free| self.free_window.replace(is_free) != is_free);
        let was_maximized = self.maximized.replace(status == 2);
        let was_fullscreen = self.fullscreen.replace(status == 1);
        self.update_visibility(WindowVisibility::Visible);
        if free_changed
            && self.free_window.get()
            && self.requested_decorations.get() == WindowDecorations::Client
            && self.decorations.get() != WindowDecorations::Client
        {
            self.request_decorations(WindowDecorations::Client);
        }
        if free_changed
            || was_maximized != self.maximized.get()
            || was_fullscreen != self.fullscreen.get()
        {
            self.notify_window_mode_changed();
        }
        free_changed
    }

    fn notify_window_mode_changed(&self) {
        if self.closed.get() || self.released.get() {
            return;
        }
        // Native mode changes can finish after the resize event. GPUI observes
        // bounds changes to refresh title-bar controls and persist the window mode.
        self.viewport.reset();
        self.emit_resize_callback();
        self.force_render.set(true);
        self.schedule_frame();
    }

    fn dispatch_input_with_callbacks(
        callbacks: &Rc<RefCell<WindowCallbacks>>,
        input: PlatformInput,
    ) -> crate::DispatchEventResult {
        let mut callback = callbacks.borrow_mut().input.take();
        let mut result = crate::DispatchEventResult::default();
        if let Some(ref mut cb) = callback {
            result = cb(input);
        }
        callbacks.borrow_mut().input = callback;
        result
    }

    fn point_from_device_pixels(&self, x: f32, y: f32) -> Point<Pixels> {
        let scale = (*self.scale.borrow()).max(f32::EPSILON);
        point(px(x / scale), px(y / scale))
    }

    fn pointer_position_from_device_pixels(&self, x: f32, y: f32) -> Point<Pixels> {
        let logical = self.point_from_device_pixels(x, y);
        #[cfg(feature = "native-input")]
        super::pointer_position::NativePointerPosition::record(
            self.handle.window_id(),
            logical,
            point(x, y),
            *self.scale.borrow(),
        );
        logical
    }

    fn pointer_position_from_arkui(&self, pointer: PointerInputData) -> Point<Pixels> {
        self.pointer_position_from_device_pixels(pointer.x, pointer.y)
    }

    fn allocate_touch_id(&self) -> TouchId {
        let raw_id = self.next_touch_id.get();
        self.next_touch_id
            .set(raw_id.checked_add(1).expect("touch ID exhausted"));
        TouchId(raw_id)
    }

    fn dispatch_raw_touch_point(
        &self,
        raw_id: i32,
        x: f32,
        y: f32,
        screen_position: (f32, f32),
        force: f32,
        phase: TouchPhase,
        allow_compatibility_tap: bool,
    ) {
        if phase == TouchPhase::Started {
            self.invalidate_pending_edit();
            self.touch_started_at.set(Some(Instant::now()));
        }
        // XComponent's touch screen coordinates do not follow a moving window
        // reliably. Resolve the local point against its current native origin
        // when starting a touch or driving an active window manipulation.
        let needs_native_screen_origin = phase == TouchPhase::Started
            || (phase == TouchPhase::Moved
                && self.primary_touch.get() == Some(raw_id)
                && self.manipulation.get().is_some());
        let screen_position = if needs_native_screen_origin {
            match self.app.borrow().as_ref().and_then(|app| {
                app.with_xcomponent_for(self.window_id, |component| {
                    component.raw().layout_position_in_screen()
                })
            }) {
                Some(Ok(origin)) => (origin.x as f32 + x, origin.y as f32 + y),
                Some(Err(error)) => {
                    warn!("Failed to locate OHOS touch surface: {error}");
                    screen_position
                }
                None => screen_position,
            }
        } else {
            screen_position
        };
        let position = self.point_from_device_pixels(x, y);
        if phase == TouchPhase::Started {
            self.native_pointer_id.set(Some(raw_id));
        }
        let id = if phase == TouchPhase::Started {
            let id = self.allocate_touch_id();
            let mut active_touches = self.active_touches.borrow_mut();
            if active_touches.is_empty() {
                self.primary_touch.set(Some(raw_id));
                self.manipulation.set(None);
                if let Some(subscription) = self.move_subscription.borrow().as_ref() {
                    subscription.cancel();
                }
                self.pointer_screen_position.set(None);
                self.record_screen_pointer(screen_position.0, screen_position.1);
                self.touch_tap_candidates.borrow_mut().insert(
                    raw_id,
                    TouchTapCandidate {
                        start_position: position,
                        // Capture this before GPUI translates the touch into its
                        // compatibility mouse gesture. A newly focused input will
                        // use the regular FocusGained path; only an input that was
                        // already focused needs an explicit IME reopen request.
                        started_in_text_input: self.pointer_targets_text_input(position),
                    },
                );
            } else {
                self.touch_tap_candidates.borrow_mut().clear();
            }
            active_touches.insert(raw_id, id);
            id
        } else {
            let Some(id) = self.active_touches.borrow().get(&raw_id).copied() else {
                return;
            };
            id
        };
        if self.primary_touch.get() == Some(raw_id) && phase == TouchPhase::Moved {
            self.record_screen_pointer(screen_position.0, screen_position.1);
        }
        self.pointer_position.set(Some(position));
        let should_reopen_keyboard = match phase {
            TouchPhase::Moved => {
                if self.touch_moved_beyond_tap_slop(raw_id, position) {
                    self.touch_tap_candidates.borrow_mut().remove(&raw_id);
                }
                false
            }
            TouchPhase::Ended => self
                .touch_tap_candidates
                .borrow_mut()
                .remove(&raw_id)
                .is_some_and(|candidate| {
                    candidate.started_in_text_input
                        && (position - candidate.start_position).magnitude()
                            <= f64::from(GestureTuning::default().touch_slop)
                }),
            TouchPhase::Cancelled => {
                self.touch_tap_candidates.borrow_mut().remove(&raw_id);
                false
            }
            TouchPhase::Started => false,
        };
        if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.active_touches.borrow_mut().remove(&raw_id);
            if self.primary_touch.get() == Some(raw_id) {
                self.primary_touch.set(None);
                self.manipulation.set(None);
                if let Some(subscription) = self.move_subscription.borrow().as_ref() {
                    subscription.cancel();
                }
            }
        }
        let now = Instant::now();
        let tap_allowed = self.touch_scroll.borrow_mut().relay(
            TouchEvent {
                id,
                phase,
                position,
                predicted_position: None,
                force: force.is_finite().then(|| force.clamp(0.0, 1.0)),
            },
            now,
            allow_compatibility_tap,
            |input| {
                // Geometry is already driven by absolute touch coordinates.
                // A compatibility move must not restart the title-bar drag.
                if matches!(input, PlatformInput::MouseMove(_)) && self.manipulation.get().is_some()
                {
                    crate::DispatchEventResult::default()
                } else {
                    self.dispatch_input(input)
                }
            },
        );
        if tap_allowed {
            self.last_compat_tap.set(Some((now, position)));
        }
        if phase == TouchPhase::Started
            && self.primary_touch.get() == Some(raw_id)
            && matches!(self.hit_window_control(), Some(WindowControlArea::Drag))
            && self
                .touch_scroll
                .borrow_mut()
                .capture_window_control(id, now, |input| self.dispatch_input(input))
        {
            self.start_window_move();
        }
        self.update_touch_long_press_timer();
        self.request_touch_momentum_frame();
        if should_reopen_keyboard && tap_allowed {
            // Focusing an already-focused GPUI input is otherwise a no-op.
            // OHOS can dismiss its IME without changing that focus, so an
            // editable tap must always be treated as a fresh show request.
            // Do not inspect DispatchEventResult::default_prevented here:
            // focusable GPUI elements set it specifically to stop ancestors
            // from stealing focus.
            self.request_keyboard();
        }
        if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.touch_started_at.set(None);
        }
    }

    fn request_touch_momentum_frame(&self) {
        if self.touch_scroll.borrow().has_momentum() {
            self.frames.request();
        }
    }

    fn update_touch_long_press_timer(&self) {
        let pending = self
            .touch_scroll
            .borrow()
            .pending_long_press(Instant::now());
        let Some((id, delay)) = pending else {
            self.long_press_timer.borrow_mut().take();
            return;
        };
        if self
            .long_press_timer
            .borrow()
            .as_ref()
            .is_some_and(|(scheduled, _)| *scheduled == id)
        {
            return;
        }
        let touch_scroll = self.touch_scroll.clone();
        let callbacks = self.callbacks.clone();
        let task = self.foreground_executor.spawn(async move {
            smol::Timer::after(delay).await;
            touch_scroll.borrow_mut().offer_long_press(id, |input| {
                Self::dispatch_input_with_callbacks(&callbacks, input)
            });
        });
        *self.long_press_timer.borrow_mut() = Some((id, task));
    }

    fn cancel_touch_input(&self) {
        self.long_press_timer.borrow_mut().take();
        self.touch_scroll
            .borrow_mut()
            .cancel(|input| self.dispatch_input(input));
        self.active_touches.borrow_mut().clear();
        self.primary_touch.set(None);
        self.manipulation.set(None);
        if let Some(subscription) = self.move_subscription.borrow().as_ref() {
            subscription.cancel();
        }
        self.touch_tap_candidates.borrow_mut().clear();
    }

    fn touch_moved_beyond_tap_slop(&self, raw_id: i32, position: Point<Pixels>) -> bool {
        self.touch_tap_candidates
            .borrow()
            .get(&raw_id)
            .is_some_and(|candidate| {
                (position - candidate.start_position).magnitude()
                    > f64::from(GestureTuning::default().touch_slop)
            })
    }

    fn pointer_targets_text_input(&self, position: Point<Pixels>) -> bool {
        let Some(mut handler) = self.input_handler.borrow_mut().take() else {
            return false;
        };
        let targets_text_input = handler.query_accepts_text_input()
            && handler
                .element_bounds()
                .is_some_and(|bounds| bounds.contains(&position));
        *self.input_handler.borrow_mut() = Some(handler);
        targets_text_input
    }

    fn dispatch_raw_touch_data(&self, event: &TouchEventData, point: &TouchPointData) {
        if point.event_tool_type == TouchPointTool::Mouse {
            return;
        }
        let phase = match event.event_type {
            OhosTouchEvent::Down => TouchPhase::Started,
            OhosTouchEvent::Move => TouchPhase::Moved,
            OhosTouchEvent::Up => TouchPhase::Ended,
            OhosTouchEvent::Cancel => TouchPhase::Cancelled,
            OhosTouchEvent::Unknown => return,
        };
        // 2-in-1 PCs report the mouse as touch points too, but with an
        // `Unknown` tool type instead of `Mouse`. The mouse callback already
        // delivers the authoritative press/move/release; replaying the
        // duplicate touch would synthesize a compatibility tap on release,
        // whose mouse-down dismisses a menu that the real press just opened.
        // Drop duplicate moves and skip the tap when a mouse event belongs to
        // this contact (either order of the touch/mouse callbacks).
        let duplicate_mouse = self.touch_duplicates_mouse(event);
        if duplicate_mouse && phase == TouchPhase::Moved {
            return;
        }
        self.dispatch_raw_touch_point(
            point.id,
            point.x,
            point.y,
            (point.screen_x, point.screen_y),
            point.force,
            phase,
            !duplicate_mouse,
        );
    }

    fn touch_duplicates_mouse(&self, event: &TouchEventData) -> bool {
        // A real mouse click is reported as a contact plus a button
        // transition, in either order. Compare the button transition with the
        // start of this contact instead of "any mouse event recently": a
        // pointer move immediately before a finger tap is not a duplicate.
        let mouse_belongs_to_contact = self.touch_started_at.get().is_some_and(|started| {
            const WINDOW: Duration = Duration::from_millis(200);
            let earliest = started.checked_sub(WINDOW).unwrap_or(started);
            self.last_mouse_button_at
                .get()
                .is_some_and(|at| at >= earliest && at <= started + WINDOW)
        });
        mouse_belongs_to_contact
            && event.touch_points.iter().all(|point| {
                matches!(
                    point.event_tool_type,
                    TouchPointTool::Unknown | TouchPointTool::Mouse
                )
            })
    }

    /// Whether this mouse press is the authoritative copy of a compatibility
    /// click that a touch contact already delivered. OHOS does not guarantee
    /// that the mouse callback arrives first, so skip the later copy and its
    /// paired release. Two deliberate clicks cannot land this close together.
    fn compat_tap_precedes_mouse(&self, button: MouseButton, position: Point<Pixels>) -> bool {
        let Some((at, tap_position)) = self.last_compat_tap.get() else {
            return false;
        };
        if at.elapsed() > Duration::from_millis(150) || (position - tap_position).magnitude() > 2.0
        {
            return false;
        }
        self.suppress_next_release.set(Some((button, at)));
        true
    }

    fn dispatch_raw_touch_event(&self, event: &TouchEventData) {
        if !event.touch_points.is_empty()
            && event
                .touch_points
                .iter()
                .all(|point| point.event_tool_type == TouchPointTool::Mouse)
        {
            return;
        }
        match event.event_type {
            OhosTouchEvent::Move | OhosTouchEvent::Cancel if !event.touch_points.is_empty() => {
                for point in &event.touch_points {
                    self.dispatch_raw_touch_data(event, point);
                }
            }
            OhosTouchEvent::Down | OhosTouchEvent::Up => {
                if let Some(point) = event.touch_points.iter().find(|point| point.id == event.id) {
                    self.dispatch_raw_touch_data(event, point);
                } else {
                    let phase = if event.event_type == OhosTouchEvent::Down {
                        TouchPhase::Started
                    } else {
                        TouchPhase::Ended
                    };
                    self.dispatch_raw_touch_point(
                        event.id,
                        event.x,
                        event.y,
                        (event.screen_x, event.screen_y),
                        event.force,
                        phase,
                        !self.touch_duplicates_mouse(event),
                    );
                }
            }
            OhosTouchEvent::Move | OhosTouchEvent::Cancel => {
                let phase = if event.event_type == OhosTouchEvent::Move {
                    TouchPhase::Moved
                } else {
                    TouchPhase::Cancelled
                };
                self.dispatch_raw_touch_point(
                    event.id,
                    event.x,
                    event.y,
                    (event.screen_x, event.screen_y),
                    event.force,
                    phase,
                    !self.touch_duplicates_mouse(event),
                );
            }
            OhosTouchEvent::Unknown => {}
        }
    }

    fn dispatch_scroll(
        &self,
        position: Point<Pixels>,
        delta: ScrollDelta,
        touch_phase: TouchPhase,
    ) -> crate::DispatchEventResult {
        Self::dispatch_input_with_callbacks(
            &self.callbacks,
            PlatformInput::Scroll(ScrollEvent {
                position,
                delta,
                modifiers: self.key_state.borrow().modifiers(),
                touch_phase,
                source: ScrollSource::Pointer,
            }),
        )
    }

    fn mouse_button(button: OhosMouseButton) -> Option<MouseButton> {
        match button {
            OhosMouseButton::NoneButton => None,
            OhosMouseButton::LeftButton => Some(MouseButton::Left),
            OhosMouseButton::RightButton => Some(MouseButton::Right),
            OhosMouseButton::MiddleButton => Some(MouseButton::Middle),
            OhosMouseButton::BackButton => Some(MouseButton::Navigate(NavigationDirection::Back)),
            OhosMouseButton::ForwardButton => {
                Some(MouseButton::Navigate(NavigationDirection::Forward))
            }
        }
    }

    fn set_native_text_input_active(&self, active: bool) {
        if !self.input_ready.get() {
            return;
        }
        if let Some(app) = self.app.borrow().as_ref() {
            if let Err(error) =
                app.set_text_input_active_for(self.window_id, active && self.surface_focused.get())
            {
                warn!("Failed to update OHOS text input focus: {error}");
            }
        }
    }

    fn restore_text_input(&self) {
        if !self.active.get() || !self.input_ready.get() || !self.surface_focused.get() {
            return;
        }
        let handler = self.input_handler.borrow_mut().take();
        if let Some(mut handler) = handler {
            let accepts_text = handler.query_accepts_text_input();
            *self.input_handler.borrow_mut() = Some(handler);
            if accepts_text {
                self.request_keyboard();
                self.schedule_ime_text_update();
            }
        }
    }

    fn show_keyboard_if_needed(&self) {
        if !self.keyboard_visible.get() && !self.ime_retry_pending.get() {
            self.request_keyboard();
        }
    }

    fn request_keyboard(&self) {
        self.set_native_text_input_active(true);
        self.cancel_keyboard_retry();
        // GPUI can focus an editor before its native window gains focus. Attaching
        // here would send IME text to that editor while shortcuts reach the old window.
        if self.active.get() && !self.input_ready.get() && self.surface_focused.get() {
            // The cached readiness can lag the native editor if the
            // TextInputReadyChanged event raced window creation; re-query instead of
            // silently dropping every later attach.
            let ready = self
                .app
                .borrow()
                .as_ref()
                .is_some_and(|app| app.is_text_input_ready_for(self.window_id));
            self.input_ready.set(ready);
        }
        if !self.active.get() || !self.input_ready.get() || !self.surface_focused.get() {
            return;
        }
        self.request_keyboard_attempt(0, self.ime_retry_generation.get(), true);
    }

    fn cancel_keyboard_retry(&self) {
        self.ime_retry_pending.set(false);
        self.ime_recovery_pending.set(false);
        self.ime_retry_generation
            .set(self.ime_retry_generation.get().wrapping_add(1));
    }

    fn request_keyboard_attempt(&self, attempt: usize, generation: u64, show_keyboard: bool) {
        if self.ime_retry_generation.get() != generation
            || self.released.get()
            || self.closed.get()
            || !self.active.get()
            || !self.input_ready.get()
            || !self.surface_focused.get()
        {
            return;
        }
        self.ime_retry_pending.set(false);
        if show_keyboard {
            self.apply_ime_action(false);
        }
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        #[cfg(feature = "api-26")]
        if super::device::sdk_api_version() >= 26 {
            if let Err(error) = app.with_ime_for(self.window_id, |ime| {
                ime.set_consume_key_events(true);
            }) {
                self.finish_keyboard_attempt(
                    attempt,
                    generation,
                    show_keyboard,
                    Err(error.to_string()),
                );
                return;
            }
        }
        let result = if show_keyboard {
            app.try_show_keyboard_for(self.window_id)
        } else {
            app.try_attach_ime_for(self.window_id)
        };
        self.finish_keyboard_attempt(
            attempt,
            generation,
            show_keyboard,
            result.map_err(|error| error.to_string()),
        );
    }

    fn finish_keyboard_attempt(
        &self,
        attempt: usize,
        generation: u64,
        show_keyboard: bool,
        result: Result<(), String>,
    ) {
        match result {
            Ok(()) => {
                self.ime_recovery_pending.set(false);
                if show_keyboard {
                    self.keyboard_visible.set(true);
                }
                if attempt > 0 || !show_keyboard {
                    info!(
                        "OHOS IME attached for window {} after retry {attempt}",
                        self.window_id
                    );
                }
                self.apply_ime_action(true);
                self.schedule_ime_text_update();
            }
            Err(error) => {
                if show_keyboard {
                    self.keyboard_visible.set(false);
                }
                warn!(
                    "Failed to attach OHOS IME for window {} (attempt {}): {error}",
                    self.window_id,
                    attempt.saturating_add(1),
                );
                // First-boot IME startup can outlast the fast retries. Keep
                // recovering while focused, backing off to avoid repeated IPC stalls.
                let delay_for = |attempt: usize| {
                    [250, 1000, 2000, 5000, 10000]
                        .get(attempt)
                        .copied()
                        .unwrap_or(30000)
                };
                let Some(window) = self
                    .window_index
                    .upgrade()
                    .and_then(|index| index.borrow().get(&self.window_id).cloned())
                else {
                    return;
                };
                self.ime_retry_pending.set(true);
                let executor = self.foreground_executor.clone();
                executor
                    .spawn(async move {
                        let mut attempt = attempt;
                        loop {
                            smol::Timer::after(Duration::from_millis(delay_for(attempt))).await;
                            let Some(window) = window.upgrade() else {
                                return;
                            };
                            let window = window.borrow();
                            if window.released.get()
                                || window.closed.get()
                                || !window.active.get()
                                || !window.input_ready.get()
                                || !window.surface_focused.get()
                                || window.ime_retry_generation.get() != generation
                            {
                                return;
                            }
                            window.ime_retry_pending.set(false);
                            let accepts_text = {
                                let mut handler = window.input_handler.borrow_mut().take();
                                let accepts_text = handler
                                    .as_mut()
                                    .map(|handler| handler.query_accepts_text_input())
                                    .unwrap_or(false);
                                *window.input_handler.borrow_mut() = handler;
                                accepts_text
                            };
                            if window.ime_retry_generation.get() != generation {
                                return;
                            }
                            if accepts_text {
                                window.request_keyboard_attempt(
                                    attempt.saturating_add(1),
                                    generation,
                                    show_keyboard,
                                );
                                return;
                            }
                            // Focus can move to a text input between the failure and
                            // this timer; keep the recovery loop alive while focused.
                            window.ime_retry_pending.set(true);
                            attempt = attempt.saturating_add(1);
                        }
                    })
                    .detach();
            }
        }
    }

    fn update_ime(&self, recover: bool, update: impl FnOnce(&IME) -> ImeResult<()>) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        match app.with_ime_for(self.window_id, update) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!(
                    "Failed to update OHOS IME for window {}: {error}",
                    self.window_id
                );
                // DETACHED and CLIENT_NOT_EDITABLE invalidate the binding's proxy.
                // Other errors cannot be repaired by taking ownership of the IME.
                if recover && matches!(error.code(), 12_800_009 | 12_800_016) {
                    self.schedule_ime_recovery();
                }
            }
            Err(error) => warn!(
                "OHOS IME unavailable for window {}: {error}",
                self.window_id
            ),
        }
    }

    fn schedule_ime_recovery(&self) {
        if self.ime_recovery_pending.replace(true) {
            return;
        }
        let generation = self.ime_retry_generation.get();
        let Some(window) = self
            .window_index
            .upgrade()
            .and_then(|index| index.borrow().get(&self.window_id).cloned())
        else {
            self.ime_recovery_pending.set(false);
            return;
        };
        // Cursor notifications run during GPUI rendering. Query text focus only
        // after that borrow ends, and never let a stale window reclaim the IME.
        self.foreground_executor
            .spawn(async move {
                let Some(window) = window.upgrade() else {
                    return;
                };
                let window = window.borrow();
                if window.ime_retry_generation.get() != generation {
                    return;
                }
                if window.released.get()
                    || window.closed.get()
                    || !window.active.get()
                    || !window.input_ready.get()
                    || !window.surface_focused.get()
                {
                    window.ime_recovery_pending.set(false);
                    return;
                }
                let handler = window.input_handler.borrow_mut().take();
                let Some(mut handler) = handler else {
                    window.ime_recovery_pending.set(false);
                    return;
                };
                let accepts_text = handler.query_accepts_text_input();
                *window.input_handler.borrow_mut() = Some(handler);
                if accepts_text && window.ime_retry_generation.get() == generation {
                    let show_keyboard = window.keyboard_visible.get();
                    info!(
                        "Recovering OHOS IME for window {} (show_keyboard={show_keyboard})",
                        window.window_id
                    );
                    if show_keyboard {
                        window.ime_recovery_pending.set(false);
                    }
                    window.request_keyboard_attempt(0, generation, show_keyboard);
                } else if window.ime_retry_generation.get() == generation {
                    window.ime_recovery_pending.set(false);
                }
            })
            .detach();
    }

    fn configure_text_input(&self, configuration: TextInputConfiguration) {
        // Configuration changes during input dispatch must not mutably borrow
        // OhosWindow. The native editor also retains hints while detached.
        // The SDK has no autocorrect, capitalization, or suggestion flags.
        if self.ime_action.replace(configuration.input_action) != configuration.input_action {
            self.apply_ime_action(true);
        }
    }

    fn apply_ime_action(&self, recover: bool) {
        let action = self.ime_action.get();
        let enter_key = match action {
            TextInputAction::Unspecified => EnterKey::Unspecified,
            TextInputAction::Enter => EnterKey::Newline,
            TextInputAction::Done => EnterKey::Done,
            TextInputAction::Go => EnterKey::Go,
            TextInputAction::Next => EnterKey::Next,
            TextInputAction::Previous => EnterKey::Previous,
            TextInputAction::Search => EnterKey::Search,
            TextInputAction::Send => EnterKey::Send,
        };
        let input_type = if action == TextInputAction::Enter {
            InputType::Multiline
        } else {
            InputType::Text
        };
        self.update_ime(recover, |ime| {
            ime.try_update_configuration(enter_key, input_type)
        });
    }

    fn hide_keyboard_if_needed(&self) {
        self.cancel_keyboard_retry();
        if self.keyboard_visible.replace(false) {
            if let Some(app) = self.app.borrow().as_ref() {
                app.hide_keyboard_for(self.window_id);
            }
        }
    }

    fn detach_ime(&self) {
        if let Some(app) = self.app.borrow().as_ref() {
            if let Err(error) = app.try_detach_ime_for(self.window_id) {
                warn!(
                    "Failed to detach OHOS IME for window {}: {error}",
                    self.window_id
                );
            }
        }
    }

    fn finish_native_keyboard_show(&self) {
        self.cancel_keyboard_retry();
        self.keyboard_visible.set(true);
    }

    fn notify_keyboard_hidden_by_user_if_needed(&self) {
        // A hide during pending recovery changes what we restore, but hardware
        // text input must still regain its connection without showing a keyboard.
        if self.keyboard_visible.replace(false) && !self.ime_recovery_pending.get() {
            self.cancel_keyboard_retry();
        }
    }

    fn keyboard_inset_for_overlap(&self, overlap_device_px: i32) -> Pixels {
        const MIN_CONTENT_HEIGHT: f32 = 64.0;

        let overlap = overlap_device_px.max(0) as f32;
        let scale = self.scale_factor().max(1.0);
        let mut inset = (overlap / scale).max(0.0);
        let bounds_height = self.bounds.borrow().size.height.as_f32().max(0.0);
        let max_inset = (bounds_height - MIN_CONTENT_HEIGHT).max(0.0);
        if inset > max_inset {
            inset = max_inset;
        }
        px(inset)
    }

    fn keyboard_overlap_from_avoid_area_device_px(&self) -> Option<i32> {
        let app_ref = self.app.borrow();
        let app = app_ref.as_ref()?;

        let content_rect = app.content_rect_for(self.window_id);
        if content_rect.height <= 0 {
            return Some(0);
        }

        // Use actual XComponent rect as layout basis for keyboard-avoid computation.
        // This keeps behavior correct for embedded/non-fullscreen XComponents.
        let layout_top = content_rect.top;
        let layout_height = content_rect.height.max(0);
        if layout_height <= 0 {
            return Some(0);
        }
        let window_rect = app.window_rect_for(self.window_id);
        let window_top = window_rect.top;
        let window_bottom = window_rect.top.saturating_add(window_rect.height.max(0));

        let keyboard_area = app.avoid_area_for(self.window_id, AvoidAreaType::Keyboard);
        let system_area = app.avoid_area_for(self.window_id, AvoidAreaType::System);
        let system_gesture_area = app.avoid_area_for(self.window_id, AvoidAreaType::SystemGesture);
        let navigation_indicator_area =
            app.avoid_area_for(self.window_id, AvoidAreaType::NavigationIndicator);

        // OHOS avoid-area bottomRect coordinates are in window/screen space.
        // XComponent's content_rect can be reported in safe-content coordinates on some devices.
        // For root full-width layouts, infer top-safe offset so intersection uses a consistent space.
        let root_layout_width_matches_window = content_rect.width > 0
            && window_rect.width > 0
            && (content_rect.width - window_rect.width).abs() <= 1;
        let can_infer_root_safe_top = layout_top == 0
            && layout_height > 0
            && window_rect.height >= layout_height
            && root_layout_width_matches_window;
        let inferred_outside_bottom_safe = if can_infer_root_safe_top {
            let bottom_safe_overlap = |area: Option<openharmony_ability::AvoidArea>| -> i32 {
                let Some(area) = area else {
                    return 0;
                };
                if !area.visible || area.bottom_rect.height <= 0 {
                    return 0;
                }
                let start = area.bottom_rect.top;
                let end = area
                    .bottom_rect
                    .top
                    .saturating_add(area.bottom_rect.height.max(0));
                if end < window_bottom {
                    return 0;
                }
                (window_bottom - start)
                    .max(0)
                    .min(area.bottom_rect.height.max(0))
            };

            bottom_safe_overlap(system_area)
                .max(bottom_safe_overlap(system_gesture_area))
                .max(bottom_safe_overlap(navigation_indicator_area))
        } else {
            0
        };
        let inferred_top_safe = if can_infer_root_safe_top {
            (window_rect.height.max(0) - layout_height - inferred_outside_bottom_safe).max(0)
        } else {
            0
        };
        // Convert GPUI layout bounds to screen space before intersection.
        let layout_top_screen = window_top
            .saturating_add(inferred_top_safe)
            .saturating_add(layout_top);
        let layout_bottom_screen = layout_top_screen.saturating_add(layout_height);

        let keyboard_avoid_visible = keyboard_area.map(|a| a.visible).unwrap_or(false);
        // A show request does not prove that an on-screen keyboard occupies
        // the window. Without keyboard geometry, system/navigation bars are
        // only safe-area insets; counting them here shrinks the viewport twice.
        if keyboard_area.is_none_or(|area| area.bottom_rect.height <= 0) {
            return Some(0);
        }
        if !(self.keyboard_visible.get() || keyboard_avoid_visible) {
            return Some(0);
        }

        // Keyboard event only determines show/hide state.
        // Actual inset is derived from avoid-area geometry.
        // When keyboard is shown, include bottom occlusion union of:
        // - Keyboard area
        // - System bottom area (3-button navigation etc.)
        // - System gesture area
        // - Navigation indicator area
        // This prevents under-subtraction where keyboard area excludes nav area.
        let mut intervals: Vec<(i32, i32)> = Vec::with_capacity(4);
        let mut push_bottom_overlap_interval =
            |area: openharmony_ability::AvoidArea, require_visible: bool| {
                if area.bottom_rect.height <= 0 {
                    return;
                }
                if require_visible && !area.visible {
                    return;
                }
                let start = area.bottom_rect.top.max(layout_top_screen);
                let end = area
                    .bottom_rect
                    .top
                    .saturating_add(area.bottom_rect.height.max(0))
                    .min(layout_bottom_screen);
                if end > start {
                    intervals.push((start, end));
                }
            };

        if let Some(area) = keyboard_area {
            push_bottom_overlap_interval(area, true);
        }
        if let Some(area) = system_area {
            push_bottom_overlap_interval(area, false);
        }
        if let Some(area) = system_gesture_area {
            push_bottom_overlap_interval(area, false);
        }
        if let Some(area) = navigation_indicator_area {
            push_bottom_overlap_interval(area, false);
        }

        if intervals.is_empty() {
            return Some(0);
        }

        intervals.sort_unstable_by_key(|(start, _)| *start);
        let mut union_overlap = 0i32;
        let mut current = intervals[0];
        for &(start, end) in intervals.iter().skip(1) {
            if start <= current.1 {
                current.1 = current.1.max(end);
            } else {
                union_overlap = union_overlap.saturating_add(current.1 - current.0);
                current = (start, end);
            }
        }
        union_overlap = union_overlap.saturating_add(current.1 - current.0);

        let geometric_overlap = union_overlap.min(layout_height.max(0));
        let clamped_overlap = geometric_overlap;

        Some(clamped_overlap)
    }

    fn refresh_keyboard_overlap_device_px(&self) -> bool {
        let previous_overlap = self.keyboard_overlap_device_px.get();
        let next_overlap = self
            .keyboard_overlap_from_avoid_area_device_px()
            .unwrap_or(0)
            .max(0);
        if previous_overlap != next_overlap {
            self.keyboard_overlap_device_px.set(next_overlap);
            true
        } else {
            false
        }
    }

    fn current_safe_area_insets(&self) -> WindowInsets {
        let Some(app) = self.app.borrow().clone() else {
            return WindowInsets::default();
        };
        let content = app.content_rect_for(self.window_id);
        let window = app.window_rect_for(self.window_id);
        let mut top = 0_i32;
        let mut right = 0_i32;
        let mut bottom = 0_i32;
        let mut left = 0_i32;
        for kind in [
            AvoidAreaType::System,
            AvoidAreaType::Cutout,
            AvoidAreaType::NavigationIndicator,
        ] {
            if let Some(area) = app.avoid_area_for(self.window_id, kind) {
                if area.visible {
                    top = top.max(area.top_rect.height.max(0));
                    right = right.max(area.right_rect.width.max(0));
                    bottom = bottom.max(area.bottom_rect.height.max(0));
                    left = left.max(area.left_rect.width.max(0));
                }
            }
        }

        let scale = self.scale_factor().max(1.0);
        let applied_top = content.top.max(0);
        let applied_left = content.left.max(0);
        let applied_right = (window.width - content.width - content.left).max(0);
        let applied_bottom = (window.height - content.height - content.top).max(0);
        WindowInsets {
            safe_area: Edges {
                top: px((top - applied_top).max(0) as f32 / scale),
                right: px((right - applied_right).max(0) as f32 / scale),
                bottom: px((bottom - applied_bottom).max(0) as f32 / scale),
                left: px((left - applied_left).max(0) as f32 / scale),
            },
            // Keyboard overlap is represented by visual_viewport_bounds.
            ime: Edges::default(),
            corner_radii: Corners::default(),
        }
    }

    fn refresh_insets(&self) {
        let next = self.current_safe_area_insets();
        if *self.insets.borrow() == next {
            return;
        }
        *self.insets.borrow_mut() = next.clone();
        let mut callback = self.callbacks.borrow_mut().insets_changed.take();
        if let Some(ref mut callback) = callback {
            callback(next);
        }
        self.callbacks.borrow_mut().insets_changed = callback;
    }

    fn update_visibility(&self, next: WindowVisibility) {
        if next != WindowVisibility::Visible {
            self.cancel_activation();
            self.cancel_touch_input();
            self.presentation.request();
        }
        let changed = self.visibility.replace(next) != next;
        self.sync_frame_delivery();
        if !changed {
            return;
        }
        let mut callback = self.callbacks.borrow_mut().visibility_change.take();
        if let Some(ref mut callback) = callback {
            callback(next);
        }
        self.callbacks.borrow_mut().visibility_change = callback;
    }

    pub(crate) fn back_handler_state(&self) -> (bool, BackHandler) {
        (self.back_enabled.get(), self.back_handler.clone())
    }

    fn set_hovered(&self, hovered: bool) {
        if !hovered {
            // XComponent can lose the release after the pointer leaves its window.
            // Cancel the press before an unpressed re-entry can move the window.
            self.cancel_mouse_input();
        }
        if self.hovered.replace(hovered) == hovered {
            return;
        }
        if hovered {
            self.cursor_style.set(None);
        } else {
            self.dispatch_input(PlatformInput::MouseExited(MouseExitEvent {
                position: self.mouse_position(),
                pressed_button: None,
                modifiers: self.key_state.borrow().modifiers(),
            }));
        }
        let mut callback = self.callbacks.borrow_mut().hover_status_change.take();
        if let Some(ref mut callback) = callback {
            callback(hovered);
        }
        self.callbacks.borrow_mut().hover_status_change = callback;
    }

    fn cancel_mouse_input(&self) {
        if self.primary_touch.get().is_none() {
            self.manipulation.set(None);
            if let Some(subscription) = self.move_subscription.borrow().as_ref() {
                subscription.cancel();
            }
            self.geometry_request.borrow_mut().take();
        }
        self.mouse_click.borrow_mut().take();
        for button in self.pressed_mouse_button.take().iter() {
            self.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
                button,
                position: point(px(-1.), px(-1.)),
                modifiers: self.key_state.borrow().modifiers(),
                click_count: 0,
            }));
        }
    }

    fn window_client(&self) -> Option<WindowClient> {
        let app = self.app.borrow().clone()?;
        match WindowClient::new(&app) {
            Ok(client) => Some(client),
            Err(error) => {
                warn!("Cannot access OHOS window bridge: {error}");
                None
            }
        }
    }

    fn record_screen_pointer(&self, x: f32, y: f32) {
        if x.is_finite() && y.is_finite() {
            let pointer = (x.round() as i64, y.round() as i64);
            if self.pointer_screen_position.replace(Some(pointer)) != Some(pointer) {
                self.update_manipulation(pointer);
            }
        }
    }

    fn begin_manipulation(&self, edge: Option<ResizeEdge>) {
        if self.closed.get()
            || (edge.is_some() && self.maximized.get())
            || self.fullscreen.get()
            || (edge.is_some() && !self.is_resizable)
            || (edge.is_none() && !self.is_movable)
            || (!self.pressed_mouse_button.get().contains(MouseButton::Left)
                && self.active_touches.borrow().is_empty())
        {
            return;
        }
        let Some(pointer) = self.pointer_screen_position.get() else {
            return;
        };
        let Some(initial) = self.current_native_state() else {
            return;
        };
        debug!("Geometry begin: initial={initial:?} pointer={pointer:?}");
        self.manipulation
            .set(Some(super::window_manipulation::WindowManipulation {
                initial,
                pointer,
                edge,
            }));
    }

    fn update_manipulation(&self, pointer: (i64, i64)) {
        let Some(session) = self.manipulation.get() else {
            return;
        };
        if self.primary_touch.get().is_some()
            && (pointer.0.saturating_sub(session.pointer.0) as f64)
                .hypot(pointer.1.saturating_sub(session.pointer.1) as f64)
                > f64::from(GestureTuning::default().touch_slop * self.scale_factor())
        {
            self.touch_scroll.borrow_mut().cancel_window_control_click();
        }
        if session.edge.is_none() {
            return;
        }
        let minimum = self
            .min_size
            .map(|size| {
                (
                    (size.width.as_f32() * self.scale_factor()).ceil().max(1.0) as i64,
                    (size.height.as_f32() * self.scale_factor()).ceil().max(1.0) as i64,
                )
            })
            .unwrap_or((1, 1));
        *self.geometry_request.borrow_mut() = Some(session.update(pointer, minimum));
        if self.geometry_pending.get() {
            return;
        }
        let Some(client) = self.window_client() else {
            return;
        };
        self.geometry_pending.set(true);
        let requests = self.geometry_request.clone();
        let pending = self.geometry_pending.clone();
        let window_id = self.window_id;
        let resizing = session.edge.is_some();
        self.foreground_executor
            .spawn(async move {
                loop {
                    let Some(rect) = requests.borrow_mut().take() else {
                        break;
                    };
                    let result = async {
                        if resizing {
                            client
                                .resize_window(window_id, rect.width, rect.height)
                                .await?;
                        }
                        client
                            .move_window_to(window_id, rect.left, rect.top)
                            .await?;
                        Ok::<_, anyhow::Error>(())
                    }
                    .await;
                    if let Err(error) = result {
                        warn!("OHOS window manipulation failed: {error}");
                        break;
                    }
                }
                pending.set(false);
            })
            .detach();
    }

    fn hit_window_control(&self) -> Option<WindowControlArea> {
        let mut callback = self.callbacks.borrow_mut().hit_test_window_control.take();
        let area = callback.as_mut().and_then(|callback| callback());
        self.callbacks.borrow_mut().hit_test_window_control = callback;
        area
    }

    fn current_native_state(&self) -> Option<super::window_state::NativeWindowState> {
        let app = self.app.borrow().clone()?;
        let rect = app.window_rect_for(self.window_id);
        (rect.width > 0 && rect.height > 0).then_some(super::window_state::NativeWindowState {
            left: i64::from(rect.left),
            top: i64::from(rect.top),
            width: i64::from(rect.width),
            height: i64::from(rect.height),
            maximized: self.maximized.get(),
            fullscreen: self.fullscreen.get(),
        })
    }

    fn remember_floating_state(&self) {
        if !self.maximized.get() && !self.fullscreen.get() {
            self.floating_state.set(self.current_native_state());
        }
    }

    async fn recover_floating_window(
        client: &WindowClient,
        window_id: i64,
        floating_state: Option<super::window_state::NativeWindowState>,
    ) -> napi_ohos::Result<()> {
        client.recover_window(window_id).await?;
        if let Some(state) = floating_state {
            // OHOS can snapshot the previous position before an awaited move is
            // reflected in window properties. Retain GPUI's saved floating position.
            client
                .move_window_to(window_id, state.left, state.top)
                .await?;
        }
        Ok(())
    }

    fn apply_requested_background(&self) {
        if self.requested_background.get().is_none()
            || self.background_pending.get()
            || self
                .app
                .borrow()
                .as_ref()
                .is_none_or(|app| app.native_window_for(self.window_id).is_none())
        {
            self.presentation.request();
            return;
        }
        let Some(client) = self.window_client() else {
            return;
        };
        self.background_pending.set(true);
        let requested = self.requested_background.clone();
        let current = self.background_appearance.clone();
        let pending = self.background_pending.clone();
        let generation = self.background_generation.clone();
        let expected_generation = generation.get();
        let window_id = self.window_id;
        let window_appearance = self.appearance.get();
        self.foreground_executor
            .spawn(async move {
                while generation.get() == expected_generation {
                    let Some(appearance) = requested.get() else {
                        break;
                    };
                    let color = if appearance == WindowBackgroundAppearance::Opaque {
                        if matches!(
                            window_appearance,
                            WindowAppearance::Dark | WindowAppearance::VibrantDark
                        ) {
                            0xff000000
                        } else {
                            0xffffffff
                        }
                    } else {
                        0x00000000
                    };
                    let result = client.set_window_background_color(window_id, color).await;
                    if generation.get() != expected_generation {
                        return;
                    }
                    match result {
                        Ok(()) => {
                            let radius = if appearance == WindowBackgroundAppearance::Blurred {
                                24.0
                            } else {
                                0.0
                            };
                            match client.set_window_blur(window_id, radius).await {
                                Ok(()) => current.set(appearance),
                                Err(error) => {
                                    warn!("Failed to set OHOS window backdrop blur: {error}");
                                    if appearance == WindowBackgroundAppearance::Blurred {
                                        current.set(WindowBackgroundAppearance::Transparent);
                                    }
                                }
                            }
                        }
                        Err(error) => warn!("Failed to set OHOS window background: {error}"),
                    }
                    if requested.get() == Some(appearance) {
                        break;
                    }
                }
                if generation.get() == expected_generation {
                    pending.set(false);
                }
            })
            .detach();
    }

    pub(crate) fn apply_window_options(&self) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        if app.native_window_for(self.window_id).is_none() {
            return;
        }
        let Some(client) = self.window_client() else {
            return;
        };
        let window_id = self.window_id;
        let title = self.title.borrow().clone();
        let min_size = self.min_size.map(|size| {
            (
                (size.width.as_f32() * self.scale_factor()).ceil().max(1.0) as i64,
                (size.height.as_f32() * self.scale_factor()).ceil().max(1.0) as i64,
            )
        });
        let flags =
            1 | if self.is_resizable { 2 | 8 } else { 0 } | if self.is_minimizable { 4 } else { 0 };
        let movable = self.is_movable;
        self.foreground_executor
            .spawn(async move {
                if let Err(error) = client.set_window_title(window_id, title).await {
                    warn!("Failed to apply OHOS initial title: {error}");
                }
                if let Err(error) = client.set_window_decoration_flags(window_id, flags).await {
                    warn!("Failed to apply OHOS window controls: {error}");
                }
                if let Some((width, height)) = min_size {
                    if let Err(error) = client
                        .set_window_limits(window_id, width, height, 0, 0)
                        .await
                    {
                        warn!("Failed to apply OHOS window minimum size: {error}");
                    }
                }
                // Older SDKs have no draggable switch; custom GPUI dragging still checks this flag.
                if !movable {
                    if let Err(error) = client.set_window_draggable(window_id, false).await {
                        warn!("Failed to disable OHOS native window dragging: {error}");
                    }
                }
            })
            .detach();
    }

    fn apply_restored_state(&self) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        if app.native_window_for(self.window_id).is_none() || self.restore_pending.get() {
            return;
        }
        let Some(client) = self.window_client() else {
            return;
        };
        if self.restore_state.borrow().is_none() {
            return;
        }
        self.restore_pending.set(true);
        let window_id = self.window_id;
        let fullscreen = self.fullscreen.clone();
        let maximized = self.maximized.clone();
        let pending = self.restore_pending.clone();
        let requests = self.restore_state.clone();
        let mode_operations = self.mode_operations.clone();
        let window = self
            .window_index
            .upgrade()
            .and_then(|index| index.borrow().get(&window_id).cloned());
        self.foreground_executor
            .spawn(async move {
                // GPUI applies the saved mode before Zed supplies native geometry.
                // Keep that transition and this entire restore sequence from interleaving.
                let _operation = mode_operations.lock().await;
                loop {
                    let Some(state) = requests.borrow_mut().take() else {
                        break;
                    };
                    let result = async {
                        if fullscreen.get() {
                            client.set_fullscreen(window_id, false).await?;
                        }
                        if maximized.get() {
                            client.recover_window(window_id).await?;
                        }
                        client
                            .move_window_to(window_id, state.left, state.top)
                            .await?;
                        client
                            .resize_window(window_id, state.width, state.height)
                            .await?;
                        if state.fullscreen {
                            client.set_fullscreen(window_id, true).await?;
                        } else if state.maximized {
                            client.maximize_window(window_id).await?;
                        }
                        Ok::<_, anyhow::Error>(())
                    }
                    .await;
                    match result {
                        Ok(()) => {
                            fullscreen.set(state.fullscreen);
                            maximized.set(state.maximized && !state.fullscreen);
                            if let Some(window) = window.as_ref().and_then(Weak::upgrade) {
                                let window = window.borrow();
                                window.floating_state.set(Some(
                                    super::window_state::NativeWindowState {
                                        maximized: false,
                                        fullscreen: false,
                                        ..state
                                    },
                                ));
                            }
                        }
                        Err(error) => warn!("Failed to restore OHOS native window state: {error}"),
                    }
                }
                pending.set(false);
                if let Some(window) = window.and_then(|window| window.upgrade()) {
                    let window = window.borrow();
                    let content_rect = window
                        .app
                        .borrow()
                        .as_ref()
                        .map(|app| app.content_rect_for(window_id));
                    if let Some(rect) = content_rect {
                        if rect.width > 1 && rect.height > 1 {
                            let scale = window.scale_factor();
                            window.bounds.borrow_mut().size = size(
                                px(rect.width as f32 / scale),
                                px(rect.height as f32 / scale),
                            );
                        }
                    }
                    window.refresh_keyboard_overlap_device_px();
                    window.notify_window_mode_changed();
                    window.refresh_insets();
                }
            })
            .detach();
    }

    fn request_title(&self, title: &str) {
        *self.title.borrow_mut() = title.to_owned();
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        if app.native_window_for(self.window_id).is_none() {
            return;
        }
        let Some(client) = self.window_client() else {
            return;
        };
        let window_id = self.window_id;
        let requested = title.to_owned();
        let generation = self.title_generation.clone();
        let token = generation.get().wrapping_add(1);
        generation.set(token);
        self.foreground_executor
            .spawn(async move {
                if generation.get() != token {
                    return;
                }
                if let Err(error) = client.set_window_title(window_id, requested).await {
                    warn!("Failed to set OHOS window title: {error}");
                }
            })
            .detach();
    }

    fn request_app_id(&self, _app_id: &str) {
        // Not supported on OHOS
    }

    fn request_map(&self) -> Result<()> {
        anyhow::ensure!(!self.closed.get(), "Cannot map a closed OHOS window");
        // GPUI maps every window during construction, including hidden windows.
        // Starting a workspace Ability here would show it before Zed is ready.
        if !self.show_on_creation {
            return Ok(());
        }
        self.visibility.set(WindowVisibility::Visible);
        self.sync_frame_delivery();
        let client = self
            .window_client()
            .ok_or_else(|| anyhow::anyhow!("OHOS window bridge is unavailable"))?;
        let window_id = self.window_id;
        self.foreground_executor
            .spawn(async move {
                if let Err(error) = client.show_window(window_id).await {
                    warn!("Failed to map OHOS window: {error}");
                }
            })
            .detach();
        Ok(())
    }

    fn request_resize(&self, size: Size<Pixels>) {
        let Some(client) = self.window_client() else {
            return;
        };
        let scale = self.scale_factor();
        let width = (size.width.as_f32() * scale).round() as i64;
        let height = (size.height.as_f32() * scale).round() as i64;
        if width <= 0 || height <= 0 {
            return;
        }
        let window_id = self.window_id;
        self.foreground_executor
            .spawn(async move {
                if let Err(error) = client.resize_window(window_id, width, height).await {
                    warn!("Failed to resize OHOS window {window_id}: {error}");
                }
            })
            .detach();
    }

    fn restore_cursor_after_move(&self) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        super::platform::set_cursor_visibility(
            &self.cursor_visibility,
            true,
            app,
            &self.foreground_executor,
        );
    }

    fn effective_content_size(&self) -> Size<Pixels> {
        let bounds_size = self.bounds.borrow().size;
        let bounds_height = bounds_size.height.as_f32().max(0.0);
        let keyboard_inset = self
            .keyboard_inset_for_overlap(self.keyboard_overlap_device_px.get())
            .as_f32();
        size(
            bounds_size.width,
            px((bounds_height - keyboard_inset).max(0.0)),
        )
    }

    fn emit_resize_callback(&self) {
        if self.restore_pending.get() {
            return;
        }
        let scale = *self.scale.borrow();
        let bounds_size = self.bounds.borrow().size;
        let content_size = self.effective_content_size();
        let mut callback = self.callbacks.borrow_mut().resize.take();
        self.viewport
            .publish(bounds_size, content_size, scale, &mut callback);
        self.callbacks.borrow_mut().resize = callback;
    }

    fn emit_visual_viewport_changed(&self) {
        self.emit_resize_callback();
        let mut callback = self.callbacks.borrow_mut().visual_viewport_changed.take();
        if let Some(ref mut callback) = callback {
            callback();
        }
        self.callbacks.borrow_mut().visual_viewport_changed = callback;
    }

    /// Initialize the renderer when native_window becomes available (after SurfaceCreate event).
    /// This method gets the raw_window_handle from OpenHarmonyApp's native_window.
    pub(crate) fn initialize_renderer(&self) -> Result<()> {
        let mut renderer_guard = self.renderer.borrow_mut();
        if renderer_guard.is_some() {
            // Already initialized
            return Ok(());
        }

        // Get native_window from OpenHarmonyApp - it should be available after SurfaceCreate
        let app = self.app.borrow();
        let app_ref = app.as_ref().ok_or_else(|| {
            anyhow::anyhow!("OpenHarmonyApp not available when initializing renderer")
        })?;

        // Check that native_window is available - this is required for the renderer to work.
        // The actual window handle is obtained via HasWindowHandle trait implementation.
        let _native_window = app_ref.native_window_for(self.window_id).ok_or_else(|| {
            anyhow::anyhow!(
                "native_window not available yet - SurfaceCreate event may not have been received"
            )
        })?;

        // Get the actual window size from content_rect.
        // Using the correct size is important because mismatched sizes between
        // the surface configuration and the actual native_window can cause
        // rendering issues (stretched/cropped content, black borders, etc.)
        // even though create_platform_window_surface itself won't fail.
        let content_rect = app_ref.content_rect_for(self.window_id);
        let scale = self.scale_factor();
        let device_width = if content_rect.width > 0 {
            content_rect.width as u32
        } else {
            // Fallback to bounds if content_rect is not available yet
            (self.bounds.borrow().size.width.as_f32() * scale) as u32
        };
        let device_height = if content_rect.height > 0 {
            content_rect.height as u32
        } else {
            (self.bounds.borrow().size.height.as_f32() * scale) as u32
        };

        debug!(
            "OhosWindow: Initializing renderer with size {}x{}",
            device_width, device_height
        );

        // Update window bounds to match actual content_rect (convert device px -> logical px)
        if content_rect.width > 0 && content_rect.height > 0 && !self.restore_pending.get() {
            let logical_size = size(
                px(device_width as f32 / scale),
                px(device_height as f32 / scale),
            );
            let window_rect = app_ref.window_rect_for(self.window_id);
            // The XComponent origin is relative to its window. Surface creation
            // can follow the window's last move event, so retain its screen origin.
            let logical_origin = if window_rect.width > 0 && window_rect.height > 0 {
                point(
                    px((window_rect.left + content_rect.left) as f32 / scale),
                    px((window_rect.top + content_rect.top) as f32 / scale),
                )
            } else {
                self.bounds.borrow().origin
            };
            *self.bounds.borrow_mut() = Bounds::new(logical_origin, logical_size);
        }

        let config = WgpuSurfaceConfig {
            size: Size {
                width: DevicePixels(device_width as i32),
                height: DevicePixels(device_height as i32),
            },
            transparent: true,
        };

        debug!(
            "OhosWindow: Surface config - width: {}, height: {}, transparent: true",
            device_width, device_height
        );

        // Debug: Check window handle before creating renderer
        match self.window_handle() {
            Ok(handle) => {
                debug!(
                    "OhosWindow: Window handle obtained successfully: {:?}",
                    handle.as_raw()
                );
            }
            Err(e) => {
                warn!("OhosWindow: Failed to get window handle: {:?}", e);
                return Err(anyhow::anyhow!("Window handle not available: {:?}", e));
            }
        }

        debug!("OhosWindow: Creating WgpuRenderer...");

        // Create renderer using the window's HasWindowHandle and HasDisplayHandle implementation
        // which will get the raw_window_handle from native_window
        let renderer = WgpuRenderer::new(&self.gpu_context, self, config, self.fallback_atlas.borrow().clone())
            .map_err(|e| {
                warn!("OhosWindow: WgpuRenderer::new failed: {}", e);
                anyhow::anyhow!("Failed to create Wgpu renderer: {}. Make sure native_window is available from OpenHarmonyApp.", e)
            })?;

        *self.fallback_atlas.borrow_mut() = Some(renderer.sprite_atlas().clone());
        *renderer_guard = Some(renderer);
        // Zed can open its first window after SurfaceCreate was already delivered.
        // Bootstrap frame delivery from the native surface initialized above.
        self.surface_available.set(true);
        drop(renderer_guard);
        drop(app);
        // The first GPUI window can miss both SurfaceCreate and native focus events.
        self.focus_native_surface();
        self.sync_frame_delivery();
        self.force_render.set(true);
        self.schedule_frame();
        debug!("OhosWindow: Renderer initialized successfully");
        Ok(())
    }

    pub(crate) fn handle_event(&self, event: &Event) {
        let ended = self
            .native_drag
            .borrow()
            .as_ref()
            .is_some_and(|drag| drag.ended());
        if ended {
            info!(
                "OHOS native file drag ended; releasing action for window {}",
                self.window_id
            );
            self.native_drag.borrow_mut().take();
            self.dispatch_input(PlatformInput::FileDrop(crate::FileDropEvent::Ended));
        }
        match event {
            Event::TextInputReadyChanged { window_id, ready } if *window_id == self.window_id => {
                debug!("OhosWindow: native input ready={ready} for window {window_id}");
                // A new editor lifetime invalidates retries for its predecessor.
                self.input_ready.set(*ready);
                self.cancel_keyboard_retry();
                self.keyboard_visible.set(false);
                if *ready {
                    self.apply_ime_action(true);
                    self.restore_text_input();
                }
            }
            Event::SurfaceCreate => {
                self.surface_available.set(true);
                self.cursor_style.set(None);
                self.sync_frame_delivery();
                debug!("OhosWindow: SurfaceCreate event received - initializing renderer");
                self.focus_native_surface();
                self.request_decorations(self.requested_decorations.get());
                self.apply_window_options();
                self.background_generation
                    .set(self.background_generation.get().wrapping_add(1));
                self.background_pending.set(false);
                self.apply_requested_background();
                self.apply_restored_state();
                self.initialize_accessibility();
                // Initialize renderer when SurfaceCreate event is received
                // Note: on_finish_launching is handled at the platform level (OhosPlatform::handle_ohos_event)
                // before windows are created.
                match self.initialize_renderer() {
                    Ok(()) => {
                        debug!("OhosWindow: Renderer initialized successfully");
                    }
                    Err(e) => {
                        warn!(
                            "OhosWindow: Failed to initialize renderer: {}. Make sure native_window is available from OpenHarmonyApp.",
                            e
                        );
                    }
                }
                if self.refresh_keyboard_overlap_device_px() {
                    self.emit_visual_viewport_changed();
                }
                self.emit_resize_callback();
                self.refresh_insets();
                self.focus_native_surface();
                if self.activation_pending.get() {
                    self.activate();
                }
            }
            Event::SurfaceDestroy => {
                self.cancel_activation();
                self.cancel_keyboard_retry();
                self.cancel_touch_input();
                self.presentation.request();
                self.surface_available.set(false);
                self.input_ready.set(false);
                self.sync_frame_delivery();
                self.background_generation
                    .set(self.background_generation.get().wrapping_add(1));
                self.background_pending.set(false);
                self.native_drag.borrow_mut().take();
                self.manipulation.set(None);
                if let Some(subscription) = self.move_subscription.borrow().as_ref() {
                    subscription.cancel();
                }
                self.geometry_request.borrow_mut().take();
                if self.incoming_drag.replace(false) {
                    self.dispatch_input(PlatformInput::FileDrop(crate::FileDropEvent::Exited));
                }
                self.keyboard_visible.set(false);
                self.release_accessibility();
                self.renderer.borrow_mut().take();
                self.set_hovered(false);
            }
            Event::WindowResize {
                window_id,
                size: ohos_size,
            } if *window_id == self.window_id => {
                // openharmony-ability currently maps both the ArkTS windowSizeChange callback and
                // the XComponent surface callback to WindowResize. In a floating 2-in-1 window the
                // former includes the server-side title bar, while the native render surface does
                // not. Prefer the active XComponent rect whenever it is available so that the
                // renderer and GPUI viewport always use the drawable content size.
                let (content_rect, window_rect) = self
                    .app
                    .borrow()
                    .as_ref()
                    .map(|app| {
                        (
                            app.content_rect_for(self.window_id),
                            app.window_rect_for(self.window_id),
                        )
                    })
                    .unwrap_or_default();
                let device_width = if content_rect.width > 0 {
                    content_rect.width
                } else {
                    ohos_size.width
                };
                let device_height = if content_rect.height > 0 {
                    content_rect.height
                } else {
                    ohos_size.height
                };
                if device_width != ohos_size.width || device_height != ohos_size.height {
                    debug!(
                        "OhosWindow: Normalizing window resize {}x{} to XComponent surface {}x{}",
                        ohos_size.width, ohos_size.height, device_width, device_height,
                    );
                }
                let scale = *self.scale.borrow();
                let width = device_width as f32;
                let height = device_height as f32;
                let new_size = size(px(width / scale), px(height / scale));

                // Update renderer's drawable size
                if let Some(ref mut renderer) = *self.renderer.borrow_mut() {
                    let device_size = Size {
                        width: DevicePixels(width as i32),
                        height: DevicePixels(height as i32),
                    };
                    renderer.update_drawable_size(device_size);
                }
                // Native recover/resize/maximize can briefly expose a 1x1 surface.
                // Publishing it during restoration permanently collapses Zed's docks.
                if self.restore_pending.get() {
                    return;
                }
                // Hiding native decorations also moves the content origin without
                // moving the native window, so a surface resize must refresh both.
                let origin = if window_rect.width > 0 && window_rect.height > 0 {
                    point(
                        px((window_rect.left + content_rect.left) as f32 / scale),
                        px((window_rect.top + content_rect.top) as f32 / scale),
                    )
                } else {
                    self.bounds.borrow().origin
                };
                *self.bounds.borrow_mut() = Bounds::new(origin, new_size);
                let overlap_changed = self.refresh_keyboard_overlap_device_px();
                if overlap_changed {
                    self.emit_visual_viewport_changed();
                } else {
                    self.emit_resize_callback();
                }
                self.refresh_insets();
            }
            Event::ContentRectChange(info) if info.window_id == self.window_id => {
                let scale = self.scale_factor();
                let content_rect = self
                    .app
                    .borrow()
                    .as_ref()
                    .map(|app| app.content_rect_for(self.window_id))
                    .unwrap_or_default();
                let next_origin = point(
                    px((info.rect.left + content_rect.left) as f32 / scale),
                    px((info.rect.top + content_rect.top) as f32 / scale),
                );
                let mut bounds = self.bounds.borrow_mut();
                let moved = bounds.origin != next_origin;
                bounds.origin = next_origin;
                drop(bounds);
                if moved {
                    let mut callback = self.callbacks.borrow_mut().moved.take();
                    if let Some(ref mut callback) = callback {
                        callback();
                    }
                    self.callbacks.borrow_mut().moved = callback;
                }
                if self.refresh_keyboard_overlap_device_px() {
                    self.emit_visual_viewport_changed();
                }
                self.refresh_insets();
            }
            Event::AvoidAreaChange(info) => {
                if matches!(
                    info.area_type,
                    AvoidAreaType::Keyboard
                        | AvoidAreaType::System
                        | AvoidAreaType::SystemGesture
                        | AvoidAreaType::NavigationIndicator
                ) && self.refresh_keyboard_overlap_device_px()
                {
                    self.emit_visual_viewport_changed();
                }
                self.refresh_insets();
            }
            Event::Start => self.update_visibility(WindowVisibility::Visible),
            Event::Stop => self.update_visibility(WindowVisibility::Hidden),
            #[cfg(not(feature = "native-vsync"))]
            Event::WindowRedraw(_) => {
                self.frames.on_xcomponent_callback();
                self.draw_native_frame(Instant::now());
            }
            Event::Input(input_event) => {
                self.handle_input_event(input_event);
            }
            Event::WindowFocusChanged { window_id, focused } => {
                if *focused {
                    self.cancel_activation();
                }
                let next = *focused && *window_id == self.window_id;
                if next {
                    self.update_visibility(WindowVisibility::Visible);
                }
                if let Some(adapter) = self.a11y_adapter.borrow().as_ref() {
                    if let Err(error) = adapter.set_host_focus_state(next) {
                        warn!("Failed to update OHOS accessibility focus: {error}");
                    }
                }
                if self.active.replace(next) != next {
                    self.invalidate_pending_edit();
                    let mut callback = self.callbacks.borrow_mut().active_status_change.take();
                    if let Some(ref mut cb) = callback {
                        cb(next);
                    }
                    self.callbacks.borrow_mut().active_status_change = callback;
                }
                if !next {
                    self.set_hovered(false);
                    self.pressed_mouse_button.set(MouseButtons::default());
                    self.mouse_click.borrow_mut().take();
                    let modifiers_changed = self.key_state.borrow_mut().clear_pressed();
                    if let Some(event) = modifiers_changed {
                        self.dispatch_input(event);
                    }
                    self.hide_keyboard_if_needed();
                    if self.refresh_keyboard_overlap_device_px() {
                        self.emit_visual_viewport_changed();
                    }
                } else {
                    // Native menu popups can leave ArkUI focus on the menu bar.
                    // GPUI focus alone does not restore hardware key delivery,
                    // especially for dialogs without an IME input handler.
                    self.focus_native_surface();
                    // GPUI can retain the editor's focus while another native
                    // window owns the process-wide IME. Returning to this window
                    // must reattach its editor even without a GPUI FocusGained.
                    self.restore_text_input();
                }
            }
            Event::ConfigChanged(configuration) => {
                let appearance = self
                    .appearance_override
                    .get()
                    .unwrap_or_else(|| appearance_for_color_mode(configuration.color_mode));
                self.set_appearance(appearance);
                self.refresh_display();
            }
            Event::WindowDestroy => {
                self.cancel_keyboard_retry();
                if self.closed.replace(true) {
                    return;
                }
                self.sync_frame_delivery();
                self.update_visibility(WindowVisibility::Hidden);
                self.active.set(false);
                self.set_hovered(false);
                self.callbacks.borrow_mut().pending_inputs.clear();
                if self.refresh_keyboard_overlap_device_px() {
                    self.emit_visual_viewport_changed();
                }
                // The native window has already been destroyed. A close veto
                // cannot restore it, so always release GPUI's window state.
                if let Some(callback) = self.callbacks.borrow_mut().close.take() {
                    callback();
                }
            }
            Event::KeyboardEvent(height) => {
                if *height <= 0 {
                    self.notify_keyboard_hidden_by_user_if_needed();
                } else {
                    self.finish_native_keyboard_show();
                }
                if self.refresh_keyboard_overlap_device_px() {
                    self.emit_visual_viewport_changed();
                }
            }
            _ => {}
        }
    }

    fn schedule_ime_text_update(&self) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        if self.ime_text_update_pending.replace(true) {
            return;
        }
        let pending = self.ime_text_update_pending.clone();
        let input_handler = self.input_handler.clone();
        let window_id = self.window_id;
        let window = self
            .window_index
            .upgrade()
            .and_then(|index| index.borrow().get(&window_id).cloned());
        self.foreground_executor
            .spawn(async move {
                pending.set(false);
                // A queued snapshot can outlive a child window close. Avoid
                // querying GPUI's handler after its native editor is gone.
                if !app.is_text_input_ready_for(window_id) {
                    return;
                }
                let state = {
                    let Ok(mut guard) = input_handler.try_borrow_mut() else {
                        return;
                    };
                    let snapshot = (|| {
                        let handler = guard.as_mut()?;
                        let selection = handler.selected_text_range(true)?;
                        let caret = if selection.reversed {
                            selection.range.start
                        } else {
                            selection.range.end
                        };
                        // Fetch bounded context, including one extra unit to detect
                        // documents exceeding the native selection-notification limit.
                        let start = if caret <= 8192 {
                            0
                        } else {
                            caret.saturating_sub(4096)
                        };
                        let mut adjusted = None;
                        let text = handler
                            .text_for_range(start..start.saturating_add(8193), &mut adjusted)?;
                        Some(TextState {
                            text,
                            offset: adjusted.map_or(start, |range| range.start),
                            selection: selection.range,
                            reversed: selection.reversed,
                        })
                    })();
                    // Clear the previous field's context when the new handler
                    // provides no document text.
                    snapshot.unwrap_or_default()
                };
                if app.is_text_input_ready_for(window_id) {
                    if let Some(window) = window.and_then(|window| window.upgrade()) {
                        window
                            .borrow()
                            .update_ime(true, |ime| ime.try_update_text_state(state));
                    }
                }
            })
            .detach();
    }

    pub(crate) fn handle_input_event(&self, event: &InputEvent) {
        if self.closed.get() || self.released.get() {
            return;
        }
        {
            let mut callbacks = self.callbacks.borrow_mut();
            if !callbacks.input_registered || !callbacks.pending_inputs.is_empty() {
                const MAX_PENDING_INPUTS: usize = 64;
                if callbacks.pending_inputs.len() >= MAX_PENDING_INPUTS {
                    callbacks.pending_inputs.pop_front();
                }
                callbacks.pending_inputs.push_back(event.clone());
                return;
            }
        }
        self.dispatch_input_event(event);
    }

    fn dispatch_input_event(&self, event: &InputEvent) {
        let pointer_activation = match event {
            InputEvent::XComponent(XComponentInputEvent::Mouse(event)) => {
                event.action == MouseAction::Press
            }
            InputEvent::XComponent(XComponentInputEvent::Touch(event)) => {
                event.event_type == OhosTouchEvent::Down
            }
            InputEvent::ArkUi(ArkUiInputEvent::Pointer(event)) => {
                event.action == UIInputAction::Down
            }
            _ => false,
        };
        if pointer_activation {
            // A click on the already-active window produces no focus-change
            // event, but still supersedes activation of another pending window.
            self.cancel_other_activations();
        }
        let activates_window = pointer_activation
            || match event {
                InputEvent::XComponent(XComponentInputEvent::Key(event)) => {
                    event.action == KeyAction::Down
                }
                InputEvent::ArkUi(ArkUiInputEvent::Key(event)) => event.action == KeyAction::Down,
                _ => false,
            };
        if activates_window && !self.active.get() {
            // OHOS can deliver the activating press before WINDOW_ACTIVE. Actions
            // dispatched by that press must already see this as the active window.
            let windows = self
                .windows
                .upgrade()
                .map(|windows| {
                    windows
                        .borrow()
                        .iter()
                        .filter_map(Weak::upgrade)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for window in windows {
                window.borrow().handle_event(&Event::WindowFocusChanged {
                    window_id: self.window_id,
                    focused: true,
                });
            }
        }
        match event {
            InputEvent::ArkUi(ArkUiInputEvent::FocusChanged(focused)) => {
                // The IME candidate window and soft keyboard are ArkUI nodes:
                // they can take node focus while this application window stays
                // active. Detaching the native text session there loses every
                // composition and commit callback (candidates keep appearing,
                // but the editor never receives the text). Only tear the
                // session down when the application window itself is inactive.
                if !*focused && self.active.get() {
                    return;
                }
                if self.surface_focused.replace(*focused) == *focused {
                    return;
                }
                self.invalidate_pending_edit();
                if *focused {
                    self.restore_text_input();
                    // ArkUI popups change focus outside GPUI's input dispatch.
                    // Rebuild cached views when the native surface regains focus.
                    self.force_render.set(true);
                    self.schedule_frame();
                } else {
                    // A native popup can take node focus while the window stays active.
                    // Keeping the IME attached sends its Enter/arrow/text callbacks to
                    // the editor underneath the menu instead of to ArkUI.
                    self.set_native_text_input_active(false);
                    self.hide_keyboard_if_needed();
                    self.detach_ime();
                    let change = self.key_state.borrow_mut().clear_pressed();
                    if let Some(change) = change {
                        self.dispatch_input(change);
                    }
                }
            }
            InputEvent::Ime(ime_event) => {
                if !self.surface_focused.get() {
                    return;
                }
                if !matches!(ime_event, ImeEvent::ImeStatusEvent(_)) {
                    let has_focused_handler = self
                        .input_handler
                        .borrow_mut()
                        .as_mut()
                        .is_some_and(PlatformInputHandler::query_accepts_focused_text_input);
                    if !has_focused_handler {
                        // GPUI installs the focused input handler during paint. IME
                        // edits bypass dispatch_key_event's dirty-frame refresh, so
                        // complete it before borrowing the handler when focus moved.
                        let was_deferred = self.defer_presentation.replace(true);
                        self.draw_requested_frame(Instant::now());
                        self.defer_presentation.set(was_deferred);
                    }
                }
                if let ImeEvent::ExtendActionEvent(action) = ime_event {
                    self.text_editing().dispatch_edit_action(*action);
                    self.schedule_ime_text_update();
                    return;
                }
                if !matches!(ime_event, ImeEvent::ImeStatusEvent(_)) {
                    self.invalidate_pending_edit();
                }
                if let ImeEvent::ImeStatusEvent(status) = ime_event {
                    match status {
                        openharmony_ability::ime::KeyboardStatus::Hide => {
                            self.notify_keyboard_hidden_by_user_if_needed();
                        }
                        openharmony_ability::ime::KeyboardStatus::Show => {
                            self.finish_native_keyboard_show();
                        }
                        _ => {}
                    }
                    if self.refresh_keyboard_overlap_device_px() {
                        self.emit_visual_viewport_changed();
                    }
                }

                // Ability already delivers IME callbacks on the UI thread. Deferring
                // only IME edits lets synchronous space/key events overtake the text.
                self.dispatch_ime_event(ime_event.clone());
                self.schedule_ime_text_update();
            }
            InputEvent::XComponent(XComponentInputEvent::Mouse(mouse_event)) => {
                if mouse_event.action == MouseAction::Press
                    && mouse_event.screen_x.is_finite()
                    && mouse_event.screen_y.is_finite()
                {
                    let absolute = (
                        mouse_event.screen_x.round() as i64,
                        mouse_event.screen_y.round() as i64,
                    );
                    self.pointer_screen_position.set(Some(absolute));
                    if let Some(mut session) = self.manipulation.get() {
                        session.pointer = absolute;
                        self.manipulation.set(Some(session));
                    }
                }
                let position =
                    self.pointer_position_from_device_pixels(mouse_event.x, mouse_event.y);
                self.pointer_position.set(Some(position));
                let event_button = Self::mouse_button(mouse_event.button);
                match mouse_event.action {
                    MouseAction::Press => {
                        let Some(button) = event_button else {
                            return;
                        };
                        if self.compat_tap_precedes_mouse(button, position) {
                            return;
                        }
                        self.last_mouse_button_at.set(Some(Instant::now()));
                        self.invalidate_pending_edit();
                        let now = Instant::now();
                        let tuning = GestureTuning::default();
                        let mut click = self.mouse_click.borrow_mut();
                        let click_count = click
                            .as_ref()
                            .filter(|last| {
                                last.button == button
                                    && now.duration_since(last.time) <= tuning.multi_tap_interval
                                    && (position - last.position).magnitude()
                                        <= f64::from(tuning.multi_tap_slop)
                            })
                            .map_or(1, |last| last.count.saturating_add(1));
                        *click = Some(MouseClickState {
                            button,
                            position,
                            time: now,
                            count: click_count,
                        });
                        drop(click);
                        let mut held = self.pressed_mouse_button.get();
                        held.press(button);
                        self.pressed_mouse_button.set(held);
                        let result =
                            self.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
                                button,
                                position,
                                modifiers: self.key_state.borrow().modifiers(),
                                click_count,
                                first_mouse: false,
                            }));
                        // Title-bar controls can consume presses inside the parent's drag area.
                        // Starting a move anyway lets its queued position overwrite maximize.
                        if result.propagate
                            && !result.default_prevented
                            && button == MouseButton::Left
                            && matches!(self.hit_window_control(), Some(WindowControlArea::Drag))
                        {
                            self.start_window_move();
                        }
                    }
                    MouseAction::Release => {
                        self.manipulation.set(None);
                        if let Some(subscription) = self.move_subscription.borrow().as_ref() {
                            subscription.cancel();
                        }
                        let Some(button) = event_button.or(self.pressed_mouse_button.get().first())
                        else {
                            return;
                        };
                        if let Some((suppressed_button, suppressed_at)) =
                            self.suppress_next_release.take()
                        {
                            if suppressed_button == button
                                && suppressed_at.elapsed() < Duration::from_secs(2)
                            {
                                self.last_mouse_button_at.set(Some(Instant::now()));
                                return;
                            }
                        }
                        let mut held = self.pressed_mouse_button.get();
                        held.release(button);
                        self.pressed_mouse_button.set(held);
                        let reopen_keyboard = button == MouseButton::Left
                            && self.mouse_click.borrow().as_ref().is_some_and(|click| {
                                (position - click.position).magnitude()
                                    <= f64::from(GestureTuning::default().touch_slop)
                            })
                            && self.pointer_targets_text_input(position);
                        self.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
                            button,
                            position,
                            modifiers: self.key_state.borrow().modifiers(),
                            click_count: self
                                .mouse_click
                                .borrow()
                                .as_ref()
                                .filter(|click| click.button == button)
                                .map_or(1, |click| click.count),
                        }));
                        // Desktop/2in1 taps can arrive through the mouse callback.
                        // Reopen a retained text focus after a system IME dismissal.
                        if reopen_keyboard {
                            self.request_keyboard();
                        }
                    }
                    MouseAction::Move => {
                        self.restore_cursor_after_move();
                        if self.manipulation.get().is_some() {
                            return;
                        }
                        self.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
                            position,
                            pressed_button: self
                                .pressed_mouse_button
                                .get()
                                .first()
                                .or(event_button),
                            modifiers: self.key_state.borrow().modifiers(),
                        }));
                    }
                    MouseAction::Cancel => {
                        self.cancel_mouse_input();
                    }
                    MouseAction::None => {}
                }
            }
            InputEvent::XComponent(XComponentInputEvent::Hover(hovered)) => {
                self.set_hovered(*hovered);
            }
            InputEvent::ArkUi(ArkUiInputEvent::Pointer(pointer)) => {
                if pointer.event_type == UIInputEvent::Touch
                    && (pointer.source_type == UIInputSourceType::Mouse
                        || pointer.tool_type == UIInputToolType::Mouse)
                {
                    return;
                }
                if let Some(id) = pointer.pointer_id {
                    self.native_pointer_id.set(Some(id));
                }
                if pointer.source_type == UIInputSourceType::Mouse
                    && pointer.action == UIInputAction::Move
                {
                    // Releases and hover exits can both be absent outside a native
                    // window. Reconcile buttons before using re-entry coordinates,
                    // but only while the pointer is outside: the ArkUI pointer
                    // stream reports `buttons_pressed = false` for ordinary moves
                    // inside the window while XComponent still holds the press.
                    // Cancelling there would inject a MouseUp at (-1, -1) before
                    // the real release and swallow every fast click.
                    if pointer.mouse_buttons_pressed == Some(false) {
                        if self.pressed_mouse_button.get().first().is_some() && !self.hovered.get()
                        {
                            self.cancel_mouse_input();
                        }
                    } else {
                        self.record_screen_pointer(pointer.display_x, pointer.display_y);
                    }
                }
            }
            InputEvent::ArkUi(ArkUiInputEvent::Drag(drag)) => {
                self.dispatch_file_drop(drag);
            }
            InputEvent::ArkUi(ArkUiInputEvent::Axis(axis_event)) => {
                let position = self.pointer_position_from_arkui(axis_event.pointer);
                self.pointer_position.set(Some(position));
                let raw = point(axis_event.delta_x, axis_event.delta_y);
                let scale = *self.scale.borrow();
                let wheel = axis_event.pointer.source_type == UIInputSourceType::Mouse
                    && axis_event.pointer.tool_type != UIInputToolType::Touchpad;
                let packet = ScrollPacket::axis(raw, scale, wheel, axis_event.scroll_step);
                let Some(packet) = packet else {
                    return;
                };
                let touch_phase = match axis_event.pointer.action {
                    UIInputAction::Down => TouchPhase::Started,
                    UIInputAction::Up | UIInputAction::Cancel => TouchPhase::Ended,
                    UIInputAction::Move => TouchPhase::Moved,
                };
                if packet.should_dispatch(touch_phase) {
                    #[cfg(feature = "native-input")]
                    let _native = packet.bind(self.handle.window_id(), scale);
                    self.dispatch_scroll(position, packet.delta, touch_phase);
                }
            }
            InputEvent::ArkUi(ArkUiInputEvent::Gesture(
                openharmony_ability::GestureEvent::Pan(pan),
            )) => {
                if pan.pointer.source_type == UIInputSourceType::Mouse
                    || pan.pointer.tool_type == UIInputToolType::Mouse
                {
                    return;
                }
                let phase = match pan.phase {
                    openharmony_ability::GesturePhase::Start => TouchPhase::Started,
                    openharmony_ability::GesturePhase::Update => TouchPhase::Moved,
                    openharmony_ability::GesturePhase::End => TouchPhase::Ended,
                    openharmony_ability::GesturePhase::Cancel => TouchPhase::Cancelled,
                };
                let id = pan
                    .pointer
                    .pointer_id
                    .and_then(|id| self.active_touches.borrow().get(&id).copied());
                let delta = self.point_from_device_pixels(pan.delta_x, pan.delta_y);
                let velocity = self
                    .point_from_device_pixels(pan.velocity_x, pan.velocity_y)
                    .map(f32::from);
                self.touch_scroll.borrow_mut().native_pan(
                    NativePanInput {
                        id,
                        phase,
                        delta,
                        velocity,
                    },
                    Instant::now(),
                    |input| self.dispatch_input(input),
                );
                self.update_touch_long_press_timer();
                self.request_touch_momentum_frame();
            }
            // Raw contacts retain click/long-press and control capture support.
            // Pan End already contains ArkUI's velocity; Swipe must not launch
            // a second momentum curve for the same contact.
            InputEvent::ArkUi(ArkUiInputEvent::Gesture(_)) => {}
            InputEvent::XComponent(XComponentInputEvent::Touch(touch_event)) => {
                self.dispatch_raw_touch_event(touch_event);
            }
            InputEvent::XComponent(XComponentInputEvent::Key(key_event)) => {
                let inputs = self.key_state.borrow_mut().handle(key_event);
                for input in inputs {
                    self.dispatch_keyboard_input(input, false);
                }
            }
            InputEvent::ArkUi(ArkUiInputEvent::Key(key_event)) => {
                let response = key_event.response.clone();
                let inputs = self.key_state.borrow_mut().handle_arkui(key_event);
                let mut consumed = false;
                for input in inputs {
                    consumed |= self.dispatch_keyboard_input(input, true);
                }
                if consumed {
                    response.consume();
                }
            }
        }
    }

    fn invalidate_pending_edit(&self) {
        self.edit_generation
            .set(self.edit_generation.get().wrapping_add(1));
    }

    fn dispatch_ime_event(&self, ime_event: ImeEvent) {
        let handler_ref = &self.input_handler;
        let callbacks = &self.callbacks;
        if let ImeEvent::MoveCursorEvent(direction) = ime_event {
            let key = match direction {
                ImeDirection::Up => "up",
                ImeDirection::Down => "down",
                ImeDirection::Left => "left",
                ImeDirection::Right => "right",
                ImeDirection::None => return,
            };
            Self::dispatch_input_with_callbacks(
                callbacks,
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke: Keystroke {
                        key: key.into(),
                        key_char: None,
                        modifiers: Modifiers::default(),
                    },
                    is_held: false,
                    prefer_character_input: false,
                }),
            );
            return;
        }
        if let ImeEvent::EnterEvent(action) = ime_event {
            if let Some(handler) = handler_ref.borrow_mut().as_mut() {
                handler.unmark_text();
            }
            // Confirm keys must reach application key bindings;
            // Search/Send/Next must not silently insert a newline.
            let result = Self::dispatch_input_with_callbacks(
                callbacks,
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke: Keystroke {
                        key: "enter".into(),
                        key_char: None,
                        modifiers: Modifiers::default(),
                    },
                    is_held: false,
                    prefer_character_input: false,
                }),
            );
            if result.propagate
                && matches!(
                    EnterKey::from(action as u32),
                    EnterKey::Newline | EnterKey::Unspecified
                )
            {
                if let Some(handler) = handler_ref.borrow_mut().as_mut() {
                    handler.replace_text_in_range(None, "\n");
                }
            }
            return;
        }
        let mut handler_guard = handler_ref.borrow_mut();
        let Some(handler) = handler_guard.as_mut() else {
            return;
        };

        match ime_event {
            ImeEvent::TextInputEvent(data) => {
                handler.replace_text_in_range(None, &data.text);
                handler.unmark_text();
            }
            ImeEvent::PreviewTextEvent { text, start, end } => {
                let range = (start >= 0 && end >= start).then_some(start as usize..end as usize);
                handler.replace_and_mark_text_in_range(range, &text, None);
            }
            ImeEvent::FinishPreviewEvent => handler.unmark_text(),
            ImeEvent::EnterEvent(_) => unreachable!("confirm key handled above"),
            ImeEvent::MoveCursorEvent(_) => {
                unreachable!("cursor key handled above")
            }
            ImeEvent::ExtendActionEvent(_) => {
                unreachable!("edit command handled above")
            }
            ImeEvent::SelectionEvent { start, end } => {
                if start >= 0 && end >= 0 {
                    handler.unmark_text();
                    handler
                        .set_selected_text_range(start.min(end) as usize..start.max(end) as usize);
                }
            }
            ImeEvent::BackspaceEvent(len) | ImeEvent::DeleteForwardEvent(len) => {
                let forward = matches!(ime_event, ImeEvent::DeleteForwardEvent(_));
                let len = (len).max(0) as usize;
                if len == 0 {
                    return;
                }

                let selection = handler.selected_text_range(true);
                let has_editable_text = selection.as_ref().is_some_and(|selection| {
                    handler
                        .text_for_range(selection.range.clone(), &mut None)
                        .is_some()
                });
                if !has_editable_text {
                    // Terminals expose an IME cursor without editable backing text.
                    // Let their key handlers encode deletion for the PTY.
                    // Dispatch can re-enter input handling, so release the borrow first.
                    drop(handler_guard);
                    for _ in 0..len {
                        let keystroke = Keystroke {
                            key: if forward { "delete" } else { "backspace" }.into(),
                            key_char: None,
                            modifiers: Modifiers::default(),
                        };
                        Self::dispatch_input_with_callbacks(
                            callbacks,
                            PlatformInput::KeyDown(KeyDownEvent {
                                keystroke: keystroke.clone(),
                                is_held: false,
                                prefer_character_input: false,
                            }),
                        );
                        Self::dispatch_input_with_callbacks(
                            callbacks,
                            PlatformInput::KeyUp(gpui::KeyUpEvent { keystroke }),
                        );
                    }
                    return;
                }

                if let Some(selection) = selection {
                    let range = if selection.range.start != selection.range.end {
                        selection.range
                    } else {
                        let caret = if selection.reversed {
                            selection.range.start
                        } else {
                            selection.range.end
                        };
                        // Read one extra UTF-16 unit so a one-unit
                        // deletion cannot split a surrogate pair.
                        let request = if forward {
                            caret..caret.saturating_add(len).saturating_add(1)
                        } else {
                            caret.saturating_sub(len.saturating_add(1))..caret
                        };
                        let mut adjusted = None;
                        let text = handler.text_for_range(request, &mut adjusted);
                        let mut units = 0;
                        if let Some(text) = text {
                            if forward {
                                for character in text.chars() {
                                    units += character.len_utf16();
                                    if units >= len {
                                        break;
                                    }
                                }
                            } else {
                                for character in text.chars().rev() {
                                    units += character.len_utf16();
                                    if units >= len {
                                        break;
                                    }
                                }
                            }
                        } else {
                            units = len;
                        }
                        if forward {
                            caret..caret.saturating_add(units)
                        } else {
                            caret.saturating_sub(units)..caret
                        }
                    };
                    handler.replace_text_in_range(Some(range), "");
                } else {
                    handler.replace_text_in_range(None, "");
                }
            }
            ImeEvent::ImeStatusEvent(status) => {
                if matches!(status, openharmony_ability::ime::KeyboardStatus::Hide) {
                    handler.unmark_text();
                }
            }
        }
    }

    fn dispatch_file_drop(&self, drag: &openharmony_ability::DragInputData) {
        use openharmony_ability::DragPhase;
        let offset = self
            .app
            .borrow()
            .as_ref()
            .map(|app| app.content_rect_for(self.window_id))
            .unwrap_or_default();
        let position = self.point_from_device_pixels(
            drag.window_x - offset.left as f32,
            drag.window_y - offset.top as f32,
        );
        if matches!(drag.phase, DragPhase::Enter | DragPhase::Drop) {
            let paths = match super::platform::resolve_file_paths(&drag.file_uris) {
                Ok(paths) => paths,
                Err(error) => {
                    warn!("Failed to resolve OHOS dropped files: {error:#}");
                    if matches!(drag.phase, DragPhase::Drop) {
                        if let Some(app) = self.app.borrow().clone() {
                            self.background_executor
                                .spawn(super::platform::show_platform_error(
                                    app,
                                    self.window_id,
                                    "Unable to open dropped files",
                                    format!("{error:#}"),
                                ))
                                .detach();
                        }
                    }
                    Vec::new()
                }
            };
            // The final drop can contain different data or lose access. Never
            // submit paths retained from an earlier hover when it cannot be resolved.
            if paths.is_empty() {
                if self.incoming_drag.replace(false) {
                    self.dispatch_input(PlatformInput::FileDrop(crate::FileDropEvent::Exited));
                }
                return;
            }
            info!(
                "OHOS native drag {:?} window={} uris={} paths={} at ({}, {})",
                drag.phase,
                self.window_id,
                drag.file_uris.len(),
                paths.len(),
                drag.window_x,
                drag.window_y
            );
            if self.incoming_drag.replace(true) {
                // GPUI preserves an active drag on Entered. Clear the hover payload
                // before installing the final paths delivered by the native drop.
                self.dispatch_input(PlatformInput::FileDrop(crate::FileDropEvent::Exited));
            }
            self.dispatch_input(PlatformInput::FileDrop(crate::FileDropEvent::Entered {
                position,
                paths: crate::ExternalPaths(paths.into()),
            }));
        }
        match drag.phase {
            DragPhase::Move if self.incoming_drag.get() => {
                self.dispatch_input(PlatformInput::FileDrop(crate::FileDropEvent::Pending {
                    position,
                }));
            }
            DragPhase::Drop if self.incoming_drag.replace(false) => {
                // OHOS may supply paths only on Drop. Zed's drop targets become
                // visible after Entered, so paint their hitboxes before Submit.
                // Request presentation to bypass inactive-window frame throttling.
                self.presentation.request();
                self.draw_requested_frame(Instant::now());
                let result =
                    self.dispatch_input(PlatformInput::FileDrop(crate::FileDropEvent::Submit {
                        position,
                    }));
                if !result.propagate || result.default_prevented {
                    drag.response.accept();
                    super::platform::retain_file_access(
                        self.file_access.clone(),
                        &self.background_executor,
                        drag.file_uris.clone(),
                    );
                }
                self.dispatch_input(PlatformInput::FileDrop(crate::FileDropEvent::Exited));
            }
            DragPhase::Leave if self.incoming_drag.replace(false) => {
                self.dispatch_input(PlatformInput::FileDrop(crate::FileDropEvent::Exited));
            }
            _ => {}
        }
    }

    fn dispatch_keyboard_input(&self, input: PlatformInput, after_ime: bool) -> bool {
        let character = if after_ime {
            match &input {
                PlatformInput::KeyDown(event) => event.keystroke.key_char.clone(),
                _ => None,
            }
        } else {
            None
        };
        let edit_action = if let PlatformInput::KeyDown(event) = &input {
            self.invalidate_pending_edit();
            let modifiers = event.keystroke.modifiers;
            if modifiers.control
                && !modifiers.alt
                && !modifiers.shift
                && !modifiers.platform
                && !modifiers.function
            {
                match event.keystroke.key.as_str() {
                    "a" => Some(ImeAction::SelectAll),
                    "x" => Some(ImeAction::Cut),
                    "c" => Some(ImeAction::Copy),
                    "v" => Some(ImeAction::Paste),
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        };
        let result = self.dispatch_input(input);
        let mut consumed = !result.propagate || result.default_prevented;
        if !consumed {
            if let Some(action) = edit_action {
                consumed = self.fallback_edit_action(action);
            } else if let Some(character) = character {
                // ArkUI sends this callback only after the IME declines the key.
                // Numeric-keypad/space input still needs a text insertion path.
                if let Some(handler) = self.input_handler.borrow_mut().as_mut() {
                    if handler.query_accepts_text_input() {
                        handler.unmark_text();
                        handler.replace_text_in_range(None, &character);
                        consumed = true;
                    }
                }
                self.schedule_ime_text_update();
            }
        }
        consumed
    }

    fn text_editing(&self) -> OhosTextEditing {
        OhosTextEditing {
            app: self.app.clone(),
            callbacks: self.callbacks.clone(),
            input_handler: self.input_handler.clone(),
            edit_generation: self.edit_generation.clone(),
            foreground_executor: self.foreground_executor.clone(),
            clipboard: self.clipboard.clone(),
            window_id: self.window_id,
        }
    }

    fn fallback_edit_action(&self, action: ImeAction) -> bool {
        let accepted = self.text_editing().fallback_edit_action(action);
        self.schedule_ime_text_update();
        accepted
    }

    fn dispatch_input(&self, input: PlatformInput) -> crate::DispatchEventResult {
        Self::dispatch_input_with_callbacks(&self.callbacks, input)
    }
}

struct OhosTextEditing {
    app: Rc<RefCell<Option<OpenHarmonyApp>>>,
    callbacks: Rc<RefCell<WindowCallbacks>>,
    input_handler: Rc<RefCell<Option<PlatformInputHandler>>>,
    edit_generation: Rc<Cell<u64>>,
    foreground_executor: ForegroundExecutor,
    clipboard: super::clipboard::OhosClipboard,
    window_id: i64,
}

impl OhosTextEditing {
    fn invalidate_pending_edit(&self) {
        self.edit_generation
            .set(self.edit_generation.get().wrapping_add(1));
    }

    fn dispatch_input(&self, input: PlatformInput) -> crate::DispatchEventResult {
        OhosWindow::dispatch_input_with_callbacks(&self.callbacks, input)
    }

    fn dispatch_edit_action(&self, action: ImeAction) {
        let key = match action {
            ImeAction::SelectAll => "a",
            ImeAction::Cut => "x",
            ImeAction::Copy => "c",
            ImeAction::Paste => "v",
        };
        let result = self.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: Keystroke {
                key: key.into(),
                key_char: None,
                modifiers: Modifiers {
                    control: true,
                    ..Default::default()
                },
            },
            is_held: false,
            prefer_character_input: false,
        }));
        if result.propagate && !result.default_prevented {
            self.fallback_edit_action(action);
        }
    }

    fn fallback_edit_action(&self, action: ImeAction) -> bool {
        self.invalidate_pending_edit();
        let (selection, selected_text, input_bounds) = {
            let mut guard = self.input_handler.borrow_mut();
            let Some(handler) = guard.as_mut() else {
                return false;
            };
            if !handler.query_accepts_text_input() {
                return false;
            }
            if action == ImeAction::SelectAll {
                if let Some(length) = handler.text_length_utf16() {
                    handler.set_selected_text_range(0..length);
                    return true;
                }
                return false;
            }
            let Some(selection) = handler.selected_text_range(false) else {
                return false;
            };
            if action != ImeAction::Paste && selection.range.is_empty() {
                return false;
            }
            let mut adjusted = None;
            let selected_text = handler.text_for_range(selection.range.clone(), &mut adjusted);
            if action != ImeAction::Paste
                && (selected_text
                    .as_ref()
                    .map(|text| text.encode_utf16().count())
                    != Some(selection.range.len())
                    || adjusted.is_some_and(|range| range != selection.range))
            {
                return false;
            }
            (selection, selected_text, handler.element_bounds())
        };
        let Some(app) = self.app.borrow().clone() else {
            return false;
        };
        let handler_ref = self.input_handler.clone();
        let generation = self.edit_generation.clone();
        let expected_generation = generation.get();
        let window_id = self.window_id;
        let clipboard = self.clipboard.clone();
        self.foreground_executor
            .spawn(async move {
                let replacement = if action == ImeAction::Paste {
                    match clipboard.read().await {
                        Ok(item) => item.and_then(|item| item.text()),
                        Err(error) => {
                            warn!("Failed to paste OHOS clipboard: {error}");
                            return;
                        }
                    }
                } else {
                    let Some(text) = &selected_text else {
                        return;
                    };
                    if let Err(error) = clipboard
                        .write(crate::ClipboardItem::new_string(text.clone()))
                        .await
                    {
                        warn!("Failed to copy OHOS selection: {error}");
                        return;
                    }
                    if action == ImeAction::Copy {
                        return;
                    }
                    Some(String::new())
                };
                let Some(replacement) = replacement else {
                    return;
                };
                // An async clipboard permission prompt/read can outlive the input.
                // Never replace another selection or delete text after a failed copy.
                if generation.get() != expected_generation
                    || app.native_window_for(window_id).is_none()
                {
                    return;
                }
                let mut guard = handler_ref.borrow_mut();
                let Some(handler) = guard.as_mut() else {
                    return;
                };
                if !handler.query_accepts_text_input()
                    || handler.element_bounds() != input_bounds
                    || handler.selected_text_range(false).is_none_or(|current| {
                        current.range != selection.range || current.reversed != selection.reversed
                    })
                {
                    return;
                }
                if let Some(original) = &selected_text {
                    let mut adjusted = None;
                    if handler
                        .text_for_range(selection.range.clone(), &mut adjusted)
                        .as_ref()
                        != Some(original)
                    {
                        return;
                    }
                }
                handler.unmark_text();
                handler.replace_text_in_range(Some(selection.range), &replacement);
            })
            .detach();
        true
    }
}

impl HasWindowHandle for OhosWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        self.app
            .borrow()
            .as_ref()
            .and_then(|app| app.native_window_for(self.window_id))
            .and_then(|native_window| native_window.raw_window_handle())
            .map(|raw_handle| unsafe { raw_window_handle::WindowHandle::borrow_raw(raw_handle) })
            .ok_or(raw_window_handle::HandleError::Unavailable)
    }
}

impl HasWindowHandle for OhosWindowHandle {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let window = self.inner.borrow();
        let app = window.app.borrow();
        app.as_ref()
            .and_then(|app| app.native_window_for(window.window_id))
            .and_then(|native_window| native_window.raw_window_handle())
            .map(|raw_handle| unsafe { raw_window_handle::WindowHandle::borrow_raw(raw_handle) })
            .ok_or(raw_window_handle::HandleError::Unavailable)
    }
}

impl HasDisplayHandle for OhosWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::ohos())
    }
}

impl HasDisplayHandle for OhosWindowHandle {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::ohos())
    }
}

impl PlatformWindow for OhosWindowHandle {
    fn insets(&self) -> WindowInsets {
        self.with_window(|window| window.insets())
    }

    fn on_insets_changed(&self, callback: Box<dyn FnMut(WindowInsets)>) {
        self.with_window(|window| window.on_insets_changed(callback))
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.with_window(|window| window.bounds())
    }

    fn is_maximized(&self) -> bool {
        self.with_window(|window| window.is_maximized())
    }

    fn window_bounds(&self) -> WindowBounds {
        self.with_window(|window| window.window_bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.with_window(|window| window.content_size())
    }

    fn visual_viewport_bounds(&self) -> Bounds<Pixels> {
        self.with_window(|window| window.visual_viewport_bounds())
    }

    fn on_visual_viewport_changed(&self, callback: Box<dyn FnMut()>) {
        self.with_window(|window| window.on_visual_viewport_changed(callback))
    }

    fn resize(&mut self, size: Size<Pixels>) {
        self.with_window(|window| window.request_resize(size))
    }

    fn scale_factor(&self) -> f32 {
        self.with_window(|window| window.scale_factor())
    }

    fn appearance(&self) -> WindowAppearance {
        self.with_window(|window| window.appearance())
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        self.with_window(|window| window.display())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.with_window(|window| window.mouse_position())
    }

    fn modifiers(&self) -> Modifiers {
        self.with_window(|window| window.modifiers())
    }

    fn capslock(&self) -> Capslock {
        self.with_window(|window| window.capslock())
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        *self.input_handler.borrow_mut() = Some(input_handler);
        self.with_window(|window| window.schedule_ime_text_update());
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.input_handler.borrow_mut().take()
    }

    fn set_text_input_configuration(&mut self, configuration: TextInputConfiguration) {
        self.with_window(|window| window.configure_text_input(configuration));
    }

    fn prompt(
        &self,
        level: PromptLevel,
        msg: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        self.with_window(|window| window.prompt(level, msg, detail, answers))
    }

    fn activate(&self) {
        self.with_window(|window| window.activate())
    }

    fn request_attention(&self) {
        self.with_window(|window| window.request_attention())
    }

    fn is_active(&self) -> bool {
        self.with_window(|window| window.is_active())
    }

    fn is_hovered(&self) -> bool {
        self.with_window(|window| window.is_hovered())
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        self.with_window(|window| window.background_appearance())
    }

    fn get_title(&self) -> String {
        self.with_window(|window| window.get_title())
    }
    fn native_window_state(&self) -> Option<Vec<u8>> {
        self.with_window(|window| window.native_window_state())
    }
    fn restore_native_window_state(&self, state: &[u8]) {
        self.with_window(|window| window.restore_native_window_state(state));
    }

    fn set_title(&mut self, title: &str) {
        self.with_window(|window| window.request_title(title))
    }

    fn set_background_appearance(&self, background_appearance: WindowBackgroundAppearance) {
        self.with_window(|window| window.set_background_appearance(background_appearance))
    }

    fn minimize(&self) {
        self.with_window(|window| window.minimize())
    }

    fn zoom(&self) {
        self.with_window(|window| window.zoom())
    }

    fn toggle_fullscreen(&self) {
        self.with_window(|window| window.toggle_fullscreen())
    }

    fn is_fullscreen(&self) -> bool {
        self.with_window(|window| window.is_fullscreen())
    }

    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        self.with_window(OhosWindow::frame_waker)
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.with_window(|window| window.on_request_frame(callback))
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> crate::DispatchEventResult>) {
        self.with_window(|window| window.on_input(callback))
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.with_window(|window| window.on_active_status_change(callback))
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.with_window(|window| window.on_hover_status_change(callback))
    }

    fn set_back_handler(&self, callback: Box<dyn FnMut()>) {
        self.with_window(|window| window.set_back_handler(callback))
    }

    fn set_back_enabled(&self, enabled: bool) {
        self.with_window(|window| window.set_back_enabled(enabled))
    }

    fn a11y_init(&self, callbacks: A11yCallbacks) {
        self.with_window(|window| window.a11y_init(callbacks))
    }

    fn a11y_tree_update(&self, update: accesskit::TreeUpdate) {
        self.with_window(|window| window.a11y_tree_update(update))
    }

    fn visibility(&self) -> WindowVisibility {
        self.with_window(|window| window.visibility())
    }

    fn on_visibility_change(&self, callback: Box<dyn FnMut(WindowVisibility)>) {
        self.with_window(|window| window.on_visibility_change(callback))
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.with_window(|window| window.on_resize(callback))
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.with_window(|window| window.on_moved(callback))
    }

    fn on_display_changed(&self, callback: Box<dyn FnMut()>) {
        self.with_window(|window| window.on_display_changed(callback))
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.with_window(|window| window.on_should_close(callback))
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.with_window(|window| window.on_hit_test_window_control(callback))
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.with_window(|window| window.on_close(callback))
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.with_window(|window| window.on_appearance_changed(callback))
    }

    fn on_button_layout_changed(&self, callback: Box<dyn FnMut()>) {
        self.with_window(|window| window.on_button_layout_changed(callback))
    }

    fn draw(&self, scene: &Scene) {
        self.with_window(|window| window.draw(scene))
    }

    fn schedule_frame(&self) {
        self.with_window(|window| window.schedule_frame())
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.with_window(|window| window.sprite_atlas())
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        self.with_window(|window| window.gpu_specs())
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        self.with_window(|window| window.is_subpixel_rendering_supported())
    }

    fn update_ime_position(&self, bounds: Bounds<Pixels>) {
        self.with_window(|window| window.update_ime_position(bounds))
    }

    fn show_soft_keyboard(&self) {
        self.with_window(|window| window.show_soft_keyboard())
    }

    fn hide_soft_keyboard(&self) {
        self.with_window(|window| window.hide_soft_keyboard())
    }

    fn text_input_state_changed(&self, change: TextInputStateChange) {
        self.with_window(|window| window.text_input_state_changed(change))
    }

    fn request_decorations(&self, decorations: WindowDecorations) {
        self.with_window(|window| window.request_decorations(decorations))
    }

    fn show_window_menu(&self, position: Point<Pixels>) {
        self.with_window(|window| window.show_window_menu(position))
    }

    fn start_window_move(&self) {
        self.with_window(|window| window.start_window_move())
    }

    fn can_start_external_drag(&self) -> bool {
        self.with_window(|window| window.can_start_external_drag())
    }
    fn start_external_drag(&self, payload: &crate::ExternalDragPayload) -> bool {
        self.with_window(|window| window.start_external_drag(payload))
    }

    fn start_window_resize(&self, edge: ResizeEdge) {
        self.with_window(|window| window.start_window_resize(edge))
    }

    fn window_decorations(&self) -> crate::Decorations {
        self.with_window(|window| window.window_decorations())
    }

    fn set_app_id(&mut self, app_id: &str) {
        self.with_window(|window| window.request_app_id(app_id))
    }

    fn map_window(&mut self) -> Result<()> {
        self.with_window(|window| window.request_map())
    }

    fn window_controls(&self) -> WindowControls {
        self.with_window(|window| window.window_controls())
    }

    fn set_client_inset(&self, inset: Pixels) {
        self.with_window(|window| window.set_client_inset(inset))
    }
}

impl PlatformWindow for OhosWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        *self.bounds.borrow()
    }

    fn is_maximized(&self) -> bool {
        self.maximized.get()
    }

    fn window_bounds(&self) -> WindowBounds {
        if !self.fullscreen.get() && !self.maximized.get() {
            return WindowBounds::Windowed(*self.bounds.borrow());
        }
        let restore_bounds = self
            .floating_state
            .get()
            .map(|state| {
                let scale = self.scale_factor();
                Bounds::new(
                    point(px(state.left as f32 / scale), px(state.top as f32 / scale)),
                    size(
                        px(state.width as f32 / scale),
                        px(state.height as f32 / scale),
                    ),
                )
            })
            .unwrap_or_else(|| *self.bounds.borrow());
        if self.fullscreen.get() {
            WindowBounds::Fullscreen(restore_bounds)
        } else {
            WindowBounds::Maximized(restore_bounds)
        }
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds.borrow().size
    }

    fn visual_viewport_bounds(&self) -> Bounds<Pixels> {
        Bounds::new(Point::default(), self.effective_content_size())
    }

    fn on_visual_viewport_changed(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().visual_viewport_changed = Some(callback);
    }

    fn resize(&mut self, size: Size<Pixels>) {
        self.request_resize(size);
    }

    fn scale_factor(&self) -> f32 {
        *self.scale.borrow()
    }

    fn appearance(&self) -> WindowAppearance {
        self.appearance.get()
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(self.display.clone()))
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.pointer_position
            .get()
            .unwrap_or_else(|| point(px(0.0), px(0.0)))
    }

    fn modifiers(&self) -> Modifiers {
        self.key_state.borrow().modifiers()
    }

    fn capslock(&self) -> Capslock {
        self.key_state.borrow().capslock()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        *self.input_handler.borrow_mut() = Some(input_handler);
        self.schedule_ime_text_update();
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.input_handler.borrow_mut().take()
    }

    fn set_text_input_configuration(&mut self, configuration: TextInputConfiguration) {
        self.configure_text_input(configuration);
    }

    fn prompt(
        &self,
        level: PromptLevel,
        msg: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        let client = self.window_client()?;
        if answers.is_empty() || answers.len() > 128 {
            return None;
        }
        let request = openharmony_ability_plugin_window::WindowPromptRequest {
            window_id: self.window_id,
            message: msg.to_owned(),
            detail: detail.map(str::to_owned),
            level: match level {
                PromptLevel::Info => 0,
                PromptLevel::Warning => 1,
                PromptLevel::Critical => 2,
            },
            buttons: answers
                .iter()
                .map(|answer| answer.label().to_string())
                .collect(),
            cancel_index: answers
                .iter()
                .position(PromptButton::is_cancel)
                .map(|index| index as u32),
            anchor: None,
            appearance: None,
        };
        let (sender, receiver) = oneshot::channel();
        let window = self
            .window_index
            .upgrade()
            .and_then(|index| index.borrow().get(&self.window_id).cloned());
        self.foreground_executor
            .spawn(async move {
                let result = client.show_prompt(request).await;
                if let Some(window) = window.as_ref().and_then(Weak::upgrade) {
                    let window = window.borrow();
                    if !window.closed.get() && !window.released.get() {
                        window.focus_native_surface();
                    }
                }
                match result {
                    Ok(index) => {
                        if sender.send(index as usize).is_err() {
                            log::debug!("OHOS native prompt receiver was dropped");
                        }
                    }
                    Err(error) => warn!("OHOS native prompt failed: {error}"),
                }
            })
            .detach();
        Some(receiver)
    }

    fn activate(&self) {
        if self.closed.get() || self.released.get() {
            return;
        }
        self.cancel_activation();
        self.cancel_other_activations();
        self.activation_pending.set(true);
        self.visibility.set(WindowVisibility::Visible);
        self.sync_frame_delivery();
        // Focusing also launches a deferred native window. Waiting for its
        // surface first would prevent that window from ever being created.
        let Some(client) = self.window_client() else {
            self.activation_pending.set(false);
            return;
        };
        let Some(window) = self
            .window_index
            .upgrade()
            .and_then(|index| index.borrow().get(&self.window_id).cloned())
        else {
            self.activation_pending.set(false);
            return;
        };
        let window_id = self.window_id;
        let generation = self.activation_generation.get();
        self.foreground_executor
            .spawn(async move {
                // An independent OHOS child can report showWindow success before
                // SceneBoard creates its container. Such an early focus request
                // is lost, so wait for WINDOW_ACTIVE with bounded retries.
                for delay in [0, 50, 100, 200, 400, 800, 1600, 3200, 5000, 5000] {
                    if delay > 0 {
                        smol::Timer::after(Duration::from_millis(delay)).await;
                    }
                    let should_request = window.upgrade().is_some_and(|window| {
                        let window = window.borrow();
                        if delay > 0
                            && window.active.get()
                            && window.activation_generation.get() == generation
                        {
                            window.cancel_activation();
                        }
                        window.activation_is_pending(generation)
                    });
                    if !should_request {
                        return;
                    }
                    if let Err(error) = client.focus_window(window_id).await {
                        warn!("Failed to focus OHOS window {window_id}: {error}");
                        if let Some(window) = window.upgrade() {
                            let window = window.borrow();
                            if window.activation_generation.get() == generation {
                                window.cancel_activation();
                            }
                        }
                        return;
                    }
                }
                smol::Timer::after(Duration::from_secs(5)).await;
                if let Some(window) = window.upgrade() {
                    let window = window.borrow();
                    if window.activation_is_pending(generation) {
                        window.cancel_activation();
                        warn!("OHOS window {window_id} activation was not confirmed");
                    }
                }
            })
            .detach();
    }

    fn request_attention(&self) {
        let Some(client) = self.window_client() else {
            return;
        };
        let window_id = self.window_id;
        self.foreground_executor
            .spawn(async move {
                if let Err(error) = client.request_user_attention(window_id).await {
                    warn!("Failed to request OHOS window attention: {error}");
                }
            })
            .detach();
    }

    fn is_active(&self) -> bool {
        self.active.get()
    }

    fn visibility(&self) -> WindowVisibility {
        self.visibility.get()
    }

    fn is_hovered(&self) -> bool {
        self.hovered.get()
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        self.background_appearance.get()
    }

    fn get_title(&self) -> String {
        self.title.borrow().clone()
    }

    fn native_window_state(&self) -> Option<Vec<u8>> {
        let mut state = if self.maximized.get() || self.fullscreen.get() {
            self.floating_state
                .get()
                .or_else(|| self.current_native_state())?
        } else {
            self.current_native_state()?
        };
        state.maximized = self.maximized.get();
        state.fullscreen = self.fullscreen.get();
        Some(state.encode())
    }

    fn restore_native_window_state(&self, state: &[u8]) {
        let Some(state) = super::window_state::NativeWindowState::decode(state) else {
            warn!("Ignoring invalid OHOS native window state");
            return;
        };
        *self.restore_state.borrow_mut() = Some(state);
        self.apply_restored_state();
    }

    fn set_title(&mut self, title: &str) {
        self.request_title(title)
    }

    fn set_background_appearance(&self, appearance: WindowBackgroundAppearance) {
        // Preserve the system's default background until an explicit transition is requested.
        if self
            .requested_background
            .get()
            .unwrap_or(self.background_appearance.get())
            == appearance
        {
            return;
        }
        self.requested_background.set(Some(appearance));
        self.apply_requested_background();
    }

    fn minimize(&self) {
        if !self.is_minimizable {
            return;
        }
        let Some(client) = self.window_client() else {
            return;
        };
        let window_id = self.window_id;
        let mode_operations = self.mode_operations.clone();
        self.foreground_executor
            .spawn(async move {
                let _operation = mode_operations.lock().await;
                if let Err(error) = client.minimize_window(window_id).await {
                    warn!("Failed to minimize OHOS window: {error}");
                }
            })
            .detach();
    }

    fn zoom(&self) {
        if !self.is_resizable {
            return;
        }
        let Some(client) = self.window_client() else {
            return;
        };
        let window_id = self.window_id;
        let state = self.maximized.clone();
        let window = self
            .window_index
            .upgrade()
            .and_then(|index| index.borrow().get(&window_id).cloned());
        let mode_operations = self.mode_operations.clone();
        self.foreground_executor
            .spawn(async move {
                let _operation = mode_operations.lock().await;
                let floating_state = window.as_ref().and_then(Weak::upgrade).and_then(|window| {
                    let window = window.borrow();
                    window.remember_floating_state();
                    window.floating_state.get()
                });
                let next = !state.get();
                let result = if next {
                    client.maximize_window(window_id).await
                } else {
                    Self::recover_floating_window(&client, window_id, floating_state).await
                };
                match result {
                    Ok(()) => {
                        state.set(next);
                        if let Some(window) = window.and_then(|window| window.upgrade()) {
                            window.borrow().notify_window_mode_changed();
                        }
                    }
                    Err(error) => warn!("Failed to zoom OHOS window: {error}"),
                }
            })
            .detach();
    }

    fn toggle_fullscreen(&self) {
        let Some(client) = self.window_client() else {
            return;
        };
        let window_id = self.window_id;
        let state = self.fullscreen.clone();
        let window = self
            .window_index
            .upgrade()
            .and_then(|index| index.borrow().get(&window_id).cloned());
        let mode_operations = self.mode_operations.clone();
        self.foreground_executor
            .spawn(async move {
                let _operation = mode_operations.lock().await;
                if let Some(window) = window.as_ref().and_then(Weak::upgrade) {
                    window.borrow().remember_floating_state();
                }
                let next = !state.get();
                match client.set_fullscreen(window_id, next).await {
                    Ok(()) => {
                        state.set(next);
                        if let Some(window) = window.and_then(|window| window.upgrade()) {
                            window.borrow().notify_window_mode_changed();
                        }
                    }
                    Err(error) => warn!("Failed to set OHOS fullscreen: {error}"),
                }
            })
            .detach();
    }

    fn is_fullscreen(&self) -> bool {
        self.fullscreen.get()
    }

    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        OhosWindow::frame_waker(self)
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> crate::DispatchEventResult>) {
        let should_replay = {
            let mut callbacks = self.callbacks.borrow_mut();
            let was_registered = std::mem::replace(&mut callbacks.input_registered, true);
            callbacks.input = Some(callback);
            !was_registered && !callbacks.pending_inputs.is_empty()
        };
        if should_replay {
            if let Some(window) = self
                .window_index
                .upgrade()
                .and_then(|index| index.borrow().get(&self.window_id).cloned())
            {
                self.foreground_executor
                    .spawn(async move {
                        loop {
                            let Some(window) = window.upgrade() else {
                                return;
                            };
                            let next = {
                                let window = window.borrow();
                                if window.closed.get() || window.released.get() {
                                    window.callbacks.borrow_mut().pending_inputs.clear();
                                    return;
                                }
                                let next = window.callbacks.borrow_mut().pending_inputs.pop_front();
                                next
                            };
                            let Some(input) = next else {
                                return;
                            };
                            window.borrow().dispatch_input_event(&input);
                        }
                    })
                    .detach();
            } else {
                self.callbacks.borrow_mut().pending_inputs.clear();
            }
        }
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.callbacks.borrow_mut().active_status_change = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.callbacks.borrow_mut().hover_status_change = Some(callback);
    }

    fn set_back_handler(&self, callback: Box<dyn FnMut()>) {
        *self.back_handler.borrow_mut() = Some(callback);
    }

    fn set_back_enabled(&self, enabled: bool) {
        self.back_enabled.set(enabled);
    }

    fn a11y_init(&self, callbacks: A11yCallbacks) {
        self.release_accessibility();
        *self.a11y_callbacks.borrow_mut() = Some(Arc::new(Mutex::new(callbacks)));
        self.initialize_accessibility();
    }

    fn a11y_tree_update(&self, update: accesskit::TreeUpdate) {
        self.initialize_accessibility();
        if let Some(adapter) = self.a11y_adapter.borrow().as_ref() {
            if let Err(error) = adapter.update_if_active(|| update) {
                warn!("Failed to update OHOS accessibility tree: {error}");
            }
        }
    }

    fn on_visibility_change(&self, callback: Box<dyn FnMut(WindowVisibility)>) {
        self.callbacks.borrow_mut().visibility_change = Some(callback);
    }

    fn insets(&self) -> WindowInsets {
        self.current_safe_area_insets()
    }

    fn on_insets_changed(&self, callback: Box<dyn FnMut(WindowInsets)>) {
        self.callbacks.borrow_mut().insets_changed = Some(callback);
    }

    fn show_soft_keyboard(&self) {
        // This is an explicit user-gesture request. Do not suppress it based
        // on cached visibility: the system can dismiss the IME while GPUI
        // focus remains on the same input.
        self.request_keyboard();
    }

    fn hide_soft_keyboard(&self) {
        self.hide_keyboard_if_needed();
    }

    fn text_input_state_changed(&self, change: TextInputStateChange) {
        match change {
            TextInputStateChange::FocusGained => {
                self.set_native_text_input_active(true);
                self.invalidate_pending_edit();
                self.show_keyboard_if_needed();
            }
            TextInputStateChange::FocusLost => {
                self.set_native_text_input_active(false);
                self.invalidate_pending_edit();
                self.hide_keyboard_if_needed();
                // Hiding the keyboard keeps the native editor attached. Vim command
                // mode needs hardware keys to reach GPUI instead of that IME session.
                self.detach_ime();
            }
            TextInputStateChange::SelectionChanged | TextInputStateChange::ContentChanged => {
                self.invalidate_pending_edit();
                self.schedule_ime_text_update();
            }
        }
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.callbacks.borrow_mut().resize = Some(callback);
        self.viewport.reset();
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().moved = Some(callback);
    }

    fn on_display_changed(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().display_changed = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.callbacks.borrow_mut().should_close = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.callbacks.borrow_mut().hit_test_window_control = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().appearance_changed = Some(callback);
    }

    fn on_button_layout_changed(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().button_layout_changed = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        if self.closed.get()
            || !self.surface_available.get()
            || self.visibility.get() != WindowVisibility::Visible
        {
            return;
        }
        if self.defer_presentation.get() {
            // Refreshing the focused input handler needs GPUI's paint pass, but
            // submitting each intermediate scene blocks the next IME edit. GPUI
            // clears its presentation flag even here, so retain our own demand.
            self.presentation.request();
            self.frames.request();
            return;
        }
        // Initialize renderer lazily if not already initialized
        // This ensures native_window is available (after SurfaceCreate event)
        if self.renderer.borrow().is_none() {
            if let Err(e) = self.initialize_renderer() {
                warn!("OhosWindow: Failed to initialize renderer in draw(): {}", e);
                self.presentation.record(Presentation::Unavailable);
                return;
            }
        }

        // Use WGPU renderer to render the scene.
        if let Some(ref mut renderer) = *self.renderer.borrow_mut() {
            let presentation = renderer.draw(scene);
            if self.file_access.on_presentation(presentation) {
                let file_access = self.file_access.clone();
                self.background_executor
                    .spawn(async move {
                        if let Err(error) = file_access.restore() {
                            warn!("Failed to restore OHOS file access: {error:#}");
                        }
                    })
                    .detach();
            }
            if self.presentation.record(presentation) {
                self.frames.request();
            }
        } else {
            self.presentation.record(Presentation::Unavailable);
            warn!("OhosWindow: draw called but renderer is not available");
        }
    }

    fn schedule_frame(&self) {
        self.frames.request();
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        if let Some(ref renderer) = *self.renderer.borrow() {
            renderer.sprite_atlas().clone()
        } else if let Some(atlas) = self.fallback_atlas.borrow().as_ref() {
            atlas.clone()
        } else {
            if let Err(error) = self.initialize_renderer() {
                panic!("OhosWindow: renderer must be initialized before sprite_atlas: {error}");
            }
            self.renderer
                .borrow()
                .as_ref()
                .expect("renderer should be initialized after initialize_renderer")
                .sprite_atlas()
                .clone()
        }
    }

    fn request_decorations(&self, decorations: WindowDecorations) {
        self.requested_decorations.set(decorations);
        info!(
            "OHOS window {} requested decorations: {decorations:?}",
            self.window_id
        );
        // Full-screen mobile/tablet main stages have no window frame controls until
        // entering free-window mode; apply_window_status reapplies the retained request.
        if !self.has_window_controls() {
            return;
        }
        // GPUI can request decorations before asynchronous OS window creation
        // finishes. SurfaceCreate reapplies the retained request once ready.
        if self
            .app
            .borrow()
            .as_ref()
            .is_none_or(|app| app.native_window_for(self.window_id).is_none())
        {
            return;
        }
        let Some(client) = self.window_client() else {
            return;
        };
        let window_id = self.window_id;
        let state = self.decorations.clone();
        let window_index = self.window_index.clone();
        self.foreground_executor
            .spawn(async move {
                match client
                    .set_window_decorations(window_id, decorations == WindowDecorations::Server)
                    .await
                {
                    Ok(()) => {
                        info!("OHOS window {window_id} applied decorations: {decorations:?}");
                        if state.replace(decorations) != decorations {
                            let window = window_index.upgrade().and_then(|index| {
                                index.borrow().get(&window_id).and_then(Weak::upgrade)
                            });
                            if let Some(window) = window {
                                let window = window.borrow();
                                if Rc::ptr_eq(&window.decorations, &state)
                                    && !window.released.get()
                                    && !window.closed.get()
                                {
                                    window.force_render.set(true);
                                    window.schedule_frame();
                                }
                            }
                        }
                    }
                    Err(error) => warn!("Failed to set OHOS window decorations: {error}"),
                }
            })
            .detach();
    }

    fn show_window_menu(&self, position: Point<Pixels>) {
        if !self.window_controls().window_menu {
            return;
        }
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let mut items = Vec::new();
        let mut add_command = |command: &str, text: &str| {
            items.push(MenuCommandItem {
                id: format!("window-{}:gpui-window-{command}", self.window_id),
                text: text.into(),
                enabled: true,
            });
        };
        if self.is_minimizable {
            add_command("minimize", "Minimize");
        }
        if self.is_resizable {
            add_command(
                "zoom",
                if self.maximized.get() {
                    "Restore"
                } else {
                    "Maximize"
                },
            );
        }
        add_command(
            "fullscreen",
            if self.fullscreen.get() {
                "Exit fullscreen"
            } else {
                "Fullscreen"
            },
        );
        add_command("close", "Close");
        let request = MenuPopupCommandsRequest {
            window_id: if self.window_id == 0 {
                "main".into()
            } else {
                self.window_id.to_string()
            },
            items,
            x: f64::from(position.x.as_f32().max(0.0)),
            y: f64::from(position.y.as_f32().max(0.0)),
            restore_focus_id: self.native_menu_focus_id(),
        };
        self.foreground_executor
            .spawn(async move {
                let result = async { MenuClient::new(&app)?.popup_commands(request).await }.await;
                if let Err(error) = result {
                    warn!("Failed to open OHOS window menu: {error}");
                }
            })
            .detach();
    }

    fn start_window_move(&self) {
        if self.manipulation.get().is_some() {
            return;
        }
        self.begin_manipulation(None);
        if self.manipulation.get().is_some() {
            if let Some(subscription) = self.move_subscription.borrow().as_ref() {
                subscription.request();
            }
        }
    }

    fn can_start_external_drag(&self) -> bool {
        !self.closed.get()
            && !self.native_drag_pending.get()
            && self.native_pointer_id.get().is_some()
            && (self.pressed_mouse_button.get().first().is_some()
                || !self.active_touches.borrow().is_empty())
            && self
                .native_drag
                .borrow()
                .as_ref()
                .is_none_or(|drag| drag.ended())
    }
    fn start_external_drag(&self, payload: &crate::ExternalDragPayload) -> bool {
        if !self.can_start_external_drag() {
            return false;
        }
        let Some(app) = self.app.borrow().clone() else {
            return false;
        };
        let crate::ExternalDragPayload::Files(paths) = payload;
        let files = paths
            .entries()
            .iter()
            .map(|(path, directory)| {
                let path = path
                    .to_str()
                    .ok_or_else(|| "Drag path is not UTF-8".to_owned())?;
                ohos_fileuri_binding::get_uri_from_path(path)
                    .map(|uri| (uri, *directory))
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, String>>();
        let files = match files {
            Ok(files) => files,
            Err(error) => {
                warn!("Cannot start OHOS file drag: {error}");
                return false;
            }
        };
        // GPUI promotes an active mouse drag after it leaves the viewport.
        info!(
            "OHOS native file drag input pointer {:?}",
            self.native_pointer_id.get()
        );
        let window_id = self.window_id;
        let native_drag = self.native_drag.clone();
        let pending = self.native_drag_pending.clone();
        let callbacks = self.callbacks.clone();
        let pointer = self
            .native_pointer_id
            .get()
            .filter(|id| (0..=9).contains(id))
            .unwrap_or(0);
        pending.set(true);
        app.clone().queue_after_input(move || {
            let Some(node) = app
                .with_xcomponent_for(window_id, openharmony_ability::NativeFileDrag::node_handle)
            else {
                pending.set(false);
                Self::dispatch_input_with_callbacks(
                    &callbacks,
                    PlatformInput::FileDrop(crate::FileDropEvent::Ended),
                );
                return;
            };
            // ArkUI's mouse ID is outside the drag-action range. The SDK
            // documents 0 as the default ID for a single drag.
            match openharmony_ability::NativeFileDrag::start(
                node,
                pointer,
                &files,
                app.create_waker(),
            ) {
                Ok(session) => {
                    *native_drag.borrow_mut() = Some(session);
                    info!("OHOS native file drag started for window {window_id}");
                }
                Err(error) => {
                    warn!("Failed to start OHOS native file drag: {error}");
                    Self::dispatch_input_with_callbacks(
                        &callbacks,
                        PlatformInput::FileDrop(crate::FileDropEvent::Ended),
                    );
                }
            }
            pending.set(false);
        });
        true
    }

    fn start_window_resize(&self, edge: ResizeEdge) {
        self.begin_manipulation(Some(edge));
    }

    fn window_decorations(&self) -> crate::Decorations {
        if self.decorations.get() == WindowDecorations::Client && self.has_window_controls() {
            let tiling = if self.fullscreen.get() || self.maximized.get() {
                crate::Tiling::tiled()
            } else {
                crate::Tiling::default()
            };
            crate::Decorations::Client { tiling }
        } else {
            crate::Decorations::Server
        }
    }

    fn set_app_id(&mut self, app_id: &str) {
        self.request_app_id(app_id)
    }

    fn map_window(&mut self) -> Result<()> {
        self.request_map()
    }

    fn window_controls(&self) -> WindowControls {
        let available =
            self.window_id != 0 || self.form_factor.is_desktop() || self.free_window.get();
        WindowControls {
            fullscreen: available,
            maximize: available && self.is_resizable,
            minimize: available && self.is_minimizable,
            window_menu: available && self.native_menus,
        }
    }

    fn set_client_inset(&self, _inset: Pixels) {
        // Keyboard avoidance is driven by content_size updates from avoid-area overlap.
        // client_inset is intentionally ignored on OHOS.
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        // Return GPU specs from the WGPU renderer.
        self.renderer
            .borrow()
            .as_ref()
            .map(|renderer| renderer.gpu_specs())
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn update_ime_position(&self, bounds: Bounds<Pixels>) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let scale = self.scale_factor() as f64;
        let window = app.window_rect_for(self.window_id);
        let content = app.content_rect_for(self.window_id);
        // GPUI supplies content-local logical pixels; the NDK requires absolute
        // physical screen coordinates, including window and XComponent offsets.
        let rect = ImeCursorRect {
            left: window.left as f64
                + content.left as f64
                + bounds.origin.x.as_f32() as f64 * scale,
            top: window.top as f64 + content.top as f64 + bounds.origin.y.as_f32() as f64 * scale,
            width: (bounds.size.width.as_f32() as f64 * scale).max(1.0),
            height: (bounds.size.height.as_f32() as f64 * scale).max(1.0),
        };
        self.update_ime(true, |ime| ime.try_update_cursor(rect));
    }
}

use log::warn;

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    path::{Component, Path, PathBuf},
    rc::{Rc, Weak},
    sync::atomic::{AtomicUsize, Ordering},
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::{Context as _, Result};
use futures::channel::oneshot;
use openharmony_ability::{
    create_os_window, drain_pending_window_closes, drain_pending_window_status, ColorMode, Event,
    OpenHarmonyApp, TouchInputDelivery, WindowCreateParams,
};
use openharmony_ability_plugin_app_control::{
    AppControlBridgePlugin, RestartRequest, RestartResponse, SetColorModeRequest,
    SetColorModeResponse, TerminateRequest, TerminateResponse,
};
use openharmony_ability_plugin_clipboard::ClipboardBridgePlugin;
use openharmony_ability_plugin_files::{
    dialog_type, FileDialogOptions, FilesBridgePlugin, FilesExt as _,
};
use openharmony_ability_plugin_menu as menu_plugin;

use menu_plugin::{
    register_menu_event_sender, register_menu_open_event_sender, MenuBridgePlugin, MenuClient,
    MenuItemData, MenuSetMenubarRequest,
};
use openharmony_ability_plugin_notification as notification_plugin;

use notification_plugin::{
    NotificationAction, NotificationBridgePlugin, NotificationClient, ShowNotificationRequest,
};
pub(super) use openharmony_ability_plugin_system_state as system_state_plugin;

use openharmony_ability_plugin_url::{UrlBridgePlugin, UrlExt as _};
use openharmony_ability_plugin_window::{WindowBridgePlugin, WindowClient, WindowPromptRequest};
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};
use smallvec::SmallVec;
use system_state_plugin::{
    register_system_state_event_sender, SystemStateBridgePlugin, SystemStateChangedEvent,
    SystemStateClient,
};
use url::Url;

use crate::{
    Action, ActivityGuard, AnyWindowHandle, AppLifecyclePhase, BackgroundExecutor, ClipboardItem,
    ClipboardReadError, CursorStyle, ForegroundExecutor, GestureKinds, GestureTuning, KeyContext,
    Keymap, Menu, MenuItem, OperatingSystem, OwnedMenu, OwnedMenuItem, PathPromptOptions, Platform,
    PlatformDisplay, PlatformGestures, PlatformInfo, PlatformKeyboardLayout,
    PlatformKeyboardMapper, PlatformTarget, PlatformTextSystem, PlatformWindow,
    PriorityQueueReceiver, Result as GpuiResult, RunnableVariant, ScrollPhysics,
    SystemNotification, SystemNotificationResponse, Task, ThermalState, WindowAppearance,
    WindowParams, WindowVisibility,
};

use super::{
    cursor_visibility::CursorVisibility,
    dispatcher::OhosDispatcher,
    display::OhosDisplay,
    launch::LaunchState,
    screen_capture,
    text_system::OhosTextSystem,
    wgpu_context::WgpuContext,
    window::{OhosWindow, OhosWindowContext},
};

type OpenUrlsCallback = Rc<RefCell<Option<Box<dyn FnMut(Vec<String>)>>>>;
type LifecycleCallback = Rc<RefCell<Option<Box<dyn FnMut(AppLifecyclePhase)>>>>;
type PlatformEventCallback = Rc<RefCell<Option<Box<dyn FnMut()>>>>;
type NotificationResponseCallback = Rc<RefCell<Option<Box<dyn FnMut(SystemNotificationResponse)>>>>;
type QuitCallback = Rc<RefCell<Option<Box<dyn FnMut() -> bool>>>>;
type MenuValidationCallback = Box<dyn FnMut(&dyn Action) -> bool>;
type MenuActionCallback = Box<dyn FnMut(&dyn Action)>;

pub(crate) struct OhosPlatform {
    app: Rc<RefCell<Option<OpenHarmonyApp>>>,
    form_factor: super::device::DeviceFormFactor,
    last_button_layout_enabled: Rc<Cell<bool>>,
    dispatcher: Arc<OhosDispatcher>,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<OhosTextSystem>,
    primary_display: Rc<RefCell<Option<OhosDisplay>>>,
    other_displays: Rc<RefCell<Vec<OhosDisplay>>>,
    displays_changed: PlatformEventCallback,
    main_receiver: Rc<RefCell<PriorityQueueReceiver<RunnableVariant>>>,
    gpu_context: Arc<OnceLock<Result<Arc<WgpuContext>, String>>>,
    windows: Rc<RefCell<Vec<Weak<RefCell<OhosWindow>>>>>,
    window_index: Rc<RefCell<FxHashMap<i64, Weak<RefCell<OhosWindow>>>>>,
    pending_window_inputs: Rc<RefCell<FxHashMap<i64, VecDeque<openharmony_ability::InputEvent>>>>,
    pending_window_status: Rc<RefCell<FxHashMap<i64, i32>>>,
    frame_callback_active: Rc<Cell<bool>>,
    open_urls: OpenUrlsCallback,
    pending_open_uris: Rc<RefCell<VecDeque<String>>>,
    file_access: Arc<super::file_access::FileAccess>,
    notification_response: NotificationResponseCallback,
    notification_responses: Rc<RefCell<VecDeque<SystemNotificationResponse>>>,
    app_lifecycle: LifecycleCallback,
    on_quit: QuitCallback,
    quit_started: Rc<Cell<bool>>,
    on_reopen: PlatformEventCallback,
    was_hidden: Rc<Cell<bool>>,
    hidden_window_ids: Rc<RefCell<Option<Vec<i64>>>>,
    focus_before_hide: Rc<Cell<Option<i64>>>,
    memory_warning: PlatformEventCallback,
    system_sleep: PlatformEventCallback,
    system_wake: PlatformEventCallback,
    sleeping: Rc<Cell<bool>>,
    thermal_state: Rc<Cell<ThermalState>>,
    thermal_state_change: PlatformEventCallback,
    system_state_events: Arc<Mutex<VecDeque<SystemStateChangedEvent>>>,
    bundle_code_dir: Arc<Mutex<Option<PathBuf>>>,
    window_stack_state: Rc<RefCell<Option<Vec<i64>>>>,
    window_stack_pending: Rc<Cell<bool>>,
    window_stack_generation: Rc<Cell<u64>>,
    keyboard_layout_change: PlatformEventCallback,
    keyboard_language: Rc<RefCell<Option<String>>>,
    appearance_override: Rc<Cell<Option<WindowAppearance>>>,
    clipboard: super::clipboard::OhosClipboard,
    cursor_visibility: Rc<RefCell<CursorVisibility>>,
    cursor_style: Rc<Cell<Option<(i64, CursorStyle)>>>,
    cursor_window_id: Rc<Cell<i64>>,
    idle_sleep: Arc<Mutex<IdleSleepState>>,
    native_menus: bool,
    menus: Rc<RefCell<MenuState>>,
    menu_events: Arc<Mutex<VecDeque<String>>>,
    menu_open_events: Arc<AtomicUsize>,
}

#[derive(Default)]
struct MenuState {
    menus: Option<Vec<OwnedMenu>>,
    json: String,
    actions: HashMap<String, Box<dyn Action>>,
    next_id: u64,
    on_action: Option<MenuActionCallback>,
    on_will_open: Option<Box<dyn FnMut()>>,
    on_validate: Option<MenuValidationCallback>,
}

#[derive(Default)]
struct IdleSleepState {
    guards: usize,
    applied: bool,
    reconciling: bool,
    waiters: Vec<oneshot::Sender<Result<ActivityGuard>>>,
}

fn reconcile_idle_sleep(
    state: Arc<Mutex<IdleSleepState>>,
    client: WindowClient,
    background: BackgroundExecutor,
) {
    {
        let mut state = state.lock().unwrap();
        if state.reconciling {
            return;
        }
        state.reconciling = true;
    }
    background
        .clone()
        .spawn(async move {
            loop {
                let (desired, waiters) = {
                    let mut state = state.lock().unwrap();
                    let desired = state.guards > 0;
                    let waiters = if desired == state.applied {
                        state.reconciling = false;
                        Some(std::mem::take(&mut state.waiters))
                    } else {
                        None
                    };
                    (desired, waiters)
                };
                if let Some(waiters) = waiters {
                    for waiter in waiters {
                        let state = state.clone();
                        let client = client.clone();
                        let background = background.clone();
                        let guard = ActivityGuard::new(move || {
                            state.lock().unwrap().guards -= 1;
                            reconcile_idle_sleep(state, client, background);
                        });
                        let _ = waiter.send(Ok(guard));
                    }
                    return;
                }
                match client.set_keep_screen_on(0, desired).await {
                    Ok(()) => state.lock().unwrap().applied = desired,
                    Err(error) => {
                        warn!("Failed to set OHOS keep screen on to {desired}: {error}");
                        let waiters = {
                            let mut state = state.lock().unwrap();
                            if desired {
                                let waiters = std::mem::take(&mut state.waiters);
                                state.guards -= waiters.len();
                                waiters
                            } else if state.guards == 0 {
                                state.reconciling = false;
                                return;
                            } else {
                                Vec::new()
                            }
                        };
                        for waiter in waiters {
                            let _ = waiter.send(Err(anyhow::anyhow!(error.to_string())));
                        }
                    }
                }
            }
        })
        .detach();
}

fn apply_menu_validation(items: &mut [MenuItemData], enabled: &HashMap<String, bool>) -> bool {
    let mut changed = false;
    for item in items {
        if let Some(value) = enabled.get(&item.id) {
            if item.enabled != Some(*value) {
                item.enabled = Some(*value);
                changed = true;
            }
        }
        if let Some(children) = item.submenu_items.as_mut() {
            changed |= apply_menu_validation(children, enabled);
        }
    }
    changed
}

fn scope_menu_items(items: &mut [MenuItemData], window_id: i64) {
    for item in items {
        // The pinned menu bridge forwards only menu_id, discarding window_id.
        // Keep the origin in the identifier across that boundary.
        item.id = format!("window-{window_id}:{}", item.id);
        if let Some(children) = item.submenu_items.as_mut() {
            scope_menu_items(children, window_id);
        }
    }
}

fn menu_items(
    items: &[OwnedMenuItem],
    state: &mut MenuState,
    keymap: &Keymap,
) -> Vec<MenuItemData> {
    items
        .iter()
        .map(|item| {
            let id = format!("gpui-menu-{}", state.next_id);
            state.next_id += 1;
            let (item_type, text, enabled, checked, submenu_items, accelerator) = match item {
                OwnedMenuItem::Separator => ("separator", None, None, None, None, None),
                OwnedMenuItem::Submenu(menu) => (
                    "submenu",
                    Some(menu.name.to_string()),
                    Some(!menu.disabled),
                    None,
                    Some(menu_items(&menu.items, state, keymap)),
                    None,
                ),
                OwnedMenuItem::SystemMenu(menu) => (
                    "submenu",
                    Some(menu.name.to_string()),
                    Some(true),
                    None,
                    Some(Vec::new()),
                    None,
                ),
                OwnedMenuItem::Action {
                    name,
                    action,
                    checked,
                    disabled,
                    ..
                } => {
                    // Match the desktop menu's workspace/editor context instead of
                    // showing a shortcut that belongs only to an auxiliary window.
                    static DEFAULT_CONTEXT: OnceLock<Vec<KeyContext>> = OnceLock::new();
                    let context = DEFAULT_CONTEXT.get_or_init(|| {
                        let mut context = KeyContext::new_with_defaults(OperatingSystem::Linux);
                        context.add("Workspace");
                        context.add("Pane");
                        context.add("Editor");
                        vec![context]
                    });
                    let binding = keymap
                        .bindings_for_action(action.as_ref())
                        .rfind(|binding| {
                            binding
                                .predicate()
                                .is_none_or(|predicate| predicate.eval(context))
                        })
                        .or_else(|| keymap.bindings_for_action(action.as_ref()).last());
                    let accelerator = binding.map(|binding| {
                        binding
                            .keystrokes()
                            .iter()
                            .map(|key| {
                                let modifiers = key.modifiers();
                                let mut parts = Vec::new();
                                if modifiers.control || modifiers.platform {
                                    parts.push("Ctrl");
                                }
                                if modifiers.shift {
                                    parts.push("Shift");
                                }
                                if modifiers.alt {
                                    parts.push("Alt");
                                }
                                parts.push(key.key());
                                parts.join("+")
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    });
                    state.actions.insert(id.clone(), action.boxed_clone());
                    (
                        "item",
                        Some(name.clone()),
                        Some(!disabled),
                        Some(*checked),
                        None,
                        accelerator,
                    )
                }
            };
            MenuItemData {
                id,
                item_type: item_type.into(),
                text,
                enabled,
                accelerator,
                accelerator_display_only: Some(true),
                // Keep the application's action dispatch while allowing ArkUI to
                // acquire clipboard permission through its native paste control.
                predefined_type: matches!(
                    item,
                    OwnedMenuItem::Action {
                        os_action: Some(crate::OsAction::Paste),
                        ..
                    }
                )
                .then(|| "paste".into()),
                checked,
                icon: None,
                native_icon: None,
                submenu_items,
                about_metadata: None,
            }
        })
        .collect()
}

pub(crate) fn appearance_for_color_mode(mode: ColorMode) -> WindowAppearance {
    match mode {
        ColorMode::Dark => WindowAppearance::Dark,
        ColorMode::Light | ColorMode::NoSet => WindowAppearance::Light,
    }
}

fn ohos_cursor_style(style: CursorStyle) -> i32 {
    match style {
        CursorStyle::Arrow | CursorStyle::DragLink | CursorStyle::ContextualMenu => 0,
        CursorStyle::IBeam | CursorStyle::IBeamCursorForVerticalLayout => 26,
        CursorStyle::Crosshair => 13,
        CursorStyle::ClosedHand => 17,
        CursorStyle::OpenHand => 18,
        CursorStyle::PointingHand => 19,
        CursorStyle::ResizeLeft => 2,
        CursorStyle::ResizeRight => 1,
        CursorStyle::ResizeLeftRight | CursorStyle::ResizeColumn => 5,
        CursorStyle::ResizeUp => 4,
        CursorStyle::ResizeDown => 3,
        CursorStyle::ResizeUpDown | CursorStyle::ResizeRow => 6,
        CursorStyle::ResizeUpLeftDownRight => 12,
        CursorStyle::ResizeUpRightDownLeft => 11,
        CursorStyle::OperationNotAllowed => 15,
        CursorStyle::DragCopy => 14,
    }
}

fn credential_alias(url: &str) -> String {
    format!("gpui-ohos:{:x}", Sha256::digest(url.as_bytes()))
}

impl OhosPlatform {
    pub(super) fn font_catalog(&self) -> super::text_system::FontCatalog {
        super::text_system::FontCatalog::new(self.text_system.clone())
    }

    pub(crate) fn new(app: OpenHarmonyApp, options: super::PlatformOptions) -> Result<Self> {
        let (main_sender, main_receiver) = PriorityQueueReceiver::new();
        let dispatcher = Arc::new(OhosDispatcher::new(main_sender));
        let background_executor = BackgroundExecutor::new(dispatcher.clone());
        let foreground_executor = ForegroundExecutor::new(dispatcher.clone());
        let text_system = Arc::new(OhosTextSystem::new(app.config().language));
        let menu_events = Arc::new(Mutex::new(VecDeque::new()));
        let (menu_sender, menu_receiver) = crossbeam_channel::unbounded();
        register_menu_event_sender(menu_sender);
        let pending_menu_events = menu_events.clone();
        let menu_waker = app.create_waker();
        std::thread::spawn(move || {
            while let Ok(id) = menu_receiver.recv() {
                log::info!("OHOS menu event received: {id}");
                pending_menu_events.lock().unwrap().push_back(id);
                menu_waker.wake();
            }
        });
        let menu_open_events = Arc::new(AtomicUsize::new(0));
        let (menu_open_sender, menu_open_receiver) = crossbeam_channel::unbounded();
        register_menu_open_event_sender(menu_open_sender);
        let pending_menu_open_events = menu_open_events.clone();
        let menu_open_waker = app.create_waker();
        std::thread::spawn(move || {
            while menu_open_receiver.recv().is_ok() {
                pending_menu_open_events.fetch_add(1, Ordering::Release);
                menu_open_waker.wake();
            }
        });
        let system_state_events = Arc::new(Mutex::new(VecDeque::new()));
        let bundle_code_dir = Arc::new(Mutex::new(None));
        let (system_state_sender, system_state_receiver) = crossbeam_channel::unbounded();
        register_system_state_event_sender(system_state_sender.clone());
        let pending_system_state_events = system_state_events.clone();
        let system_state_waker = app.create_waker();
        std::thread::spawn(move || {
            while let Ok(event) = system_state_receiver.recv() {
                pending_system_state_events.lock().unwrap().push_back(event);
                system_state_waker.wake();
            }
        });

        let file_access = Arc::new(super::file_access::FileAccess::new(
            app.base_path(),
            options.persist_file_access,
        ));
        let gpu_context = Arc::new(OnceLock::new());
        // EGL initialization can block in the emulator driver for several seconds.
        // WGPU owns this context and releases its thread binding between operations;
        // only native-window attachment and presentation need to stay on the UI thread.
        drop(
            std::thread::Builder::new()
                .name("OhosGpuInit".into())
                .spawn({
                    let gpu_context = gpu_context.clone();
                    let dispatcher = dispatcher.clone();
                    move || {
                        let result = match std::panic::catch_unwind(WgpuContext::new) {
                            Ok(result) => {
                                result.map(Arc::new).map_err(|error| format!("{error:#}"))
                            }
                            Err(panic) => {
                                let message = panic
                                    .downcast_ref::<String>()
                                    .map(String::as_str)
                                    .or_else(|| panic.downcast_ref::<&str>().copied())
                                    .unwrap_or("unknown GPU initialization panic");
                                Err(message.to_owned())
                            }
                        };
                        if gpu_context.set(result).is_err() {
                            log::error!("OHOS GPU initialization completed more than once");
                        }
                        dispatcher.wake_main_thread();
                    }
                })?,
        );

        let app_slot = Rc::new(RefCell::new(None));
        let form_factor = super::device::DeviceFormFactor::detect();
        let clipboard = super::clipboard::OhosClipboard::new(
            app_slot.clone(),
            foreground_executor.clone(),
            background_executor.clone(),
        );
        let platform = Self {
            app: app_slot,
            form_factor,
            last_button_layout_enabled: Rc::new(Cell::new(form_factor.is_desktop())),
            dispatcher,
            background_executor,
            foreground_executor,
            text_system,
            primary_display: Rc::new(RefCell::new(None)),
            other_displays: Rc::new(RefCell::new(Vec::new())),
            displays_changed: Rc::new(RefCell::new(None)),
            main_receiver: Rc::new(RefCell::new(main_receiver)),
            gpu_context,
            windows: Rc::new(RefCell::new(Vec::new())),
            window_index: Rc::new(RefCell::new(FxHashMap::default())),
            pending_window_inputs: Rc::new(RefCell::new(FxHashMap::default())),
            pending_window_status: Rc::new(RefCell::new(FxHashMap::default())),
            frame_callback_active: Rc::new(Cell::new(false)),
            open_urls: Rc::new(RefCell::new(None)),
            pending_open_uris: Rc::new(RefCell::new(VecDeque::new())),
            file_access,
            notification_response: Rc::new(RefCell::new(None)),
            notification_responses: Rc::new(RefCell::new(VecDeque::new())),
            app_lifecycle: Rc::new(RefCell::new(None)),
            on_quit: Rc::new(RefCell::new(None)),
            quit_started: Rc::new(Cell::new(false)),
            on_reopen: Rc::new(RefCell::new(None)),
            was_hidden: Rc::new(Cell::new(false)),
            hidden_window_ids: Rc::new(RefCell::new(None)),
            focus_before_hide: Rc::new(Cell::new(None)),
            memory_warning: Rc::new(RefCell::new(None)),
            system_sleep: Rc::new(RefCell::new(None)),
            system_wake: Rc::new(RefCell::new(None)),
            sleeping: Rc::new(Cell::new(false)),
            thermal_state: Rc::new(Cell::new(ThermalState::Nominal)),
            thermal_state_change: Rc::new(RefCell::new(None)),
            system_state_events,
            bundle_code_dir: bundle_code_dir.clone(),
            window_stack_state: Rc::new(RefCell::new(None)),
            window_stack_pending: Rc::new(Cell::new(false)),
            window_stack_generation: Rc::new(Cell::new(0)),
            keyboard_layout_change: Rc::new(RefCell::new(None)),
            keyboard_language: Rc::new(RefCell::new(None)),
            appearance_override: Rc::new(Cell::new(None)),
            clipboard,
            cursor_visibility: Rc::new(RefCell::new(CursorVisibility::default())),
            cursor_style: Rc::new(Cell::new(None)),
            cursor_window_id: Rc::new(Cell::new(0)),
            idle_sleep: Arc::new(Mutex::new(IdleSleepState::default())),
            native_menus: options.native_menus,
            menus: Rc::new(RefCell::new(MenuState::default())),
            menu_events,
            menu_open_events,
        };
        platform.set_app(app);
        if let Some(app) = platform.app.borrow().clone() {
            platform
                .background_executor
                .spawn(async move {
                    let client = match SystemStateClient::new(&app) {
                        Ok(client) => client,
                        Err(error) => {
                            warn!("Failed to create OHOS system-state client: {error}");
                            return;
                        }
                    };
                    match client.thermal_level().await {
                        Ok(level) => {
                            let _ = system_state_sender.send(SystemStateChangedEvent {
                                kind: "thermal".into(),
                                thermal_level: Some(level),
                                available_area: None,
                                displays: None,
                            });
                        }
                        Err(error) => warn!("Failed to read OHOS thermal state: {error}"),
                    }
                    match client.bundle_code_dir().await {
                        Ok(path) if Path::new(&path).is_absolute() => {
                            log::info!("OHOS bundle code directory: {path}");
                            *bundle_code_dir.lock().unwrap() = Some(PathBuf::from(path));
                        }
                        Ok(path) => warn!("OHOS bundle code directory is not absolute: {path}"),
                        Err(error) => warn!("Failed to read OHOS bundle code directory: {error}"),
                    }
                    match client.available_area().await {
                        Ok(area) => {
                            let _ = system_state_sender.send(SystemStateChangedEvent {
                                kind: "available-area".into(),
                                thermal_level: None,
                                available_area: Some(area),
                                displays: None,
                            });
                        }
                        Err(error) => warn!("Failed to read OHOS available area: {error}"),
                    }
                    match client.displays().await {
                        Ok(displays) => {
                            let _ = system_state_sender.send(SystemStateChangedEvent {
                                kind: "displays".into(),
                                thermal_level: None,
                                available_area: None,
                                displays: Some(displays),
                            });
                        }
                        Err(error) => warn!("Failed to enumerate OHOS displays: {error}"),
                    }
                })
                .detach();
        }
        Ok(platform)
    }

    pub(crate) fn set_app(&self, app: OpenHarmonyApp) {
        let windows = self.windows.clone();
        app.on_back_press_intercept(move || {
            let handler = windows
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .find_map(|window| {
                    let window = window.borrow();
                    if window.is_active() {
                        let (enabled, handler) = window.back_handler_state();
                        enabled.then_some(handler)
                    } else {
                        None
                    }
                });
            let Some(handler) = handler else {
                return false;
            };
            let mut callback = handler.borrow_mut().take();
            let handled = if let Some(ref mut callback) = callback {
                callback();
                true
            } else {
                false
            };
            *handler.borrow_mut() = callback;
            handled
        });
        if let Err(error) = app.set_touch_input_delivery(TouchInputDelivery::Both) {
            warn!("Failed to configure system pan and raw control input for GPUI: {error}");
        }
        if let Err(error) =
            app.set_keyboard_input_delivery(openharmony_ability::KeyboardInputDelivery::ArkUi)
        {
            warn!("Using raw OHOS keyboard input: {error}");
        }
        if let Err(error) = app.register_plugin(AppControlBridgePlugin) {
            warn!("Failed to register OpenHarmony app-control plugin: {error}");
        }
        if let Err(error) = app.register_plugin(UrlBridgePlugin) {
            warn!("Failed to register OpenHarmony URL plugin: {error}");
        }
        for result in [
            app.register_plugin(ClipboardBridgePlugin),
            app.register_plugin(FilesBridgePlugin),
            app.register_plugin(NotificationBridgePlugin),
            app.register_plugin(SystemStateBridgePlugin),
            app.register_plugin(WindowBridgePlugin),
        ] {
            if let Err(error) = result {
                warn!("Failed to register OpenHarmony platform plugin: {error}");
            }
        }
        if self.native_menus {
            if let Err(error) = app.register_plugin(MenuBridgePlugin) {
                warn!("Failed to register OpenHarmony menu plugin: {error}");
            }
        }
        *self.app.borrow_mut() = Some(app.clone());
        *self.keyboard_language.borrow_mut() = Some(app.config().language);
        // Seed displays synchronously via OH_NativeDisplayManager (API 14+) before
        // async SystemStateBridgePlugin enumeration completes.
        let (primary, others) = super::display::seed_native_displays(&app);
        *self.primary_display.borrow_mut() = Some(primary);
        *self.other_displays.borrow_mut() = others;
        self.dispatcher.set_waker(app.create_waker());
    }

    fn has_window_button_layout(&self) -> bool {
        self.form_factor.is_desktop()
            || self
                .windows
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .any(|window| window.borrow().has_window_controls())
            || self
                .pending_window_status
                .borrow()
                .iter()
                .any(|(&id, &status)| {
                    id != 0 || super::device::is_free_window_status(status) == Some(true)
                })
    }

    fn sync_button_layout(&self) -> bool {
        let enabled = self.has_window_button_layout();
        if self.last_button_layout_enabled.replace(enabled) == enabled {
            return false;
        }
        let windows = self
            .windows
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        for window in windows {
            window.borrow().notify_button_layout_changed();
        }
        true
    }

    fn handle_foreground_wake(&self) {
        self.dispatcher.begin_main_turn();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2);
        for _ in 0..64 {
            let runnable = self.main_receiver.borrow_mut().try_pop().ok().flatten();
            let Some(runnable) = runnable else {
                return;
            };
            OhosDispatcher::execute_runnable(runnable);
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        // A continuation is needed even without input or a display callback.
        self.dispatcher.wake_main_thread();
    }

    fn publish_menu(&self, window_id: i64) {
        if !self.native_menus {
            return;
        }
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let json = self.menus.borrow().json.clone();
        if json.is_empty() {
            return;
        }
        let json_data =
            match serde_json::from_str::<Vec<MenuItemData>>(&json).and_then(|mut items| {
                scope_menu_items(&mut items, window_id);
                serde_json::to_string(&items)
            }) {
                Ok(json) => json,
                Err(error) => {
                    warn!("Failed to scope OHOS menu to window {window_id}: {error}");
                    return;
                }
            };
        self.background_executor
            .spawn(async move {
                let result = async {
                    MenuClient::new(&app)?
                        .set_menubar(MenuSetMenubarRequest {
                            json_data,
                            window_id: if window_id == 0 {
                                "main".into()
                            } else {
                                window_id.to_string()
                            },
                        })
                        .await
                }
                .await;
                if let Err(error) = result {
                    warn!("Failed to publish OHOS menu for window {window_id}: {error}");
                }
            })
            .detach();
    }

    fn dispatch_menu_events(&self) {
        loop {
            let id = self.menu_events.lock().unwrap().pop_front();
            let Some(id) = id else { break };
            let (window_id, action_id) =
                match id.strip_prefix("window-").and_then(|id| id.split_once(':')) {
                    Some((window, action)) => match window.parse::<i64>() {
                        Ok(window) => (Some(window), action),
                        Err(_) => {
                            warn!("Invalid OHOS menu window {id}");
                            continue;
                        }
                    },
                    None => (None, id.as_str()),
                };
            if action_id.starts_with("gpui-window-") {
                let window = window_id.and_then(|window_id| {
                    self.window_index
                        .borrow()
                        .get(&window_id)
                        .and_then(Weak::upgrade)
                });
                if let Some(window) = window {
                    if !OhosWindow::dispatch_window_menu_command(&window, action_id) {
                        warn!("Unknown OHOS window menu item {id}");
                    }
                }
                continue;
            }
            let action = self
                .menus
                .borrow()
                .actions
                .get(action_id)
                .map(|action| action.boxed_clone());
            let Some(action) = action else {
                warn!("Unknown OHOS menu item {id}");
                continue;
            };
            if let Some(window_id) = window_id {
                let windows = self
                    .windows
                    .borrow()
                    .iter()
                    .filter_map(Weak::upgrade)
                    .collect::<Vec<_>>();
                if !windows
                    .iter()
                    .any(|window| window.borrow().window_id() == window_id)
                {
                    continue;
                }
                for window in windows {
                    window.borrow().handle_event(&Event::WindowFocusChanged {
                        window_id,
                        focused: true,
                    });
                }
            }
            // Native menus can still display a snapshot while validation is in flight.
            let mut on_validate = self.menus.borrow_mut().on_validate.take();
            let enabled = on_validate
                .as_mut()
                .is_none_or(|validate| validate(action.as_ref()));
            self.menus.borrow_mut().on_validate = on_validate;
            if !enabled {
                continue;
            }
            log::info!("Dispatching OHOS menu action {id}");
            let mut on_action = self.menus.borrow_mut().on_action.take();
            if let Some(ref mut callback) = on_action {
                callback(action.as_ref());
            }
            self.menus.borrow_mut().on_action = on_action;
        }
    }

    fn dispatch_menu_open_events(&self) {
        let count = self.menu_open_events.swap(0, Ordering::AcqRel);
        for _ in 0..count {
            log::info!("OHOS app menu opening");
            let mut on_will_open = self.menus.borrow_mut().on_will_open.take();
            if let Some(ref mut callback) = on_will_open {
                callback();
            }
            self.menus.borrow_mut().on_will_open = on_will_open;

            let mut on_validate = self.menus.borrow_mut().on_validate.take();
            let Some(ref mut validate) = on_validate else {
                self.menus.borrow_mut().on_validate = on_validate;
                continue;
            };
            let actions = self
                .menus
                .borrow()
                .actions
                .iter()
                .map(|(id, action)| (id.clone(), action.boxed_clone()))
                .collect::<Vec<_>>();
            let enabled = actions
                .into_iter()
                .map(|(id, action)| (id, validate(action.as_ref())))
                .collect::<HashMap<_, _>>();
            log::info!("OHOS app menu validated {} actions", enabled.len());
            self.menus.borrow_mut().on_validate = on_validate;

            let json = self.menus.borrow().json.clone();
            let Ok(mut items) = serde_json::from_str::<Vec<MenuItemData>>(&json) else {
                continue;
            };
            if !apply_menu_validation(&mut items, &enabled) {
                continue;
            }
            match serde_json::to_string(&items) {
                Ok(json) => self.menus.borrow_mut().json = json,
                Err(error) => {
                    warn!("Failed to validate OHOS app menu: {error}");
                    continue;
                }
            }
            self.publish_menu(0);
            for window in self.windows.borrow().iter().filter_map(Weak::upgrade) {
                let id = window.borrow().window_id();
                if id != 0 {
                    self.publish_menu(id);
                }
            }
        }
    }

    fn dispatch_system_state_events(&self) {
        loop {
            let event = self.system_state_events.lock().unwrap().pop_front();
            let Some(event) = event else { break };
            match event.kind.as_str() {
                "thermal" => {
                    let Some(level) = event.thermal_level else {
                        continue;
                    };
                    let state = match level {
                        0 | 1 => ThermalState::Nominal,
                        2 => ThermalState::Fair,
                        3 | 4 => ThermalState::Serious,
                        _ => ThermalState::Critical,
                    };
                    log::info!("OHOS thermal level {level} mapped to {state:?}");
                    if self.thermal_state.replace(state) != state {
                        let mut callback = self.thermal_state_change.borrow_mut().take();
                        if let Some(ref mut callback) = callback {
                            callback();
                        }
                        *self.thermal_state_change.borrow_mut() = callback;
                    }
                }
                "sleep" if !self.sleeping.replace(true) => {
                    log::info!("OHOS system sleep event");
                    let mut callback = self.system_sleep.borrow_mut().take();
                    if let Some(ref mut callback) = callback {
                        callback();
                    }
                    *self.system_sleep.borrow_mut() = callback;
                }
                "wake" if self.sleeping.replace(false) => {
                    log::info!("OHOS system wake event");
                    let mut callback = self.system_wake.borrow_mut().take();
                    if let Some(ref mut callback) = callback {
                        callback();
                    }
                    *self.system_wake.borrow_mut() = callback;
                }
                "available-area" => {
                    if let Some(area) = event.available_area {
                        if let Some(display) = self.primary_display.borrow().as_ref() {
                            display.set_available_area(
                                area.left,
                                area.top,
                                area.width,
                                area.height,
                            );
                        }
                    }
                }
                "displays" => {
                    let Some(displays) = event.displays else {
                        continue;
                    };
                    let Some(app) = self.app.borrow().clone() else {
                        continue;
                    };
                    let default = displays.iter().find(|display| display.is_default).cloned();
                    if let Some(default) = default {
                        if let Some(primary) = self.primary_display.borrow().as_ref() {
                            primary.update_snapshot(default);
                        }
                    }
                    let mut others = self.other_displays.borrow_mut();
                    let previous = std::mem::take(&mut *others);
                    *others = displays
                        .into_iter()
                        .filter(|display| {
                            !display.is_default
                                && display.id >= 0
                                && display.width > 0
                                && display.height > 0
                                && display.density_pixels.is_finite()
                                && display.density_pixels > 0.0
                        })
                        .map(|snapshot| {
                            if let Some(existing) =
                                previous.iter().find(|item| item.id_raw() == snapshot.id)
                            {
                                existing.update_snapshot(snapshot);
                                existing.clone()
                            } else {
                                OhosDisplay::from_snapshot(app.clone(), snapshot)
                            }
                        })
                        .collect();
                    log::info!("OHOS displays enumerated: {}", others.len() + 1);
                    drop(others);
                    let mut callback = self.displays_changed.borrow_mut().take();
                    if let Some(ref mut callback) = callback {
                        callback();
                    }
                    *self.displays_changed.borrow_mut() = callback;
                    // Enumeration can complete after ConfigChanged, which still
                    // observed the preceding display density.
                    let windows = self
                        .windows
                        .borrow()
                        .iter()
                        .filter_map(Weak::upgrade)
                        .collect::<Vec<_>>();
                    for window in windows {
                        window.borrow().refresh_display();
                    }
                }
                _ => {}
            }
        }
    }

    fn queue_notification_uri(&self, uri: &str) -> bool {
        let Ok(parsed) = Url::parse(uri) else {
            return false;
        };
        if parsed.scheme() != "gpui-notification" || parsed.host_str() != Some("response") {
            return false;
        }
        let mut tag = None;
        let mut action_id = None;
        for (key, value) in parsed.query_pairs() {
            match key.as_ref() {
                "tag" => tag = Some(value.into_owned()),
                "action" => action_id = Some(value.into_owned()),
                _ => {}
            }
        }
        if let Some(tag) = tag {
            log::info!("OHOS notification activated: tag={tag}, action={action_id:?}");
            self.notification_responses
                .borrow_mut()
                .push_back(SystemNotificationResponse {
                    tag: tag.into(),
                    action_id: action_id.map(Into::into),
                });
            if let Some(app) = self.app.borrow().as_ref() {
                app.create_waker().wake();
            }
        }
        true
    }

    fn dispatch_notification_responses(&self) {
        while self.notification_response.borrow().is_some() {
            let Some(response) = self.notification_responses.borrow_mut().pop_front() else {
                break;
            };
            let mut callback = self.notification_response.borrow_mut().take();
            if let Some(ref mut callback) = callback {
                callback(response);
            }
            *self.notification_response.borrow_mut() = callback;
        }
    }

    fn set_ability_visible(&self, visible: bool) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let mut window_ids = if visible {
            self.hidden_window_ids.borrow().clone().unwrap_or_else(|| {
                self.windows
                    .borrow()
                    .iter()
                    .filter_map(Weak::upgrade)
                    .find_map(|window| {
                        let window = window.borrow();
                        (window.window_id() == 0 && window.visibility() == WindowVisibility::Hidden)
                            .then_some(vec![0])
                    })
                    .unwrap_or_default()
            })
        } else {
            if self.hidden_window_ids.borrow().is_some() {
                return;
            }
            let windows = self.windows.borrow();
            let mut visible_windows = Vec::new();
            for window in windows.iter().filter_map(Weak::upgrade) {
                let window = window.borrow();
                if window.visibility() == WindowVisibility::Visible {
                    if window.is_active() {
                        self.focus_before_hide.set(Some(window.window_id()));
                    }
                    visible_windows.push(window.window_id());
                }
            }
            if visible_windows.is_empty() {
                self.focus_before_hide.set(None);
                return;
            }
            *self.hidden_window_ids.borrow_mut() = Some(visible_windows.clone());
            self.was_hidden.set(true);
            visible_windows
        };
        // Hide children before the main window; restore the main window first.
        if !visible {
            window_ids.reverse();
        }
        let platform = self.clone();
        self.foreground_executor
            .spawn(async move {
                let result = async {
                    let client = WindowClient::new(&app)?;
                    let mut failed = Vec::new();
                    let mut changed = Vec::new();
                    for id in window_ids {
                        let operation = if visible {
                            client.show_window(id).await
                        } else {
                            client.minimize_window(id).await
                        };
                        if let Err(error) = operation {
                            warn!("Failed to change OHOS window {id} visibility: {error}");
                            failed.push(id);
                        } else {
                            changed.push(id);
                        }
                    }
                    if visible {
                        let focus_id = platform
                            .focus_before_hide
                            .get()
                            .filter(|id| changed.contains(id))
                            .or_else(|| changed.iter().copied().find(|id| *id == 0))
                            .or_else(|| changed.last().copied());
                        if let Some(focus_id) = focus_id {
                            if let Err(error) = client.focus_window(focus_id).await {
                                warn!("Failed to focus restored OHOS window {focus_id}: {error}");
                            }
                        }
                        if failed.is_empty() {
                            platform.focus_before_hide.set(None);
                        }
                    }
                    Ok::<(Vec<i64>, Vec<i64>), anyhow::Error>((failed, changed))
                }
                .await;
                match result {
                    Ok((failed, restored)) if visible => {
                        *platform.hidden_window_ids.borrow_mut() =
                            (!failed.is_empty()).then_some(failed);
                        if !restored.is_empty() {
                            platform.dispatch_reopen();
                        }
                    }
                    Ok((failed, hidden)) => {
                        let any_hidden = !hidden.is_empty();
                        *platform.hidden_window_ids.borrow_mut() = any_hidden.then_some(hidden);
                        if !any_hidden {
                            platform.was_hidden.set(false);
                            platform.focus_before_hide.set(None);
                        }
                        if !failed.is_empty() {
                            warn!("Failed to hide {} OHOS windows", failed.len());
                        }
                    }
                    Err(error) => {
                        if !visible {
                            *platform.hidden_window_ids.borrow_mut() = None;
                            platform.was_hidden.set(false);
                            platform.focus_before_hide.set(None);
                        }
                        warn!("Failed to change OpenHarmony window visibility: {error}");
                    }
                }
            })
            .detach();
    }

    fn invalidate_window_stack(&self) {
        *self.window_stack_state.borrow_mut() = None;
        self.window_stack_generation
            .set(self.window_stack_generation.get().wrapping_add(1));
        self.refresh_window_stack();
    }

    fn refresh_window_stack(&self) {
        if self.window_stack_state.borrow().is_some() {
            return;
        }
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        if self.window_stack_pending.replace(true) {
            return;
        }
        let generation = self.window_stack_generation.get();
        let platform = self.clone();
        self.foreground_executor
            .spawn(async move {
                let result = async {
                    let client = WindowClient::new(&app)?;
                    let live = platform
                        .windows
                        .borrow()
                        .iter()
                        .filter_map(Weak::upgrade)
                        .collect::<Vec<_>>();
                    let mut groups: Vec<(i64, Vec<i64>)> = Vec::new();
                    for window in live {
                        let window = window.borrow();
                        let Some(display) = window.display() else {
                            continue;
                        };
                        let display_id = i64::try_from(u64::from(display.id()))?;
                        if let Some((_, ids)) = groups.iter_mut().find(|(id, _)| *id == display_id)
                        {
                            ids.push(window.window_id());
                        } else {
                            groups.push((display_id, vec![window.window_id()]));
                        }
                    }
                    let mut ids = Vec::new();
                    for (display, windows) in groups {
                        ids.extend(client.window_stack(display, windows).await?);
                    }
                    Ok::<_, anyhow::Error>(ids)
                }
                .await;
                platform.window_stack_pending.set(false);
                if generation != platform.window_stack_generation.get() {
                    platform.refresh_window_stack();
                    return;
                }
                match result {
                    Ok(ids) => *platform.window_stack_state.borrow_mut() = Some(ids),
                    Err(error) => log::debug!("OHOS window stack unavailable: {error}"),
                }
            })
            .detach();
    }

    fn dispatch_reopen(&self) {
        if !self.was_hidden.replace(false) {
            return;
        }
        // Returning from the background also emits Start. Reopen is for an
        // application with no windows; Zed creates a workspace in this callback.
        if self
            .windows
            .borrow()
            .iter()
            .any(|window| window.upgrade().is_some())
        {
            return;
        }
        let mut callback = self.on_reopen.borrow_mut().take();
        if let Some(ref mut callback) = callback {
            callback();
        }
        *self.on_reopen.borrow_mut() = callback;
    }

    fn prepare_quit(&self) -> bool {
        if self.quit_started.get() {
            return true;
        }
        let mut callback = self.on_quit.borrow_mut().take();
        let may_quit = callback.as_mut().is_none_or(|callback| callback());
        *self.on_quit.borrow_mut() = callback;
        if may_quit {
            self.quit_started.set(true);
        }
        may_quit
    }

    fn dispatch_open_uris(&self) {
        let Some(mut callback) = self.open_urls.borrow_mut().take() else {
            return;
        };
        let pending = self
            .pending_open_uris
            .borrow_mut()
            .drain(..)
            .collect::<Vec<_>>();
        let mut urls = Vec::with_capacity(pending.len());
        let mut grants = Vec::new();
        for uri in pending {
            match normalize_open_uri(uri) {
                Ok((url, grant)) => {
                    urls.push(url);
                    grants.extend(grant);
                }
                Err(error) => {
                    log::error!("Failed to resolve incoming OHOS file: {error:#}");
                    if let Some(app) = self.app.borrow().clone() {
                        self.background_executor
                            .spawn(show_platform_error(
                                app,
                                0,
                                "Unable to open file",
                                format!("{error:#}"),
                            ))
                            .detach();
                    }
                }
            }
        }
        // The Want already grants access for this process. Keep the initial open
        // callback synchronous so Zed selects its workspace before startup continues.
        retain_file_access(self.file_access.clone(), &self.background_executor, grants);
        if !urls.is_empty() {
            callback(urls);
        }
        *self.open_urls.borrow_mut() = Some(callback);
    }

    fn report_gpu_initialization_failure(&self, error: &str) {
        log::error!("Failed to initialize OHOS GPU: {error}");
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let detail = error.to_owned();
        let prompt = self.background_executor.spawn(async move {
            WindowClient::new(&app)?
                .show_prompt(WindowPromptRequest {
                    window_id: 0,
                    message: "Unable to initialize graphics".into(),
                    detail: Some(detail),
                    level: 2,
                    buttons: vec!["Close".into()],
                    cancel_index: Some(0),
                    anchor: None,
                    appearance: None,
                })
                .await?;
            Ok::<(), anyhow::Error>(())
        });
        let platform = self.clone();
        self.foreground_executor
            .spawn(async move {
                if let Err(error) = prompt.await {
                    log::error!("Failed to show OHOS graphics initialization error: {error}");
                }
                platform.quit();
            })
            .detach();
    }

    fn handle_ohos_event(&self, event: &Event, launch: &mut LaunchState) {
        let redraw = match event {
            Event::WindowRedraw(interval) => Some((0, interval)),
            Event::SubWindowRedraw {
                window_id,
                interval,
            } => Some((*window_id, interval)),
            _ => None,
        };
        if let Some((id, interval)) = redraw {
            // No foreground tasks may run inside the native frame callback, including
            // tasks that change another window's callback registration.
            let _scope = super::frame_delivery::FrameCallbackScope::enter(
                self.frame_callback_active.clone(),
            );
            let window = self.window_index.borrow().get(&id).and_then(Weak::upgrade);
            if let Some(window) = window {
                window
                    .borrow()
                    .handle_event(&Event::WindowRedraw(interval.clone()));
            }
            return;
        }

        // ArkUI can deliver raw contact and recognized Pan callbacks in the
        // same native input batch. Keep pointer and gesture callbacks together
        // without polling unrelated futures or presenting a frame between them.
        let input = match event {
            Event::Input(input) => Some((0, input)),
            Event::SubWindowInput { window_id, event } => Some((*window_id, event)),
            _ => None,
        };
        if let Some((id, input)) = input {
            let window = self.window_index.borrow().get(&id).and_then(Weak::upgrade);
            if let Some(window) = window {
                self.cursor_window_id.set(id);
                window.borrow().handle_input_event(input);
            } else if id == 0 {
                const MAX_PENDING_WINDOW_INPUTS: usize = 64;
                let mut pending = self.pending_window_inputs.borrow_mut();
                let queue = pending.entry(0).or_default();
                if queue.len() >= MAX_PENDING_WINDOW_INPUTS {
                    queue.pop_front();
                }
                queue.push_back(input.clone());
            }
            return;
        }

        if matches!(event, Event::Destroy) {
            launch.cancel();
            self.prepare_quit();
        }
        match event {
            Event::SurfaceCreate => launch.set_surface_ready(true),
            Event::SurfaceDestroy | Event::WindowDestroy => launch.set_surface_ready(false),
            _ => {}
        }
        if matches!(event, Event::Stop) {
            self.was_hidden.set(true);
        } else if matches!(event, Event::Start) {
            if self.hidden_window_ids.borrow().is_some() {
                self.set_ability_visible(true);
            } else {
                self.dispatch_reopen();
            }
        }
        if let Event::ConfigChanged(config) = event {
            // The Ability configuration exposes the active language, but not
            // a separate hardware keyboard layout identifier.
            let changed = self
                .keyboard_language
                .borrow_mut()
                .replace(config.language.clone())
                .is_some_and(|previous| previous != config.language);
            if changed {
                let mut callback = self.keyboard_layout_change.borrow_mut().take();
                if let Some(ref mut callback) = callback {
                    callback();
                }
                *self.keyboard_layout_change.borrow_mut() = callback;
            }
        }
        if matches!(
            event,
            Event::Start | Event::GainedFocus | Event::WindowFocusChanged { focused: true, .. }
        ) {
            self.clipboard.refresh_if_needed();
        }
        let phase = match event {
            Event::Start => Some(AppLifecyclePhase::Foreground),
            Event::GainedFocus => Some(AppLifecyclePhase::Active),
            Event::LostFocus => Some(AppLifecyclePhase::Inactive),
            Event::Stop => Some(AppLifecyclePhase::Background),
            _ => None,
        };
        if let Some(phase) = phase {
            let mut callback = self.app_lifecycle.borrow_mut().take();
            if let Some(ref mut callback) = callback {
                callback(phase);
            }
            *self.app_lifecycle.borrow_mut() = callback;
        }
        if matches!(event, Event::LowMemory) {
            let mut callback = self.memory_warning.borrow_mut().take();
            if let Some(ref mut callback) = callback {
                callback();
            }
            *self.memory_warning.borrow_mut() = callback;
        }
        if let Event::NewWant { uri } = event {
            if !uri.is_empty() && !self.queue_notification_uri(uri) {
                self.pending_open_uris.borrow_mut().push_back(uri.clone());
                self.dispatch_open_uris();
            }
        }
        // Ability orders input and executor wakes in one FIFO. Only a wake turn
        // may run queued futures; doing so inside input would overtake that FIFO.
        if matches!(event, Event::UserEvent) {
            self.handle_foreground_wake();
        }
        // Opening can share a wake with selection; reset shortcuts before the action.
        self.dispatch_menu_open_events();
        self.dispatch_menu_events();
        self.dispatch_system_state_events();
        self.dispatch_notification_responses();

        // Surface creation and GPU initialization can finish in either order. Start
        // before routing this event to windows: the callback can create those windows.
        let mut launched = false;
        if let Some(result) = self.gpu_context.get() {
            if let Some(callback) = launch.take_callback(true) {
                match result {
                    Ok(_) => {
                        callback();
                        launched = true;
                    }
                    Err(error) => self.report_gpu_initialization_failure(error),
                }
            }
        }
        if launched {
            if let Some(app) = self.app.borrow().as_ref() {
                // NativeAbility stores the cold-start Want after onAbilityCreate.
                // GPUI may register on_open_urls before that store happens, so
                // drain it once the first surface and app callbacks are ready.
                let initial_uri = app.take_initial_want_uri();
                if !initial_uri.is_empty() && !self.queue_notification_uri(&initial_uri) {
                    self.pending_open_uris.borrow_mut().push_front(initial_uri);
                    self.dispatch_open_uris();
                }
                self.dispatch_notification_responses();
            }
        }

        if let Event::WindowResize { window_id, .. } = event {
            let window = self
                .window_index
                .borrow()
                .get(window_id)
                .and_then(Weak::upgrade);
            if let Some(window) = window {
                window.borrow().handle_event(event);
            }
            return;
        }

        // Route events to all known OHOS windows without borrowing App.
        // This avoids RefCell borrow conflicts when callbacks trigger app updates.
        let mut live_windows: SmallVec<[Rc<RefCell<OhosWindow>>; 4]> = SmallVec::new();
        {
            let mut windows = self.windows.borrow_mut();
            windows.retain(|weak: &Weak<RefCell<OhosWindow>>| {
                if let Some(window) = weak.upgrade() {
                    live_windows.push(window);
                    true
                } else {
                    false
                }
            });
        }

        self.window_index
            .borrow_mut()
            .retain(|_, window| window.strong_count() != 0);
        let _ = self.sync_button_layout();
        if live_windows.is_empty() {
            warn!("OhosPlatform: No active windows to handle event");
        }

        if matches!(
            event,
            Event::WindowFocusChanged { .. }
                | Event::WindowDestroy
                | Event::SubWindowClosed(_)
                | Event::SurfaceCreate
                | Event::SubWindowSurfaceCreate(_)
                | Event::Start
                | Event::Stop
        ) {
            self.invalidate_window_stack();
        }
        let mut any_status_changed = false;
        let mut changed_windows: SmallVec<[Rc<RefCell<OhosWindow>>; 2]> = SmallVec::new();
        for (window_id, status) in drain_pending_window_status() {
            any_status_changed = true;
            let id = i64::from(window_id);
            self.invalidate_window_stack();
            if let Some(window) = live_windows
                .iter()
                .find(|window| window.borrow().window_id() == id)
            {
                if window.borrow().apply_window_status(status) {
                    changed_windows.push(window.clone());
                }
            } else {
                self.pending_window_status.borrow_mut().insert(id, status);
            }
        }
        if any_status_changed && !self.sync_button_layout() {
            for window in changed_windows {
                window.borrow().notify_button_layout_changed();
            }
        }

        // FloatPage reports native close-button and system closes through this
        // queue. GPUI must consume it so its window registry and close observers
        // are updated even when no SubWindowClosed event is emitted.
        if let Event::SubWindowClosed(window_id) = event {
            self.pending_window_inputs.borrow_mut().remove(window_id);
            self.pending_window_status.borrow_mut().remove(window_id);
        }
        for window_id in drain_pending_window_closes() {
            let id = i64::from(window_id);
            self.pending_window_inputs.borrow_mut().remove(&id);
            self.pending_window_status.borrow_mut().remove(&id);
            self.invalidate_window_stack();
            if let Some(window) = live_windows
                .iter()
                .find(|window| window.borrow().window_id() == id)
            {
                window.borrow().handle_event(&Event::WindowDestroy);
            }
        }

        for window in &live_windows {
            let id = window.borrow().window_id();
            match event {
                Event::TextInputReadyChanged { window_id, .. } if id == *window_id => {
                    window.borrow().handle_event(event)
                }
                Event::SubWindowSurfaceCreate(window_id) if id == *window_id => {
                    window.borrow().handle_event(&Event::SurfaceCreate)
                }
                Event::SubWindowSurfaceDestroy(window_id) if id == *window_id => {
                    window.borrow().handle_event(&Event::SurfaceDestroy)
                }
                Event::SubWindowClosed(window_id) if id == *window_id => {
                    window.borrow().handle_event(&Event::WindowDestroy)
                }
                Event::WindowResize { window_id, .. } if id == *window_id => {
                    window.borrow().handle_event(event)
                }
                Event::ContentRectChange(rect) if id == rect.window_id => {
                    window.borrow().handle_event(event)
                }
                Event::AvoidAreaChange(info) if id == info.window_id => {
                    window.borrow().handle_event(event)
                }
                Event::WindowFocusChanged { window_id, focused }
                    if *focused || id == *window_id =>
                {
                    window.borrow().handle_event(event)
                }
                Event::SurfaceCreate | Event::SurfaceDestroy | Event::WindowDestroy if id == 0 => {
                    window.borrow().handle_event(event)
                }
                Event::SubWindowSurfaceCreate(_)
                | Event::TextInputReadyChanged { .. }
                | Event::SubWindowSurfaceDestroy(_)
                | Event::SubWindowClosed(_)
                | Event::SubWindowRedraw { .. }
                | Event::SubWindowInput { .. }
                | Event::WindowResize { .. }
                | Event::ContentRectChange(_)
                | Event::AvoidAreaChange(_)
                | Event::WindowFocusChanged { .. }
                | Event::SurfaceCreate
                | Event::SurfaceDestroy
                | Event::WindowRedraw(_)
                | Event::Input(_)
                | Event::WindowDestroy => {}
                Event::GainedFocus | Event::LostFocus => {}
                Event::KeyboardEvent(_) if id != 0 => {}
                _ => window.borrow().handle_event(event),
            }
        }
        let _ = self.sync_button_layout();
        match event {
            Event::SurfaceCreate => self.publish_menu(0),
            Event::SubWindowSurfaceCreate(window_id) => self.publish_menu(*window_id),
            _ => {}
        }

        if matches!(event, Event::Destroy) {
            self.pending_open_uris.borrow_mut().clear();
            self.pending_window_inputs.borrow_mut().clear();
            self.pending_window_status.borrow_mut().clear();
            for window in &live_windows {
                window.borrow().handle_event(&Event::WindowDestroy);
            }
            self.dispatcher.shutdown();
            loop {
                let next = self.main_receiver.borrow_mut().try_pop().ok().flatten();
                let Some(runnable) = next else {
                    break;
                };
                drop(runnable);
            }
        }
    }
}

pub(super) fn set_cursor_visibility(
    state: &Rc<RefCell<CursorVisibility>>,
    visible: bool,
    app: OpenHarmonyApp,
    executor: &ForegroundExecutor,
) {
    let Some(mut visible) = state.borrow_mut().request(visible) else {
        return;
    };
    let state = state.clone();
    executor
        .spawn(async move {
            loop {
                let result = async {
                    WindowClient::new(&app)?
                        .try_set_cursor_visible(visible)
                        .await
                }
                .await;
                if matches!(result, Ok(false)) {
                    state.borrow_mut().mark_unsupported();
                    log::info!("OHOS cursor visibility is not supported by this device");
                    break;
                }
                let next = state.borrow_mut().complete(visible, result.is_ok());
                if let Err(error) = result {
                    warn!("Failed to set OHOS cursor visibility to {visible}: {error}");
                }
                let Some(next) = next else {
                    break;
                };
                visible = next;
            }
        })
        .detach();
}

pub(super) fn retain_file_access(
    file_access: Arc<super::file_access::FileAccess>,
    executor: &BackgroundExecutor,
    uris: Vec<String>,
) {
    if uris.is_empty() {
        return;
    }
    executor
        .spawn(async move {
            if let Err(error) = file_access.remember(&uris, false) {
                warn!("Could not save incoming file access: {error:#}");
            }
        })
        .detach();
}

pub(super) async fn show_platform_error(
    app: OpenHarmonyApp,
    window_id: i64,
    message: &'static str,
    detail: String,
) {
    let result = async {
        WindowClient::new(&app)?
            .show_prompt(WindowPromptRequest {
                window_id,
                message: message.into(),
                detail: Some(detail),
                level: 2,
                buttons: vec!["OK".into()],
                cancel_index: Some(0),
                anchor: None,
                appearance: None,
            })
            .await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        log::error!("Failed to show OHOS platform error: {error:#}");
    }
}

fn normalize_open_uri(uri: String) -> Result<(String, Option<String>)> {
    let Ok(parsed) = Url::parse(&uri) else {
        return Ok((uri, None));
    };
    if parsed.scheme() != "file" || parsed.host_str().is_none() {
        return Ok((uri, None));
    }

    // OHOS authorities identify file providers, not filesystem path components.
    // Resolve the granted URI before exposing a standard file URL to GPUI clients.
    let path = ohos_fileuri_binding::get_path_from_uri(&uri)
        .with_context(|| format!("Resolve file URI {uri}"))?;
    Url::from_file_path(&path)
        .map(|url| (String::from(url), Some(uri)))
        .map_err(|()| anyhow::anyhow!("File URI resolved to a non-absolute path: {path}"))
}

fn selected_paths(
    uris: Vec<String>,
    file_access: &super::file_access::FileAccess,
    writable: bool,
) -> Result<Option<Vec<PathBuf>>> {
    if uris.is_empty() {
        return Ok(None);
    }
    let paths = resolve_file_paths(&uris)?;
    if let Err(error) = file_access.remember(&uris, writable) {
        warn!("Could not save file picker access: {error:#}");
    }
    Ok(Some(paths))
}

pub(super) fn resolve_file_paths(uris: &[String]) -> Result<Vec<PathBuf>> {
    uris.iter()
        .map(|uri| {
            let native_path = ohos_fileuri_binding::get_path_from_uri(uri)
                .with_context(|| format!("Convert file URI to path: {uri}"))?;
            let path = PathBuf::from(native_path);
            anyhow::ensure!(
                path.is_absolute(),
                "File URI resolved to a non-absolute path"
            );
            Ok(path)
        })
        .collect()
}

/// OpenHarmony scroll feel: ArkUI's API 12 friction motion uses the
/// default friction coefficient `0.75` and decays velocity according to
/// `v(t) = v₀ · exp(-4.2 · friction · t)` with `t` measured in seconds.
fn ohos_scroll_physics() -> ScrollPhysics {
    const DEFAULT_FRICTION: f32 = 0.75;
    const FRICTION_SCALE: f32 = 4.2;
    const MILLISECONDS_PER_SECOND: f32 = 1000.;

    ScrollPhysics::Exponential {
        decay_per_ms: (-FRICTION_SCALE * DEFAULT_FRICTION / MILLISECONDS_PER_SECOND).exp(),
    }
}

struct OhosGestures;

impl PlatformGestures for OhosGestures {
    fn native_recognizers(&self) -> GestureKinds {
        GestureKinds {
            tap: true,
            long_press: true,
            pan: true,
            pinch: false,
        }
    }

    fn tuning(&self) -> GestureTuning {
        GestureTuning {
            // HarmonyOS touch coordinates are converted to logical pixels before
            // entering GPUI, matching the coordinate space used by Android's
            // portable gesture implementation.
            scroll_physics: ohos_scroll_physics(),
            ..GestureTuning::default()
        }
    }
}

impl Clone for OhosPlatform {
    fn clone(&self) -> Self {
        Self {
            app: self.app.clone(),
            form_factor: self.form_factor,
            last_button_layout_enabled: self.last_button_layout_enabled.clone(),
            dispatcher: self.dispatcher.clone(),
            background_executor: self.background_executor.clone(),
            foreground_executor: self.foreground_executor.clone(),
            text_system: self.text_system.clone(),
            primary_display: self.primary_display.clone(),
            other_displays: self.other_displays.clone(),
            displays_changed: self.displays_changed.clone(),
            main_receiver: self.main_receiver.clone(),
            gpu_context: self.gpu_context.clone(),
            windows: self.windows.clone(),
            window_index: self.window_index.clone(),
            pending_window_inputs: self.pending_window_inputs.clone(),
            pending_window_status: self.pending_window_status.clone(),
            frame_callback_active: self.frame_callback_active.clone(),
            open_urls: self.open_urls.clone(),
            pending_open_uris: self.pending_open_uris.clone(),
            file_access: self.file_access.clone(),
            notification_response: self.notification_response.clone(),
            notification_responses: self.notification_responses.clone(),
            app_lifecycle: self.app_lifecycle.clone(),
            on_quit: self.on_quit.clone(),
            quit_started: self.quit_started.clone(),
            on_reopen: self.on_reopen.clone(),
            was_hidden: self.was_hidden.clone(),
            hidden_window_ids: self.hidden_window_ids.clone(),
            focus_before_hide: self.focus_before_hide.clone(),
            memory_warning: self.memory_warning.clone(),
            system_sleep: self.system_sleep.clone(),
            system_wake: self.system_wake.clone(),
            sleeping: self.sleeping.clone(),
            thermal_state: self.thermal_state.clone(),
            thermal_state_change: self.thermal_state_change.clone(),
            system_state_events: self.system_state_events.clone(),
            bundle_code_dir: self.bundle_code_dir.clone(),
            window_stack_state: self.window_stack_state.clone(),
            window_stack_pending: self.window_stack_pending.clone(),
            window_stack_generation: self.window_stack_generation.clone(),
            keyboard_layout_change: self.keyboard_layout_change.clone(),
            keyboard_language: self.keyboard_language.clone(),
            appearance_override: self.appearance_override.clone(),
            clipboard: self.clipboard.clone(),
            cursor_visibility: self.cursor_visibility.clone(),
            cursor_style: self.cursor_style.clone(),
            cursor_window_id: self.cursor_window_id.clone(),
            idle_sleep: self.idle_sleep.clone(),
            native_menus: self.native_menus,
            menus: self.menus.clone(),
            menu_events: self.menu_events.clone(),
            menu_open_events: self.menu_open_events.clone(),
        }
    }
}

impl Platform for OhosPlatform {
    fn on_app_lifecycle(&self, callback: Box<dyn FnMut(AppLifecyclePhase)>) {
        *self.app_lifecycle.borrow_mut() = Some(callback);
    }

    fn on_memory_warning(&self, callback: Box<dyn FnMut()>) {
        *self.memory_warning.borrow_mut() = Some(callback);
    }

    fn platform_info(&self) -> PlatformInfo {
        PlatformInfo {
            os: OperatingSystem::Linux,
            target: PlatformTarget::Native,
        }
    }

    fn gestures(&self) -> Option<Rc<dyn PlatformGestures>> {
        Some(Rc::new(OhosGestures))
    }

    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        let platform = self.clone();
        let mut launch = LaunchState::new(on_finish_launching);
        if let Some(app) = self.app.borrow().clone() {
            app.run_loop(move |event: Event| {
                platform.handle_ohos_event(&event, &mut launch);
            });
        } else {
            warn!("OhosPlatform: App not set");
        }
    }

    fn quit(&self) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let platform = self.clone();
        let background = self.background_executor.clone();
        // GPUI may call quit while its AppCell is borrowed. Defer the callback
        // until the next foreground turn so shutdown can borrow it safely.
        self.foreground_executor
            .spawn(async move {
                if !platform.prepare_quit() {
                    return;
                }
                background
                    .spawn(async move {
                        let result = async {
                            let response = app
                                .bridge()?
                                .call_sync_from_worker::<
                                    AppControlBridgePlugin,
                                    TerminateRequest,
                                    TerminateResponse,
                                >("terminate", TerminateRequest { code: 0 })
                                .await?;
                            anyhow::ensure!(
                                response.accepted,
                                "OpenHarmony app-control plugin rejected termination"
                            );
                            Ok::<(), anyhow::Error>(())
                        }
                        .await;
                        if let Err(error) = result {
                            warn!("Failed to terminate OpenHarmony application: {error}");
                        }
                    })
                    .detach();
            })
            .detach();
    }

    fn restart(&self, binary_path: Option<PathBuf>, arguments: Vec<std::ffi::OsString>) {
        if binary_path.is_some() || !arguments.is_empty() {
            warn!("OHOS restart resumes the current Ability without replacement arguments");
        }
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let platform = self.clone();
        let window_id = self
            .windows
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .find_map(|window| {
                let window = window.borrow();
                window.is_active().then_some(window.window_id())
            })
            .unwrap_or(0);
        let request = self.background_executor.spawn({
            let app = app.clone();
            async move {
                let response = app.bridge()?.call_sync_from_worker::<
                    AppControlBridgePlugin, RestartRequest, RestartResponse
                >("restart", RestartRequest {}).await?;
                anyhow::ensure!(
                    response.accepted,
                    "OpenHarmony app-control plugin rejected restart"
                );
                Ok::<(), anyhow::Error>(())
            }
        });
        self.foreground_executor
            .spawn(async move {
                match request.await {
                    Ok(()) => {
                        // The SDK can reject restart. Destructive shutdown is safe only after
                        // acceptance; callers must persist restart state before requesting it.
                        platform.prepare_quit();
                    }
                    Err(error) => {
                        warn!("Failed to restart OpenHarmony application: {error:#}");
                        show_platform_error(app, window_id, "Restart failed", error.to_string())
                            .await;
                    }
                }
            })
            .detach();
    }

    fn activate(&self, _ignoring_other_apps: bool) {
        self.set_ability_visible(true);
    }

    fn hide_cursor_until_mouse_moves(&self) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        set_cursor_visibility(
            &self.cursor_visibility,
            false,
            app,
            &self.foreground_executor,
        );
    }

    fn is_cursor_visible(&self) -> bool {
        self.cursor_visibility.borrow().is_visible()
    }

    fn hide(&self) {
        self.set_ability_visible(false);
    }

    fn hide_other_apps(&self) {
        // Not supported on OHOS
    }

    fn unhide_other_apps(&self) {
        // Not supported on OHOS
    }

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        let mut displays = Vec::new();
        if let Some(display) = self.primary_display.borrow().as_ref() {
            displays.push(Rc::new(display.clone()) as Rc<dyn PlatformDisplay>);
        }
        displays.extend(
            self.other_displays
                .borrow()
                .iter()
                .cloned()
                .map(|display| Rc::new(display) as Rc<dyn PlatformDisplay>),
        );
        displays
    }

    fn on_displays_changed(&self, callback: Box<dyn FnMut()>) {
        *self.displays_changed.borrow_mut() = Some(callback);
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        self.primary_display
            .borrow()
            .as_ref()
            .map(|d| Rc::new(d.clone()) as Rc<dyn PlatformDisplay>)
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.windows
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .find_map(|window| {
                let window = window.borrow();
                window.is_active().then_some(window.handle)
            })
    }

    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        self.refresh_window_stack();
        let stack = self.window_stack_state.borrow();
        let stack = stack.as_ref()?;
        let windows = self
            .windows
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        Some(
            stack
                .iter()
                .filter_map(|id| {
                    windows.iter().find_map(|window| {
                        let window = window.borrow();
                        (window.window_id() == *id).then_some(window.handle)
                    })
                })
                .collect(),
        )
    }

    fn is_screen_capture_supported(&self) -> bool {
        if !super::device::can_i_use("SystemCapability.Multimedia.Media.AVScreenCapture") {
            return false;
        }
        if let Some(display) = self.primary_display.borrow().as_ref() {
            let (width, height, _) = display.dimensions_and_scale();
            if width > 0 && height > 0 {
                return true;
            }
        }
        self.app.borrow().as_ref().is_some_and(|app| {
            let (width, height) = app.display_size();
            i32::try_from(width).is_ok_and(|width| width > 0)
                && i32::try_from(height).is_ok_and(|height| height > 0)
        })
    }

    fn screen_capture_sources(
        &self,
    ) -> oneshot::Receiver<GpuiResult<Vec<Rc<dyn crate::ScreenCaptureSource>>>> {
        let mut targets = Vec::new();
        if let Some(display) = self.primary_display.borrow().as_ref() {
            let (width, height, _) = display.dimensions_and_scale();
            targets.push(screen_capture::DisplayCaptureSource {
                display_id: display.id_raw() as u64,
                width,
                height,
                is_main: true,
            });
        }
        for display in self.other_displays.borrow().iter() {
            let (width, height, _) = display.dimensions_and_scale();
            targets.push(screen_capture::DisplayCaptureSource {
                display_id: display.id_raw() as u64,
                width,
                height,
                is_main: false,
            });
        }
        screen_capture::sources(targets)
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        options: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        if matches!(options.kind, gpui::WindowKind::AnchoredPopup(_)) {
            return Err(gpui::popup::PopupNotSupportedError.into());
        }
        let gpu_context = self
            .gpu_context
            .get()
            .context("OHOS GPU initialization has not completed")?
            .as_ref()
            .map_err(|error| anyhow::anyhow!("Failed to initialize OHOS GPU: {error}"))?
            .clone();
        if let Some(app) = self.app.borrow().clone() {
            let primary = self
                .primary_display
                .borrow()
                .as_ref()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Primary OHOS display is unavailable"))?;
            let display = if let Some(id) = options.display_id {
                if primary.id() == id {
                    primary.clone()
                } else {
                    self.other_displays
                        .borrow()
                        .iter()
                        .find(|display| display.id() == id)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("Requested OHOS display is unavailable"))?
                }
            } else {
                primary.clone()
            };
            let existing = self
                .windows
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            let (window_id, fallback_atlas) = if let Some(primary) = existing.first() {
                let atlas = primary
                    .borrow()
                    .atlas()
                    .ok_or_else(|| anyhow::anyhow!("Primary OHOS window has no GPU atlas"))?;
                let scale = display.scale_factor();
                let bounds = options.bounds;
                let display_id = options
                    .display_id
                    .map(|id| i64::try_from(u64::from(id)))
                    .transpose()?;
                let window_id = create_os_window(WindowCreateParams {
                    name: format!("gpui-{}", uuid::Uuid::new_v4()),
                    native_module_name: Some(app.module_name().ok_or_else(|| {
                        anyhow::anyhow!("OHOS native module name is unavailable")
                    })?),
                    width: (bounds.size.width.as_f32() * scale).max(1.0) as i32,
                    height: (bounds.size.height.as_f32() * scale).max(1.0) as i32,
                    x: (bounds.origin.x.as_f32() * scale) as i32,
                    y: (bounds.origin.y.as_f32() * scale) as i32,
                    display_id,
                    independent: matches!(options.kind, gpui::WindowKind::Normal),
                    show: options.show,
                    focus: options.focus,
                    modal: matches!(options.kind, gpui::WindowKind::Dialog),
                    ..Default::default()
                })?;
                (window_id, Some(atlas))
            } else {
                anyhow::ensure!(
                    display.id() == primary.id(),
                    "The OHOS main window is already bound to the default display"
                );
                (0, None)
            };
            let window = OhosWindow::new(OhosWindowContext {
                form_factor: self.form_factor,
                native_menus: self.native_menus,
                windows: Rc::downgrade(&self.windows),
                window_index: Rc::downgrade(&self.window_index),
                app: self.app.clone(),
                handle,
                params: options,
                gpu_context,
                display,
                foreground_executor: self.foreground_executor.clone(),
                background_executor: self.background_executor.clone(),
                frame_callback_active: self.frame_callback_active.clone(),
                clipboard: self.clipboard.clone(),
                file_access: self.file_access.clone(),
                cursor_visibility: self.cursor_visibility.clone(),
                cursor_style: self.cursor_style.clone(),
                appearance_override: self.appearance_override.clone(),
                window_id,
                fallback_atlas,
            })?;

            if let Some(status) = self.pending_window_status.borrow_mut().remove(&window_id) {
                let _ = window.apply_window_status(status);
            }

            // GPUI fetches sprite_atlas during window initialization and caches it.
            // Renderer must be ready at open_window time to avoid caching a broken atlas.
            if window_id == 0 {
                window.initialize_renderer()?;
                window.apply_window_options();
            }

            let window = Rc::new(RefCell::new(window));
            OhosWindow::register_close_handler(&window);
            self.windows.borrow_mut().push(Rc::downgrade(&window));
            self.window_index
                .borrow_mut()
                .insert(window_id, Rc::downgrade(&window));
            if let Some(pending) = self.pending_window_inputs.borrow_mut().remove(&window_id) {
                if !pending.is_empty() {
                    self.cursor_window_id.set(window_id);
                    for input in pending {
                        window.borrow().handle_input_event(&input);
                    }
                }
            }
            self.invalidate_window_stack();
            let _ = self.sync_button_layout();
            Ok(Box::new(super::window::OhosWindowHandle::new(window)))
        } else {
            Err(anyhow::anyhow!("OpenHarmonyApp not set"))
        }
    }

    fn button_layout(&self) -> Option<gpui::WindowButtonLayout> {
        self.has_window_button_layout()
            .then(gpui::WindowButtonLayout::linux_default)
    }

    fn window_appearance(&self) -> WindowAppearance {
        if let Some(appearance) = self.appearance_override.get() {
            return appearance;
        }
        self.app
            .borrow()
            .as_ref()
            .map(|app| appearance_for_color_mode(app.config().color_mode))
            .unwrap_or_default()
    }

    fn set_window_appearance(&self, appearance: Option<WindowAppearance>) {
        self.appearance_override.set(appearance);
        let system_appearance = self
            .app
            .borrow()
            .as_ref()
            .map(|app| appearance_for_color_mode(app.config().color_mode))
            .unwrap_or_default();
        let windows: Vec<_> = self
            .windows
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        self.foreground_executor
            .spawn(async move {
                for window in windows {
                    window
                        .borrow()
                        .set_appearance(appearance.unwrap_or(system_appearance));
                }
            })
            .detach();
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let color_mode = match appearance {
            Some(WindowAppearance::Dark | WindowAppearance::VibrantDark) => 0,
            Some(WindowAppearance::Light | WindowAppearance::VibrantLight) => 1,
            None => 2,
        };
        self.background_executor
            .spawn(async move {
                let result = async {
                    let response = app
                        .bridge()?
                        .call_sync_from_worker::<
                            AppControlBridgePlugin,
                            SetColorModeRequest,
                            SetColorModeResponse,
                        >("set-color-mode", SetColorModeRequest { color_mode })
                        .await?;
                    anyhow::ensure!(response.accepted, "OpenHarmony rejected the color mode");
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if let Err(error) = result {
                    warn!("Failed to set OpenHarmony color mode: {error}");
                }
            })
            .detach();
    }

    fn open_url(&self, url: &str) {
        let Some(app) = self.app.borrow().clone() else {
            warn!("Cannot open URL before OpenHarmonyApp is set: {url}");
            return;
        };
        let url = url.to_owned();
        self.background_executor
            .spawn(async move {
                if let Err(error) = app.open_url(url).await {
                    warn!("Failed to open URL on OHOS: {error}");
                }
            })
            .detach();
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        let initial_uri = self
            .app
            .borrow()
            .as_ref()
            .map(OpenHarmonyApp::take_initial_want_uri)
            .unwrap_or_default();
        if !initial_uri.is_empty() && !self.queue_notification_uri(&initial_uri) {
            self.pending_open_uris.borrow_mut().push_front(initial_uri);
        }
        *self.open_urls.borrow_mut() = Some(callback);
        self.dispatch_open_uris();
    }

    fn show_system_notification(&self, notification: SystemNotification) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        self.background_executor
            .spawn(async move {
                let request = ShowNotificationRequest {
                    tag: notification.tag.to_string(),
                    title: notification.title.to_string(),
                    body: notification.body.to_string(),
                    actions: notification
                        .actions
                        .into_iter()
                        .map(|action| NotificationAction {
                            id: action.id.to_string(),
                            label: action.label.to_string(),
                        })
                        .collect(),
                };
                match async { NotificationClient::new(&app)?.show(request).await }.await {
                    Ok(true) => log::info!("OHOS notification published"),
                    Ok(false) => warn!("OHOS notification was not accepted"),
                    Err(error) => warn!("Failed to publish OHOS notification: {error}"),
                }
            })
            .detach();
    }

    fn dismiss_system_notification(&self, tag: &str) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let tag = tag.to_owned();
        self.background_executor
            .spawn(async move {
                match async { NotificationClient::new(&app)?.dismiss(tag).await }.await {
                    Ok(true) => log::info!("OHOS notification dismissed"),
                    Ok(false) => warn!("OHOS notification dismissal was not accepted"),
                    Err(error) => warn!("Failed to dismiss OHOS notification: {error}"),
                }
            })
            .detach();
    }

    fn on_system_notification_response(
        &self,
        callback: Box<dyn FnMut(SystemNotificationResponse)>,
    ) {
        *self.notification_response.borrow_mut() = Some(callback);
    }

    fn register_url_scheme(&self, scheme: &str) -> Task<Result<()>> {
        let Some(app) = self.app.borrow().clone() else {
            return Task::ready(Err(anyhow::anyhow!("OpenHarmonyApp not set")));
        };
        let scheme = scheme.to_owned();
        self.background_executor.spawn(async move {
            app.check_url_scheme(scheme)
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        })
    }

    fn prompt_for_paths(
        &self,
        options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel();
        let Some(app) = self.app.borrow().clone() else {
            tx.send(Err(anyhow::anyhow!("OpenHarmonyApp not set"))).ok();
            return rx;
        };
        let parent_window_id = self
            .windows
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .find_map(|window| {
                let window = window.borrow();
                window.is_active().then_some(window.window_id())
            });
        let file_access = self.file_access.clone();
        self.background_executor
            .spawn(async move {
                let result = async {
                    anyhow::ensure!(
                        options.files || options.directories,
                        "Select files or directories"
                    );
                    let kind = match (options.files, options.directories) {
                        (true, true) => dialog_type::OPEN_MIXED,
                        (true, false) => dialog_type::OPEN_FILE,
                        _ => dialog_type::OPEN_FOLDER,
                    };
                    let mut options = FileDialogOptions::new(kind).allow_many(options.multiple);
                    options.parent_window_id = parent_window_id;
                    let response = app.show_file_dialog(options).await?;
                    selected_paths(response.files, &file_access, false)
                }
                .await;
                tx.send(result).ok();
            })
            .detach();
        rx
    }

    fn prompt_for_new_path(
        &self,
        directory: &std::path::Path,
        suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (tx, rx) = oneshot::channel();
        let Some(app) = self.app.borrow().clone() else {
            tx.send(Err(anyhow::anyhow!("OpenHarmonyApp not set"))).ok();
            return rx;
        };
        let directory = directory.to_path_buf();
        let suggested_name = suggested_name.map(str::to_owned);
        let file_access = self.file_access.clone();
        let parent_window_id = self
            .windows
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .find_map(|window| {
                let window = window.borrow();
                window.is_active().then_some(window.window_id())
            });
        self.background_executor
            .spawn(async move {
                let result = async {
                    let location = directory
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("Save directory is not UTF-8"))?;
                    let uri = ohos_fileuri_binding::get_uri_from_path(location)?;
                    let mut options =
                        FileDialogOptions::new(dialog_type::SAVE_FILE).default_location(uri);
                    options.suggested_name = suggested_name;
                    options.parent_window_id = parent_window_id;
                    let response = app.show_file_dialog(options).await?;
                    Ok(selected_paths(response.files, &file_access, true)?
                        .and_then(|mut paths| paths.pop()))
                }
                .await;
                tx.send(result).ok();
            })
            .detach();
        rx
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        true
    }

    fn reveal_path(&self, path: &std::path::Path) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let directory = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        let Some(directory) = directory.to_str() else {
            warn!("Cannot reveal a non-UTF-8 path on OHOS");
            return;
        };
        let directory = directory.to_owned();
        self.background_executor
            .spawn(async move {
                if let Err(error) = app.reveal_in_dir(directory).await {
                    warn!("Failed to reveal OHOS directory: {error}");
                }
            })
            .detach();
    }

    fn open_with_system(&self, path: &std::path::Path) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let Some(path) = path.to_str() else {
            warn!("Cannot open a non-UTF-8 path on OHOS");
            return;
        };
        let uri = match ohos_fileuri_binding::get_uri_from_path(path) {
            Ok(uri) => uri,
            Err(error) => {
                warn!("Cannot make OHOS file URI for {path}: {error}");
                return;
            }
        };
        self.background_executor
            .spawn(async move {
                if let Err(error) = app.open_file(uri).await {
                    warn!("Failed to open OHOS file with system: {error}");
                }
            })
            .detach();
    }

    fn on_quit(&self, callback: Box<dyn FnMut() -> bool>) {
        *self.on_quit.borrow_mut() = Some(callback);
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        *self.on_reopen.borrow_mut() = Some(callback);
    }

    fn on_system_wake(&self, callback: Box<dyn FnMut()>) {
        *self.system_wake.borrow_mut() = Some(callback);
    }

    fn on_system_sleep(&self, callback: Box<dyn FnMut()>) {
        *self.system_sleep.borrow_mut() = Some(callback);
    }

    fn set_menus(&self, menus: Vec<Menu>, keymap: &Keymap) {
        let owned: Vec<OwnedMenu> = menus.into_iter().map(Menu::owned).collect();
        {
            let mut state = self.menus.borrow_mut();
            state.actions.clear();
            let mut items = Vec::with_capacity(owned.len());
            for menu in &owned {
                let id = format!("gpui-menu-{}", state.next_id);
                state.next_id += 1;
                items.push(MenuItemData {
                    id,
                    item_type: "submenu".into(),
                    text: Some(menu.name.to_string()),
                    enabled: Some(!menu.disabled),
                    accelerator: None,
                    accelerator_display_only: Some(true),
                    predefined_type: None,
                    checked: None,
                    icon: None,
                    native_icon: None,
                    submenu_items: Some(menu_items(&menu.items, &mut state, keymap)),
                    about_metadata: None,
                });
            }
            match serde_json::to_string(&items) {
                Ok(json) => {
                    state.json = json;
                    state.menus = Some(owned);
                }
                Err(error) => {
                    warn!("Failed to serialize OHOS app menu: {error}");
                    return;
                }
            }
        }
        self.publish_menu(0);
        for window in self.windows.borrow().iter().filter_map(Weak::upgrade) {
            let id = window.borrow().window_id();
            if id != 0 {
                self.publish_menu(id);
            }
        }
    }

    fn get_menus(&self) -> Option<Vec<OwnedMenu>> {
        self.menus.borrow().menus.clone()
    }

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {
        // Not supported on OHOS
    }

    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
        self.menus.borrow_mut().on_action = Some(callback);
    }

    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.menus.borrow_mut().on_will_open = Some(callback);
    }

    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        self.menus.borrow_mut().on_validate = Some(callback);
    }

    fn compositor_name(&self) -> &'static str {
        "OHOS"
    }

    fn app_path(&self) -> Result<PathBuf> {
        self.bundle_code_dir
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("OHOS bundle code directory is not ready"))
    }

    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        let relative = Path::new(name);
        anyhow::ensure!(
            !name.is_empty()
                && relative
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
            "auxiliary executable name must be a relative bundle path"
        );
        let path = self.app_path()?.join(relative);
        anyhow::ensure!(
            path.is_file(),
            "auxiliary executable is not in the OHOS bundle"
        );
        Ok(path)
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let window_id = self.cursor_window_id.get();
        let request = (window_id, style);
        if self.cursor_style.get() == Some(request) {
            return;
        }
        self.cursor_style.set(Some(request));
        let cursor_style = self.cursor_style.clone();
        self.foreground_executor
            .spawn(async move {
                let result = async {
                    WindowClient::new(&app)?
                        .set_cursor_icon(window_id, ohos_cursor_style(style))
                        .await
                }
                .await;
                if let Err(error) = result {
                    warn!("Failed to set OHOS cursor style: {error}");
                    if cursor_style.get() == Some(request) {
                        cursor_style.set(None);
                    }
                }
            })
            .detach();
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        self.form_factor.should_auto_hide_scrollbars()
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        self.clipboard.read_cached()
    }

    fn read_from_clipboard_async(
        &self,
    ) -> Task<std::result::Result<Option<ClipboardItem>, ClipboardReadError>> {
        self.clipboard.read()
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        let write = self.clipboard.write(item);
        self.foreground_executor
            .spawn(async move {
                if let Err(error) = write.await {
                    warn!("Failed to write OHOS clipboard: {error}");
                }
            })
            .detach();
    }

    fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Task<Result<()>> {
        let alias = credential_alias(url);
        let username = username.to_owned();
        let password = password.to_vec();
        self.background_executor
            .spawn(async move { super::credentials::write(&alias, &username, &password) })
    }

    fn read_credentials(&self, url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        let alias = credential_alias(url);
        self.background_executor
            .spawn(async move { super::credentials::read(&alias) })
    }

    fn delete_credentials(&self, url: &str) -> Task<Result<()>> {
        let alias = credential_alias(url);
        self.background_executor
            .spawn(async move { super::credentials::delete(&alias) })
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(super::keyboard::OhosKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(super::keyboard::OhosKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
        *self.keyboard_layout_change.borrow_mut() = Some(callback);
    }

    fn thermal_state(&self) -> ThermalState {
        self.thermal_state.get()
    }

    fn on_thermal_state_change(&self, callback: Box<dyn FnMut()>) {
        *self.thermal_state_change.borrow_mut() = Some(callback);
    }

    fn prevent_idle_sleep(&self, _reason: &str) -> Task<Result<ActivityGuard>> {
        let Some(app) = self.app.borrow().clone() else {
            return Task::ready(Err(anyhow::anyhow!("OpenHarmonyApp not set")));
        };
        let client = match WindowClient::new(&app) {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error.into())),
        };
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.idle_sleep.lock().unwrap();
            state.guards += 1;
            state.waiters.push(sender);
        }
        reconcile_idle_sleep(
            self.idle_sleep.clone(),
            client,
            self.background_executor.clone(),
        );
        self.background_executor
            .spawn(async move { receiver.await? })
    }

    fn read_from_primary(&self) -> Option<ClipboardItem> {
        None
    }

    fn write_to_primary(&self, _item: ClipboardItem) {}
}

//! Process-wide window registry and window creation.
//!
//! Every Luban window registers itself so the session file can record all open
//! windows, not only the one that happens to trigger a save. The registry also
//! creates additional windows and restores saved bounds.

use std::cell::{Cell, RefCell};

use gpui_kit::component::TitleBar;
use gpui_kit::{
    point, px, size, AnyWindowHandle, App, AppContext as _, Bounds, Context, Pixels,
    TitlebarOptions, WeakEntity, WindowBackgroundAppearance, WindowBounds, WindowDecorations,
    WindowOptions,
};
use luban_core::session::WindowSession;

use crate::app::{LaunchOptions, LubanApp, WindowInit};

struct Entry {
    id: u64,
    order: u64,
    weak: WeakEntity<LubanApp>,
    bounds: Bounds<Pixels>,
    maximized: bool,
}

thread_local! {
    static ENTRIES: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
    static NEXT_ORDER: Cell<u64> = const { Cell::new(1) };
}

/// Allocate a process-local window identifier.
pub fn next_window_id() -> u64 {
    NEXT_ID.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    })
}

pub fn register(id: u64, weak: WeakEntity<LubanApp>, bounds: Bounds<Pixels>, maximized: bool) {
    let order = NEXT_ORDER.with(|next| {
        let order = next.get();
        next.set(order + 1);
        order
    });
    ENTRIES.with(|entries| {
        let mut entries = entries.borrow_mut();
        entries.retain(|entry| entry.id != id);
        entries.push(Entry {
            id,
            order,
            weak,
            bounds,
            maximized,
        });
    });
}

pub fn unregister(id: u64) {
    ENTRIES.with(|entries| {
        entries.borrow_mut().retain(|entry| entry.id != id);
    });
}

pub fn update_bounds(id: u64, bounds: Bounds<Pixels>, maximized: bool) {
    ENTRIES.with(|entries| {
        if let Some(entry) = entries.borrow_mut().iter_mut().find(|entry| entry.id == id) {
            entry.bounds = bounds;
            entry.maximized = maximized;
        }
    });
}

/// Collect every live window's session. `current` is this window's snapshot;
/// other windows are updated through their entity handles.
pub fn collect(cx: &mut Context<LubanApp>, current: WindowSession) -> Vec<WindowSession> {
    type EntrySnapshot = (u64, u64, WeakEntity<LubanApp>, Bounds<Pixels>, bool);
    let entries: Vec<EntrySnapshot> = ENTRIES.with(|entries| {
        entries
            .borrow()
            .iter()
            .map(|entry| {
                (
                    entry.id,
                    entry.order,
                    entry.weak.clone(),
                    entry.bounds,
                    entry.maximized,
                )
            })
            .collect()
    });
    let mut windows: Vec<(u64, WindowSession)> = Vec::new();
    for (id, order, weak, bounds, maximized) in entries {
        let mut session = if id == current.id {
            current.clone()
        } else {
            match weak.update(cx, |app, _| app.build_window_session()) {
                Ok(session) => session,
                Err(_) => continue,
            }
        };
        session.id = id;
        fill_bounds(&mut session, bounds, maximized);
        windows.push((order, session));
    }
    if windows.is_empty() {
        return vec![current];
    }
    windows.sort_by_key(|(order, _)| *order);
    windows.into_iter().map(|(_, session)| session).collect()
}

fn fill_bounds(session: &mut WindowSession, bounds: Bounds<Pixels>, maximized: bool) {
    session.x = Some(bounds.origin.x.as_f32().round() as i32);
    session.y = Some(bounds.origin.y.as_f32().round() as i32);
    session.width = bounds.size.width.as_f32().max(480.0);
    session.height = bounds.size.height.as_f32().max(320.0);
    session.maximized = maximized;
}

/// Window options shared by the initial window and windows opened later.
pub fn window_options(
    bounds: WindowBounds,
    manual_chrome: bool,
    icon: Option<std::sync::Arc<image::RgbaImage>>,
) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(bounds),
        titlebar: Some(TitlebarOptions {
            title: Some("Luban Notepad".into()),
            ..TitleBar::title_bar_options()
        }),
        window_decorations: Some(if manual_chrome {
            WindowDecorations::Server
        } else {
            WindowDecorations::Client
        }),
        window_background: if cfg!(target_os = "linux") && !manual_chrome {
            WindowBackgroundAppearance::Transparent
        } else {
            WindowBackgroundAppearance::Opaque
        },
        app_id: Some("luban-notepad".into()),
        icon,
        ..TitleBar::window_options()
    }
}

/// Default bounds for a new window.
pub fn centered_bounds(cx: &App) -> Bounds<Pixels> {
    Bounds::centered(None, size(px(1280.0), px(820.0)), cx)
}

/// Restore saved bounds, centering anything that would land off-screen and
/// clamping the size to at least 480x320.
pub fn restored_bounds(session: &WindowSession, cx: &App) -> WindowBounds {
    let width = session.width.clamp(480.0, 10000.0);
    let height = session.height.clamp(320.0, 10000.0);
    let centered =
        || WindowBounds::Windowed(Bounds::centered(None, size(px(width), px(height)), cx));
    let (Some(x), Some(y)) = (session.x, session.y) else {
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        return if session.maximized {
            WindowBounds::Maximized(bounds)
        } else {
            WindowBounds::Windowed(bounds)
        };
    };
    let bounds = Bounds::new(
        point(px(x as f32), px(y as f32)),
        size(px(width), px(height)),
    );
    if visible_enough(&bounds, cx) {
        if session.maximized {
            WindowBounds::Maximized(bounds)
        } else {
            WindowBounds::Windowed(bounds)
        }
    } else {
        centered()
    }
}

fn visible_enough(bounds: &Bounds<Pixels>, cx: &App) -> bool {
    let Some(display) = cx.primary_display() else {
        return true;
    };
    let visible = display.visible_bounds();
    let overlap_w = (bounds.right().as_f32().min(visible.right().as_f32())
        - bounds.left().as_f32().max(visible.left().as_f32()))
    .max(0.0);
    let overlap_h = (bounds.bottom().as_f32().min(visible.bottom().as_f32())
        - bounds.top().as_f32().max(visible.top().as_f32()))
    .max(0.0);
    overlap_w * overlap_h >= 80.0 * 80.0
}

/// Open a new window with the given initial content.
pub fn open_window(
    init: WindowInit,
    manual_chrome: bool,
    cx: &mut App,
) -> anyhow::Result<AnyWindowHandle> {
    let bounds = centered_bounds(cx);
    open_window_at(init, WindowBounds::Windowed(bounds), manual_chrome, cx)
}

/// Open a window at explicit bounds (used by session restore).
pub fn open_window_at(
    init: WindowInit,
    bounds: WindowBounds,
    manual_chrome: bool,
    cx: &mut App,
) -> anyhow::Result<AnyWindowHandle> {
    let id = next_window_id();
    open_window_with(
        LaunchOptions {
            files: Vec::new(),
            smoke_test: false,
            manual_window_chrome: manual_chrome,
            recoveries: Vec::new(),
            init,
            auto_recovery: false,
            window_id: id,
        },
        bounds,
        manual_chrome,
        cx,
    )
}

/// Open a window holding one moved tab. A dirty disk-engine tab brings its
/// crash-recovery entry so the new window replays the unsaved edits.
pub fn open_window_for_tab(
    tab: luban_core::session::SessionTab,
    recovery: Option<luban_core::disk_engine::RecoveryInfo>,
    manual_chrome: bool,
    cx: &mut App,
) -> anyhow::Result<AnyWindowHandle> {
    let bounds = centered_bounds(cx);
    let id = next_window_id();
    let auto_recovery = recovery.is_some();
    let recoveries = recovery.into_iter().collect();
    open_window_with(
        LaunchOptions {
            files: Vec::new(),
            smoke_test: false,
            manual_window_chrome: manual_chrome,
            recoveries,
            init: WindowInit::Tabs(vec![tab]),
            auto_recovery,
            window_id: id,
        },
        WindowBounds::Windowed(bounds),
        manual_chrome,
        cx,
    )
}

/// Open a window from a fully prepared launch description.
pub fn open_window_with(
    options: LaunchOptions,
    bounds: WindowBounds,
    manual_chrome: bool,
    cx: &mut App,
) -> anyhow::Result<AnyWindowHandle> {
    let id = options.window_id;
    let initial_bounds = match &bounds {
        WindowBounds::Windowed(bounds)
        | WindowBounds::Maximized(bounds)
        | WindowBounds::Fullscreen(bounds) => *bounds,
    };
    let maximized = matches!(bounds, WindowBounds::Maximized(_));
    let window_options = window_options(bounds, manual_chrome, crate::window_icon());
    let (handle, entity) = gpui_kit::open_window(window_options, cx, move |window, cx| {
        cx.new(|cx| LubanApp::new(window, cx, options))
    })?;
    register(id, entity.downgrade(), initial_bounds, maximized);
    #[cfg(target_os = "linux")]
    if manual_chrome {
        crate::remove_window_decorations(handle, cx);
    }
    Ok(handle)
}

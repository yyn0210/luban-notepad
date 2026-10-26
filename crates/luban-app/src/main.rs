#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Luban Notepad — a cross-platform, GPUI-based reimplementation of the
//! Notepad++ editing experience.

rust_i18n::i18n!("locales", fallback = "en");

mod app;
mod dialogs;
mod explorer;
mod find;
mod huge_file;
mod i18n;
mod icons;
mod large_file;
mod preferences;
mod theme;
mod util;

use std::path::PathBuf;
use std::sync::Arc;

use app::LaunchOptions;
use gpui_kit::component::TitleBar;
use gpui_kit::*;
use luban_core::settings::Settings;

use crate::i18n::tr_args;

fn parse_args() -> LaunchOptions {
    let mut files = Vec::new();
    let mut new_window = false;
    let mut smoke_test = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "{}",
                    tr_args(
                        "cli.help",
                        &[("version", env!("CARGO_PKG_VERSION").to_string())]
                    )
                );
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("luban-notepad {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--new" | "-n" => new_window = true,
            "--smoke-test" => smoke_test = true,
            other if !other.starts_with('-') => files.push(PathBuf::from(other)),
            other => log::warn!("ignoring unknown argument {other}"),
        }
    }
    LaunchOptions {
        files,
        new_window,
        smoke_test,
        manual_window_chrome: manual_window_chrome(),
    }
}

/// Whether the application removes the window-manager titlebar itself.
///
/// On X11, gpui-component's root view reserves a 20px transparent margin for a
/// client-side shadow. Only a compositor can blend that margin; without one it
/// is painted black. The window is therefore created with server decorations
/// (no shadow margin) and its titlebar is removed through `_MOTIF_WM_HINTS`
/// after creation. Wayland always composites, so it keeps client decorations.
fn manual_window_chrome() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("WAYLAND_DISPLAY").is_none_or(|display| display.is_empty())
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Remove the window-manager titlebar and border from the undecorated X11
/// window, leaving the client area to the application-drawn titlebar. Returns
/// whether the hint had to be written.
#[cfg(target_os = "linux")]
fn apply_borderless_hint(window: &Window) -> anyhow::Result<bool> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};

    let xid = match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Xlib(handle)) => handle.window as u32,
        Ok(RawWindowHandle::Xcb(handle)) => handle.window.get(),
        Ok(other) => anyhow::bail!("cannot remove window decorations from {other:?}"),
        Err(error) => anyhow::bail!("cannot read the X11 window handle: {error}"),
    };

    let (connection, _) = x11rb::connect(None)?;
    let motif = connection
        .intern_atom(false, b"_MOTIF_WM_HINTS")?
        .reply()?
        .atom;
    let current = connection
        .get_property(false, xid, motif, AtomEnum::ANY, 0, 5)?
        .reply()?;
    let desired = [2u32, 0, 0, 0, 0];
    if current
        .value32()
        .is_some_and(|values| values.take(5).collect::<Vec<_>>() == desired)
    {
        return Ok(false);
    }

    // [flags, functions, decorations, input_mode, status]: remove the window
    // manager decorations while leaving everything else alone.
    let data: Vec<u8> = desired
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect();
    connection.change_property(PropMode::REPLACE, xid, motif, motif, 32, 5, &data)?;
    connection.flush()?;
    Ok(true)
}

/// Remove the window-manager titlebar and keep it removed while the window
/// manager finishes managing the freshly mapped window.
#[cfg(target_os = "linux")]
fn remove_window_decorations(handle: AnyWindowHandle, cx: &mut App) {
    let _ = handle.update(cx, |_, window, _| match apply_borderless_hint(window) {
        Ok(true) => log::info!("removed the window manager titlebar"),
        Ok(false) => {}
        Err(error) => log::warn!("failed to remove the window manager titlebar: {error:#}"),
    });

    cx.spawn(async move |cx| {
        for delay in [120u64, 350, 800] {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(delay))
                .await;
            let _ = handle.update(cx, |_, window, _| {
                let _ = apply_borderless_hint(window);
            });
        }
    })
    .detach();
}

/// Decode the icon embedded in the binary so the taskbar entry is correct even
/// when the desktop entry and icon theme are not installed (X11 only).
fn window_icon() -> Option<Arc<image::RgbaImage>> {
    const ICON_PNG: &[u8] =
        include_bytes!("../../../packaging/linux/icons/hicolor/256x256/apps/luban-notepad.png");
    match image::load_from_memory(ICON_PNG) {
        Ok(icon) => Some(Arc::new(icon.to_rgba8())),
        Err(error) => {
            log::warn!("failed to decode the embedded window icon: {error}");
            None
        }
    }
}

fn main() {
    // Remove journals left behind by terminated instances. Only directories
    // older than a day are touched so another running instance is unaffected.
    let _ = luban_core::disk_engine::cleanup_stale(std::time::Duration::from_secs(24 * 3600));
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("luban_notepad=info,gpui=warn"),
    )
    .init();

    // Resolve the UI language before parsing arguments so that `--help` is
    // localized too. `LubanApp::new` loads the same settings again for its own
    // state; keeping the two loads independent avoids threading the struct
    // through the launch options.
    let settings = Settings::load();
    i18n::apply_language(&settings.ui_language);

    let options = parse_args();

    gpui_kit::application()
        .with_assets(icons::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
            let manual_chrome = options.manual_window_chrome;
            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Luban Notepad".into()),
                    ..TitleBar::title_bar_options()
                }),
                // X11 asks for server decorations and removes the titlebar
                // itself, because client decorations add a transparent shadow
                // margin that cannot blend without a compositor. Every other
                // platform requests client decorations and lets the platform
                // report whether it can honour them; the render path falls back
                // to a standalone menu row when it cannot.
                window_decorations: Some(if manual_chrome {
                    WindowDecorations::Server
                } else {
                    WindowDecorations::Client
                }),
                // A manually decorated window has no shadow margin and needs no
                // compositor to blend its surface. Client decorations on Linux
                // do use a transparent margin, so that path stays transparent.
                window_background: if cfg!(target_os = "linux") && !manual_chrome {
                    WindowBackgroundAppearance::Transparent
                } else {
                    WindowBackgroundAppearance::Opaque
                },
                // Keep the window grouped with its desktop entry so docks and
                // taskbars pick up the installed icon on Wayland as well.
                app_id: Some("luban-notepad".into()),
                icon: window_icon(),
                ..TitleBar::window_options()
            };
            let result = gpui_kit::open_window(window_options, cx, |window, cx| {
                log::info!("window decorations: {:?}", window.window_decorations());
                cx.new(|cx| app::LubanApp::new(window, cx, options))
            });
            let handle = match result {
                Ok((handle, _)) => handle,
                Err(error) => {
                    log::error!("failed to open Luban Notepad window: {error:#}");
                    cx.quit();
                    return;
                }
            };
            #[cfg(target_os = "linux")]
            if manual_chrome {
                remove_window_decorations(handle, cx);
            }
            #[cfg(not(target_os = "linux"))]
            let _ = handle;
            cx.activate(true);
        });
}

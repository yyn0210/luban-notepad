#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Luban Notepad — a cross-platform, GPUI-based reimplementation of the
//! Notepad++ editing experience.

rust_i18n::i18n!("locales", fallback = "en");

mod app;
mod dialogs;
mod explorer;
mod find;
mod i18n;
mod icons;
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
    }
}

/// Client-side decorations reserve a transparent margin around the window for
/// the shadow. Only a compositor can blend that margin; without one it is
/// painted black, so fall back to server-side decorations in that case.
fn use_client_decorations() -> bool {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|display| !display.is_empty()) {
            // Wayland always composites.
            return true;
        }
        x11_compositor_present().unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Whether a compositing manager owns the `_NET_WM_CM_Sn` selection. GPUI's
/// own check also accepts `_NET_SUPPORTING_WM_CHECK`, which stays present when
/// the window manager runs without compositing.
#[cfg(target_os = "linux")]
fn x11_compositor_present() -> Option<bool> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

    let (connection, screen_index) = x11rb::connect(None).ok()?;
    let root = connection.setup().roots.get(screen_index)?.root;

    let selection = connection
        .intern_atom(false, format!("_NET_WM_CM_S{screen_index}").as_bytes())
        .ok()?
        .reply()
        .ok()?
        .atom;
    let owner = connection
        .get_selection_owner(selection)
        .ok()?
        .reply()
        .ok()?
        .owner;
    if owner != x11rb::NONE {
        return Some(true);
    }

    // Some compositors only publish the owner property.
    let property = connection
        .intern_atom(false, b"_NET_WM_CM_OWNER")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let reply = connection
        .get_property(false, root, property, AtomEnum::WINDOW, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    Some(reply.value_len > 0)
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
            let client_decorations = use_client_decorations();
            log::info!(
                "using {} window decorations",
                if client_decorations {
                    "client-side"
                } else {
                    "server-side (no compositor)"
                }
            );
            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Luban Notepad".into()),
                    ..TitleBar::title_bar_options()
                }),
                // The application draws its own title bar on every desktop.
                // Without a compositor the shadow margin cannot blend, so ask
                // the window manager for server-side decorations instead.
                window_decorations: Some(if client_decorations {
                    WindowDecorations::Client
                } else {
                    WindowDecorations::Server
                }),
                // Client-side decorations composite their shadow over the
                // transparent margin; an opaque window would paint it black.
                window_background: if cfg!(target_os = "linux") && client_decorations {
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
                cx.new(|cx| app::LubanApp::new(window, cx, options))
            });
            if let Err(error) = result {
                log::error!("failed to open Luban Notepad window: {error:#}");
                cx.quit();
                return;
            }
            cx.activate(true);
        });
}

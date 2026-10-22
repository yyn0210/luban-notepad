#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Luban Notepad — a cross-platform, GPUI-based reimplementation of the
//! Notepad++ editing experience.

rust_i18n::i18n!("locales", fallback = "en");

mod app;
mod dialogs;
mod explorer;
mod find;
mod i18n;
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
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Luban Notepad".into()),
                    ..TitleBar::title_bar_options()
                }),
                // The application draws its own title bar on every desktop;
                // Linux compositors must hand window decorations to us.
                window_decorations: Some(WindowDecorations::Client),
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

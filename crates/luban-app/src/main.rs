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

use app::LaunchOptions;
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
                    ..Default::default()
                }),
                ..Default::default()
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

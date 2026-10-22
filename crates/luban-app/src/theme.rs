use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::{App, Window};
use luban_core::settings::{Settings, ThemeChoice};

pub fn apply_theme(settings: &Settings, window: Option<&mut Window>, cx: &mut App) {
    let mode = match settings.theme {
        ThemeChoice::Dark => ThemeMode::Dark,
        ThemeChoice::Light => ThemeMode::Light,
    };
    Theme::change(mode, window, cx);
}

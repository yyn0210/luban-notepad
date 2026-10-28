use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::{App, Window};
use luban_core::settings::{Settings, ThemeChoice};

pub fn apply_theme(settings: &Settings, window: Option<&mut Window>, cx: &mut App) {
    let mode = match settings.theme {
        ThemeChoice::Dark => ThemeMode::Dark,
        ThemeChoice::Light => ThemeMode::Light,
        ThemeChoice::System => {
            Theme::sync_system_appearance(window, cx);
            return;
        }
    };
    Theme::change(mode, window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_choice_is_addressable() {
        for theme in [ThemeChoice::Light, ThemeChoice::Dark, ThemeChoice::System] {
            let settings = Settings {
                theme,
                ..Settings::default()
            };
            assert_eq!(settings.theme, theme);
        }
    }
}

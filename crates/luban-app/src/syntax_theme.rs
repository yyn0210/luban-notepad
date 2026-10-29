//! Syntax colour themes and their mapping onto gpui-component's semantic
//! highlight roles.
//!
//! The editor highlighter names spans by semantic role (`keyword`, `string`,
//! `comment`, ...), so a TextMate theme only needs its scope colours mapped
//! onto those roles. Built-in themes come from `two-face`; imported
//! `.tmTheme` files are parsed with syntect. Export uses the same syntect
//! theme, so exported HTML/RTF matches what the editor shows.

use std::path::Path;
use std::sync::Arc;

use gpui_kit::component::highlighter::{HighlightTheme, ThemeStyle};
use gpui_kit::component::theme::Theme;
use gpui_kit::App;
use luban_core::settings::Settings;
use serde_json::json;
use syntect::highlighting::{Color, FontStyle, Theme as SyntectTheme};
use two_face::theme::{extra, EmbeddedThemeName};

pub const CUSTOM_ID: &str = "custom";

#[derive(Clone, Copy, Debug)]
pub struct SyntaxThemeOption {
    pub id: &'static str,
    pub label: &'static str,
    pub embedded: EmbeddedThemeName,
}

pub const THEME_OPTIONS: &[SyntaxThemeOption] = &[
    SyntaxThemeOption {
        id: "InspiredGitHub",
        label: "InspiredGitHub",
        embedded: EmbeddedThemeName::InspiredGithub,
    },
    SyntaxThemeOption {
        id: "Github",
        label: "GitHub",
        embedded: EmbeddedThemeName::Github,
    },
    SyntaxThemeOption {
        id: "SolarizedLight",
        label: "Solarized Light",
        embedded: EmbeddedThemeName::SolarizedLight,
    },
    SyntaxThemeOption {
        id: "SolarizedDark",
        label: "Solarized Dark",
        embedded: EmbeddedThemeName::SolarizedDark,
    },
    SyntaxThemeOption {
        id: "Monokai",
        label: "Monokai",
        embedded: EmbeddedThemeName::MonokaiExtended,
    },
    SyntaxThemeOption {
        id: "Nord",
        label: "Nord",
        embedded: EmbeddedThemeName::Nord,
    },
    SyntaxThemeOption {
        id: "Dracula",
        label: "Dracula",
        embedded: EmbeddedThemeName::Dracula,
    },
    SyntaxThemeOption {
        id: "OneHalfDark",
        label: "One Half Dark",
        embedded: EmbeddedThemeName::OneHalfDark,
    },
    SyntaxThemeOption {
        id: "OneHalfLight",
        label: "One Half Light",
        embedded: EmbeddedThemeName::OneHalfLight,
    },
    SyntaxThemeOption {
        id: "GruvboxDark",
        label: "Gruvbox Dark",
        embedded: EmbeddedThemeName::GruvboxDark,
    },
    SyntaxThemeOption {
        id: "GruvboxLight",
        label: "Gruvbox Light",
        embedded: EmbeddedThemeName::GruvboxLight,
    },
];

/// Semantic role -> TextMate scope used to pick its colour.
const ROLES: &[(&str, &str)] = &[
    ("attribute", "entity.other.attribute-name"),
    ("boolean", "constant.language.boolean"),
    ("comment", "comment"),
    ("comment.doc", "comment.block.documentation"),
    ("constant", "constant"),
    ("constructor", "entity.name.function.constructor"),
    ("embedded", "meta.embedded"),
    ("enum", "entity.name.type.enum"),
    ("function", "entity.name.function"),
    ("keyword", "keyword"),
    ("label", "entity.name.label"),
    ("number", "constant.numeric"),
    ("operator", "keyword.operator"),
    ("preproc", "keyword.control.import"),
    ("property", "variable.other.property"),
    ("punctuation", "punctuation"),
    ("punctuation.bracket", "punctuation.definition"),
    ("string", "string"),
    ("string.escape", "constant.character.escape"),
    ("string.regex", "string.regexp"),
    ("tag", "entity.name.tag"),
    ("title", "entity.name.section"),
    ("type", "entity.name.type"),
    ("variable", "variable"),
    ("variant", "variable.other.enummember"),
];

fn color_hex(color: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

fn role_style(theme: &SyntectTheme, scope_text: &str) -> Option<(Color, FontStyle)> {
    let scope = syntect::parsing::Scope::new(scope_text).ok()?;
    let mut best: Option<(syntect::parsing::MatchPower, Color, FontStyle)> = None;
    for item in &theme.scopes {
        if let Some(power) = item.scope.does_match(&[scope]) {
            let color = item
                .style
                .foreground
                .unwrap_or_else(|| theme.settings.foreground.unwrap_or(Color::BLACK));
            let font_style = item.style.font_style.unwrap_or(FontStyle::empty());
            if best
                .as_ref()
                .is_none_or(|(best_power, _, _)| power > *best_power)
            {
                best = Some((power, color, font_style));
            }
        }
    }
    best.map(|(_, color, font_style)| (color, font_style))
}

fn theme_style(color: Color, font_style: FontStyle) -> Option<ThemeStyle> {
    let mut value = json!({ "color": color_hex(color) });
    if font_style.contains(FontStyle::ITALIC) {
        value["font_style"] = json!("italic");
    }
    if font_style.contains(FontStyle::BOLD) {
        value["font_weight"] = json!(700);
    }
    serde_json::from_value(value).ok()
}

/// Convert a syntect theme into the editor's semantic highlight theme.
pub fn highlight_theme(name: &str, theme: &SyntectTheme) -> HighlightTheme {
    let background = theme.settings.background.unwrap_or(Color::WHITE);
    let foreground = theme.settings.foreground.unwrap_or(Color::BLACK);
    let mut syntax = serde_json::Map::new();
    for (role, scope) in ROLES {
        if let Some((color, font_style)) = role_style(theme, scope) {
            if let Some(style) = theme_style(color, font_style) {
                if let Ok(value) = serde_json::to_value(style) {
                    syntax.insert((*role).to_string(), value);
                }
            }
        }
    }
    let background_color = theme.settings.background.unwrap_or(Color::WHITE);
    let luminance = u32::from(background_color.r)
        + u32::from(background_color.g)
        + u32::from(background_color.b);
    let appearance = if luminance < 384 { "dark" } else { "light" };
    let value = json!({
        "name": name,
        "appearance": appearance,
        "style": {
            "editor.background": color_hex(background),
            "editor.foreground": color_hex(foreground),
            "editor.line_number": color_hex(theme.settings.gutter.unwrap_or(foreground)),
            "editor.active_line_number": color_hex(theme.settings.gutter_foreground.unwrap_or(foreground)),
            "editor.active_line.background": color_hex(theme.settings.line_highlight.unwrap_or(background)),
            "syntax": syntax,
        }
    });
    serde_json::from_value(value).unwrap_or_else(|_| (*HighlightTheme::default_light()).clone())
}

/// Resolve the configured syntax theme to a syntect theme.
///
/// `None` means the built-in editor theme should stay in place.
pub fn resolve_theme(settings: &Settings) -> Result<Option<(String, SyntectTheme)>, String> {
    let id = settings.syntax_theme.trim();
    if id.is_empty() || id.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    if id == CUSTOM_ID {
        let Some(path) = settings.syntax_theme_path.as_deref() else {
            return Err("custom syntax theme path is missing".to_string());
        };
        let theme = syntect::highlighting::ThemeSet::get_theme(path)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Custom".to_string());
        return Ok(Some((name, theme)));
    }
    let Some(option) = THEME_OPTIONS.iter().find(|option| option.id == id) else {
        return Err(format!("unknown syntax theme '{id}'"));
    };
    let themes = extra();
    let theme = themes.get(option.embedded).clone();
    Ok(Some((option.label.to_string(), theme)))
}

/// Apply the configured syntax theme to the global editor theme.
pub fn apply(settings: &Settings, cx: &mut App) -> Result<(), String> {
    match resolve_theme(settings)? {
        Some((name, theme)) => {
            let highlight = Arc::new(highlight_theme(&name, &theme));
            Theme::update(cx, |theme| theme.highlight_theme = highlight);
        }
        None => {
            let highlight = if Theme::global(cx).mode.is_dark() {
                HighlightTheme::default_dark()
            } else {
                HighlightTheme::default_light()
            };
            Theme::update(cx, |theme| theme.highlight_theme = highlight);
        }
    }
    Ok(())
}

/// Parse an imported `.tmTheme` file before storing its path.
pub fn load_custom(path: &Path) -> Result<SyntectTheme, String> {
    syntect::highlighting::ThemeSet::get_theme(path)
        .map_err(|err| format!("{}: {err}", path.display()))
}

/// The theme used by HTML/RTF export.
pub fn export_theme(settings: &Settings) -> (String, SyntectTheme) {
    match resolve_theme(settings) {
        Ok(Some(theme)) => theme,
        Ok(None) | Err(_) => {
            let themes = extra();
            (
                "InspiredGitHub".to_string(),
                themes.get(EmbeddedThemeName::InspiredGithub).clone(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_themes_map_to_semantic_roles() {
        let themes = extra();
        let theme = themes.get(EmbeddedThemeName::MonokaiExtended);
        let highlight = highlight_theme("Monokai", theme);
        assert!(highlight.style.syntax.keyword.is_some());
        assert!(highlight.style.syntax.string.is_some());
        assert!(highlight.style.syntax.comment.is_some());
    }

    #[test]
    fn unknown_theme_is_an_error_and_default_is_none() {
        let settings = Settings {
            syntax_theme: "default".to_string(),
            ..Settings::default()
        };
        assert!(resolve_theme(&settings).unwrap().is_none());
        let settings = Settings {
            syntax_theme: "does-not-exist".to_string(),
            ..Settings::default()
        };
        assert!(resolve_theme(&settings).is_err());
    }

    #[test]
    fn every_option_has_a_unique_id() {
        let mut ids: Vec<&str> = THEME_OPTIONS.iter().map(|option| option.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }
}

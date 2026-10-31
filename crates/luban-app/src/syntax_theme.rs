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
    // The highlighter names unstyled source/text spans `primary`; giving that
    // role the theme foreground makes plain identifiers follow the theme
    // instead of the application palette.
    if let Some(style) = theme_style(foreground, FontStyle::empty()) {
        if let Ok(value) = serde_json::to_value(style) {
            syntax.insert("primary".to_string(), value);
        }
    }
    let background_color = theme.settings.background.unwrap_or(Color::WHITE);
    // TextMate's `gutter` is the gutter *background*; `gutterForeground` is
    // the line-number text. The syntect build reads the background through
    // `editor_gutter_background`; the text colour keys are kept for the
    // tree-sitter build and for themes consumed elsewhere.
    let gutter_background = theme.settings.gutter.unwrap_or(background);
    let gutter_foreground = theme.settings.gutter_foreground.unwrap_or(foreground);
    let luminance = u32::from(background_color.r)
        + u32::from(background_color.g)
        + u32::from(background_color.b);
    let appearance = if luminance < 384 { "dark" } else { "light" };
    let value = json!({
        "name": name,
        "appearance": appearance,
        "style": {
            // The tree-sitter build reads dotted Zed keys; the syntect build
            // (what this application uses) reads the snake_case aliases.
            "editor.background": color_hex(background),
            "editor_background": color_hex(background),
            "editor.foreground": color_hex(foreground),
            "editor_foreground": color_hex(foreground),
            "editor.gutter.background": color_hex(gutter_background),
            "editor_gutter_background": color_hex(gutter_background),
            "editor.line_number": color_hex(gutter_foreground),
            "editor_line_number": color_hex(gutter_foreground),
            "editor.active_line_number": color_hex(gutter_foreground),
            "editor_active_line_number": color_hex(gutter_foreground),
            "editor.active_line.background": color_hex(theme.settings.line_highlight.unwrap_or(background)),
            "editor_active_line": color_hex(theme.settings.line_highlight.unwrap_or(background)),
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
    if let Some(rest) = id.strip_prefix("pkg:") {
        let root = luban_core::settings::packages_dir().join("themes");
        let root = root.canonicalize().unwrap_or(root);
        let candidates: Vec<&str> = match rest.rsplit_once(':') {
            Some((path, _name)) if path != rest => vec![path, rest],
            _ => vec![rest],
        };
        for candidate in candidates {
            let relative = Path::new(candidate);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
            {
                continue;
            }
            let path = root.join(relative);
            let Ok(path) = path.canonicalize() else {
                continue;
            };
            if !path.starts_with(&root) || !path.is_file() {
                continue;
            }
            let theme = syntect::highlighting::ThemeSet::get_theme(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            let name = theme
                .name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| {
                    path.file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Theme".to_string())
                });
            return Ok(Some((name, theme)));
        }
        return Err(format!("unknown package theme '{id}'"));
    }
    let Some(option) = THEME_OPTIONS.iter().find(|option| option.id == id) else {
        return Err(format!("unknown syntax theme '{id}'"));
    };
    let themes = extra();
    let theme = themes.get(option.embedded).clone();
    Ok(Some((option.label.to_string(), theme)))
}

/// The theme used when a saved theme cannot be resolved.
pub fn fallback_highlight_theme() -> HighlightTheme {
    let themes = extra();
    highlight_theme(
        "InspiredGitHub",
        themes.get(EmbeddedThemeName::InspiredGithub),
    )
}

/// Apply the configured syntax theme to the global editor theme.
///
/// Returns the load error without touching the active theme, so interactive
/// callers can keep the previous theme when a new choice fails.
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

/// Apply the saved theme at startup, resetting a broken theme to
/// InspiredGitHub so the fallback does not depend on the previous theme.
pub fn apply_with_fallback(settings: &Settings, cx: &mut App) -> Result<(), String> {
    match apply(settings, cx) {
        Ok(()) => Ok(()),
        Err(error) => {
            let highlight = Arc::new(fallback_highlight_theme());
            Theme::update(cx, |theme| theme.highlight_theme = highlight);
            Err(error)
        }
    }
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
    fn fallback_theme_is_inspired_github() {
        let fallback = fallback_highlight_theme();
        assert_eq!(fallback.name, "InspiredGitHub");
        assert!(fallback.style.syntax.keyword.is_some());
        assert!(fallback.style.syntax.primary.is_some());
        assert!(fallback.style.editor_background.is_some());
        assert!(fallback.style.editor_foreground.is_some());
        assert!(fallback.style.editor_line_number.is_some());
        assert!(fallback.style.editor_gutter_background.is_some());
    }

    #[test]
    fn gutter_colors_map_to_background_and_line_numbers() {
        use syntect::highlighting::ThemeSettings;

        let theme = SyntectTheme {
            name: Some("Gutter".to_string()),
            author: None,
            settings: ThemeSettings {
                background: Some(Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                }),
                foreground: Some(Color {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                }),
                gutter: Some(Color {
                    r: 240,
                    g: 240,
                    b: 240,
                    a: 255,
                }),
                gutter_foreground: Some(Color {
                    r: 128,
                    g: 0,
                    b: 0,
                    a: 255,
                }),
                ..Default::default()
            },
            scopes: Vec::new(),
        };
        let highlight = highlight_theme("Gutter", &theme);
        let expected_gutter: gpui_kit::Hsla = serde_json::from_value(json!("#f0f0f0")).unwrap();
        let expected_number: gpui_kit::Hsla = serde_json::from_value(json!("#800000")).unwrap();
        assert_eq!(
            highlight.style.editor_gutter_background,
            Some(expected_gutter)
        );
        assert_eq!(highlight.style.editor_line_number, Some(expected_number));
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

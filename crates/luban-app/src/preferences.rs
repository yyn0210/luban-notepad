use gpui_kit::component::input::InputState;
use gpui_kit::{AppContext as _, Context, Entity, Window};

use crate::app::LubanApp;
use crate::i18n::tr;

pub struct PreferencesState {
    pub open: bool,
    /// Settings search query; filters the visible groups.
    pub search: Entity<InputState>,
    /// System font families, loaded the first time the font picker opens.
    pub fonts: Option<Vec<String>>,
    pub font_picker: Option<FontPickerState>,
    /// Language selected in the per-language section.
    pub language: String,
    pub language_tab_size: Entity<InputState>,
}

impl PreferencesState {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>) -> Self {
        let language_tab_size = cx.new(|cx| InputState::new(window, cx).default_value("4"));
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("prefs.search_placeholder")));
        Self {
            open: false,
            search,
            fonts: None,
            font_picker: None,
            language: "Python".to_string(),
            language_tab_size,
        }
    }
}

pub struct FontPickerState {
    pub filter: Entity<InputState>,
    pub fonts: Vec<String>,
}

impl FontPickerState {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>, fonts: Vec<String>) -> Self {
        let filter = cx
            .new(|cx| InputState::new(window, cx).placeholder(tr("prefs.font_search_placeholder")));
        Self { filter, fonts }
    }
}

/// Case-insensitive filter over the installed font families. Every
/// whitespace-separated term must appear in the name, so "dejavu mono"
/// matches "DejaVu Sans Mono".
pub(crate) fn matching_fonts<'a>(fonts: &'a [String], filter: &str) -> Vec<&'a str> {
    let terms: Vec<String> = filter
        .split_whitespace()
        .map(|term| term.to_lowercase())
        .collect();
    fonts
        .iter()
        .map(String::as_str)
        .filter(|name| {
            let name = name.to_lowercase();
            terms.iter().all(|term| name.contains(term))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::matching_fonts;

    fn fonts() -> Vec<String> {
        vec![
            "DejaVu Sans Mono".to_string(),
            "Fira Code".to_string(),
            "JetBrains Mono".to_string(),
        ]
    }

    #[test]
    fn empty_filter_lists_every_font() {
        assert_eq!(matching_fonts(&fonts(), "").len(), 3);
        assert_eq!(matching_fonts(&fonts(), "   ").len(), 3);
    }

    #[test]
    fn filter_matches_case_insensitively() {
        assert_eq!(
            matching_fonts(&fonts(), "mono"),
            ["DejaVu Sans Mono", "JetBrains Mono"]
        );
        assert_eq!(matching_fonts(&fonts(), "JETBRAINS"), ["JetBrains Mono"]);
        assert!(matching_fonts(&fonts(), "comic sans").is_empty());
    }

    #[test]
    fn every_filter_term_must_match() {
        assert_eq!(
            matching_fonts(&fonts(), "dejavu mono"),
            ["DejaVu Sans Mono"]
        );
        assert_eq!(matching_fonts(&fonts(), "  code  "), ["Fira Code"]);
        assert!(matching_fonts(&fonts(), "dejavu comic").is_empty());
    }
}

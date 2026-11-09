//! User-visible plugin text that can carry several translations.
//!
//! [`LocalizedText`] is the host-side representation: a plain base string or a
//! locale-keyed map. Manifests feed it from the additive `localizations` table
//! (base strings stay as strings), and panel entries from `title_i18n` /
//! `label_i18n`.
//!
//! The host resolves the value against the active UI language. A missing exact
//! tag falls back to the same primary language (`zh` for `zh-CN` and vice
//! versa), then to English, then to the first non-empty entry in tag order.
//! Plain strings are language-neutral and always win.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Upper bound on the translations carried by one field.
pub const MAX_LOCALES: usize = 16;
/// Upper bound on a locale tag (`zh-CN`, `pt_BR`, ...).
pub const MAX_LOCALE_TAG_BYTES: usize = 24;

/// One user-visible string, either language-neutral or translated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LocalizedText {
    /// A single string used for every UI language.
    Plain(String),
    /// Translations keyed by locale tag.
    Localized(BTreeMap<String, String>),
}

impl LocalizedText {
    /// Build a language-neutral string.
    pub fn new(text: impl Into<String>) -> Self {
        Self::Plain(text.into())
    }

    /// Combine a base string (the pre-i18n manifest/panel value, treated as
    /// English) with per-locale translations.
    pub fn with_translations(
        base: impl Into<String>,
        translations: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let base = base.into();
        let mut values = BTreeMap::new();
        if !base.is_empty() {
            values.insert("en".to_string(), base.clone());
        }
        for (tag, text) in translations {
            if !text.is_empty() {
                values.insert(tag, text);
            }
        }
        if values.is_empty() || (values.len() == 1 && values.contains_key("en")) {
            Self::Plain(base)
        } else {
            Self::Localized(values)
        }
    }

    /// Resolve the text for `locale` using the documented fallback chain.
    pub fn resolve(&self, locale: &str) -> &str {
        match self {
            Self::Plain(text) => text,
            Self::Localized(values) => resolve_map(values, locale),
        }
    }

    /// True when no stored translation has visible text.
    pub fn is_blank(&self) -> bool {
        match self {
            Self::Plain(text) => text.trim().is_empty(),
            Self::Localized(values) => values.values().all(|text| text.trim().is_empty()),
        }
    }

    /// Every stored translation (and the plain text once, with an empty tag).
    pub fn values(&self) -> impl Iterator<Item = (&str, &str)> {
        let mut plain = None;
        let mut localized = None;
        match self {
            Self::Plain(text) => plain = Some(("", text.as_str())),
            Self::Localized(values) => {
                localized = Some(
                    values
                        .iter()
                        .map(|(tag, text)| (tag.as_str(), text.as_str())),
                )
            }
        }
        plain.into_iter().chain(localized.into_iter().flatten())
    }
}

impl From<&str> for LocalizedText {
    fn from(text: &str) -> Self {
        Self::Plain(text.to_string())
    }
}

impl From<String> for LocalizedText {
    fn from(text: String) -> Self {
        Self::Plain(text)
    }
}

fn resolve_map<'a>(values: &'a BTreeMap<String, String>, locale: &str) -> &'a str {
    let locale = locale.trim();
    if !locale.is_empty() {
        if let Some(text) = values
            .iter()
            .find(|(tag, text)| tag.trim().eq_ignore_ascii_case(locale) && !text.is_empty())
            .map(|(_, text)| text.as_str())
        {
            return text;
        }
        let requested = normalize_tag(locale);
        if let Some(text) = values
            .iter()
            .find(|(tag, text)| normalize_tag(tag) == requested && !text.is_empty())
            .map(|(_, text)| text.as_str())
        {
            return text;
        }
        let primary = requested.split('-').next().unwrap_or(requested.as_str());
        if let Some(text) = values
            .iter()
            .find(|(tag, text)| {
                normalize_tag(tag)
                    .split('-')
                    .next()
                    .is_some_and(|tag| tag == primary)
                    && !text.is_empty()
            })
            .map(|(_, text)| text.as_str())
        {
            return text;
        }
    }
    if let Some(text) = values
        .iter()
        .find(|(tag, text)| tag.trim().eq_ignore_ascii_case("en") && !text.is_empty())
        .map(|(_, text)| text.as_str())
    {
        return text;
    }
    values
        .values()
        .find(|text| !text.is_empty())
        .map(String::as_str)
        .unwrap_or("")
}

fn normalize_tag(tag: &str) -> String {
    tag.trim().replace('_', "-").to_ascii_lowercase()
}

/// Whether `tag` looks like a locale tag. Translation keys are intentionally
/// restricted so a malformed manifest fails validation instead of silently
/// falling back forever.
pub fn valid_locale_tag(tag: &str) -> bool {
    let tag = tag.trim();
    !tag.is_empty()
        && tag.len() <= MAX_LOCALE_TAG_BYTES
        && tag.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn localized(entries: &[(&str, &str)]) -> LocalizedText {
        LocalizedText::Localized(
            entries
                .iter()
                .map(|(tag, text)| (tag.to_string(), text.to_string()))
                .collect(),
        )
    }

    #[test]
    fn plain_text_is_returned_for_every_locale() {
        let text = LocalizedText::new("Preview");
        assert_eq!(text.resolve("en"), "Preview");
        assert_eq!(text.resolve("zh-CN"), "Preview");
        assert_eq!(text.resolve(""), "Preview");
    }

    #[test]
    fn exact_tag_wins_and_is_case_insensitive() {
        let text = localized(&[("zh-CN", "预览"), ("en", "Preview")]);
        assert_eq!(text.resolve("zh-CN"), "预览");
        assert_eq!(text.resolve("zh-cn"), "预览");
        assert_eq!(text.resolve("en"), "Preview");
    }

    #[test]
    fn falls_back_to_primary_language_then_english() {
        let text = localized(&[("zh", "预览"), ("en", "Preview")]);
        assert_eq!(text.resolve("zh-CN"), "预览");
        assert_eq!(text.resolve("en-US"), "Preview");
        let text = localized(&[("zh-TW", "預覽"), ("en", "Preview")]);
        assert_eq!(text.resolve("zh-CN"), "預覽");
    }

    #[test]
    fn underscore_tags_match_dash_locales() {
        let text = localized(&[("zh_CN", "预览")]);
        assert_eq!(text.resolve("zh-CN"), "预览");
        let text = localized(&[("zh-CN", "预览")]);
        assert_eq!(text.resolve("zh_CN"), "预览");
    }

    #[test]
    fn final_fallback_is_the_first_non_empty_entry() {
        let text = localized(&[("de", "Vorschau"), ("fr", "")]);
        assert_eq!(text.resolve("ja"), "Vorschau");
        assert!(LocalizedText::Localized(BTreeMap::new())
            .resolve("en")
            .is_empty());
    }

    #[test]
    fn blank_detection_covers_plain_and_maps() {
        assert!(LocalizedText::new("  ").is_blank());
        assert!(localized(&[("en", " "), ("zh-CN", "")]).is_blank());
        assert!(!localized(&[("en", "Preview"), ("zh-CN", "")]).is_blank());
    }

    #[test]
    fn locale_tags_are_validated() {
        assert!(valid_locale_tag("en"));
        assert!(valid_locale_tag("zh-CN"));
        assert!(valid_locale_tag("pt_BR"));
        assert!(!valid_locale_tag(""));
        assert!(!valid_locale_tag("zh CN"));
        assert!(!valid_locale_tag("简体中文"));
    }

    #[test]
    fn deserializes_strings_and_locale_maps() {
        let plain: LocalizedText = serde_json::from_str("\"Preview\"").unwrap();
        assert_eq!(plain, LocalizedText::Plain("Preview".to_string()));
        let map: LocalizedText =
            serde_json::from_str(r#"{"en":"Preview","zh-CN":"预览"}"#).unwrap();
        assert_eq!(map.resolve("zh-CN"), "预览");
    }
}

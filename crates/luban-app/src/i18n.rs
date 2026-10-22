//! UI translations and locale switching.

use std::ops::Deref;

/// Translate a catalog key using the current locale.
pub fn tr(key: &str) -> String {
    rust_i18n::t!(key).to_string()
}

/// Translate a key and substitute `%{name}` placeholders.
pub fn tr_args(key: &str, args: &[(&str, String)]) -> String {
    let template = tr(key);
    let names: Vec<&str> = args.iter().map(|(name, _)| *name).collect();
    let values: Vec<String> = args.iter().map(|(_, value)| value.clone()).collect();
    rust_i18n::replace_patterns(&template, &names, &values)
}

/// Resolve and apply the UI language. Returns the effective locale tag.
pub fn apply_language(preference: &str) -> &'static str {
    let locale = luban_core::i18n::resolve(preference);
    rust_i18n::set_locale(locale);
    // Keep gpui-component's own strings (calendar, select, table, ...) in the
    // same language.
    gpui_kit::component::set_locale(locale);
    locale
}

/// Display name of the currently active locale.
pub fn active_language_name() -> &'static str {
    let locale = rust_i18n::locale();
    luban_core::i18n::locale_name(locale.deref())
}

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_every_locale_and_every_used_key() {
        let yaml = include_str!("../locales/ui.yml");
        let value: serde_json::Value =
            serde_saphyr::from_str(yaml).expect("UI catalog is valid YAML");
        let entries = value.as_object().expect("catalog is a map");
        let mut catalog = std::collections::BTreeSet::new();
        for (key, entry) in entries {
            if key == "_version" {
                continue;
            }
            let locales = entry.as_object().expect("locale map");
            for locale in ["en", "zh-CN"] {
                let text = locales
                    .get(locale)
                    .and_then(|value| value.as_str())
                    .unwrap_or_else(|| panic!("{key} is missing {locale}"));
                assert!(!text.trim().is_empty(), "{key} has an empty {locale}");
            }
            catalog.insert(key.as_str());
        }

        for source in [
            include_str!("app.rs"),
            include_str!("dialogs.rs"),
            include_str!("find.rs"),
            include_str!("preferences.rs"),
            include_str!("main.rs"),
        ] {
            for key in key_literals(source) {
                assert!(
                    catalog.contains(key),
                    "source uses missing catalog key {key}"
                );
            }
        }
    }

    fn key_literals(source: &str) -> Vec<&str> {
        const PREFIXES: &[&str] = &[
            "cli.",
            "common.",
            "menu.",
            "toolbar.",
            "status.",
            "header.",
            "untitled.",
            "explorer.",
            "find.",
            "ff.",
            "dialog.",
            "confirm.",
            "about.",
            "shortcuts.",
            "prefs.",
        ];
        let mut keys = Vec::new();
        let mut rest = source;
        while let Some(start) = rest.find('"') {
            rest = &rest[start + 1..];
            let Some(end) = rest.find('"') else { break };
            let candidate = &rest[..end];
            if PREFIXES
                .iter()
                .any(|prefix| candidate.starts_with(prefix) && candidate.len() > prefix.len())
            {
                keys.push(candidate);
            }
            rest = &rest[end + 1..];
        }
        keys
    }

    #[test]
    fn translations_follow_the_locale() {
        let _guard = test_lock();
        rust_i18n::set_locale("en");
        assert_eq!(tr("menu.file"), "File");
        rust_i18n::set_locale("zh-CN");
        assert_eq!(tr("menu.file"), "文件");
        assert_eq!(
            tr_args("status.opened", &[("path", "a.txt".into())]),
            "已打开 a.txt"
        );
        rust_i18n::set_locale("en");
    }
}

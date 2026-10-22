//! Locale selection and translation helpers for engine-provided strings.
//!
//! The application owns the UI catalog; this module covers strings produced by
//! the engine itself (encoding/EOL labels, language display names and errors).
//! [`resolve`] maps a user preference and the operating-system locale to one of
//! the supported tags.

use std::env;

use crate::language;

/// Locale used when nothing else matches.
pub const FALLBACK: &str = "en";

/// Locales shipped with the engine catalog.
pub const SUPPORTED: &[&str] = &["en", "zh-CN"];

/// Environment variable that overrides the persisted preference.
pub const LANGUAGE_ENV: &str = "LUBAN_NOTEPAD_LANG";

/// Resolve the effective locale from the environment, a user preference
/// (`"system"`, `"en"` or `"zh-CN"`) and the operating-system locale.
pub fn resolve(preference: &str) -> &'static str {
    let preference = env::var(LANGUAGE_ENV).unwrap_or_else(|_| preference.to_string());
    let os_locale = sys_locale::get_locale();
    resolve_from(&preference, os_locale.as_deref())
}

/// Pure form of [`resolve`], separated so that callers and tests can supply the
/// OS locale instead of reading the process environment.
pub fn resolve_from(preference: &str, os_locale: Option<&str>) -> &'static str {
    match normalize(preference).as_deref() {
        None | Some("system") => os_locale
            .and_then(normalize)
            .as_deref()
            .and_then(locale_for)
            .unwrap_or(FALLBACK),
        Some(tag) => locale_for(tag).unwrap_or(FALLBACK),
    }
}

/// Human-readable name of a locale tag, shown in the preferences window.
pub fn locale_name(tag: &str) -> &'static str {
    match tag {
        "zh-CN" => "简体中文",
        _ => "English",
    }
}

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

/// Display name for a canonical language identifier.
///
/// Canonical names stay English because they are persisted in sessions and map
/// to syntect syntax definitions; only descriptive names are translated.
pub fn language_display_name(canonical: &str) -> String {
    let key = match canonical {
        language::PLAIN_TEXT => "core.language.plain_text",
        "Assembly" => "core.language.assembly",
        "Batch" => "core.language.batch",
        "Diff" => "core.language.diff",
        "Properties" => "core.language.properties",
        _ => return canonical.to_string(),
    };
    tr(key)
}

/// Normalize POSIX and BCP 47 locale tags to lowercase BCP 47.
fn normalize(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let value = value
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('_', "-")
        .to_ascii_lowercase();
    let value = value.trim_matches('-');
    (!value.is_empty()).then(|| value.to_string())
}

fn locale_for(tag: &str) -> Option<&'static str> {
    match tag.split('-').next().unwrap_or(tag) {
        "en" => Some("en"),
        // Simplified Chinese is the fallback for every Chinese variant until a
        // Traditional Chinese catalog exists.
        "zh" => Some("zh-CN"),
        _ => None,
    }
}

/// Serialize tests that change the process-wide locale.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_preference_follows_the_os_locale() {
        assert_eq!(resolve_from("system", Some("zh-CN")), "zh-CN");
        assert_eq!(resolve_from("", Some("zh_CN.UTF-8")), "zh-CN");
        assert_eq!(resolve_from("system", Some("zh-Hans-CN")), "zh-CN");
        assert_eq!(resolve_from("system", Some("zh-TW")), "zh-CN");
        assert_eq!(resolve_from("system", Some("zh-Hant-HK")), "zh-CN");
        assert_eq!(resolve_from("system", Some("en-US")), "en");
        assert_eq!(resolve_from("system", Some("fr-FR")), "en");
        assert_eq!(resolve_from("system", Some("C")), "en");
        assert_eq!(resolve_from("system", Some("POSIX")), "en");
        assert_eq!(resolve_from("system", None), "en");
    }

    #[test]
    fn explicit_preference_overrides_the_os_locale() {
        assert_eq!(resolve_from("en", Some("zh-CN")), "en");
        assert_eq!(resolve_from("zh-CN", Some("en-US")), "zh-CN");
        assert_eq!(resolve_from("zh_TW.UTF-8", Some("en-US")), "zh-CN");
        assert_eq!(resolve_from("fr", Some("zh-CN")), "en");
    }

    #[test]
    fn language_names_are_translated_but_canonical_ids_are_not() {
        let _guard = crate::i18n::test_lock();
        rust_i18n::set_locale("en");
        assert_eq!(language_display_name(language::PLAIN_TEXT), "Plain Text");
        assert_eq!(language_display_name("Rust"), "Rust");

        rust_i18n::set_locale("zh-CN");
        assert_eq!(language_display_name(language::PLAIN_TEXT), "纯文本");
        assert_eq!(language_display_name("Assembly"), "汇编");
        assert_eq!(language_display_name("Rust"), "Rust");
        rust_i18n::set_locale("en");
    }

    #[test]
    fn engine_errors_use_the_current_locale() {
        let _guard = crate::i18n::test_lock();
        let error = crate::Error::InvalidPath(std::path::PathBuf::from("/tmp/x"));
        rust_i18n::set_locale("en");
        assert!(error.to_string().contains("invalid path"));
        rust_i18n::set_locale("zh-CN");
        assert!(error.to_string().contains("无效路径"));
        assert_eq!(crate::TextEncoding::Utf8Bom.label(), "带 BOM 的 UTF-8");
        rust_i18n::set_locale("en");
    }

    #[test]
    fn catalog_has_every_locale_and_every_used_key() {
        let yaml = include_str!("../locales/ui.yml");
        let value: serde_json::Value =
            serde_saphyr::from_str(yaml).expect("engine catalog is valid YAML");
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
            include_str!("error.rs"),
            include_str!("encoding.rs"),
            include_str!("eol.rs"),
            include_str!("i18n.rs"),
            include_str!("language.rs"),
            include_str!("workspace.rs"),
            include_str!("document.rs"),
            include_str!("search.rs"),
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
        const PREFIXES: &[&str] = &["core."];
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
}

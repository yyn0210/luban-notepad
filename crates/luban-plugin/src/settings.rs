//! Plugin settings schemas and value normalization.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::manifest::{default_setting_value, normalize_setting_value, SettingContribution};

/// Default values for a plugin's settings contributions.
pub fn defaults(settings: &[SettingContribution]) -> Map<String, Value> {
    let mut map = Map::new();
    for setting in settings {
        map.insert(setting.key.clone(), default_setting_value(setting));
    }
    map
}

/// Validate stored settings and fill missing/invalid values from defaults.
pub fn normalize(
    settings: &[SettingContribution],
    stored: Option<&Map<String, Value>>,
) -> Map<String, Value> {
    let mut map = Map::new();
    for setting in settings {
        let value = stored.and_then(|stored| stored.get(&setting.key));
        let (value, fallback) = normalize_setting_value(setting, value);
        if fallback && value != default_setting_value(setting) {
            log::warn!("plugin setting '{}' was reset to its default", setting.key);
        }
        map.insert(setting.key.clone(), value);
    }
    map
}

/// A sorted map is convenient for deterministic tests and UI rendering.
pub fn sorted(settings: &Map<String, Value>) -> BTreeMap<String, Value> {
    settings
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{SettingContribution, SettingType};

    fn setting() -> SettingContribution {
        SettingContribution {
            key: "count".into(),
            setting_type: SettingType::Number,
            label: "Count".into(),
            description: None,
            default: Value::from(3.0),
            max_length: None,
            min: Some(1.0),
            max: Some(5.0),
            step: Some(1.0),
            values: Vec::new(),
        }
    }

    #[test]
    fn fills_defaults_and_rejects_out_of_range_values() {
        let defaults = defaults(&[setting()]);
        assert_eq!(defaults.get("count"), Some(&Value::from(3.0)));
        let mut stored = Map::new();
        stored.insert("count".into(), Value::from(9.0));
        let normalized = normalize(&[setting()], Some(&stored));
        assert_eq!(normalized.get("count"), Some(&Value::from(3.0)));
        stored.insert("count".into(), Value::from(4.0));
        let normalized = normalize(&[setting()], Some(&stored));
        assert_eq!(normalized.get("count"), Some(&Value::from(4.0)));
    }
}

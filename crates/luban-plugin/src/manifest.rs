//! Plugin manifest parsing and validation.
//!
//! A plugin is a directory containing a UTF-8 `plugin.json` manifest and the
//! Lua entry file it names. Validation is strict and deterministic so that a
//! malformed plugin is rejected before any Lua code runs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::permissions::PermissionSet;

const KNOWN_FIELDS: &[&str] = &[
    "id",
    "name",
    "version",
    "api_version",
    "main",
    "description",
    "author",
    "permissions",
    "dependencies",
    "contributes",
    "activate",
    "deactivate",
];

/// Report unknown top-level fields without rejecting the manifest, so a plugin
/// written for a newer API still loads when the used fields are valid.
fn warn_unknown_fields(json: &str, dir: &Path) {
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(json) else {
        return;
    };
    for key in object.keys() {
        if !KNOWN_FIELDS.contains(&key.as_str()) {
            log::warn!(
                "{}: unknown manifest field '{key}'",
                dir.join(MANIFEST_FILE).display()
            );
        }
    }
}

/// Host API major version understood by this build.
pub const API_VERSION: u32 = 1;
/// Manifest file name inside a plugin directory.
pub const MANIFEST_FILE: &str = "plugin.json";
/// Maximum manifest size.
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
/// Maximum Lua entry file size.
pub const MAX_MAIN_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api_version: u32,
    pub main: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<PluginDependency>,
    #[serde(default)]
    pub contributes: Contributions,
    #[serde(default)]
    pub activate: Option<String>,
    #[serde(default)]
    pub deactivate: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginDependency {
    pub id: String,
    pub version: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Contributions {
    #[serde(default)]
    pub commands: Vec<CommandContribution>,
    #[serde(default)]
    pub menus: Vec<MenuContribution>,
    #[serde(default)]
    pub status_bar: Vec<StatusBarContribution>,
    #[serde(default)]
    pub settings: Vec<SettingContribution>,
    #[serde(default)]
    pub panels: Vec<PanelContribution>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandCondition {
    #[default]
    Always,
    Document,
    Save,
    EditText,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandContribution {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    pub entry: String,
    #[serde(default)]
    pub default_keybinding: Option<String>,
    #[serde(default)]
    pub when: CommandCondition,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MenuContribution {
    /// Only `plugins` is accepted by API version 1.
    pub menu: String,
    pub command: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusAlignment {
    #[default]
    Left,
    Right,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatusBarContribution {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub alignment: StatusAlignment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingType {
    Boolean,
    String,
    Number,
    Enum,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettingContribution {
    pub key: String,
    #[serde(rename = "type")]
    pub setting_type: SettingType,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default: Value,
    #[serde(default)]
    pub max_length: Option<usize>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub values: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanelPlacement {
    #[default]
    Right,
    Bottom,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PanelContribution {
    pub id: String,
    pub title: String,
    pub entry: String,
    #[serde(default)]
    pub placement: PanelPlacement,
}

impl PluginManifest {
    /// Load and validate a manifest from a plugin directory.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join(MANIFEST_FILE);
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if metadata.len() as usize > MAX_MANIFEST_BYTES {
            return Err(format!(
                "{} is larger than {} bytes",
                path.display(),
                MAX_MANIFEST_BYTES
            ));
        }
        let json = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::parse(&json, dir)
    }

    /// Parse a manifest from JSON text and validate it against `dir`.
    pub fn parse(json: &str, dir: &Path) -> Result<Self, String> {
        warn_unknown_fields(json, dir);
        let manifest: Self = serde_json::from_str(json)
            .map_err(|error| format!("invalid plugin manifest JSON: {error}"))?;
        manifest.validate(dir)?;
        Ok(manifest)
    }

    pub fn validate(&self, dir: &Path) -> Result<(), String> {
        let mut errors = Vec::new();
        if !is_plugin_id(&self.id) {
            errors.push(format!(
                "plugin id '{}' must match [a-z0-9][a-z0-9._-]*",
                self.id
            ));
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 120 {
            errors.push("plugin name must be 1-120 characters".to_string());
        }
        if self.version.trim().is_empty() || semver::Version::parse(&self.version).is_err() {
            errors.push(format!("plugin version '{}' is not SemVer", self.version));
        }
        if self.api_version > API_VERSION {
            errors.push(format!(
                "plugin api_version {} is newer than the supported version {API_VERSION}",
                self.api_version
            ));
        } else if self.api_version < API_VERSION {
            log::warn!(
                "plugin '{}' targets API version {}; loading with compatibility warnings",
                self.id,
                self.api_version
            );
        }
        if self
            .description
            .as_ref()
            .is_some_and(|description| description.chars().count() > 500)
        {
            errors.push("plugin description is longer than 500 characters".to_string());
        }
        if let Some(message) = validate_main_path(&self.main) {
            errors.push(message);
        } else if let Err(message) = self.main_path(dir) {
            errors.push(message);
        }
        for dependency in &self.dependencies {
            if !is_plugin_id(&dependency.id) {
                errors.push(format!("dependency id '{}' is invalid", dependency.id));
            }
            if semver::VersionReq::parse(&dependency.version).is_err() {
                errors.push(format!(
                    "dependency '{}' has an invalid version range '{}'",
                    dependency.id, dependency.version
                ));
            }
        }
        if let Some(activate) = &self.activate {
            if !is_identifier(activate) {
                errors.push(format!(
                    "activate function '{activate}' is not an identifier"
                ));
            }
        }
        if let Some(deactivate) = &self.deactivate {
            if !is_identifier(deactivate) {
                errors.push(format!(
                    "deactivate function '{deactivate}' is not an identifier"
                ));
            }
        }
        self.validate_contributions(&mut errors);
        self.validate_permissions(&mut errors);

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn validate_contributions(&self, errors: &mut Vec<String>) {
        let mut command_ids = BTreeSet::new();
        for command in &self.contributes.commands {
            if !is_command_id(&command.id) {
                errors.push(format!("command id '{}' is invalid", command.id));
            }
            if !command_ids.insert(command.id.clone()) {
                errors.push(format!("duplicate command id '{}'", command.id));
            }
            if command.label.trim().is_empty() || command.label.chars().count() > 120 {
                errors.push(format!(
                    "command '{}' label must be 1-120 characters",
                    command.id
                ));
            }
            if !is_identifier(&command.entry) {
                errors.push(format!(
                    "command '{}' entry '{}' is not an identifier",
                    command.id, command.entry
                ));
            }
            if command
                .default_keybinding
                .as_deref()
                .is_some_and(|binding| !looks_like_binding(binding))
            {
                errors.push(format!(
                    "command '{}' has an invalid default keybinding",
                    command.id
                ));
            }
            if command.id.starts_with("macro.")
                || command.id.starts_with("palette.")
                || command.id.starts_with("help.")
            {
                errors.push(format!(
                    "command id '{}' uses a reserved prefix",
                    command.id
                ));
            }
        }

        for menu in &self.contributes.menus {
            if menu.menu != "plugins" {
                errors.push(format!(
                    "menu '{}' is not supported; API version 1 only accepts 'plugins'",
                    menu.menu
                ));
            }
            if !command_ids.contains(&menu.command) {
                errors.push(format!(
                    "menu references unknown command '{}'",
                    menu.command
                ));
            }
        }

        let mut status_ids = BTreeSet::new();
        for item in &self.contributes.status_bar {
            if !is_command_id(&item.id) {
                errors.push(format!("status bar id '{}' is invalid", item.id));
            }
            if !status_ids.insert(item.id.clone()) {
                errors.push(format!("duplicate status bar id '{}'", item.id));
            }
            if item.text.chars().count() > 200 {
                errors.push(format!("status bar '{}' text is too long", item.id));
            }
            if let Some(command) = &item.command {
                if !command_ids.contains(command) {
                    errors.push(format!(
                        "status bar '{}' references unknown command '{}'",
                        item.id, command
                    ));
                }
            }
        }

        let mut setting_keys = BTreeSet::new();
        for setting in &self.contributes.settings {
            if !is_setting_key(&setting.key) {
                errors.push(format!("setting key '{}' is invalid", setting.key));
            }
            if !setting_keys.insert(setting.key.clone()) {
                errors.push(format!("duplicate setting key '{}'", setting.key));
            }
            if setting.label.trim().is_empty() {
                errors.push(format!("setting '{}' has an empty label", setting.key));
            }
            if let Err(message) = validate_setting_schema(setting) {
                errors.push(format!("setting '{}': {message}", setting.key));
            }
        }

        let mut panel_ids = BTreeSet::new();
        for panel in &self.contributes.panels {
            if !is_command_id(&panel.id) {
                errors.push(format!("panel id '{}' is invalid", panel.id));
            }
            if !panel_ids.insert(panel.id.clone()) {
                errors.push(format!("duplicate panel id '{}'", panel.id));
            }
            if panel.title.trim().is_empty() {
                errors.push(format!("panel '{}' has an empty title", panel.id));
            }
            if !is_identifier(&panel.entry) {
                errors.push(format!(
                    "panel '{}' entry '{}' is not an identifier",
                    panel.id, panel.entry
                ));
            }
        }
    }

    fn validate_permissions(&self, _errors: &mut Vec<String>) {
        let set = PermissionSet::parse(&self.permissions);
        for permission in set.unknown() {
            log::warn!(
                "plugin '{}' declares unknown permission '{}'",
                self.id,
                permission
            );
        }
    }

    pub fn permissions(&self) -> PermissionSet {
        PermissionSet::parse(&self.permissions)
    }

    /// Full path of the Lua entry file, validated to stay inside `dir`.
    pub fn main_path(&self, dir: &Path) -> Result<PathBuf, String> {
        let path = dir.join(&self.main);
        let canonical = path
            .canonicalize()
            .map_err(|error| format!("cannot resolve plugin entry {}: {error}", path.display()))?;
        let root = dir.canonicalize().map_err(|error| {
            format!("cannot resolve plugin directory {}: {error}", dir.display())
        })?;
        if !canonical.starts_with(&root) {
            return Err(format!(
                "plugin entry {} escapes the plugin directory",
                path.display()
            ));
        }
        let metadata = fs::metadata(&canonical).map_err(|error| {
            format!("cannot read plugin entry {}: {error}", canonical.display())
        })?;
        if !metadata.is_file() {
            return Err(format!(
                "plugin entry {} is not a file",
                canonical.display()
            ));
        }
        if metadata.len() as usize > MAX_MAIN_BYTES {
            return Err(format!(
                "plugin entry {} is larger than {} bytes",
                canonical.display(),
                MAX_MAIN_BYTES
            ));
        }
        Ok(canonical)
    }

    /// Contribution defaults keyed by setting key.
    pub fn setting_defaults(&self) -> BTreeMap<String, Value> {
        self.contributes
            .settings
            .iter()
            .map(|setting| (setting.key.clone(), default_setting_value(setting)))
            .collect()
    }
}

/// Validate a raw path for `main` without touching the filesystem.
fn validate_main_path(main: &str) -> Option<String> {
    let path = Path::new(main);
    if main.trim().is_empty() {
        return Some("plugin main is empty".to_string());
    }
    if path.is_absolute() {
        return Some("plugin main must be relative".to_string());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => {
                return Some(format!(
                    "plugin main '{main}' must not escape the plugin directory"
                ))
            }
        }
    }
    if !main.to_ascii_lowercase().ends_with(".lua") {
        return Some(format!("plugin main '{main}' must be a .lua file"));
    }
    None
}

pub fn default_setting_value(setting: &SettingContribution) -> Value {
    match setting.setting_type {
        SettingType::Boolean => Value::Bool(setting.default.as_bool().unwrap_or(false)),
        SettingType::String => {
            Value::String(setting.default.as_str().unwrap_or_default().to_string())
        }
        SettingType::Number => {
            let value = setting.default.as_f64().unwrap_or(0.0);
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .unwrap_or_else(|| Value::Number(0.into()))
        }
        SettingType::Enum => Value::String(
            setting
                .default
                .as_str()
                .filter(|value| setting.values.iter().any(|item| item == value))
                .or_else(|| setting.values.first().map(String::as_str))
                .unwrap_or_default()
                .to_string(),
        ),
    }
}

pub fn validate_setting_schema(setting: &SettingContribution) -> Result<(), String> {
    if setting.key.trim().is_empty() {
        return Err("key is empty".to_string());
    }
    match setting.setting_type {
        SettingType::Boolean => {
            if !setting.default.is_null() && !setting.default.is_boolean() {
                return Err("default must be a boolean".to_string());
            }
        }
        SettingType::String => {
            if !setting.default.is_null() && !setting.default.is_string() {
                return Err("default must be a string".to_string());
            }
            if setting.max_length == Some(0) {
                return Err("max_length must be positive".to_string());
            }
        }
        SettingType::Number => {
            if !setting.default.is_null() && setting.default.as_f64().is_none() {
                return Err("default must be a number".to_string());
            }
            if let (Some(min), Some(max)) = (setting.min, setting.max) {
                if min > max {
                    return Err("min is greater than max".to_string());
                }
            }
            if setting
                .step
                .is_some_and(|step| !step.is_finite() || step <= 0.0)
            {
                return Err("step must be positive".to_string());
            }
        }
        SettingType::Enum => {
            if setting.values.is_empty() {
                return Err("enum values must not be empty".to_string());
            }
            if !setting.default.is_null()
                && !setting
                    .default
                    .as_str()
                    .is_some_and(|value| setting.values.iter().any(|item| item == value))
            {
                return Err("default must be one of the enum values".to_string());
            }
        }
    }
    Ok(())
}

/// Normalize a stored value against a contribution schema.
///
/// Invalid or missing values fall back to the contribution default; the second
/// element reports whether a fallback happened.
pub fn normalize_setting_value(
    setting: &SettingContribution,
    value: Option<&Value>,
) -> (Value, bool) {
    let Some(value) = value else {
        return (default_setting_value(setting), true);
    };
    let normalized = match setting.setting_type {
        SettingType::Boolean => value.as_bool().map(Value::Bool),
        SettingType::String => value.as_str().and_then(|text| {
            let max = setting.max_length.unwrap_or(64 * 1024).min(1024 * 1024);
            (text.chars().count() <= max).then(|| Value::String(text.to_string()))
        }),
        SettingType::Number => value.as_f64().and_then(|number| {
            if !number.is_finite()
                || setting.min.is_some_and(|min| number < min)
                || setting.max.is_some_and(|max| number > max)
            {
                None
            } else {
                serde_json::Number::from_f64(number).map(Value::Number)
            }
        }),
        SettingType::Enum => value
            .as_str()
            .filter(|text| setting.values.iter().any(|item| item == text))
            .map(|text| Value::String(text.to_string())),
    };
    match normalized {
        Some(value) => (value, false),
        None => (default_setting_value(setting), true),
    }
}

/// Reject values that cannot be stored as a setting.
pub fn setting_value_is_valid(setting: &SettingContribution, value: &Value) -> bool {
    !normalize_setting_value(setting, Some(value)).1
}

pub fn is_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        && id.chars().all(|ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-')
        })
}

pub fn is_command_id(id: &str) -> bool {
    is_plugin_id(id)
}

pub fn is_setting_key(key: &str) -> bool {
    !key.is_empty()
        && key.chars().count() <= 64
        && key
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

pub fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn looks_like_binding(binding: &str) -> bool {
    let binding = binding.trim();
    if binding.is_empty() {
        return false;
    }
    // The GUI crate performs the authoritative parse. Here we only reject
    // obvious mistakes so a manifest can never install a bare key.
    binding.contains('+') || binding.to_ascii_lowercase().starts_with('f')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json(extra: &str) -> String {
        format!(
            r#"{{
                "id": "com.example.demo",
                "name": "Demo",
                "version": "1.0.0",
                "api_version": 1,
                "main": "main.lua",
                "permissions": ["filesystem"],
                "contributes": {{
                    "commands": [
                        {{ "id": "example.format", "label": "Format", "entry": "format", "default_keybinding": "ctrl+alt+f" }}
                    ],
                    "menus": [{{ "menu": "plugins", "command": "example.format" }}],
                    "status_bar": [{{ "id": "example.status", "text": "ready", "command": "example.format" }}],
                    "settings": [{{ "key": "uppercase", "type": "boolean", "label": "Uppercase", "default": false }}],
                    "panels": [{{ "id": "example.panel", "title": "Demo", "entry": "list", "placement": "right" }}]
                }}
                {extra}
            }}"#
        )
    }

    #[test]
    fn parses_a_valid_manifest() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.lua"), "return {}").unwrap();
        let manifest = PluginManifest::parse(&manifest_json(""), dir.path()).unwrap();
        assert_eq!(manifest.id, "com.example.demo");
        assert_eq!(manifest.contributes.commands.len(), 1);
        assert_eq!(
            manifest.setting_defaults().get("uppercase"),
            Some(&Value::Bool(false))
        );
    }

    #[test]
    fn rejects_invalid_identity_and_version() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.lua"), "return {}").unwrap();
        let json = manifest_json("").replace("com.example.demo", "Bad ID");
        assert!(PluginManifest::parse(&json, dir.path()).is_err());
        let json = manifest_json("").replace("\"1.0.0\"", "\"nope\"");
        assert!(PluginManifest::parse(&json, dir.path()).is_err());
        let json = manifest_json("").replace("\"api_version\": 1", "\"api_version\": 2");
        assert!(PluginManifest::parse(&json, dir.path()).is_err());
    }

    #[test]
    fn rejects_path_escape_and_missing_entry() {
        let dir = tempfile::tempdir().unwrap();
        let json = manifest_json("").replace("\"main\": \"main.lua\"", "\"main\": \"../x.lua\"");
        assert!(PluginManifest::parse(&json, dir.path()).is_err());
        let json = manifest_json("");
        assert!(PluginManifest::parse(&json, dir.path()).is_err());
    }

    #[test]
    fn rejects_duplicate_and_unknown_contributions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.lua"), "return {}").unwrap();
        let json = manifest_json("").replace(
            r#"{ "menu": "plugins", "command": "example.format" }"#,
            r#"{ "menu": "plugins", "command": "missing.command" }"#,
        );
        assert!(PluginManifest::parse(&json, dir.path()).is_err());
        let json = manifest_json("").replace(
            r#"{ "id": "example.format", "label": "Format", "entry": "format", "default_keybinding": "ctrl+alt+f" }"#,
            r#"{ "id": "example.format", "label": "Format", "entry": "format" }, { "id": "example.format", "label": "Again", "entry": "again" }"#,
        );
        assert!(PluginManifest::parse(&json, dir.path()).is_err());
    }

    #[test]
    fn validates_and_normalizes_setting_values() {
        let setting = SettingContribution {
            key: "count".into(),
            setting_type: SettingType::Number,
            label: "Count".into(),
            description: None,
            default: Value::from(2),
            max_length: None,
            min: Some(1.0),
            max: Some(10.0),
            step: Some(1.0),
            values: Vec::new(),
        };
        assert!(setting_value_is_valid(&setting, &Value::from(5)));
        assert!(!setting_value_is_valid(&setting, &Value::from(0)));
        assert!(!setting_value_is_valid(&setting, &Value::from(11)));
        let (value, fallback) = normalize_setting_value(&setting, Some(&Value::from(0)));
        assert!(fallback);
        assert_eq!(value, Value::from(2.0));
    }
}

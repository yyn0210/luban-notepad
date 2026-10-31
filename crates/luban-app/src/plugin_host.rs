//! Application-side plugin host.
//!
//! The headless `luban-plugin` crate owns manifests, permissions, dependency
//! resolution and Lua. This module turns its contributions into application
//! commands/status items/panels and maps document callbacks onto the running
//! editor.

use std::collections::BTreeMap;
use std::path::PathBuf;

use luban_core::Settings;
use luban_plugin::install::{self, InstallRecords};
use luban_plugin::manifest::{
    CommandCondition, PanelPlacement, PluginManifest, SettingContribution, StatusAlignment,
};
use luban_plugin::registry::{LoadPlan, PluginRegistry, PluginState};
use luban_plugin::runtime::{
    CallInput, PanelContent, PluginIdentity, PluginRuntime, RuntimeCall, RuntimeOutput,
};

use crate::commands::{CommandWhen, DynamicCommandSpec};

#[derive(Clone, Debug)]
pub struct PluginCommand {
    pub id: &'static str,
    pub label: String,
    pub plugin_id: String,
    pub entry: String,
    pub when: CommandWhen,
    pub default_binding: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PluginStatusItem {
    pub plugin_id: String,
    pub id: String,
    pub text: String,
    pub command: Option<&'static str>,
    pub alignment: StatusAlignment,
}

#[derive(Clone, Debug)]
pub struct PluginPanelSpec {
    pub plugin_id: String,
    pub id: String,
    pub title: String,
    pub entry: String,
    pub placement: PanelPlacement,
}

pub struct PluginHost {
    pub root: PathBuf,
    pub records: InstallRecords,
    pub registry: PluginRegistry,
    pub plan: LoadPlan,
    pub commands: Vec<PluginCommand>,
    pub status_items: Vec<PluginStatusItem>,
    pub panels: Vec<PluginPanelSpec>,
    pub settings: BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
    pub schemas: BTreeMap<String, Vec<SettingContribution>>,
    pub errors: Vec<String>,
    dynamic_status: BTreeMap<(String, String), String>,
    runtimes: BTreeMap<String, PluginRuntime>,
}

impl PluginHost {
    pub fn load(settings: &Settings, safe_mode: bool) -> Self {
        let root = luban_core::settings::plugins_dir();
        let mut errors = Vec::new();
        if let Err(error) = std::fs::create_dir_all(&root) {
            errors.push(format!("cannot create {}: {error}", root.display()));
        }
        let records = match install::load_records(&root) {
            Ok(records) => records,
            Err(error) => {
                errors.push(error);
                InstallRecords::default()
            }
        };
        let registry = PluginRegistry::discover(&root);
        for error in &registry.errors {
            errors.push(format!("{}: {}", error.path.display(), error.message));
        }
        let disabled = settings.disabled_plugins.iter().cloned().collect();
        let plugins_enabled = settings.plugins_enabled && !safe_mode;
        let mut plan = registry.plan(plugins_enabled, &disabled, &records);
        let mut runtimes = BTreeMap::new();
        let mut commands = Vec::new();
        let mut status_items = Vec::new();
        let mut panels = Vec::new();
        let mut plugin_settings = BTreeMap::new();
        let mut schemas = BTreeMap::new();
        let mut dynamic_status = BTreeMap::new();

        for id in plan.order.clone() {
            let Some(entry) = registry.get(&id) else {
                continue;
            };
            let Some(manifest) = &entry.manifest else {
                continue;
            };
            let plugin_settings_map = luban_plugin::settings::normalize(
                &manifest.contributes.settings,
                settings.plugin_settings.get(&id),
            );
            let identity = PluginIdentity {
                id: id.clone(),
                version: manifest.version.clone(),
                data_dir: entry.dir.join("data"),
            };
            let allowed_roots = vec![entry.dir.join("data")];
            let input = CallInput {
                plugin: identity,
                document: None,
                settings: plugin_settings_map.clone(),
                allowed_roots,
            };
            let mut runtime =
                match PluginRuntime::new(manifest, &entry.dir, entry.permissions.clone()) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        errors.push(format!("plugin '{id}': {error}"));
                        plan.states
                            .insert(id.clone(), PluginState::LoadError(error));
                        continue;
                    }
                };
            let activation = runtime.activate(input);
            for action in &activation.output.actions {
                log::debug!("plugin '{id}' activation action: {action:?}");
            }
            if let Some(error) = &activation.error {
                errors.push(error.clone());
                plan.states
                    .insert(id.clone(), PluginState::LoadError(error.clone()));
                continue;
            }
            for item in &manifest.contributes.status_bar {
                if let Some(command) = &item.command {
                    let leaked: &'static str = Box::leak(command.clone().into_boxed_str());
                    status_items.push(PluginStatusItem {
                        plugin_id: id.clone(),
                        id: item.id.clone(),
                        text: item.text.clone(),
                        command: Some(leaked),
                        alignment: item.alignment,
                    });
                } else {
                    status_items.push(PluginStatusItem {
                        plugin_id: id.clone(),
                        id: item.id.clone(),
                        text: item.text.clone(),
                        command: None,
                        alignment: item.alignment,
                    });
                }
                dynamic_status.insert((id.clone(), item.id.clone()), item.text.clone());
            }
            for panel in &manifest.contributes.panels {
                panels.push(PluginPanelSpec {
                    plugin_id: id.clone(),
                    id: panel.id.clone(),
                    title: panel.title.clone(),
                    entry: panel.entry.clone(),
                    placement: panel.placement,
                });
            }
            for command in &manifest.contributes.commands {
                if crate::commands::spec(&command.id).is_some()
                    || commands
                        .iter()
                        .any(|existing: &PluginCommand| existing.id == command.id)
                {
                    errors.push(format!(
                        "plugin '{}' command '{}' conflicts with an existing command",
                        id, command.id
                    ));
                    continue;
                }
                let leaked: &'static str = Box::leak(command.id.clone().into_boxed_str());
                commands.push(PluginCommand {
                    id: leaked,
                    label: command.label.clone(),
                    plugin_id: id.clone(),
                    entry: command.entry.clone(),
                    when: match command.when {
                        CommandCondition::Always => CommandWhen::Always,
                        CommandCondition::Document => CommandWhen::HasDocument,
                        CommandCondition::Save => CommandWhen::CanSave,
                        CommandCondition::EditText => CommandWhen::CanEditFullText,
                    },
                    default_binding: command.default_keybinding.clone(),
                });
            }
            schemas.insert(id.clone(), manifest.contributes.settings.clone());
            plugin_settings.insert(id.clone(), plugin_settings_map);
            runtimes.insert(id.clone(), runtime);
        }

        Self {
            root,
            records,
            registry,
            plan,
            commands,
            status_items,
            panels,
            settings: plugin_settings,
            schemas,
            errors,
            dynamic_status,
            runtimes,
        }
    }

    pub fn dynamic_specs(&self) -> Vec<DynamicCommandSpec> {
        let mut static_defaults: std::collections::BTreeSet<String> = crate::commands::COMMANDS
            .iter()
            .filter_map(|command| command.default_binding)
            .map(|binding| binding.to_ascii_lowercase())
            .collect();
        self.commands
            .iter()
            .map(|command| {
                let default_binding = command.default_binding.clone().filter(|binding| {
                    if crate::commands::parse_binding(binding).is_err() {
                        log::warn!(
                            "plugin '{}' command '{}' has an invalid default binding",
                            command.plugin_id,
                            command.id
                        );
                        return false;
                    }
                    if !static_defaults.insert(binding.to_ascii_lowercase()) {
                        log::warn!(
                            "plugin '{}' command '{}' default binding conflicts with another command",
                            command.plugin_id,
                            command.id
                        );
                        return false;
                    }
                    true
                });
                DynamicCommandSpec {
                    id: command.id,
                    default_binding,
                }
            })
            .collect()
    }

    pub fn command(&self, id: &str) -> Option<&PluginCommand> {
        self.commands.iter().find(|command| command.id == id)
    }

    pub fn has_command(&self, id: &str) -> bool {
        self.command(id).is_some()
    }

    /// Command IDs declared by every valid manifest, including plugins that are
    /// disabled, unapproved or failed to activate. Used to preserve shortcut
    /// overrides while a plugin is temporarily unavailable.
    pub fn known_command_ids(&self) -> std::collections::HashSet<String> {
        self.registry
            .entries
            .values()
            .filter_map(|entry| entry.manifest.as_ref())
            .flat_map(|manifest| manifest.contributes.commands.iter())
            .map(|command| command.id.clone())
            .collect()
    }

    pub fn run_command(
        &mut self,
        plugin_id: &str,
        entry: &str,
        command_id: &str,
        input: CallInput,
    ) -> Option<RuntimeCall<()>> {
        let runtime = self.runtimes.get_mut(plugin_id)?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.call_command(entry, command_id, input)
        })) {
            Ok(call) => Some(call),
            Err(_) => {
                log::error!("plugin '{plugin_id}' panicked while running '{command_id}'");
                Some(RuntimeCall {
                    output: RuntimeOutput {
                        actions: Vec::new(),
                    },
                    value: None,
                    error: Some(format!("plugin '{plugin_id}' panicked")),
                })
            }
        }
    }

    pub fn panel_content(
        &mut self,
        plugin_id: &str,
        panel_id: &str,
        input: CallInput,
    ) -> Option<RuntimeCall<PanelContent>> {
        let panel = self
            .panels
            .iter()
            .find(|panel| panel.plugin_id == plugin_id && panel.id == panel_id)?
            .clone();
        let runtime = self.runtimes.get_mut(plugin_id)?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.call_panel(&panel.entry, input)
        })) {
            Ok(call) => Some(call),
            Err(_) => {
                log::error!("plugin '{plugin_id}' panicked while rendering panel '{panel_id}'");
                Some(RuntimeCall {
                    output: RuntimeOutput {
                        actions: Vec::new(),
                    },
                    value: None,
                    error: Some(format!("plugin '{plugin_id}' panicked")),
                })
            }
        }
    }

    pub fn status_text(&self, plugin_id: &str, id: &str) -> Option<&str> {
        self.dynamic_status
            .get(&(plugin_id.to_string(), id.to_string()))
            .map(String::as_str)
            .or_else(|| {
                self.status_items
                    .iter()
                    .find(|item| item.plugin_id == plugin_id && item.id == id)
                    .map(|item| item.text.as_str())
            })
    }

    pub fn set_status(&mut self, plugin_id: &str, id: &str, text: String) {
        self.dynamic_status
            .insert((plugin_id.to_string(), id.to_string()), text);
    }

    pub fn status_items(&self, alignment: StatusAlignment) -> Vec<&PluginStatusItem> {
        self.status_items
            .iter()
            .filter(|item| item.alignment == alignment)
            .collect()
    }

    pub fn settings_for(&self, plugin_id: &str) -> serde_json::Map<String, serde_json::Value> {
        self.settings.get(plugin_id).cloned().unwrap_or_default()
    }

    pub fn update_setting(&mut self, plugin_id: &str, key: &str, value: serde_json::Value) -> bool {
        let Some(schema) = self
            .schemas
            .get(plugin_id)
            .and_then(|schema| schema.iter().find(|setting| setting.key == key))
        else {
            return false;
        };
        if !luban_plugin::manifest::setting_value_is_valid(schema, &value) {
            return false;
        }
        self.settings
            .entry(plugin_id.to_string())
            .or_default()
            .insert(key.to_string(), value);
        true
    }

    pub fn manifest(&self, plugin_id: &str) -> Option<&PluginManifest> {
        self.registry
            .get(plugin_id)
            .and_then(|entry| entry.manifest.as_ref())
    }

    pub fn approved(&self, plugin_id: &str) -> bool {
        let Some(manifest) = self.manifest(plugin_id) else {
            return false;
        };
        install::approval_status(&self.records, manifest).is_approved()
    }

    pub fn state(&self, plugin_id: &str) -> Option<&PluginState> {
        self.plan.states.get(plugin_id)
    }

    /// Installed plugins whose manifests depend on `plugin_id`.
    pub fn dependents_of(&self, plugin_id: &str) -> Vec<String> {
        dependent_plugin_ids(&self.registry, plugin_id)
    }
}

fn dependent_plugin_ids(registry: &PluginRegistry, plugin_id: &str) -> Vec<String> {
    let mut dependents: Vec<String> = registry
        .entries
        .values()
        .filter(|entry| entry.id != plugin_id)
        .filter(|entry| {
            entry.manifest.as_ref().is_some_and(|manifest| {
                manifest
                    .dependencies
                    .iter()
                    .any(|dependency| dependency.id == plugin_id)
            })
        })
        .map(|entry| entry.id.clone())
        .collect();
    dependents.sort();
    dependents
}

#[cfg(test)]
mod tests {
    use super::*;
    use luban_plugin::manifest::PluginDependency;
    use luban_plugin::registry::PluginEntry;

    fn entry(id: &str, dependencies: &[&str]) -> PluginEntry {
        PluginEntry {
            id: id.to_string(),
            dir: PathBuf::from(id),
            manifest: Some(PluginManifest {
                id: id.to_string(),
                name: id.to_string(),
                version: "1.0.0".to_string(),
                api_version: 1,
                main: "main.lua".to_string(),
                description: None,
                author: None,
                permissions: Vec::new(),
                dependencies: dependencies
                    .iter()
                    .map(|id| PluginDependency {
                        id: (*id).to_string(),
                        version: ">=1.0.0".to_string(),
                    })
                    .collect(),
                contributes: Default::default(),
                activate: None,
                deactivate: None,
            }),
            permissions: Default::default(),
            error: None,
        }
    }

    #[test]
    fn finds_dependents_of_a_plugin() {
        let mut registry = PluginRegistry::default();
        for item in [
            entry("base", &[]),
            entry("first", &["base"]),
            entry("second", &["base", "other"]),
            entry("unrelated", &["other"]),
        ] {
            registry.entries.insert(item.id.clone(), item);
        }
        assert_eq!(dependent_plugin_ids(&registry, "base"), ["first", "second"]);
        assert!(dependent_plugin_ids(&registry, "second").is_empty());
    }
}

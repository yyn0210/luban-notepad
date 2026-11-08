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
    CommandCondition, PanelPlacement, PanelRefresh, PluginManifest, SettingContribution,
    StatusAlignment,
};
use luban_plugin::registry::{LoadPlan, PluginRegistry, PluginState};
use luban_plugin::runtime::{
    CallContextKind, CallInput, PanelContent, PluginIdentity, PluginRuntime, RuntimeCall,
    RuntimeOutput,
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
    /// Whether the manifest contributes this command to the Plugins menu.
    pub in_menu: bool,
}

fn contributes_to_plugins_menu(manifest: &PluginManifest, command_id: &str) -> bool {
    manifest
        .contributes
        .menus
        .iter()
        .any(|menu| menu.menu == "plugins" && menu.command == command_id)
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
    pub width: Option<u16>,
    pub refresh: PanelRefresh,
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

/// Where a plugin-requested command run (`HostAction::RunCommand`) routes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunCommandRoute {
    /// `depth > MAX_COMMAND_DEPTH`: the cascade is rejected.
    DepthExceeded,
    /// A built-in command; args, if any, are ignored.
    BuiltIn,
    /// Not built-in: the host resolves a live plugin command or reports the
    /// command as not found.
    HostCommand,
}

/// Pure routing decision for a plugin-requested command run, consumed by the
/// app-side action executor. `depth` is the depth of the command being
/// launched (the requesting command's `command_depth` + 1); the runtime's own
/// `command.run` applies the same cap, so both layers agree on the limit.
pub(crate) fn route_run_command(id: &str, depth: u8) -> RunCommandRoute {
    if depth > luban_plugin::runtime::MAX_COMMAND_DEPTH {
        return RunCommandRoute::DepthExceeded;
    }
    if crate::commands::spec(id).is_some() {
        return RunCommandRoute::BuiltIn;
    }
    RunCommandRoute::HostCommand
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
                command_depth: 0,
                context_kind: CallContextKind::default(),
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
                    width: panel.width,
                    refresh: panel.refresh,
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
                    in_menu: contributes_to_plugins_menu(manifest, &command.id),
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

    /// Run a plugin command entry. Quarantine contract with the runtime: the
    /// runtime records non-panic outcomes itself (inside `call_command`), so
    /// the host counts only panics here — `record_outcome` cannot run while
    /// the call unwinds. One counter, no double-counting.
    pub fn run_command(
        &mut self,
        plugin_id: &str,
        entry: &str,
        command_id: &str,
        args: Option<serde_json::Value>,
        input: CallInput,
    ) -> Option<RuntimeCall<()>> {
        let runtime = self.runtimes.get_mut(plugin_id)?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.call_command(entry, command_id, args, input)
        }));
        Some(match result {
            Ok(call) => call,
            Err(_) => {
                // Panic path: the runtime's own recording never ran.
                runtime.record_failure();
                log::error!("plugin '{plugin_id}' panicked while running '{command_id}'");
                RuntimeCall {
                    output: RuntimeOutput {
                        actions: Vec::new(),
                    },
                    value: None,
                    error: Some(format!("plugin '{plugin_id}' panicked")),
                }
            }
        })
    }

    /// Render one plugin panel. Quarantine contract with the runtime: the
    /// runtime records non-panic outcomes itself (inside `call_panel`), so
    /// the host counts only panics here — `record_outcome` cannot run while
    /// the call unwinds. One counter, no double-counting.
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
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.call_panel(&panel.entry, input)
        }));
        Some(match result {
            Ok(call) => call,
            Err(_) => {
                // Panic path: the runtime's own recording never ran.
                runtime.record_failure();
                log::error!("plugin '{plugin_id}' panicked while rendering panel '{panel_id}'");
                RuntimeCall {
                    output: RuntimeOutput {
                        actions: Vec::new(),
                    },
                    value: None,
                    error: Some(format!("plugin '{plugin_id}' panicked")),
                }
            }
        })
    }

    // The dispatch wrappers below are consumed by the Task 10 event/timer
    // pump (and quarantine auto-disable); until then only the tests call them.

    /// Active plugin ids whose manifest declares `event` and whose runtime
    /// loaded — the Task 10 event pump dispatches each of these.
    #[allow(dead_code)]
    pub fn listeners_of(&self, event: &str) -> Vec<String> {
        self.plan
            .order
            .iter()
            .filter(|id| {
                self.runtimes.contains_key(*id)
                    && self
                        .manifest(id)
                        .is_some_and(|manifest| manifest.events.iter().any(|e| e == event))
            })
            .cloned()
            .collect()
    }

    /// Dispatch a declared event to one plugin. Returns `None` when the plugin
    /// has no runtime (disabled/failed to load). Same quarantine contract as
    /// [`Self::run_command`]: the runtime records non-panic outcomes inside
    /// `dispatch_event`; the host counts only panics — one counter, no
    /// double-counting.
    #[allow(dead_code)]
    pub fn dispatch_event(
        &mut self,
        plugin_id: &str,
        event: &str,
        input: CallInput,
    ) -> Option<RuntimeCall<()>> {
        let runtime = self.runtimes.get_mut(plugin_id)?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.dispatch_event(event, input)
        }));
        Some(match result {
            Ok(call) => call,
            Err(_) => {
                // Panic path: the runtime's own recording never ran.
                runtime.record_failure();
                log::error!("plugin '{plugin_id}' panicked while handling event '{event}'");
                RuntimeCall {
                    output: RuntimeOutput::default(),
                    value: Some(()),
                    error: Some(format!("plugin '{plugin_id}' panicked in event '{event}'")),
                }
            }
        })
    }

    /// Due timer ids of one plugin (BTreeMap key order; the Task 10 pump caps
    /// dispatches per tick). Empty for a plugin without a runtime.
    #[allow(dead_code)]
    pub fn due_timer_ids(&self, plugin_id: &str, now: std::time::Instant) -> Vec<String> {
        self.runtimes
            .get(plugin_id)
            .map(|runtime| runtime.due_timer_ids(now))
            .unwrap_or_default()
    }

    /// Fire one timer. `None` = the plugin has no runtime (disabled/failed to
    /// load); `Some(Err(_))` = unknown timer id (vanished mid-batch — a benign
    /// skip for the pump, documented on the runtime method, no failure
    /// counted); `Some(Ok(call))` = dispatched, check `call.error`. Same
    /// quarantine contract as [`Self::run_command`]: the runtime records
    /// non-panic outcomes inside `fire_timer`; the host counts only panics —
    /// one counter, no double-counting.
    #[allow(dead_code)]
    pub fn fire_timer(
        &mut self,
        plugin_id: &str,
        id: &str,
        input: CallInput,
    ) -> Option<Result<RuntimeCall<()>, String>> {
        let runtime = self.runtimes.get_mut(plugin_id)?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.fire_timer(id, input)
        }));
        Some(match result {
            Ok(result) => result,
            Err(_) => {
                // Panic path: the runtime's own recording never ran.
                runtime.record_failure();
                log::error!("plugin '{plugin_id}' panicked while firing timer '{id}'");
                Ok(RuntimeCall {
                    output: RuntimeOutput::default(),
                    value: Some(()),
                    error: Some(format!("plugin '{plugin_id}' panicked in timer '{id}'")),
                })
            }
        })
    }

    /// True when the plugin's consecutive-failure count reached the quarantine
    /// threshold (the Task 10 pump auto-disables it). False for a plugin
    /// without a runtime.
    #[allow(dead_code)]
    pub fn quarantine_due(&self, plugin_id: &str) -> bool {
        self.runtimes
            .get(plugin_id)
            .is_some_and(|runtime| runtime.should_quarantine())
    }

    /// Clear the failure counter (the user re-enabled a quarantined plugin
    /// from the Plugins menu). No-op for a plugin without a runtime.
    #[allow(dead_code)]
    pub fn reset_failures(&mut self, plugin_id: &str) {
        if let Some(runtime) = self.runtimes.get(plugin_id) {
            runtime.reset_failures();
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
    use luban_plugin::install::{InstallRecord, InstallSource};
    use luban_plugin::manifest::PluginDependency;
    use luban_plugin::registry::PluginEntry;
    use luban_plugin::runtime::HostAction;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn entry(id: &str, dependencies: &[&str]) -> PluginEntry {
        PluginEntry {
            id: id.to_string(),
            dir: PathBuf::from(id),
            manifest: Some(PluginManifest {
                id: id.to_string(),
                name: id.to_string(),
                version: "1.0.0".to_string(),
                api_version: 2,
                main: "main.lua".to_string(),
                description: None,
                author: None,
                permissions: Vec::new(),
                events: Vec::new(),
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

    #[test]
    fn plugins_menu_only_lists_contributed_commands() {
        let mut manifest = entry("demo", &[]).manifest.unwrap();
        manifest
            .contributes
            .menus
            .push(luban_plugin::manifest::MenuContribution {
                menu: "plugins".to_string(),
                command: "demo.show".to_string(),
                label: None,
            });
        assert!(contributes_to_plugins_menu(&manifest, "demo.show"));
        assert!(!contributes_to_plugins_menu(&manifest, "demo.hidden"));
    }

    // ------------------------------------------------------------------
    // Runtime-backed tests. Unlike the struct-literal manifests above, these
    // write real plugin directories (manifest + main.lua) into a tempdir
    // plugins root and load actual `PluginRuntime`s through a test-only
    // mirror of `PluginHost::load`.
    //
    // The catch_unwind panic arms (run_command/panel_content/dispatch_event/
    // fire_timer) cannot be driven from here: mlua contains Lua errors as
    // `Result` values, so a real unwind requires a Rust-side bug (e.g. a
    // RefCell double borrow) that a healthy plugin cannot provoke. Those arms
    // are covered by review; their observable contract — panics count on the
    // same consecutive-failure counter the runtime uses — is exercised below
    // via `record_failure`, the exact call each arm makes.
    // ------------------------------------------------------------------

    /// Write one plugin directory (manifest + main.lua) into a plugins root.
    /// The manifest declares `activate`/`deactivate` (main.lua must define
    /// them) and contributes one command `test.run` backed by entry `run`.
    fn write_plugin_dir(root: &Path, id: &str, events: &[&str], main: &str) {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        let events = events
            .iter()
            .map(|event| format!(r#""{event}""#))
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(
            dir.join("plugin.json"),
            format!(
                r#"{{
                    "id": "{id}",
                    "name": "{id}",
                    "version": "1.0.0",
                    "api_version": 2,
                    "main": "main.lua",
                    "permissions": [],
                    "events": [{events}],
                    "activate": "activate",
                    "deactivate": "deactivate",
                    "contributes": {{
                        "commands": [{{ "id": "test.run", "label": "Run", "entry": "run" }}]
                    }}
                }}"#
            ),
        )
        .unwrap();
        fs::write(dir.join("main.lua"), main).unwrap();
    }

    /// Approve every discovered plugin so `registry.plan` marks them as
    /// loadable (fresh install records carrying the manifest permission hash).
    fn approved_records(root: &Path) -> InstallRecords {
        let registry = PluginRegistry::discover(root);
        let mut records = InstallRecords::default();
        for (id, entry) in &registry.entries {
            let Some(manifest) = entry.manifest.as_ref() else {
                continue;
            };
            records.plugins.insert(
                id.clone(),
                InstallRecord {
                    id: id.clone(),
                    version: manifest.version.clone(),
                    source: InstallSource::Local {
                        path: root.join(id),
                    },
                    installed_at: 0,
                    approved_permissions_hash: Some(manifest.permissions().approval_hash()),
                },
            );
        }
        records
    }

    /// Build a host from a real plugins root: discover, plan, create and
    /// activate every runtime — a test-only mirror of [`PluginHost::load`]
    /// without the UI contributions (commands/status/panels), which these
    /// tests do not need.
    fn host_from(root: &Path) -> PluginHost {
        let records = approved_records(root);
        let registry = PluginRegistry::discover(root);
        let mut plan = registry.plan(true, &BTreeSet::new(), &records);
        let mut runtimes = BTreeMap::new();
        let mut errors = Vec::new();
        let mut settings = BTreeMap::new();
        let mut schemas = BTreeMap::new();
        for id in plan.order.clone() {
            let Some(entry) = registry.get(&id) else {
                continue;
            };
            let Some(manifest) = &entry.manifest else {
                continue;
            };
            let plugin_settings =
                luban_plugin::settings::normalize(&manifest.contributes.settings, None);
            let input = CallInput {
                plugin: PluginIdentity {
                    id: id.clone(),
                    version: manifest.version.clone(),
                    data_dir: entry.dir.join("data"),
                },
                document: None,
                settings: plugin_settings.clone(),
                allowed_roots: vec![entry.dir.join("data")],
                command_depth: 0,
                context_kind: CallContextKind::default(),
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
            if let Some(error) = &activation.error {
                errors.push(error.clone());
                plan.states
                    .insert(id.clone(), PluginState::LoadError(error.clone()));
                continue;
            }
            schemas.insert(id.clone(), manifest.contributes.settings.clone());
            settings.insert(id.clone(), plugin_settings);
            runtimes.insert(id.clone(), runtime);
        }
        PluginHost {
            root: root.to_path_buf(),
            records,
            registry,
            plan,
            commands: Vec::new(),
            status_items: Vec::new(),
            panels: Vec::new(),
            settings,
            schemas,
            errors,
            dynamic_status: BTreeMap::new(),
            runtimes,
        }
    }

    /// A host-side `CallInput` mirroring what the app passes (see
    /// `plugin_ui::run_plugin_command`).
    fn host_input(host: &PluginHost, plugin_id: &str) -> CallInput {
        let manifest = host.manifest(plugin_id).expect("test plugin manifest");
        CallInput {
            plugin: PluginIdentity {
                id: plugin_id.to_string(),
                version: manifest.version.clone(),
                data_dir: host.root.join(plugin_id).join("data"),
            },
            document: None,
            settings: host.settings_for(plugin_id),
            allowed_roots: vec![host.root.join(plugin_id).join("data")],
            command_depth: 0,
            context_kind: CallContextKind::default(),
        }
    }

    #[test]
    fn listeners_of_respects_manifest_and_runtime() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin_dir(
            dir.path(),
            "alpha",
            &["on_edit"],
            r#"return { activate = function(ctx) end, run = function(ctx) end }"#,
        );
        // beta declares the event but its main.lua does not even parse, so the
        // runtime never loads and it must not receive the event.
        write_plugin_dir(dir.path(), "beta", &["on_edit"], "return {");
        write_plugin_dir(
            dir.path(),
            "gamma",
            &[],
            r#"return { activate = function(ctx) end, run = function(ctx) end }"#,
        );
        let host = host_from(dir.path());
        assert!(!host.runtimes.contains_key("beta"));
        assert!(matches!(
            host.state("beta"),
            Some(PluginState::LoadError(_))
        ));
        // Only alpha: declares the event AND has a loaded runtime.
        assert_eq!(host.listeners_of("on_edit"), ["alpha".to_string()]);
        assert!(host.listeners_of("on_document_save").is_empty());
    }

    #[test]
    fn quarantine_surfaces_from_runtime() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin_dir(
            dir.path(),
            "flaky",
            &[],
            r#"return { activate = function(ctx) end, run = function(ctx) error("boom") end }"#,
        );
        let mut host = host_from(dir.path());
        let input = host_input(&host, "flaky");
        for _ in 0..9 {
            let call = host
                .run_command("flaky", "run", "test.run", None, input.clone())
                .expect("runtime loaded");
            assert!(call.error.is_some(), "expected a failing command");
        }
        // The runtime recorded every failing call; one step below threshold.
        assert_eq!(host.runtimes["flaky"].consecutive_failures(), 9);
        assert!(!host.quarantine_due("flaky"));
        // The host panic arms record on the SAME counter — this is the exact
        // call each catch_unwind arm makes — so ten host-visible failures
        // quarantine the plugin no matter which path they came from.
        host.runtimes["flaky"].record_failure();
        assert!(host.quarantine_due("flaky"));
        // Re-enabling a quarantined plugin clears the counter.
        host.reset_failures("flaky");
        assert!(!host.quarantine_due("flaky"));
    }

    #[test]
    fn run_command_for_unknown_plugin_is_none() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin_dir(
            dir.path(),
            "quiet",
            &[],
            r#"return { activate = function(ctx) end, run = function(ctx) end }"#,
        );
        let mut host = host_from(dir.path());
        let input = host_input(&host, "quiet");
        // Unknown plugin id: None, no panic, and no failure recorded anywhere.
        assert!(host
            .run_command("ghost", "run", "test.run", None, input.clone())
            .is_none());
        assert_eq!(host.runtimes["quiet"].consecutive_failures(), 0);
        assert!(!host.quarantine_due("ghost"));
    }

    #[test]
    fn due_timer_ids_and_fire_timer_forward() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin_dir(
            dir.path(),
            "timer",
            &[],
            r#"local M = {}
            function M.activate(ctx)
                luban.timer.after("t1", 10000, function(ctx) M.fired = (M.fired or 0) + 1 end)
            end
            function M.run(ctx) luban.notify(tostring(M.fired or 0)) end
            return M"#,
        );
        let mut host = host_from(dir.path());
        let input = host_input(&host, "timer");
        let now = Instant::now();
        // The activation registered the one-shot timer "t1" with a 10 s delay:
        // far beyond any activation→now skew, so the not-due assertion cannot
        // race the clock.
        assert!(host.due_timer_ids("timer", now).is_empty());
        assert_eq!(
            host.due_timer_ids("timer", now + Duration::from_millis(10_000)),
            ["t1".to_string()]
        );
        // A plugin without a runtime reports no timers and dispatches nothing.
        assert!(host.due_timer_ids("ghost", now).is_empty());
        assert!(host.fire_timer("ghost", "t1", input.clone()).is_none());

        let call = host
            .fire_timer("timer", "t1", input.clone())
            .expect("runtime loaded")
            .expect("timer 't1' was due");
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        // The callback ran: the module-local counter moved 0 → 1.
        let run = host
            .run_command("timer", "run", "test.run", None, input.clone())
            .unwrap();
        assert!(run.is_ok(), "{}", run.error.unwrap_or_default());
        assert_eq!(run.output.actions, vec![HostAction::Notify("1".into())]);

        // The one-shot timer is gone after firing. An unknown id is a benign
        // Err for the caller (no failure recorded), not a panic.
        assert!(host
            .due_timer_ids("timer", now + Duration::from_millis(11_000))
            .is_empty());
        assert!(host
            .fire_timer("timer", "t1", input.clone())
            .unwrap()
            .is_err());
        assert!(host.fire_timer("timer", "ghost", input).unwrap().is_err());
        assert_eq!(host.runtimes["timer"].consecutive_failures(), 0);
    }

    #[test]
    fn host_run_command_passes_args_to_entry() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin_dir(
            dir.path(),
            "argsy",
            &[],
            r#"return { activate = function(ctx) end, run = function(ctx, args)
                luban.notify(tostring(args and args.n or "nil"))
            end }"#,
        );
        let mut host = host_from(dir.path());
        let input = host_input(&host, "argsy");
        // Host-level parity with the runtime tests: the args payload the app
        // executor hands to run_command reaches the entry as its second
        // parameter, and `None` reaches it as nil.
        let call = host
            .run_command(
                "argsy",
                "run",
                "test.run",
                Some(serde_json::json!({ "n": 7 })),
                input.clone(),
            )
            .expect("runtime loaded");
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        assert_eq!(call.output.actions, vec![HostAction::Notify("7".into())]);
        let call = host
            .run_command("argsy", "run", "test.run", None, input)
            .expect("runtime loaded");
        assert!(call.is_ok(), "{}", call.error.unwrap_or_default());
        assert_eq!(call.output.actions, vec![HostAction::Notify("nil".into())]);
    }

    #[test]
    fn run_command_route_rejects_depth_and_knows_builtins() {
        // The depth of the command being launched is capped: the first chain
        // step past MAX_COMMAND_DEPTH is rejected before any host lookup.
        assert_eq!(
            route_run_command("anything", luban_plugin::runtime::MAX_COMMAND_DEPTH + 1),
            RunCommandRoute::DepthExceeded
        );
        assert_eq!(
            route_run_command("anything", u8::MAX),
            RunCommandRoute::DepthExceeded
        );
        // A launch at the cap itself is still allowed; the runtime enforces
        // the same boundary for its own `command.run`.
        assert_eq!(
            route_run_command("ghost.command", luban_plugin::runtime::MAX_COMMAND_DEPTH),
            RunCommandRoute::HostCommand
        );
        // Built-ins route before the host, plugin commands and unknown ids
        // alike resolve there.
        assert_eq!(route_run_command("file.save", 0), RunCommandRoute::BuiltIn);
        assert_eq!(
            route_run_command("unknown.command", 0),
            RunCommandRoute::HostCommand
        );
    }
}

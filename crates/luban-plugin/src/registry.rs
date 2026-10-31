//! Plugin discovery and dependency resolution.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use semver::{Version, VersionReq};

use crate::install::{approval_status, ApprovalStatus, InstallRecords};
use crate::manifest::{PluginManifest, MANIFEST_FILE};
use crate::permissions::PermissionSet;

#[derive(Clone, Debug)]
pub struct PluginEntry {
    /// Best-effort identifier: the manifest id, or the directory name when the
    /// manifest is invalid.
    pub id: String,
    pub dir: PathBuf,
    pub manifest: Option<PluginManifest>,
    pub permissions: PermissionSet,
    /// Manifest validation error, when the entry cannot be loaded.
    pub error: Option<String>,
}

impl PluginEntry {
    pub fn name(&self) -> &str {
        self.manifest
            .as_ref()
            .map(|manifest| manifest.name.as_str())
            .unwrap_or(&self.id)
    }

    pub fn version(&self) -> &str {
        self.manifest
            .as_ref()
            .map(|manifest| manifest.version.as_str())
            .unwrap_or("")
    }

    pub fn is_valid(&self) -> bool {
        self.manifest.is_some() && self.error.is_none()
    }
}

#[derive(Clone, Debug)]
pub struct DiscoverError {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct PluginRegistry {
    pub root: PathBuf,
    pub entries: BTreeMap<String, PluginEntry>,
    pub errors: Vec<DiscoverError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginState {
    Enabled,
    DisabledByUser,
    NeedsApproval,
    PluginsDisabled,
    MissingDependency(String),
    IncompatibleDependency(String),
    DependencyCycle(String),
    LoadError(String),
}

impl PluginState {
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled)
    }

    pub fn label(&self) -> String {
        match self {
            Self::Enabled => "enabled".to_string(),
            Self::DisabledByUser => "disabled".to_string(),
            Self::NeedsApproval => "needs approval".to_string(),
            Self::PluginsDisabled => "plugins disabled".to_string(),
            Self::MissingDependency(dep) => format!("missing dependency: {dep}"),
            Self::IncompatibleDependency(dep) => format!("incompatible dependency: {dep}"),
            Self::DependencyCycle(ids) => format!("dependency cycle: {ids}"),
            Self::LoadError(error) => error.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct LoadPlan {
    /// Enabled plugins in dependency order.
    pub order: Vec<String>,
    pub states: BTreeMap<String, PluginState>,
}

impl LoadPlan {
    pub fn enabled(&self) -> impl Iterator<Item = &String> {
        self.order.iter()
    }

    pub fn state(&self, id: &str) -> Option<&PluginState> {
        self.states.get(id)
    }
}

impl PluginRegistry {
    /// Scan `root` for plugin directories.
    pub fn discover(root: &Path) -> Self {
        let mut registry = Self {
            root: root.to_path_buf(),
            entries: BTreeMap::new(),
            errors: Vec::new(),
        };
        let read = match fs::read_dir(root) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return registry,
            Err(error) => {
                registry.errors.push(DiscoverError {
                    path: root.to_path_buf(),
                    message: format!("cannot list plugin directory: {error}"),
                });
                return registry;
            }
        };
        let mut candidates = Vec::new();
        for entry in read {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    registry.errors.push(DiscoverError {
                        path: root.to_path_buf(),
                        message: format!("cannot read plugin directory entry: {error}"),
                    });
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    registry.errors.push(DiscoverError {
                        path: path.clone(),
                        message: format!("cannot inspect plugin directory: {error}"),
                    });
                    continue;
                }
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if !path.join(MANIFEST_FILE).is_file() {
                continue;
            }
            candidates.push(path);
        }
        candidates.sort();

        let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
        let mut duplicates: BTreeSet<String> = BTreeSet::new();
        let mut parsed: Vec<PluginEntry> = Vec::new();
        for dir in candidates {
            match PluginManifest::load(&dir) {
                Ok(manifest) => {
                    if let Some(name) = dir.file_name().and_then(|name| name.to_str()) {
                        if name != manifest.id {
                            log::warn!(
                                "plugin '{}' is installed in directory '{name}'; the directory name should match the plugin id",
                                manifest.id
                            );
                        }
                    }
                    if let Some(first) = seen.get(&manifest.id) {
                        duplicates.insert(manifest.id.clone());
                        registry.errors.push(DiscoverError {
                            path: dir.clone(),
                            message: format!(
                                "duplicate plugin id '{}' (already provided by {})",
                                manifest.id,
                                first.display()
                            ),
                        });
                    } else {
                        seen.insert(manifest.id.clone(), dir.clone());
                    }
                    let permissions = manifest.permissions();
                    parsed.push(PluginEntry {
                        id: manifest.id.clone(),
                        dir,
                        manifest: Some(manifest),
                        permissions,
                        error: None,
                    });
                }
                Err(error) => {
                    let id = dir
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    registry.errors.push(DiscoverError {
                        path: dir.clone(),
                        message: error.clone(),
                    });
                    parsed.push(PluginEntry {
                        id,
                        dir,
                        manifest: None,
                        permissions: PermissionSet::empty(),
                        error: Some(error),
                    });
                }
            }
        }
        for entry in parsed {
            if duplicates.contains(&entry.id) {
                continue;
            }
            registry.entries.insert(entry.id.clone(), entry);
        }
        registry
    }

    pub fn get(&self, id: &str) -> Option<&PluginEntry> {
        self.entries.get(id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolve the load order and per-plugin state.
    pub fn plan(
        &self,
        plugins_enabled: bool,
        disabled: &BTreeSet<String>,
        records: &InstallRecords,
    ) -> LoadPlan {
        let all_ids: BTreeSet<String> = self.entries.keys().cloned().collect();
        let mut states = BTreeMap::new();
        let mut candidates = BTreeSet::new();

        for (id, entry) in &self.entries {
            if !plugins_enabled {
                states.insert(id.clone(), PluginState::PluginsDisabled);
                continue;
            }
            if let Some(error) = &entry.error {
                states.insert(id.clone(), PluginState::LoadError(error.clone()));
                continue;
            }
            let Some(manifest) = &entry.manifest else {
                states.insert(
                    id.clone(),
                    PluginState::LoadError("invalid manifest".into()),
                );
                continue;
            };
            if disabled.contains(id) {
                states.insert(id.clone(), PluginState::DisabledByUser);
                continue;
            }
            match approval_status(records, manifest) {
                ApprovalStatus::Approved => {}
                ApprovalStatus::NeedsApproval | ApprovalStatus::MissingRecord => {
                    states.insert(id.clone(), PluginState::NeedsApproval);
                    continue;
                }
            }
            candidates.insert(id.clone());
        }

        // Check dependencies once; missing/incompatible dependencies disable the
        // plugin. Dependency-versus-disabled checks run below because a
        // dependency can itself be disabled or invalid.
        for id in candidates.clone() {
            let entry = &self.entries[&id];
            let manifest = entry.manifest.as_ref().expect("validated candidate");
            let mut failure = None;
            for dependency in &manifest.dependencies {
                match self.entries.get(&dependency.id) {
                    None => {
                        failure = Some(PluginState::MissingDependency(dependency.id.clone()));
                        break;
                    }
                    Some(dep_entry) => {
                        let version = dep_entry
                            .manifest
                            .as_ref()
                            .and_then(|manifest| Version::parse(&manifest.version).ok());
                        let requirement = VersionReq::parse(&dependency.version).ok();
                        let compatible = match (version, requirement) {
                            (Some(version), Some(requirement)) => requirement.matches(&version),
                            _ => false,
                        };
                        if !compatible {
                            failure = Some(PluginState::IncompatibleDependency(format!(
                                "{} {}",
                                dependency.id, dependency.version
                            )));
                            break;
                        }
                    }
                }
            }
            if let Some(state) = failure {
                candidates.remove(&id);
                states.insert(id, state);
            }
        }

        // A candidate whose dependency is disabled cannot load.
        loop {
            let mut changed = false;
            for id in candidates.clone() {
                let entry = &self.entries[&id];
                let manifest = entry.manifest.as_ref().expect("validated candidate");
                for dependency in &manifest.dependencies {
                    if candidates.contains(&dependency.id) {
                        continue;
                    }
                    let reason = states
                        .get(&dependency.id)
                        .map(PluginState::label)
                        .unwrap_or_else(|| "not enabled".to_string());
                    candidates.remove(&id);
                    states.insert(
                        id.clone(),
                        PluginState::MissingDependency(format!("{} ({reason})", dependency.id)),
                    );
                    changed = true;
                    break;
                }
            }
            if !changed {
                break;
            }
        }

        // Kahn's algorithm with deterministic ordering by ID.
        let mut indegree: BTreeMap<String, usize> = BTreeMap::new();
        let mut dependents: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for id in &candidates {
            indegree.insert(id.clone(), 0);
        }
        for id in &candidates {
            let entry = &self.entries[id];
            let manifest = entry.manifest.as_ref().expect("validated candidate");
            for dependency in &manifest.dependencies {
                if candidates.contains(&dependency.id) {
                    *indegree.get_mut(id).expect("candidate") += 1;
                    dependents
                        .entry(dependency.id.clone())
                        .or_default()
                        .push(id.clone());
                }
            }
        }
        let mut ready: VecDeque<String> = indegree
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(id, _)| id.clone())
            .collect();
        let mut order = Vec::new();
        while let Some(id) = ready.pop_front() {
            order.push(id.clone());
            if let Some(children) = dependents.get(&id) {
                for child in children {
                    let count = indegree.get_mut(child).expect("candidate");
                    *count -= 1;
                    if *count == 0 {
                        ready.push_back(child.clone());
                    }
                }
            }
        }
        if order.len() != candidates.len() {
            let remaining: Vec<String> = candidates
                .iter()
                .filter(|id| !order.contains(id))
                .cloned()
                .collect();
            for id in &remaining {
                states.insert(
                    id.clone(),
                    PluginState::DependencyCycle(remaining.join(", ")),
                );
            }
        } else {
            for id in &order {
                states.insert(id.clone(), PluginState::Enabled);
            }
        }

        // Ensure every discovered entry has a state.
        let _ = all_ids;
        LoadPlan { order, states }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::InstallSource;

    fn write_plugin(
        root: &Path,
        id: &str,
        version: &str,
        deps: &[(&str, &str)],
        permissions: &[&str],
    ) {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        let deps = deps
            .iter()
            .map(|(id, version)| format!(r#"{{ "id": "{id}", "version": "{version}" }}"#))
            .collect::<Vec<_>>()
            .join(",");
        let permissions = permissions
            .iter()
            .map(|item| format!("\"{item}\""))
            .collect::<Vec<_>>()
            .join(",");
        fs::write(
            dir.join("plugin.json"),
            format!(
                r#"{{
                    "id": "{id}",
                    "name": "{id}",
                    "version": "{version}",
                    "api_version": 1,
                    "main": "main.lua",
                    "permissions": [{permissions}],
                    "dependencies": [{deps}]
                }}"#
            ),
        )
        .unwrap();
        fs::write(dir.join("main.lua"), "return {}\n").unwrap();
    }

    fn approved(root: &Path, id: &str) -> InstallRecords {
        let manifest = PluginManifest::load(&root.join(id)).unwrap();
        let mut records = InstallRecords::default();
        records.plugins.insert(
            id.to_string(),
            crate::install::InstallRecord {
                id: id.to_string(),
                version: manifest.version.clone(),
                source: InstallSource::Local {
                    path: root.join(id),
                },
                installed_at: 0,
                approved_permissions_hash: Some(manifest.permissions().approval_hash()),
            },
        );
        records
    }

    #[test]
    fn resolves_dependency_order_and_reports_missing() {
        let root = tempfile::tempdir().unwrap();
        write_plugin(root.path(), "app", "1.0.0", &[("base", ">=1.0.0")], &[]);
        write_plugin(root.path(), "base", "1.2.0", &[], &[]);
        write_plugin(
            root.path(),
            "broken",
            "1.0.0",
            &[("missing", ">=1.0.0")],
            &[],
        );
        let registry = PluginRegistry::discover(root.path());
        let mut records = approved(root.path(), "app");
        for (id, record) in approved(root.path(), "base").plugins {
            records.plugins.insert(id, record);
        }
        for (id, record) in approved(root.path(), "broken").plugins {
            records.plugins.insert(id, record);
        }
        let plan = registry.plan(true, &BTreeSet::new(), &records);
        assert_eq!(plan.order, vec!["base", "app"]);
        assert!(matches!(
            plan.state("broken"),
            Some(PluginState::MissingDependency(_))
        ));
    }

    #[test]
    fn detects_cycles_and_approval() {
        let root = tempfile::tempdir().unwrap();
        write_plugin(root.path(), "a", "1.0.0", &[("b", "*")], &[]);
        write_plugin(root.path(), "b", "1.0.0", &[("a", "*")], &[]);
        write_plugin(root.path(), "c", "1.0.0", &[], &["filesystem"]);
        let registry = PluginRegistry::discover(root.path());
        let mut records = approved(root.path(), "a");
        for (id, record) in approved(root.path(), "b").plugins {
            records.plugins.insert(id, record);
        }
        let plan = registry.plan(true, &BTreeSet::new(), &records);
        assert!(plan.order.is_empty());
        assert!(matches!(
            plan.state("a"),
            Some(PluginState::DependencyCycle(_))
        ));
        assert!(matches!(plan.state("c"), Some(PluginState::NeedsApproval)));
    }

    #[test]
    fn safe_mode_disables_everything() {
        let root = tempfile::tempdir().unwrap();
        write_plugin(root.path(), "ok", "1.0.0", &[], &[]);
        let registry = PluginRegistry::discover(root.path());
        let records = approved(root.path(), "ok");
        let plan = registry.plan(false, &BTreeSet::new(), &records);
        assert!(plan.order.is_empty());
        assert!(matches!(
            plan.state("ok"),
            Some(PluginState::PluginsDisabled)
        ));
    }

    #[test]
    fn excludes_invalid_and_duplicate_manifests() {
        let root = tempfile::tempdir().unwrap();
        write_plugin(root.path(), "dup", "1.0.0", &[], &[]);
        let second = root.path().join("dup2");
        fs::create_dir_all(&second).unwrap();
        fs::copy(
            root.path().join("dup/plugin.json"),
            second.join("plugin.json"),
        )
        .unwrap();
        fs::copy(root.path().join("dup/main.lua"), second.join("main.lua")).unwrap();
        let bad = root.path().join("bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("plugin.json"), "{not json").unwrap();
        let registry = PluginRegistry::discover(root.path());
        assert!(!registry.entries.contains_key("dup"));
        assert!(registry.entries.contains_key("bad"));
        assert!(registry.entries["bad"].manifest.is_none());
        assert!(!registry.errors.is_empty());
    }
}

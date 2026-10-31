//! Installing, updating and uninstalling plugins.
//!
//! Installation never executes plugin code. A plugin is validated in a
//! temporary directory and only then atomically moved into place. Zip archives
//! are checked for path traversal and symlinks before extraction.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::manifest::{PluginManifest, MANIFEST_FILE};
use crate::permissions::PermissionSet;

pub const RECORDS_FILE: &str = ".installed.json";
pub const RECORDS_SCHEMA: u32 = 1;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
const MAX_FILES: usize = 2_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InstallSource {
    Local {
        path: PathBuf,
    },
    Zip {
        path: PathBuf,
    },
    Git {
        url: String,
        #[serde(default)]
        reference: Option<String>,
    },
}

impl InstallSource {
    pub fn label(&self) -> String {
        match self {
            Self::Local { path } => format!("local:{}", path.display()),
            Self::Zip { path } => format!("zip:{}", path.display()),
            Self::Git { url, reference } => match reference {
                Some(reference) => format!("git:{url}#{reference}"),
                None => format!("git:{url}"),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstallRecord {
    pub id: String,
    pub version: String,
    pub source: InstallSource,
    pub installed_at: u64,
    pub approved_permissions_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstallRecords {
    #[serde(default = "default_schema")]
    pub schema: u32,
    #[serde(default)]
    pub plugins: BTreeMap<String, InstallRecord>,
}

fn default_schema() -> u32 {
    RECORDS_SCHEMA
}

impl Default for InstallRecords {
    fn default() -> Self {
        Self {
            schema: RECORDS_SCHEMA,
            plugins: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalStatus {
    Approved,
    NeedsApproval,
    MissingRecord,
}

impl ApprovalStatus {
    pub fn is_approved(&self) -> bool {
        matches!(self, Self::Approved)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::NeedsApproval => "needs approval",
            Self::MissingRecord => "not installed through the manager",
        }
    }
}

pub fn records_path(root: &Path) -> PathBuf {
    root.join(RECORDS_FILE)
}

pub fn load_records(root: &Path) -> Result<InstallRecords, String> {
    let path = records_path(root);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(InstallRecords::default())
        }
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let records: InstallRecords = serde_json::from_str(&text)
        .map_err(|error| format!("invalid {}: {error}", path.display()))?;
    if records.schema != RECORDS_SCHEMA {
        return Err(format!(
            "unsupported install record schema {} (expected {RECORDS_SCHEMA})",
            records.schema
        ));
    }
    Ok(records)
}

pub fn save_records(root: &Path, records: &InstallRecords) -> Result<(), String> {
    fs::create_dir_all(root)
        .map_err(|error| format!("cannot create {}: {error}", root.display()))?;
    let json = serde_json::to_string_pretty(records)
        .map_err(|error| format!("cannot serialize install records: {error}"))?;
    luban_core::atomic_write::write_atomic(&records_path(root), json.as_bytes())
        .map_err(|error| format!("cannot save install records: {error}"))
}

/// Approval status for one manifest against the install records.
pub fn approval_status(records: &InstallRecords, manifest: &PluginManifest) -> ApprovalStatus {
    let Some(record) = records.plugins.get(&manifest.id) else {
        return ApprovalStatus::MissingRecord;
    };
    match &record.approved_permissions_hash {
        Some(hash) if hash == &manifest.permissions().approval_hash() => ApprovalStatus::Approved,
        _ => ApprovalStatus::NeedsApproval,
    }
}

/// Record the user's approval of a plugin's current permissions.
pub fn approve(root: &Path, manifest: &PluginManifest) -> Result<InstallRecord, String> {
    let mut records = load_records(root)?;
    let hash = manifest.permissions().approval_hash();
    let plugin_dir = root.join(&manifest.id);
    let entry = records
        .plugins
        .entry(manifest.id.clone())
        .or_insert_with(|| InstallRecord {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            source: InstallSource::Local { path: plugin_dir },
            installed_at: now_secs(),
            approved_permissions_hash: None,
        });
    entry.version = manifest.version.clone();
    entry.approved_permissions_hash = Some(hash);
    let record = entry.clone();
    save_records(root, &records)?;
    Ok(record)
}

/// Prefix of the error returned when an install would replace an existing
/// plugin. The UI strips it to ask for explicit confirmation.
pub const REPLACE_CONFIRMATION: &str = "plugin-already-installed:";

/// Cached source archives, kept outside plugin directories so updates still
/// work after the user deletes or moves the original zip file.
const SOURCES_DIR: &str = ".sources";

fn zip_stash_path(root: &Path, id: &str) -> PathBuf {
    root.join(SOURCES_DIR).join(format!("{id}.zip"))
}

fn stash_zip(root: &Path, id: &str, zip_path: &Path) -> Result<(), String> {
    let target = zip_stash_path(root, id);
    let parent = target
        .parent()
        .ok_or_else(|| format!("invalid cache path {}", target.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let temp = parent.join(format!(".{id}.zip.tmp-{}", now_nanos()));
    fs::copy(zip_path, &temp).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!(
            "cannot cache {} at {}: {error}",
            zip_path.display(),
            target.display()
        )
    })?;
    if target.exists() {
        fs::remove_file(&target)
            .map_err(|error| format!("cannot replace {}: {error}", target.display()))?;
    }
    fs::rename(&temp, &target).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("cannot store {}: {error}", target.display())
    })
}

/// Install a plugin from a local directory.
pub fn install_local(root: &Path, source_dir: &Path) -> Result<InstallRecord, String> {
    install_local_with(root, source_dir, false)
}

/// Install a plugin from a local directory, optionally replacing an existing
/// installation of the same id.
pub fn install_local_with(
    root: &Path,
    source_dir: &Path,
    replace: bool,
) -> Result<InstallRecord, String> {
    install_from_dir(
        root,
        source_dir,
        InstallSource::Local {
            path: absolute(source_dir)?,
        },
        None,
        replace,
    )
}

/// Install a plugin from a zip archive.
pub fn install_zip(root: &Path, zip_path: &Path) -> Result<InstallRecord, String> {
    install_zip_with(root, zip_path, false)
}

/// Install a plugin from a zip archive, optionally replacing an existing
/// installation of the same id.
pub fn install_zip_with(
    root: &Path,
    zip_path: &Path,
    replace: bool,
) -> Result<InstallRecord, String> {
    let temp = temp_dir(root, "zip")?;
    let result = extract_zip(zip_path, &temp);
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&temp);
        return Err(error);
    }
    let plugin_dir = match find_plugin_root(&temp) {
        Ok(dir) => dir,
        Err(error) => {
            let _ = fs::remove_dir_all(&temp);
            return Err(error);
        }
    };
    let source = InstallSource::Zip {
        path: absolute(zip_path)?,
    };
    let record = install_from_dir(root, &plugin_dir, source, None, replace);
    let _ = fs::remove_dir_all(&temp);
    if let Ok(record) = &record {
        if let Err(error) = stash_zip(root, &record.id, zip_path) {
            log::warn!(
                "cannot keep a cached copy of {}: {error}",
                zip_path.display()
            );
        }
    }
    record
}

/// Install a plugin by cloning a Git repository.
pub fn install_git(
    root: &Path,
    url: &str,
    reference: Option<&str>,
) -> Result<InstallRecord, String> {
    install_git_with(root, url, reference, false)
}

/// Install a plugin by cloning a Git repository, optionally replacing an
/// existing installation of the same id.
pub fn install_git_with(
    root: &Path,
    url: &str,
    reference: Option<&str>,
    replace: bool,
) -> Result<InstallRecord, String> {
    install_git_from(root, url, reference, None, replace)
}

/// Reinstall a plugin from its recorded source.
pub fn update(root: &Path, id: &str) -> Result<InstallRecord, String> {
    let records = load_records(root)?;
    let record = records
        .plugins
        .get(id)
        .ok_or_else(|| format!("plugin '{id}' has no install record"))?
        .clone();
    let previous = record.approved_permissions_hash.clone();
    match &record.source {
        InstallSource::Local { path } => {
            if !path.is_dir() {
                return Err(format!(
                    "local source {} no longer exists; install again from a directory",
                    path.display()
                ));
            }
            install_from_dir(root, path, record.source.clone(), previous, true)
        }
        InstallSource::Zip { path } => {
            let cached = zip_stash_path(root, id);
            let source_path = if path.is_file() {
                path.clone()
            } else if cached.is_file() {
                cached
            } else {
                return Err(format!(
                    "zip source {} no longer exists and no cached copy is available; install again from a zip file",
                    path.display()
                ));
            };
            let temp = temp_dir(root, "zip")?;
            let result = extract_zip(&source_path, &temp).and_then(|()| {
                let plugin_dir = find_plugin_root(&temp)?;
                install_from_dir(root, &plugin_dir, record.source.clone(), previous, true)
            });
            let _ = fs::remove_dir_all(&temp);
            result
        }
        InstallSource::Git { url, reference } => {
            install_git_from(root, url, reference.as_deref(), previous, true)
        }
    }
}

fn install_git_from(
    root: &Path,
    url: &str,
    reference: Option<&str>,
    previous_approval: Option<String>,
    replace: bool,
) -> Result<InstallRecord, String> {
    let temp = temp_dir(root, "git")?;
    let repo = temp.join("repo");
    let mut command = Command::new("git");
    command.arg("clone").arg("--depth").arg("1");
    if let Some(reference) = reference {
        command.arg("--branch").arg(reference);
    }
    command.arg("--").arg(url).arg(&repo);
    let output = command
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_dir_all(&temp);
        return Err(format!(
            "git clone failed ({}): {}",
            output.status,
            stderr.trim()
        ));
    }
    let plugin_dir = match find_plugin_root(&repo) {
        Ok(dir) => dir,
        Err(error) => {
            let _ = fs::remove_dir_all(&temp);
            return Err(error);
        }
    };
    let source = InstallSource::Git {
        url: url.to_string(),
        reference: reference.map(str::to_string),
    };
    let record = install_from_dir(root, &plugin_dir, source, previous_approval, replace);
    let _ = fs::remove_dir_all(&temp);
    record
}

/// Uninstall a plugin, optionally removing its data directory.
///
/// Settings live in `settings.json` and are removed by the caller when
/// `remove_data` is true.
pub fn uninstall(root: &Path, id: &str, remove_data: bool) -> Result<(), String> {
    let plugin_dir = root.join(id);
    if !plugin_dir.starts_with(root) {
        return Err(format!("invalid plugin id '{id}'"));
    }
    if !plugin_dir.exists() {
        return Err(format!("plugin '{id}' is not installed"));
    }
    let data = plugin_dir.join("data");
    if !remove_data && data.is_dir() {
        let stash = root.join(".data").join(id);
        if let Some(parent) = stash.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        if stash.exists() {
            merge_copy(&data, &stash)?;
            fs::remove_dir_all(&data)
                .map_err(|error| format!("cannot remove {}: {error}", data.display()))?;
        } else {
            fs::rename(&data, &stash)
                .map_err(|error| format!("cannot preserve plugin data: {error}"))?;
        }
    }
    fs::remove_dir_all(&plugin_dir)
        .map_err(|error| format!("cannot remove {}: {error}", plugin_dir.display()))?;
    let cached = zip_stash_path(root, id);
    if cached.exists() {
        fs::remove_file(&cached)
            .map_err(|error| format!("cannot remove {}: {error}", cached.display()))?;
    }
    if remove_data {
        let stash = root.join(".data").join(id);
        if stash.exists() {
            fs::remove_dir_all(&stash)
                .map_err(|error| format!("cannot remove {}: {error}", stash.display()))?;
        }
    }
    let mut records = load_records(root)?;
    records.plugins.remove(id);
    save_records(root, &records)
}

fn install_from_dir(
    root: &Path,
    source_dir: &Path,
    source: InstallSource,
    previous_approval: Option<String>,
    replace: bool,
) -> Result<InstallRecord, String> {
    let manifest = PluginManifest::load(source_dir)?;
    validate_dependencies(root, &manifest)?;
    if !source_dir.is_dir() {
        return Err(format!("{} is not a directory", source_dir.display()));
    }
    fs::create_dir_all(root)
        .map_err(|error| format!("cannot create {}: {error}", root.display()))?;
    let temp = temp_dir(root, "install")?;
    let staged = temp.join("plugin");
    if let Err(error) = copy_plugin_tree(source_dir, &staged) {
        let _ = fs::remove_dir_all(&temp);
        return Err(error);
    }
    let staged_manifest = match PluginManifest::load(&staged) {
        Ok(manifest) => manifest,
        Err(error) => {
            let _ = fs::remove_dir_all(&temp);
            return Err(error);
        }
    };
    if staged_manifest.id != manifest.id {
        let _ = fs::remove_dir_all(&temp);
        return Err("plugin id changed while staging the installation".to_string());
    }
    if !replace && root.join(&staged_manifest.id).exists() {
        let _ = fs::remove_dir_all(&temp);
        return Err(format!("{REPLACE_CONFIRMATION}{}", staged_manifest.id));
    }
    restore_stashed_data(root, &staged_manifest.id, &staged)?;
    if let Err(error) = commit_dir(root, &staged_manifest.id, &staged) {
        let _ = fs::remove_dir_all(&temp);
        return Err(error);
    }
    let _ = fs::remove_dir_all(&temp);

    let installed_dir = root.join(&staged_manifest.id);
    let installed = PluginManifest::load(&installed_dir)?;
    let permissions = installed.permissions();
    let approved = if permissions.is_empty() {
        Some(permissions.approval_hash())
    } else {
        previous_approval.filter(|hash| hash == &permissions.approval_hash())
    };
    let mut records = load_records(root)?;
    let record = InstallRecord {
        id: installed.id.clone(),
        version: installed.version.clone(),
        source,
        installed_at: now_secs(),
        approved_permissions_hash: approved,
    };
    records.plugins.insert(installed.id.clone(), record.clone());
    save_records(root, &records)?;
    Ok(record)
}

fn validate_dependencies(root: &Path, manifest: &PluginManifest) -> Result<(), String> {
    let mut problems = Vec::new();
    for dependency in &manifest.dependencies {
        if dependency.id == manifest.id {
            problems.push("a plugin cannot depend on itself".to_string());
            continue;
        }
        let dependency_dir = root.join(&dependency.id);
        if !dependency_dir.is_dir() {
            problems.push(format!("{} is not installed", dependency.id));
            continue;
        }
        let installed = match PluginManifest::load(&dependency_dir) {
            Ok(installed) => installed,
            Err(error) => {
                problems.push(format!("{} cannot be loaded: {error}", dependency.id));
                continue;
            }
        };
        let version = Version::parse(&installed.version);
        let requirement = VersionReq::parse(&dependency.version);
        match (version, requirement) {
            (Ok(version), Ok(requirement)) if requirement.matches(&version) => {}
            (Ok(version), Ok(_)) => problems.push(format!(
                "{} requires {}, installed {}",
                dependency.id, dependency.version, version
            )),
            _ => problems.push(format!("{} has an invalid version", dependency.id)),
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "missing or incompatible dependencies: {}",
            problems.join("; ")
        ))
    }
}

fn commit_dir(root: &Path, id: &str, staged: &Path) -> Result<(), String> {
    let target = root.join(id);
    let backup = root.join(format!(".backup-{id}-{}", std::process::id()));
    if backup.exists() {
        fs::remove_dir_all(&backup)
            .map_err(|error| format!("cannot clear {}: {error}", backup.display()))?;
    }
    let had_target = target.exists();
    if had_target {
        fs::rename(&target, &backup)
            .map_err(|error| format!("cannot move old plugin aside: {error}"))?;
    }
    match fs::rename(staged, &target) {
        Ok(()) => {
            if backup.exists() {
                let _ = fs::remove_dir_all(&backup);
            }
            Ok(())
        }
        Err(error) => {
            if had_target {
                let _ = fs::rename(&backup, &target);
            }
            Err(format!("cannot install plugin: {error}"))
        }
    }
}

fn restore_stashed_data(root: &Path, id: &str, staged: &Path) -> Result<(), String> {
    let stash = root.join(".data").join(id);
    if !stash.is_dir() {
        return Ok(());
    }
    let data = staged.join("data");
    if data.exists() {
        merge_copy(&stash, &data)?;
        fs::remove_dir_all(&stash)
            .map_err(|error| format!("cannot remove {}: {error}", stash.display()))?;
    } else {
        fs::rename(&stash, &data)
            .map_err(|error| format!("cannot restore plugin data: {error}"))?;
    }
    Ok(())
}

fn copy_plugin_tree(source: &Path, target: &Path) -> Result<(), String> {
    let source = source
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", source.display()))?;
    fs::create_dir_all(target)
        .map_err(|error| format!("cannot create {}: {error}", target.display()))?;
    let mut files = 0usize;
    let mut total = 0u64;
    for entry in WalkDir::new(&source)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry.map_err(|error| format!("cannot walk plugin directory: {error}"))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(&source)
            .map_err(|_| "plugin path escaped its directory".to_string())?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let destination = target.join(relative);
        let file_type = entry.file_type();
        if file_type.is_dir() {
            fs::create_dir_all(&destination)
                .map_err(|error| format!("cannot create {}: {error}", destination.display()))?;
            continue;
        }
        let source_file = if file_type.is_symlink() {
            let resolved = path
                .canonicalize()
                .map_err(|error| format!("cannot resolve symlink {}: {error}", path.display()))?;
            if !resolved.starts_with(&source) {
                return Err(format!(
                    "plugin symlink {} escapes the plugin directory",
                    path.display()
                ));
            }
            let metadata = fs::metadata(&resolved)
                .map_err(|error| format!("cannot stat {}: {error}", resolved.display()))?;
            if !metadata.is_file() {
                return Err(format!(
                    "plugin symlink {} does not point to a regular file",
                    path.display()
                ));
            }
            resolved
        } else if file_type.is_file() {
            path.to_path_buf()
        } else {
            return Err(format!(
                "unsupported file type in plugin: {}",
                path.display()
            ));
        };
        let metadata = fs::metadata(&source_file)
            .map_err(|error| format!("cannot stat {}: {error}", source_file.display()))?;
        if metadata.len() > MAX_FILE_BYTES {
            return Err(format!(
                "plugin file {} is larger than {} bytes",
                source_file.display(),
                MAX_FILE_BYTES
            ));
        }
        total = total.saturating_add(metadata.len());
        if total > MAX_TOTAL_BYTES {
            return Err("plugin is larger than the installation limit".to_string());
        }
        files += 1;
        if files > MAX_FILES {
            return Err("plugin contains too many files".to_string());
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        fs::copy(&source_file, &destination).map_err(|error| {
            format!(
                "cannot copy {} to {}: {error}",
                source_file.display(),
                destination.display()
            )
        })?;
    }
    Ok(())
}

fn extract_zip(zip_path: &Path, target: &Path) -> Result<(), String> {
    let file = fs::File::open(zip_path)
        .map_err(|error| format!("cannot open {}: {error}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| format!("invalid zip archive {}: {error}", zip_path.display()))?;
    fs::create_dir_all(target)
        .map_err(|error| format!("cannot create {}: {error}", target.display()))?;
    let mut files = 0usize;
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("cannot read zip entry: {error}"))?;
        let name = entry.name().to_string();
        let relative = safe_zip_path(&name)?;
        if entry.is_dir() {
            let directory = target.join(&relative);
            fs::create_dir_all(&directory)
                .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
            continue;
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(format!("zip entry '{name}' is a symlink"));
        }
        if entry.size() > MAX_FILE_BYTES {
            return Err(format!(
                "zip entry '{name}' is larger than {} bytes",
                MAX_FILE_BYTES
            ));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_TOTAL_BYTES {
            return Err("zip archive is larger than the installation limit".to_string());
        }
        files += 1;
        if files > MAX_FILES {
            return Err("zip archive contains too many files".to_string());
        }
        let destination = target.join(&relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let mut output = fs::File::create(&destination)
            .map_err(|error| format!("cannot create {}: {error}", destination.display()))?;
        io::copy(&mut entry, &mut output)
            .map_err(|error| format!("cannot extract '{name}': {error}"))?;
    }
    Ok(())
}

fn safe_zip_path(name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.contains('\\') {
        return Err(format!("unsafe zip entry name '{name}'"));
    }
    let path = Path::new(name);
    if path.is_absolute() {
        return Err(format!("unsafe absolute zip entry '{name}'"));
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => clean.push(value),
            Component::CurDir => {}
            _ => return Err(format!("unsafe zip entry name '{name}'")),
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(format!("unsafe zip entry name '{name}'"));
    }
    Ok(clean)
}

fn find_plugin_root(root: &Path) -> Result<PathBuf, String> {
    if root.join(MANIFEST_FILE).is_file() {
        return Ok(root.to_path_buf());
    }
    let mut found = Vec::new();
    for entry in WalkDir::new(root)
        .min_depth(1)
        .max_depth(3)
        .follow_links(false)
    {
        let entry = entry.map_err(|error| format!("cannot walk archive: {error}"))?;
        if entry.file_name() == MANIFEST_FILE && entry.file_type().is_file() {
            found.push(entry.path().to_path_buf());
        }
    }
    match found.len() {
        0 => Err(format!("no {MANIFEST_FILE} found in the package")),
        1 => Ok(found[0]
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "invalid manifest location".to_string())?),
        _ => Err(format!(
            "multiple {MANIFEST_FILE} files found; a package must contain exactly one plugin"
        )),
    }
}

fn merge_copy(source: &Path, target: &Path) -> Result<(), String> {
    fs::create_dir_all(target)
        .map_err(|error| format!("cannot create {}: {error}", target.display()))?;
    for entry in WalkDir::new(source).follow_links(false).sort_by_file_name() {
        let entry = entry.map_err(|error| format!("cannot walk {}: {error}", source.display()))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|_| "path escaped its directory".to_string())?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let destination = target.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&destination)
                .map_err(|error| format!("cannot create {}: {error}", destination.display()))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
            }
            fs::copy(entry.path(), &destination).map_err(|error| {
                format!(
                    "cannot copy {} to {}: {error}",
                    entry.path().display(),
                    destination.display()
                )
            })?;
        }
    }
    Ok(())
}

fn temp_dir(root: &Path, kind: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(root)
        .map_err(|error| format!("cannot create {}: {error}", root.display()))?;
    let path = root.join(format!(".{kind}-{}-{}", std::process::id(), now_nanos()));
    if path.exists() {
        fs::remove_dir_all(&path)
            .map_err(|error| format!("cannot clear {}: {error}", path.display()))?;
    }
    fs::create_dir_all(&path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    Ok(path)
}

fn absolute(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| format!("cannot resolve {}: {error}", path.display()))
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

/// Parse a permission set from a manifest for callers that only have a record.
pub fn record_permissions(manifest: &PluginManifest) -> PermissionSet {
    manifest.permissions()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn write_plugin(dir: &Path, id: &str, permissions: &[&str]) {
        fs::create_dir_all(dir).unwrap();
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
                    "name": "Demo",
                    "version": "1.0.0",
                    "api_version": 1,
                    "main": "main.lua",
                    "permissions": [{permissions}],
                    "contributes": {{
                        "commands": [{{ "id": "{id}.run", "label": "Run", "entry": "run" }}]
                    }}
                }}"#
            ),
        )
        .unwrap();
        fs::write(dir.join("main.lua"), "return { run = function() end }\n").unwrap();
    }

    fn zip_plugin(path: &Path, entries: &[(&str, &str)]) {
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, data) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(data.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn installs_and_uninstalls_a_local_plugin() {
        let root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        write_plugin(source.path(), "com.example.demo", &["filesystem"]);
        let records_root = root.path().join("plugins");
        let record = install_local(&records_root, source.path()).unwrap();
        assert_eq!(record.id, "com.example.demo");
        assert_eq!(record.approved_permissions_hash, None);
        assert!(records_root.join("com.example.demo/main.lua").is_file());
        let records = load_records(&records_root).unwrap();
        let manifest = PluginManifest::load(&records_root.join("com.example.demo")).unwrap();
        assert_eq!(
            approval_status(&records, &manifest),
            ApprovalStatus::NeedsApproval
        );
        approve(&records_root, &manifest).unwrap();
        let records = load_records(&records_root).unwrap();
        assert_eq!(
            approval_status(&records, &manifest),
            ApprovalStatus::Approved
        );
        fs::create_dir_all(records_root.join("com.example.demo/data")).unwrap();
        fs::write(records_root.join("com.example.demo/data/state.txt"), "keep").unwrap();
        uninstall(&records_root, "com.example.demo", false).unwrap();
        assert!(!records_root.join("com.example.demo").exists());
        assert!(records_root
            .join(".data/com.example.demo/state.txt")
            .is_file());
        assert!(load_records(&records_root).unwrap().plugins.is_empty());
    }

    #[test]
    fn empty_permissions_are_approved_automatically() {
        let root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        write_plugin(source.path(), "com.example.free", &[]);
        let plugins = root.path().join("plugins");
        let record = install_local(&plugins, source.path()).unwrap();
        assert!(record.approved_permissions_hash.is_some());
    }

    #[test]
    fn zip_install_rejects_traversal() {
        let root = tempfile::tempdir().unwrap();
        let zip_path = root.path().join("bad.zip");
        zip_plugin(
            &zip_path,
            &[("../escape.lua", "return {}"), ("plugin.json", "{}")],
        );
        let plugins = root.path().join("plugins");
        assert!(install_zip(&plugins, &zip_path).is_err());
        assert!(!root.path().join("escape.lua").exists());
    }

    #[test]
    fn zip_install_finds_a_single_root_directory() {
        let root = tempfile::tempdir().unwrap();
        let zip_path = root.path().join("ok.zip");
        let manifest = r#"{
            "id": "com.example.zip",
            "name": "Zip",
            "version": "1.0.0",
            "api_version": 1,
            "main": "main.lua"
        }"#;
        zip_plugin(
            &zip_path,
            &[
                ("repo/plugin.json", manifest),
                ("repo/main.lua", "return {}\n"),
            ],
        );
        let plugins = root.path().join("plugins");
        let record = install_zip(&plugins, &zip_path).unwrap();
        assert_eq!(record.id, "com.example.zip");
        assert!(plugins.join("com.example.zip/main.lua").is_file());
    }

    #[test]
    fn zip_update_uses_the_cached_source() {
        let root = tempfile::tempdir().unwrap();
        let zip_path = root.path().join("cached.zip");
        let manifest = r#"{
            "id": "com.example.cached",
            "name": "Cached",
            "version": "1.0.0",
            "api_version": 1,
            "main": "main.lua"
        }"#;
        zip_plugin(
            &zip_path,
            &[("plugin.json", manifest), ("main.lua", "return {}\n")],
        );
        let plugins = root.path().join("plugins");
        install_zip(&plugins, &zip_path).unwrap();
        assert!(zip_stash_path(&plugins, "com.example.cached").is_file());

        fs::remove_file(&zip_path).unwrap();
        let record = update(&plugins, "com.example.cached").unwrap();
        assert_eq!(record.version, "1.0.0");
        assert!(plugins.join("com.example.cached/main.lua").is_file());

        uninstall(&plugins, "com.example.cached", true).unwrap();
        assert!(!zip_stash_path(&plugins, "com.example.cached").exists());
    }

    #[test]
    fn install_validates_dependencies() {
        let root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        write_plugin(source.path(), "com.example.dependent", &[]);
        let manifest = serde_json::json!({
            "id": "com.example.dependent",
            "name": "Dependent",
            "version": "1.0.0",
            "api_version": 1,
            "main": "main.lua",
            "dependencies": [{ "id": "com.example.base", "version": ">=1.0.0" }]
        });
        fs::write(
            source.path().join("plugin.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        let plugins = root.path().join("plugins");
        assert!(install_local(&plugins, source.path()).is_err());

        let base = tempfile::tempdir().unwrap();
        write_plugin(base.path(), "com.example.base", &[]);
        install_local(&plugins, base.path()).unwrap();
        install_local(&plugins, source.path()).unwrap();
    }

    #[test]
    fn install_rejects_symlinks_that_escape() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let root = tempfile::tempdir().unwrap();
            let source = tempfile::tempdir().unwrap();
            write_plugin(source.path(), "com.example.link", &[]);
            symlink("/etc/hostname", source.path().join("outside.lua")).unwrap();
            let plugins = root.path().join("plugins");
            assert!(install_local(&plugins, source.path()).is_err());
        }
    }

    #[test]
    fn install_requires_confirmation_before_replacing() {
        let root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        write_plugin(source.path(), "com.example.replace", &[]);
        let plugins = root.path().join("plugins");
        install_local(&plugins, source.path()).unwrap();

        fs::write(
            source.path().join("main.lua"),
            "return { run = function() end }\n-- v2\n",
        )
        .unwrap();
        let error = install_local(&plugins, source.path()).unwrap_err();
        assert!(error.starts_with(REPLACE_CONFIRMATION));
        assert!(
            !fs::read_to_string(plugins.join("com.example.replace/main.lua"))
                .unwrap()
                .contains("v2")
        );

        let record = install_local_with(&plugins, source.path(), true).unwrap();
        assert_eq!(record.id, "com.example.replace");
        assert!(
            fs::read_to_string(plugins.join("com.example.replace/main.lua"))
                .unwrap()
                .contains("v2")
        );
    }

    #[test]
    fn update_reinstalls_the_recorded_local_source() {
        let root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        write_plugin(source.path(), "com.example.update", &[]);
        let plugins = root.path().join("plugins");
        install_local(&plugins, source.path()).unwrap();

        let manifest = fs::read_to_string(source.path().join("plugin.json"))
            .unwrap()
            .replace("1.0.0", "1.1.0");
        fs::write(source.path().join("plugin.json"), manifest).unwrap();
        let record = update(&plugins, "com.example.update").unwrap();
        assert_eq!(record.version, "1.1.0");
        assert_eq!(
            PluginManifest::load(&plugins.join("com.example.update"))
                .unwrap()
                .version,
            "1.1.0"
        );
    }

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn git_install_and_update_use_the_recorded_source() {
        if !Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        write_plugin(repo.path(), "com.example.git", &[]);
        assert!(git(repo.path(), &["init", "-q"]));
        assert!(git(repo.path(), &["add", "."]));
        assert!(git(repo.path(), &["commit", "-qm", "initial"]));

        let url = format!("file://{}", repo.path().display());
        let plugins = root.path().join("plugins");
        let record = install_git(&plugins, &url, None).unwrap();
        assert_eq!(record.id, "com.example.git");

        let manifest = fs::read_to_string(repo.path().join("plugin.json"))
            .unwrap()
            .replace("1.0.0", "1.1.0");
        fs::write(repo.path().join("plugin.json"), manifest).unwrap();
        assert!(git(repo.path(), &["add", "."]));
        assert!(git(repo.path(), &["commit", "-qm", "bump"]));
        let updated = update(&plugins, "com.example.git").unwrap();
        assert_eq!(updated.version, "1.1.0");
    }
}

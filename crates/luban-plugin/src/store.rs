//! Plugin store catalog model and validation.
//!
//! The store catalog is plain JSON fetched over HTTPS. Everything in this
//! module is defensive: malformed entries are rejected with a precise reason,
//! a single bad plugin never prevents the remaining catalog from loading, and
//! artifact metadata is validated before any bytes are downloaded.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::manifest::is_plugin_id;

/// Catalog schema understood by this build.
pub const CATALOG_VERSION: u32 = 1;
/// Maximum accepted catalog response size.
pub const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
/// Maximum accepted size of one plugin entry.
pub const MAX_PLUGIN_ENTRY_BYTES: usize = 256 * 1024;
/// Maximum accepted artifact size.
pub const MAX_ARTIFACT_BYTES: u64 = 50 * 1024 * 1024;
/// Cache metadata schema written next to a cached catalog.
pub const CACHE_SCHEMA: u32 = 1;

/// A platform an artifact can target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Target {
    Universal,
    LinuxX64,
    LinuxArm64,
    DarwinX64,
    DarwinArm64,
    WindowsX64,
    WindowsArm64,
}

impl Target {
    /// Every accepted artifact target, including `universal`.
    pub const ALL: [Self; 7] = [
        Self::Universal,
        Self::LinuxX64,
        Self::LinuxArm64,
        Self::DarwinX64,
        Self::DarwinArm64,
        Self::WindowsX64,
        Self::WindowsArm64,
    ];

    /// Concrete platforms; `universal` is not included.
    pub const PLATFORMS: [Self; 6] = [
        Self::LinuxX64,
        Self::LinuxArm64,
        Self::DarwinX64,
        Self::DarwinArm64,
        Self::WindowsX64,
        Self::WindowsArm64,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Universal => "universal",
            Self::LinuxX64 => "linux-x64",
            Self::LinuxArm64 => "linux-arm64",
            Self::DarwinX64 => "darwin-x64",
            Self::DarwinArm64 => "darwin-arm64",
            Self::WindowsX64 => "windows-x64",
            Self::WindowsArm64 => "windows-arm64",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|target| target.as_str() == value.trim())
    }

    pub fn is_universal(self) -> bool {
        self == Self::Universal
    }

    /// Target for the running OS and CPU architecture.
    pub fn current() -> Option<Self> {
        Self::for_platform(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// Map an OS/architecture pair to a target. Unknown pairs return `None`
    /// instead of guessing a compatible package.
    pub fn for_platform(os: &str, arch: &str) -> Option<Self> {
        let arch = arch.to_ascii_lowercase();
        let arch = match arch.as_str() {
            "x86_64" | "amd64" => "x64",
            "aarch64" | "arm64" => "arm64",
            _ => return None,
        };
        match (os.to_ascii_lowercase().as_str(), arch) {
            ("linux", "x64") => Some(Self::LinuxX64),
            ("linux", "arm64") => Some(Self::LinuxArm64),
            ("macos" | "darwin", "x64") => Some(Self::DarwinX64),
            ("macos" | "darwin", "arm64") => Some(Self::DarwinArm64),
            ("windows", "x64") => Some(Self::WindowsX64),
            ("windows", "arm64") => Some(Self::WindowsArm64),
            _ => None,
        }
    }
}

/// One downloadable package for a version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub target: Target,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// Localized display text for one language tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Localization {
    pub name: String,
    pub description: Option<String>,
}

/// One published version of a plugin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreVersion {
    pub version: String,
    pub api_version: u32,
    pub released_at: Option<String>,
    pub release_notes: Option<String>,
    pub artifacts: Vec<Artifact>,
}

impl StoreVersion {
    /// Artifact for a target: an exact match wins over `universal`.
    pub fn artifact_for(&self, target: Target) -> Option<(&Artifact, bool)> {
        if let Some(artifact) = self.artifacts.iter().find(|item| item.target == target) {
            return Some((artifact, true));
        }
        if target != Target::Universal {
            if let Some(artifact) = self
                .artifacts
                .iter()
                .find(|item| item.target == Target::Universal)
            {
                return Some((artifact, false));
            }
        }
        None
    }
}

/// A plugin listed in the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorePlugin {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub publisher: String,
    pub verified: bool,
    pub icon: Option<String>,
    pub tags: Vec<String>,
    pub permissions: Vec<String>,
    pub source: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub versions: Vec<StoreVersion>,
    pub localizations: BTreeMap<String, Localization>,
}

/// Repository identity from the catalog header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryInfo {
    pub id: String,
    pub name: String,
    pub homepage: Option<String>,
}

/// Parsed and validated catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    pub repository: RepositoryInfo,
    pub generated_at: Option<String>,
    pub plugins: Vec<StorePlugin>,
}

/// A catalog plus non-fatal problems found while parsing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCatalog {
    pub catalog: Catalog,
    pub warnings: Vec<String>,
}

/// Metadata stored next to the cached catalog.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogCacheMeta {
    pub schema: u32,
    pub source: String,
    pub fetched_at: u64,
}

/// One selected version/artifact pair for the current platform.
#[derive(Clone, Copy, Debug)]
pub struct InstallCandidate<'a> {
    pub version: &'a StoreVersion,
    pub artifact: &'a Artifact,
    /// True when the artifact matches the target exactly instead of using a
    /// `universal` fallback.
    pub exact_target: bool,
}

/// Relationship between the installed version and the catalog candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    NotInstalled,
    Installed,
    UpdateAvailable { installed: String, latest: String },
}

impl Catalog {
    pub fn find(&self, id: &str) -> Option<&StorePlugin> {
        self.plugins.iter().find(|plugin| plugin.id == id)
    }

    /// Plugin ids sorted for stable presentation.
    pub fn sorted_ids(&self, locale: &str) -> Vec<String> {
        let mut plugins: Vec<&StorePlugin> = self.plugins.iter().collect();
        plugins.sort_by(|left, right| {
            let left_name = left.localized_name(locale).to_lowercase();
            let right_name = right.localized_name(locale).to_lowercase();
            left_name
                .cmp(&right_name)
                .then_with(|| left.id.cmp(&right.id))
        });
        plugins
            .into_iter()
            .map(|plugin| plugin.id.clone())
            .collect()
    }
}

impl StorePlugin {
    /// Name for the requested UI language.
    pub fn localized_name<'a>(&'a self, locale: &str) -> &'a str {
        self.localization_for(locale)
            .map(|item| item.name.as_str())
            .unwrap_or(self.name.as_str())
    }

    /// Description for the requested UI language.
    pub fn localized_description<'a>(&'a self, locale: &str) -> Option<&'a str> {
        self.localization_for(locale)
            .and_then(|item| item.description.as_deref())
            .or(self.description.as_deref())
    }

    fn localization_for(&self, locale: &str) -> Option<&Localization> {
        let locale = locale.trim();
        if locale.is_empty() {
            return None;
        }
        let locale = locale.to_ascii_lowercase();
        if let Some(item) = self
            .localizations
            .iter()
            .find(|(tag, _)| tag.eq_ignore_ascii_case(&locale))
            .map(|(_, item)| item)
        {
            return Some(item);
        }
        let primary = locale
            .split(['-', '_'])
            .next()
            .filter(|value| !value.is_empty())?;
        self.localizations
            .iter()
            .find(|(tag, _)| {
                tag.split(['-', '_'])
                    .next()
                    .is_some_and(|value| value.eq_ignore_ascii_case(primary))
            })
            .map(|(_, item)| item)
    }

    /// Highest version whose API exactly matches the host, regardless of
    /// platform. The manifest layer loads only `manifest::API_VERSION`, so
    /// entries declaring an older or newer API must never be offered.
    pub fn latest_compatible(&self, host_api: u32) -> Option<&StoreVersion> {
        self.highest_version_where(|version| version.api_version == host_api)
    }

    /// True when no published version matches the host API (kept for its
    /// existing callers; despite the name it also covers entries that are
    /// too old).
    pub fn requires_newer_api(&self, host_api: u32) -> bool {
        !self
            .versions
            .iter()
            .any(|version| version.api_version == host_api)
    }

    /// Highest API version declared across all entries, used for messages.
    /// The `unwrap_or(1)` default describes version-less legacy entries,
    /// which predate per-version API data and really are API 1.
    pub fn required_api_version(&self) -> u32 {
        self.versions
            .iter()
            .map(|version| version.api_version)
            .max()
            .unwrap_or(1)
    }

    /// Highest compatible version that has a package for `target`.
    pub fn install_candidate(&self, target: Target, host_api: u32) -> Option<InstallCandidate<'_>> {
        let mut best: Option<(Version, &StoreVersion, &Artifact, bool)> = None;
        for version in &self.versions {
            if version.api_version != host_api {
                continue;
            }
            let Some((artifact, exact)) = version.artifact_for(target) else {
                continue;
            };
            let Ok(parsed) = Version::parse(&version.version) else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|(best_version, _, _, _)| parsed > *best_version)
            {
                best = Some((parsed, version, artifact, exact));
            }
        }
        best.map(|(_, version, artifact, exact_target)| InstallCandidate {
            version,
            artifact,
            exact_target,
        })
    }

    /// Whether any published version can run on `target`.
    pub fn supports_target(&self, target: Target) -> bool {
        self.versions
            .iter()
            .any(|version| version.artifact_for(target).is_some())
    }

    /// Targets the plugin publishes, across all versions.
    pub fn supported_targets(&self) -> Vec<Target> {
        let mut targets = BTreeSet::new();
        for version in &self.versions {
            for artifact in &version.artifacts {
                targets.insert(artifact.target);
            }
        }
        targets.into_iter().collect()
    }

    /// Compare an installed version with the best installable version.
    pub fn update_status(
        &self,
        installed: Option<&str>,
        target: Target,
        host_api: u32,
    ) -> UpdateStatus {
        let Some(installed) = installed else {
            return UpdateStatus::NotInstalled;
        };
        let Some(candidate) = self.install_candidate(target, host_api) else {
            return UpdateStatus::Installed;
        };
        let Ok(installed_version) = Version::parse(installed) else {
            return UpdateStatus::Installed;
        };
        let Ok(latest) = Version::parse(&candidate.version.version) else {
            return UpdateStatus::Installed;
        };
        if latest > installed_version {
            UpdateStatus::UpdateAvailable {
                installed: installed.to_string(),
                latest: candidate.version.version.clone(),
            }
        } else {
            UpdateStatus::Installed
        }
    }

    /// Case-insensitive search over localized text, identity, publisher and
    /// tags. Every whitespace-separated term must match.
    pub fn matches_query(&self, query: &str, locale: &str) -> bool {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|term| term.to_lowercase())
            .collect();
        if terms.is_empty() {
            return true;
        }
        let mut haystack = String::new();
        for part in [
            self.localized_name(locale),
            self.localized_description(locale).unwrap_or(""),
            &self.name,
            self.description.as_deref().unwrap_or(""),
            &self.id,
            &self.publisher,
            &self.tags.join(" "),
            &self.permissions.join(" "),
        ] {
            haystack.push_str(part);
            haystack.push('\n');
        }
        let haystack = haystack.to_lowercase();
        terms.iter().all(|term| haystack.contains(term))
    }

    fn highest_version_where(
        &self,
        predicate: impl Fn(&StoreVersion) -> bool,
    ) -> Option<&StoreVersion> {
        let mut best: Option<(Version, &StoreVersion)> = None;
        for version in self.versions.iter().filter(|version| predicate(version)) {
            let Ok(parsed) = Version::parse(&version.version) else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|(best_version, _)| parsed > *best_version)
            {
                best = Some((parsed, version));
            }
        }
        best.map(|(_, version)| version)
    }
}

/// Parse and validate a catalog. Fatal problems (unsupported version, missing
/// header, oversized document) reject the whole catalog; a malformed plugin
/// entry is skipped and reported in `warnings`.
pub fn parse_catalog(bytes: &[u8]) -> Result<ParsedCatalog, String> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(format!(
            "catalog is larger than {} bytes",
            MAX_CATALOG_BYTES
        ));
    }
    let text =
        std::str::from_utf8(bytes).map_err(|error| format!("catalog is not UTF-8: {error}"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("invalid catalog JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "catalog must be a JSON object".to_string())?;

    let version = object
        .get("catalog_version")
        .ok_or_else(|| "catalog_version is required".to_string())?
        .as_u64()
        .ok_or_else(|| "catalog_version must be an integer".to_string())?;
    if version != u64::from(CATALOG_VERSION) {
        return Err(format!(
            "unsupported catalog_version {version}; this build supports {CATALOG_VERSION}"
        ));
    }

    let repository_value = object
        .get("repository")
        .ok_or_else(|| "repository is required".to_string())?;
    let repository = parse_repository(repository_value)?;

    let generated_at = match object.get("generated_at") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Null) | None => None,
        Some(_) => return Err("generated_at must be a string".to_string()),
    };

    let plugins_value = object
        .get("plugins")
        .ok_or_else(|| "plugins is required".to_string())?;
    let plugins = plugins_value
        .as_array()
        .ok_or_else(|| "plugins must be an array".to_string())?;

    let mut parsed = Vec::new();
    let mut warnings = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, entry) in plugins.iter().enumerate() {
        let id = entry
            .as_object()
            .and_then(|object| object.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        // Reject oversized entries before allocating their model.
        if serde_json::to_string(entry)
            .map(|text| text.len() > MAX_PLUGIN_ENTRY_BYTES)
            .unwrap_or(false)
        {
            warnings.push(format!(
                "plugins[{index}]: entry is larger than {MAX_PLUGIN_ENTRY_BYTES} bytes"
            ));
            continue;
        }
        match parse_plugin(entry) {
            Ok(plugin) => {
                if !seen.insert(plugin.id.clone()) {
                    warnings.push(format!(
                        "plugins[{index}]: duplicate plugin id '{}'; entry ignored",
                        plugin.id
                    ));
                    continue;
                }
                parsed.push(plugin);
            }
            Err(error) => {
                let label = id.unwrap_or_else(|| format!("plugins[{index}]"));
                warnings.push(format!("{label}: {error}"));
            }
        }
    }

    Ok(ParsedCatalog {
        catalog: Catalog {
            repository,
            generated_at,
            plugins: parsed,
        },
        warnings,
    })
}

fn parse_repository(value: &Value) -> Result<RepositoryInfo, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "repository must be an object".to_string())?;
    let id = required_str(object, "repository.id")?;
    if id.trim().is_empty() || id.chars().count() > 128 {
        return Err("repository.id must be 1-128 characters".to_string());
    }
    let name = required_str(object, "repository.name")?;
    if name.trim().is_empty() || name.chars().count() > 128 {
        return Err("repository.name must be 1-128 characters".to_string());
    }
    let homepage = optional_str(object, "repository.homepage")?;
    if let Some(homepage) = homepage {
        if !is_https_url(homepage) {
            return Err("repository.homepage must be an HTTPS URL".to_string());
        }
    }
    Ok(RepositoryInfo {
        id: id.to_string(),
        name: name.to_string(),
        homepage: homepage.map(str::to_string),
    })
}

fn parse_plugin(value: &Value) -> Result<StorePlugin, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "plugin entry must be an object".to_string())?;
    let id = required_str(object, "id")?;
    if !is_plugin_id(id) {
        return Err(format!("plugin id '{id}' must match [a-z0-9][a-z0-9._-]*"));
    }
    let name = required_str(object, "name")?;
    if name.trim().is_empty() || name.chars().count() > 128 {
        return Err("name must be 1-128 characters".to_string());
    }
    let description = optional_str(object, "description")?.map(str::to_string);
    let publisher = required_str(object, "publisher")?;
    if publisher.trim().is_empty() || publisher.chars().count() > 128 {
        return Err("publisher must be 1-128 characters".to_string());
    }
    let verified = match object.get("verified") {
        None | Some(Value::Null) => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "verified must be a boolean".to_string())?,
    };
    let icon = optional_str(object, "icon")?;
    if let Some(icon) = icon {
        if !is_https_url(icon) {
            return Err("icon must be an HTTPS URL".to_string());
        }
    }
    let tags = string_array(object, "tags")?;
    let mut unique_tags = BTreeSet::new();
    for tag in &tags {
        if tag.trim().is_empty() || tag.chars().count() > 64 {
            return Err("tags must be 1-64 characters each".to_string());
        }
        if !unique_tags.insert(tag.to_lowercase()) {
            return Err(format!("duplicate tag '{tag}'"));
        }
    }
    let permissions = string_array(object, "permissions")?;
    let mut unique_permissions = BTreeSet::new();
    for permission in &permissions {
        if permission.trim().is_empty() || permission.chars().count() > 64 {
            return Err("permissions must be 1-64 characters each".to_string());
        }
        if !unique_permissions.insert(permission.clone()) {
            return Err(format!("duplicate permission '{permission}'"));
        }
    }
    let source = optional_url(object, "source")?;
    let homepage = optional_url(object, "homepage")?;
    let license = optional_str(object, "license")?.map(str::to_string);
    if license
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err("license must not be empty".to_string());
    }

    let versions_value = object
        .get("versions")
        .ok_or_else(|| "versions is required".to_string())?;
    let versions_array = versions_value
        .as_array()
        .ok_or_else(|| "versions must be an array".to_string())?;
    if versions_array.is_empty() {
        return Err("versions must not be empty".to_string());
    }
    let mut versions = Vec::new();
    let mut seen_versions = BTreeSet::new();
    for (index, entry) in versions_array.iter().enumerate() {
        let version =
            parse_version(entry).map_err(|error| format!("versions[{index}]: {error}"))?;
        if !seen_versions.insert(version.version.clone()) {
            return Err(format!("duplicate version '{}'", version.version));
        }
        versions.push(version);
    }

    let latest = required_str(object, "latest_version")?;
    let latest_version =
        Version::parse(latest).map_err(|_| format!("latest_version '{latest}' is not SemVer"))?;
    let highest = versions
        .iter()
        .filter_map(|version| Version::parse(&version.version).ok())
        .max()
        .ok_or_else(|| "versions must contain a valid SemVer".to_string())?;
    if latest_version != highest {
        return Err(format!(
            "latest_version '{latest}' does not match the highest published version '{highest}'"
        ));
    }

    let localizations = parse_localizations(object)?;

    Ok(StorePlugin {
        id: id.to_string(),
        name: name.to_string(),
        description,
        publisher: publisher.to_string(),
        verified,
        icon: icon.map(str::to_string),
        tags,
        permissions,
        source,
        homepage,
        license,
        versions,
        localizations,
    })
}

fn parse_version(value: &Value) -> Result<StoreVersion, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "version entry must be an object".to_string())?;
    let version = required_str(object, "version")?;
    Version::parse(version).map_err(|_| format!("version '{version}' is not SemVer"))?;
    let api_version = match object.get("api_version") {
        None | Some(Value::Null) => 1,
        Some(value) => value
            .as_u64()
            .filter(|value| *value >= 1 && *value <= u64::from(u32::MAX))
            .ok_or_else(|| "api_version must be a positive integer".to_string())?
            as u32,
    };
    let released_at = optional_str(object, "released_at")?.map(str::to_string);
    let release_notes = optional_str(object, "release_notes")?.map(str::to_string);

    let artifacts_value = object
        .get("artifacts")
        .ok_or_else(|| "artifacts is required".to_string())?;
    let artifacts_array = artifacts_value
        .as_array()
        .ok_or_else(|| "artifacts must be an array".to_string())?;
    if artifacts_array.is_empty() {
        return Err("artifacts must not be empty".to_string());
    }
    let mut artifacts = Vec::new();
    let mut seen_targets = BTreeSet::new();
    for (index, entry) in artifacts_array.iter().enumerate() {
        let artifact =
            parse_artifact(entry).map_err(|error| format!("artifacts[{index}]: {error}"))?;
        if !seen_targets.insert(artifact.target) {
            return Err(format!(
                "duplicate artifact target '{}'",
                artifact.target.as_str()
            ));
        }
        artifacts.push(artifact);
    }

    Ok(StoreVersion {
        version: version.to_string(),
        api_version,
        released_at,
        release_notes,
        artifacts,
    })
}

fn parse_artifact(value: &Value) -> Result<Artifact, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "artifact must be an object".to_string())?;
    let target_text = required_str(object, "target")?;
    let target = Target::parse(target_text)
        .ok_or_else(|| format!("unknown artifact target '{target_text}'"))?;
    let url = required_str(object, "url")?;
    if !is_https_url(url) {
        return Err("artifact url must be an absolute HTTPS URL without credentials".to_string());
    }
    let sha256 = required_str(object, "sha256")?;
    if !is_sha256(sha256) {
        return Err("sha256 must be 64 hexadecimal characters".to_string());
    }
    let size = object
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| "size must be a positive integer".to_string())?;
    if size == 0 || size > MAX_ARTIFACT_BYTES {
        return Err(format!(
            "size must be between 1 and {MAX_ARTIFACT_BYTES} bytes"
        ));
    }
    Ok(Artifact {
        target,
        url: url.to_string(),
        sha256: sha256.to_ascii_lowercase(),
        size,
    })
}

fn parse_localizations(
    object: &Map<String, Value>,
) -> Result<BTreeMap<String, Localization>, String> {
    let mut localizations = BTreeMap::new();
    let Some(value) = object.get("localizations") else {
        return Ok(localizations);
    };
    if value.is_null() {
        return Ok(localizations);
    }
    let map = value
        .as_object()
        .ok_or_else(|| "localizations must be an object".to_string())?;
    for (tag, entry) in map {
        if tag.trim().is_empty() || tag.chars().count() > 35 {
            return Err(format!("invalid localization tag '{tag}'"));
        }
        let entry = entry
            .as_object()
            .ok_or_else(|| format!("localization '{tag}' must be an object"))?;
        let name = required_str(entry, &format!("localizations.{tag}.name"))?;
        if name.trim().is_empty() || name.chars().count() > 128 {
            return Err(format!(
                "localization '{tag}' name must be 1-128 characters"
            ));
        }
        let description =
            optional_str(entry, &format!("localizations.{tag}.description"))?.map(str::to_string);
        localizations.insert(
            tag.clone(),
            Localization {
                name: name.to_string(),
                description,
            },
        );
    }
    Ok(localizations)
}

fn required_str<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    object
        .get(key.split('.').next_back().unwrap_or(key))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} is required and must be a string"))
}

fn optional_str<'a>(object: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    match object.get(key.split('.').next_back().unwrap_or(key)) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn optional_url(object: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    let value = optional_str(object, key)?;
    if let Some(value) = value {
        if !is_https_url(value) {
            return Err(format!("{key} must be an HTTPS URL"));
        }
    }
    Ok(value.map(str::to_string))
}

fn string_array(object: &Map<String, Value>, key: &str) -> Result<Vec<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(values)) => {
            let mut result = Vec::new();
            for value in values {
                let text = value
                    .as_str()
                    .ok_or_else(|| format!("{key} must contain strings"))?;
                result.push(text.to_string());
            }
            Ok(result)
        }
        Some(_) => Err(format!("{key} must be an array")),
    }
}

/// Absolute HTTPS URL without credentials or a fragment.
pub fn is_https_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    if rest.is_empty()
        || url
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '\\')
        || url.contains('#')
    {
        return false;
    }
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    !authority.is_empty() && !authority.contains('@')
}

pub fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Validate a `plugin_store_url` setting value.
pub fn validate_catalog_source(source: &str) -> Result<(), String> {
    let source = source.trim();
    if source.is_empty() {
        return Err("plugin store URL must not be empty".to_string());
    }
    if source.starts_with("https://") {
        if is_https_url(source) {
            return Ok(());
        }
        return Err("plugin store URL must be a valid HTTPS URL".to_string());
    }
    if let Some(path) = source.strip_prefix("file://") {
        if path.starts_with('/') && !path.trim().is_empty() {
            return Ok(());
        }
        return Err("file:// plugin store URLs must use an absolute path".to_string());
    }
    if Path::new(source).is_absolute() {
        return Ok(());
    }
    Err("plugin store URL must use https://, file://, or an absolute path".to_string())
}

/// Default catalog URL published by the Luban Notepad store repository.
pub const DEFAULT_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/yyn0210/luban-notepad-store/main/catalog/index.json";

/// SHA-256 of a file as lowercase hexadecimal.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(target: &str) -> Value {
        serde_json::json!({
            "target": target,
            "url": format!("https://example.com/demo-1.0.0-{target}.zip"),
            "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "size": 1234
        })
    }

    fn plugin_json(id: &str, version: &str, artifacts: Vec<Value>) -> Value {
        serde_json::json!({
            "id": id,
            "name": "Demo",
            "description": "Demo plugin",
            "publisher": "example",
            "tags": ["demo", "text"],
            "permissions": ["filesystem"],
            "latest_version": version,
            "versions": [{
                "version": version,
                "api_version": 2,
                "release_notes": "Initial release.",
                "artifacts": artifacts
            }]
        })
    }

    fn catalog_json(plugins: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "catalog_version": 1,
            "repository": {
                "id": "luban-notepad-official",
                "name": "Luban Notepad Plugin Store",
                "homepage": "https://github.com/yyn0210/luban-notepad-store"
            },
            "generated_at": "2026-10-31T00:00:00Z",
            "plugins": plugins
        }))
        .unwrap()
    }

    #[test]
    fn parses_a_valid_catalog() {
        let parsed = parse_catalog(&catalog_json(vec![plugin_json(
            "com.example.demo",
            "1.0.0",
            vec![artifact("universal")],
        )]))
        .unwrap();
        assert!(parsed.warnings.is_empty());
        assert_eq!(parsed.catalog.plugins.len(), 1);
        let plugin = &parsed.catalog.plugins[0];
        assert_eq!(plugin.id, "com.example.demo");
        assert_eq!(plugin.versions[0].api_version, 2);
        assert_eq!(plugin.versions[0].artifacts[0].target, Target::Universal);
    }

    #[test]
    fn localizes_names_and_descriptions() {
        let mut plugin = plugin_json("com.example.demo", "1.0.0", vec![artifact("universal")]);
        plugin["localizations"] = serde_json::json!({
            "zh-CN": { "name": "示例插件", "description": "中文描述" }
        });
        let parsed = parse_catalog(&catalog_json(vec![plugin])).unwrap();
        let plugin = &parsed.catalog.plugins[0];
        assert_eq!(plugin.localized_name("zh-CN"), "示例插件");
        assert_eq!(plugin.localized_description("zh-CN"), Some("中文描述"));
        assert_eq!(plugin.localized_name("zh"), "示例插件");
        assert_eq!(plugin.localized_name("zh-cn"), "示例插件");
        assert_eq!(plugin.localized_name("en"), "Demo");
    }

    #[test]
    fn selects_exact_target_before_universal() {
        let parsed = parse_catalog(&catalog_json(vec![plugin_json(
            "com.example.demo",
            "1.0.0",
            vec![artifact("universal"), artifact("linux-x64")],
        )]))
        .unwrap();
        let plugin = &parsed.catalog.plugins[0];
        let candidate = plugin.install_candidate(Target::LinuxX64, 2).unwrap();
        assert!(candidate.exact_target);
        assert_eq!(candidate.artifact.target, Target::LinuxX64);
        let candidate = plugin.install_candidate(Target::DarwinArm64, 2).unwrap();
        assert!(!candidate.exact_target);
        assert_eq!(candidate.artifact.target, Target::Universal);
    }

    #[test]
    fn partial_platform_coverage_is_valid() {
        let parsed = parse_catalog(&catalog_json(vec![plugin_json(
            "com.example.demo",
            "1.0.0",
            vec![artifact("windows-arm64")],
        )]))
        .unwrap();
        let plugin = &parsed.catalog.plugins[0];
        assert!(plugin.supports_target(Target::WindowsArm64));
        assert!(!plugin.supports_target(Target::LinuxX64));
        assert!(plugin.install_candidate(Target::LinuxX64, 2).is_none());
    }

    #[test]
    fn rejects_unknown_and_duplicate_targets() {
        for artifacts in [
            vec![artifact("plan9-x64")],
            vec![artifact("linux-x64"), artifact("linux-x64")],
            vec![],
        ] {
            let parsed = parse_catalog(&catalog_json(vec![plugin_json(
                "com.example.demo",
                "1.0.0",
                artifacts,
            )]))
            .unwrap();
            assert!(parsed.catalog.plugins.is_empty());
            assert_eq!(parsed.warnings.len(), 1);
        }
    }

    #[test]
    fn rejects_invalid_url_hash_and_size() {
        let mut item = artifact("linux-x64");
        item["url"] = serde_json::json!("http://example.com/demo.zip");
        let parsed = parse_catalog(&catalog_json(vec![plugin_json(
            "com.example.demo",
            "1.0.0",
            vec![item],
        )]))
        .unwrap();
        assert!(parsed.catalog.plugins.is_empty());
        let mut item = artifact("linux-x64");
        item["sha256"] = serde_json::json!("nope");
        let parsed = parse_catalog(&catalog_json(vec![plugin_json(
            "com.example.demo",
            "1.0.0",
            vec![item],
        )]))
        .unwrap();
        assert!(parsed.catalog.plugins.is_empty());
        let mut item = artifact("linux-x64");
        item["size"] = serde_json::json!(0);
        let parsed = parse_catalog(&catalog_json(vec![plugin_json(
            "com.example.demo",
            "1.0.0",
            vec![item],
        )]))
        .unwrap();
        assert!(parsed.catalog.plugins.is_empty());
        let mut item = artifact("linux-x64");
        item["size"] = serde_json::json!(MAX_ARTIFACT_BYTES + 1);
        let parsed = parse_catalog(&catalog_json(vec![plugin_json(
            "com.example.demo",
            "1.0.0",
            vec![item],
        )]))
        .unwrap();
        assert!(parsed.catalog.plugins.is_empty());
    }

    #[test]
    fn isolates_a_bad_entry() {
        let mut bad = plugin_json("com.example.bad", "1.0.0", vec![artifact("linux-x64")]);
        bad["versions"] = serde_json::json!([]);
        let parsed = parse_catalog(&catalog_json(vec![
            plugin_json("com.example.good", "1.0.0", vec![artifact("universal")]),
            bad,
        ]))
        .unwrap();
        assert_eq!(parsed.catalog.plugins.len(), 1);
        assert_eq!(parsed.catalog.plugins[0].id, "com.example.good");
        assert_eq!(parsed.warnings.len(), 1);
    }

    #[test]
    fn rejects_duplicate_ids_and_version_mismatch() {
        let parsed = parse_catalog(&catalog_json(vec![
            plugin_json("com.example.demo", "1.0.0", vec![artifact("universal")]),
            plugin_json("com.example.demo", "1.0.0", vec![artifact("universal")]),
        ]))
        .unwrap();
        assert_eq!(parsed.catalog.plugins.len(), 1);
        assert_eq!(parsed.warnings.len(), 1);

        let mut plugin = plugin_json("com.example.demo", "1.0.0", vec![artifact("universal")]);
        plugin["latest_version"] = serde_json::json!("2.0.0");
        let parsed = parse_catalog(&catalog_json(vec![plugin])).unwrap();
        assert!(parsed.catalog.plugins.is_empty());
    }

    #[test]
    fn rejects_bad_catalog_headers() {
        assert!(parse_catalog(b"not json").is_err());
        let mut value: Value = serde_json::from_slice(&catalog_json(Vec::new())).unwrap();
        value["catalog_version"] = serde_json::json!(2);
        assert!(parse_catalog(value.to_string().as_bytes()).is_err());
        value["catalog_version"] = serde_json::json!(1);
        value.as_object_mut().unwrap().remove("repository");
        assert!(parse_catalog(value.to_string().as_bytes()).is_err());
        let oversized = vec![b' '; MAX_CATALOG_BYTES + 1];
        assert!(parse_catalog(&oversized).is_err());
    }

    #[test]
    fn computes_update_status() {
        let mut plugin = plugin_json("com.example.demo", "1.0.0", vec![artifact("universal")]);
        plugin["versions"] = serde_json::json!([
            {
                "version": "1.0.0",
                "api_version": 2,
                "artifacts": [artifact("universal")]
            },
            {
                "version": "1.2.0",
                "api_version": 2,
                "artifacts": [artifact("universal")]
            }
        ]);
        plugin["latest_version"] = serde_json::json!("1.2.0");
        let parsed = parse_catalog(&catalog_json(vec![plugin])).unwrap();
        let plugin = &parsed.catalog.plugins[0];
        assert_eq!(
            plugin.update_status(None, Target::LinuxX64, 2),
            UpdateStatus::NotInstalled
        );
        assert_eq!(
            plugin.update_status(Some("1.0.0"), Target::LinuxX64, 2),
            UpdateStatus::UpdateAvailable {
                installed: "1.0.0".to_string(),
                latest: "1.2.0".to_string()
            }
        );
        assert_eq!(
            plugin.update_status(Some("1.2.0"), Target::LinuxX64, 2),
            UpdateStatus::Installed
        );
    }

    #[test]
    fn filters_incompatible_api_versions() {
        let mut plugin = plugin_json("com.example.demo", "2.0.0", vec![artifact("universal")]);
        plugin["versions"][0]["api_version"] = serde_json::json!(3);
        let parsed = parse_catalog(&catalog_json(vec![plugin])).unwrap();
        let plugin = &parsed.catalog.plugins[0];
        assert!(plugin.install_candidate(Target::LinuxX64, 2).is_none());
        assert!(plugin.latest_compatible(2).is_none());
        assert!(plugin.requires_newer_api(2));
        assert_eq!(plugin.required_api_version(), 3);
    }

    /// The manifest layer loads only `API_VERSION` exactly, so a catalog
    /// entry for an older API (e.g. a legacy v1 plugin) must not be offered
    /// as an install candidate or update.
    #[test]
    fn rejects_older_api_versions() {
        let mut plugin = plugin_json("com.example.demo", "1.0.0", vec![artifact("universal")]);
        plugin["versions"][0]["api_version"] = serde_json::json!(1);
        let parsed = parse_catalog(&catalog_json(vec![plugin])).unwrap();
        let plugin = &parsed.catalog.plugins[0];
        assert!(plugin.install_candidate(Target::LinuxX64, 2).is_none());
        assert!(plugin.latest_compatible(2).is_none());
        assert!(plugin.requires_newer_api(2));
        assert_eq!(plugin.required_api_version(), 1);
        assert_eq!(
            plugin.update_status(Some("1.0.0"), Target::LinuxX64, 2),
            UpdateStatus::Installed
        );
    }

    /// A plugin publishing both an older and a newer API version offers
    /// neither to a host in between; `required_api_version` reports the
    /// newest declared API for the message.
    #[test]
    fn skips_versions_straddling_the_host_api() {
        let mut plugin = plugin_json("com.example.demo", "3.0.0", vec![artifact("universal")]);
        plugin["versions"] = serde_json::json!([
            {
                "version": "1.0.0",
                "api_version": 1,
                "artifacts": [artifact("universal")]
            },
            {
                "version": "3.0.0",
                "api_version": 3,
                "artifacts": [artifact("universal")]
            }
        ]);
        plugin["latest_version"] = serde_json::json!("3.0.0");
        let parsed = parse_catalog(&catalog_json(vec![plugin])).unwrap();
        let plugin = &parsed.catalog.plugins[0];
        assert!(plugin.install_candidate(Target::LinuxX64, 2).is_none());
        assert!(plugin.latest_compatible(2).is_none());
        assert!(plugin.requires_newer_api(2));
        assert_eq!(plugin.required_api_version(), 3);
    }

    #[test]
    fn searches_across_fields() {
        let parsed = parse_catalog(&catalog_json(vec![plugin_json(
            "com.example.demo",
            "1.0.0",
            vec![artifact("universal")],
        )]))
        .unwrap();
        let plugin = &parsed.catalog.plugins[0];
        assert!(plugin.matches_query("demo", "en"));
        assert!(plugin.matches_query("DEMO text", "en"));
        assert!(plugin.matches_query("example", "en"));
        assert!(!plugin.matches_query("demo missing", "en"));
        assert!(plugin.matches_query("   ", "en"));
    }

    #[test]
    fn maps_platforms_without_guessing() {
        assert_eq!(
            Target::for_platform("linux", "x86_64"),
            Some(Target::LinuxX64)
        );
        assert_eq!(
            Target::for_platform("linux", "aarch64"),
            Some(Target::LinuxArm64)
        );
        assert_eq!(
            Target::for_platform("macos", "aarch64"),
            Some(Target::DarwinArm64)
        );
        assert_eq!(
            Target::for_platform("windows", "x86_64"),
            Some(Target::WindowsX64)
        );
        assert_eq!(Target::for_platform("freebsd", "x86_64"), None);
        assert_eq!(Target::for_platform("linux", "riscv64"), None);
    }

    #[test]
    fn validates_catalog_sources() {
        assert_eq!(
            DEFAULT_CATALOG_URL,
            luban_core::settings::DEFAULT_PLUGIN_STORE_URL
        );
        assert_eq!(
            DEFAULT_CATALOG_URL,
            "https://raw.githubusercontent.com/yyn0210/luban-notepad-store/main/catalog/index.json"
        );
        assert!(validate_catalog_source(DEFAULT_CATALOG_URL).is_ok());
        assert!(validate_catalog_source("https://example.com/catalog.json").is_ok());
        assert!(validate_catalog_source("file:///tmp/catalog.json").is_ok());
        // A unix-style `/tmp/...` is rooted but not absolute on Windows, so use
        // the platform temp directory for the plain-path form.
        let absolute = std::env::temp_dir().join("catalog.json");
        assert!(validate_catalog_source(&absolute.to_string_lossy()).is_ok());
        assert!(validate_catalog_source("http://example.com/catalog.json").is_err());
        assert!(validate_catalog_source("ftp://example.com/catalog.json").is_err());
        assert!(validate_catalog_source("relative/catalog.json").is_err());
        assert!(validate_catalog_source("").is_err());
    }
}

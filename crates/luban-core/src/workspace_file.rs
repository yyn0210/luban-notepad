//! Multi-root workspace files (`.luban-workspace`).
//!
//! A workspace file is UTF-8 JSON holding the roots and the settings that may
//! override the global configuration for this workspace. Reading a file never
//! fails on unknown fields; it only reports warnings for values that had to be
//! degraded.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::settings::{normalize_ruler_columns, Settings};

pub const WORKSPACE_VERSION: u32 = 1;
pub const WORKSPACE_EXTENSION: &str = "luban-workspace";

/// Settings a workspace file may override.
///
/// Every field is optional so that a missing value inherits the global
/// setting. Theme, font, keybindings, macros, per-language settings and local
/// history are deliberately absent: they stay global.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceSettings {
    pub tab_size: Option<usize>,
    pub use_spaces: Option<bool>,
    pub word_wrap: Option<bool>,
    pub show_line_numbers: Option<bool>,
    pub show_whitespace: Option<bool>,
    pub show_indent_guides: Option<bool>,
    pub show_ruler: Option<bool>,
    pub ruler_columns: Option<Vec<usize>>,
    pub show_document_map: Option<bool>,
    pub respect_gitignore: Option<bool>,
    pub search_include: Option<String>,
    pub search_exclude: Option<String>,
}

impl WorkspaceSettings {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Copy every present override onto `settings`.
    pub fn apply_to(&self, settings: &mut Settings) {
        if let Some(value) = self.tab_size {
            settings.tab_size = value;
        }
        if let Some(value) = self.use_spaces {
            settings.use_spaces = value;
        }
        if let Some(value) = self.word_wrap {
            settings.word_wrap = value;
        }
        if let Some(value) = self.show_line_numbers {
            settings.show_line_numbers = value;
        }
        if let Some(value) = self.show_whitespace {
            settings.show_whitespace = value;
        }
        if let Some(value) = self.show_indent_guides {
            settings.show_indent_guides = value;
        }
        if let Some(value) = self.show_ruler {
            settings.show_ruler = value;
        }
        if let Some(columns) = &self.ruler_columns {
            settings.ruler_columns = columns.clone();
        }
        if let Some(value) = self.show_document_map {
            settings.show_document_map = value;
        }
        if let Some(value) = self.respect_gitignore {
            settings.respect_gitignore = value;
        }
        if let Some(value) = &self.search_include {
            settings.search_include = value.clone();
        }
        if let Some(value) = &self.search_exclude {
            settings.search_exclude = value.clone();
        }
    }

    /// Capture a workspace override from the current global settings, leaving
    /// fields that still match the defaults out of the file.
    pub fn capture(settings: &Settings, previous: &Settings) -> Self {
        let mut out = Self::default();
        if settings.tab_size != previous.tab_size {
            out.tab_size = Some(settings.tab_size);
        }
        if settings.use_spaces != previous.use_spaces {
            out.use_spaces = Some(settings.use_spaces);
        }
        if settings.word_wrap != previous.word_wrap {
            out.word_wrap = Some(settings.word_wrap);
        }
        if settings.show_line_numbers != previous.show_line_numbers {
            out.show_line_numbers = Some(settings.show_line_numbers);
        }
        if settings.show_whitespace != previous.show_whitespace {
            out.show_whitespace = Some(settings.show_whitespace);
        }
        if settings.show_indent_guides != previous.show_indent_guides {
            out.show_indent_guides = Some(settings.show_indent_guides);
        }
        if settings.show_ruler != previous.show_ruler {
            out.show_ruler = Some(settings.show_ruler);
        }
        if settings.ruler_columns != previous.ruler_columns {
            out.ruler_columns = Some(settings.ruler_columns.clone());
        }
        if settings.show_document_map != previous.show_document_map {
            out.show_document_map = Some(settings.show_document_map);
        }
        if settings.respect_gitignore != previous.respect_gitignore {
            out.respect_gitignore = Some(settings.respect_gitignore);
        }
        if settings.search_include != previous.search_include {
            out.search_include = Some(settings.search_include.clone());
        }
        if settings.search_exclude != previous.search_exclude {
            out.search_exclude = Some(settings.search_exclude.clone());
        }
        out
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceFile {
    pub version: u32,
    pub name: String,
    pub roots: Vec<PathBuf>,
    pub settings: WorkspaceSettings,
}

impl Default for WorkspaceFile {
    fn default() -> Self {
        Self {
            version: WORKSPACE_VERSION,
            name: String::new(),
            roots: Vec::new(),
            settings: WorkspaceSettings::default(),
        }
    }
}

/// A workspace file together with the non-fatal warnings produced while
/// reading it.
#[derive(Clone, Debug)]
pub struct LoadedWorkspace {
    pub workspace: WorkspaceFile,
    pub warnings: Vec<String>,
}

impl WorkspaceFile {
    /// Read a workspace file. Unknown versions keep the roots and name but
    /// fall back to inherited settings, with a warning.
    pub fn load(path: &Path) -> Result<LoadedWorkspace> {
        let json = fs::read_to_string(path)?;
        let mut warnings = Vec::new();
        let raw: serde_json::Value = serde_json::from_str(&json)?;
        let version = raw
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1) as u32;
        let mut workspace: WorkspaceFile = serde_json::from_value(raw).map_err(Error::from)?;
        if version > WORKSPACE_VERSION {
            warnings.push(format!(
                "workspace version {version} is newer than {WORKSPACE_VERSION}; settings fall back to defaults"
            ));
            workspace.settings = WorkspaceSettings::default();
        }
        workspace.version = WORKSPACE_VERSION;
        workspace.validate();
        if workspace.roots.is_empty() {
            warnings.push("workspace has no roots".to_string());
        }
        Ok(LoadedWorkspace {
            workspace,
            warnings,
        })
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        crate::atomic_write::write_atomic(path, json.as_bytes())?;
        Ok(())
    }

    /// Resolve roots against the workspace file's directory. Relative roots
    /// that no longer exist stay in the model; the caller marks them
    /// unavailable.
    pub fn resolved_roots(&self, workspace_path: &Path) -> Vec<PathBuf> {
        let base = workspace_path.parent().unwrap_or_else(|| Path::new("."));
        self.roots
            .iter()
            .map(|root| {
                let resolved = if root.is_absolute() {
                    root.clone()
                } else {
                    base.join(root)
                };
                resolved.components().collect()
            })
            .collect()
    }

    pub fn display_name(&self, workspace_path: &Path) -> String {
        let name = self.name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
        workspace_path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| {
                workspace_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| workspace_path.display().to_string())
            })
    }

    /// Suggest a workspace path inside `root`.
    pub fn suggested_path(root: &Path) -> PathBuf {
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "workspace".to_string());
        root.join(format!("{name}.{WORKSPACE_EXTENSION}"))
    }

    fn validate(&mut self) {
        self.name = self.name.trim().to_string();
        self.roots.retain(|root| !root.as_os_str().is_empty());
        if let Some(size) = self.settings.tab_size.as_mut() {
            *size = (*size).clamp(1, 16);
        }
        if let Some(columns) = self.settings.ruler_columns.as_mut() {
            normalize_ruler_columns(columns);
        }
        if let Some(include) = self.settings.search_include.as_mut() {
            *include = include.trim().to_string();
        }
        if let Some(exclude) = self.settings.search_exclude.as_mut() {
            *exclude = exclude.trim().to_string();
        }
    }
}

/// Whether `path` looks like a workspace file by extension.
pub fn is_workspace_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(WORKSPACE_EXTENSION))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_roots_and_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.luban-workspace");
        let workspace = WorkspaceFile {
            version: WORKSPACE_VERSION,
            name: "Demo".to_string(),
            roots: vec![PathBuf::from("a"), dir.path().join("b")],
            settings: WorkspaceSettings {
                tab_size: Some(2),
                word_wrap: Some(true),
                ruler_columns: Some(vec![120, 0, 80, 80]),
                respect_gitignore: Some(false),
                ..WorkspaceSettings::default()
            },
        };
        workspace.save_to(&path).unwrap();
        let loaded = WorkspaceFile::load(&path).unwrap();
        assert_eq!(loaded.workspace.name, "Demo");
        assert_eq!(loaded.workspace.roots.len(), 2);
        assert_eq!(loaded.workspace.settings.tab_size, Some(2));
        assert_eq!(
            loaded.workspace.settings.ruler_columns.as_deref(),
            Some([80, 120].as_slice())
        );
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn relative_roots_resolve_against_the_workspace_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.luban-workspace");
        let absolute_root = dir.path().join("abs");
        let workspace = WorkspaceFile {
            roots: vec![PathBuf::from("src"), absolute_root.clone()],
            ..WorkspaceFile::default()
        };
        let roots = workspace.resolved_roots(&path);
        assert_eq!(roots[0], dir.path().join("src"));
        assert_eq!(roots[1], absolute_root);
    }

    #[test]
    fn unknown_version_keeps_roots_but_resets_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.luban-workspace");
        fs::write(
            &path,
            r#"{"version": 9, "name": "Future", "roots": ["."],
                "settings": {"tab_size": 8}}"#,
        )
        .unwrap();
        let loaded = WorkspaceFile::load(&path).unwrap();
        assert_eq!(loaded.workspace.roots, vec![PathBuf::from(".")]);
        assert_eq!(loaded.workspace.settings.tab_size, None);
        assert_eq!(loaded.warnings.len(), 1);
    }

    #[test]
    fn missing_version_is_current_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.luban-workspace");
        fs::write(&path, r#"{"roots": ["."], "settings": {"tab_size": 3}}"#).unwrap();
        let loaded = WorkspaceFile::load(&path).unwrap();
        assert_eq!(loaded.workspace.version, WORKSPACE_VERSION);
        assert_eq!(loaded.workspace.settings.tab_size, Some(3));
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn overrides_apply_only_present_fields() {
        let mut global = Settings {
            tab_size: 4,
            word_wrap: false,
            ..Settings::default()
        };
        let overrides = WorkspaceSettings {
            tab_size: Some(8),
            ..WorkspaceSettings::default()
        };
        overrides.apply_to(&mut global);
        assert_eq!(global.tab_size, 8);
        assert!(!global.word_wrap);
        assert!(global.show_indent_guides);
    }

    #[test]
    fn capture_records_changes() {
        let previous = Settings::default();
        let mut current = previous.clone();
        current.word_wrap = true;
        current.respect_gitignore = false;
        let overrides = WorkspaceSettings::capture(&current, &previous);
        assert_eq!(overrides.word_wrap, Some(true));
        assert_eq!(overrides.respect_gitignore, Some(false));
        assert_eq!(overrides.tab_size, None);
    }

    #[test]
    fn detects_workspace_extension() {
        assert!(is_workspace_file(Path::new("a.luban-workspace")));
        assert!(is_workspace_file(Path::new("a.LUBAN-WORKSPACE")));
        assert!(!is_workspace_file(Path::new("a.json")));
    }
}

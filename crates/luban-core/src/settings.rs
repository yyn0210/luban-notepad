//! Persistent application settings.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::encoding::TextEncoding;
use crate::eol::Eol;
use crate::large_file::SizeUnit;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    Dark,
    #[default]
    Light,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    /// Name of a syntect theme. The GUI falls back to the built-in default if
    /// the name is not available.
    pub syntax_theme: String,
    /// Empty selects the bundled monospace font.
    pub font_family: String,
    pub font_size: f32,
    pub tab_size: usize,
    pub use_spaces: bool,
    pub word_wrap: bool,
    pub show_line_numbers: bool,
    pub show_status_bar: bool,
    pub show_toolbar: bool,
    pub show_file_explorer: bool,
    pub show_hidden_files: bool,
    pub highlight_current_line: bool,
    pub highlight_matching_brackets: bool,
    pub auto_indent: bool,
    pub trim_trailing_whitespace_on_save: bool,
    pub restore_session: bool,
    pub autosave_session: bool,
    pub session_interval_secs: u64,
    pub max_recent_files: usize,
    pub recent_files: Vec<PathBuf>,
    pub large_file_threshold: usize,
    /// Show the large-file controller automatically for files at or above the
    /// controller threshold.
    pub large_file_controller: bool,
    /// Files at or above this size open through the controller. Independent of
    /// `large_file_threshold`, which controls the reduced editor mode.
    pub large_file_controller_threshold: usize,
    /// Default length of a partial open when no size has been chosen.
    pub large_file_partial_size: usize,
    /// Unit used for byte counts in the controller.
    pub large_file_display_unit: SizeUnit,
    pub max_file_size_mb: u64,
    pub default_encoding: TextEncoding,
    pub default_eol: Eol,
    /// UI language: `"system"` follows the operating system, otherwise a
    /// supported tag such as `"en"` or `"zh-CN"`.
    #[serde(default = "default_ui_language")]
    pub ui_language: String,
}

fn default_ui_language() -> String {
    "system".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Light,
            syntax_theme: "InspiredGitHub".to_string(),
            font_family: String::new(),
            font_size: 14.0,
            tab_size: 4,
            use_spaces: true,
            word_wrap: false,
            show_line_numbers: true,
            show_status_bar: true,
            show_toolbar: true,
            show_file_explorer: false,
            show_hidden_files: false,
            highlight_current_line: true,
            highlight_matching_brackets: true,
            auto_indent: true,
            trim_trailing_whitespace_on_save: false,
            restore_session: true,
            autosave_session: true,
            session_interval_secs: 20,
            max_recent_files: 20,
            recent_files: Vec::new(),
            large_file_threshold: 100 * 1024 * 1024,
            large_file_controller: true,
            large_file_controller_threshold: 200 * 1024 * 1024,
            large_file_partial_size: 200 * 1024 * 1024,
            large_file_display_unit: SizeUnit::Bytes,
            max_file_size_mb: 1024,
            default_encoding: TextEncoding::Utf8,
            default_eol: Eol::native(),
            ui_language: default_ui_language(),
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(json) => match serde_json::from_str::<Settings>(&json) {
                Ok(mut settings) => {
                    settings.validate();
                    settings
                }
                Err(err) => {
                    log::warn!("ignoring invalid settings at {}: {err}", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                log::warn!("could not read settings at {}: {err}", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        fs::write(path, json)
    }

    pub fn path() -> PathBuf {
        config_dir().join("settings.json")
    }

    pub fn validate(&mut self) {
        self.font_size = self.font_size.clamp(6.0, 72.0);
        self.tab_size = self.tab_size.clamp(1, 16);
        self.session_interval_secs = self.session_interval_secs.clamp(5, 3600);
        self.max_recent_files = self.max_recent_files.min(100);
        self.large_file_threshold = self
            .large_file_threshold
            .clamp(64 * 1024, 512 * 1024 * 1024);
        self.large_file_controller_threshold = self
            .large_file_controller_threshold
            .clamp(1024 * 1024, 4096 * 1024 * 1024);
        self.large_file_partial_size = self
            .large_file_partial_size
            .clamp(1024 * 1024, 4096 * 1024 * 1024);
        self.max_file_size_mb = self.max_file_size_mb.clamp(1, 4096);
        if self.syntax_theme.is_empty() {
            self.syntax_theme = "InspiredGitHub".to_string();
        }
        self.recent_files.retain(|p| !p.as_os_str().is_empty());
        self.recent_files.dedup_by(|a, b| paths_equal(a, b));
        self.recent_files.truncate(self.max_recent_files);
        let language = self.ui_language.trim();
        if language.is_empty()
            || (language != "system" && !crate::i18n::SUPPORTED.contains(&language))
        {
            self.ui_language = "system".to_string();
        } else {
            self.ui_language = language.to_string();
        }
    }

    /// Push a file to the top of the recent list, removing duplicates.
    pub fn add_recent(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        self.recent_files.retain(|p| !paths_equal(p, &path));
        self.recent_files.insert(0, path);
        self.recent_files.truncate(self.max_recent_files);
    }

    pub fn remove_recent(&mut self, path: &Path) {
        self.recent_files.retain(|p| !paths_equal(p, path));
    }

    pub fn clear_recent(&mut self) {
        self.recent_files.clear();
    }

    pub fn max_file_size_bytes(&self) -> u64 {
        self.max_file_size_mb.saturating_mul(1024 * 1024)
    }

    pub fn large_file_controller_threshold_bytes(&self) -> u64 {
        self.large_file_controller_threshold as u64
    }

    pub fn large_file_partial_size_bytes(&self) -> u64 {
        (self.large_file_partial_size as u64).min(self.max_file_size_bytes())
    }
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// Marker file shipped next to the executable by the Windows portable build.
pub const PORTABLE_MARKER: &str = "luban-notepad.portable";

/// Per-user configuration directory. `LUBAN_NOTEPAD_CONFIG_DIR` overrides it,
/// which is useful for tests and for launchers that relocate the profile.
///
/// A portable install keeps `settings.json` and `session.json` in `data/` next
/// to the executable, selected by a [`PORTABLE_MARKER`] file. Installed builds
/// fall back to the normal per-user directory.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = env::var("LUBAN_NOTEPAD_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = portable_config_dir(&exe) {
            return dir;
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("luban-notepad")
}

/// Returns `<exe dir>/data` when the portable marker sits next to `exe`.
fn portable_config_dir(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    dir.join(PORTABLE_MARKER)
        .is_file()
        .then(|| dir.join("data"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let s = Settings::default();
        assert_eq!(s.tab_size, 4);
        assert!(s.show_line_numbers);
        assert!(s.restore_session);
        assert_eq!(s.default_encoding, TextEncoding::Utf8);
        assert_eq!(s.max_file_size_bytes(), 1024 * 1024 * 1024);
        assert_eq!(s.large_file_threshold, 100 * 1024 * 1024);
        assert!(s.large_file_controller);
        assert_eq!(s.large_file_controller_threshold, 200 * 1024 * 1024);
        assert_eq!(s.large_file_partial_size, 200 * 1024 * 1024);
        assert_eq!(s.large_file_display_unit, SizeUnit::Bytes);
    }

    #[test]
    fn controller_thresholds_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{"large_file_controller_threshold": 0, "large_file_partial_size": 0}"#,
        )
        .unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.large_file_controller_threshold, 1024 * 1024);
        assert_eq!(loaded.large_file_partial_size, 1024 * 1024);
        fs::write(
            &path,
            r#"{"large_file_controller_threshold": 999999999999, "large_file_partial_size": 999999999999}"#,
        )
        .unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.large_file_controller_threshold, 4096 * 1024 * 1024);
        assert_eq!(loaded.large_file_partial_size, 4096 * 1024 * 1024);
    }

    #[test]
    fn partial_default_is_capped_by_the_open_limit() {
        let s = Settings {
            max_file_size_mb: 50,
            ..Settings::default()
        };
        assert_eq!(s.large_file_partial_size_bytes(), 50 * 1024 * 1024);
    }

    #[test]
    fn large_file_threshold_is_validated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"large_file_threshold": 0}"#).unwrap();
        assert_eq!(Settings::load_from(&path).large_file_threshold, 64 * 1024);
        fs::write(&path, r#"{"large_file_threshold": 999999999999}"#).unwrap();
        assert_eq!(
            Settings::load_from(&path).large_file_threshold,
            512 * 1024 * 1024
        );
    }

    #[test]
    fn ui_language_defaults_to_system_and_accepts_supported_tags() {
        assert_eq!(Settings::default().ui_language, "system");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"font_size": 14}"#).unwrap();
        assert_eq!(Settings::load_from(&path).ui_language, "system");

        fs::write(&path, r#"{"ui_language": "zh-CN"}"#).unwrap();
        assert_eq!(Settings::load_from(&path).ui_language, "zh-CN");

        fs::write(&path, r#"{"ui_language": "fr"}"#).unwrap();
        assert_eq!(Settings::load_from(&path).ui_language, "system");
    }

    #[test]
    fn round_trips_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut s = Settings {
            font_size: 17.5,
            theme: ThemeChoice::Dark,
            ..Settings::default()
        };
        s.add_recent("/tmp/a.txt");
        s.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path);
        assert!((loaded.font_size - 17.5).abs() < f32::EPSILON);
        assert_eq!(loaded.theme, ThemeChoice::Dark);
        assert_eq!(loaded.recent_files, vec![PathBuf::from("/tmp/a.txt")]);
    }

    #[test]
    fn invalid_values_are_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{"font_size": 1000, "tab_size": 0, "max_recent_files": 2,
                "recent_files": ["/a", "/b", "/c"]}"#,
        )
        .unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.font_size, 72.0);
        assert_eq!(loaded.tab_size, 1);
        assert_eq!(loaded.recent_files.len(), 2);
    }

    #[test]
    fn recent_files_are_deduplicated() {
        let mut s = Settings {
            max_recent_files: 3,
            ..Settings::default()
        };
        s.add_recent("/a");
        s.add_recent("/b");
        s.add_recent("/a");
        assert_eq!(
            s.recent_files,
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
        s.add_recent("/c");
        s.add_recent("/d");
        assert_eq!(
            s.recent_files,
            vec![
                PathBuf::from("/d"),
                PathBuf::from("/c"),
                PathBuf::from("/a")
            ]
        );
        s.remove_recent(Path::new("/c"));
        assert!(!s.recent_files.contains(&PathBuf::from("/c")));
    }

    #[test]
    fn missing_file_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = Settings::load_from(&dir.path().join("absent.json"));
        assert_eq!(loaded.tab_size, Settings::default().tab_size);
    }

    #[test]
    fn portable_marker_moves_config_next_to_executable() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("luban-notepad.exe");
        fs::write(&exe, b"").unwrap();
        assert_eq!(portable_config_dir(&exe), None);

        fs::write(dir.path().join(PORTABLE_MARKER), b"").unwrap();
        assert_eq!(portable_config_dir(&exe), Some(dir.path().join("data")));
    }
}

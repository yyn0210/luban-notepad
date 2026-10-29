//! Persistent application settings.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::encoding::TextEncoding;
use crate::eol::Eol;
use crate::large_file::SizeUnit;
use crate::macros::MacroSet;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    Dark,
    #[default]
    Light,
    /// Follow the operating system's light/dark appearance.
    System,
}

/// Per-language indentation and line-ending defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LanguageSettings {
    pub tab_size: Option<usize>,
    pub use_spaces: Option<bool>,
    pub eol: Option<Eol>,
}

impl LanguageSettings {
    pub fn is_empty(&self) -> bool {
        self.tab_size.is_none() && self.use_spaces.is_none() && self.eol.is_none()
    }
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
    /// Keep a snapshot of the previous on-disk content before each save.
    #[serde(default = "default_true")]
    pub local_history_enabled: bool,
    /// Snapshot retention in days.
    #[serde(default = "default_history_days")]
    pub local_history_days: u64,
    /// Upper bound for one file's stored snapshots, in MiB.
    #[serde(default = "default_history_max_mb")]
    pub local_history_max_mb: u64,
    /// Per-command shortcut overrides, keyed by command ID. The value uses
    /// `+`-separated modifiers and a key name, e.g. `ctrl+shift+p`.
    #[serde(default)]
    pub keybindings: std::collections::BTreeMap<String, String>,
    /// Recent non-empty find queries, most recent first.
    #[serde(default)]
    pub search_history: Vec<String>,
    /// Recent non-empty replacement texts, most recent first.
    #[serde(default)]
    pub replace_history: Vec<String>,
    /// Command-level macros and their shortcuts.
    #[serde(default)]
    pub macros: MacroSet,
    /// Per-language indentation and line-ending overrides.
    #[serde(default)]
    pub language_settings: std::collections::BTreeMap<String, LanguageSettings>,
    /// Path to an imported `.tmTheme` file. `None` uses a built-in theme.
    #[serde(default)]
    pub syntax_theme_path: Option<PathBuf>,
}

fn default_true() -> bool {
    true
}

fn default_history_days() -> u64 {
    30
}

fn default_history_max_mb() -> u64 {
    200
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
            local_history_enabled: true,
            local_history_days: 30,
            local_history_max_mb: 200,
            keybindings: std::collections::BTreeMap::new(),
            search_history: Vec::new(),
            replace_history: Vec::new(),
            macros: MacroSet::default(),
            language_settings: std::collections::BTreeMap::new(),
            syntax_theme_path: None,
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
        self.local_history_days = self.local_history_days.clamp(1, 3650);
        self.local_history_max_mb = self.local_history_max_mb.clamp(1, 4096);
        self.keybindings
            .retain(|id, binding| !id.trim().is_empty() && !binding.trim().is_empty());
        for binding in self.keybindings.values_mut() {
            *binding = binding.trim().to_lowercase();
        }
        normalize_history(&mut self.search_history);
        normalize_history(&mut self.replace_history);
        self.macros.validate();
        self.language_settings.retain(|name, settings| {
            if name.trim().is_empty() {
                return false;
            }
            if let Some(size) = settings.tab_size.as_mut() {
                *size = (*size).clamp(1, 16);
            }
            !settings.is_empty()
        });
        if self
            .syntax_theme_path
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            self.syntax_theme_path = None;
        }
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

    /// Record a non-empty find query, most recent first, capped at 20.
    pub fn record_search(&mut self, query: &str) {
        push_history(&mut self.search_history, query);
    }

    /// Record a non-empty replacement text, most recent first, capped at 20.
    pub fn record_replacement(&mut self, replacement: &str) {
        push_history(&mut self.replace_history, replacement);
    }

    pub fn language_overrides(&self, language: &str) -> Option<&LanguageSettings> {
        self.language_settings.get(language)
    }

    /// Indentation width for a language, falling back to the global setting.
    pub fn tab_size_for(&self, language: &str) -> usize {
        self.language_overrides(language)
            .and_then(|settings| settings.tab_size)
            .unwrap_or(self.tab_size)
    }

    pub fn use_spaces_for(&self, language: &str) -> bool {
        self.language_overrides(language)
            .and_then(|settings| settings.use_spaces)
            .unwrap_or(self.use_spaces)
    }

    /// Line ending for a newly created document in `language`.
    pub fn eol_for_new_document(&self, language: &str) -> Eol {
        self.language_overrides(language)
            .and_then(|settings| settings.eol)
            .unwrap_or(self.default_eol)
    }
}

fn normalize_history(history: &mut Vec<String>) {
    let mut out: Vec<String> = Vec::new();
    for entry in history.drain(..) {
        let entry = entry.trim().to_string();
        if entry.is_empty() || out.iter().any(|existing| existing == &entry) {
            continue;
        }
        out.push(entry);
        if out.len() >= 20 {
            break;
        }
    }
    *history = out;
}

fn push_history(history: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    history.retain(|entry| entry != value);
    history.insert(0, value.to_string());
    history.truncate(20);
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
    fn local_history_and_keybindings_default_and_clamp() {
        let s = Settings::default();
        assert!(s.local_history_enabled);
        assert_eq!(s.local_history_days, 30);
        assert_eq!(s.local_history_max_mb, 200);
        assert!(s.keybindings.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{"local_history_days": 0, "local_history_max_mb": 0,
                "keybindings": {"save": " Ctrl+S ", "empty": "  "}}"#,
        )
        .unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.local_history_days, 1);
        assert_eq!(loaded.local_history_max_mb, 1);
        assert_eq!(
            loaded.keybindings.get("save").map(String::as_str),
            Some("ctrl+s")
        );
        assert!(!loaded.keybindings.contains_key("empty"));
    }

    #[test]
    fn search_history_dedupes_and_caps() {
        let mut settings = Settings::default();
        settings.record_search("  alpha  ");
        settings.record_search("beta");
        settings.record_search("alpha");
        assert_eq!(settings.search_history, ["alpha", "beta"]);
        for index in 0..30 {
            settings.record_search(&format!("q{index}"));
        }
        assert_eq!(settings.search_history.len(), 20);
        assert_eq!(settings.search_history[0], "q29");
    }

    #[test]
    fn language_overrides_fall_back_and_validate() {
        let mut settings = Settings::default();
        assert_eq!(settings.tab_size_for("Python"), settings.tab_size);
        assert_eq!(
            settings.eol_for_new_document("Python"),
            settings.default_eol
        );
        settings.language_settings.insert(
            "Python".to_string(),
            LanguageSettings {
                tab_size: Some(99),
                use_spaces: Some(false),
                eol: Some(Eol::CrLf),
            },
        );
        settings
            .language_settings
            .insert("Empty".to_string(), LanguageSettings::default());
        settings.validate();
        assert_eq!(settings.tab_size_for("Python"), 16);
        assert!(!settings.use_spaces_for("Python"));
        assert_eq!(settings.eol_for_new_document("Python"), Eol::CrLf);
        assert!(!settings.language_settings.contains_key("Empty"));
    }

    #[test]
    fn macros_and_theme_path_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut settings = Settings::default();
        settings
            .macros
            .upsert("demo", vec!["edit.duplicate-line".to_string()])
            .unwrap();
        settings.syntax_theme_path = Some(PathBuf::from("/tmp/theme.tmTheme"));
        settings.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(
            loaded.macros.find("demo").unwrap().commands,
            ["edit.duplicate-line"]
        );
        assert_eq!(
            loaded.syntax_theme_path.as_deref(),
            Some(Path::new("/tmp/theme.tmTheme"))
        );
    }

    #[test]
    fn theme_round_trips_system() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let s = Settings {
            theme: ThemeChoice::System,
            ..Settings::default()
        };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).theme, ThemeChoice::System);
        assert!(fs::read_to_string(&path).unwrap().contains("\"system\""));
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

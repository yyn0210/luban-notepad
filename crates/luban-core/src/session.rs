//! Session persistence (open tabs, caret positions, unsaved buffers).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::encoding::TextEncoding;
use crate::eol::Eol;
use crate::settings::config_dir;

pub const SESSION_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionTab {
    pub path: Option<PathBuf>,
    /// Present for untitled or modified tabs so unsaved work survives a
    /// restart/crash. `None` for unmodified files on disk.
    pub text: Option<String>,
    pub encoding: TextEncoding,
    pub eol: Eol,
    pub language: String,
    pub cursor: usize,
    pub anchor: usize,
    pub scroll_offset: f32,
    pub modified: bool,
    /// Byte range of a partially opened file (`start`, `end`). `None` for
    /// whole-file tabs and for tabs restored from older sessions.
    pub range: Option<[u64; 2]>,
    /// Total size of the source file when the range was recorded.
    pub source_size: Option<u64>,
    /// The tab is backed by the disk engine.
    #[serde(default)]
    pub huge: bool,
    /// Global 1-based line restored for a disk-engine tab.
    #[serde(default)]
    pub huge_line: Option<u64>,
    /// Global 1-based column restored for a disk-engine tab.
    #[serde(default)]
    pub huge_column: Option<u64>,
    /// Scroll offset (lines from the top of the window) for a disk-engine tab.
    #[serde(default)]
    pub huge_scroll: Option<f32>,
    /// Whether a disk-engine tab had unsaved edits. The edits themselves are
    /// never written to the session.
    #[serde(default)]
    pub huge_modified: bool,
    /// Recovery directory identifier for a disk-engine tab with unsaved
    /// edits. `None` for clean tabs and for older sessions.
    #[serde(default)]
    pub recovery_id: Option<String>,
}

impl Default for SessionTab {
    fn default() -> Self {
        Self {
            path: None,
            text: None,
            encoding: TextEncoding::Utf8,
            eol: Eol::native(),
            language: "Plain Text".to_string(),
            cursor: 0,
            anchor: 0,
            scroll_offset: 0.0,
            modified: false,
            range: None,
            source_size: None,
            huge: false,
            huge_line: None,
            huge_column: None,
            huge_scroll: None,
            huge_modified: false,
            recovery_id: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub version: u32,
    pub tabs: Vec<SessionTab>,
    pub active_tab: usize,
    pub workspace_root: Option<PathBuf>,
    pub window_width: f32,
    pub window_height: f32,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            version: SESSION_VERSION,
            tabs: Vec::new(),
            active_tab: 0,
            workspace_root: None,
            window_width: 1200.0,
            window_height: 800.0,
        }
    }
}

impl Session {
    pub fn path() -> PathBuf {
        config_dir().join("session.json")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(json) => match serde_json::from_str::<Session>(&json) {
                Ok(mut session) => {
                    session.version = SESSION_VERSION;
                    if session.active_tab >= session.tabs.len() {
                        session.active_tab = session.tabs.len().saturating_sub(1);
                    }
                    session.window_width = session.window_width.clamp(480.0, 10000.0);
                    session.window_height = session.window_height.clamp(320.0, 10000.0);
                    session
                }
                Err(err) => {
                    log::warn!("ignoring invalid session at {}: {err}", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                log::warn!("could not read session at {}: {err}", path.display());
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
        // Write-then-rename keeps a crash from truncating the previous session.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        #[cfg(windows)]
        {
            if path.exists() {
                let _ = fs::remove_file(path);
            }
        }
        fs::rename(tmp, path)
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let session = Session {
            tabs: vec![SessionTab {
                path: Some(PathBuf::from("/tmp/foo.rs")),
                text: Some("fn main() {}".into()),
                encoding: TextEncoding::Utf8,
                eol: Eol::Lf,
                language: "Rust".into(),
                cursor: 5,
                anchor: 2,
                scroll_offset: 12.5,
                modified: true,
                range: Some([100, 4096]),
                source_size: Some(8192),
                huge: true,
                huge_line: Some(42),
                huge_column: Some(3),
                huge_scroll: Some(1.5),
                huge_modified: true,
                recovery_id: Some("recovery-1".into()),
            }],
            active_tab: 0,
            workspace_root: Some(PathBuf::from("/tmp")),
            ..Session::default()
        };
        session.save_to(&path).unwrap();
        let loaded = Session::load_from(&path);
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs[0].text.as_deref(), Some("fn main() {}"));
        assert_eq!(loaded.tabs[0].cursor, 5);
        assert_eq!(loaded.tabs[0].language, "Rust");
        assert_eq!(loaded.tabs[0].range, Some([100, 4096]));
        assert_eq!(loaded.tabs[0].source_size, Some(8192));
        assert!(loaded.tabs[0].huge);
        assert_eq!(loaded.tabs[0].huge_line, Some(42));
        assert_eq!(loaded.tabs[0].huge_column, Some(3));
        assert_eq!(loaded.tabs[0].huge_scroll, Some(1.5));
        assert!(loaded.tabs[0].huge_modified);
        assert_eq!(loaded.tabs[0].recovery_id.as_deref(), Some("recovery-1"));
        assert_eq!(loaded.workspace_root, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn clamps_active_tab_and_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        fs::write(
            &path,
            r#"{"tabs": [], "active_tab": 9, "window_width": 2, "window_height": 999999}"#,
        )
        .unwrap();
        let loaded = Session::load_from(&path);
        assert_eq!(loaded.active_tab, 0);
        assert_eq!(loaded.window_width, 480.0);
        assert_eq!(loaded.window_height, 10000.0);
    }

    #[test]
    fn missing_session_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = Session::load_from(&dir.path().join("none.json"));
        assert!(loaded.is_empty());
        assert_eq!(loaded.version, SESSION_VERSION);
    }
}

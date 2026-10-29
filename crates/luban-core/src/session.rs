//! Session persistence (windows, tab groups, open tabs, caret positions,
//! unsaved buffers).
//!
//! Version 2 stores a list of windows, each with up to two tab groups. A
//! version 1 file (a single flat tab list) is migrated on load.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::encoding::TextEncoding;
use crate::eol::Eol;
use crate::settings::config_dir;

pub const SESSION_VERSION: u32 = 2;

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
    /// 1-based line numbers bookmarked in this tab.
    #[serde(default)]
    pub bookmarks: Vec<usize>,
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
            bookmarks: Vec::new(),
        }
    }
}

/// How a window is split. `Right` creates a second group beside the first;
/// `Down` places it below.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitLayout {
    #[default]
    None,
    Right,
    Down,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupSession {
    pub active_tab: usize,
    pub tabs: Vec<SessionTab>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSession {
    pub id: u64,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
    pub workspace_file: Option<PathBuf>,
    pub workspace_roots: Vec<PathBuf>,
    pub split: SplitLayout,
    pub active_group: usize,
    pub groups: Vec<GroupSession>,
}

impl Default for WindowSession {
    fn default() -> Self {
        Self {
            id: 1,
            x: None,
            y: None,
            width: 1200.0,
            height: 800.0,
            maximized: false,
            workspace_file: None,
            workspace_roots: Vec::new(),
            split: SplitLayout::None,
            active_group: 0,
            groups: vec![GroupSession::default()],
        }
    }
}

impl WindowSession {
    pub fn normalize(&mut self) {
        self.width = self.width.clamp(480.0, 10000.0);
        self.height = self.height.clamp(320.0, 10000.0);
        if self.groups.is_empty() {
            self.groups.push(GroupSession::default());
        }
        if self.split == SplitLayout::None || self.groups.len() == 1 {
            self.split = SplitLayout::None;
            self.active_group = 0;
            self.groups.truncate(1);
        } else {
            self.groups.truncate(2);
        }
        self.active_group = self.active_group.min(self.groups.len() - 1);
        for group in &mut self.groups {
            if group.active_tab >= group.tabs.len() {
                group.active_tab = group.tabs.len().saturating_sub(1);
            }
        }
    }

    pub fn tab_count(&self) -> usize {
        self.groups.iter().map(|group| group.tabs.len()).sum()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub version: u32,
    pub windows: Vec<WindowSession>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            version: SESSION_VERSION,
            windows: Vec::new(),
        }
    }
}

/// Version 1 layout, read only during migration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct LegacySession {
    tabs: Vec<SessionTab>,
    active_tab: usize,
    workspace_root: Option<PathBuf>,
    window_width: f32,
    window_height: f32,
}

impl LegacySession {
    fn migrate(self) -> WindowSession {
        let roots = self.workspace_root.into_iter().collect();
        let width = if self.window_width > 0.0 {
            self.window_width
        } else {
            1200.0
        };
        let height = if self.window_height > 0.0 {
            self.window_height
        } else {
            800.0
        };
        WindowSession {
            id: 1,
            width,
            height,
            workspace_roots: roots,
            groups: vec![GroupSession {
                active_tab: self.active_tab,
                tabs: self.tabs,
            }],
            ..WindowSession::default()
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
            Ok(json) => Self::from_json(&json).unwrap_or_else(|| {
                log::warn!("ignoring invalid session at {}", path.display());
                Self::default()
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                log::warn!("could not read session at {}: {err}", path.display());
                Self::default()
            }
        }
    }

    /// Parse a session document, migrating version 1 data. Returns `None` when
    /// the JSON cannot be understood at all.
    pub fn from_json(json: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(json).ok()?;
        let mut session = if value.get("windows").is_some() {
            serde_json::from_value::<Session>(value).ok()?
        } else {
            let legacy: LegacySession = serde_json::from_value(value).ok()?;
            Session {
                version: SESSION_VERSION,
                windows: vec![legacy.migrate()],
            }
        };
        session.version = SESSION_VERSION;
        session.normalize();
        Some(session)
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::atomic_write::write_atomic(path, json.as_bytes())
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    pub fn normalize(&mut self) {
        if self.windows.is_empty() {
            return;
        }
        for (index, window) in self.windows.iter_mut().enumerate() {
            if window.id == 0 {
                window.id = index as u64 + 1;
            }
            window.normalize();
        }
    }

    /// Build the v2 document around a single window.
    pub fn single(window: WindowSession) -> Self {
        let mut session = Self {
            version: SESSION_VERSION,
            windows: vec![window],
        };
        session.normalize();
        session
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(path: &str) -> SessionTab {
        SessionTab {
            path: Some(PathBuf::from(path)),
            text: Some("text".into()),
            cursor: 3,
            ..SessionTab::default()
        }
    }

    #[test]
    fn round_trips_multiple_windows_and_groups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let session = Session {
            version: SESSION_VERSION,
            windows: vec![
                WindowSession {
                    id: 1,
                    x: Some(100),
                    y: Some(50),
                    width: 900.0,
                    height: 700.0,
                    maximized: true,
                    workspace_file: Some(PathBuf::from("/w/a.luban-workspace")),
                    workspace_roots: vec![PathBuf::from("/w")],
                    split: SplitLayout::Right,
                    active_group: 1,
                    groups: vec![
                        GroupSession {
                            active_tab: 0,
                            tabs: vec![tab("/a.rs")],
                        },
                        GroupSession {
                            active_tab: 1,
                            tabs: vec![tab("/b.rs"), tab("/c.rs")],
                        },
                    ],
                },
                WindowSession {
                    id: 2,
                    split: SplitLayout::None,
                    groups: vec![GroupSession {
                        active_tab: 0,
                        tabs: vec![tab("/d.rs")],
                    }],
                    ..WindowSession::default()
                },
            ],
        };
        session.save_to(&path).unwrap();
        let loaded = Session::load_from(&path);
        assert_eq!(loaded.windows.len(), 2);
        assert_eq!(loaded.windows[0].split, SplitLayout::Right);
        assert_eq!(loaded.windows[0].active_group, 1);
        assert_eq!(loaded.windows[0].groups[1].tabs.len(), 2);
        assert!(loaded.windows[0].maximized);
        assert_eq!(loaded.windows[0].x, Some(100));
        assert_eq!(loaded.windows[1].id, 2);
    }

    #[test]
    fn migrates_version_one_sessions() {
        let legacy = r#"{
            "version": 1,
            "tabs": [
                {"path": "/a.rs", "cursor": 4, "text": "abc", "modified": true},
                {"path": null, "cursor": 0}
            ],
            "active_tab": 1,
            "workspace_root": "/w",
            "window_width": 1000,
            "window_height": 700
        }"#;
        let session = Session::from_json(legacy).unwrap();
        assert_eq!(session.version, SESSION_VERSION);
        assert_eq!(session.windows.len(), 1);
        let window = &session.windows[0];
        assert_eq!(window.workspace_roots, vec![PathBuf::from("/w")]);
        assert_eq!(window.width, 1000.0);
        assert_eq!(window.groups.len(), 1);
        assert_eq!(window.groups[0].active_tab, 1);
        assert_eq!(window.groups[0].tabs.len(), 2);
        assert_eq!(window.groups[0].tabs[0].text.as_deref(), Some("abc"));
    }

    #[test]
    fn clamps_groups_and_active_tabs() {
        let mut window = WindowSession {
            split: SplitLayout::Down,
            active_group: 9,
            groups: vec![
                GroupSession {
                    active_tab: 9,
                    tabs: vec![tab("/a")],
                },
                GroupSession::default(),
            ],
            ..WindowSession::default()
        };
        window.normalize();
        assert_eq!(window.active_group, 1);
        assert_eq!(window.groups[0].active_tab, 0);
        assert_eq!(window.width, 1200.0);

        let mut none = WindowSession {
            split: SplitLayout::None,
            groups: vec![GroupSession::default(), GroupSession::default()],
            ..WindowSession::default()
        };
        none.normalize();
        assert_eq!(none.groups.len(), 1);
        assert_eq!(none.split, SplitLayout::None);
    }

    #[test]
    fn clamps_window_size() {
        let mut session =
            Session::from_json(r#"{"version":2,"windows":[{"width":2,"height":999999}]}"#).unwrap();
        assert_eq!(session.windows[0].width, 480.0);
        assert_eq!(session.windows[0].height, 10000.0);
        session.normalize();
    }

    #[test]
    fn invalid_json_is_none() {
        assert!(Session::from_json("not json").is_none());
        assert!(Session::from_json(r#"{"windows": 3}"#).is_none());
    }

    #[test]
    fn missing_session_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = Session::load_from(&dir.path().join("none.json"));
        assert!(loaded.is_empty());
        assert_eq!(loaded.version, SESSION_VERSION);
    }
}

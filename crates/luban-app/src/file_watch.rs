//! Watch the directories of open files for external changes.
//!
//! Watching the parent directory (instead of the file inode) catches atomic
//! replacements, renames and deletions. Events are debounced so a burst of
//! writes produces one notice; the application then compares the recorded
//! metadata with the file on disk rather than trusting the event alone.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// How long events for one path are collected before the metadata is checked.
pub const DEBOUNCE: Duration = Duration::from_millis(500);
/// Events caused by our own save are ignored for this long.
const SELF_SAVE_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchEventKind {
    /// The file exists and needs a metadata comparison by the caller.
    Changed,
    /// The file no longer exists.
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchEvent {
    pub path: PathBuf,
    pub kind: WatchEventKind,
}

#[derive(Clone, Copy, Debug)]
struct IgnoredSave {
    size: u64,
    modified: Option<SystemTime>,
    until: Instant,
}

/// Paths registered by the caller plus the paths the backend reports for them.
#[derive(Default)]
struct TrackedPaths {
    /// Registered paths. Events are reported with these paths, not with the
    /// ones the backend uses.
    paths: HashSet<PathBuf>,
    /// Backend path -> registered path.
    ///
    /// Watchers resolve symbolic links in the watched directory: macOS
    /// FSEvents canonicalises its watch roots, so a document under `/var`
    /// arrives as `/private/var` and would never match the registered path.
    aliases: HashMap<PathBuf, PathBuf>,
}

impl TrackedPaths {
    fn replace(&mut self, paths: &[PathBuf]) {
        if self.paths.len() == paths.len() && paths.iter().all(|path| self.paths.contains(path)) {
            return;
        }
        self.paths.clear();
        self.aliases.clear();
        for path in paths {
            self.paths.insert(path.clone());
            if let Some(resolved) = resolved_path(path) {
                if resolved != *path {
                    self.aliases.insert(resolved, path.clone());
                }
            }
        }
    }

    fn registered(&self, reported: &Path) -> Option<&PathBuf> {
        self.paths
            .get(reported)
            .or_else(|| self.aliases.get(reported))
    }
}

/// The path a notification backend reports for `path`.
fn resolved_path(path: &Path) -> Option<PathBuf> {
    if let Ok(resolved) = fs::canonicalize(path) {
        return Some(resolved);
    }
    // A tracked file may be gone (a pending deletion); its directory still
    // resolves, so the backend's path can be predicted from it.
    let name = path.file_name()?;
    fs::canonicalize(path.parent()?)
        .ok()
        .map(|parent| parent.join(name))
}

/// Directory watcher shared by every open tab.
pub struct FileWatcher {
    watcher: Option<RecommendedWatcher>,
    watched: HashMap<PathBuf, usize>,
    tracked: Arc<Mutex<TrackedPaths>>,
    pending: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    ignored: HashMap<PathBuf, IgnoredSave>,
    debounce: Duration,
}

impl std::fmt::Debug for FileWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileWatcher")
            .field("watched", &self.watched.len())
            .finish()
    }
}

impl Default for FileWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl FileWatcher {
    pub fn new() -> Self {
        Self::with_debounce(DEBOUNCE)
    }

    pub fn with_debounce(debounce: Duration) -> Self {
        let tracked: Arc<Mutex<TrackedPaths>> = Arc::new(Mutex::new(TrackedPaths::default()));
        let pending: Arc<Mutex<HashMap<PathBuf, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
        let callback_tracked = tracked.clone();
        let callback_pending = pending.clone();
        let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            let Ok(event) = result else {
                return;
            };
            let tracked = match callback_tracked.lock() {
                Ok(tracked) => tracked,
                Err(_) => return,
            };
            let mut pending = match callback_pending.lock() {
                Ok(pending) => pending,
                Err(_) => return,
            };
            for path in event.paths {
                if let Some(registered) = tracked.registered(&path) {
                    pending.insert(registered.clone(), Instant::now());
                }
            }
        })
        .map_err(|error| {
            log::warn!("file watching is unavailable: {error}");
            error
        })
        .ok();
        Self {
            watcher,
            watched: HashMap::new(),
            tracked,
            pending,
            ignored: HashMap::new(),
            debounce,
        }
    }

    /// Keep exactly the parent directories of `paths` watched.
    pub fn sync(&mut self, paths: &[PathBuf]) {
        if let Ok(mut tracked) = self.tracked.lock() {
            tracked.replace(paths);
        }
        let mut desired: HashMap<PathBuf, usize> = HashMap::new();
        for path in paths {
            if let Some(parent) = path.parent() {
                *desired.entry(parent.to_path_buf()).or_insert(0) += 1;
            }
        }
        let unwatch: Vec<PathBuf> = self
            .watched
            .keys()
            .filter(|dir| !desired.contains_key(*dir))
            .cloned()
            .collect();
        for dir in unwatch {
            self.watched.remove(&dir);
            if let Some(watcher) = self.watcher.as_mut() {
                let _ = watcher.unwatch(&dir);
            }
        }
        let watch: Vec<PathBuf> = desired
            .keys()
            .filter(|dir| !self.watched.contains_key(*dir))
            .cloned()
            .collect();
        for dir in watch {
            let Some(watcher) = self.watcher.as_mut() else {
                break;
            };
            match watcher.watch(&dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watched.insert(dir, 1);
                }
                Err(error) => log::warn!("could not watch {}: {error}", dir.display()),
            }
        }
        if let Ok(mut pending) = self.pending.lock() {
            pending.retain(|path, _| paths.iter().any(|tracked| tracked == path));
        }
        self.ignored
            .retain(|path, _| paths.iter().any(|tracked| tracked == path));
    }

    /// Rewrite tracked paths after a rename. The next `sync` call confirms the
    /// new set from the tab list.
    pub fn remap(&mut self, from: &Path, to: &Path) {
        if let Ok(mut tracked) = self.tracked.lock() {
            tracked.paths = tracked
                .paths
                .drain()
                .map(|path| {
                    if path == from {
                        to.to_path_buf()
                    } else if let Ok(suffix) = path.strip_prefix(from) {
                        to.join(suffix)
                    } else {
                        path
                    }
                })
                .collect();
            tracked.aliases.clear();
        }
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
        self.ignored.clear();
    }

    /// Remember the metadata produced by our own save so the resulting event
    /// does not become an external-change notice.
    pub fn note_saved(&mut self, path: &Path, size: u64, modified: Option<SystemTime>) {
        self.ignored.insert(
            path.to_path_buf(),
            IgnoredSave {
                size,
                modified,
                until: Instant::now() + SELF_SAVE_GRACE,
            },
        );
    }

    /// Return events whose debounce window has elapsed.
    pub fn poll(&mut self) -> Vec<WatchEvent> {
        let now = Instant::now();
        let due: Vec<PathBuf> = match self.pending.lock() {
            Ok(mut pending) => {
                let due: Vec<PathBuf> = pending
                    .iter()
                    .filter(|(_, at)| now.duration_since(**at) >= self.debounce)
                    .map(|(path, _)| path.clone())
                    .collect();
                for path in &due {
                    pending.remove(path);
                }
                due
            }
            Err(_) => return Vec::new(),
        };
        let tracked: HashSet<PathBuf> = self
            .tracked
            .lock()
            .map(|tracked| tracked.paths.clone())
            .unwrap_or_default();
        let mut events = Vec::new();
        for path in due {
            if !tracked.contains(&path) {
                continue;
            }
            if let Some(ignored) = self.ignored.get(&path) {
                if now < ignored.until && self.save_matches(&path, ignored) {
                    continue;
                }
                if now >= ignored.until {
                    self.ignored.remove(&path);
                }
            }
            match fs::metadata(&path) {
                Ok(_) => events.push(WatchEvent {
                    path,
                    kind: WatchEventKind::Changed,
                }),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    events.push(WatchEvent {
                        path,
                        kind: WatchEventKind::Removed,
                    });
                }
                // An unreadable file is treated as changed; saving still
                // performs its own preflight check.
                Err(_) => events.push(WatchEvent {
                    path,
                    kind: WatchEventKind::Changed,
                }),
            }
        }
        events
    }

    fn save_matches(&self, path: &Path, ignored: &IgnoredSave) -> bool {
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        metadata.len() == ignored.size && metadata.modified().ok() == ignored.modified
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for_event(watcher: &mut FileWatcher, path: &Path) -> Option<WatchEvent> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            for event in watcher.poll() {
                if event.path == path {
                    return Some(event);
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        None
    }

    #[cfg(unix)]
    #[test]
    fn resolved_backend_paths_map_back_to_registered_paths() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("note.txt"), "one").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let registered = link.join("note.txt");
        let mut tracked = TrackedPaths::default();
        tracked.replace(std::slice::from_ref(&registered));

        // macOS FSEvents reports the resolved path of the watched directory.
        let reported = resolved_path(&registered).unwrap();
        assert_ne!(reported, registered);
        assert_eq!(tracked.registered(&reported), Some(&registered));
    }

    #[test]
    fn reports_an_external_modification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, "one").unwrap();
        let mut watcher = FileWatcher::with_debounce(Duration::from_millis(10));
        watcher.sync(std::slice::from_ref(&path));
        fs::write(&path, "two, longer").unwrap();
        let event = wait_for_event(&mut watcher, &path).expect("modification event");
        assert_eq!(event.kind, WatchEventKind::Changed);
    }

    #[test]
    fn reports_a_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, "one").unwrap();
        let mut watcher = FileWatcher::with_debounce(Duration::from_millis(10));
        watcher.sync(std::slice::from_ref(&path));
        fs::remove_file(&path).unwrap();
        let event = wait_for_event(&mut watcher, &path).expect("deletion event");
        assert_eq!(event.kind, WatchEventKind::Removed);
    }

    #[test]
    fn ignores_the_application_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, "one").unwrap();
        let mut watcher = FileWatcher::with_debounce(Duration::from_millis(10));
        watcher.sync(std::slice::from_ref(&path));
        fs::write(&path, "saved content").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        watcher.note_saved(&path, metadata.len(), metadata.modified().ok());
        std::thread::sleep(Duration::from_millis(30));
        assert!(watcher.poll().is_empty());
    }
}

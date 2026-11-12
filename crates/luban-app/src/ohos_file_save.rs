//! Host-provided writer for files outside the OpenHarmony application sandbox.
//!
//! The system document picker grants access to a selected file through its
//! `file://docs/...` URI, not through the sandbox path. POSIX writes to that
//! path fail with `EACCES`, so the host installs a writer that opens the URI
//! through ArkTS `fs` and writes the bytes there. Only user-storage paths take
//! this route; sandbox files keep the normal atomic save.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::OnceLock;

/// A host write that completes on the GPUI foreground executor.
pub type WriteFuture = Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>;

/// Opens `path` by URI and writes `bytes`.
pub type FileWriter = Box<dyn Fn(PathBuf, Vec<u8>) -> WriteFuture + Send + Sync>;

static WRITER: OnceLock<FileWriter> = OnceLock::new();

/// Install the host writer once, during ability initialization.
pub fn set_writer(writer: FileWriter) -> bool {
    WRITER.set(writer).is_ok()
}

pub(crate) fn writer() -> Option<&'static FileWriter> {
    WRITER.get()
}

/// Paths returned by the system picker live outside the app sandbox.
///
/// The sandbox lives under `/data/storage` and `/data/app`; user files are
/// exposed under `/storage/Users` (and media under `/storage/media`).
pub(crate) fn is_user_path(path: &Path) -> bool {
    path.starts_with("/storage/Users") || path.starts_with("/storage/media")
}

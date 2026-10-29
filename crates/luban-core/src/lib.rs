//! Core document engine for Luban Notepad.
//!
//! The crate is deliberately free of GUI dependencies so that every piece of
//! editor behaviour (encoding, line endings, undo, search, sessions, settings,
//! project-wide search) can be unit-tested headlessly and reused by other
//! frontends.

rust_i18n::i18n!("locales", fallback = "en");

pub mod atomic_write;
pub mod bookmarks;
pub mod disk_engine;
pub mod document;
pub mod encoding;
pub mod eol;
pub mod error;
pub mod export;
pub mod i18n;
pub mod language;
pub mod large_file;
pub mod line_ops;
pub mod local_history;
pub mod macros;
pub mod search;
pub mod session;
pub mod settings;
pub mod text;
pub mod undo;
pub mod workspace;

pub use document::{DiskStatus, Document};
pub use encoding::TextEncoding;
pub use eol::Eol;
pub use error::{Error, Result};
pub use search::{Match, SearchEngine, SearchOptions};
pub use session::{Session, SessionTab};
pub use settings::Settings;
pub use undo::UndoStack;

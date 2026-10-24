use std::fmt;
use std::path::PathBuf;

use crate::i18n::tr_args;

/// Errors produced by the core engine.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),

    InvalidPath(PathBuf),

    FileTooLarge { size: u64, limit: u64 },

    Encoding(String),

    Serialization(serde_json::Error),

    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => {
                f.write_str(&tr_args("core.error.io", &[("error", error.to_string())]))
            }
            Self::InvalidPath(path) => f.write_str(&tr_args(
                "core.error.invalid_path",
                &[("path", path.display().to_string())],
            )),
            Self::FileTooLarge { size, limit } => f.write_str(&tr_args(
                "core.error.file_too_large",
                &[("size", size.to_string()), ("limit", limit.to_string())],
            )),
            Self::Encoding(error) => {
                f.write_str(&tr_args("core.error.encoding", &[("error", error.clone())]))
            }
            Self::Serialization(error) => f.write_str(&tr_args(
                "core.error.serialization",
                &[("error", error.to_string())],
            )),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

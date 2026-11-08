//! Text preview sampling for the file picker.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use luban_core::encoding::{decode, looks_binary};
use luban_core::eol;
use luban_core::i18n::language_display_name;
use luban_core::language;

/// Bytes read from the beginning of a file for a preview.
pub const PREVIEW_BYTES: u64 = 64 * 1024;
/// Maximum number of lines rendered in the preview pane.
pub const PREVIEW_LINES: usize = 200;
/// Number of recently previewed files kept without re-reading them.
const PREVIEW_CACHE_ENTRIES: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub struct PreviewText {
    pub name: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub language: String,
    pub encoding: String,
    pub eol: String,
    pub lines: Vec<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreviewContent {
    Text(PreviewText),
    Binary { size: u64 },
}

/// What the preview pane shows right now.
#[derive(Clone, Debug, PartialEq)]
pub enum PreviewKind {
    None,
    Loading,
    Folder {
        name: String,
        path: PathBuf,
        items: Option<usize>,
    },
    Multiple {
        count: usize,
        total_size: u64,
    },
    Text(PreviewText),
    Binary {
        size: u64,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug)]
struct CachedPreview {
    path: PathBuf,
    size: Option<u64>,
    modified: Option<SystemTime>,
    content: PreviewContent,
}

#[derive(Clone, Debug, Default)]
pub struct PreviewState {
    pub kind: Option<PreviewKind>,
    pub generation: u64,
    cache: Vec<CachedPreview>,
}

impl PreviewState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cached(
        &self,
        path: &Path,
        size: Option<u64>,
        modified: Option<SystemTime>,
    ) -> Option<PreviewContent> {
        self.cache
            .iter()
            .find(|entry| entry.path == path && entry.size == size && entry.modified == modified)
            .map(|entry| entry.content.clone())
    }

    pub fn store(
        &mut self,
        path: PathBuf,
        size: Option<u64>,
        modified: Option<SystemTime>,
        content: PreviewContent,
    ) {
        self.cache.retain(|entry| {
            !(entry.path == path && entry.size == size && entry.modified == modified)
        });
        if self.cache.len() >= PREVIEW_CACHE_ENTRIES {
            self.cache.remove(0);
        }
        self.cache.push(CachedPreview {
            path,
            size,
            modified,
            content,
        });
    }

    pub fn clear_current(&mut self) {
        self.kind = None;
    }
}

pub fn load_preview(
    path: &Path,
    size: Option<u64>,
    modified: Option<SystemTime>,
) -> Result<PreviewContent, String> {
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(PREVIEW_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(preview_from_bytes(path, size, modified, bytes))
}

/// Build preview content from an already-read sample. Kept separate so the
/// decoding and truncation rules are unit-testable without touching disk.
pub fn preview_from_bytes(
    path: &Path,
    size: Option<u64>,
    modified: Option<SystemTime>,
    bytes: Vec<u8>,
) -> PreviewContent {
    let size = size.unwrap_or(bytes.len() as u64);
    if looks_binary(&bytes) {
        return PreviewContent::Binary { size };
    }
    let decoded = decode(&bytes);
    let line_count = decoded.text.lines().count();
    let truncated = line_count > PREVIEW_LINES || bytes.len() as u64 >= PREVIEW_BYTES;
    let lines = decoded
        .text
        .lines()
        .take(PREVIEW_LINES)
        .map(|line| line.replace('\t', "    "))
        .collect();
    let language = language_display_name(language::detect_from_path(path));
    PreviewContent::Text(PreviewText {
        name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        size,
        modified,
        language,
        encoding: decoded.encoding.short_label().to_string(),
        eol: eol::detect(&decoded.text).label().to_string(),
        lines,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_bytes(lines: usize) -> Vec<u8> {
        (0..lines)
            .map(|index| format!("line {index}\n"))
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn text_preview_reports_encoding_language_and_eol() {
        let path = Path::new("/tmp/notes.md");
        let content = preview_from_bytes(path, Some(12), None, b"# title\nbody\n".to_vec());
        let PreviewContent::Text(text) = content else {
            panic!("expected text preview");
        };
        assert_eq!(text.lines, vec!["# title", "body"]);
        assert_eq!(text.encoding, "UTF-8");
        assert_eq!(text.eol, "LF");
        assert_eq!(text.language, "Markdown");
        assert!(!text.truncated);
    }

    #[test]
    fn binary_samples_are_not_decoded() {
        let content = preview_from_bytes(
            Path::new("/tmp/image.png"),
            Some(4),
            None,
            vec![0x89, 0x50, 0x00, 0x47],
        );
        assert_eq!(content, PreviewContent::Binary { size: 4 });
    }

    #[test]
    fn long_text_is_truncated_to_the_line_limit() {
        let content = preview_from_bytes(
            Path::new("/tmp/long.txt"),
            None,
            None,
            text_bytes(PREVIEW_LINES + 25),
        );
        let PreviewContent::Text(text) = content else {
            panic!("expected text preview");
        };
        assert_eq!(text.lines.len(), PREVIEW_LINES);
        assert!(text.truncated);
    }

    #[test]
    fn cache_entries_require_the_same_metadata() {
        let path = PathBuf::from("/tmp/a.txt");
        let content = PreviewContent::Binary { size: 1 };
        let mut state = PreviewState::new();
        state.store(path.clone(), Some(1), None, content.clone());
        assert_eq!(state.cached(&path, Some(1), None), Some(content));
        assert_eq!(state.cached(&path, Some(2), None), None);
    }
}

//! Large-file range reading and the range math used by the controller.
//!
//! The controller opens a byte range instead of the whole file. Ranges are
//! aligned to line boundaries before decoding so a slice never starts or ends
//! in the middle of a character or a line.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::encoding::{self, TextEncoding};
use crate::eol::{self, Eol};
use crate::error::{Error, Result};
use crate::language;

/// Bytes sampled from the beginning of a file to detect its encoding.
pub const HEAD_SAMPLE_BYTES: u64 = 64 * 1024;
/// Size of the chunks used for background reads.
pub const READ_CHUNK_BYTES: usize = 1024 * 1024;
/// How far to look for a line boundary outside the requested range.
pub const BOUNDARY_WINDOW_BYTES: u64 = 1024 * 1024;

/// Unit used to display byte counts in the controller. Input fields always
/// take bytes; the unit only affects labels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SizeUnit {
    #[default]
    Bytes,
    Kib,
    Mib,
    Gib,
}

impl SizeUnit {
    pub const ALL: [SizeUnit; 4] = [Self::Bytes, Self::Kib, Self::Mib, Self::Gib];

    pub fn factor(self) -> u64 {
        match self {
            Self::Bytes => 1,
            Self::Kib => 1024,
            Self::Mib => 1024 * 1024,
            Self::Gib => 1024 * 1024 * 1024,
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Bytes => "B",
            Self::Kib => "KiB",
            Self::Mib => "MiB",
            Self::Gib => "GiB",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Bytes => "lfc.unit.bytes",
            Self::Kib => "lfc.unit.kib",
            Self::Mib => "lfc.unit.mib",
            Self::Gib => "lfc.unit.gib",
        }
    }
}

/// Which part of the file the controller opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RangeMode {
    #[default]
    Whole,
    Beginning,
    End,
    Middle,
}

impl RangeMode {
    pub fn key(self) -> &'static str {
        match self {
            Self::Whole => "lfc.mode.whole",
            Self::Beginning => "lfc.mode.beginning",
            Self::End => "lfc.mode.end",
            Self::Middle => "lfc.mode.middle",
        }
    }
}

/// A validated byte range for the controller.
///
/// The selection keeps `from`, `to` (exclusive) and `size` consistent and
/// applies the rule documented in the controller: changing one field keeps the
/// others valid instead of rejecting the edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeSelection {
    total: u64,
    default_size: u64,
    mode: RangeMode,
    from: u64,
    to: u64,
}

impl RangeSelection {
    pub fn new(total: u64, default_size: u64) -> Self {
        let default_size = default_size.max(1);
        let mut selection = Self {
            total,
            default_size,
            mode: RangeMode::Whole,
            from: 0,
            to: total,
        };
        selection.apply_mode();
        selection
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn default_size(&self) -> u64 {
        self.default_size
    }

    pub fn mode(&self) -> RangeMode {
        self.mode
    }

    pub fn from(&self) -> u64 {
        self.from
    }

    /// Exclusive end offset.
    pub fn to(&self) -> u64 {
        self.to
    }

    pub fn size(&self) -> u64 {
        self.to.saturating_sub(self.from)
    }

    /// Point the selection at another size, keeping the current mode.
    pub fn set_total(&mut self, total: u64, default_size: u64) {
        self.total = total;
        self.default_size = default_size.max(1);
        self.apply_mode();
    }

    pub fn set_mode(&mut self, mode: RangeMode) {
        self.mode = mode;
        self.apply_mode();
    }

    fn apply_mode(&mut self) {
        let total = self.total;
        match self.mode {
            RangeMode::Whole => {
                self.from = 0;
                self.to = total;
            }
            RangeMode::Beginning => {
                self.from = 0;
                self.to = total.min(self.default_size);
            }
            RangeMode::End => {
                let size = total.min(self.default_size);
                self.from = total - size;
                self.to = total;
            }
            RangeMode::Middle => {
                let size = total.min(self.default_size);
                self.from = (total - size) / 2;
                self.to = self.from + size;
            }
        }
    }

    /// Set the start offset. The end stays valid; if it would fall at or before
    /// the new start, the default size is used from the new start.
    pub fn set_from(&mut self, from: u64) -> Result<()> {
        if self.total == 0 {
            return Err(self.invalid_range());
        }
        let from = from.min(self.total - 1);
        if from >= self.to {
            let to = from.saturating_add(self.default_size).min(self.total);
            self.to = to.max(from + 1);
        }
        self.from = from;
        Ok(())
    }

    /// Set the exclusive end offset.
    pub fn set_to(&mut self, to: u64) -> Result<()> {
        if self.total == 0 || self.from >= self.total {
            return Err(self.invalid_range());
        }
        self.to = to.clamp(self.from + 1, self.total);
        Ok(())
    }

    /// Set the range length, moving the end.
    pub fn set_size(&mut self, size: u64) -> Result<()> {
        if self.total == 0 || self.from >= self.total {
            return Err(self.invalid_range());
        }
        let size = size.clamp(1, self.total - self.from);
        self.to = self.from + size;
        Ok(())
    }

    /// Set an arbitrary range. Used when restoring a saved partial range or
    /// when the source file changed size while the controller was open.
    pub fn set_range(&mut self, from: u64, to: u64) -> Result<()> {
        if self.total == 0 || from >= to || to > self.total {
            return Err(self.invalid_range());
        }
        self.from = from;
        self.to = to;
        Ok(())
    }

    /// Whether the selection can be loaded with `max_len` as the size limit.
    pub fn validate(&self, max_len: u64) -> Result<()> {
        if self.total == 0 || self.from >= self.to || self.to > self.total {
            return Err(self.invalid_range());
        }
        if self.size() > max_len {
            return Err(Error::FileTooLarge {
                size: self.size(),
                limit: max_len,
            });
        }
        Ok(())
    }

    fn invalid_range(&self) -> Error {
        Error::InvalidRange {
            start: self.from,
            end: self.to,
            size: self.total,
        }
    }
}

/// Decoded content of a byte range.
#[derive(Debug)]
pub struct RangeContent {
    pub text: String,
    pub encoding: TextEncoding,
    pub eol: Eol,
    pub non_text: bool,
    /// Actual first byte offset after line-boundary alignment.
    pub start: u64,
    /// Actual exclusive end offset after line-boundary alignment.
    pub end: u64,
    pub total_size: u64,
    pub last_modified: Option<SystemTime>,
}

/// Read and decode `from..to` of a file.
///
/// The requested range is clamped to the file and expanded to complete lines
/// where possible. The actual range never exceeds `max_len` bytes. `progress`
/// receives the number of bytes read so far. A set `cancel` stops the read
/// with [`Error::Cancelled`].
pub fn read_range(
    path: &Path,
    from: u64,
    to: u64,
    max_len: u64,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> Result<RangeContent> {
    let metadata = fs::metadata(path)?;
    if metadata.is_dir() {
        return Err(Error::InvalidPath(path.to_path_buf()));
    }
    let total = metadata.len();
    if total == 0 || from >= total {
        return Err(Error::InvalidRange {
            start: from,
            end: to,
            size: total,
        });
    }
    let to = to.min(total);
    if from >= to {
        return Err(Error::InvalidRange {
            start: from,
            end: to,
            size: total,
        });
    }

    let (encoding, non_text) = detect_head(path, total)?;
    let last_modified = metadata.modified().ok();
    // UTF-16 offsets must start on a code-unit boundary. Round an odd start
    // up (the byte belongs to a unit that may be a newline) and an odd end up.
    let (mut from, mut to) = (from, to);
    if encoding.is_utf16() {
        if !from.is_multiple_of(2) {
            from = (from + 1).min(total);
        }
        if !to.is_multiple_of(2) {
            to = (to + 1).min(total);
        }
        if from >= to || from >= total {
            return Err(Error::InvalidRange {
                start: from,
                end: to,
                size: total,
            });
        }
    }

    if non_text {
        let end = to.min(from.saturating_add(max_len.max(1)));
        let bytes = read_bytes(path, from, end - from, cancel, progress)?;
        let actual_end = from + bytes.len() as u64;
        let decoded = encoding::decode_lossless(bytes);
        let detected_eol = eol::detect(&decoded.text);
        return Ok(RangeContent {
            text: decoded.text,
            encoding: decoded.encoding,
            eol: detected_eol,
            non_text: true,
            start: from,
            end: actual_end,
            total_size: total,
            last_modified,
        });
    }

    let window_start = from.saturating_sub(BOUNDARY_WINDOW_BYTES);
    let window_end = to.saturating_add(BOUNDARY_WINDOW_BYTES).min(total);
    let window = read_bytes(
        path,
        window_start,
        window_end - window_start,
        cancel,
        progress,
    )?;
    let available_end = window_start + window.len() as u64;

    let scanner = NewlineScanner::new(encoding);
    let mut start = from;
    let mut end = to.min(available_end);
    if from > 0 {
        match scanner.find_backward(&window, (from - window_start) as usize) {
            Some(offset) => start = window_start + offset as u64,
            // No preceding newline and the window starts at byte 0 means the
            // requested offset is inside the first line.
            None if window_start == 0 => start = 0,
            None => {}
        }
    }
    if to < total && !scanner.is_line_start(&window, (to - window_start) as usize) {
        if let Some(offset) = scanner.find_forward(&window, (to - window_start) as usize) {
            end = end.max((window_start + offset as u64).min(available_end));
        }
    }
    if start >= end {
        start = from;
        end = to.min(available_end);
    }
    if end - start > max_len {
        end = start + max_len;
        // Prefer dropping the last, partially loaded line over splitting it.
        if let Some(offset) = scanner.find_backward(&window, (end - window_start) as usize) {
            let aligned = window_start + offset as u64;
            if aligned > start {
                end = aligned;
            }
        }
    }

    let slice_start = (start - window_start) as usize;
    let slice_end = (end - window_start) as usize;
    let slice = window.get(slice_start..slice_end).unwrap_or(&[]).to_vec();
    let text = encoding::decode_with(&slice, encoding);
    let detected_eol = eol::detect(&text);
    let text = eol::to_lf_owned(text);
    Ok(RangeContent {
        text,
        encoding,
        eol: detected_eol,
        non_text: false,
        start,
        end: start + slice.len() as u64,
        total_size: total,
        last_modified,
    })
}

/// Read a whole file in chunks so the UI can show progress and cancel.
pub fn read_whole(
    path: &Path,
    max_size: u64,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> Result<Vec<u8>> {
    let metadata = fs::metadata(path)?;
    if metadata.is_dir() {
        return Err(Error::InvalidPath(path.to_path_buf()));
    }
    let len = metadata.len();
    if len > max_size {
        return Err(Error::FileTooLarge {
            size: len,
            limit: max_size,
        });
    }
    let mut file = File::open(path)?;
    let capacity = usize::try_from(len).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    let mut buffer = vec![0u8; READ_CHUNK_BYTES];
    let mut read = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        read += count as u64;
        progress.store(read, Ordering::Relaxed);
    }
    progress.store(read, Ordering::Relaxed);
    Ok(bytes)
}

fn detect_head(path: &Path, total: u64) -> Result<(TextEncoding, bool)> {
    let mut file = File::open(path)?;
    let mut head = vec![0u8; total.min(HEAD_SAMPLE_BYTES) as usize];
    let mut filled = 0;
    while filled < head.len() {
        let count = file.read(&mut head[filled..])?;
        if count == 0 {
            break;
        }
        filled += count;
    }
    head.truncate(filled);
    let non_text = encoding::looks_binary(&head);
    let detected = if non_text {
        TextEncoding::Windows1252
    } else {
        encoding::decode(&head).encoding
    };
    Ok((detected, non_text))
}

fn read_bytes(
    path: &Path,
    offset: u64,
    len: u64,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let capacity = usize::try_from(len).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    let mut buffer = vec![0u8; READ_CHUNK_BYTES];
    let mut remaining = len;
    let mut read = 0u64;
    while remaining > 0 {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let want = remaining.min(buffer.len() as u64) as usize;
        let count = file.read(&mut buffer[..want])?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        remaining -= count as u64;
        read += count as u64;
        progress.store(read, Ordering::Relaxed);
    }
    Ok(bytes)
}

/// Finds the byte offset just after the first line terminator at or after a
/// position. UTF-16 files use two-byte code units.
struct NewlineScanner {
    utf16: Option<bool>,
}

impl NewlineScanner {
    fn new(encoding: TextEncoding) -> Self {
        let utf16 = match encoding {
            TextEncoding::Utf16Le => Some(true),
            TextEncoding::Utf16Be => Some(false),
            _ => None,
        };
        Self { utf16 }
    }

    fn find_forward(&self, bytes: &[u8], from: usize) -> Option<usize> {
        match self.utf16 {
            None => {
                let mut index = from;
                while index < bytes.len() {
                    match bytes[index] {
                        b'\n' => return Some(index + 1),
                        b'\r' => {
                            return Some(if bytes.get(index + 1) == Some(&b'\n') {
                                index + 2
                            } else {
                                index + 1
                            });
                        }
                        _ => index += 1,
                    }
                }
                None
            }
            Some(little_endian) => {
                let mut index = if from.is_multiple_of(2) {
                    from
                } else {
                    from + 1
                };
                while index + 1 < bytes.len() {
                    let unit = if little_endian {
                        u16::from_le_bytes([bytes[index], bytes[index + 1]])
                    } else {
                        u16::from_be_bytes([bytes[index], bytes[index + 1]])
                    };
                    if unit == b'\n' as u16 {
                        return Some(index + 2);
                    }
                    if unit == b'\r' as u16 {
                        let next = index + 2;
                        if next + 1 < bytes.len() {
                            let next_unit = if little_endian {
                                u16::from_le_bytes([bytes[next], bytes[next + 1]])
                            } else {
                                u16::from_be_bytes([bytes[next], bytes[next + 1]])
                            };
                            if next_unit == b'\n' as u16 {
                                return Some(index + 4);
                            }
                        }
                        return Some(index + 2);
                    }
                    index += 2;
                }
                None
            }
        }
    }

    /// Whether `offset` is already the first byte of a line.
    fn is_line_start(&self, bytes: &[u8], offset: usize) -> bool {
        if offset == 0 {
            return true;
        }
        match self.utf16 {
            None => match bytes.get(offset - 1) {
                Some(b'\n') => true,
                // Between the CR and LF of a CRLF pair the line is not done.
                Some(b'\r') => bytes.get(offset) != Some(&b'\n'),
                _ => false,
            },
            Some(little_endian) => {
                if offset < 2 {
                    return true;
                }
                let mut index = offset - 2;
                let mut checks = 0;
                while checks < 2 && index + 1 < bytes.len() {
                    let unit = if little_endian {
                        u16::from_le_bytes([bytes[index], bytes[index + 1]])
                    } else {
                        u16::from_be_bytes([bytes[index], bytes[index + 1]])
                    };
                    if unit == b'\n' as u16 || unit == b'\r' as u16 {
                        return true;
                    }
                    if index < 2 {
                        break;
                    }
                    index -= 2;
                    checks += 1;
                }
                false
            }
        }
    }

    /// Offset of the start of the line containing `before`, or `None` when no
    /// terminator exists in the window.
    fn find_backward(&self, bytes: &[u8], before: usize) -> Option<usize> {
        let before = before.min(bytes.len());
        match self.utf16 {
            None => {
                let mut index = before;
                while index > 0 {
                    index -= 1;
                    match bytes[index] {
                        b'\n' => return Some(index + 1),
                        b'\r' => {
                            return Some(if bytes.get(index + 1) == Some(&b'\n') {
                                index + 2
                            } else {
                                index + 1
                            });
                        }
                        _ => {}
                    }
                }
                None
            }
            Some(little_endian) => {
                let mut index = before & !1;
                while index >= 2 {
                    index -= 2;
                    let unit = if little_endian {
                        u16::from_le_bytes([bytes[index], bytes[index + 1]])
                    } else {
                        u16::from_be_bytes([bytes[index], bytes[index + 1]])
                    };
                    if unit == b'\n' as u16 {
                        return Some(index + 2);
                    }
                    if unit == b'\r' as u16 {
                        let next = index + 2;
                        if next + 1 < bytes.len() {
                            let next_unit = if little_endian {
                                u16::from_le_bytes([bytes[next], bytes[next + 1]])
                            } else {
                                u16::from_be_bytes([bytes[next], bytes[next + 1]])
                            };
                            if next_unit == b'\n' as u16 {
                                return Some(index + 4);
                            }
                        }
                        return Some(index + 2);
                    }
                }
                None
            }
        }
    }
}

/// Why a set of controller fields cannot be applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeInputError {
    StartNotBeforeEnd,
    ZeroSize,
}

/// Validate the three controller fields. This is separate from
/// [`RangeSelection::validate`] because it checks the raw text values the user
/// is about to apply, before they are normalized into a selection.
pub fn validate_range_inputs(
    from: u64,
    to: u64,
    size: u64,
) -> std::result::Result<(), RangeInputError> {
    if from >= to {
        return Err(RangeInputError::StartNotBeforeEnd);
    }
    if size == 0 {
        return Err(RangeInputError::ZeroSize);
    }
    Ok(())
}

/// Parse a byte count typed by the user. Commas, underscores and spaces are
/// accepted as grouping separators.
pub fn parse_byte_count(input: &str) -> Option<u64> {
    let mut value = 0u64;
    let mut digits = 0usize;
    for ch in input.chars() {
        match ch {
            '0'..='9' => {
                value = value
                    .checked_mul(10)?
                    .checked_add(u64::from(ch as u8 - b'0'))?;
                digits += 1;
            }
            ',' | '_' | ' ' | '\t' => {}
            _ => return None,
        }
    }
    (digits > 0).then_some(value)
}

/// Group a number with thousands separators.
pub fn format_number(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let offset = digits.len() % 3;
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (index + 3 - offset).is_multiple_of(3) && index >= offset {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Format a byte count using the selected unit.
pub fn format_bytes(value: u64, unit: SizeUnit) -> String {
    if unit == SizeUnit::Bytes {
        return format_number(value);
    }
    let scaled = value as f64 / unit.factor() as f64;
    let mut text = format!("{scaled:.2}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    format!("{text} {}", unit.short_label())
}

/// File name suggested for saving an opened portion.
pub fn partial_save_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "part".to_string());
    match path.extension() {
        Some(extension) if !extension.is_empty() => {
            format!("{stem}-part.{}", extension.to_string_lossy())
        }
        _ => format!("{stem}-part.txt"),
    }
}

/// Language name for a range opened from `path`.
pub fn language_for(path: &Path) -> String {
    language::detect_from_path(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(contents: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    fn read(path: &Path, from: u64, to: u64, max: u64) -> RangeContent {
        read_range(
            path,
            from,
            to,
            max,
            &AtomicBool::new(false),
            &AtomicU64::new(0),
        )
        .unwrap()
    }

    #[test]
    fn byte_counts_parse_and_format() {
        assert_eq!(parse_byte_count("261,510,797"), Some(261_510_797));
        assert_eq!(parse_byte_count("1_000"), Some(1000));
        assert_eq!(parse_byte_count(" 12 345 "), Some(12345));
        assert_eq!(parse_byte_count(""), None);
        assert_eq!(parse_byte_count("12x"), None);
        assert_eq!(parse_byte_count("99999999999999999999999"), None);
        assert_eq!(format_number(261_510_797), "261,510,797");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_bytes(1024, SizeUnit::Bytes), "1,024");
        assert_eq!(format_bytes(1024, SizeUnit::Kib), "1 KiB");
        assert_eq!(format_bytes(1024 * 1024 * 5 / 2, SizeUnit::Mib), "2.5 MiB");
    }

    #[test]
    fn modes_fill_default_ranges() {
        let mut selection = RangeSelection::new(1000, 100);
        assert_eq!((selection.from(), selection.to()), (0, 1000), "whole file");
        selection.set_mode(RangeMode::Beginning);
        assert_eq!((selection.from(), selection.to()), (0, 100));
        selection.set_mode(RangeMode::End);
        assert_eq!((selection.from(), selection.to()), (900, 1000));
        selection.set_mode(RangeMode::Middle);
        assert_eq!((selection.from(), selection.to()), (450, 550));
        selection.set_mode(RangeMode::Whole);
        assert_eq!((selection.from(), selection.to()), (0, 1000));
    }

    #[test]
    fn fields_stay_consistent() {
        let mut selection = RangeSelection::new(1000, 100);
        selection.set_from(950).unwrap();
        assert_eq!((selection.from(), selection.to()), (950, 1000));
        selection.set_size(20).unwrap();
        assert_eq!((selection.from(), selection.to()), (950, 970));
        selection.set_to(990).unwrap();
        assert_eq!((selection.from(), selection.to()), (950, 990));
        assert_eq!(selection.size(), 40);
        selection.set_from(999).unwrap();
        assert_eq!((selection.from(), selection.to()), (999, 1000));
        assert!(selection.validate(100).is_ok());
        assert!(selection.validate(0).is_err());
    }

    #[test]
    fn arbitrary_ranges_validate() {
        let mut selection = RangeSelection::new(1000, 100);
        selection.set_range(200, 800).unwrap();
        assert_eq!((selection.from(), selection.to()), (200, 800));
        assert!(selection.set_range(800, 800).is_err());
        assert!(selection.set_range(0, 1001).is_err());
        assert!(selection.set_range(0, 1000).is_ok());
    }

    #[test]
    fn controller_inputs_reject_zero_and_reversed_ranges() {
        assert_eq!(validate_range_inputs(0, 100, 100), Ok(()));
        assert_eq!(
            validate_range_inputs(0, 100, 0),
            Err(RangeInputError::ZeroSize)
        );
        assert_eq!(
            validate_range_inputs(100, 100, 1),
            Err(RangeInputError::StartNotBeforeEnd)
        );
        assert_eq!(
            validate_range_inputs(200, 100, 1),
            Err(RangeInputError::StartNotBeforeEnd)
        );
    }

    #[test]
    fn empty_file_has_no_partial_range() {
        let mut selection = RangeSelection::new(0, 100);
        assert_eq!(selection.size(), 0);
        assert!(selection.set_from(0).is_err());
        assert!(selection.set_size(1).is_err());
        assert!(selection.validate(100).is_err());
    }

    #[test]
    fn ranges_align_to_complete_lines() {
        let (_dir, path) = write_temp(b"aaa\nbbb\nccc\nddd\n");
        let content = read(&path, 5, 10, 1024);
        assert_eq!(content.text, "bbb\nccc\n");
        assert_eq!((content.start, content.end), (4, 12));

        let content = read(&path, 0, 6, 1024);
        assert_eq!(content.text, "aaa\nbbb\n");
        assert_eq!((content.start, content.end), (0, 8));

        // A range that already ends at a line start must not grow on reload.
        let content = read(&path, 4, 8, 1024);
        assert_eq!(content.text, "bbb\n");
        assert_eq!((content.start, content.end), (4, 8));

        let content = read(&path, 9, 16, 1024);
        assert_eq!(content.text, "ccc\nddd\n");
        assert_eq!((content.start, content.end), (8, 16));
    }

    #[test]
    fn crlf_and_cr_ranges_keep_complete_lines() {
        let (_dir, path) = write_temp(b"aaa\r\nbbb\r\nccc\r\n");
        let content = read(&path, 6, 12, 1024);
        assert_eq!(content.text, "bbb\nccc\n");
        assert_eq!((content.start, content.end), (5, 15));

        let (_dir, path) = write_temp(b"aaa\rbbb\rccc\r");
        let content = read(&path, 5, 9, 1024);
        assert_eq!(content.text, "bbb\nccc\n");
        assert_eq!((content.start, content.end), (4, 12));
    }

    #[test]
    fn first_line_offsets_include_the_whole_line() {
        let (_dir, path) = write_temp(b"aaa\nbbb\n");
        let content = read(&path, 1, 3, 1024);
        assert_eq!(content.text, "aaa\n");
        assert_eq!((content.start, content.end), (0, 4));
    }

    #[test]
    fn utf16_odd_offsets_are_aligned_to_code_units() {
        let text = "alpha\nbeta\ngamma\n";
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let (_dir, path) = write_temp(&bytes);
        let content = read(&path, 13, 17, 1024);
        assert_eq!(content.text, "beta\n");
        assert_eq!((content.start, content.end), (14, 24));
    }

    #[test]
    fn range_never_exceeds_the_limit() {
        let (_dir, path) = write_temp(b"111\n222\n333\n");
        let content = read(&path, 0, 12, 4);
        assert!(content.end - content.start <= 4);
        assert_eq!(content.text, "111\n");

        let content = read(&path, 0, 12, 5);
        assert!(content.end - content.start <= 5);
        assert_eq!(content.text, "111\n");
        assert_eq!(content.end, 4);
    }

    #[test]
    fn unknown_offsets_are_rejected() {
        let (_dir, path) = write_temp(b"abc\n");
        let result = read_range(
            &path,
            10,
            20,
            1024,
            &AtomicBool::new(false),
            &AtomicU64::new(0),
        );
        assert!(matches!(result, Err(Error::InvalidRange { .. })));
    }

    #[test]
    fn cancellation_stops_the_read() {
        let (_dir, path) = write_temp(&vec![b'a'; 1024 * 1024 * 2]);
        let cancel = AtomicBool::new(true);
        let result = read_range(&path, 0, 1024, 1024 * 1024, &cancel, &AtomicU64::new(0));
        assert!(matches!(result, Err(Error::Cancelled)));
        let result = read_whole(&path, u64::MAX, &cancel, &AtomicU64::new(0));
        assert!(matches!(result, Err(Error::Cancelled)));
    }

    #[test]
    fn utf16_ranges_use_code_unit_boundaries() {
        let text = "alpha\nbeta\ngamma\n";
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let (_dir, path) = write_temp(&bytes);
        // Start inside "beta" and end inside "gamma"; both offsets must grow
        // to complete lines.
        let content = read(&path, 16, 20, 1024);
        assert_eq!(content.text, "beta\n");
        assert_eq!(content.encoding, TextEncoding::Utf16Le);
    }

    #[test]
    fn non_text_ranges_keep_their_bytes() {
        let bytes = vec![0u8, 1, 2, 10, 3, 4, 5];
        let (_dir, path) = write_temp(&bytes);
        let content = read(&path, 0, bytes.len() as u64, 1024);
        assert!(content.non_text);
        assert_eq!(content.text, "\u{0}\u{1}\u{2}\n\u{3}\u{4}\u{5}");
    }

    #[test]
    fn partial_save_names_are_distinct() {
        assert_eq!(partial_save_name(Path::new("/tmp/log.txt")), "log-part.txt");
        assert_eq!(
            partial_save_name(Path::new("/tmp/archive")),
            "archive-part.txt"
        );
    }
}

//! Text file encoding detection, decoding and encoding.
//!
//! Notepad++ distinguishes UTF-8 (with and without BOM), UTF-16 LE/BE and
//! legacy single/double byte code pages. We expose the same concepts through
//! [`TextEncoding`], backed by `encoding_rs` for the actual conversions.

use encoding_rs::{Encoding, BIG5, EUC_KR, GB18030, SHIFT_JIS, UTF_16BE, UTF_16LE, WINDOWS_1252};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::i18n::tr;

/// User-selectable file encodings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TextEncoding {
    #[default]
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Windows1252,
    Iso88591,
    Gb18030,
    ShiftJis,
    EucKr,
    Big5,
}

impl TextEncoding {
    /// Menu label used by the UI.
    pub fn label(self) -> String {
        tr(match self {
            Self::Utf8 => "core.encoding.utf8",
            Self::Utf8Bom => "core.encoding.utf8_bom",
            Self::Utf16Le => "core.encoding.utf16_le",
            Self::Utf16Be => "core.encoding.utf16_be",
            Self::Windows1252 => "core.encoding.windows1252",
            Self::Iso88591 => "core.encoding.iso88591",
            Self::Gb18030 => "core.encoding.gb18030",
            Self::ShiftJis => "core.encoding.shift_jis",
            Self::EucKr => "core.encoding.euc_kr",
            Self::Big5 => "core.encoding.big5",
        })
    }

    /// Short label used in the status bar.
    pub fn short_label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf8Bom => "UTF-8-BOM",
            Self::Utf16Le => "UTF-16LE",
            Self::Utf16Be => "UTF-16BE",
            Self::Windows1252 => "ANSI",
            Self::Iso88591 => "ISO-8859-1",
            Self::Gb18030 => "GB18030",
            Self::ShiftJis => "Shift-JIS",
            Self::EucKr => "EUC-KR",
            Self::Big5 => "Big5",
        }
    }

    /// All encodings in a stable order for menus.
    pub const ALL: [TextEncoding; 10] = [
        Self::Utf8,
        Self::Utf8Bom,
        Self::Utf16Le,
        Self::Utf16Be,
        Self::Windows1252,
        Self::Iso88591,
        Self::Gb18030,
        Self::ShiftJis,
        Self::EucKr,
        Self::Big5,
    ];

    /// The underlying `encoding_rs` codec.
    pub fn codec(self) -> &'static Encoding {
        match self {
            Self::Utf8 | Self::Utf8Bom => encoding_rs::UTF_8,
            Self::Utf16Le => UTF_16LE,
            Self::Utf16Be => UTF_16BE,
            // WHATWG maps both "ANSI" and "ISO-8859-1" labels onto
            // windows-1252; keeping them separate in the menu mirrors editors
            // while using the same lossless single-byte codec.
            Self::Windows1252 | Self::Iso88591 => WINDOWS_1252,
            Self::Gb18030 => GB18030,
            Self::ShiftJis => SHIFT_JIS,
            Self::EucKr => EUC_KR,
            Self::Big5 => BIG5,
        }
    }

    /// Whether a BOM is written when saving.
    pub fn bom(self) -> &'static [u8] {
        match self {
            Self::Utf8Bom => &[0xEF, 0xBB, 0xBF],
            Self::Utf16Le => &[0xFF, 0xFE],
            Self::Utf16Be => &[0xFE, 0xFF],
            _ => &[],
        }
    }

    pub fn is_utf16(self) -> bool {
        matches!(self, Self::Utf16Le | Self::Utf16Be)
    }
}

/// Result of inspecting a byte stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedText {
    pub text: String,
    pub encoding: TextEncoding,
    /// A BOM was present and stripped from `text`.
    pub had_bom: bool,
}

/// Detect a file's encoding, decode it to UTF-8 and strip any BOM.
///
/// Detection order mirrors Notepad++/common editors:
/// 1. explicit UTF-8 / UTF-16 BOM,
/// 2. valid UTF-8,
/// 3. statistical detection with `chardetng`, restricted to the legacy
///    encodings we support,
/// 4. Windows-1252 as a lossless byte-preserving fallback.
pub fn decode(bytes: &[u8]) -> DecodedText {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return DecodedText {
            text: String::from_utf8_lossy(rest).into_owned(),
            encoding: TextEncoding::Utf8Bom,
            had_bom: true,
        };
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let (text, _, _) = UTF_16LE.decode(rest);
        return DecodedText {
            text: text.into_owned(),
            encoding: TextEncoding::Utf16Le,
            had_bom: true,
        };
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let (text, _, _) = UTF_16BE.decode(rest);
        return DecodedText {
            text: text.into_owned(),
            encoding: TextEncoding::Utf16Be,
            had_bom: true,
        };
    }

    if let Some((encoding, text)) = detect_bomless_utf16(bytes) {
        return DecodedText {
            text,
            encoding,
            had_bom: false,
        };
    }

    if let Ok(text) = std::str::from_utf8(bytes) {
        return DecodedText {
            text: text.to_owned(),
            encoding: TextEncoding::Utf8,
            had_bom: false,
        };
    }

    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    let guessed = detector.guess(None, true);
    let encoding = match_encoding(guessed);
    let (text, _, _) = encoding.codec().decode(bytes);
    DecodedText {
        text: text.into_owned(),
        encoding,
        had_bom: false,
    }
}

/// Detect BOM-less UTF-16 by looking for a strong one-sided NUL pattern and
/// confirming that the decoded text contains no unexpected control characters.
fn detect_bomless_utf16(bytes: &[u8]) -> Option<(TextEncoding, String)> {
    if bytes.len() < 4 || !bytes.contains(&0) {
        return None;
    }
    let sample = &bytes[..bytes.len().min(8192)];
    let mut even_zero = 0usize;
    let mut odd_zero = 0usize;
    let mut even_count = 0usize;
    let mut odd_count = 0usize;
    for (i, &byte) in sample.iter().enumerate() {
        if i % 2 == 0 {
            even_count += 1;
            even_zero += usize::from(byte == 0);
        } else {
            odd_count += 1;
            odd_zero += usize::from(byte == 0);
        }
    }
    let even_ratio = even_zero as f32 / even_count.max(1) as f32;
    let odd_ratio = odd_zero as f32 / odd_count.max(1) as f32;
    let encoding = if odd_ratio > 0.6 && even_ratio < 0.25 {
        TextEncoding::Utf16Le
    } else if even_ratio > 0.6 && odd_ratio < 0.25 {
        TextEncoding::Utf16Be
    } else {
        return None;
    };
    let (sample_text, _, _) = encoding.codec().decode(sample);
    if sample_text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\x0C'))
    {
        return None;
    }
    let (text, _, _) = encoding.codec().decode(bytes);
    Some((encoding, text.into_owned()))
}

fn match_encoding(encoding: &'static Encoding) -> TextEncoding {
    if encoding == GB18030 {
        TextEncoding::Gb18030
    } else if encoding == SHIFT_JIS {
        TextEncoding::ShiftJis
    } else if encoding == EUC_KR {
        TextEncoding::EucKr
    } else if encoding == BIG5 {
        TextEncoding::Big5
    } else if encoding == encoding_rs::UTF_8 {
        TextEncoding::Utf8
    } else if encoding == encoding_rs::WINDOWS_1252 {
        TextEncoding::Windows1252
    } else {
        // `chardetng` can report other legacy encodings (for example
        // windows-1251). Windows-1252 keeps every byte reversible and is what
        // Notepad++ calls "ANSI" on western systems.
        TextEncoding::Windows1252
    }
}

/// Encode text using `encoding`, including the BOM when the encoding implies
/// one. Characters that cannot be represented are replaced with `?`/U+FFFD by
/// `encoding_rs`; the number of replacements is returned.
pub fn encode(text: &str, encoding: TextEncoding) -> (Vec<u8>, bool) {
    // `encoding_rs` follows the WHATWG Encoding Standard, whose only output
    // encoding is UTF-8: calling `encode()` on a UTF-16 codec yields UTF-8.
    // Encode UTF-16 manually, honoring the requested byte order.
    if encoding.is_utf16() {
        let little_endian = encoding == TextEncoding::Utf16Le;
        let mut out = Vec::with_capacity(2 + text.len() * 2);
        out.extend_from_slice(encoding.bom());
        for unit in text.encode_utf16() {
            out.extend_from_slice(&if little_endian {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        return (out, false);
    }
    let (body, _, had_errors) = encoding.codec().encode(text);
    let mut out = Vec::with_capacity(encoding.bom().len() + body.len());
    out.extend_from_slice(encoding.bom());
    out.extend_from_slice(&body);
    (out, had_errors)
}

/// Re-decode text bytes into another encoding without changing characters.
///
/// The encoded bytes are decoded again so that the returned text is clean
/// UTF-8 (invalid source sequences yield replacement characters). The caller
/// can then store the new encoding on the document.
pub fn convert_bytes(bytes: &[u8], from: TextEncoding, to: TextEncoding) -> Result<String> {
    let decoded = decode_with(bytes, from);
    if decoded.is_empty() && !bytes.is_empty() {
        return Err(Error::Encoding(tr("core.encoding.decode_failed")));
    }
    // Validate that the destination can represent the text.
    let (_, had_errors) = encode(&decoded, to);
    if had_errors {
        // Still perform the conversion: Notepad++ writes '?' for unrepresentable
        // characters rather than refusing to save. We surface it in the status.
        log::warn!("some characters are not representable in {}", to.label());
    }
    Ok(decoded)
}

/// Decode with a specific encoding, stripping the matching BOM if present.
pub fn decode_with(bytes: &[u8], encoding: TextEncoding) -> String {
    let bom = encoding.bom();
    let body = if !bom.is_empty() && bytes.starts_with(bom) {
        &bytes[bom.len()..]
    } else {
        bytes
    };
    let (text, _, _) = encoding.codec().decode(body);
    text.into_owned()
}

/// Heuristic binary test used before opening a file as text.
pub fn looks_binary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let sample = &bytes[..bytes.len().min(8192)];
    let bom = sample.starts_with(&[0xFF, 0xFE]) || sample.starts_with(&[0xFE, 0xFF]);
    if bom {
        let decoded = decode(sample).text;
        return decoded
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\x0C'));
    }
    if !sample.contains(&0) {
        return false;
    }

    // BOM-less UTF-16 text has NULs concentrated in every other byte. A random
    // binary file with NULs rarely shows that strong, one-sided pattern.
    let mut even_zero = 0usize;
    let mut odd_zero = 0usize;
    let mut even_count = 0usize;
    let mut odd_count = 0usize;
    for (i, &byte) in sample.iter().enumerate() {
        if i % 2 == 0 {
            even_count += 1;
            even_zero += usize::from(byte == 0);
        } else {
            odd_count += 1;
            odd_zero += usize::from(byte == 0);
        }
    }
    let even_ratio = even_zero as f32 / even_count.max(1) as f32;
    let odd_ratio = odd_zero as f32 / odd_count.max(1) as f32;
    let utf16_like =
        (odd_ratio > 0.6 && even_ratio < 0.25) || (even_ratio > 0.6 && odd_ratio < 0.25);
    if !utf16_like {
        return true;
    }
    // Confirm the NUL pattern really is BOM-less UTF-16 by decoding it: binary
    // data that happens to alternate NULs usually decodes to control chars.
    if even_ratio >= odd_ratio {
        let (text, _, _) = UTF_16BE.decode(sample);
        text.chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\x0C'))
    } else {
        let (text, _, _) = UTF_16LE.decode(sample);
        text.chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\x0C'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf8_with_and_without_bom() {
        let plain = decode("héllo → 世界".as_bytes());
        assert_eq!(plain.encoding, TextEncoding::Utf8);
        assert!(!plain.had_bom);
        assert_eq!(plain.text, "héllo → 世界");

        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice("hello".as_bytes());
        let decoded = decode(&with_bom);
        assert_eq!(decoded.encoding, TextEncoding::Utf8Bom);
        assert!(decoded.had_bom);
        assert_eq!(decoded.text, "hello");
    }

    #[test]
    fn decodes_utf16_le_and_be() {
        let text = "Hello 世界";
        let le = encode(text, TextEncoding::Utf16Le).0;
        let decoded = decode(&le);
        assert_eq!(decoded.encoding, TextEncoding::Utf16Le);
        assert_eq!(decoded.text, text);

        let be = encode(text, TextEncoding::Utf16Be).0;
        let decoded = decode(&be);
        assert_eq!(decoded.encoding, TextEncoding::Utf16Be);
        assert_eq!(decoded.text, text);
    }

    #[test]
    fn detects_bomless_utf16() {
        let text = "Hello\n世界\n";
        let mut le = Vec::new();
        for unit in text.encode_utf16() {
            le.extend_from_slice(&unit.to_le_bytes());
        }
        let decoded = decode(&le);
        assert_eq!(decoded.encoding, TextEncoding::Utf16Le);
        assert!(!decoded.had_bom);
        assert_eq!(decoded.text, text);

        let mut be = Vec::new();
        for unit in text.encode_utf16() {
            be.extend_from_slice(&unit.to_be_bytes());
        }
        let decoded = decode(&be);
        assert_eq!(decoded.encoding, TextEncoding::Utf16Be);
        assert_eq!(decoded.text, text);
    }

    #[test]
    fn round_trips_legacy_encodings() {
        for encoding in [
            TextEncoding::Windows1252,
            TextEncoding::Iso88591,
            TextEncoding::Gb18030,
            TextEncoding::ShiftJis,
            TextEncoding::EucKr,
            TextEncoding::Big5,
        ] {
            let (bytes, errors) = encode("hello", encoding);
            assert!(!errors);
            assert_eq!(decode_with(&bytes, encoding), "hello", "{encoding:?}");
        }
    }

    #[test]
    fn encodes_bom_when_requested() {
        let (bytes, _) = encode("x", TextEncoding::Utf8Bom);
        assert_eq!(&bytes, b"\xEF\xBB\xBFx");
        let (bytes, _) = encode("x", TextEncoding::Utf16Le);
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
    }

    #[test]
    fn detects_binary_and_text() {
        assert!(looks_binary(&[0, 1, 2, 0, 4]));
        assert!(!looks_binary("plain text\n".as_bytes()));
        let utf16 = encode("hello\nworld", TextEncoding::Utf16Le).0;
        assert!(!looks_binary(&utf16));
    }

    #[test]
    fn preserves_utf8_round_trip() {
        let (bytes, errors) = encode("a\nb\tc 世界", TextEncoding::Utf8);
        assert!(!errors);
        assert_eq!(decode(&bytes).text, "a\nb\tc 世界");
    }
}

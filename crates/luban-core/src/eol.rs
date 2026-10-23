//! Line-ending detection and conversion.

use serde::{Deserialize, Serialize};

use crate::i18n::tr;

/// A line-ending convention.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Eol {
    #[default]
    Lf,
    CrLf,
    Cr,
}

impl Eol {
    pub const ALL: [Eol; 3] = [Self::Lf, Self::CrLf, Self::Cr];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::Cr => "\r",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::CrLf => "CRLF",
            Self::Cr => "CR",
        }
    }

    pub fn long_label(self) -> String {
        tr(match self {
            Self::Lf => "core.eol.lf_long",
            Self::CrLf => "core.eol.crlf_long",
            Self::Cr => "core.eol.cr_long",
        })
    }

    /// Default line ending for newly created documents on this platform.
    pub fn native() -> Self {
        if cfg!(windows) {
            Self::CrLf
        } else {
            Self::Lf
        }
    }
}

impl std::fmt::Display for Eol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Detect the dominant line ending in `text`.
///
/// Ties are resolved in favour of the convention that appears first, matching
/// the behaviour users expect when a file mixes CRLF and LF lines. Text with no
/// line break reports the platform-native convention.
pub fn detect(text: &str) -> Eol {
    let bytes = text.as_bytes();
    let (mut crlf, mut lf, mut cr) = (0usize, 0usize, 0usize);
    let mut first: Option<Eol> = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                if bytes.get(i + 1) == Some(&b'\n') {
                    crlf += 1;
                    first.get_or_insert(Eol::CrLf);
                    i += 2;
                } else {
                    cr += 1;
                    first.get_or_insert(Eol::Cr);
                    i += 1;
                }
            }
            b'\n' => {
                lf += 1;
                first.get_or_insert(Eol::Lf);
                i += 1;
            }
            _ => i += 1,
        }
    }

    match first {
        None => Eol::native(),
        Some(first) => {
            let best = crlf.max(lf).max(cr);
            // Resolve ties in favour of the first convention encountered.
            let winner = if crlf == best {
                Eol::CrLf
            } else if lf == best {
                Eol::Lf
            } else {
                Eol::Cr
            };
            let count = match first {
                Eol::CrLf => crlf,
                Eol::Lf => lf,
                Eol::Cr => cr,
            };
            if count == best {
                first
            } else {
                winner
            }
        }
    }
}

/// Rewrite every line break in `text` to `eol`.
pub fn normalize(text: &str, eol: Eol) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str(eol.as_str());
            }
            '\n' => out.push_str(eol.as_str()),
            _ => out.push(c),
        }
    }
    out
}

/// Normalize every line break to `\n` for internal editing.
pub fn to_lf(text: &str) -> String {
    normalize(text, Eol::Lf)
}

/// Owned variant of [`to_lf`] that avoids a second full-size allocation.
///
/// Text without `\r` is returned unchanged. Otherwise CRLF and CR sequences are
/// compacted in place; CR and LF are ASCII, so rewriting them never splits a
/// multi-byte UTF-8 sequence.
pub fn to_lf_owned(mut text: String) -> String {
    if !text.as_bytes().contains(&b'\r') {
        return text;
    }
    // SAFETY: only ASCII bytes are rewritten or removed, so the buffer stays
    // valid UTF-8.
    let bytes = unsafe { text.as_mut_vec() };
    let mut read = 0usize;
    let mut write = 0usize;
    while read < bytes.len() {
        let byte = bytes[read];
        if byte == b'\r' {
            bytes[write] = b'\n';
            write += 1;
            read += 1;
            if read < bytes.len() && bytes[read] == b'\n' {
                read += 1;
            }
        } else {
            if write != read {
                bytes[write] = byte;
            }
            write += 1;
            read += 1;
        }
    }
    bytes.truncate(write);
    text
}

/// Owned variant of [`normalize`]. LF output reuses the input buffer; CR output
/// is compacted in place; CRLF output falls back to building a new string.
pub fn normalize_owned(text: String, eol: Eol) -> String {
    match eol {
        Eol::Lf => to_lf_owned(text),
        Eol::Cr => {
            let mut text = text;
            if !text.as_bytes().iter().any(|b| *b == b'\r' || *b == b'\n') {
                return text;
            }
            // SAFETY: only ASCII bytes are rewritten or removed.
            let bytes = unsafe { text.as_mut_vec() };
            let mut read = 0usize;
            let mut write = 0usize;
            while read < bytes.len() {
                match bytes[read] {
                    b'\r' => {
                        bytes[write] = b'\r';
                        write += 1;
                        read += 1;
                        if read < bytes.len() && bytes[read] == b'\n' {
                            read += 1;
                        }
                    }
                    b'\n' => {
                        bytes[write] = b'\r';
                        write += 1;
                        read += 1;
                    }
                    byte => {
                        if write != read {
                            bytes[write] = byte;
                        }
                        write += 1;
                        read += 1;
                    }
                }
            }
            bytes.truncate(write);
            text
        }
        Eol::CrLf => normalize(&text, eol),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_line_endings() {
        assert_eq!(detect("a\nb\nc"), Eol::Lf);
        assert_eq!(detect("a\r\nb\r\nc"), Eol::CrLf);
        assert_eq!(detect("a\rb\rc"), Eol::Cr);
        assert_eq!(detect("no newline"), Eol::native());
        assert_eq!(detect("mixed\r\nmore\n"), Eol::CrLf);
        assert_eq!(detect("mixed\nmore\r\nmore\r\n"), Eol::CrLf);
    }

    #[test]
    fn normalizes_all_conventions() {
        assert_eq!(normalize("a\r\nb\nc\rd", Eol::Lf), "a\nb\nc\nd");
        assert_eq!(normalize("a\nb", Eol::CrLf), "a\r\nb");
        assert_eq!(normalize("a\r\nb", Eol::Cr), "a\rb");
        assert_eq!(normalize("", Eol::CrLf), "");
    }

    #[test]
    fn lf_normalization_is_idempotent() {
        let mixed = "one\r\ntwo\rthree\nfour";
        let once = to_lf(mixed);
        assert_eq!(once, "one\ntwo\nthree\nfour");
        assert_eq!(to_lf(&once), once);
    }

    #[test]
    fn owned_lf_normalization_matches_borrowed() {
        let cases = [
            "",
            "no line breaks",
            "a\nb\n",
            "a\r\nb\r\n",
            "a\rb\r",
            "mixed\r\nlf\ncr\rend",
            "héllo\r\n世界\r\n🙂",
        ];
        for case in cases {
            assert_eq!(to_lf_owned(case.to_string()), to_lf(case), "{case:?}");
        }
    }

    #[test]
    fn owned_normalization_matches_borrowed() {
        let cases = [
            "",
            "no line breaks",
            "a\nb\n",
            "a\r\nb\r\n",
            "a\rb\r",
            "mixed\r\nlf\ncr\rend",
            "héllo\r\n世界\r\n🙂",
        ];
        for eol in Eol::ALL {
            for case in cases {
                assert_eq!(
                    normalize_owned(case.to_string(), eol),
                    normalize(case, eol),
                    "{eol:?} {case:?}"
                );
            }
        }
    }
}

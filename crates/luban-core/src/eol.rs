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
}

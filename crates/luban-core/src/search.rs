//! Literal and regular-expression search/replace.
//!
//! All public positions are character indices so they can be handed directly to
//! the text widget. Internally `regex` works on bytes; conversions happen once
//! per match using a char-boundary table.

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::text::{char_boundaries, char_count, char_to_byte};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchOptions {
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub dot_matches_newline: bool,
    pub wrap_around: bool,
    /// UI hint: restrict the search to the active selection.
    pub in_selection: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            match_case: false,
            whole_word: false,
            regex: false,
            dot_matches_newline: false,
            wrap_around: true,
            in_selection: false,
        }
    }
}

/// A match expressed as a character range `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Match {
    pub start: usize,
    pub end: usize,
}

impl Match {
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

#[derive(Clone, Debug)]
pub struct SearchEngine {
    pattern: String,
    options: SearchOptions,
    regex: Option<Regex>,
    error: Option<String>,
}

impl SearchEngine {
    pub fn new(pattern: impl Into<String>, options: SearchOptions) -> Self {
        let pattern = pattern.into();
        let mut engine = Self {
            pattern: pattern.clone(),
            options,
            regex: None,
            error: None,
        };
        if pattern.is_empty() {
            return engine;
        }
        let mut flags = String::new();
        if !options.match_case {
            flags.push('i');
        }
        if options.dot_matches_newline {
            flags.push('s');
        }
        if options.regex {
            // Notepad++ treats ^ and $ as line anchors in regex mode.
            flags.push('m');
        }
        let mut body = if options.regex {
            pattern
        } else {
            regex::escape(&pattern)
        };
        if options.whole_word {
            body = format!(r"\b(?:{body})\b");
        }
        let expression = if flags.is_empty() {
            body
        } else {
            format!("(?{flags})(?:{body})")
        };
        match RegexBuilder::new(&expression)
            .size_limit(16 * 1024 * 1024)
            .build()
        {
            Ok(regex) => engine.regex = Some(regex),
            Err(err) => engine.error = Some(err.to_string()),
        }
        engine
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub fn options(&self) -> SearchOptions {
        self.options
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn is_valid(&self) -> bool {
        self.error.is_none()
    }

    pub fn has_pattern(&self) -> bool {
        !self.pattern.is_empty()
    }

    /// Find all matches within an optional character range.
    pub fn find_all(&self, text: &str, range: Option<(usize, usize)>, limit: usize) -> Vec<Match> {
        let Some(regex) = &self.regex else {
            return Vec::new();
        };
        let (byte_start, byte_end, char_start) = resolve_range(text, range);
        let haystack = &text[byte_start..byte_end];
        let boundaries = char_boundaries(haystack);
        let mut out = Vec::new();
        for m in regex.find_iter(haystack) {
            if out.len() >= limit {
                break;
            }
            let start = boundaries.partition_point(|&b| b < m.start());
            let end = boundaries.partition_point(|&b| b < m.end());
            out.push(Match {
                start: char_start + start,
                end: char_start + end,
            });
        }
        out
    }

    /// Find the next or previous match. `from` is a character index.
    /// Wrapping is controlled by [`SearchOptions::wrap_around`].
    pub fn find_from(
        &self,
        text: &str,
        from: usize,
        forward: bool,
        range: Option<(usize, usize)>,
    ) -> Option<Match> {
        let (byte_start, byte_end, char_start) = resolve_range(text, range);
        let haystack = &text[byte_start..byte_end];
        let boundaries = char_boundaries(haystack);
        let char_end = char_start + boundaries.len().saturating_sub(1);
        let from = from.clamp(char_start, char_end);

        let mut matches: Vec<Match> = Vec::new();
        for m in self.regex.as_ref()?.find_iter(haystack) {
            let start = boundaries.partition_point(|&b| b < m.start());
            let end = boundaries.partition_point(|&b| b < m.end());
            matches.push(Match {
                start: char_start + start,
                end: char_start + end,
            });
        }
        if matches.is_empty() {
            return None;
        }
        if forward {
            if let Some(m) = matches.iter().find(|m| m.start >= from) {
                return Some(*m);
            }
            if self.options.wrap_around {
                return matches.first().copied();
            }
        } else if let Some(m) = matches.iter().rev().find(|m| m.start < from) {
            return Some(*m);
        } else if self.options.wrap_around {
            return matches.last().copied();
        }
        None
    }

    pub fn count(&self, text: &str, range: Option<(usize, usize)>, limit: usize) -> usize {
        self.find_all(text, range, limit).len()
    }

    /// Replace every match, returning the new text and the number of
    /// replacements. In regex mode `$1`, `${name}` and `$$` are expanded like
    /// the regex crate; in literal mode the escapes `\\n`, `\\r`, `\\t`, `\\\\`
    /// are expanded.
    pub fn replace_all(
        &self,
        text: &str,
        replacement: &str,
        range: Option<(usize, usize)>,
    ) -> Result<(String, usize), String> {
        let regex = self.regex.as_ref().ok_or_else(|| {
            self.error
                .clone()
                .unwrap_or_else(|| crate::i18n::tr("core.search.empty_pattern"))
        })?;
        let (byte_start, byte_end, _) = resolve_range(text, range);
        let haystack = &text[byte_start..byte_end];
        let mut out = String::with_capacity(haystack.len());
        let mut last = 0usize;
        let mut count = 0usize;
        for captures in regex.captures_iter(haystack) {
            let Some(m) = captures.get(0) else { continue };
            out.push_str(&haystack[last..m.start()]);
            if self.options.regex {
                let mut expanded = String::new();
                captures.expand(replacement, &mut expanded);
                out.push_str(&expanded);
            } else {
                out.push_str(&expand_literal_replacement(replacement));
            }
            last = m.end();
            count += 1;
        }
        out.push_str(&haystack[last..]);
        let mut result = String::with_capacity(text.len());
        result.push_str(&text[..byte_start]);
        result.push_str(&out);
        result.push_str(&text[byte_end..]);
        Ok((result, count))
    }

    /// Replace a single previously found match, expanding capture references
    /// when in regex mode.
    pub fn replace_match(&self, text: &str, matched: Match, replacement: &str) -> String {
        let regex = self.regex.as_ref();
        let b0 = char_to_byte(text, matched.start);
        let b1 = char_to_byte(text, matched.end);
        let expanded = if self.options.regex {
            if let Some(regex) = regex {
                if let Some(captures) = regex.captures_at(text, b0) {
                    if captures.get(0).map(|m| m.start()) == Some(b0) {
                        let mut out = String::new();
                        captures.expand(replacement, &mut out);
                        out
                    } else {
                        expand_literal_replacement(replacement)
                    }
                } else {
                    expand_literal_replacement(replacement)
                }
            } else {
                expand_literal_replacement(replacement)
            }
        } else {
            expand_literal_replacement(replacement)
        };
        let mut out = String::with_capacity(text.len() + expanded.len());
        out.push_str(&text[..b0]);
        out.push_str(&expanded);
        out.push_str(&text[b1..]);
        out
    }
}

fn resolve_range(text: &str, range: Option<(usize, usize)>) -> (usize, usize, usize) {
    let total = char_count(text);
    let (start, end) = range.unwrap_or((0, total));
    let (start, end) = if start <= end {
        (start.min(total), end.min(total))
    } else {
        (end.min(total), start.min(total))
    };
    (char_to_byte(text, start), char_to_byte(text, end), start)
}

fn expand_literal_replacement(replacement: &str) -> String {
    let mut out = String::with_capacity(replacement.len());
    let mut chars = replacement.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(pattern: &str, options: SearchOptions) -> SearchEngine {
        SearchEngine::new(pattern, options)
    }

    fn literal() -> SearchOptions {
        SearchOptions {
            match_case: true,
            ..SearchOptions::default()
        }
    }

    #[test]
    fn finds_literal_matches() {
        let e = engine("cat", literal());
        let matches = e.find_all("cat catalog Cat", None, 100);
        assert_eq!(matches.len(), 2);
        assert_eq!(&"cat catalog Cat"[0..3], "cat");
    }

    #[test]
    fn case_insensitive_by_default() {
        let e = engine("cat", SearchOptions::default());
        assert_eq!(e.find_all("Cat cat CAT", None, 100).len(), 3);
    }

    #[test]
    fn whole_word_only() {
        let options = SearchOptions {
            match_case: true,
            whole_word: true,
            ..SearchOptions::default()
        };
        let e = engine("cat", options);
        let matches = e.find_all("cat catalog cat.", None, 100);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn reports_invalid_regex() {
        let options = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };
        let e = engine("(unclosed", options);
        assert!(!e.is_valid());
        assert!(e.error().is_some());
        assert!(e.find_all("anything", None, 10).is_empty());
    }

    #[test]
    fn handles_unicode_char_indices() {
        let e = engine("世界", literal());
        let text = "a世界b世界";
        let matches = e.find_all(text, None, 10);
        assert_eq!(
            matches,
            vec![Match { start: 1, end: 3 }, Match { start: 4, end: 6 }]
        );
        assert_eq!(
            &text[char_to_byte(text, matches[0].start)..char_to_byte(text, matches[0].end)],
            "世界"
        );
    }

    #[test]
    fn honors_range() {
        let e = engine("a", literal());
        let text = "a a a a";
        let matches = e.find_all(text, Some((2, 5)), 10);
        assert_eq!(
            matches,
            vec![Match { start: 2, end: 3 }, Match { start: 4, end: 5 }]
        );
    }

    #[test]
    fn finds_next_and_previous_with_wrap() {
        let e = engine("a", literal());
        let text = "a b a b a";
        assert_eq!(
            e.find_from(text, 0, true, None),
            Some(Match { start: 0, end: 1 })
        );
        assert_eq!(
            e.find_from(text, 1, true, None),
            Some(Match { start: 4, end: 5 })
        );
        assert_eq!(
            e.find_from(text, 5, true, None),
            Some(Match { start: 8, end: 9 })
        );
        assert_eq!(
            e.find_from(text, 9, true, None),
            Some(Match { start: 0, end: 1 })
        );
        assert_eq!(
            e.find_from(text, 8, false, None),
            Some(Match { start: 4, end: 5 })
        );
        assert_eq!(
            e.find_from(text, 0, false, None),
            Some(Match { start: 8, end: 9 })
        );

        let no_wrap = SearchOptions {
            match_case: true,
            wrap_around: false,
            ..SearchOptions::default()
        };
        let e = engine("a", no_wrap);
        assert_eq!(e.find_from(text, 9, true, None), None);
    }

    #[test]
    fn supports_regex_and_multiline_anchors() {
        let options = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };
        let e = engine(r"^\d+", options);
        let matches = e.find_all("12\nab\n34", None, 10);
        assert_eq!(
            matches,
            vec![Match { start: 0, end: 2 }, Match { start: 6, end: 8 }]
        );
    }

    #[test]
    fn replaces_all_and_counts() {
        let options = SearchOptions {
            match_case: true,
            ..SearchOptions::default()
        };
        let e = engine("cat", options);
        let (out, count) = e.replace_all("cat and cat", "dog", None).unwrap();
        assert_eq!(out, "dog and dog");
        assert_eq!(count, 2);
    }

    #[test]
    fn replaces_with_capture_groups() {
        let options = SearchOptions {
            regex: true,
            match_case: true,
            ..SearchOptions::default()
        };
        let e = engine(r"(\w+)@(\w+)", options);
        let (out, count) = e.replace_all("a@b c@d", "$2:$1", None).unwrap();
        assert_eq!(out, "b:a d:c");
        assert_eq!(count, 2);
    }

    #[test]
    fn literal_replacement_expands_escapes() {
        let e = engine("x", literal());
        let (out, _) = e.replace_all("x", r"a\nb\tc\\d", None).unwrap();
        assert_eq!(out, "a\nb\tc\\d");
    }

    #[test]
    fn replaces_single_match_with_captures() {
        let options = SearchOptions {
            regex: true,
            match_case: true,
            ..SearchOptions::default()
        };
        let e = engine(r"(\d+)", options);
        let m = e.find_all("x 42 y", None, 1)[0];
        assert_eq!(e.replace_match("x 42 y", m, "<$1>"), "x <42> y");
    }

    #[test]
    fn handles_zero_length_matches() {
        let options = SearchOptions {
            regex: true,
            match_case: true,
            ..SearchOptions::default()
        };
        let e = engine(r"^", options);
        let matches = e.find_all("a\nb\n", None, 10);
        assert_eq!(matches.len(), 3);
        let (out, count) = e.replace_all("a\nb\n", ">", None).unwrap();
        assert_eq!(out, ">a\n>b\n>");
        assert_eq!(count, 3);
    }

    #[test]
    fn replaces_only_in_range() {
        let e = engine("a", literal());
        let (out, count) = e.replace_all("a a a", "x", Some((2, 5))).unwrap();
        assert_eq!(out, "a x x");
        assert_eq!(count, 2);
    }

    #[test]
    fn empty_pattern_matches_nothing() {
        let e = engine("", SearchOptions::default());
        assert!(!e.has_pattern());
        assert!(e.find_all("abc", None, 10).is_empty());
        assert!(e.replace_all("abc", "x", None).is_err());
    }
}

//! Literal and regular-expression search/replace.
//!
//! All public positions are character indices so they can be handed directly to
//! the text widget. Internally `regex` works on bytes; conversions happen once
//! per match using a char-boundary table.

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::text::char_to_byte;

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

/// A match expressed as UTF-8 byte offsets into the searched slice. Used by
/// the disk engine, whose positions are global byte offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteMatch {
    pub start: usize,
    pub end: usize,
}

impl ByteMatch {
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

impl SearchEngine {
    /// First match whose start is at or after `from` (a byte offset).
    pub fn find_first_in(&self, text: &str, from: usize) -> Option<ByteMatch> {
        let regex = self.regex.as_ref()?;
        if from > text.len() {
            return None;
        }
        let from = if text.is_char_boundary(from) {
            from
        } else {
            let mut adjusted = from;
            while adjusted < text.len() && !text.is_char_boundary(adjusted) {
                adjusted += 1;
            }
            adjusted
        };
        regex.find_at(text, from).map(|matched| ByteMatch {
            start: matched.start(),
            end: matched.end(),
        })
    }

    /// Last match starting before `until` (a byte offset).
    pub fn find_last_in(&self, text: &str, until: usize) -> Option<ByteMatch> {
        let regex = self.regex.as_ref()?;
        let mut until = until.min(text.len());
        while until > 0 && !text.is_char_boundary(until) {
            until -= 1;
        }
        regex
            .find_iter(&text[..until])
            .last()
            .map(|matched| ByteMatch {
                start: matched.start(),
                end: matched.end(),
            })
    }

    /// All matches inside `text`, in order.
    pub fn find_all_bytes(&self, text: &str, limit: usize) -> Vec<ByteMatch> {
        let Some(regex) = self.regex.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for matched in regex.find_iter(text) {
            if out.len() >= limit {
                break;
            }
            out.push(ByteMatch {
                start: matched.start(),
                end: matched.end(),
            });
        }
        out
    }

    /// Expand `replacement` for a byte match.
    pub fn replacement_for_bytes(
        &self,
        text: &str,
        matched: ByteMatch,
        replacement: &str,
    ) -> String {
        if !self.options.regex {
            return expand_literal_replacement(replacement);
        }
        if let Some(regex) = self.regex.as_ref() {
            if let Some(captures) = regex.captures_at(text, matched.start) {
                if captures.get(0).map(|found| found.start()) == Some(matched.start) {
                    let mut out = String::new();
                    captures.expand(replacement, &mut out);
                    return out;
                }
            }
        }
        expand_literal_replacement(replacement)
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
        let mut out = Vec::new();
        let mut mapper = CharMapper::new();
        for m in regex.find_iter(haystack) {
            if out.len() >= limit {
                break;
            }
            out.push(mapper.map_match(haystack, m.start(), m.end(), char_start));
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
        let regex = self.regex.as_ref()?;
        let mut mapper = CharMapper::new();
        if forward {
            let mut first = None;
            for m in regex.find_iter(haystack) {
                let mapped = mapper.map_match(haystack, m.start(), m.end(), char_start);
                first.get_or_insert(mapped);
                if mapped.start >= from {
                    return Some(mapped);
                }
            }
            if self.options.wrap_around {
                return first;
            }
        } else {
            let mut last = None;
            let mut before = None;
            for m in regex.find_iter(haystack) {
                let mapped = mapper.map_match(haystack, m.start(), m.end(), char_start);
                last = Some(mapped);
                if mapped.start < from {
                    before = Some(mapped);
                }
            }
            if before.is_some() {
                return before;
            }
            if self.options.wrap_around {
                return last;
            }
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
        if byte_start == 0 && byte_end == text.len() {
            // The whole document was replaced in `out`; skip a second copy.
            return Ok((out, count));
        }
        let mut result = String::with_capacity(text.len());
        result.push_str(&text[..byte_start]);
        result.push_str(&out);
        result.push_str(&text[byte_end..]);
        Ok((result, count))
    }

    /// Expand `replacement` for a single match without rebuilding the
    /// surrounding text. Large files combine this with targeted editor edits
    /// instead of materializing a whole replacement buffer.
    pub fn replacement_for(&self, text: &str, matched: Match, replacement: &str) -> String {
        if !self.options.regex {
            return expand_literal_replacement(replacement);
        }
        let b0 = char_to_byte(text, matched.start);
        if let Some(regex) = self.regex.as_ref() {
            if let Some(captures) = regex.captures_at(text, b0) {
                if captures.get(0).map(|m| m.start()) == Some(b0) {
                    let mut out = String::new();
                    captures.expand(replacement, &mut out);
                    return out;
                }
            }
        }
        expand_literal_replacement(replacement)
    }

    /// Replace a single previously found match, expanding capture references
    /// when in regex mode.
    pub fn replace_match(&self, text: &str, matched: Match, replacement: &str) -> String {
        let b0 = char_to_byte(text, matched.start);
        let b1 = char_to_byte(text, matched.end);
        let expanded = self.replacement_for(text, matched, replacement);
        let mut out = String::with_capacity(text.len() + expanded.len());
        out.push_str(&text[..b0]);
        out.push_str(&expanded);
        out.push_str(&text[b1..]);
        out
    }
}

fn resolve_range(text: &str, range: Option<(usize, usize)>) -> (usize, usize, usize) {
    let (start, end) = match range {
        None => return (0, text.len(), 0),
        Some((start, end)) if start <= end => (start, end),
        Some((start, end)) => (end, start),
    };
    (char_to_byte(text, start), char_to_byte(text, end), start)
}

/// Incrementally converts regex byte offsets into character indices.
///
/// Regex matches arrive in byte order, so each conversion only scans the bytes
/// since the previous match. This avoids the per-character boundary table that
/// would otherwise allocate gigabytes for large files.
struct CharMapper {
    byte: usize,
    chars: usize,
}

impl CharMapper {
    fn new() -> Self {
        Self { byte: 0, chars: 0 }
    }

    fn map_match(&mut self, haystack: &str, start: usize, end: usize, char_start: usize) -> Match {
        self.chars += haystack[self.byte..start].chars().count();
        let start_char = self.chars;
        self.chars += haystack[start..end].chars().count();
        self.byte = end;
        Match {
            start: char_start + start_char,
            end: char_start + self.chars,
        }
    }
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

    #[test]
    fn incremental_mapping_handles_many_unicode_matches() {
        let e = engine("é", literal());
        let text = "aé世é🙂é\n".repeat(500);
        let matches = e.find_all(&text, None, 10_000);
        assert_eq!(matches.len(), 1500);
        for matched in &matches {
            assert_eq!(
                &text[char_to_byte(&text, matched.start)..char_to_byte(&text, matched.end)],
                "é"
            );
        }
    }

    #[test]
    fn find_from_stops_at_the_first_match_without_collecting() {
        let e = engine("needle", literal());
        let text = format!("{}needle{}", "x".repeat(100_000), "y".repeat(100_000));
        let found = e.find_from(&text, 0, true, None).unwrap();
        assert_eq!(found.start, 100_000);
        assert_eq!(found.end, 100_006);
        assert_eq!(e.find_from(&text, found.end, true, None), Some(found));
    }

    #[test]
    fn expands_single_replacements_without_rebuilding_text() {
        let options = SearchOptions {
            regex: true,
            match_case: true,
            ..SearchOptions::default()
        };
        let e = engine(r"(\w+)@(\w+)", options);
        let text = "a@b c@d";
        let matched = e.find_all(text, None, 1)[0];
        assert_eq!(e.replacement_for(text, matched, "$2:$1"), "b:a");
        assert_eq!(e.replace_match(text, matched, "$2:$1"), "b:a c@d");

        let e = engine("x", literal());
        assert_eq!(
            e.replacement_for("x", Match { start: 0, end: 1 }, r"\n"),
            "\n"
        );
    }
}

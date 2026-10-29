//! Literal and regular-expression search/replace.
//!
//! All public positions are character indices so they can be handed directly to
//! the text widget. Internally regex engines work on bytes; conversions happen
//! once per match. The default engine is the Rust `regex` crate; an optional
//! fancy-regex path adds look-around and backreferences.

use fancy_regex::Regex as FancyRegex;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::text::char_to_byte;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchOptions {
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
    /// Use fancy-regex for look-around and backreferences. Only meaningful
    /// together with `regex`.
    #[serde(default)]
    pub enhanced_regex: bool,
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
            enhanced_regex: false,
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

/// Data for the replace-preview dialog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplacePreview {
    pub count: usize,
    /// Up to the requested number of `(before, after)` samples.
    pub samples: Vec<(String, String)>,
}

type GroupRange = Option<(usize, usize)>;

#[derive(Clone, Debug)]
enum Pattern {
    Rust(Regex),
    Fancy(FancyRegex),
}

#[derive(Clone, Debug)]
pub struct SearchEngine {
    pattern: String,
    options: SearchOptions,
    compiled: Option<Pattern>,
    error: Option<String>,
}

impl SearchEngine {
    /// First match whose start is at or after `from` (a byte offset).
    pub fn find_first_in(&self, text: &str, from: usize) -> Option<ByteMatch> {
        if from > text.len() {
            return None;
        }
        let from = align_boundary(text, from);
        match self.compiled.as_ref()? {
            Pattern::Rust(regex) => regex.find_at(text, from).map(|matched| ByteMatch {
                start: matched.start(),
                end: matched.end(),
            }),
            Pattern::Fancy(regex) => {
                regex
                    .find_from_pos(text, from)
                    .ok()
                    .flatten()
                    .map(|matched| ByteMatch {
                        start: matched.start(),
                        end: matched.end(),
                    })
            }
        }
    }

    /// Last match starting before `until` (a byte offset).
    pub fn find_last_in(&self, text: &str, until: usize) -> Option<ByteMatch> {
        let mut until = until.min(text.len());
        while until > 0 && !text.is_char_boundary(until) {
            until -= 1;
        }
        let haystack = &text[..until];
        match self.compiled.as_ref()? {
            Pattern::Rust(regex) => regex.find_iter(haystack).last().map(|matched| ByteMatch {
                start: matched.start(),
                end: matched.end(),
            }),
            Pattern::Fancy(regex) => regex
                .find_iter(haystack)
                .filter_map(|matched| matched.ok())
                .last()
                .map(|matched| ByteMatch {
                    start: matched.start(),
                    end: matched.end(),
                }),
        }
    }

    /// All matches inside `text`, in order.
    pub fn find_all_bytes(&self, text: &str, limit: usize) -> Vec<ByteMatch> {
        let Some(pattern) = self.compiled.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match pattern {
            Pattern::Rust(regex) => {
                for matched in regex.find_iter(text) {
                    if out.len() >= limit {
                        break;
                    }
                    out.push(ByteMatch {
                        start: matched.start(),
                        end: matched.end(),
                    });
                }
            }
            Pattern::Fancy(regex) => {
                for matched in regex.find_iter(text).flatten() {
                    if out.len() >= limit {
                        break;
                    }
                    out.push(ByteMatch {
                        start: matched.start(),
                        end: matched.end(),
                    });
                }
            }
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
        match self.compiled.as_ref() {
            Some(Pattern::Rust(regex)) => {
                if let Some(captures) = regex.captures_at(text, matched.start) {
                    if captures.get(0).map(|found| found.start()) == Some(matched.start) {
                        return expand_rust_captures(replacement, text, &captures);
                    }
                }
            }
            Some(Pattern::Fancy(regex)) => {
                if let Ok(Some(captures)) = regex.captures_from_pos(text, matched.start) {
                    if captures.get(0).map(|found| found.start()) == Some(matched.start) {
                        return expand_fancy_captures(replacement, text, &captures);
                    }
                }
            }
            None => {}
        }
        expand_literal_replacement(replacement)
    }
}

impl SearchEngine {
    pub fn new(pattern: impl Into<String>, options: SearchOptions) -> Self {
        let pattern = pattern.into();
        let mut engine = Self {
            pattern: pattern.clone(),
            options,
            compiled: None,
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
        if options.regex && options.enhanced_regex {
            match FancyRegex::new(&expression) {
                Ok(regex) => engine.compiled = Some(Pattern::Fancy(regex)),
                Err(err) => engine.error = Some(err.to_string()),
            }
        } else {
            match RegexBuilder::new(&expression)
                .size_limit(16 * 1024 * 1024)
                .build()
            {
                Ok(regex) => engine.compiled = Some(Pattern::Rust(regex)),
                Err(err) => engine.error = Some(err.to_string()),
            }
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
        let Some(pattern) = self.compiled.as_ref() else {
            return Vec::new();
        };
        let (byte_start, byte_end, char_start) = resolve_range(text, range);
        let haystack = &text[byte_start..byte_end];
        let mut out = Vec::new();
        let mut mapper = CharMapper::new();
        match pattern {
            Pattern::Rust(regex) => {
                for matched in regex.find_iter(haystack) {
                    if out.len() >= limit {
                        break;
                    }
                    out.push(mapper.map_match(
                        haystack,
                        matched.start(),
                        matched.end(),
                        char_start,
                    ));
                }
            }
            Pattern::Fancy(regex) => {
                for matched in regex.find_iter(haystack).flatten() {
                    if out.len() >= limit {
                        break;
                    }
                    out.push(mapper.map_match(
                        haystack,
                        matched.start(),
                        matched.end(),
                        char_start,
                    ));
                }
            }
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
        let pattern = self.compiled.as_ref()?;
        let mut mapper = CharMapper::new();
        let mut first: Option<Match> = None;
        let mut last: Option<Match> = None;
        let mut before: Option<Match> = None;
        let mut visit = |matched: Match| -> Option<Match> {
            first.get_or_insert(matched);
            last = Some(matched);
            if matched.start < from {
                before = Some(matched);
            }
            if forward && matched.start >= from {
                return Some(matched);
            }
            None
        };
        match pattern {
            Pattern::Rust(regex) => {
                for matched in regex.find_iter(haystack) {
                    let mapped =
                        mapper.map_match(haystack, matched.start(), matched.end(), char_start);
                    if let Some(found) = visit(mapped) {
                        return Some(found);
                    }
                }
            }
            Pattern::Fancy(regex) => {
                for matched in regex.find_iter(haystack).flatten() {
                    let mapped =
                        mapper.map_match(haystack, matched.start(), matched.end(), char_start);
                    if let Some(found) = visit(mapped) {
                        return Some(found);
                    }
                }
            }
        }
        drop(visit);
        if !forward {
            if before.is_some() {
                return before;
            }
            if self.options.wrap_around {
                return last;
            }
            return None;
        }
        if self.options.wrap_around {
            return first;
        }
        None
    }

    pub fn count(&self, text: &str, range: Option<(usize, usize)>, limit: usize) -> usize {
        self.find_all(text, range, limit).len()
    }

    /// Replace every match, returning the new text and the number of
    /// replacements.
    ///
    /// In regex mode `$1` and `\1` refer to captures, `${name}`/`$name`
    /// refer to named captures, and `\U`/`\L`/`\E` switch case conversion. In
    /// literal mode the escapes `\n`, `\r`, `\t`, `\\` are expanded.
    pub fn replace_all(
        &self,
        text: &str,
        replacement: &str,
        range: Option<(usize, usize)>,
    ) -> Result<(String, usize), String> {
        let pattern = self.compiled.as_ref().ok_or_else(|| {
            self.error
                .clone()
                .unwrap_or_else(|| crate::i18n::tr("core.search.empty_pattern"))
        })?;
        let (byte_start, byte_end, _) = resolve_range(text, range);
        let haystack = &text[byte_start..byte_end];
        let mut out = String::with_capacity(haystack.len());
        let mut last = 0usize;
        let mut count = 0usize;
        match pattern {
            Pattern::Rust(regex) => {
                for captures in regex.captures_iter(haystack) {
                    let Some(m) = captures.get(0) else { continue };
                    out.push_str(&haystack[last..m.start()]);
                    if self.options.regex {
                        out.push_str(&expand_rust_captures(replacement, haystack, &captures));
                    } else {
                        out.push_str(&expand_literal_replacement(replacement));
                    }
                    last = m.end();
                    count += 1;
                }
            }
            Pattern::Fancy(regex) => {
                for captures in regex.captures_iter(haystack) {
                    let captures = captures.map_err(|err| err.to_string())?;
                    let Some(m) = captures.get(0) else { continue };
                    out.push_str(&haystack[last..m.start()]);
                    if self.options.regex {
                        out.push_str(&expand_fancy_captures(replacement, haystack, &captures));
                    } else {
                        out.push_str(&expand_literal_replacement(replacement));
                    }
                    last = m.end();
                    count += 1;
                }
            }
        }
        out.push_str(&haystack[last..]);
        if byte_start == 0 && byte_end == text.len() {
            return Ok((out, count));
        }
        let mut result = String::with_capacity(text.len());
        result.push_str(&text[..byte_start]);
        result.push_str(&out);
        result.push_str(&text[byte_end..]);
        Ok((result, count))
    }

    /// Build the data shown before a Replace All is confirmed.
    pub fn preview_replace(
        &self,
        text: &str,
        replacement: &str,
        range: Option<(usize, usize)>,
        samples: usize,
    ) -> Result<ReplacePreview, String> {
        let pattern = self.compiled.as_ref().ok_or_else(|| {
            self.error
                .clone()
                .unwrap_or_else(|| crate::i18n::tr("core.search.empty_pattern"))
        })?;
        let (byte_start, byte_end, _) = resolve_range(text, range);
        let haystack = &text[byte_start..byte_end];
        let mut preview = ReplacePreview::default();
        let mut push = |before: &str, after: String| {
            preview.count += 1;
            if preview.samples.len() < samples {
                preview.samples.push((before.to_string(), after));
            }
        };
        match pattern {
            Pattern::Rust(regex) => {
                for captures in regex.captures_iter(haystack) {
                    let Some(m) = captures.get(0) else { continue };
                    push(
                        &haystack[m.start()..m.end()],
                        if self.options.regex {
                            expand_rust_captures(replacement, haystack, &captures)
                        } else {
                            expand_literal_replacement(replacement)
                        },
                    );
                }
            }
            Pattern::Fancy(regex) => {
                for captures in regex.captures_iter(haystack) {
                    let captures = captures.map_err(|err| err.to_string())?;
                    let Some(m) = captures.get(0) else { continue };
                    push(
                        &haystack[m.start()..m.end()],
                        if self.options.regex {
                            expand_fancy_captures(replacement, haystack, &captures)
                        } else {
                            expand_literal_replacement(replacement)
                        },
                    );
                }
            }
        }
        Ok(preview)
    }

    /// Expand `replacement` for a single match without rebuilding the
    /// surrounding text. Large files combine this with targeted editor edits
    /// instead of materializing a whole replacement buffer.
    pub fn replacement_for(&self, text: &str, matched: Match, replacement: &str) -> String {
        if !self.options.regex {
            return expand_literal_replacement(replacement);
        }
        let b0 = char_to_byte(text, matched.start);
        match self.compiled.as_ref() {
            Some(Pattern::Rust(regex)) => {
                if let Some(captures) = regex.captures_at(text, b0) {
                    if captures.get(0).map(|m| m.start()) == Some(b0) {
                        return expand_rust_captures(replacement, text, &captures);
                    }
                }
            }
            Some(Pattern::Fancy(regex)) => {
                if let Ok(Some(captures)) = regex.captures_from_pos(text, b0) {
                    if captures.get(0).map(|m| m.start()) == Some(b0) {
                        return expand_fancy_captures(replacement, text, &captures);
                    }
                }
            }
            None => {}
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

fn align_boundary(text: &str, mut byte: usize) -> usize {
    while byte < text.len() && !text.is_char_boundary(byte) {
        byte += 1;
    }
    byte
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

fn expand_rust_captures(template: &str, text: &str, captures: &regex::Captures<'_>) -> String {
    let groups: Vec<GroupRange> = (0..captures.len())
        .map(|index| {
            captures
                .get(index)
                .map(|found| (found.start(), found.end()))
        })
        .collect();
    expand_template(template, text, &groups, &|name| {
        captures
            .name(name)
            .map(|found| (found.start(), found.end()))
    })
}

fn expand_fancy_captures(
    template: &str,
    text: &str,
    captures: &fancy_regex::Captures<'_>,
) -> String {
    let groups: Vec<GroupRange> = (0..captures.len())
        .map(|index| {
            captures
                .get(index)
                .map(|found| (found.start(), found.end()))
        })
        .collect();
    expand_template(template, text, &groups, &|name| {
        captures
            .name(name)
            .map(|found| (found.start(), found.end()))
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaseMode {
    Keep,
    Upper,
    Lower,
}

fn push_case(out: &mut String, value: &str, mode: CaseMode) {
    match mode {
        CaseMode::Keep => out.push_str(value),
        CaseMode::Upper => out.push_str(&value.to_uppercase()),
        CaseMode::Lower => out.push_str(&value.to_lowercase()),
    }
}

fn expand_template(
    template: &str,
    text: &str,
    groups: &[GroupRange],
    name_lookup: &dyn Fn(&str) -> Option<(usize, usize)>,
) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::with_capacity(template.len());
    let mut mode = CaseMode::Keep;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '\\' {
            index += 1;
            let Some(&next) = chars.get(index) else {
                out.push('\\');
                break;
            };
            match next {
                'n' => push_case(&mut out, "\n", mode),
                'r' => push_case(&mut out, "\r", mode),
                't' => push_case(&mut out, "\t", mode),
                '\\' => push_case(&mut out, "\\", mode),
                'U' => mode = CaseMode::Upper,
                'L' => mode = CaseMode::Lower,
                'E' => mode = CaseMode::Keep,
                '1'..='9' => {
                    let group = next.to_digit(10).unwrap_or(0) as usize;
                    push_group(&mut out, text, groups, group, mode);
                }
                other => {
                    push_case(&mut out, "\\", mode);
                    push_case(&mut out, &other.to_string(), mode);
                }
            }
            index += 1;
            continue;
        }
        if ch == '$' {
            index += 1;
            let Some(&next) = chars.get(index) else {
                push_case(&mut out, "$", mode);
                break;
            };
            if next == '$' {
                push_case(&mut out, "$", mode);
                index += 1;
                continue;
            }
            if next == '{' {
                let start = index + 1;
                let mut end = start;
                while end < chars.len() && chars[end] != '}' {
                    end += 1;
                }
                let name: String = chars[start..end].iter().collect();
                push_named(&mut out, text, groups, name_lookup, &name, mode);
                index = (end + 1).min(chars.len());
                continue;
            }
            if next.is_ascii_digit() {
                let mut end = index;
                while end < chars.len() && chars[end].is_ascii_digit() {
                    end += 1;
                }
                let number: String = chars[index..end].iter().collect();
                let group = number.parse::<usize>().unwrap_or(0);
                push_group(&mut out, text, groups, group, mode);
                index = end;
                continue;
            }
            let mut end = index;
            while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            if end == index {
                push_case(&mut out, "$", mode);
                continue;
            }
            let name: String = chars[index..end].iter().collect();
            push_named(&mut out, text, groups, name_lookup, &name, mode);
            index = end;
            continue;
        }
        push_case(&mut out, &ch.to_string(), mode);
        index += 1;
    }
    out
}

fn push_group(out: &mut String, text: &str, groups: &[GroupRange], group: usize, mode: CaseMode) {
    if let Some(Some((start, end))) = groups.get(group) {
        push_case(out, &text[*start..*end], mode);
    }
}

fn push_named(
    out: &mut String,
    text: &str,
    groups: &[GroupRange],
    name_lookup: &dyn Fn(&str) -> Option<(usize, usize)>,
    name: &str,
    mode: CaseMode,
) {
    if let Ok(number) = name.parse::<usize>() {
        push_group(out, text, groups, number, mode);
        return;
    }
    if let Some((start, end)) = name_lookup(name) {
        push_case(out, &text[start..end], mode);
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
    fn enhanced_regex_supports_lookaround_and_backreferences() {
        let options = SearchOptions {
            regex: true,
            enhanced_regex: true,
            match_case: true,
            ..SearchOptions::default()
        };
        let e = engine(r"(?<=foo)bar", options);
        assert_eq!(
            e.find_all("foobar bazbar", None, 10),
            vec![Match { start: 3, end: 6 }]
        );
        let e = engine(r"(\w+) \1", options);
        assert_eq!(
            e.find_all("hello hello world", None, 10),
            vec![Match { start: 0, end: 11 }]
        );
        // The default engine rejects the same pattern.
        let rust = engine(
            r"(?<=foo)bar",
            SearchOptions {
                regex: true,
                ..Default::default()
            },
        );
        assert!(!rust.is_valid());
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

        let (out, _) = e.replace_all("a@b c@d", r"\2:\1", None).unwrap();
        assert_eq!(out, "b:a d:c");

        let (out, _) = e.replace_all("a@b c@d", r"\U\1\E@\L\2\E", None).unwrap();
        assert_eq!(out, "A@b C@d");
    }

    #[test]
    fn enhanced_regex_replace_supports_lookaround_and_backreferences() {
        let options = SearchOptions {
            regex: true,
            enhanced_regex: true,
            match_case: true,
            ..SearchOptions::default()
        };
        let e = engine(r"(?<=a)b", options);
        let (out, count) = e.replace_all("ab cb", "X", None).unwrap();
        assert_eq!(out, "aX cb");
        assert_eq!(count, 1);
        let e = engine(r"(\w+) \1", options);
        let (out, _) = e.replace_all("x x y", "[$1]", None).unwrap();
        assert_eq!(out, "[x] y");
    }

    #[test]
    fn preview_reports_count_and_samples() {
        let options = SearchOptions {
            regex: true,
            match_case: true,
            ..SearchOptions::default()
        };
        let e = engine(r"(\w+)@(\w+)", options);
        let preview = e.preview_replace("a@b c@d e@f", "$2:$1", None, 2).unwrap();
        assert_eq!(preview.count, 3);
        assert_eq!(
            preview.samples,
            vec![
                ("a@b".to_string(), "b:a".to_string()),
                ("c@d".to_string(), "d:c".to_string())
            ]
        );
        let preview = e.preview_replace("nothing", "$1", None, 5).unwrap();
        assert_eq!(preview.count, 0);
        assert!(preview.samples.is_empty());
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
        assert_eq!(e.replace_match("x 42 y", m, "<\\1>"), "x <42> y");
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

    #[test]
    fn named_captures_work_in_both_engines() {
        let options = SearchOptions {
            regex: true,
            match_case: true,
            ..SearchOptions::default()
        };
        let e = engine(r"(?P<word>\w+)@(?P<domain>\w+)", options);
        let (out, _) = e.replace_all("a@b", "${domain}:$word", None).unwrap();
        assert_eq!(out, "b:a");

        let enhanced = SearchOptions {
            enhanced_regex: true,
            ..options
        };
        let e = engine(r"(?<word>\w+)@(?<domain>\w+)", enhanced);
        let (out, _) = e.replace_all("a@b", "${domain}:$word", None).unwrap();
        assert_eq!(out, "b:a");
    }
}

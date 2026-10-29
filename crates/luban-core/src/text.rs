//! Character-index based text utilities shared by the editor and tests.
//!
//! The GUI (egui) addresses text by character index, while `regex` reports
//! byte offsets. Everything in this module is therefore explicit about which
//! unit it uses.

/// Convert a character index into a byte offset.
pub fn char_to_byte(text: &str, char_index: usize) -> usize {
    if char_index == 0 {
        return 0;
    }
    text.char_indices()
        .nth(char_index)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

/// Convert a byte offset (which must be on a char boundary) into a character
/// index.
pub fn byte_to_char(text: &str, byte: usize) -> usize {
    let byte = byte.min(text.len());
    text[..byte].chars().count()
}

pub fn char_count(text: &str) -> usize {
    text.chars().count()
}

pub fn clamp_char(text: &str, index: usize) -> usize {
    index.min(char_count(text))
}

/// Extract a character range as an owned string. Indices are clamped and
/// swapped if reversed.
pub fn slice_chars(text: &str, start: usize, end: usize) -> String {
    let total = char_count(text);
    let (start, end) = (start.min(end).min(total), start.max(end).min(total));
    let b0 = char_to_byte(text, start);
    let b1 = char_to_byte(text, end);
    text[b0..b1].to_owned()
}

/// Byte-based line index for a UTF-8 string, used by search result mapping.
pub fn char_boundaries(text: &str) -> Vec<usize> {
    let mut v: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    v.push(text.len());
    v
}

/// A line map for text with mixed line endings.
#[derive(Clone, Debug)]
pub struct LineIndex {
    starts: Vec<usize>,
    /// Exclusive end of each line's visible content (before its terminator).
    content_ends: Vec<usize>,
    char_count: usize,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0usize];
        let mut content_ends = Vec::new();
        let mut count = 0;
        let mut previous_was_cr = false;
        for (index, ch) in text.chars().enumerate() {
            count = index + 1;
            match ch {
                '\n' => {
                    content_ends.push(if previous_was_cr { index - 1 } else { index });
                    starts.push(count);
                }
                '\r' => {}
                _ => {
                    if previous_was_cr {
                        content_ends.push(index - 1);
                        starts.push(index);
                    }
                }
            }
            previous_was_cr = ch == '\r';
        }
        if previous_was_cr {
            content_ends.push(count.saturating_sub(1));
            starts.push(count);
        } else {
            content_ends.push(count);
        }
        Self {
            starts,
            content_ends,
            char_count: count,
        }
    }

    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    pub fn char_count(&self) -> usize {
        self.char_count
    }

    pub fn line_of_char(&self, index: usize) -> usize {
        let index = index.min(self.char_count);
        self.starts
            .partition_point(|&start| start <= index)
            .saturating_sub(1)
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.starts.get(line).copied().unwrap_or(self.char_count)
    }

    /// Exclusive end of the line, including its terminator.
    pub fn line_end_including_newline(&self, line: usize) -> usize {
        self.starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.char_count)
    }

    /// Exclusive end of the visible line content, before its terminator.
    pub fn line_content_end(&self, line: usize) -> usize {
        self.content_ends
            .get(line)
            .copied()
            .unwrap_or(self.char_count)
    }

    /// 1-based (line, column) for a character index.
    pub fn line_col(&self, index: usize) -> (usize, usize) {
        let index = index.min(self.char_count);
        let line = self.line_of_char(index);
        let start = self.line_start(line);
        (line + 1, index - start + 1)
    }

    /// 1-based line/column to character index. Columns past the end of a line
    /// are clamped to the end of its visible content, never past the newline.
    pub fn char_index(&self, line: usize, column: usize) -> usize {
        let line = line.saturating_sub(1).min(self.len().saturating_sub(1));
        let start = self.line_start(line);
        let end = self.line_content_end(line);
        (start + column.saturating_sub(1)).min(end)
    }

    /// Character range of a line's content, excluding its terminator.
    pub fn content_range(&self, _text: &str, line: usize) -> (usize, usize) {
        (self.line_start(line), self.line_content_end(line))
    }

    pub fn line_text(&self, text: &str, line: usize) -> String {
        let (s, e) = self.content_range(text, line);
        slice_chars(text, s, e)
    }

    /// Line range covering the given character selection. A selection ending
    /// exactly at the start of a line does not include that line.
    pub fn selection_lines(&self, start: usize, end: usize) -> (usize, usize) {
        let (a, b) = (start.min(end), start.max(end));
        let first = self.line_of_char(a);
        let mut last = self.line_of_char(b);
        if last > first
            && b == self.line_start(last)
            && self.line_of_char(b.saturating_sub(1)) == last - 1
        {
            last -= 1;
        }
        (first, last)
    }
}

/// Expand a selection to complete lines and return the character range
/// including the final line terminator.
pub fn line_block(text: &str, start: usize, end: usize) -> (usize, usize) {
    let index = LineIndex::new(text);
    let (first, last) = index.selection_lines(start, end);
    (
        index.line_start(first),
        index.line_end_including_newline(last),
    )
}

/// Delete the selected lines (or the caret's line when there is no selection).
pub fn delete_lines(text: &str, start: usize, end: usize) -> (String, usize) {
    let index = LineIndex::new(text);
    let (first, last) = index.selection_lines(start, end);
    let block_start = index.line_start(first);
    let block_end = index.line_end_including_newline(last);
    let mut out = String::with_capacity(text.len());
    let b0 = char_to_byte(text, block_start);
    let b1 = char_to_byte(text, block_end);
    out.push_str(&text[..b0]);
    out.push_str(&text[b1..]);
    let new_pos = block_start.min(char_count(&out));
    (out, new_pos)
}

/// Duplicate the selected lines. The cursor is placed on the duplicated copy.
pub fn duplicate_lines(text: &str, start: usize, end: usize) -> (String, usize, usize) {
    let index = LineIndex::new(text);
    let (first, last) = index.selection_lines(start, end);
    let block_start = index.line_start(first);
    let block_end = index.line_end_including_newline(last);
    let block = slice_chars(text, block_start, block_end);
    let has_terminator = block.ends_with('\n') || block.ends_with('\r');
    let mut out = String::with_capacity(text.len() + block.len() + 1);
    let b0 = char_to_byte(text, block_start);
    let b1 = char_to_byte(text, block_end);
    out.push_str(&text[..b1]);
    if !has_terminator {
        out.push('\n');
    }
    out.push_str(&block);
    out.push_str(&text[b1..]);
    let added = block.chars().count() + usize::from(!has_terminator);
    let _ = b0;
    let new_start = block_start + added;
    let new_end = new_start + block.chars().count();
    (out, new_start, new_end)
}

/// Remove trailing spaces/tabs from every line.
pub fn trim_trailing_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let end = rest.find(['\r', '\n']).unwrap_or(rest.len());
        let (content, after) = rest.split_at(end);
        out.push_str(content.trim_end_matches([' ', '\t']));
        if after.is_empty() {
            break;
        }
        if let Some(after_cr) = after.strip_prefix("\r\n") {
            out.push_str("\r\n");
            rest = after_cr;
        } else if let Some(after_cr) = after.strip_prefix('\r') {
            out.push('\r');
            rest = after_cr;
        } else if let Some(after_lf) = after.strip_prefix('\n') {
            out.push('\n');
            rest = after_lf;
        } else {
            unreachable!("split on CR/LF must advance");
        }
    }
    out
}

/// Convert leading tabs of every line to spaces (respecting tab stops).
pub fn tabs_to_spaces(text: &str, tab_size: usize) -> String {
    let tab_size = tab_size.max(1);
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (content, ending) = split_terminator(line);
        let mut expanded = String::new();
        let mut column = 0usize;
        let mut consumed = 0usize;
        for (byte, ch) in content.char_indices() {
            match ch {
                ' ' => {
                    expanded.push(' ');
                    column += 1;
                }
                '\t' => {
                    let width = tab_size - (column % tab_size);
                    expanded.push_str(&" ".repeat(width));
                    column += width;
                }
                _ => {
                    consumed = byte;
                    break;
                }
            }
            consumed = byte + ch.len_utf8();
        }
        out.push_str(&expanded);
        out.push_str(&content[consumed..]);
        out.push_str(ending);
    }
    out
}

/// Convert leading groups of `tab_size` spaces to tabs.
pub fn spaces_to_tabs(text: &str, tab_size: usize) -> String {
    let tab_size = tab_size.max(1);
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (content, ending) = split_terminator(line);
        let mut spaces = 0;
        for ch in content.chars() {
            if ch == ' ' {
                spaces += 1;
            } else {
                break;
            }
        }
        let tabs = spaces / tab_size;
        let remainder = spaces % tab_size;
        out.push_str(&"\t".repeat(tabs));
        out.push_str(&" ".repeat(remainder));
        out.push_str(&content[spaces..]);
        out.push_str(ending);
    }
    out
}

fn split_terminator(line: &str) -> (&str, &str) {
    if let Some(rest) = line.strip_suffix("\r\n") {
        (rest, "\r\n")
    } else if let Some(rest) = line.strip_suffix('\n') {
        (rest, "\n")
    } else if let Some(rest) = line.strip_suffix('\r') {
        (rest, "\r")
    } else {
        (line, "")
    }
}

/// Indent or outdent complete lines within a selection.
///
/// Returns `(new_text, new_selection_start, new_selection_end)`.
pub fn indent_lines(
    text: &str,
    start: usize,
    end: usize,
    tab_size: usize,
    use_spaces: bool,
    outdent: bool,
) -> (String, usize, usize) {
    let tab_size = tab_size.max(1);
    let index = LineIndex::new(text);
    let (first, last) = index.selection_lines(start, end);
    let unit = if use_spaces {
        " ".repeat(tab_size)
    } else {
        "\t".to_string()
    };
    let block_start = index.line_start(first);
    let block_end = index.line_end_including_newline(last);

    // Each edit is applied at a line start. Map original positions into the
    // edited buffer so the selection follows the text.
    let mut edits: Vec<(usize, isize)> = Vec::new();
    for line in first..=last {
        if outdent {
            let line_start = index.line_start(line);
            let content = index.line_text(text, line);
            let removed = if content.starts_with('\t') {
                1
            } else {
                content
                    .chars()
                    .take(tab_size)
                    .take_while(|c| *c == ' ')
                    .count()
            };
            if removed > 0 {
                edits.push((line_start, -(removed as isize)));
            }
        } else {
            edits.push((index.line_start(line), unit.chars().count() as isize));
        }
    }

    let mut out = String::with_capacity(text.len() + 16);
    let mut cursor = 0usize;
    for &(pos, delta) in &edits {
        let pos = pos.max(cursor).min(char_count(text));
        out.push_str(&slice_chars(text, cursor, pos));
        if delta > 0 {
            out.push_str(&unit);
        } else {
            let remove = (-delta) as usize;
            // The removed characters are known to be spaces or a tab.
            let removed_text = slice_chars(text, pos, pos + remove);
            debug_assert!(removed_text.chars().all(|c| c == ' ' || c == '\t'));
        }
        cursor = if delta > 0 {
            pos
        } else {
            (pos + (-delta) as usize).min(char_count(text))
        };
    }
    out.push_str(&slice_chars(text, cursor, char_count(text)));

    let map = |position: usize| -> usize {
        let mut shift = 0isize;
        for &(pos, delta) in &edits {
            if pos < position || (delta > 0 && pos == position && position != block_start) {
                shift += delta;
            } else if delta < 0 && pos == position {
                // Removing indentation at the caret keeps the caret at the
                // start of the line content.
                shift += 0;
            }
        }
        (position as isize + shift).max(0) as usize
    };
    let mut ns = map(start);
    let mut ne = map(end);
    if ns > ne {
        std::mem::swap(&mut ns, &mut ne);
    }
    let _ = block_end;
    let out_len = char_count(&out);
    (out, ns.min(out_len), ne.min(out_len))
}

/// Insert a line comment prefix on every selected line, or remove it when all
/// selected non-empty lines already carry one.
pub fn toggle_line_comments(
    text: &str,
    start: usize,
    end: usize,
    prefix: &str,
) -> (String, usize, usize) {
    if prefix.is_empty() {
        return (text.to_owned(), start, end);
    }
    let index = LineIndex::new(text);
    let (first, last) = index.selection_lines(start, end);
    let block_start = index.line_start(first);
    let block_end = index.line_end_including_newline(last);
    let block = slice_chars(text, block_start, block_end);
    let lines: Vec<&str> = block.split_inclusive('\n').collect();

    let all_commented = lines.iter().all(|line| {
        let (content, _) = split_terminator(line);
        let trimmed = content.trim_start();
        trimmed.is_empty() || trimmed.starts_with(prefix)
    });

    let mut changed = String::with_capacity(block.len() + prefix.len() * lines.len());

    for line in &lines {
        let (content, ending) = split_terminator(line);
        if content.trim().is_empty() {
            changed.push_str(content);
            changed.push_str(ending);
            continue;
        }
        let indent_len = content.len() - content.trim_start().len();
        let (indent, rest) = content.split_at(indent_len);
        if all_commented {
            if let Some(after) = rest.strip_prefix(prefix) {
                changed.push_str(indent);
                changed.push_str(after.strip_prefix(' ').unwrap_or(after));
            } else {
                changed.push_str(content);
            }
        } else {
            changed.push_str(indent);
            changed.push_str(prefix);
            if !rest.is_empty() && !rest.starts_with(' ') && !prefix.ends_with(' ') {
                changed.push(' ');
            }
            changed.push_str(rest);
        }
        changed.push_str(ending);
    }

    let mut out = String::with_capacity(text.len() + changed.len());
    out.push_str(&slice_chars(text, 0, block_start));
    out.push_str(&changed);
    out.push_str(&slice_chars(text, block_end, char_count(text)));
    (out, block_start, block_start + changed.chars().count())
}

/// Sort complete lines in a selection.
pub fn sort_lines(
    text: &str,
    start: usize,
    end: usize,
    ascending: bool,
    case_insensitive: bool,
) -> (String, usize, usize) {
    let index = LineIndex::new(text);
    let (first, last) = index.selection_lines(start, end);
    let block_start = index.line_start(first);
    let block_end = index.line_end_including_newline(last);
    let block = slice_chars(text, block_start, block_end);
    let trailing_newline = block.ends_with('\n') || block.ends_with('\r');
    let eol = crate::eol::detect(&block);
    let body = block.trim_end_matches(['\r', '\n']);
    let normalized = crate::eol::to_lf(body);
    let mut lines: Vec<&str> = if body.is_empty() {
        Vec::new()
    } else {
        normalized.split('\n').collect()
    };
    lines.sort_by(|a, b| {
        let (a, b) = if case_insensitive {
            (a.to_lowercase(), b.to_lowercase())
        } else {
            (a.to_string(), b.to_string())
        };
        a.cmp(&b)
    });
    if !ascending {
        lines.reverse();
    }
    let mut sorted = lines.join(eol.as_str());
    if trailing_newline {
        sorted.push_str(eol.as_str());
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&slice_chars(text, 0, block_start));
    out.push_str(&sorted);
    out.push_str(&slice_chars(text, block_end, char_count(text)));
    (out, block_start, block_start + sorted.chars().count())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseMode {
    Upper,
    Lower,
    Title,
    Toggle,
}

/// Apply case conversion to a character range.
pub fn transform_case(
    text: &str,
    start: usize,
    end: usize,
    mode: CaseMode,
) -> (String, usize, usize) {
    let (a, b) = (start.min(end), start.max(end));
    let selected = slice_chars(text, a, b);
    let converted = match mode {
        CaseMode::Upper => selected.to_uppercase(),
        CaseMode::Lower => selected.to_lowercase(),
        CaseMode::Title => {
            let mut out = String::with_capacity(selected.len());
            let mut at_word_start = true;
            for ch in selected.chars() {
                if ch.is_alphanumeric() {
                    if at_word_start {
                        out.extend(ch.to_uppercase());
                    } else {
                        out.extend(ch.to_lowercase());
                    }
                    at_word_start = false;
                } else {
                    out.push(ch);
                    at_word_start = true;
                }
            }
            out
        }
        CaseMode::Toggle => selected
            .chars()
            .flat_map(|ch| {
                if ch.is_uppercase() {
                    ch.to_lowercase().collect::<Vec<_>>()
                } else {
                    ch.to_uppercase().collect::<Vec<_>>()
                }
            })
            .collect(),
    };
    let mut out = String::with_capacity(text.len() + converted.len());
    out.push_str(&slice_chars(text, 0, a));
    out.push_str(&converted);
    out.push_str(&slice_chars(text, b, char_count(text)));
    (out, a, a + converted.chars().count())
}

/// Range of the word (identifier/whitespace run) at `index`.
pub fn word_range_at(text: &str, index: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return (0, 0);
    }
    let index = index.min(chars.len());
    let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
    let probe = if index == chars.len() {
        index.saturating_sub(1)
    } else {
        index
    };
    if !is_word(chars[probe]) {
        return (index, index);
    }
    let mut start = probe;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = probe + 1;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    (start, end)
}

/// Find the bracket matching the one adjacent to `caret` (a character index
/// between characters). Returns a pair of char indices.
pub fn matching_bracket(text: &str, caret: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let is_open = |c: char| matches!(c, '(' | '[' | '{');
    let is_close = |c: char| matches!(c, ')' | ']' | '}');
    let counterpart = |c: char| match c {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        ')' => '(',
        ']' => '[',
        '}' => '{',
        _ => c,
    };
    let candidates = [caret.saturating_sub(1), caret];
    for &i in &candidates {
        let Some(&ch) = chars.get(i) else { continue };
        if !(is_open(ch) || is_close(ch)) {
            continue;
        }
        let want = counterpart(ch);
        if is_open(ch) {
            let mut depth = 0isize;
            for (j, &c) in chars.iter().enumerate().skip(i) {
                if c == ch {
                    depth += 1;
                } else if c == want {
                    depth -= 1;
                    if depth == 0 {
                        return Some((i, j));
                    }
                }
            }
        } else {
            let mut depth = 0isize;
            for j in (0..=i).rev() {
                let c = chars[j];
                if c == ch {
                    depth += 1;
                } else if c == want {
                    depth -= 1;
                    if depth == 0 {
                        return Some((j, i));
                    }
                }
            }
        }
    }
    None
}

/// Compute the indentation to use for a newly inserted line.
pub fn auto_indent(text: &str, char_index: usize, unit: &str) -> String {
    let line_index = LineIndex::new(text);
    let line = line_index.line_of_char(char_index);
    let start = line_index.line_start(line);
    let content = slice_chars(text, start, char_index);
    let indent: String = content
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let trimmed = content.trim_end();
    let mut out = indent.clone();
    if trimmed.ends_with('{')
        || trimmed.ends_with('[')
        || trimmed.ends_with('(')
        || trimmed.ends_with(':')
    {
        out.push_str(unit);
    }
    out
}

/// Return the number of leading visual columns of `text`, expanding tabs.
pub fn visual_indent(text: &str, tab_size: usize) -> usize {
    let mut col = 0;
    for ch in text.chars() {
        match ch {
            ' ' => col += 1,
            '\t' => col += tab_size - (col % tab_size.max(1)),
            _ => break,
        }
    }
    col
}

/// Truncate a string for single-line display, adding an ellipsis.
pub fn preview(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out.replace('\t', "    ")
}

/// Whether a line consists only of a comment marker and text.
///
/// Comment-only lines are transparent for indentation folding: an outer block
/// continues past a comment even when the comment is written at column zero.
fn is_comment_only(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed.starts_with('#')
        || trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with("--")
        || trimmed.starts_with('%')
        || trimmed.starts_with(';')
}

/// Fold candidates for indentation-structured languages.
///
/// A candidate is `(start_line, end_line)`, both zero-based. The editor hides
/// the lines strictly between the two, so a block whose last child is line
/// `last` returns `end_line = last + 1`; the parent line stays visible as the
/// fold summary. Blank lines do not open or close blocks, and a candidate is
/// only emitted when it hides at least one line.
pub fn indent_fold_candidates(text: &str, tab_size: usize) -> Vec<(usize, usize)> {
    let index = LineIndex::new(text);
    let line_count = index.len();
    let mut candidates = Vec::new();
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut previous: Option<(usize, usize)> = None;

    for line in 0..line_count {
        let content = index.line_text(text, line);
        if content.trim().is_empty() || is_comment_only(&content) {
            continue;
        }
        let indent = visual_indent(&content, tab_size.max(1));
        while let Some(&(top_indent, start)) = stack.last() {
            if indent > top_indent {
                break;
            }
            stack.pop();
            let end = line.saturating_sub(1);
            if end > start {
                candidates.push((start, (end + 1).min(line_count)));
            }
        }
        if let Some((previous_indent, previous_line)) = previous {
            if indent > previous_indent {
                stack.push((previous_indent, previous_line));
            }
        }
        previous = Some((indent, line));
    }

    let last_line = line_count.saturating_sub(1);
    while let Some((_, start)) = stack.pop() {
        if last_line > start {
            candidates.push((start, (last_line + 1).min(line_count)));
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_byte_conversions() {
        let s = "aé世🙂z";
        assert_eq!(char_to_byte(s, 0), 0);
        assert_eq!(char_to_byte(s, 2), 3);
        assert_eq!(char_to_byte(s, 4), 10);
        assert_eq!(char_to_byte(s, 99), s.len());
        assert_eq!(byte_to_char(s, 3), 2);
        assert_eq!(byte_to_char(s, s.len()), 5);
        assert_eq!(slice_chars(s, 1, 3), "é世");
        assert_eq!(slice_chars(s, 3, 1), "é世");
    }

    #[test]
    fn indent_fold_candidates_follow_python_blocks() {
        let text = "def f():\n    a = 1\n    b = 2\n\nx = 3\n";
        let candidates = indent_fold_candidates(text, 4);
        assert!(candidates
            .iter()
            .any(|(start, end)| *start == 0 && *end >= 3));
        assert!(indent_fold_candidates("a\nb\nc\n", 4).is_empty());
    }

    #[test]
    fn indent_fold_candidates_nest_and_support_tabs() {
        let text = "def f():\n    if x:\n        a\n    b\nc\n";
        let candidates = indent_fold_candidates(text, 4);
        let starts: Vec<usize> = candidates.iter().map(|(start, _)| *start).collect();
        assert!(starts.contains(&0));
        assert!(starts.contains(&1));
        assert!(candidates.iter().all(|(start, end)| end > start));

        let makefile = "all:\n\tcc a.c\n\techo done\n";
        let candidates = indent_fold_candidates(makefile, 4);
        assert!(candidates.iter().any(|(start, _)| *start == 0));
    }

    #[test]
    fn indent_fold_candidates_ignore_comment_only_lines() {
        let text = "def f():\n    a\n# note\n    b\nx = 1\n";
        let candidates = indent_fold_candidates(text, 4);
        assert!(candidates
            .iter()
            .any(|(start, end)| *start == 0 && *end >= 4));
    }

    #[test]
    fn line_index_handles_mixed_endings() {
        let text = "a\r\nb\nc\rd";
        let idx = LineIndex::new(text);
        assert_eq!(idx.len(), 4);
        assert_eq!(idx.line_of_char(0), 0);
        assert_eq!(idx.line_of_char(3), 1);
        assert_eq!(idx.line_col(4), (2, 2));
        assert_eq!(idx.line_text(text, 1), "b");
        assert_eq!(idx.line_text(text, 2), "c");
        assert_eq!(idx.line_text(text, 3), "d");
        assert_eq!(idx.char_index(1, 99), 1);
        assert_eq!(idx.line_end_including_newline(0), 3);
    }

    #[test]
    fn line_index_trailing_newline() {
        let idx = LineIndex::new("a\n");
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.line_col(2), (2, 1));
        assert_eq!(idx.line_text("a\n", 1), "");
    }

    #[test]
    fn line_block_expands_selection() {
        let text = "one\ntwo\nthree\nfour";
        let (s, e) = line_block(text, 1, 5);
        assert_eq!(slice_chars(text, s, e), "one\ntwo\n");
        let (s, e) = line_block(text, 1, 8);
        assert_eq!(slice_chars(text, s, e), "one\ntwo\n");
        let (s, e) = line_block(text, 1, 9);
        assert_eq!(slice_chars(text, s, e), "one\ntwo\nthree\n");
    }

    #[test]
    fn deletes_and_duplicates_lines() {
        let text = "one\ntwo\nthree";
        let (out, pos) = delete_lines(text, 4, 4);
        assert_eq!(out, "one\nthree");
        assert_eq!(pos, 4);

        let (out, s, e) = duplicate_lines("one\ntwo\n", 0, 0);
        assert_eq!(out, "one\none\ntwo\n");
        assert_eq!(slice_chars(&out, s, e), "one\n");

        let (out, _, _) = duplicate_lines("last", 0, 0);
        assert_eq!(out, "last\nlast");
    }

    #[test]
    fn trims_trailing_whitespace_per_line() {
        assert_eq!(trim_trailing_whitespace("a  \nb\t \n c"), "a\nb\n c");
        assert_eq!(trim_trailing_whitespace("no-eol"), "no-eol");
        assert_eq!(trim_trailing_whitespace("a\r\nb  \r\n"), "a\r\nb\r\n");
    }

    #[test]
    fn converts_indentation() {
        assert_eq!(tabs_to_spaces("\t\ta\n \tb", 4), "        a\n    b");
        assert_eq!(spaces_to_tabs("        a\n    b   c", 4), "\t\ta\n\tb   c");
    }

    #[test]
    fn indents_and_outdents_selected_lines() {
        let text = "a\nb\n";
        let (out, _, _) = indent_lines(text, 0, 3, 4, true, false);
        assert_eq!(out, "    a\n    b\n");
        let (back, _, _) = indent_lines(&out, 0, 10, 4, true, true);
        assert_eq!(back, text);
    }

    #[test]
    fn toggles_line_comments() {
        let text = "a\n  b\n\n";
        let (out, _, _) = toggle_line_comments(text, 0, 3, "//");
        assert_eq!(out, "// a\n  // b\n\n");
        let (back, _, _) = toggle_line_comments(&out, 0, out.chars().count(), "//");
        assert_eq!(back, text);
    }

    #[test]
    fn sorts_selected_lines() {
        let text = "b\nc\na\n";
        let (out, _, _) = sort_lines(text, 0, 6, true, false);
        assert_eq!(out, "a\nb\nc\n");
        let (out, _, _) = sort_lines(text, 0, 6, false, false);
        assert_eq!(out, "c\nb\na\n");
    }

    #[test]
    fn transforms_case() {
        let (out, _, _) = transform_case("hello world", 0, 5, CaseMode::Upper);
        assert_eq!(out, "HELLO world");
        let (out, s, e) = transform_case("abc", 0, 3, CaseMode::Toggle);
        assert_eq!(out, "ABC");
        assert_eq!((s, e), (0, 3));
        let (out, _, _) = transform_case("hello world", 0, 11, CaseMode::Title);
        assert_eq!(out, "Hello World");
    }

    #[test]
    fn finds_words_and_brackets() {
        assert_eq!(word_range_at("hello world", 2), (0, 5));
        assert_eq!(word_range_at("hello world", 7), (6, 11));
        assert_eq!(matching_bracket("a(b[c]d)e", 2), Some((1, 7)));
        assert_eq!(matching_bracket("a(b[c]d)e", 4), Some((3, 5)));
        assert_eq!(matching_bracket("no brackets", 3), None);
    }

    #[test]
    fn computes_auto_indent() {
        assert_eq!(auto_indent("    foo", 7, "    "), "    ");
        assert_eq!(auto_indent("if x {", 6, "    "), "    ");
        assert_eq!(auto_indent("\tbar", 4, "\t"), "\t");
    }

    #[test]
    fn previews_long_text() {
        assert_eq!(preview("hello", 10), "hello");
        assert_eq!(preview("hello world", 5), "hello…");
    }
}

//! Line-oriented editing operations shared by the editor commands.
//!
//! Every function takes character indices (the GUI's unit), returns the new
//! text and a selection covering the edited block, and preserves the original
//! line terminators. Operations that only touch complete lines are kept here
//! so they can be unit-tested without GPUI.

use crate::eol;
use crate::text::LineIndex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    Text,
    Numeric,
    Column,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnPayload {
    Text(String),
    Numbers { start: i64, step: i64, width: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterOutcome {
    pub text: String,
    pub selection: (usize, usize),
    pub removed: usize,
    pub matched: usize,
}

#[derive(Clone, Debug)]
struct Line {
    content: String,
    ending: String,
}

impl Line {
    fn len(&self) -> usize {
        self.content.chars().count() + self.ending.chars().count()
    }

    fn render(&self, out: &mut String) {
        out.push_str(&self.content);
        out.push_str(&self.ending);
    }
}

fn split_lines(text: &str) -> Vec<Line> {
    let index = LineIndex::new(text);
    (0..index.len())
        .map(|line| {
            let start = index.line_start(line);
            let content_end = index.line_content_end(line);
            let end = index.line_end_including_newline(line);
            Line {
                content: crate::text::slice_chars(text, start, content_end),
                ending: crate::text::slice_chars(text, content_end, end),
            }
        })
        .collect()
}

fn render(lines: &[Line]) -> String {
    let mut out = String::new();
    for line in lines {
        line.render(&mut out);
    }
    out
}

fn offset_of(lines: &[Line], line: usize) -> usize {
    lines.iter().take(line).map(Line::len).sum()
}

fn selection_for(lines: &[Line], first: usize, last: usize) -> (usize, usize) {
    let start = offset_of(lines, first);
    let end = start + lines[first..=last].iter().map(Line::len).sum::<usize>();
    (start, end)
}

/// Lines affected by a command. Commands that operate on the whole document
/// when there is no selection use `whole_document`; line movement and the
/// column editor stay on the current line.
fn target_lines(
    index: &LineIndex,
    start: usize,
    end: usize,
    whole_document: bool,
) -> (usize, usize) {
    if whole_document && start == end && !index.is_empty() {
        // A trailing newline creates an empty final line in LineIndex; it is
        // the terminator of the previous line, not a line of its own.
        let ends_with_newline = index.line_end_including_newline(index.len() - 1)
            == index.char_count()
            && index.line_start(index.len() - 1) == index.char_count();
        let last = if ends_with_newline {
            index.len().saturating_sub(2)
        } else {
            index.len() - 1
        };
        (0, last)
    } else {
        index.selection_lines(start, end)
    }
}

/// Move the selected complete lines up or down by one position.
pub fn move_lines(text: &str, start: usize, end: usize, up: bool) -> (String, usize, usize) {
    let mut lines = split_lines(text);
    let index = LineIndex::new(text);
    let (first, last) = index.selection_lines(start, end);
    if up {
        if first == 0 {
            return (text.to_string(), start, end);
        }
        let previous = lines.remove(first - 1);
        lines.insert(last, previous);
        let new_first = first - 1;
        let selection = selection_for(&lines, new_first, new_first + (last - first));
        (render(&lines), selection.0, selection.1)
    } else {
        if last + 1 >= index.len() || last + 1 == lines.len() {
            return (text.to_string(), start, end);
        }
        // Moving the last (possibly empty) line down is a no-op.
        if lines[last + 1].content.is_empty() && lines[last + 1].ending.is_empty() {
            return (text.to_string(), start, end);
        }
        let next = lines.remove(last + 1);
        lines.insert(first, next);
        let selection = selection_for(&lines, first + 1, last + 1);
        (render(&lines), selection.0, selection.1)
    }
}

/// Join the selected lines (or the current line and the next one) with spaces.
pub fn join_lines(text: &str, start: usize, end: usize) -> (String, usize, usize) {
    let mut lines = split_lines(text);
    let index = LineIndex::new(text);
    let (first, mut last) = index.selection_lines(start, end);
    if first == last {
        if last + 1 >= index.len() || last + 1 >= lines.len() {
            return (text.to_string(), start, end);
        }
        if lines[last + 1].content.is_empty() && lines[last + 1].ending.is_empty() {
            return (text.to_string(), start, end);
        }
        last += 1;
    }
    if first > last {
        return (text.to_string(), start, end);
    }
    let mut joined = String::new();
    for (offset, line) in lines[first..=last].iter().enumerate() {
        if offset > 0 {
            joined.push(' ');
        }
        joined.push_str(line.content.trim_end_matches([' ', '\t']));
    }
    let ending = lines[last].ending.clone();
    let replacement = Line {
        content: joined,
        ending,
    };
    lines.splice(first..=last, [replacement]);
    let selection = selection_for(&lines, first, first);
    (render(&lines), selection.0, selection.1)
}

/// Split each selected line at the given 1-based character column.
pub fn split_lines_at(
    text: &str,
    start: usize,
    end: usize,
    column: usize,
) -> (String, usize, usize) {
    let mut lines = split_lines(text);
    let index = LineIndex::new(text);
    let (first, last) = index.selection_lines(start, end);
    if column == 0 {
        return (text.to_string(), start, end);
    }
    let default_eol = eol::detect(text).as_str().to_string();
    let mut replacement = Vec::new();
    for line in &lines[first..=last] {
        let chars: Vec<char> = line.content.chars().collect();
        if column > chars.len() || chars.is_empty() {
            replacement.push(line.clone());
            continue;
        }
        let left: String = chars[..column].iter().collect();
        let right: String = chars[column..].iter().collect();
        let ending = if line.ending.is_empty() {
            default_eol.clone()
        } else {
            line.ending.clone()
        };
        replacement.push(Line {
            content: left,
            ending,
        });
        replacement.push(Line {
            content: right,
            ending: line.ending.clone(),
        });
    }
    let added = replacement.len().saturating_sub(last - first + 1);
    lines.splice(first..=last, replacement);
    let new_last = last + added;
    let selection = selection_for(&lines, first, new_last);
    (render(&lines), selection.0, selection.1)
}

/// Remove duplicate lines, keeping the first occurrence.
pub fn remove_duplicate_lines(text: &str, start: usize, end: usize) -> (String, usize, usize) {
    let mut lines = split_lines(text);
    let index = LineIndex::new(text);
    let (first, last) = target_lines(&index, start, end, true);
    let mut seen = std::collections::HashSet::new();
    let mut kept = Vec::new();
    for line in &lines[first..=last] {
        if seen.insert(line.content.clone()) {
            kept.push(line.clone());
        }
    }
    let new_last = first + kept.len().saturating_sub(1);
    lines.splice(first..=last, kept);
    let selection = selection_for(&lines, first, new_last.max(first));
    (render(&lines), selection.0, selection.1)
}

/// Remove empty or whitespace-only lines.
pub fn remove_empty_lines(text: &str, start: usize, end: usize) -> (String, usize, usize) {
    let mut lines = split_lines(text);
    let index = LineIndex::new(text);
    let (first, last) = target_lines(&index, start, end, true);
    let kept: Vec<Line> = lines[first..=last]
        .iter()
        .filter(|line| !line.content.trim().is_empty())
        .cloned()
        .collect();
    let new_last = first + kept.len().saturating_sub(1);
    lines.splice(first..=last, kept);
    let selection = selection_for(&lines, first, new_last.max(first));
    (render(&lines), selection.0, selection.1)
}

/// Sort complete lines. `column` is 1-based and only used by [`SortMode::Column`].
pub fn sort_lines(
    text: &str,
    start: usize,
    end: usize,
    mode: SortMode,
    ascending: bool,
    ignore_case: bool,
    column: usize,
) -> (String, usize, usize) {
    let mut lines = split_lines(text);
    let index = LineIndex::new(text);
    let (first, last) = target_lines(&index, start, end, true);
    let mut block: Vec<Line> = lines[first..=last].to_vec();
    block.sort_by(|a, b| match mode {
        SortMode::Numeric => compare_numeric_ordered(a, b, ascending),
        _ => {
            let ordering = compare_lines(a, b, mode, ignore_case, column);
            if ascending {
                ordering
            } else {
                ordering.reverse()
            }
        }
    });
    let new_last = first + block.len().saturating_sub(1);
    lines.splice(first..=last, block);
    let selection = selection_for(&lines, first, new_last.max(first));
    (render(&lines), selection.0, selection.1)
}

fn compare_lines(
    a: &Line,
    b: &Line,
    mode: SortMode,
    ignore_case: bool,
    column: usize,
) -> std::cmp::Ordering {
    match mode {
        SortMode::Text => compare_text(&a.content, &b.content, ignore_case),
        SortMode::Numeric => compare_numeric(&a.content, &b.content),
        SortMode::Column => {
            let a = column_slice(&a.content, column);
            let b = column_slice(&b.content, column);
            compare_text(&a, &b, ignore_case)
        }
    }
}

fn column_slice(content: &str, column: usize) -> String {
    content
        .chars()
        .skip(column.saturating_sub(1))
        .collect::<String>()
}

fn compare_text(a: &str, b: &str, ignore_case: bool) -> std::cmp::Ordering {
    if ignore_case {
        a.to_lowercase().cmp(&b.to_lowercase())
    } else {
        a.cmp(b)
    }
}

fn compare_numeric_ordered(a: &Line, b: &Line, ascending: bool) -> std::cmp::Ordering {
    let a_number = a.content.trim().parse::<f64>();
    let b_number = b.content.trim().parse::<f64>();
    match (a_number, b_number) {
        (Ok(a), Ok(b)) => {
            let ordering = a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal);
            if ascending {
                ordering
            } else {
                ordering.reverse()
            }
        }
        // Non-numeric rows stay after numeric rows in both directions.
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => std::cmp::Ordering::Equal,
    }
}

fn compare_numeric(a: &str, b: &str) -> std::cmp::Ordering {
    let a_number = a.trim().parse::<f64>();
    let b_number = b.trim().parse::<f64>();
    match (a_number, b_number) {
        (Ok(a), Ok(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

/// Keep or remove lines containing `needle`.
pub fn filter_lines(
    text: &str,
    start: usize,
    end: usize,
    needle: &str,
    keep: bool,
    match_case: bool,
    regex: bool,
) -> Result<FilterOutcome, String> {
    let mut lines = split_lines(text);
    let index = LineIndex::new(text);
    let (first, last) = target_lines(&index, start, end, true);
    let matcher = if regex {
        let pattern = if match_case {
            needle.to_string()
        } else {
            format!("(?i:{needle})")
        };
        let compiled = regex::Regex::new(&pattern).map_err(|err| err.to_string())?;
        Box::new(move |content: &str| compiled.is_match(content)) as Box<dyn Fn(&str) -> bool>
    } else if match_case {
        let needle = needle.to_string();
        Box::new(move |content: &str| content.contains(&needle)) as Box<dyn Fn(&str) -> bool>
    } else {
        let needle = needle.to_lowercase();
        Box::new(move |content: &str| content.to_lowercase().contains(&needle))
            as Box<dyn Fn(&str) -> bool>
    };
    let mut removed = 0;
    let mut matched_count = 0;
    let kept: Vec<Line> = lines[first..=last]
        .iter()
        .filter(|line| {
            let matched = matcher(&line.content);
            if matched {
                matched_count += 1;
            }
            let keep_line = if keep { matched } else { !matched };
            if !keep_line {
                removed += 1;
            }
            keep_line
        })
        .cloned()
        .collect();
    let new_last = first + kept.len().saturating_sub(1);
    lines.splice(first..=last, kept);
    let selection = selection_for(&lines, first, new_last.max(first));
    Ok(FilterOutcome {
        text: render(&lines),
        selection,
        removed,
        matched: matched_count,
    })
}

/// Insert text or a number sequence at a fixed 1-based character column.
pub fn column_edit(
    text: &str,
    start: usize,
    end: usize,
    column: usize,
    payload: &ColumnPayload,
) -> (String, usize, usize) {
    let mut lines = split_lines(text);
    let index = LineIndex::new(text);
    let (first, last) = index.selection_lines(start, end);
    let column = column.max(1);
    let mut number = match payload {
        ColumnPayload::Numbers { start, .. } => *start,
        ColumnPayload::Text(_) => 0,
    };
    for line in &mut lines[first..=last] {
        let length = line.content.chars().count();
        if length < column - 1 {
            line.content.push_str(&" ".repeat(column - 1 - length));
        }
        let insert = match payload {
            ColumnPayload::Text(value) => value.clone(),
            ColumnPayload::Numbers { step, width, .. } => {
                let formatted = if *width == 0 {
                    number.to_string()
                } else {
                    format!("{number:0width$}", width = *width)
                };
                number += *step;
                formatted
            }
        };
        let chars: Vec<char> = line.content.chars().collect();
        let split = (column - 1).min(chars.len());
        let mut updated = String::new();
        updated.extend(chars[..split].iter());
        updated.push_str(&insert);
        updated.extend(chars[split..].iter());
        line.content = updated;
    }
    let selection = selection_for(&lines, first, last);
    (render(&lines), selection.0, selection.1)
}

/// Number of lines in `text`, matching [`LineIndex::len`].
pub fn line_count(text: &str) -> usize {
    LineIndex::new(text).len()
}

/// Character offset of a 1-based line number.
pub fn line_start(text: &str, line: usize) -> usize {
    LineIndex::new(text).line_start(line.saturating_sub(1))
}

/// 1-based line of a character offset.
pub fn line_of_char(text: &str, offset: usize) -> usize {
    LineIndex::new(text).line_of_char(offset) + 1
}

/// Clip a 1-based line number to the document.
pub fn clamp_line(text: &str, line: usize) -> usize {
    let count = line_count(text);
    line.clamp(1, count.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_lines_up_and_down() {
        let text = "a\nb\nc\n";
        let (out, s, e) = move_lines(text, 2, 2, true);
        assert_eq!(out, "b\na\nc\n");
        assert_eq!(&out[s..e], "b\n");
        let (out, s, e) = move_lines(&out, 1, 1, false);
        assert_eq!(out, "a\nb\nc\n");
        assert_eq!(&out[s..e], "b\n");
        assert_eq!(move_lines("a\nb", 1, 1, true).0, "a\nb");
        assert_eq!(move_lines("a\nb", 4, 4, false).0, "a\nb");
    }

    #[test]
    fn joins_selected_lines_with_spaces() {
        let text = "one  \ntwo\nthree";
        let (out, s, e) = join_lines(text, 1, 1);
        assert_eq!(out, "one two\nthree");
        assert_eq!(&out[s..e], "one two\n");
        let (out, _, _) = join_lines(text, 0, 15);
        assert_eq!(out, "one two three");
        assert_eq!(join_lines("a\nb", 4, 4).0, "a\nb");
    }

    #[test]
    fn splits_lines_at_a_column() {
        let text = "abcdef\nxy\n";
        let (out, s, e) = split_lines_at(text, 0, 0, 3);
        assert_eq!(out, "abc\ndef\nxy\n");
        assert_eq!(&out[s..e], "abc\ndef\n");
        // A column past the end leaves that line unchanged.
        let (out, _, _) = split_lines_at("ab\n", 0, 0, 9);
        assert_eq!(out, "ab\n");
        // Splitting the last line gives the first half a newline. The line has
        // no terminator of its own, so the document falls back to the platform
        // convention.
        let native = eol::Eol::native().as_str();
        let (out, _, _) = split_lines_at("abcd", 0, 0, 2);
        assert_eq!(out, format!("ab{native}cd"));
    }

    #[test]
    fn removes_duplicates_and_empty_lines() {
        let (out, _, _) = remove_duplicate_lines("a\nb\na\nc\nb\n", 0, 0);
        assert_eq!(out, "a\nb\nc\n");
        let (out, _, _) = remove_empty_lines("a\n\n  \nb\n", 0, 0);
        assert_eq!(out, "a\nb\n");
        assert_eq!(remove_empty_lines("\n\n", 0, 0).0, "");
    }

    #[test]
    fn sorts_text_numeric_and_by_column() {
        let (out, s, e) = sort_lines("b\nA\nc\n", 0, 0, SortMode::Text, true, false, 1);
        assert_eq!(out, "A\nb\nc\n");
        assert_eq!(&out[s..e], "A\nb\nc\n");
        let (out, _, _) = sort_lines("2\n10\n1\n", 0, 0, SortMode::Numeric, true, false, 1);
        assert_eq!(out, "1\n2\n10\n");
        let (out, _, _) = sort_lines("2\nx\n10\n", 0, 0, SortMode::Numeric, false, false, 1);
        assert_eq!(out, "10\n2\nx\n");
        let (out, _, _) = sort_lines("x:2\ny:10\nz:1\n", 0, 0, SortMode::Column, true, false, 3);
        assert_eq!(out, "z:1\ny:10\nx:2\n");
        let (out, _, _) = sort_lines("b\nA\n", 0, 0, SortMode::Text, true, true, 1);
        assert_eq!(out, "A\nb\n");
    }

    #[test]
    fn filters_lines_with_literal_and_regex() {
        let text = "alpha\nbeta\nALPHA\n";
        let out = filter_lines(text, 0, 0, "alpha", true, false, false).unwrap();
        assert_eq!(out.text, "alpha\nALPHA\n");
        assert_eq!(out.removed, 1);
        assert_eq!(out.matched, 2);
        let out = filter_lines(text, 0, 0, "missing", true, false, false).unwrap();
        assert_eq!(out.matched, 0);
        let out = filter_lines(text, 0, 0, "alpha", false, true, false).unwrap();
        assert_eq!(out.text, "beta\nALPHA\n");
        let out = filter_lines(text, 0, 0, r"^a.*a$", true, false, true).unwrap();
        assert_eq!(out.text, "alpha\nALPHA\n");
        assert!(filter_lines(text, 0, 0, "(", true, false, true).is_err());
    }

    #[test]
    fn column_editor_inserts_text_and_numbers() {
        let text = "a\nbb\nc";
        let (out, _, _) = column_edit(
            text,
            0,
            text.chars().count(),
            3,
            &ColumnPayload::Text("X".to_string()),
        );
        assert_eq!(out, "a X\nbbX\nc X");
        let (out, _, _) = column_edit(
            text,
            0,
            text.chars().count(),
            1,
            &ColumnPayload::Numbers {
                start: 1,
                step: 2,
                width: 3,
            },
        );
        assert_eq!(out, "001a\n003bb\n005c");
    }

    #[test]
    fn line_helpers_match_index() {
        assert_eq!(line_count("a\nb\n"), 3);
        assert_eq!(line_start("a\nb\n", 2), 2);
        assert_eq!(line_of_char("a\nb\n", 2), 2);
        assert_eq!(clamp_line("a\nb\n", 99), 3);
        assert_eq!(clamp_line("a", 0), 1);
    }
}

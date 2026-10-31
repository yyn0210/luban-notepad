//! Snippet selection and tab-stop navigation state.

use luban_core::snippet::{Snippet, SnippetExpansion, TabStop};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnippetSession {
    pub tab_id: u64,
    /// Insertion offset in character coordinates.
    pub offset: usize,
    pub stops: Vec<TabStop>,
    pub current: usize,
    pub inserted_chars: usize,
    pub final_cursor: usize,
}

impl SnippetSession {
    pub fn new(tab_id: u64, offset: usize, expansion: &SnippetExpansion) -> Self {
        Self {
            tab_id,
            offset,
            stops: expansion.stops.clone(),
            current: 0,
            inserted_chars: expansion.text.chars().count(),
            final_cursor: offset
                + expansion
                    .final_cursor
                    .unwrap_or_else(|| expansion.text.chars().count()),
        }
    }

    /// Re-anchor later stops after the current stop was edited.
    ///
    /// `selection_end` is the document offset after the edit. When the current
    /// stop changed length, the stops after it and the final cursor move by the
    /// same amount.
    pub fn adjust_for_edit(&mut self, selection_end: usize) {
        let Some(stop) = self.stops.get_mut(self.current) else {
            return;
        };
        let expected_end = self.offset + stop.end;
        let shift = selection_end as isize - expected_end as isize;
        if shift == 0 {
            return;
        }
        stop.end = selection_end.saturating_sub(self.offset).max(stop.start);
        for later in self.stops.iter_mut().skip(self.current + 1) {
            later.start = later.start.saturating_add_signed(shift);
            later.end = later.end.saturating_add_signed(shift);
        }
        self.final_cursor = self.final_cursor.saturating_add_signed(shift);
    }

    #[cfg(test)]
    pub fn current_stop(&self) -> Option<(usize, usize)> {
        self.stops
            .get(self.current)
            .map(|stop| (self.offset + stop.start, self.offset + stop.end))
    }

    /// Move to the next/previous stop. Returns `None` when the session ends.
    pub fn advance(&mut self, forward: bool) -> Option<(usize, usize)> {
        if self.stops.is_empty() {
            return None;
        }
        if forward {
            if self.current + 1 >= self.stops.len() {
                return None;
            }
            self.current += 1;
        } else if self.current == 0 {
            return None;
        } else {
            self.current -= 1;
        }
        self.stops
            .get(self.current)
            .map(|stop| (self.offset + stop.start, self.offset + stop.end))
    }
}

#[derive(Clone, Debug)]
pub struct SnippetRow {
    pub label: String,
    pub detail: String,
    pub snippet: Snippet,
}

/// Whether a snippet applies to every language.
fn is_global(snippet: &Snippet) -> bool {
    snippet
        .scope
        .as_deref()
        .is_none_or(|scope| scope.split(',').all(|selector| selector.trim().is_empty()))
}

/// Build the picker rows for the current language.
///
/// Snippets written for this language come before global ones; within each
/// group rows stay alphabetical.
pub fn rows(snippets: &[Snippet], language: &str, token: &str) -> Vec<SnippetRow> {
    let mut rows: Vec<SnippetRow> = snippets
        .iter()
        .filter(|snippet| snippet.matches_language(language, token))
        .map(|snippet| SnippetRow {
            label: snippet.name.clone(),
            detail: snippet
                .trigger
                .as_ref()
                .map(|trigger| format!("{trigger} · {}", snippet.source.display()))
                .unwrap_or_else(|| snippet.source.display().to_string()),
            snippet: snippet.clone(),
        })
        .collect();
    rows.sort_by(|a, b| {
        is_global(&a.snippet)
            .cmp(&is_global(&b.snippet))
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use luban_core::snippet::expand;
    use std::path::PathBuf;

    fn snippet(name: &str, scope: Option<&str>) -> Snippet {
        Snippet {
            name: name.into(),
            trigger: Some("t".into()),
            scope: scope.map(str::to_string),
            body: "x".into(),
            source: PathBuf::from("test.json"),
        }
    }

    #[test]
    fn filters_snippets_by_scope() {
        let snippets = vec![
            snippet("rust", Some("source.rust")),
            snippet("global", None),
            snippet("python", Some("source.python")),
        ];
        let rows = rows(&snippets, "Rust", "rust");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.label == "global"));
        assert!(rows.iter().any(|row| row.label == "rust"));
    }

    #[test]
    fn scoped_snippets_sort_before_global_ones() {
        let snippets = vec![
            snippet("alpha global", None),
            snippet("zebra rust", Some("source.rust")),
            snippet("beta global", Some("  ")),
            snippet("amber rust", Some("source.rust")),
        ];
        let rows = rows(&snippets, "Rust", "rust");
        let labels: Vec<&str> = rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(
            labels,
            ["amber rust", "zebra rust", "alpha global", "beta global"]
        );
    }

    #[test]
    fn session_shifts_later_stops_after_an_edit() {
        let expansion = expand("let ${1:name} = ${2:value};");
        let mut session = SnippetSession::new(1, 0, &expansion);
        // "name" replaced by "answer": the second stop moves by two chars.
        session.adjust_for_edit(10);
        assert_eq!(session.current_stop(), Some((4, 10)));
        assert_eq!(session.advance(true), Some((13, 18)));
        // "value" replaced by "9": the final cursor follows the shorter text.
        session.adjust_for_edit(14);
        assert_eq!(session.advance(true), None);
        assert_eq!(session.final_cursor, 15);
    }

    #[test]
    fn session_navigates_stops_and_ends() {
        let expansion = expand("a${1:one}b${2:two}c");
        let mut session = SnippetSession::new(7, 10, &expansion);
        assert_eq!(session.current_stop(), Some((11, 14)));
        assert_eq!(session.advance(true), Some((15, 18)));
        assert_eq!(session.advance(true), None);
        assert_eq!(session.advance(false), Some((11, 14)));
    }
}

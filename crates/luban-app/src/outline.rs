//! Symbol outline state and hierarchy projection.

use std::collections::HashSet;

use luban_core::symbols::{extract_symbols, Symbol};

use crate::i18n::tr;

#[derive(Clone, Debug, Default)]
pub struct OutlineState {
    pub symbols: Vec<Symbol>,
    pub language: String,
    pub revision: u64,
    pub truncated: bool,
    pub supported: bool,
    pub collapsed: HashSet<usize>,
    pub error: Option<String>,
}

impl OutlineState {
    /// Re-extract when the document revision or language changed. Empty and
    /// unsupported documents keep an empty outline with an explanatory state.
    pub fn refresh(&mut self, language: &str, text: &str, revision: u64) {
        if self.language == language && self.revision == revision {
            return;
        }
        self.language = language.to_string();
        self.revision = revision;
        self.error = None;
        if text.is_empty() {
            self.symbols.clear();
            self.truncated = false;
            self.supported = true;
            self.collapsed.clear();
            return;
        }
        let outcome = extract_symbols(language, text, luban_core::symbols::MAX_SYMBOLS);
        self.symbols = outcome.symbols;
        self.truncated = outcome.truncated;
        self.supported = outcome.supported;
        if !outcome.supported {
            self.error = Some(tr("outline.unsupported"));
        }
        self.collapsed
            .retain(|line| self.symbols.iter().any(|symbol| symbol.line == *line));
    }

    pub fn clear(&mut self) {
        self.symbols.clear();
        self.language.clear();
        self.revision = 0;
        self.truncated = false;
        self.supported = false;
        self.collapsed.clear();
        self.error = None;
    }

    pub fn toggle(&mut self, line: usize) {
        if !self.collapsed.remove(&line) {
            self.collapsed.insert(line);
        }
    }

    pub fn rows(&self) -> Vec<OutlineRow> {
        rows_for(&self.symbols, &self.collapsed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlineRow {
    pub index: usize,
    /// Nesting depth derived from indentation.
    pub depth: usize,
    pub has_children: bool,
    pub collapsed: bool,
}

/// Flatten symbols into visible rows, hiding descendants of collapsed ones.
pub fn rows_for(symbols: &[Symbol], collapsed: &HashSet<usize>) -> Vec<OutlineRow> {
    let mut rows = Vec::new();
    let mut stack: Vec<(usize, usize)> = Vec::new(); // (indent, symbol index)
    let mut hidden_at: Vec<usize> = Vec::new(); // indices of collapsed ancestors
    for (index, symbol) in symbols.iter().enumerate() {
        while stack
            .last()
            .is_some_and(|(indent, _)| *indent >= symbol.indent)
        {
            stack.pop();
        }
        let has_children = symbols
            .get(index + 1)
            .is_some_and(|next| next.indent > symbol.indent);
        let depth = stack.len();
        stack.push((symbol.indent, index));
        hidden_at.retain(|ancestor| {
            symbols
                .get(*ancestor)
                .is_some_and(|ancestor_symbol| ancestor_symbol.indent < symbol.indent)
        });
        if !hidden_at.is_empty() {
            continue;
        }
        rows.push(OutlineRow {
            index,
            depth,
            has_children,
            collapsed: collapsed.contains(&symbol.line),
        });
        if collapsed.contains(&symbol.line) && has_children {
            hidden_at.push(index);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(name: &str, line: usize, indent: usize) -> Symbol {
        Symbol {
            name: name.into(),
            kind: "fn".into(),
            line,
            indent,
        }
    }

    #[test]
    fn rows_reflect_indentation_depth() {
        let symbols = vec![
            symbol("impl", 1, 0),
            symbol("one", 2, 1),
            symbol("two", 3, 1),
            symbol("main", 5, 0),
        ];
        let rows = rows_for(&symbols, &HashSet::new());
        let depths: Vec<_> = rows.iter().map(|row| row.depth).collect();
        assert_eq!(depths, [0, 1, 1, 0]);
        assert!(rows[0].has_children);
        assert!(!rows[1].has_children);
    }

    #[test]
    fn collapsing_hides_descendants_until_the_next_peer() {
        let symbols = vec![
            symbol("impl", 1, 0),
            symbol("one", 2, 1),
            symbol("two", 3, 1),
            symbol("main", 5, 0),
        ];
        let collapsed = HashSet::from([1usize]);
        let rows = rows_for(&symbols, &collapsed);
        let names: Vec<_> = rows
            .iter()
            .map(|row| symbols[row.index].name.as_str())
            .collect();
        assert_eq!(names, ["impl", "main"]);
    }

    #[test]
    fn outline_state_refreshes_only_on_changes() {
        let mut outline = OutlineState::default();
        outline.refresh("Rust", "fn main() {}\n", 1);
        assert_eq!(outline.symbols.len(), 1);
        outline.refresh("Rust", "fn main() {}\n", 1);
        assert_eq!(outline.symbols.len(), 1);
        outline.refresh("Rust", "fn a() {}\nfn b() {}\n", 2);
        assert_eq!(outline.symbols.len(), 2);
        outline.toggle(1);
        assert!(outline.collapsed.contains(&1));
        outline.toggle(1);
        assert!(!outline.collapsed.contains(&1));
    }

    #[test]
    fn empty_and_unsupported_documents_have_states() {
        let mut outline = OutlineState::default();
        outline.refresh("Rust", "", 1);
        assert!(outline.symbols.is_empty());
        assert!(outline.supported);

        outline.refresh("Plain Text", "hello", 2);
        assert!(!outline.supported);
        assert!(outline.error.is_some());
    }
}

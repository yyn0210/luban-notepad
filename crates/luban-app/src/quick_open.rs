//! Quick open / Goto Anything row construction.
//!
//! The UI lives in `app.rs`; this module turns the parsed query, workspace
//! files, open tabs and outline symbols into rows and resolves the chosen row.

use std::path::PathBuf;

use luban_core::fuzzy::{self, QuickFile};
use luban_core::symbols::Symbol;

/// Maximum rows shown for a file query.
pub const MAX_QUICK_ROWS: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuickTarget {
    /// Open a path, optionally at a 1-based line.
    File { path: PathBuf, line: Option<u64> },
    /// Jump to a line inside a specific tab.
    Line { tab_id: u64, line: usize },
    /// Jump to a symbol in the active document.
    Symbol { tab_id: u64, line: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickRow {
    pub label: String,
    pub detail: String,
    pub target: QuickTarget,
}

/// One open tab offered to quick open.
#[derive(Clone, Debug)]
pub struct OpenFile {
    pub path: PathBuf,
    /// Path relative to its workspace root when known.
    pub relative: String,
    pub name: String,
    pub modified: bool,
}

/// Build file rows for a fuzzy query.
///
/// Open tabs come first, then other indexed workspace files. Duplicate paths
/// are removed. `line` is attached to every file target so `file:42` opens the
/// chosen file at that line.
pub fn file_rows(
    query: &str,
    line: Option<u64>,
    open: &[OpenFile],
    workspace_files: &[(String, PathBuf)],
    limit: usize,
) -> Vec<QuickRow> {
    let mut candidates: Vec<QuickFile> = Vec::new();
    let mut rows: Vec<QuickRow> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for file in open {
        seen.push(file.path.clone());
        candidates.push(QuickFile {
            relative: if file.relative.is_empty() {
                file.path.display().to_string()
            } else {
                file.relative.clone()
            },
            name: file.name.clone(),
            is_open: true,
        });
        rows.push(QuickRow {
            label: if file.modified {
                format!("● {}", file.name)
            } else {
                file.name.clone()
            },
            detail: if file.relative.is_empty() {
                file.path.display().to_string()
            } else {
                file.relative.clone()
            },
            target: QuickTarget::File {
                path: file.path.clone(),
                line,
            },
        });
    }
    for (relative, path) in workspace_files {
        if seen
            .iter()
            .any(|seen| luban_core::settings::paths_equal(seen, path))
        {
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| relative.clone());
        candidates.push(QuickFile {
            relative: relative.clone(),
            name: name.clone(),
            is_open: false,
        });
        rows.push(QuickRow {
            label: name,
            detail: relative.clone(),
            target: QuickTarget::File {
                path: path.clone(),
                line,
            },
        });
    }
    let ranked = fuzzy::rank_files(query, &candidates, limit.min(MAX_QUICK_ROWS));
    ranked
        .into_iter()
        .filter_map(|matched| rows.get(matched.index).cloned())
        .collect()
}

/// Rows for `@symbol` queries. An empty query lists every symbol.
pub fn symbol_rows(query: &str, symbols: &[Symbol], tab_id: u64) -> Vec<QuickRow> {
    let mut matches: Vec<(usize, i32)> = Vec::new();
    for (index, symbol) in symbols.iter().enumerate() {
        if query.is_empty() {
            matches.push((index, 0));
        } else if let Some(matched) = fuzzy::fuzzy_match(query, &symbol.name) {
            matches.push((index, matched.score));
        }
    }
    matches.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    matches
        .into_iter()
        .take(MAX_QUICK_ROWS)
        .map(|(index, _)| {
            let symbol = &symbols[index];
            QuickRow {
                label: symbol.name.clone(),
                detail: format!("{}  :{}", symbol.kind, symbol.line),
                target: QuickTarget::Symbol {
                    tab_id,
                    line: symbol.line,
                },
            }
        })
        .collect()
}

/// Build the single row for `:123`.
pub fn line_row(line: u64, tab_id: Option<u64>, total_lines: usize) -> Result<QuickRow, String> {
    let Some(tab_id) = tab_id else {
        return Err(crate::i18n::tr("command.goto.no_document"));
    };
    if line == 0 || line > total_lines.max(1) as u64 {
        return Err(crate::i18n::tr_args(
            "command.goto.invalid_line",
            &[
                ("line", line.to_string()),
                ("total", total_lines.max(1).to_string()),
            ],
        ));
    }
    Ok(QuickRow {
        label: format!(":{line}"),
        detail: crate::i18n::tr_args("command.goto.row_detail", &[("line", line.to_string())]),
        target: QuickTarget::Line {
            tab_id,
            line: line as usize,
        },
    })
}

/// A directory prefix typed by clicking a breadcrumb segment. The trailing
/// separator keeps the query unmistakably path-shaped.
pub fn directory_query(prefix: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_file(path: &str, relative: &str) -> OpenFile {
        OpenFile {
            path: PathBuf::from(path),
            relative: relative.to_string(),
            name: PathBuf::from(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            modified: false,
        }
    }

    #[test]
    fn open_files_rank_before_workspace_files() {
        let open = vec![open_file("/w/src/member.rs", "src/member.rs")];
        let files = vec![
            ("src/main.rs".to_string(), PathBuf::from("/w/src/main.rs")),
            (
                "src/member.rs".to_string(),
                PathBuf::from("/w/src/member.rs"),
            ),
        ];
        let rows = file_rows("m", None, &open, &files, 50);
        assert_eq!(rows.len(), 2);
        assert!(
            matches!(rows[0].target, QuickTarget::File { ref path, .. } if path.ends_with("member.rs"))
        );
    }

    #[test]
    fn file_queries_carry_the_line_suffix() {
        let files = vec![("src/main.rs".to_string(), PathBuf::from("/w/src/main.rs"))];
        let rows = file_rows("main", Some(42), &[], &files, 50);
        assert_eq!(rows.len(), 1);
        assert!(matches!(
            rows[0].target,
            QuickTarget::File { line: Some(42), .. }
        ));
    }

    #[test]
    fn limit_is_capped() {
        let files: Vec<(String, PathBuf)> = (0..80)
            .map(|index| {
                (
                    format!("f{index}.rs"),
                    PathBuf::from(format!("/w/f{index}.rs")),
                )
            })
            .collect();
        let rows = file_rows("", None, &[], &files, 500);
        assert_eq!(rows.len(), MAX_QUICK_ROWS);
    }

    #[test]
    fn symbol_rows_filter_and_sort() {
        let symbols = vec![
            Symbol {
                name: "main".into(),
                kind: "fn".into(),
                line: 1,
                indent: 0,
            },
            Symbol {
                name: "other".into(),
                kind: "fn".into(),
                line: 10,
                indent: 0,
            },
        ];
        let rows = symbol_rows("main", &symbols, 3);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target, QuickTarget::Symbol { tab_id: 3, line: 1 });
        assert_eq!(symbol_rows("", &symbols, 3).len(), 2);
    }

    #[test]
    fn line_rows_validate_the_document_and_range() {
        assert!(line_row(5, None, 10).is_err());
        assert!(line_row(0, Some(1), 10).is_err());
        assert!(line_row(11, Some(1), 10).is_err());
        let row = line_row(10, Some(1), 10).unwrap();
        assert_eq!(
            row.target,
            QuickTarget::Line {
                tab_id: 1,
                line: 10
            }
        );
    }

    #[test]
    fn directory_prefixes_end_with_a_separator() {
        assert_eq!(directory_query("src/ui"), "src/ui/");
        assert_eq!(directory_query("src/ui/"), "src/ui/");
        assert_eq!(directory_query(""), "");
    }
}

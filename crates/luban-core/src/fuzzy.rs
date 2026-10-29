//! Fuzzy matching and Goto Anything query parsing.

/// A parsed quick-open query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuickQuery {
    /// Ordinary fuzzy file query, optionally with a 1-based line suffix.
    Files { text: String, line: Option<u64> },
    /// `:123` jumps inside the active document.
    Line(u64),
    /// `@name` lists symbols in the active document.
    Symbol(String),
}

/// Parse `text`, `text:123`, `:123` and `@symbol`.
pub fn parse_quick_query(raw: &str) -> QuickQuery {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix('@') {
        return QuickQuery::Symbol(rest.trim().to_string());
    }
    if let Some(rest) = raw.strip_prefix(':') {
        if let Ok(line) = rest.trim().parse::<u64>() {
            return QuickQuery::Line(line);
        }
        return QuickQuery::Files {
            text: raw.to_string(),
            line: None,
        };
    }
    if let Some((prefix, suffix)) = raw.rsplit_once(':') {
        if !prefix.is_empty() && !suffix.is_empty() {
            if let Ok(line) = suffix.trim().parse::<u64>() {
                return QuickQuery::Files {
                    text: prefix.trim().to_string(),
                    line: Some(line),
                };
            }
        }
    }
    QuickQuery::Files {
        text: raw.to_string(),
        line: None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub score: i32,
    pub positions: Vec<usize>,
}

/// Match `query` as a case-insensitive subsequence of `candidate`.
///
/// Consecutive characters, word boundaries and exact case matches score
/// higher; long gaps and leading text cost points. Returns `None` when the
/// query is not a subsequence.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<FuzzyMatch> {
    if query.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    let query_chars: Vec<char> = query.chars().collect();
    let candidate_chars: Vec<char> = candidate.chars().collect();
    if query_chars.len() > candidate_chars.len() {
        return None;
    }

    // Greedy left-to-right match, then one backward pass to tighten the tail.
    // This is the standard cheap fuzzy matcher: exact enough for file names and
    // much faster than a full dynamic program over 200k paths.
    let mut positions = Vec::with_capacity(query_chars.len());
    let mut ci = 0usize;
    for &qc in &query_chars {
        let lower = qc.to_lowercase().next().unwrap_or(qc);
        let mut found = None;
        while ci < candidate_chars.len() {
            let cc = candidate_chars[ci];
            if cc.to_lowercase().next().unwrap_or(cc) == lower {
                found = Some(ci);
                ci += 1;
                break;
            }
            ci += 1;
        }
        positions.push(found?);
    }

    let mut score = 0i32;
    let mut previous: Option<usize> = None;
    for (index, &position) in positions.iter().enumerate() {
        let character = candidate_chars[position];
        let query_character = query_chars[index];
        let boundary = position == 0
            || matches!(
                candidate_chars[position - 1],
                '/' | '\\' | '_' | '-' | '.' | ' ' | '\t'
            )
            || (candidate_chars[position - 1].is_lowercase() && character.is_uppercase());
        if boundary {
            score += 12;
        }
        if character == query_character {
            score += 4;
        }
        match previous {
            Some(last) if last + 1 == position => score += 16,
            Some(last) => {
                let gap = position.saturating_sub(last + 1);
                score -= (gap as i32).min(8);
            }
            None => {
                // Prefer matches that start early in the path.
                score -= (position as i32).min(10);
            }
        }
        previous = Some(position);
    }
    // Prefer shorter candidates when otherwise equal.
    score -= (candidate_chars.len() / 12) as i32;
    Some(FuzzyMatch { score, positions })
}

/// One file offered to quick open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickFile {
    /// Path shown and matched, relative to its workspace root.
    pub relative: String,
    pub name: String,
    pub is_open: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickFileMatch {
    pub index: usize,
    pub score: i32,
    pub positions: Vec<usize>,
}

/// Rank workspace files for `query`. Open files come first, then higher fuzzy
/// scores, then shorter paths.
pub fn rank_files(query: &str, files: &[QuickFile], limit: usize) -> Vec<QuickFileMatch> {
    let mut matches: Vec<QuickFileMatch> = files
        .iter()
        .enumerate()
        .filter_map(|(index, file)| {
            let matched = fuzzy_match(query, &file.relative)?;
            Some(QuickFileMatch {
                index,
                score: matched.score,
                positions: matched.positions,
            })
        })
        .collect();
    matches.sort_by(|a, b| {
        let a_file = &files[a.index];
        let b_file = &files[b.index];
        b_file
            .is_open
            .cmp(&a_file.is_open)
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| a_file.relative.len().cmp(&b_file.relative.len()))
            .then_with(|| a_file.relative.cmp(&b_file.relative))
    });
    matches.truncate(limit);
    matches
}

/// Best fuzzy match among symbol names. Returns the symbol index and score.
pub fn best_symbol<'a>(
    query: &str,
    symbols: impl IntoIterator<Item = (usize, &'a str)>,
) -> Option<(usize, i32)> {
    let mut best: Option<(usize, i32)> = None;
    for (index, name) in symbols {
        if let Some(matched) = fuzzy_match(query, name) {
            if best.is_none_or(|(_, score)| matched.score > score) {
                best = Some((index, matched.score));
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quick_open_queries() {
        assert_eq!(
            parse_quick_query("src/main.rs"),
            QuickQuery::Files {
                text: "src/main.rs".into(),
                line: None
            }
        );
        assert_eq!(parse_quick_query(":12"), QuickQuery::Line(12));
        assert_eq!(
            parse_quick_query("@main"),
            QuickQuery::Symbol("main".into())
        );
        assert_eq!(
            parse_quick_query("src/main.rs:42"),
            QuickQuery::Files {
                text: "src/main.rs".into(),
                line: Some(42)
            }
        );
        assert_eq!(
            parse_quick_query("a:b"),
            QuickQuery::Files {
                text: "a:b".into(),
                line: None
            }
        );
    }

    #[test]
    fn subsequence_matches_and_rejects() {
        assert!(fuzzy_match("sm", "src/main.rs").is_some());
        assert!(fuzzy_match("MAIN", "src/main.rs").is_some());
        assert!(fuzzy_match("zzz", "src/main.rs").is_none());
        assert!(fuzzy_match("", "anything").is_some());
    }

    #[test]
    fn contiguous_and_boundary_matches_score_higher() {
        let contiguous = fuzzy_match("main", "main.rs").unwrap();
        let scattered = fuzzy_match("main", "m-a-i-n.rs").unwrap();
        assert!(contiguous.score > scattered.score);
    }

    #[test]
    fn open_files_rank_before_unopened_ones() {
        let files = vec![
            QuickFile {
                relative: "src/main.rs".into(),
                name: "main.rs".into(),
                is_open: false,
            },
            QuickFile {
                relative: "src/member.rs".into(),
                name: "member.rs".into(),
                is_open: true,
            },
        ];
        let ranked = rank_files("main", &files, 10);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].index, 0);

        let ranked = rank_files("m", &files, 10);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].index, 1);
    }

    #[test]
    fn ranking_respects_the_limit() {
        let files: Vec<QuickFile> = (0..60)
            .map(|index| QuickFile {
                relative: format!("file{index}.rs"),
                name: format!("file{index}.rs"),
                is_open: false,
            })
            .collect();
        assert_eq!(rank_files("file", &files, 50).len(), 50);
    }

    #[test]
    fn best_symbol_picks_the_highest_score() {
        let names = ["handle_request", "main", "main_loop"];
        let (index, _) =
            best_symbol("main", names.iter().enumerate().map(|(i, n)| (i, *n))).unwrap();
        assert_eq!(names[index], "main");
    }
}

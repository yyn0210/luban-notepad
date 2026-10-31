//! Command-line parsing helpers shared by the binary entry point.

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct GotoTarget {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
}

/// Parse `path:line[:column]`.
///
/// Numbers are consumed from the right so Windows drive letters and paths that
/// contain colons keep working; an ambiguous path ending in `:<number>` is
/// interpreted as a line number.
pub fn parse_goto(text: &str) -> Result<GotoTarget, String> {
    let mut rest = text;
    let mut numbers = Vec::new();
    for _ in 0..2 {
        let Some((head, tail)) = rest.rsplit_once(':') else {
            break;
        };
        if tail.is_empty() || !tail.chars().all(|ch| ch.is_ascii_digit()) {
            break;
        }
        let number = tail
            .parse::<usize>()
            .map_err(|error| format!("invalid number in --goto: {error}"))?;
        numbers.push(number);
        rest = head;
    }
    if numbers.is_empty() {
        return Err("--goto expects path:line[:column]".to_string());
    }
    if rest.is_empty() {
        return Err("--goto is missing a file path".to_string());
    }
    // Numbers are collected right-to-left: [column, line].
    let line = numbers.last().copied().unwrap_or(1).max(1);
    let column = numbers.first().copied().unwrap_or(1).max(1);
    let column = if numbers.len() > 1 { column } else { 1 };
    Ok(GotoTarget {
        path: PathBuf::from(rest),
        line,
        column,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_goto_paths_and_positions() {
        let target = parse_goto("src/main.rs:42:5").unwrap();
        assert_eq!(target.path, PathBuf::from("src/main.rs"));
        assert_eq!(target.line, 42);
        assert_eq!(target.column, 5);

        let target = parse_goto("C:\\work\\file.txt:7").unwrap();
        assert_eq!(target.path, PathBuf::from("C:\\work\\file.txt"));
        assert_eq!(target.line, 7);
        assert_eq!(target.column, 1);

        let target = parse_goto("file.txt:0:0").unwrap();
        assert_eq!(target.line, 1);
        assert_eq!(target.column, 1);

        assert!(parse_goto("file.txt").is_err());
        assert!(parse_goto(":12").is_err());
    }
}

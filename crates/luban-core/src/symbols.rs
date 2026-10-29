//! Regex-based symbol extraction for the outline panel.
//!
//! The extractor is intentionally heuristic: it recognises declaration-shaped
//! lines per language without parsing the file. Symbols carry a 1-based line,
//! an indentation level and a short kind label.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

/// Symbols beyond this limit are dropped and reported as truncated.
pub const MAX_SYMBOLS: usize = 5_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    /// 1-based line number.
    pub line: usize,
    /// Indentation in 4-column tabs, clamped for display.
    pub indent: usize,
}

#[derive(Clone, Debug, Default)]
pub struct SymbolOutcome {
    pub symbols: Vec<Symbol>,
    pub truncated: bool,
    /// False for languages without a symbol extractor.
    pub supported: bool,
}

#[derive(Clone, Copy)]
enum Kind {
    Fixed(&'static str),
    Heading,
}

struct Pattern {
    regex: Regex,
    name_group: usize,
    kind: Kind,
}

fn patterns() -> &'static HashMap<&'static str, Vec<Pattern>> {
    static PATTERNS: OnceLock<HashMap<&'static str, Vec<Pattern>>> = OnceLock::new();
    PATTERNS.get_or_init(build_patterns)
}

fn build_patterns() -> HashMap<&'static str, Vec<Pattern>> {
    let mut map = HashMap::new();
    let mut add = |language: &'static str, regex: &str, name_group: usize, kind: Kind| {
        match Regex::new(regex) {
            Ok(regex) => map.entry(language).or_insert_with(Vec::new).push(Pattern {
                regex,
                name_group,
                kind,
            }),
            Err(error) => log::warn!("symbol regex for {language} failed: {error}"),
        }
    };

    // Rust
    add(
        "Rust",
        r#"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?(?:async[ \t]+)?(?:unsafe[ \t]+)?(?:extern[ \t]+"[^"]*"[ \t]+)?(?:const[ \t]+)?fn[ \t]+([A-Za-z_][A-Za-z0-9_]*)"#,
        1,
        Kind::Fixed("fn"),
    );
    add(
        "Rust",
        r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?struct[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("struct"),
    );
    add(
        "Rust",
        r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?enum[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("enum"),
    );
    add(
        "Rust",
        r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?trait[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("trait"),
    );
    add(
        "Rust",
        r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?union[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("union"),
    );
    add(
        "Rust",
        r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?(?:unsafe[ \t]+)?impl(?:<[^>]*>)?[ \t]+(?:[A-Za-z0-9_:<>, ']+[ \t]+for[ \t]+)?([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("impl"),
    );
    add(
        "Rust",
        r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("mod"),
    );
    add(
        "Rust",
        r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?const[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("const"),
    );
    add(
        "Rust",
        r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?static[ \t]+(?:mut[ \t]+)?([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("static"),
    );
    add(
        "Rust",
        r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?type[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("type"),
    );
    add(
        "Rust",
        r"(?m)^[ \t]*macro_rules![ \t]*([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("macro"),
    );

    // Python
    add(
        "Python",
        r"(?m)^[ \t]*(?:async[ \t]+)?def[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("def"),
    );
    add(
        "Python",
        r"(?m)^[ \t]*class[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("class"),
    );

    // JavaScript / TypeScript
    for language in ["JavaScript", "TypeScript"] {
        add(
            language,
            r"(?m)^[ \t]*(?:export[ \t]+)?(?:default[ \t]+)?(?:async[ \t]+)?function[ \t]*\*?[ \t]*([A-Za-z_$][A-Za-z0-9_$]*)",
            1,
            Kind::Fixed("function"),
        );
        add(
            language,
            r"(?m)^[ \t]*(?:export[ \t]+)?(?:default[ \t]+)?(?:abstract[ \t]+)?class[ \t]+([A-Za-z_$][A-Za-z0-9_$]*)",
            1,
            Kind::Fixed("class"),
        );
        add(
            language,
            r"(?m)^[ \t]*(?:export[ \t]+)?interface[ \t]+([A-Za-z_$][A-Za-z0-9_$]*)",
            1,
            Kind::Fixed("interface"),
        );
        add(
            language,
            r"(?m)^[ \t]*(?:export[ \t]+)?type[ \t]+([A-Za-z_$][A-Za-z0-9_$]*)",
            1,
            Kind::Fixed("type"),
        );
        add(
            language,
            r"(?m)^[ \t]*(?:export[ \t]+)?(?:const[ \t]+)?enum[ \t]+([A-Za-z_$][A-Za-z0-9_$]*)",
            1,
            Kind::Fixed("enum"),
        );
        add(
            language,
            r"(?m)^[ \t]*(?:export[ \t]+)?const[ \t]+([A-Za-z_$][A-Za-z0-9_$]*)[ \t]*=[ \t]*(?:async[ \t]*)?(?:\([^)]*\)|[A-Za-z_$][A-Za-z0-9_$]*)[ \t]*=>",
            1,
            Kind::Fixed("arrow fn"),
        );
        add(
            language,
            r"(?m)^[ \t]+(?:public[ \t]+|private[ \t]+|protected[ \t]+|static[ \t]+|async[ \t]+|get[ \t]+|set[ \t]+)*([A-Za-z_$][A-Za-z0-9_$]*)[ \t]*\([^;{}]*\)[ \t]*(?::[ \t]*[^={]+)?\{",
            1,
            Kind::Fixed("method"),
        );
    }

    // C / C++
    for language in ["C", "C++"] {
        add(
            language,
            r"(?m)^[ \t]*(?:(?:static|inline|virtual|extern|constexpr|const|explicit|friend|unsigned|signed)[ \t]+)*[A-Za-z_][A-Za-z0-9_:<>,*& \t]*?[ \t*&]+([A-Za-z_][A-Za-z0-9_:~]*)[ \t]*\([^;{}]*\)[ \t]*(?:const[ \t]*)?(?:noexcept[ \t]*)?\{",
            1,
            Kind::Fixed("function"),
        );
        add(
            language,
            r"(?m)^[ \t]*(?:typedef[ \t]+)?(?:class|struct|union)[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
            1,
            Kind::Fixed("type"),
        );
        add(
            language,
            r"(?m)^[ \t]*enum[ \t]+(?:class[ \t]+)?([A-Za-z_][A-Za-z0-9_]*)",
            1,
            Kind::Fixed("enum"),
        );
        add(
            language,
            r"(?m)^[ \t]*namespace[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
            1,
            Kind::Fixed("namespace"),
        );
        add(
            language,
            r"(?m)^[ \t]*#[ \t]*define[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
            1,
            Kind::Fixed("define"),
        );
    }

    // Java / C# / Kotlin
    for language in ["Java", "C#", "Kotlin"] {
        add(
            language,
            r"(?m)^[ \t]*(?:(?:public|private|protected|internal|static|final|abstract|sealed|open|data|partial|record)[ \t]+)*(?:class|interface|enum|struct|object|record)[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
            1,
            Kind::Fixed("type"),
        );
        add(
            language,
            r"(?m)^[ \t]*(?:(?:public|private|protected|internal|static|final|abstract|synchronized|native|default|override|open|suspend|async|inline)[ \t]+)*(?:fun[ \t]+)?[A-Za-z_][A-Za-z0-9_<>,?\[\] \t.]*[ \t]+([A-Za-z_][A-Za-z0-9_]*)[ \t]*\([^;{}]*\)[ \t]*(?::[ \t]*[^={]+)?(?:throws[^{]+)?\{",
            1,
            Kind::Fixed("method"),
        );
        add(
            language,
            r"(?m)^[ \t]*(?:fun[ \t]+)(?:<[^>]*>[ \t]*)?([A-Za-z_][A-Za-z0-9_]*)",
            1,
            Kind::Fixed("fun"),
        );
    }
    add(
        "Kotlin",
        r"(?m)^[ \t]*(?:typealias[ \t]+)([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("type"),
    );

    // Go
    add(
        "Go",
        r"(?m)^[ \t]*func[ \t]+(?:\([^)]*\)[ \t]*)?([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("func"),
    );
    add(
        "Go",
        r"(?m)^[ \t]*type[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("type"),
    );
    add(
        "Go",
        r"(?m)^[ \t]*(?:var|const)[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("var"),
    );

    // Ruby
    add(
        "Ruby",
        r"(?m)^[ \t]*def[ \t]+(?:self\.)?([A-Za-z_][A-Za-z0-9_?!]*)",
        1,
        Kind::Fixed("def"),
    );
    add(
        "Ruby",
        r"(?m)^[ \t]*class[ \t]+([A-Za-z_][A-Za-z0-9_:]*)",
        1,
        Kind::Fixed("class"),
    );
    add(
        "Ruby",
        r"(?m)^[ \t]*module[ \t]+([A-Za-z_][A-Za-z0-9_:]*)",
        1,
        Kind::Fixed("module"),
    );

    // PHP
    add(
        "PHP",
        r"(?m)^[ \t]*(?:(?:public|private|protected|static|final|abstract)[ \t]+)*function[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("function"),
    );
    add(
        "PHP",
        r"(?m)^[ \t]*(?:abstract[ \t]+|final[ \t]+)?(?:class|interface|trait)[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
        1,
        Kind::Fixed("type"),
    );

    // Shell
    for language in ["Shell", "Batch"] {
        add(
            language,
            r"(?m)^[ \t]*function[ \t]+([A-Za-z_][A-Za-z0-9_-]*)",
            1,
            Kind::Fixed("function"),
        );
        add(
            language,
            r"(?m)^[ \t]*([A-Za-z_][A-Za-z0-9_-]*)[ \t]*\(\)[ \t]*\{",
            1,
            Kind::Fixed("function"),
        );
    }

    // Markdown
    add(
        "Markdown",
        r"(?m)^(#{1,6})[ \t]+(.+?)[ \t]*#*[ \t]*$",
        2,
        Kind::Heading,
    );

    map
}

/// Extract symbols for `language`. Returns an empty, unsupported outcome for
/// languages without patterns.
pub fn extract_symbols(language: &str, text: &str, max: usize) -> SymbolOutcome {
    let Some(patterns) = patterns().get(language) else {
        return SymbolOutcome::default();
    };
    let max = max.min(MAX_SYMBOLS);
    let mut symbols = Vec::new();
    for pattern in patterns {
        for captures in pattern.regex.captures_iter(text) {
            let Some(matched) = captures.get(pattern.name_group) else {
                continue;
            };
            let name = matched.as_str().trim();
            if name.is_empty() {
                continue;
            }
            let line_start = text[..matched.start()]
                .rfind('\n')
                .map(|index| index + 1)
                .unwrap_or(0);
            let (line, indent) = line_and_indent(text, line_start);
            let kind = match pattern.kind {
                Kind::Fixed(kind) => kind.to_string(),
                Kind::Heading => format!("h{}", captures[1].chars().count()),
            };
            symbols.push(Symbol {
                name: name.to_string(),
                kind,
                line,
                indent,
            });
            if symbols.len() >= max {
                symbols.sort_by_key(|symbol| symbol.line);
                dedup(&mut symbols);
                return SymbolOutcome {
                    symbols,
                    truncated: true,
                    supported: true,
                };
            }
        }
    }
    if language == "Markdown" {
        add_setext_headings(text, max, &mut symbols);
    }
    symbols.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.name.cmp(&b.name)));
    dedup(&mut symbols);
    if symbols.len() > max {
        symbols.truncate(max);
        SymbolOutcome {
            symbols,
            truncated: true,
            supported: true,
        }
    } else {
        SymbolOutcome {
            symbols,
            truncated: false,
            supported: true,
        }
    }
}

fn line_and_indent(text: &str, line_start: usize) -> (usize, usize) {
    let line = text[..line_start]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1;
    let mut columns = 0usize;
    for ch in text[line_start..].chars() {
        match ch {
            ' ' => columns += 1,
            '\t' => columns += 4,
            _ => break,
        }
    }
    (line, (columns / 4).min(16))
}

fn add_setext_headings(text: &str, max: usize, symbols: &mut Vec<Symbol>) {
    let lines: Vec<&str> = text.lines().collect();
    for index in 0..lines.len().saturating_sub(1) {
        let title = lines[index].trim();
        if title.is_empty() || title.starts_with('#') || title.starts_with('>') {
            continue;
        }
        let underline = lines[index + 1].trim();
        let kind = if underline.len() >= 2 && underline.chars().all(|ch| ch == '=') {
            "h1"
        } else if underline.len() >= 2 && underline.chars().all(|ch| ch == '-') {
            "h2"
        } else {
            continue;
        };
        symbols.push(Symbol {
            name: title.to_string(),
            kind: kind.to_string(),
            line: index + 1,
            indent: 0,
        });
        if symbols.len() >= max {
            return;
        }
    }
}

fn dedup(symbols: &mut Vec<Symbol>) {
    symbols.dedup_by(|a, b| a.line == b.line && a.name == b.name);
}

/// The innermost symbol whose indentation scope contains `line` (1-based).
pub fn containing_symbol(symbols: &[Symbol], line: usize) -> Option<&Symbol> {
    let mut stack: Vec<&Symbol> = Vec::new();
    for symbol in symbols.iter().filter(|symbol| symbol.line <= line) {
        while stack.last().is_some_and(|top| top.indent >= symbol.indent) {
            stack.pop();
        }
        stack.push(symbol);
    }
    stack.last().copied()
}

/// Line number of a symbol, clamped to `total_lines`.
pub fn symbol_line(symbol: &Symbol, total_lines: usize) -> usize {
    symbol.line.clamp(1, total_lines.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_declarations() {
        let text = r#"
mod demo;
pub struct Config { value: usize }
enum Mode { A, B }
trait Run { fn run(&self); }
impl Config {
    pub fn new(value: usize) -> Self { Self { value } }
}
fn main() {}
const LIMIT: usize = 1;
static GLOBAL: usize = 2;
type Alias = usize;
macro_rules! my_macro { () => {} }
"#;
        let outcome = extract_symbols("Rust", text, 100);
        let names: Vec<_> = outcome.symbols.iter().map(|s| s.name.as_str()).collect();
        for expected in [
            "demo", "Config", "Mode", "Run", "new", "main", "LIMIT", "GLOBAL", "Alias", "my_macro",
        ] {
            assert!(names.contains(&expected), "missing {expected} in {names:?}");
        }
    }

    #[test]
    fn extracts_python_and_tracks_indent() {
        let text =
            "class Widget:\n    def render(self):\n        pass\n\nasync def boot():\n    pass\n";
        let outcome = extract_symbols("Python", text, 100);
        assert_eq!(outcome.symbols.len(), 3);
        assert_eq!(outcome.symbols[0].name, "Widget");
        assert_eq!(outcome.symbols[0].indent, 0);
        assert_eq!(outcome.symbols[1].name, "render");
        assert_eq!(outcome.symbols[1].indent, 1);
        assert_eq!(outcome.symbols[2].name, "boot");
        assert_eq!(outcome.symbols[2].line, 5);
    }

    #[test]
    fn extracts_typescript_arrows_and_methods() {
        let text = "export function boot() {}\nexport class App {\n  render() {}\n}\nexport const run = async () => {};\ninterface Props {}\ntype Id = string;\nenum Mode { A }\n";
        let outcome = extract_symbols("TypeScript", text, 100);
        let names: Vec<_> = outcome.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"boot"));
        assert!(names.contains(&"App"));
        assert!(names.contains(&"render"));
        assert!(names.contains(&"run"));
        assert!(names.contains(&"Props"));
        assert!(names.contains(&"Id"));
        assert!(names.contains(&"Mode"));
    }

    #[test]
    fn extracts_markdown_atx_and_setext() {
        let text = "# Title\n## Section\ntext\nAnother\n=======\nSub\n---\n";
        let outcome = extract_symbols("Markdown", text, 100);
        let names: Vec<_> = outcome.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Title", "Section", "Another", "Sub"]);
        assert_eq!(outcome.symbols[3].kind, "h2");
    }

    #[test]
    fn unsupported_language_is_empty() {
        let outcome = extract_symbols("Plain Text", "fn main() {}", 100);
        assert!(!outcome.supported);
        assert!(outcome.symbols.is_empty());
    }

    #[test]
    fn truncates_at_the_limit() {
        let text = (0..20)
            .map(|index| format!("fn f{index}() {{}}\n"))
            .collect::<String>();
        let outcome = extract_symbols("Rust", &text, 5);
        assert_eq!(outcome.symbols.len(), 5);
        assert!(outcome.truncated);
    }

    #[test]
    fn containing_symbol_uses_indentation_scopes() {
        let text = "impl A {\n    fn one() {\n    }\n    fn two() {\n    }\n}\nfn outside() {}\n";
        let outcome = extract_symbols("Rust", text, 100);
        assert_eq!(containing_symbol(&outcome.symbols, 3).unwrap().name, "one");
        assert_eq!(containing_symbol(&outcome.symbols, 5).unwrap().name, "two");
        assert_eq!(
            containing_symbol(&outcome.symbols, 7).unwrap().name,
            "outside"
        );
        assert!(containing_symbol(&outcome.symbols, 0).is_none());
    }

    #[test]
    fn symbol_line_is_clamped() {
        let symbol = Symbol {
            name: "x".into(),
            kind: "fn".into(),
            line: 100,
            indent: 0,
        };
        assert_eq!(symbol_line(&symbol, 10), 10);
        assert_eq!(symbol_line(&symbol, 0), 1);
    }
}

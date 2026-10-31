//! User snippet packages and placeholder expansion.
//!
//! The parser accepts Sublime-compatible `.sublime-snippet` XML files and a
//! small JSON collection format. Expansion supports basic tab stops and
//! defaults; regex transformations and mirrors are intentionally out of scope.

use std::fs;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader;
use serde_json::Value;
use walkdir::WalkDir;

pub const MAX_SNIPPET_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_SNIPPETS: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    pub name: String,
    pub trigger: Option<String>,
    pub scope: Option<String>,
    pub body: String,
    pub source: PathBuf,
}

impl Snippet {
    /// Whether the snippet applies to a language.
    ///
    /// `token` is the syntax token used by the highlighter (for example
    /// `rust`). Scopes are matched against `source.<token>`, `text.<token>`,
    /// and the display language name; complex Sublime selectors are not
    /// supported.
    pub fn matches_language(&self, language: &str, token: &str) -> bool {
        let Some(scope) = &self.scope else {
            return true;
        };
        scope.split(',').any(|selector| {
            let selector = selector.trim();
            if selector.is_empty() {
                return true;
            }
            let selector = selector
                .strip_prefix("source.")
                .or_else(|| selector.strip_prefix("text."))
                .unwrap_or(selector);
            selector.eq_ignore_ascii_case(language) || selector.eq_ignore_ascii_case(token)
        })
    }
}

/// Whether a scope only uses selectors this host can evaluate.
///
/// Sublime scopes may contain descendant/group/negation selectors, which are
/// not supported. Such snippets are skipped (or only partially matched); the
/// caller logs these scopes so the limitation is visible.
pub fn scope_is_fully_supported(scope: &str) -> bool {
    scope.split(',').all(|selector| {
        let selector = selector.trim();
        if selector.is_empty() {
            return true;
        }
        let name = selector
            .strip_prefix("source.")
            .or_else(|| selector.strip_prefix("text."))
            .unwrap_or(selector);
        !name.is_empty()
            && !name.contains('.')
            && !name
                .chars()
                .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '+')))
    })
}

#[derive(Clone, Debug)]
pub struct SnippetLoadError {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct SnippetSet {
    pub snippets: Vec<Snippet>,
    pub errors: Vec<SnippetLoadError>,
}

impl SnippetSet {
    pub fn is_empty(&self) -> bool {
        self.snippets.is_empty()
    }

    /// Snippets that apply to a language, in package order.
    pub fn for_language<'a>(&'a self, language: &str, token: &str) -> Vec<&'a Snippet> {
        self.snippets
            .iter()
            .filter(|snippet| snippet.matches_language(language, token))
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabStop {
    pub index: usize,
    /// Character offsets relative to the expanded snippet text.
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnippetExpansion {
    pub text: String,
    pub stops: Vec<TabStop>,
    pub final_cursor: Option<usize>,
    pub warnings: Vec<String>,
}

/// Expand `$1`, `${1:default}` and `$0` placeholders.
pub fn expand(body: &str) -> SnippetExpansion {
    let chars: Vec<char> = body.chars().collect();
    let mut output = String::with_capacity(body.len());
    let mut stops = Vec::new();
    let mut final_cursor = None;
    let mut warnings = Vec::new();
    let mut position = 0usize;
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '\\' && chars.get(index + 1) == Some(&'$') {
            output.push('$');
            position += 1;
            index += 2;
            continue;
        }
        if ch != '$' {
            output.push(ch);
            position += 1;
            index += 1;
            continue;
        }
        if chars
            .get(index + 1)
            .is_some_and(|next| next.is_ascii_digit())
        {
            let mut end = index + 1;
            let mut number = 0usize;
            while let Some(digit) = chars.get(end).and_then(|ch| ch.to_digit(10)) {
                number = number.saturating_mul(10).saturating_add(digit as usize);
                end += 1;
            }
            if number == 0 {
                final_cursor = Some(position);
            } else {
                stops.push(TabStop {
                    index: number,
                    start: position,
                    end: position,
                });
            }
            index = end;
            continue;
        }
        if chars.get(index + 1) == Some(&'{') {
            match parse_braced(&chars, index) {
                Ok(Some((number, default, next))) => {
                    let start = position;
                    output.push_str(&default);
                    position += default.chars().count();
                    if number == 0 {
                        final_cursor = Some(position);
                    } else {
                        stops.push(TabStop {
                            index: number,
                            start,
                            end: position,
                        });
                    }
                    index = next;
                    continue;
                }
                Ok(None) => {
                    warnings.push(
                        "unsupported ${...} placeholder was kept as literal text".to_string(),
                    );
                }
                Err(message) => {
                    warnings.push(message);
                }
            }
        }
        output.push('$');
        position += 1;
        index += 1;
    }
    stops.sort_by_key(|stop| stop.index);
    SnippetExpansion {
        text: output,
        stops,
        final_cursor,
        warnings,
    }
}

/// Parse `${number}`, `${number:default}` or `${number|choices|}` at `start`.
///
/// Returns the parsed number, default text and the index after the closing
/// brace. `Ok(None)` means the construct is syntactically valid but uses an
/// unsupported feature and should be kept literal.
fn parse_braced(chars: &[char], start: usize) -> Result<Option<(usize, String, usize)>, String> {
    let mut index = start + 2;
    let mut number = 0usize;
    let mut digits = 0usize;
    while let Some(digit) = chars.get(index).and_then(|ch| ch.to_digit(10)) {
        number = number.saturating_mul(10).saturating_add(digit as usize);
        index += 1;
        digits += 1;
    }
    if digits == 0 {
        return Ok(None);
    }
    match chars.get(index) {
        Some('}') => Ok(Some((number, String::new(), index + 1))),
        Some(':') => {
            let mut depth = 1usize;
            let mut cursor = index + 1;
            let mut default = String::new();
            while cursor < chars.len() {
                match chars[cursor] {
                    '{' => {
                        depth += 1;
                        default.push('{');
                    }
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            return Ok(Some((number, default, cursor + 1)));
                        }
                        default.push('}');
                    }
                    ch => default.push(ch),
                }
                cursor += 1;
            }
            Err("unterminated snippet placeholder".to_string())
        }
        Some('|') => Ok(None),
        Some(_) => Ok(None),
        None => Err("unterminated snippet placeholder".to_string()),
    }
}

/// Load all snippets below a package directory.
pub fn load_snippets(dir: &Path) -> SnippetSet {
    let mut set = SnippetSet::default();
    if !dir.is_dir() {
        return set;
    }
    let mut paths = Vec::new();
    for entry in WalkDir::new(dir).follow_links(false).sort_by_file_name() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                set.errors.push(SnippetLoadError {
                    path: dir.to_path_buf(),
                    message: format!("cannot walk snippets: {error}"),
                });
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.ends_with(".sublime-snippet") || name.ends_with(".json") {
            paths.push(entry.path().to_path_buf());
        }
    }
    paths.sort();
    for path in paths {
        if set.snippets.len() >= MAX_SNIPPETS {
            set.errors.push(SnippetLoadError {
                path: path.clone(),
                message: format!("snippet limit of {MAX_SNIPPETS} reached"),
            });
            break;
        }
        match fs::metadata(&path) {
            Ok(metadata) if metadata.len() > MAX_SNIPPET_BYTES => {
                set.errors.push(SnippetLoadError {
                    path,
                    message: format!("snippet file is larger than {MAX_SNIPPET_BYTES} bytes"),
                });
                continue;
            }
            Err(error) => {
                set.errors.push(SnippetLoadError {
                    path,
                    message: format!("cannot read snippet file: {error}"),
                });
                continue;
            }
            _ => {}
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                set.errors.push(SnippetLoadError {
                    path,
                    message: format!("cannot read snippet file: {error}"),
                });
                continue;
            }
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let result = if name.to_ascii_lowercase().ends_with(".sublime-snippet") {
            parse_sublime_snippet(&text, &path).map(|snippet| vec![snippet])
        } else {
            parse_json_snippets(&text, &path)
        };
        match result {
            Ok(snippets) => {
                for snippet in snippets {
                    if set.snippets.len() >= MAX_SNIPPETS {
                        break;
                    }
                    set.snippets.push(snippet);
                }
            }
            Err(message) => set.errors.push(SnippetLoadError { path, message }),
        }
    }
    set
}

/// Parse a `.sublime-snippet` XML file.
pub fn parse_sublime_snippet(xml: &str, source: &Path) -> Result<Snippet, String> {
    let mut reader = Reader::from_str(xml);
    let mut content = None;
    let mut trigger = None;
    let mut scope = None;
    let mut description = None;
    let mut current: Option<&'static str> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                current = match event.name().as_ref() {
                    "content" => Some("content"),
                    "tabTrigger" => Some("trigger"),
                    "scope" => Some("scope"),
                    "description" => Some("description"),
                    _ => None,
                };
            }
            Ok(Event::End(_)) => current = None,
            Ok(Event::Text(event)) => {
                if let Some(field) = current {
                    let text = quick_xml::escape::unescape(event.as_ref())
                        .map_err(|error| format!("invalid XML text: {error}"))?
                        .to_string();
                    append_field(
                        field,
                        text,
                        &mut content,
                        &mut trigger,
                        &mut scope,
                        &mut description,
                        false,
                    );
                }
            }
            Ok(Event::CData(event)) => {
                if let Some(field) = current {
                    let text = event.as_ref().to_string();
                    append_field(
                        field,
                        text,
                        &mut content,
                        &mut trigger,
                        &mut scope,
                        &mut description,
                        true,
                    );
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(format!("invalid snippet XML: {error}")),
        }
    }
    let body = content.ok_or_else(|| "snippet has no <content>".to_string())?;
    if body.is_empty() {
        return Err("snippet content is empty".to_string());
    }
    let trigger = trigger
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let scope = scope
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let description = description
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let name = description
        .or_else(|| trigger.clone())
        .unwrap_or_else(|| "Snippet".to_string());
    Ok(Snippet {
        name,
        trigger,
        scope,
        body,
        source: source.to_path_buf(),
    })
}

fn append_field(
    field: &'static str,
    text: String,
    content: &mut Option<String>,
    trigger: &mut Option<String>,
    scope: &mut Option<String>,
    description: &mut Option<String>,
    replace: bool,
) {
    let target = match field {
        "content" => content,
        "trigger" => trigger,
        "scope" => scope,
        _ => description,
    };
    match target {
        Some(existing) if !replace => existing.push_str(&text),
        _ => *target = Some(text),
    }
}

/// Parse a JSON snippet collection.
pub fn parse_json_snippets(json: &str, source: &Path) -> Result<Vec<Snippet>, String> {
    let value: Value =
        serde_json::from_str(json).map_err(|error| format!("invalid snippet JSON: {error}"))?;
    let items = match &value {
        Value::Array(items) => items.clone(),
        Value::Object(object) => match object.get("snippets") {
            Some(Value::Array(items)) => items.clone(),
            _ => {
                return Err("JSON snippet file must be an array or contain a snippets array".into())
            }
        },
        _ => return Err("JSON snippet file must be an array or object".into()),
    };
    let mut snippets = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let object = item
            .as_object()
            .ok_or_else(|| format!("snippet #{index} must be an object"))?;
        let body = match object.get("body") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(lines)) => {
                let mut text = String::new();
                for (line_index, line) in lines.iter().enumerate() {
                    let line = line
                        .as_str()
                        .ok_or_else(|| format!("snippet #{index} body line is not a string"))?;
                    if line_index > 0 {
                        text.push('\n');
                    }
                    text.push_str(line);
                }
                text
            }
            _ => return Err(format!("snippet #{index} has no body")),
        };
        if body.is_empty() {
            return Err(format!("snippet #{index} body is empty"));
        }
        let trigger = object
            .get("trigger")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        let name = object
            .get("name")
            .or_else(|| object.get("description"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .or_else(|| trigger.clone())
            .unwrap_or_else(|| format!("Snippet {}", index + 1));
        let scope = object
            .get("scope")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        snippets.push(Snippet {
            name,
            trigger,
            scope,
            body,
            source: source.to_path_buf(),
        });
    }
    Ok(snippets)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn char_slice(text: &str, start: usize, end: usize) -> String {
        text.chars()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect()
    }

    #[test]
    fn expands_plain_stops_defaults_and_final_cursor() {
        let expansion = expand("fn ${1:name}($2) {\n\t$0\n}");
        assert_eq!(expansion.text, "fn name() {\n\t\n}");
        assert_eq!(expansion.stops.len(), 2);
        assert_eq!(expansion.stops[0].index, 1);
        assert_eq!(
            char_slice(
                &expansion.text,
                expansion.stops[0].start,
                expansion.stops[0].end
            ),
            "name"
        );
        assert_eq!(expansion.stops[1].index, 2);
        assert_eq!(
            expansion.final_cursor,
            Some("fn name() {\n\t".chars().count())
        );
        assert!(expansion.warnings.is_empty());
    }

    #[test]
    fn keeps_escaped_and_unknown_placeholders_literal() {
        let expansion = expand(r"cost: \$5 and ${1|a,b|}");
        assert_eq!(expansion.text, r"cost: $5 and ${1|a,b|}");
        assert_eq!(expansion.warnings.len(), 1);
    }

    #[test]
    fn parses_sublime_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <snippet>
            <content><![CDATA[println!("${1:value}");]]></content>
            <tabTrigger>pl</tabTrigger>
            <scope>source.rust</scope>
            <description>Print line</description>
        </snippet>"#;
        let snippet = parse_sublime_snippet(xml, Path::new("test.sublime-snippet")).unwrap();
        assert_eq!(snippet.name, "Print line");
        assert_eq!(snippet.trigger.as_deref(), Some("pl"));
        assert_eq!(snippet.scope.as_deref(), Some("source.rust"));
        assert!(snippet.matches_language("Rust", "rust"));
        assert!(!snippet.matches_language("Python", "python"));
    }

    #[test]
    fn detects_unsupported_sublime_selectors() {
        assert!(scope_is_fully_supported("source.rust"));
        assert!(scope_is_fully_supported("source.rust, text.rust"));
        assert!(scope_is_fully_supported("  "));
        assert!(!scope_is_fully_supported("source.rust meta.function"));
        assert!(!scope_is_fully_supported("source.rust - source.toml"));
        assert!(!scope_is_fully_supported("text.html.markdown"));
    }

    #[test]
    fn parses_json_collections() {
        let json = r#"{
            "snippets": [
                { "name": "Rust main", "trigger": "main", "scope": "source.rust", "body": ["fn main() {", "    $0", "}"] },
                { "trigger": "log", "body": "luban.log(\"info\", \"$1\")" }
            ]
        }"#;
        let snippets = parse_json_snippets(json, Path::new("snippets.json")).unwrap();
        assert_eq!(snippets.len(), 2);
        assert_eq!(snippets[0].body, "fn main() {\n    $0\n}");
        assert_eq!(snippets[0].name, "Rust main");
        assert!(snippets[1].matches_language("Python", "python"));
    }

    #[test]
    fn loads_and_isolates_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("good.sublime-snippet"),
            "<snippet><content>hi</content></snippet>",
        )
        .unwrap();
        fs::write(dir.path().join("bad.json"), "{not json").unwrap();
        let set = load_snippets(dir.path());
        assert_eq!(set.snippets.len(), 1);
        assert_eq!(set.errors.len(), 1);
    }

    #[test]
    fn expansion_is_char_based_for_multibyte_text() {
        let expansion = expand("你好 ${1:世界}!");
        assert_eq!(expansion.text, "你好 世界!");
        let stop = &expansion.stops[0];
        assert_eq!(char_slice(&expansion.text, stop.start, stop.end), "世界");
    }
}

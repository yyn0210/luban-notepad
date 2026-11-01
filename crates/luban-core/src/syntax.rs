//! Shared syntect parsing and semantic-role naming.
//!
//! The editor highlighter and the plugin runtime both need the same syntax
//! set, the same language lookup and the same mapping from TextMate scopes to
//! the semantic roles understood by the theme. Keeping that logic here avoids
//! the two paths drifting apart.

use std::collections::HashMap;
use std::ops::Range;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// Highlights as byte ranges paired with a semantic role name.
pub type Highlights = Vec<(Range<usize>, &'static str)>;

/// Find a syntax by language token, name or one of the editor's aliases.
pub fn find_syntax(syntaxes: &SyntaxSet, language: &str) -> Option<SyntaxReference> {
    let alias = match language {
        "text" | "" => return None,
        "csharp" => "cs",
        "make" => "Makefile",
        "markdown" => "md",
        "embedded_template" | "ejs" => "erb",
        other => other,
    };
    [language, alias]
        .into_iter()
        .find_map(|token| syntaxes.find_syntax_by_token(token))
        .or_else(|| syntaxes.find_syntax_by_name(language))
        .filter(|syntax| syntax.name != "Plain Text")
        .cloned()
}

/// Parse `text` with `syntax` and return semantic-role highlights.
///
/// Ranges are ordered, non-overlapping and only cover spans that map to a
/// semantic role; callers that need full coverage must fill the gaps.
pub fn parse(syntax: &SyntaxReference, syntaxes: &SyntaxSet, text: &str) -> Highlights {
    let mut parser = ParseState::new(syntax);
    let mut scopes = ScopeStack::new();
    let mut names = HashMap::<Scope, Option<&'static str>>::new();
    let mut highlights = Highlights::new();
    let mut offset = 0;

    let mut push = |range: Range<usize>, scopes: &ScopeStack, highlights: &mut Highlights| {
        if range.is_empty() {
            return;
        }
        let name = scopes
            .scopes
            .iter()
            .rev()
            .find_map(|scope| *names.entry(*scope).or_insert_with(|| semantic_name(*scope)));
        if let Some(name) = name {
            match highlights.last_mut() {
                Some((last, last_name)) if last.end == range.start && *last_name == name => {
                    last.end = range.end;
                }
                _ => highlights.push((range, name)),
            }
        }
    };

    for line in LinesWithEndings::from(text) {
        if let Ok(operations) = parser.parse_line(line, syntaxes) {
            let mut cursor = 0;
            for (index, operation) in operations {
                push(offset + cursor..offset + index, &scopes, &mut highlights);
                let _ = scopes.apply(&operation);
                cursor = index;
            }
            push(
                offset + cursor..offset + line.len(),
                &scopes,
                &mut highlights,
            );
        }
        offset += line.len();
    }
    highlights
}

/// Parse `text` with the active syntax set and return contiguous spans that
/// cover the whole input. Unstyled gaps use the `primary` role.
pub fn highlight_spans(text: &str, language: &str) -> Vec<(String, &'static str)> {
    if text.is_empty() {
        return Vec::new();
    }
    let syntaxes = crate::user_syntax::active_set();
    let Some(syntax) = find_syntax(&syntaxes, language) else {
        return vec![(text.to_string(), "primary")];
    };
    let highlights = parse(&syntax, &syntaxes, text);
    let mut spans = Vec::new();
    let mut cursor = 0;
    for (range, role) in highlights {
        let start = range.start.max(cursor);
        let end = range.end.min(text.len());
        if start > cursor {
            push_span(&mut spans, &text[cursor..start], "primary");
        }
        if start < end {
            push_span(&mut spans, &text[start..end], role);
        }
        cursor = end.max(cursor);
    }
    if cursor < text.len() {
        push_span(&mut spans, &text[cursor..], "primary");
    }
    spans
}

fn push_span(spans: &mut Vec<(String, &'static str)>, text: &str, role: &'static str) {
    if text.is_empty() {
        return;
    }
    match spans.last_mut() {
        Some((last, last_role)) if *last_role == role => last.push_str(text),
        _ => spans.push((text.to_string(), role)),
    }
}

/// Map a TextMate scope to the semantic role used by the theme registry.
pub fn semantic_name(scope: Scope) -> Option<&'static str> {
    let scope = scope.build_string();
    let is = |prefix: &str| {
        scope == prefix
            || scope
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'))
    };

    let name = if is("source") || is("text") {
        "primary"
    } else if is("punctuation.definition.string")
        || is("punctuation.definition.comment")
        || is("invalid")
        || is("meta")
    {
        return None;
    } else if is("comment.block.documentation") || is("comment.line.documentation") {
        "comment.doc"
    } else if is("comment") {
        "comment"
    } else if is("constant.character.escape") {
        "string.escape"
    } else if is("string.regexp") {
        "string.regex"
    } else if is("string") {
        "string"
    } else if is("constant.numeric") {
        "number"
    } else if is("constant.language.boolean") {
        "boolean"
    } else if is("constant") || is("support.constant") || is("entity.name.constant") {
        "constant"
    } else if is("keyword.operator") {
        "operator"
    } else if is("keyword.control.import") || is("meta.preprocessor") {
        "preproc"
    } else if is("keyword") || is("storage.modifier") {
        "keyword"
    } else if is("storage.type") {
        if [
            "class",
            "enum",
            "function",
            "impl",
            "interface",
            "module",
            "namespace",
            "struct",
            "trait",
            "type",
            "let",
            "var",
            "const",
        ]
        .iter()
        .any(|word| scope.contains(word))
        {
            "keyword"
        } else {
            "type"
        }
    } else if is("entity.name.function")
        || is("support.function")
        || is("variable.function")
        || is("meta.function-call")
    {
        "function"
    } else if is("entity.name.type")
        || is("entity.name.class")
        || is("entity.name.struct")
        || is("entity.name.enum")
        || is("entity.name.trait")
        || is("entity.name.interface")
        || is("entity.other.inherited-class")
        || is("support.type")
        || is("support.class")
    {
        "type"
    } else if is("entity.name.tag") {
        "tag"
    } else if is("entity.other.attribute-name") {
        "attribute"
    } else if is("entity.name.label") {
        "label"
    } else if is("entity.name.section") || is("markup.heading") {
        "title"
    } else if is("variable.language") {
        "variable.special"
    } else if is("variable.other.member")
        || is("variable.other.property")
        || is("support.variable.property")
        || is("meta.mapping.key")
    {
        "property"
    } else if is("variable") {
        "variable"
    } else if is("markup.bold") {
        "emphasis.strong"
    } else if is("markup.italic") {
        "emphasis"
    } else if is("markup.underline.link") {
        "link_uri"
    } else if is("markup.raw") {
        "text.literal"
    } else if is("markup.list") {
        "punctuation.list_marker"
    } else if is("punctuation.section") || is("punctuation.definition.block") {
        "punctuation.bracket"
    } else if is("punctuation.separator") || is("punctuation.terminator") {
        "punctuation.delimiter"
    } else if is("punctuation") {
        "punctuation"
    } else {
        return None;
    };
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use two_face::syntax::extra_newlines;

    #[test]
    fn resolves_known_languages_and_aliases() {
        let syntaxes = extra_newlines();
        assert!(find_syntax(&syntaxes, "rust").is_some());
        assert!(find_syntax(&syntaxes, "md").is_some());
        assert!(find_syntax(&syntaxes, "markdown").is_some());
        assert!(find_syntax(&syntaxes, "text").is_none());
        assert!(find_syntax(&syntaxes, "no-such-language").is_none());
    }

    #[test]
    fn highlights_keywords_with_semantic_roles() {
        let syntaxes = extra_newlines();
        let syntax = find_syntax(&syntaxes, "rust").unwrap();
        let highlights = parse(&syntax, &syntaxes, "fn main() {}\n");
        assert!(highlights.iter().any(|(_, name)| *name == "keyword"));
        assert!(highlights.iter().any(|(_, name)| *name == "primary"));
        assert_eq!(
            semantic_name(Scope::new("source.rust").unwrap()),
            Some("primary")
        );
    }

    #[test]
    fn spans_cover_the_whole_text() {
        let spans = highlight_spans("fn main() { let x = \"s\"; }\n", "rust");
        assert!(!spans.is_empty());
        let joined: String = spans.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(joined, "fn main() { let x = \"s\"; }\n");
        assert!(spans.iter().any(|(_, role)| *role == "keyword"));
        assert!(spans.iter().any(|(_, role)| *role == "string"));
    }

    #[test]
    fn unknown_language_and_empty_text_fall_back() {
        assert_eq!(
            highlight_spans("hello", "no-such-language"),
            vec![("hello".to_string(), "primary")]
        );
        assert!(highlight_spans("", "rust").is_empty());
    }
}

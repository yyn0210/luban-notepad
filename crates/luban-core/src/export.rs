//! Standalone HTML and RTF export.
//!
//! The export reads a document's text and renders it with the same
//! syntect/TextMate theme the editor uses. It never touches the document, the
//! undo stack or the session.

use std::fmt::Write as _;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Color, FontStyle, Style, Theme};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Html,
    Rtf,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Rtf => "rtf",
        }
    }
}

fn syntax_for<'a>(
    syntaxes: &'a SyntaxSet,
    token: &str,
) -> Option<&'a syntect::parsing::SyntaxReference> {
    let token = match token {
        "text" | "Plain Text" => return None,
        "csharp" => "cs",
        "make" => "Makefile",
        "markdown" => "md",
        other => other,
    };
    syntaxes
        .find_syntax_by_token(token)
        .or_else(|| syntaxes.find_syntax_by_name(token))
}

fn runs(text: &str, token: &str, theme: &Theme) -> Vec<(Style, String)> {
    let syntaxes = crate::user_syntax::active_set();
    let Some(syntax) = syntax_for(&syntaxes, token) else {
        return vec![(
            Style {
                foreground: theme.settings.foreground.unwrap_or(Color::BLACK),
                background: theme.settings.background.unwrap_or(Color::WHITE),
                font_style: FontStyle::empty(),
            },
            text.to_string(),
        )];
    };
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(text) {
        match highlighter.highlight_line(line, &syntaxes) {
            Ok(parts) => {
                for (style, piece) in parts {
                    if !piece.is_empty() {
                        out.push((style, piece.to_string()));
                    }
                }
            }
            Err(_) => out.push((
                Style {
                    foreground: theme.settings.foreground.unwrap_or(Color::BLACK),
                    background: theme.settings.background.unwrap_or(Color::WHITE),
                    font_style: FontStyle::empty(),
                },
                line.to_string(),
            )),
        }
    }
    out
}

fn html_color(color: Option<Color>, fallback: Color) -> String {
    let color = color.unwrap_or(fallback);
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Render `text` as a standalone HTML document.
pub fn to_html(text: &str, token: &str, theme: &Theme, title: &str) -> String {
    let foreground = html_color(theme.settings.foreground, Color::BLACK);
    let background = html_color(theme.settings.background, Color::WHITE);
    let mut body = String::new();
    for (style, piece) in runs(text, token, theme) {
        let color = html_color(Some(style.foreground), Color::BLACK);
        let mut span = format!(r#"<span style="color:{color}""#);
        if style.font_style.contains(FontStyle::ITALIC) {
            span.push_str(";font-style:italic");
        }
        if style.font_style.contains(FontStyle::BOLD) {
            span.push_str(";font-weight:bold");
        }
        span.push('>');
        span.push_str(&html_escape(&piece));
        span.push_str("</span>");
        body.push_str(&span);
    }
    format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>{}</title>\n<style>\nbody {{ background:{background}; color:{foreground}; margin:0; }}\npre {{ margin:0; padding:16px; font-family:\"DejaVu Sans Mono\",\"Consolas\",\"Menlo\",monospace; font-size:13px; line-height:1.5; white-space:pre; }}\n</style>\n</head>\n<body>\n<pre>{}</pre>\n</body>\n</html>\n",
        html_escape(title),
        body
    )
}

fn rtf_escape(text: &str, out: &mut String) {
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        match ch {
            '\\' => out.push_str("\\\\"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '\r' => {
                if chars.get(index + 1) == Some(&'\n') {
                    index += 1;
                }
                out.push_str("\\par\n");
            }
            '\n' => out.push_str("\\par\n"),
            '\t' => out.push_str("\\tab "),
            ch if (ch as u32) < 0x20 => {}
            ch if (ch as u32) < 0x7f => out.push(ch),
            ch => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{}?", *unit as i32);
                }
            }
        }
        index += 1;
    }
}

fn rtf_color(color: Color) -> (u8, u8, u8) {
    (color.r, color.g, color.b)
}

/// Render `text` as a standalone RTF document.
pub fn to_rtf(text: &str, token: &str, theme: &Theme) -> String {
    let foreground = theme.settings.foreground.unwrap_or(Color::BLACK);
    let background = theme.settings.background.unwrap_or(Color::WHITE);
    let mut colors: Vec<(u8, u8, u8)> = vec![rtf_color(background), rtf_color(foreground)];
    let mut color_index = |color: Color| {
        let rgb = rtf_color(color);
        if let Some(index) = colors.iter().position(|existing| *existing == rgb) {
            return index + 1;
        }
        colors.push(rgb);
        colors.len()
    };
    let foreground_index = 2usize;
    let background_index = 1usize;
    let mut body = String::new();
    for (style, piece) in runs(text, token, theme) {
        let index = color_index(style.foreground);
        let _ = write!(body, "\\cf{index}");
        if style.font_style.contains(FontStyle::ITALIC) {
            body.push_str("\\i");
        }
        if style.font_style.contains(FontStyle::BOLD) {
            body.push_str("\\b");
        }
        rtf_escape(&piece, &mut body);
        if style.font_style.contains(FontStyle::ITALIC) {
            body.push_str("\\i0");
        }
        if style.font_style.contains(FontStyle::BOLD) {
            body.push_str("\\b0");
        }
    }
    let mut table = String::new();
    for (r, g, b) in &colors {
        let _ = write!(table, "\\red{r}\\green{g}\\blue{b};");
    }
    format!(
        "{{\\rtf1\\ansi\\ansicpg1252\\deff0\\uc1\n{{\\fonttbl{{\\f0\\fmodern\\fcharset0 Consolas;}}}}\n{{\\colortbl;{table}}}\n\\viewkind4\\pard\\f0\\fs20\\cb{background_index}\\cf{foreground_index} {body}\\par\n}}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use two_face::theme::{extra, EmbeddedThemeName};

    fn theme() -> Theme {
        extra().get(EmbeddedThemeName::InspiredGithub).clone()
    }

    #[test]
    fn html_escapes_markup_and_keeps_syntax_colors() {
        let html = to_html(
            "fn main() { let x = \"<&>\"; }\n",
            "rust",
            &theme(),
            "a < b",
        );
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<title>a &lt; b</title>"));
        assert!(html.contains("&lt;&amp;&gt;"));
        assert!(html.contains("<span style=\"color:#"));
        // The style attribute must be closed before the tag ends; browsers
        // recover from a missing quote, but the exported document is text a
        // user may post-process, so it has to be well-formed.
        assert!(!html.contains("#000000>"));
        for line in html.lines().filter(|line| line.contains("<span style=")) {
            assert!(line.contains("\">"), "unclosed style attribute: {line}");
        }
        assert!(html.contains("white-space:pre"));
    }

    #[test]
    fn html_without_language_is_plain_but_escaped() {
        let html = to_html("<tag> & \"quoted\"\n", "text", &theme(), "plain");
        assert!(html.contains("&lt;tag&gt; &amp; &quot;quoted&quot;"));
    }

    #[test]
    fn rtf_has_tables_and_escapes() {
        let rtf = to_rtf("hello {\\} <world>\n", "text", &theme());
        assert!(rtf.starts_with("{\\rtf1"));
        assert!(rtf.contains("{\\fonttbl"));
        assert!(rtf.contains("{\\colortbl;"));
        assert!(rtf.contains("\\\\") || rtf.contains("\\{"));
        assert!(rtf.contains("\\par"));
        assert!(rtf.contains("\\fs20"));
    }

    #[test]
    fn rtf_encodes_non_ascii_as_unicode() {
        let rtf = to_rtf("世界\n", "text", &theme());
        assert!(rtf.contains("\\u19990?"));
        assert!(rtf.contains("\\u30028?"));
    }
}

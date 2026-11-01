//! Syntect highlighter that honours the user syntax package set.
//!
//! gpui-component ships a process-wide static syntax set. The editor needs to
//! see `.sublime-syntax` files from the user package directory, so this module
//! implements the same `InputHighlighter` adapter against a syntax set supplied
//! by `luban_core::user_syntax`.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter,
    InputHighlighterFactory, Rope,
};
use gpui_kit::{HighlightStyle, SharedString, Task, Window};
use luban_core::syntax::{find_syntax, parse, Highlights};
use syntect::parsing::{SyntaxReference, SyntaxSet};

const SYNC_PARSE_MAX_BYTES: usize = 64 * 1024;
const PARSE_DEBOUNCE: Duration = Duration::from_millis(150);

/// A factory bound to the syntax set that is active in the application.
///
/// Plain-text and unrecognised languages still get a highlighter: it paints
/// the whole run with the theme's `primary` foreground. Without it the editor
/// falls back to the application palette, so a custom syntax theme would leave
/// syntax-less documents in the UI colour.
pub fn factory() -> InputHighlighterFactory {
    let syntaxes = luban_core::user_syntax::active_set();
    Rc::new(move |language| {
        Some(match find_syntax(&syntaxes, language) {
            Some(syntax) => Box::new(SyntectInputHighlighter::new(
                language,
                syntax,
                syntaxes.clone(),
            )) as Box<dyn InputHighlighter>,
            None => Box::new(PlainTextHighlighter::new(language)),
        })
    })
}

fn resolve_styles(
    highlights: &[(Range<usize>, &'static str)],
    range: &Range<usize>,
    resolver: &dyn HighlightStyleResolver,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let first = highlights.partition_point(|(highlight, _)| highlight.end <= range.start);
    let mut runs = Vec::new();
    let mut cursor = range.start;
    // A TextMate theme only lists the scopes it overrides; everything else
    // inherits the theme foreground. Without this fallback a semantic role
    // the theme does not define (variables, functions) would be painted in
    // the application palette instead of the theme colour.
    let primary = resolver.style("primary");
    let themed = |name: &str| resolver.style(name).or(primary).unwrap_or_default();
    let plain = || primary.unwrap_or_default();

    for (highlight, name) in &highlights[first..] {
        if highlight.start >= range.end {
            break;
        }
        let start = highlight.start.max(range.start);
        let end = highlight.end.min(range.end);
        if start >= end || end <= cursor {
            continue;
        }
        if cursor < start {
            runs.push((cursor..start, plain()));
        }
        runs.push((start..end, themed(name)));
        cursor = end;
    }

    if cursor < range.end {
        runs.push((cursor..range.end, plain()));
    }
    runs
}

fn shift(highlights: &mut Highlights, edit: &InputEdit) {
    let delta = edit.new_end_byte as isize - edit.old_end_byte as isize;
    highlights.retain_mut(|(range, _)| {
        if range.end <= edit.start_byte {
            true
        } else if range.start >= edit.old_end_byte {
            range.start = range.start.saturating_add_signed(delta);
            range.end = range.end.saturating_add_signed(delta);
            true
        } else {
            false
        }
    });
}

struct SyntectInputHighlighter {
    language: SharedString,
    syntax: SyntaxReference,
    syntaxes: Arc<SyntaxSet>,
    highlights: Rc<RefCell<Highlights>>,
    folds: Rc<RefCell<Vec<FoldRange>>>,
    parse_task: Option<Task<()>>,
}

/// Paints documents without a syntax in the theme's foreground.
struct PlainTextHighlighter {
    language: SharedString,
}

impl PlainTextHighlighter {
    fn new(language: &str) -> Self {
        Self {
            language: language.to_owned().into(),
        }
    }
}

impl InputHighlighter for PlainTextHighlighter {
    fn language(&self) -> SharedString {
        self.language.clone()
    }

    fn update(
        &mut self,
        _: Option<InputEdit>,
        _: &Rope,
        _: bool,
        _: &mut Window,
        _: &mut gpui_kit::Context<EditorState>,
    ) {
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        vec![(range.clone(), resolver.style("primary").unwrap_or_default())]
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

impl SyntectInputHighlighter {
    fn new(language: &str, syntax: SyntaxReference, syntaxes: Arc<SyntaxSet>) -> Self {
        Self {
            language: language.to_owned().into(),
            syntax,
            syntaxes,
            highlights: Rc::default(),
            folds: Rc::default(),
            parse_task: None,
        }
    }
}

impl InputHighlighter for SyntectInputHighlighter {
    fn language(&self) -> SharedString {
        self.language.clone()
    }

    fn update(
        &mut self,
        edit: Option<InputEdit>,
        text: &Rope,
        folding: bool,
        window: &mut Window,
        cx: &mut gpui_kit::Context<EditorState>,
    ) {
        if let Some(edit) = edit {
            shift(&mut self.highlights.borrow_mut(), &edit);
        }

        if text.len() <= SYNC_PARSE_MAX_BYTES {
            self.parse_task = None;
            let text = text.to_string();
            *self.highlights.borrow_mut() = parse(&self.syntax, &self.syntaxes, &text);
            *self.folds.borrow_mut() = if folding {
                brace_fold_ranges(&text)
            } else {
                Vec::new()
            };
            return;
        }

        let syntax = self.syntax.clone();
        let syntaxes = self.syntaxes.clone();
        let highlights = self.highlights.clone();
        let folds = self.folds.clone();
        let text = text.clone();
        self.parse_task = Some(cx.spawn_in(window, async move |entity, cx| {
            cx.background_executor().timer(PARSE_DEBOUNCE).await;
            let (parsed, folded) = cx
                .background_executor()
                .spawn(async move {
                    let text = text.to_string();
                    let folded = if folding {
                        brace_fold_ranges(&text)
                    } else {
                        Vec::new()
                    };
                    (parse(&syntax, &syntaxes, &text), folded)
                })
                .await;
            *highlights.borrow_mut() = parsed;
            *folds.borrow_mut() = folded.clone();
            let _ = entity.update(cx, |state, cx| {
                state.apply_highlighter_fold_candidates(folded, cx);
            });
        }));
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        resolve_styles(&self.highlights.borrow(), range, resolver)
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        self.folds.borrow().clone()
    }
}

fn brace_fold_ranges(text: &str) -> Vec<FoldRange> {
    let mut starts = Vec::new();
    let mut ranges = Vec::new();

    for (line_number, line) in text.lines().enumerate() {
        let mut chars = line.chars().peekable();
        let mut quote = None;
        let mut escaped = false;
        while let Some(character) = chars.next() {
            match quote {
                Some(open) => {
                    if escaped {
                        escaped = false;
                    } else if character == '\\' {
                        escaped = true;
                    } else if character == open {
                        quote = None;
                    }
                }
                None => match character {
                    '/' if chars.peek() == Some(&'/') => break,
                    '#' => break,
                    '"' | '\'' | '`' => quote = Some(character),
                    '{' => starts.push(line_number),
                    '}' => {
                        if let Some(start) = starts.pop() {
                            if line_number.saturating_sub(start) >= 2 {
                                ranges.push(FoldRange::new(start, line_number));
                            }
                        }
                    }
                    _ => {}
                },
            }
        }
    }
    ranges.sort_by_key(|range| range.start_line);
    ranges.dedup_by_key(|range| range.start_line);
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::blue;

    #[test]
    fn resolves_known_and_user_syntaxes() {
        let bundled = two_face::syntax::extra_newlines();
        assert!(find_syntax(&bundled, "rust").is_some());
        assert!(find_syntax(&bundled, "text").is_none());
        assert!(find_syntax(&bundled, "no-such-language").is_none());
    }

    struct PrimaryResolver;

    impl HighlightStyleResolver for PrimaryResolver {
        fn style(&self, name: &str) -> Option<HighlightStyle> {
            (name == "primary").then(|| HighlightStyle {
                color: Some(blue()),
                ..Default::default()
            })
        }
    }

    #[test]
    fn plain_text_and_unknown_languages_use_the_theme_foreground() {
        let factory = factory();
        for language in ["text", "", "no-such-language"] {
            let highlighter = factory(language).expect("syntax-less documents get a highlighter");
            let styles = highlighter.styles(&(0..5), &PrimaryResolver);
            assert_eq!(styles.len(), 1, "{language}");
            assert_eq!(styles[0].0, 0..5, "{language}");
            assert_eq!(styles[0].1.color, Some(blue()), "{language}");
        }
    }

    #[test]
    fn roles_the_theme_does_not_define_inherit_the_foreground() {
        let highlights = vec![(0..4, "variable"), (4..5, "comment"), (5..6, "primary")];
        let runs = resolve_styles(&highlights, &(0..6), &PrimaryResolver);
        assert_eq!(runs.first().unwrap().0.start, 0);
        assert_eq!(runs.last().unwrap().0.end, 6);
        for (range, style) in &runs {
            assert_eq!(style.color, Some(blue()), "range {range:?}");
        }
    }
}

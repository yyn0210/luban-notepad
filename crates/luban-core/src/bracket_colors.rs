//! Paired-bracket nesting scan for the bracket-colour decoration.

/// Maximum document size that receives bracket colours.
pub const MAX_BRACKET_COLOR_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BracketPair {
    /// Byte offset of the opening bracket.
    pub open: usize,
    /// Byte offset of the closing bracket.
    pub close: usize,
    /// Zero-based nesting depth shared by both characters.
    pub depth: usize,
}

/// Scan `text` and return every matched `()`, `[]` and `{}` pair with the
/// nesting depth at which it was opened. Unmatched brackets are ignored.
///
/// Strings and comments are not parsed, so brackets inside them take part in
/// pairing; this is the documented approximation for the feature.
pub fn scan_brackets(text: &str) -> Vec<BracketPair> {
    let mut stack: Vec<(char, usize, usize)> = Vec::new();
    let mut pairs = Vec::new();
    for (offset, ch) in text.char_indices() {
        match ch {
            '(' | '[' | '{' => stack.push((ch, offset, stack.len())),
            ')' | ']' | '}' => {
                let expected = match ch {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                // Find the nearest matching opener; a mismatched closer does
                // not disturb the stack (the scan is approximate by design).
                let position = stack.iter().rposition(|(open, _, _)| *open == expected);
                let Some(position) = position else {
                    continue;
                };
                let (_, open, depth) = stack.remove(position);
                pairs.push(BracketPair {
                    open,
                    close: offset,
                    depth,
                });
            }
            _ => {}
        }
    }
    pairs
}

/// Cycle a palette of at least four readable colours.
pub fn depth_color_index(depth: usize, palette_len: usize) -> usize {
    if palette_len == 0 {
        0
    } else {
        depth % palette_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_simple_brackets() {
        let pairs = scan_brackets("(a)");
        assert_eq!(pairs.len(), 1);
        assert_eq!((pairs[0].open, pairs[0].close, pairs[0].depth), (0, 2, 0));
    }

    #[test]
    fn nested_pairs_get_increasing_depth() {
        let pairs = scan_brackets("{[()]}");
        assert_eq!(pairs.len(), 3);
        let by_open: Vec<_> = pairs.iter().map(|pair| pair.depth).collect();
        assert_eq!(by_open, [2, 1, 0]);
        let outer = pairs.iter().find(|pair| pair.open == 0).unwrap();
        assert_eq!(outer.close, 5);
        assert_eq!(outer.depth, 0);
    }

    #[test]
    fn unmatched_brackets_are_ignored() {
        let pairs = scan_brackets("(a [b)");
        // The `)` closes `(`, leaving `[` unmatched.
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].open, 0);
        let pairs = scan_brackets("((( [)])");
        assert!(!pairs.is_empty());
    }

    #[test]
    fn unicode_offsets_are_byte_offsets() {
        let text = "é(x)";
        let pairs = scan_brackets(text);
        assert_eq!(pairs[0].open, 2);
        assert_eq!(pairs[0].close, 4);
    }

    #[test]
    fn mismatched_closers_do_not_close_other_types() {
        let pairs = scan_brackets("( [ ) ]");
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn colour_index_cycles_the_palette() {
        assert_eq!(depth_color_index(0, 4), 0);
        assert_eq!(depth_color_index(5, 4), 1);
        assert_eq!(depth_color_index(3, 0), 0);
    }
}

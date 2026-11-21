use std::sync::LazyLock;

use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

use crate::marker::MarkerKind;

/// Words and characters of a text, as the status bar shows them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextCounts {
    /// Words (Unicode word boundaries; punctuation and Markdown markers are not words).
    pub words: usize,
    /// Characters, spaces included, line breaks excluded.
    pub chars: usize,
}

/// Count the words and characters of Markdown `text`. The targets of links and images
/// (`](…)`) are left out: they are not text the reader sees.
pub fn count_text(text: &str) -> TextCounts {
    static LINK_TARGET: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\]\([^)\n]*\)").expect("valid regex"));
    let visible = LINK_TARGET.replace_all(text, "]");
    TextCounts {
        words: visible.unicode_words().count(),
        chars: visible.chars().filter(|&c| c != '\n' && c != '\r').count(),
    }
}

/// `n` with thousands separators (`12,345`).
pub fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Build the display string for context markers, handling nested checkbox compaction.
/// Returns a vector of (display_string, depth) tuples.
pub fn build_context_display(markers: &[MarkerKind]) -> Vec<(String, usize)> {
    let mut result = Vec::new();
    let mut depth = 0;
    let mut prev_was_checkbox = false;

    for (i, marker) in markers.iter().enumerate() {
        // Skip a list marker grouped with a preceding checkbox. `prev_was_checkbox` implies
        // a prior iteration ran, so `i > 0` here always holds.
        if marker.is_list_item() && prev_was_checkbox {
            depth += 1;
            continue;
        }

        // Increment depth before block-level markers (except first)
        if i > 0 && marker.is_block_level() {
            depth += 1;
        }

        // Determine display string (nested checkboxes use compact form)
        let display_str = match marker {
            MarkerKind::Checkbox { checked: false } if prev_was_checkbox => " ]".to_string(),
            MarkerKind::Checkbox { checked: true } if prev_was_checkbox => "x]".to_string(),
            _ => marker.status_bar_str(),
        };

        result.push((display_str, depth));
        prev_was_checkbox = marker.is_checkbox();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marker::{OrderedMarker, UnorderedMarker};

    fn unordered_list(marker: UnorderedMarker) -> MarkerKind {
        MarkerKind::ListItem {
            ordered: false,
            unordered_marker: Some(marker),
            ordered_marker: None,
            number: None,
        }
    }

    fn ordered_list(number: u32, style: OrderedMarker) -> MarkerKind {
        MarkerKind::ListItem {
            ordered: true,
            unordered_marker: None,
            ordered_marker: Some(style),
            number: Some(number),
        }
    }

    #[test]
    fn status_bar_str_blockquote() {
        assert_eq!(MarkerKind::BlockQuote.status_bar_str(), ">");
    }

    #[test]
    fn status_bar_str_unordered_list_minus() {
        assert_eq!(unordered_list(UnorderedMarker::Minus).status_bar_str(), "-");
    }

    #[test]
    fn status_bar_str_unordered_list_star() {
        assert_eq!(unordered_list(UnorderedMarker::Star).status_bar_str(), "*");
    }

    #[test]
    fn status_bar_str_unordered_list_plus() {
        assert_eq!(unordered_list(UnorderedMarker::Plus).status_bar_str(), "+");
    }

    #[test]
    fn status_bar_str_ordered_list_dot() {
        assert_eq!(ordered_list(1, OrderedMarker::Dot).status_bar_str(), "1.");
        assert_eq!(ordered_list(42, OrderedMarker::Dot).status_bar_str(), "42.");
    }

    #[test]
    fn status_bar_str_ordered_list_parenthesis() {
        assert_eq!(
            ordered_list(1, OrderedMarker::Parenthesis).status_bar_str(),
            "1)"
        );
        assert_eq!(
            ordered_list(10, OrderedMarker::Parenthesis).status_bar_str(),
            "10)"
        );
    }

    #[test]
    fn status_bar_str_checkbox_unchecked() {
        assert_eq!(
            MarkerKind::Checkbox { checked: false }.status_bar_str(),
            "[ ]"
        );
    }

    #[test]
    fn status_bar_str_checkbox_checked() {
        assert_eq!(
            MarkerKind::Checkbox { checked: true }.status_bar_str(),
            "[x]"
        );
    }

    #[test]
    fn status_bar_str_code_block_no_language() {
        assert_eq!(
            MarkerKind::CodeBlockFence {
                language: None,
                is_opening: true
            }
            .status_bar_str(),
            "```"
        );
    }

    #[test]
    fn status_bar_str_code_block_with_language() {
        assert_eq!(
            MarkerKind::CodeBlockFence {
                language: Some("rust".to_string()),
                is_opening: true
            }
            .status_bar_str(),
            "```rust"
        );
    }

    #[test]
    fn display_simple_list() {
        let markers = vec![unordered_list(UnorderedMarker::Minus)];
        let display = build_context_display(&markers);
        assert_eq!(display, vec![("-".to_string(), 0)]);
    }

    #[test]
    fn display_nested_lists() {
        let markers = vec![
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
        ];
        let display = build_context_display(&markers);
        assert_eq!(display, vec![("-".to_string(), 0), ("-".to_string(), 1)]);
    }

    #[test]
    fn display_list_with_checkbox() {
        // - [x] displays as "- [x]" at same depth
        let markers = vec![
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: true },
        ];
        let display = build_context_display(&markers);
        assert_eq!(display, vec![("-".to_string(), 0), ("[x]".to_string(), 0)]);
    }

    #[test]
    fn display_nested_checkboxes_compact() {
        // - [x] - [ ] displays as "- [x] ]" (nested checkbox compacted)
        let markers = vec![
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: true },
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: false },
        ];
        let display = build_context_display(&markers);
        // After [x], the next - is skipped, depth increments, then [ ] shows as " ]"
        assert_eq!(
            display,
            vec![
                ("-".to_string(), 0),
                ("[x]".to_string(), 0),
                (" ]".to_string(), 1)
            ]
        );
    }

    #[test]
    fn display_nested_checked_checkbox_compact() {
        // - [ ] - [x] displays as "- [ ]x]"
        let markers = vec![
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: false },
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: true },
        ];
        let display = build_context_display(&markers);
        assert_eq!(
            display,
            vec![
                ("-".to_string(), 0),
                ("[ ]".to_string(), 0),
                ("x]".to_string(), 1)
            ]
        );
    }

    #[test]
    fn display_blockquote_list() {
        let markers = vec![
            MarkerKind::BlockQuote,
            unordered_list(UnorderedMarker::Minus),
        ];
        let display = build_context_display(&markers);
        assert_eq!(display, vec![(">".to_string(), 0), ("-".to_string(), 1)]);
    }

    #[test]
    fn display_ordered_list() {
        let markers = vec![ordered_list(3, OrderedMarker::Dot)];
        let display = build_context_display(&markers);
        assert_eq!(display, vec![("3.".to_string(), 0)]);
    }

    #[test]
    fn display_code_block() {
        let markers = vec![MarkerKind::CodeBlockFence {
            language: Some("python".to_string()),
            is_opening: true,
        }];
        let display = build_context_display(&markers);
        assert_eq!(display, vec![("```python".to_string(), 0)]);
    }

    #[test]
    fn display_deeply_nested() {
        // > - [x] - [ ] - [x]
        let markers = vec![
            MarkerKind::BlockQuote,
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: true },
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: false },
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: true },
        ];
        let display = build_context_display(&markers);
        // > at depth 0, - [x] at depth 1, ] at depth 2, x] at depth 3
        assert_eq!(
            display,
            vec![
                (">".to_string(), 0),
                ("-".to_string(), 1),
                ("[x]".to_string(), 1),
                (" ]".to_string(), 2),
                ("x]".to_string(), 3),
            ]
        );
    }

    #[test]
    fn depth_cycles_after_six() {
        // 7 nested lists should cycle back to depth 0 for color
        let markers = vec![
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
        ];
        let display = build_context_display(&markers);
        assert_eq!(display.len(), 7);
        // Depths: 0, 1, 2, 3, 4, 5, 6
        assert_eq!(display[6].1, 6);
        // Color cycling happens in status_bar() with depth % 6
    }
}

#[cfg(test)]
mod count_tests {
    use super::*;

    #[test]
    fn counts_words_and_characters() {
        let c = count_text("# Title\n\nSome *bold* text, l'albero.\n");
        assert_eq!(c.words, 5, "Title, Some, bold, text, l'albero");
        assert_eq!(
            c.chars,
            "# Title".len() + "Some *bold* text, l'albero.".len()
        );
        // Link and image targets are not counted; their visible text is.
        let c = count_text("See [the docs](https://example.com/a/b) ![shot](img/x.png)");
        assert_eq!(c.words, 4, "See, the, docs, shot");
        assert_eq!(count_text(""), TextCounts::default());
        assert_eq!(count_text("città è\r\n").chars, 7);
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1000), "1,000");
        assert_eq!(group_thousands(1234567), "1,234,567");
    }
}

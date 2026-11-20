//! Markdown formatting commands as pure text edits: toggle inline styles (bold,
//! italic, …), set a paragraph style on the selected lines (heading, list, quote),
//! insert links, images and code blocks.
//!
//! Each function takes the document text and the selection and returns one
//! [`TextEdit`]: a byte range to replace, its replacement, and the selection afterwards.
//! The editor applies it as a single undo step.

use std::ops::Range;
use std::path::{Component, Path, PathBuf};

/// A replacement of `range` (in the old text) by `text`; `selection` is in the new text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub text: String,
    pub selection: Range<usize>,
}

/// An inline style: its opening and closing markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inline {
    Bold,
    Italic,
    Underline,
    Strikethrough,
    Highlight,
    Code,
}

impl Inline {
    fn markers(self) -> (&'static str, &'static str) {
        match self {
            Inline::Bold => ("**", "**"),
            Inline::Italic => ("*", "*"),
            Inline::Underline => ("<u>", "</u>"),
            Inline::Strikethrough => ("~~", "~~"),
            Inline::Highlight => ("==", "=="),
            Inline::Code => ("`", "`"),
        }
    }
}

/// Toggle an inline style on the selection. An empty selection applies to the word
/// under the cursor, or inserts an empty pair with the cursor inside.
pub fn toggle_inline(text: &str, selection: Range<usize>, style: Inline) -> TextEdit {
    let sel = if selection.is_empty() {
        word_at(text, selection.start).unwrap_or(selection)
    } else {
        selection
    };
    let (open, close) = style.markers();

    // Already styled around the selection: remove the markers.
    if let Some((before, after)) = surrounding_markers(text, &sel, style) {
        let inner = &text[sel.clone()];
        let start = sel.start - before;
        return TextEdit {
            range: start..sel.end + after,
            text: inner.to_string(),
            selection: start..start + inner.len(),
        };
    }
    // The selection itself includes the markers: remove them.
    let inner = &text[sel.clone()];
    if inner.len() >= open.len() + close.len()
        && inner.starts_with(open)
        && inner.ends_with(close)
        && !(style == Inline::Italic && inner.starts_with("**") && !inner.starts_with("***"))
    {
        let stripped = &inner[open.len()..inner.len() - close.len()];
        return TextEdit {
            range: sel.clone(),
            text: stripped.to_string(),
            selection: sel.start..sel.start + stripped.len(),
        };
    }
    TextEdit {
        range: sel.clone(),
        text: format!("{open}{inner}{close}"),
        selection: sel.start + open.len()..sel.start + open.len() + inner.len(),
    }
}

/// Lengths of the style's markers immediately around `sel`, if it is styled. Runs of
/// `*` are read as Markdown does: 1 = italic, 2 = bold, 3 = both.
fn surrounding_markers(text: &str, sel: &Range<usize>, style: Inline) -> Option<(usize, usize)> {
    let before = &text[..sel.start];
    let after = &text[sel.end..];
    match style {
        Inline::Bold | Inline::Italic => {
            let n = before.bytes().rev().take_while(|&b| b == b'*').count();
            let m = after.bytes().take_while(|&b| b == b'*').count();
            let run = n.min(m);
            let (k, present) = if style == Inline::Bold {
                (2, run >= 2)
            } else {
                (1, run == 1 || run == 3)
            };
            present.then_some((k, k))
        }
        _ => {
            let (open, close) = style.markers();
            (before.ends_with(open) && after.starts_with(close))
                .then_some((open.len(), close.len()))
        }
    }
}

/// The word (letters, digits, `_`) around `pos`, if any.
fn word_at(text: &str, pos: usize) -> Option<Range<usize>> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let start = text[..pos]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_word(c))
        .last()
        .map_or(pos, |(i, _)| i);
    let end = text[pos..]
        .char_indices()
        .find(|&(_, c)| !is_word(c))
        .map_or(text.len(), |(i, _)| pos + i);
    (start < end).then_some(start..end)
}

/// A paragraph style for whole lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    Normal,
    Heading(u8),
    Bulleted,
    Numbered,
    Quote,
}

/// Set (or, if every line already has it, remove) a paragraph style on the lines the
/// selection touches. Existing block markers are replaced; indentation is kept.
pub fn set_block(text: &str, selection: Range<usize>, style: Block) -> TextEdit {
    let start = text[..selection.start].rfind('\n').map_or(0, |i| i + 1);
    // A selection ending at the start of a line does not include that line.
    let end_pos = if selection.end > selection.start && text[..selection.end].ends_with('\n') {
        selection.end - 1
    } else {
        selection.end
    };
    let end = text[end_pos..]
        .find('\n')
        .map_or(text.len(), |i| end_pos + i);
    let block = &text[start..end];
    let lines: Vec<&str> = block.split('\n').collect();

    let parsed: Vec<(&str, Block, &str)> = lines.iter().map(|l| split_marker(l)).collect();
    let content_lines: Vec<&(&str, Block, &str)> = parsed
        .iter()
        .filter(|(_, _, rest)| !rest.is_empty())
        .collect();
    let all_have = !content_lines.is_empty()
        && content_lines.iter().all(|(_, b, _)| *b == style)
        && style != Block::Normal;
    let target = if all_have { Block::Normal } else { style };

    let single = lines.len() == 1;
    let mut number = 0;
    let mut out = Vec::with_capacity(lines.len());
    for (indent, _, rest) in &parsed {
        // Blank lines stay blank in a multi-line block.
        if rest.is_empty() && !single {
            out.push(String::new());
            continue;
        }
        let prefix = match target {
            Block::Normal => String::new(),
            Block::Heading(n) => format!("{} ", "#".repeat(n.clamp(1, 6) as usize)),
            Block::Bulleted => "- ".to_string(),
            Block::Numbered => {
                number += 1;
                format!("{number}. ")
            }
            Block::Quote => "> ".to_string(),
        };
        // Headings and quotes start at the margin; list items keep their nesting.
        let indent = match target {
            Block::Bulleted | Block::Numbered => *indent,
            _ => "",
        };
        out.push(format!("{indent}{prefix}{rest}"));
    }
    let new = out.join("\n");

    let selection = if selection.is_empty() && single {
        // Keep the caret on the same character of the text.
        let (indent, _, rest) = parsed[0];
        let old_text_start = start + lines[0].len() - rest.len();
        let new_text_start = start + new.len() - rest.len();
        let caret = selection.start.max(old_text_start) - old_text_start + new_text_start;
        let _ = indent;
        caret..caret
    } else {
        start..start + new.len()
    };
    TextEdit {
        range: start..end,
        text: new,
        selection,
    }
}

/// Split a line into indentation, its block marker (as a style), and the rest.
fn split_marker(line: &str) -> (&str, Block, &str) {
    let body = line.trim_start_matches([' ', '\t']);
    let indent = &line[..line.len() - body.len()];
    // Heading: 1-6 `#` then a space (or nothing after).
    let hashes = body.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) {
        let after = &body[hashes..];
        if after.is_empty() || after.starts_with(' ') {
            return (
                indent,
                Block::Heading(hashes as u8),
                after.trim_start_matches(' '),
            );
        }
    }
    if let Some(rest) = body.strip_prefix("> ").or_else(|| body.strip_prefix('>')) {
        return (indent, Block::Quote, rest);
    }
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = body.strip_prefix(bullet) {
            // A task box stays with the text.
            return (indent, Block::Bulleted, rest);
        }
    }
    let digits = body.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && digits <= 9 {
        let after = &body[digits..];
        if let Some(rest) = after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))
        {
            return (indent, Block::Numbered, rest);
        }
    }
    (indent, Block::Normal, body)
}

/// Insert a link around the selection: `[selection]()` with the caret in the URL, or, if
/// the selection looks like a URL, `[](url)` with the caret in the text.
pub fn insert_link(text: &str, selection: Range<usize>) -> TextEdit {
    let inner = &text[selection.clone()];
    let start = selection.start;
    if inner.contains("://") && !inner.contains(char::is_whitespace) {
        let new = format!("[]({inner})");
        return TextEdit {
            range: selection,
            text: new,
            selection: start + 1..start + 1,
        };
    }
    let new = format!("[{inner}]()");
    let caret = start + inner.len() + 3;
    TextEdit {
        range: selection,
        text: new,
        selection: caret..caret,
    }
}

/// Insert (or replace the selection with) an image link to `target`.
pub fn insert_image(selection: Range<usize>, target: &Path, doc_dir: Option<&Path>) -> TextEdit {
    let text = image_markdown(target, doc_dir);
    let end = selection.start + text.len();
    TextEdit {
        range: selection,
        text,
        selection: end..end,
    }
}

/// Wrap the lines of the selection in a fenced code block (or insert an empty one with
/// the caret inside).
pub fn insert_code_block(text: &str, selection: Range<usize>) -> TextEdit {
    if selection.is_empty() {
        let at_line_start = selection.start == 0 || text[..selection.start].ends_with('\n');
        let lead = if at_line_start { "" } else { "\n" };
        let new = format!("{lead}```\n\n```\n");
        let caret = selection.start + lead.len() + 4;
        return TextEdit {
            range: selection,
            text: new,
            selection: caret..caret,
        };
    }
    let start = text[..selection.start].rfind('\n').map_or(0, |i| i + 1);
    let end_pos = if text[..selection.end].ends_with('\n') {
        selection.end - 1
    } else {
        selection.end
    };
    let end = text[end_pos..]
        .find('\n')
        .map_or(text.len(), |i| end_pos + i);
    let body = &text[start..end];
    let new = format!("```\n{body}\n```");
    TextEdit {
        range: start..end,
        text: new.clone(),
        selection: start..start + new.len(),
    }
}

/// `![name](path)` for an image file, the path relative to the document's folder when
/// possible (absolute otherwise); angle brackets keep spaces and parentheses valid.
pub fn image_markdown(target: &Path, doc_dir: Option<&Path>) -> String {
    let name = target
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let path = doc_dir
        .and_then(|dir| relative_path(target, dir))
        .unwrap_or_else(|| target.to_path_buf());
    let mut link = path.to_string_lossy().replace('\\', "/");
    if link.contains([' ', '(', ')', '<', '>']) {
        link = format!("<{link}>");
    }
    format!("![{name}]({link})")
}

/// `target` relative to the directory `base` (both absolute), with `..` as needed.
pub fn relative_path(target: &Path, base: &Path) -> Option<PathBuf> {
    if !target.is_absolute() || !base.is_absolute() {
        return None;
    }
    let t: Vec<Component> = target.components().collect();
    let b: Vec<Component> = base.components().collect();
    // Different roots (e.g. Windows drives): no relative path.
    if t.first() != b.first() {
        return None;
    }
    let common = t.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in common..b.len() {
        out.push("..");
    }
    for c in &t[common..] {
        out.push(c.as_os_str());
    }
    Some(out)
}

/// File extensions treated as images when dropped or inserted.
pub fn is_image(path: &Path) -> bool {
    const IMAGES: [&str; 8] = ["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "avif"];
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGES.iter().any(|i| i.eq_ignore_ascii_case(e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, e: &TextEdit) -> String {
        format!(
            "{}{}{}",
            &text[..e.range.start],
            e.text,
            &text[e.range.end..]
        )
    }

    #[test]
    fn inline_styles_toggle() {
        let t = "say hello world";
        let e = toggle_inline(t, 4..9, Inline::Bold);
        let t2 = apply(t, &e);
        assert_eq!(t2, "say **hello** world");
        assert_eq!(&t2[e.selection.clone()], "hello");
        // Toggling again (selection still on the word) removes it.
        let e2 = toggle_inline(&t2, e.selection.clone(), Inline::Bold);
        assert_eq!(apply(&t2, &e2), t);
        // Italic inside bold adds a third star, and comes off again.
        let e3 = toggle_inline(&t2, e.selection.clone(), Inline::Italic);
        let t3 = apply(&t2, &e3);
        assert_eq!(t3, "say ***hello*** world");
        let e4 = toggle_inline(&t3, e3.selection.clone(), Inline::Italic);
        assert_eq!(apply(&t3, &e4), "say **hello** world");
        // Italic is not mistaken for bold's markers.
        let e5 = toggle_inline(&t2, e.selection.clone(), Inline::Italic);
        assert_eq!(apply(&t2, &e5), "say ***hello*** world");
    }

    #[test]
    fn inline_on_caret_uses_the_word_or_inserts_a_pair() {
        let t = "say hello world";
        let e = toggle_inline(t, 6..6, Inline::Strikethrough);
        assert_eq!(apply(t, &e), "say ~~hello~~ world");
        let t = "say  world";
        let e = toggle_inline(t, 4..4, Inline::Code);
        assert_eq!(apply(t, &e), "say `` world");
        assert_eq!(e.selection, 5..5);
        let e = toggle_inline("a b", 2..3, Inline::Underline);
        assert_eq!(apply("a b", &e), "a <u>b</u>");
        // Selecting the markers too also removes them.
        let e = toggle_inline("x ==y== z", 2..7, Inline::Highlight);
        assert_eq!(apply("x ==y== z", &e), "x y z");
    }

    #[test]
    fn block_styles() {
        let t = "intro\nfirst\nsecond\n";
        let e = set_block(t, 6..13, Block::Bulleted);
        assert_eq!(apply(t, &e), "intro\n- first\n- second\n");
        let t2 = apply(t, &e);
        // Same style again: back to normal.
        let e = set_block(&t2, e.selection.clone(), Block::Bulleted);
        assert_eq!(apply(&t2, &e), t);
        // Numbered, replacing bullets.
        let e = set_block(&t2, 6..16, Block::Numbered);
        assert_eq!(apply(&t2, &e), "intro\n1. first\n2. second\n");
        // Heading on the caret's line, caret kept on the same letter.
        let t = "## Title here\n";
        let e = set_block(t, 5..5, Block::Heading(1));
        let t3 = apply(t, &e);
        assert_eq!(t3, "# Title here\n");
        assert_eq!(&t3[e.selection.start..e.selection.start + 2], "tl");
        // Quote; then Normal strips it.
        let e = set_block("a\nb", 0..3, Block::Quote);
        assert_eq!(apply("a\nb", &e), "> a\n> b");
        let e = set_block("> a\n> b", 0..7, Block::Normal);
        assert_eq!(apply("> a\n> b", &e), "a\nb");
        // A selection ending at a line start leaves that line alone.
        let e = set_block("a\nb\n", 0..2, Block::Heading(2));
        assert_eq!(apply("a\nb\n", &e), "## a\nb\n");
        // Nested list items keep their indentation.
        let e = set_block("- a\n  - b", 0..9, Block::Numbered);
        assert_eq!(apply("- a\n  - b", &e), "1. a\n  2. b");
    }

    #[test]
    fn links_images_and_code_blocks() {
        let e = insert_link("see docs here", 4..8);
        assert_eq!(apply("see docs here", &e), "see [docs]() here");
        assert_eq!(e.selection, 11..11);
        let e = insert_link("https://x.y/z", 0..13);
        assert_eq!(apply("https://x.y/z", &e), "[](https://x.y/z)");
        assert_eq!(e.selection, 1..1);

        let e = insert_code_block("a\nb\nc", 2..3);
        assert_eq!(apply("a\nb\nc", &e), "a\n```\nb\n```\nc");
        let e = insert_code_block("text", 4..4);
        assert_eq!(apply("text", &e), "text\n```\n\n```\n");
        assert_eq!(e.selection, 9..9);

        // An absolute folder on this platform (`/docs` is relative on Windows: no drive).
        let docs = Path::new(if cfg!(windows) { r"C:\docs" } else { "/docs" });
        assert_eq!(
            image_markdown(&docs.join("img").join("a b.png"), Some(&docs.join("sub"))),
            "![a b](<../img/a b.png>)"
        );
        assert_eq!(
            image_markdown(&docs.join("x.png"), Some(docs)),
            "![x](x.png)"
        );
        assert_eq!(
            image_markdown(Path::new("/i/x.png"), None),
            "![x](/i/x.png)"
        );
        assert!(is_image(Path::new("a.PNG")) && !is_image(Path::new("a.md")));
    }
}

//! Footnotes (`text[^1]` … `[^1]: the note`): finding references and definitions,
//! the next free label, and the edits that insert a new footnote.
//!
//! Plain text scans, independent of the tree-sitter parse: a footnote reference is
//! `[^label]` (label without spaces or `]`), a definition is a line starting with
//! `[^label]:` (up to three spaces of indent), continued by the indented lines after it.

use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

use crate::format::TextEdit;

static REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\^([^\]\s]+)\]").expect("valid regex"));
static DEFINITION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}(\[\^([^\]\s]+)\]):[ \t]?").expect("valid regex"));

/// A footnote marker found in the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub label: String,
    /// The `[^label]` bytes (without the definition's `:`).
    pub range: Range<usize>,
    /// The marker starts a definition (`[^label]: …`) rather than referring to one.
    pub definition: bool,
}

/// A footnote's definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    /// The `[^label]` marker of the definition line.
    pub marker: Range<usize>,
    /// Where the note's text starts (after `[^label]: `).
    pub content_start: usize,
    /// The note's text: the rest of the line plus its indented continuation lines,
    /// joined with spaces.
    pub content: String,
}

/// The footnote marker of `line` (starting at byte `line_start` of the text) that
/// contains byte `off`, if any.
pub fn marker_at(line: &str, line_start: usize, off: usize) -> Option<Marker> {
    let rel = off.checked_sub(line_start)?;
    if let Some(caps) = DEFINITION.captures(line) {
        let m = caps.get(1).expect("group 1");
        if (m.start()..m.end() + 1).contains(&rel) {
            return Some(Marker {
                label: caps[2].to_string(),
                range: line_start + m.start()..line_start + m.end(),
                definition: true,
            });
        }
    }
    REFERENCE.captures_iter(line).find_map(|caps| {
        let m = caps.get(0).expect("group 0");
        (m.start()..m.end()).contains(&rel).then(|| Marker {
            label: caps[1].to_string(),
            range: line_start + m.start()..line_start + m.end(),
            definition: false,
        })
    })
}

/// Every line of `text` with its starting byte.
fn lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut start = 0;
    text.split_inclusive('\n').map(move |l| {
        let s = start;
        start += l.len();
        (s, l.trim_end_matches(['\n', '\r']))
    })
}

/// The definition of footnote `label` in `text`.
pub fn definition(text: &str, label: &str) -> Option<Definition> {
    let mut found: Option<Definition> = None;
    for (start, line) in lines(text) {
        if let Some(def) = found.as_mut() {
            // Continuation: indented lines right after the definition.
            if line.starts_with("    ") || line.starts_with('\t') {
                let more = line.trim();
                if !more.is_empty() {
                    if !def.content.is_empty() {
                        def.content.push(' ');
                    }
                    def.content.push_str(more);
                }
                continue;
            }
            break;
        }
        if let Some(caps) = DEFINITION.captures(line)
            && &caps[2] == label
        {
            let marker = caps.get(1).expect("group 1");
            let whole = caps.get(0).expect("group 0");
            found = Some(Definition {
                marker: start + marker.start()..start + marker.end(),
                content_start: start + whole.end(),
                content: line[whole.end()..].trim().to_string(),
            });
        }
    }
    found
}

/// The first reference to footnote `label` in `text` (definitions excluded).
pub fn first_reference(text: &str, label: &str) -> Option<Range<usize>> {
    for (start, line) in lines(text) {
        let def_end = DEFINITION
            .captures(line)
            .map_or(0, |c| c.get(1).expect("group 1").end());
        for caps in REFERENCE.captures_iter(line) {
            let m = caps.get(0).expect("group 0");
            if m.start() >= def_end && &caps[1] == label {
                return Some(start + m.start()..start + m.end());
            }
        }
    }
    None
}

/// The label for a new footnote: one more than the largest numeric label in `text`.
pub fn next_label(text: &str) -> String {
    let max = REFERENCE
        .captures_iter(text)
        .filter_map(|c| c[1].parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    (max + 1).to_string()
}

/// The edits that insert a new footnote: the reference `[^n]` after the selection, and
/// its definition `[^n]: ` at the end of the document (after a blank line, unless the
/// document already ends with footnote definitions). Returns the edits (in original
/// coordinates, applied as one step) and the caret afterwards: in the new definition.
pub fn insert(text: &str, selection: Range<usize>) -> (Vec<TextEdit>, usize) {
    let label = next_label(text);
    let reference = format!("[^{label}]");
    let body = text.trim_end_matches(['\n', '\r']);
    let last_line = body.rsplit('\n').next().unwrap_or("");
    let ends_with_notes = DEFINITION.is_match(last_line)
        || (last_line.starts_with("    ") && body.lines().rev().any(|l| DEFINITION.is_match(l)));
    let mut tail = String::new();
    if !text.is_empty() && !text.ends_with('\n') {
        tail.push('\n');
    }
    if !body.is_empty() && !ends_with_notes && !text.ends_with("\n\n") {
        tail.push('\n');
    }
    tail.push_str(&format!("{reference}: "));
    let at = selection.end;
    let caret = text.len() + reference.len() + tail.len();
    let edits = vec![
        TextEdit {
            range: at..at,
            text: reference,
            selection: caret..caret,
        },
        TextEdit {
            range: text.len()..text.len(),
            text: tail,
            selection: caret..caret,
        },
    ];
    (edits, caret)
}

/// The superscript form of a numeric label (`12` → `¹²`), or `None` for other labels.
pub fn superscript(label: &str) -> Option<String> {
    const DIGITS: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
    if label.is_empty() || !label.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(label.bytes().map(|b| DIGITS[(b - b'0') as usize]).collect())
}

/// Apply `edits` (non-overlapping, in original coordinates) to `text`: edits at the
/// same offset land in the order given.
pub fn apply(text: &str, edits: &[TextEdit]) -> String {
    let mut order: Vec<usize> = (0..edits.len()).collect();
    order.sort_by_key(|&i| {
        (
            std::cmp::Reverse(edits[i].range.start),
            std::cmp::Reverse(i),
        )
    });
    let mut out = text.to_string();
    for i in order {
        out.replace_range(edits[i].range.clone(), &edits[i].text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "Text[^1] and more[^note].\n\n[^1]: First note.\n[^note]: Second\n    on two lines.\nAfter.\n";

    #[test]
    fn finds_markers_under_the_cursor() {
        let line = "Text[^1] and more[^note].";
        let m = marker_at(line, 0, 5).unwrap();
        assert_eq!(
            (m.label.as_str(), m.range.clone(), m.definition),
            ("1", 4..8, false)
        );
        assert_eq!(marker_at(line, 0, 18).unwrap().label, "note");
        assert_eq!(marker_at(line, 0, 2), None);
        let def = marker_at("[^1]: First note.", 100, 102).unwrap();
        assert!(def.definition);
        assert_eq!(def.range, 100..104);
        // The definition's `:` belongs to its marker too.
        assert!(marker_at("[^1]: First note.", 0, 4).unwrap().definition);
    }

    #[test]
    fn finds_definitions_with_continuations() {
        let d = definition(DOC, "1").unwrap();
        assert_eq!(d.content, "First note.");
        assert_eq!(&DOC[d.marker.clone()], "[^1]");
        assert_eq!(&DOC[d.content_start..d.content_start + 5], "First");
        assert_eq!(
            definition(DOC, "note").unwrap().content,
            "Second on two lines."
        );
        assert_eq!(definition(DOC, "missing"), None);
    }

    #[test]
    fn first_reference_skips_the_definition() {
        assert_eq!(first_reference(DOC, "1"), Some(4..8));
        assert_eq!(first_reference("[^1]: only a definition\n", "1"), None);
    }

    #[test]
    fn next_label_follows_the_numbers() {
        assert_eq!(next_label("no notes"), "1");
        assert_eq!(next_label(DOC), "2");
        assert_eq!(next_label("a[^3] b[^x] c[^10]"), "11");
    }

    #[test]
    fn insert_adds_reference_and_definition() {
        let text = "Some text.\n";
        let (edits, caret) = insert(text, 4..4);
        let out = apply(text, &edits);
        assert_eq!(out, "Some[^1] text.\n\n[^1]: ");
        assert_eq!(caret, out.len());
        // Appended to existing definitions without a blank line between them.
        let text = "A[^1] b.\n\n[^1]: One.\n";
        let (edits, caret) = insert(text, 8..8);
        let out = apply(text, &edits);
        assert_eq!(out, "A[^1] b.[^2]\n\n[^1]: One.\n[^2]: ");
        assert_eq!(caret, out.len());
        // A document without a final newline, and the caret at its very end.
        let text = "End";
        let (edits, caret) = insert(text, 3..3);
        let out = apply(text, &edits);
        assert_eq!(out, "End[^1]\n\n[^1]: ");
        assert_eq!(caret, out.len());
    }

    #[test]
    fn superscript_numbers_only() {
        assert_eq!(superscript("12").as_deref(), Some("¹²"));
        assert_eq!(superscript("note"), None);
    }
}

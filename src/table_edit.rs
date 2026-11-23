//! Structural table edits (the context menu's Table submenu): clear, delete, insert or
//! delete rows and columns, set a column's alignment.
//!
//! Each operation reads the table into a small model (cell texts + per-column alignment),
//! changes the model, and writes the whole table back, padded so its columns line up in
//! the source. The result is one [`TextEdit`] (one undo step) that also places the caret
//! in the cell the operation is about.

use ropey::Rope;
use std::ops::Range;
use unicode_width::UnicodeWidthStr;

use crate::format::TextEdit;
use crate::table::{Align, CellPos, TableInfo};

/// An operation of the context menu's Table submenu, applied at the caret's cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableOp {
    /// Empty every cell, header included.
    Clear,
    /// Remove the whole table.
    Delete,
    InsertRowAbove,
    InsertRowBelow,
    DeleteRow,
    InsertColLeft,
    InsertColRight,
    /// Set the current column's alignment.
    Align(Align),
    DeleteCol,
}

/// The table read out of the source: line prefix (a blockquote's `> `, indentation), rows
/// of cell texts (row 0 is the header) and each column's alignment (`None`: unspecified).
struct Model {
    prefix: String,
    rows: Vec<Vec<String>>,
    aligns: Vec<Option<Align>>,
}

fn slice(rope: &Rope, range: Range<usize>) -> String {
    rope.slice(rope.byte_to_char(range.start)..rope.byte_to_char(range.end))
        .to_string()
}

/// Byte range of line `line`, without its line break.
fn line_range(rope: &Rope, line: usize) -> Range<usize> {
    let start = rope.line_to_byte(line);
    let mut end = if line + 1 < rope.len_lines() {
        rope.line_to_byte(line + 1)
    } else {
        rope.len_bytes()
    };
    while end > start && matches!(rope.get_byte(end - 1), Some(b'\n' | b'\r')) {
        end -= 1;
    }
    start..end
}

/// Split a row's text on its unescaped pipes, dropping the empty border segments.
fn split_row(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut segs = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'|' => {
                segs.push(&text[start..i]);
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    segs.push(&text[start.min(text.len())..]);
    if segs.len() > 1 && segs.first().is_some_and(|s| s.trim().is_empty()) {
        segs.remove(0);
    }
    if segs.len() > 1 && segs.last().is_some_and(|s| s.trim().is_empty()) {
        segs.pop();
    }
    segs
}

/// A delimiter cell's alignment: `:--` left, `--:` right, `:-:` center, `---` unspecified.
fn delimiter_align(cell: &str) -> Option<Align> {
    let c = cell.trim();
    match (c.starts_with(':'), c.ends_with(':') && c.len() > 1) {
        (true, true) => Some(Align::Center),
        (true, false) => Some(Align::Left),
        (false, true) => Some(Align::Right),
        (false, false) => None,
    }
}

fn read_model(rope: &Rope, table: &TableInfo) -> Model {
    let header_line = line_range(rope, rope.byte_to_line(table.header.line.start));
    let prefix: String = slice(rope, header_line)
        .chars()
        .take_while(|c| matches!(c, ' ' | '\t' | '>'))
        .collect();
    let ncols = table.ncols.max(1);
    let rows = (0..table.nrows())
        .filter_map(|r| table.row(r))
        .map(|row| {
            (0..ncols)
                .map(|c| {
                    row.cells
                        .get(c)
                        .map(|cell| slice(rope, cell.content.clone()).trim().to_string())
                        .unwrap_or_default()
                })
                .collect()
        })
        .collect();
    let delim_line = line_range(rope, rope.byte_to_line(table.delimiter_line.start));
    let delim = slice(rope, delim_line);
    let delim = delim.get(prefix.len()..).unwrap_or(&delim);
    let mut aligns: Vec<Option<Align>> =
        split_row(delim).into_iter().map(delimiter_align).collect();
    aligns.resize(ncols, None);
    Model {
        prefix,
        rows,
        aligns,
    }
}

/// Write `model` back as a padded pipe table (no trailing newline). Also returns the byte
/// offset (into the returned text) where each cell's content starts.
fn write_model(model: &Model) -> (String, Vec<Vec<usize>>) {
    let ncols = model.aligns.len();
    let widths: Vec<usize> = (0..ncols)
        .map(|c| {
            model
                .rows
                .iter()
                .map(|r| r.get(c).map_or(0, |t| t.width()))
                .max()
                .unwrap_or(0)
                .max(3)
        })
        .collect();
    let mut out = String::new();
    let mut starts = Vec::with_capacity(model.rows.len());
    let write_row = |out: &mut String, cells: &[String]| -> Vec<usize> {
        out.push_str(&model.prefix);
        out.push('|');
        let mut row_starts = Vec::with_capacity(ncols);
        for (c, &w) in widths.iter().enumerate() {
            let text = cells.get(c).map_or("", String::as_str);
            let pad = w.saturating_sub(text.width());
            let left = match model.aligns[c] {
                Some(Align::Right) => pad,
                Some(Align::Center) => pad / 2,
                _ => 0,
            };
            out.push(' ');
            out.push_str(&" ".repeat(left));
            row_starts.push(out.len());
            out.push_str(text);
            out.push_str(&" ".repeat(pad - left));
            out.push_str(" |");
        }
        row_starts
    };
    for (r, cells) in model.rows.iter().enumerate() {
        if r > 0 {
            out.push('\n');
        }
        starts.push(write_row(&mut out, cells));
        if r == 0 {
            out.push('\n');
            out.push_str(&model.prefix);
            out.push('|');
            for (c, &w) in widths.iter().enumerate() {
                let rule = match model.aligns[c] {
                    None => "-".repeat(w),
                    Some(Align::Left) => format!(":{}", "-".repeat(w - 1)),
                    Some(Align::Right) => format!("{}:", "-".repeat(w - 1)),
                    Some(Align::Center) => format!(":{}:", "-".repeat(w - 2)),
                };
                out.push(' ');
                out.push_str(&rule);
                out.push_str(" |");
            }
        }
    }
    (out, starts)
}

/// The edit removing `table` entirely (its lines and the line break after them).
fn delete_table(rope: &Rope, table: &TableInfo) -> TextEdit {
    let last = table.last_line(|b| rope.byte_to_line(b));
    let end = if last + 1 < rope.len_lines() {
        rope.line_to_byte(last + 1)
    } else {
        rope.len_bytes()
    };
    let start = table.block.start;
    TextEdit {
        range: start..end,
        text: String::new(),
        selection: start..start,
    }
}

/// Apply `op` to `table` with the caret in cell `at`.
pub fn apply(rope: &Rope, table: &TableInfo, at: CellPos, op: TableOp) -> TextEdit {
    let mut m = read_model(rope, table);
    let ncols = m.aligns.len();
    let col = at.col.min(ncols - 1);
    let row = at.row.min(m.rows.len() - 1);
    let empty_row = || vec![String::new(); ncols];
    let target = match op {
        TableOp::Delete => return delete_table(rope, table),
        TableOp::Clear => {
            for cell in m.rows.iter_mut().flatten() {
                cell.clear();
            }
            CellPos { row, col }
        }
        TableOp::InsertRowAbove => {
            // Above the header, the new empty row becomes the header.
            m.rows.insert(row, empty_row());
            CellPos { row, col }
        }
        TableOp::InsertRowBelow => {
            m.rows.insert(row + 1, empty_row());
            CellPos { row: row + 1, col }
        }
        TableOp::DeleteRow => {
            if m.rows.len() == 1 {
                return delete_table(rope, table);
            }
            // Deleting the header promotes the first body row.
            m.rows.remove(row);
            CellPos {
                row: row.min(m.rows.len() - 1),
                col,
            }
        }
        TableOp::InsertColLeft | TableOp::InsertColRight => {
            let at_col = if op == TableOp::InsertColLeft {
                col
            } else {
                col + 1
            };
            for r in &mut m.rows {
                r.insert(at_col, String::new());
            }
            m.aligns.insert(at_col, None);
            CellPos { row, col: at_col }
        }
        TableOp::DeleteCol => {
            if ncols == 1 {
                return delete_table(rope, table);
            }
            for r in &mut m.rows {
                r.remove(col);
            }
            m.aligns.remove(col);
            CellPos {
                row,
                col: col.min(ncols - 2),
            }
        }
        TableOp::Align(a) => {
            m.aligns[col] = Some(a);
            CellPos { row, col }
        }
    };
    let (text, starts) = write_model(&m);
    let last = table.last_line(|b| rope.byte_to_line(b));
    let range = table.block.start..line_range(rope, last).end;
    let caret = range.start + starts[target.row][target.col];
    TextEdit {
        range,
        text,
        selection: caret..caret,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;
    use crate::table::Align;

    /// Apply `op` with the caret in cell `(row, col)` of the only table in `src`; returns
    /// the new document and the caret's offset in it.
    fn run(src: &str, row: usize, col: usize, op: TableOp) -> (String, usize) {
        let buf: Buffer = src.parse().unwrap();
        let table = buf.parsed().tables[0].clone();
        let e = apply(buf.rope(), &table, CellPos { row, col }, op);
        let mut out = src.to_string();
        out.replace_range(e.range.clone(), &e.text);
        (out, e.selection.start)
    }

    const T: &str = "intro\n\n| A | Bb |\n|---|:-:|\n| 1 | 2 |\n\nafter\n";

    #[test]
    fn insert_row_below_pads_columns() {
        let (out, caret) = run(T, 1, 1, TableOp::InsertRowBelow);
        assert_eq!(
            out,
            "intro\n\n| A   | Bb  |\n| --- | :-: |\n| 1   |  2  |\n|     |     |\n\nafter\n"
        );
        // Caret in the new row's second cell (centered: one padding space before it).
        assert!(out[..caret].ends_with("|  2  |\n|     |  "));
    }

    #[test]
    fn insert_row_above_header_becomes_header() {
        let (out, caret) = run(T, 0, 0, TableOp::InsertRowAbove);
        assert_eq!(
            out,
            "intro\n\n|     |     |\n| --- | :-: |\n| A   | Bb  |\n| 1   |  2  |\n\nafter\n"
        );
        assert_eq!(caret, "intro\n\n| ".len());
    }

    #[test]
    fn delete_row_and_header() {
        let (out, _) = run(T, 1, 0, TableOp::DeleteRow);
        assert_eq!(out, "intro\n\n| A   | Bb  |\n| --- | :-: |\n\nafter\n");
        // Deleting the header promotes the first body row.
        let (out, _) = run(T, 0, 0, TableOp::DeleteRow);
        assert_eq!(out, "intro\n\n| 1   |  2  |\n| --- | :-: |\n\nafter\n");
    }

    #[test]
    fn columns_insert_delete_align() {
        let (out, caret) = run(T, 1, 0, TableOp::InsertColRight);
        assert_eq!(
            out,
            "intro\n\n| A   |     | Bb  |\n| --- | --- | :-: |\n| 1   |     |  2  |\n\nafter\n"
        );
        assert!(out[..caret].ends_with("| 1   | "));
        let (out, _) = run(T, 0, 1, TableOp::InsertColLeft);
        assert!(out.contains("| A   |     | Bb  |"));
        let (out, _) = run(T, 0, 0, TableOp::DeleteCol);
        assert_eq!(out, "intro\n\n| Bb  |\n| :-: |\n|  2  |\n\nafter\n");
        let (out, _) = run(T, 1, 0, TableOp::Align(Align::Right));
        assert_eq!(
            out,
            "intro\n\n|   A | Bb  |\n| --: | :-: |\n|   1 |  2  |\n\nafter\n"
        );
    }

    #[test]
    fn clear_and_delete() {
        let (out, _) = run(T, 1, 1, TableOp::Clear);
        assert_eq!(
            out,
            "intro\n\n|     |     |\n| --- | :-: |\n|     |     |\n\nafter\n"
        );
        let (out, caret) = run(T, 1, 1, TableOp::Delete);
        assert_eq!(out, "intro\n\n\nafter\n");
        assert_eq!(caret, "intro\n\n".len());
    }

    #[test]
    fn last_column_or_row_deletes_the_table() {
        let src = "| A |\n|---|\n";
        assert_eq!(run(src, 0, 0, TableOp::DeleteCol).0, "");
        assert_eq!(run(src, 0, 0, TableOp::DeleteRow).0, "");
    }

    #[test]
    fn keeps_blockquote_prefix_and_escaped_pipes() {
        let src = "> | a \\| b | c |\n> |---|---|\n> | 1 | 2 |\n";
        let (out, _) = run(src, 1, 1, TableOp::InsertRowBelow);
        assert_eq!(
            out,
            "> | a \\| b | c   |\n> | ------ | --- |\n> | 1      | 2   |\n> |        |     |\n"
        );
    }

    #[test]
    fn ragged_rows_are_filled() {
        let src = "| A | B |\n|---|---|\n| 1 |\n";
        let (out, _) = run(src, 1, 0, TableOp::Align(Align::Left));
        assert_eq!(out, "| A   | B   |\n| :-- | --- |\n| 1   |     |\n");
    }
}

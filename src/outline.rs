//! Outline helpers for the table of contents (drawn by `ui::toc`): which heading the
//! cursor is in.

use crate::marker::HeadingInfo;

/// Index of the section the cursor is in: the last heading with `line <= cursor_line`.
/// `None` when the cursor sits before the first heading. Assumes `headings` is sorted
/// by `line` ascending (document order), which `collect_node_infos` guarantees.
pub fn current_heading_index(headings: &[HeadingInfo], cursor_line: usize) -> Option<usize> {
    headings
        .partition_point(|h| h.line <= cursor_line)
        .checked_sub(1)
}

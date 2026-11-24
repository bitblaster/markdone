mod action;
mod theme;

pub use action::Direction;
pub use theme::EditorTheme;

use std::ops::Range;

use crate::buffer::Buffer;
use crate::cursor::{Cursor, Selection, grapheme_column, offset_at_column, prev_grapheme_boundary};
use crate::marker::{LineMarkers, MarkerKind, OrderedMarker, UnorderedMarker};
use crate::table::{
    CellPos, RowKind, TableCell, TableInfo, cell_caret_range, cell_pos_at, grid_ok,
    line_break_after, line_break_before, row_kind_at_line,
};

/// Context about the line at the cursor, used by smart editing actions.
struct LineContext {
    /// The current line's markers.
    line: LineMarkers,
}

/// Cached tab cycle states for a specific line.
#[derive(Clone, Default)]
struct TabCycleCache {
    /// The line index this cache is for.
    line_idx: usize,
    /// The cached cycle states.
    states: Vec<String>,
}

/// Ascend from `node` (inclusive) through its parents, returning the first node
/// whose kind is `kind`.
fn ancestor_of_kind<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == kind {
            return Some(n);
        }
        current = n.parent();
    }
    None
}

/// Byte indices of unescaped `|` characters in `s` (a `|` is escaped when preceded
/// by an odd number of backslashes).
fn unescaped_pipe_indices(s: &str) -> Vec<usize> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    for i in 0..bytes.len() {
        if bytes[i] == b'|' {
            let mut backslashes = 0;
            let mut j = i;
            while j > 0 && bytes[j - 1] == b'\\' {
                backslashes += 1;
                j -= 1;
            }
            if backslashes % 2 == 0 {
                out.push(i);
            }
        }
    }
    out
}

/// Column count of a candidate pipe-header row: split the trimmed line on its
/// unescaped pipes and drop the empty leading/trailing segments the surrounding
/// pipes produce. `pipes` are byte indices into `trimmed`.
/// Cell contents of a pipe row (segments between pipes), dropping the empty leading/
/// trailing segments produced by the outer `|` bars. Index 0 is the cell after the
/// leading pipe (i.e. between `pipes[0]` and `pipes[1]` for a bounded row).
fn pipe_row_cells<'a>(trimmed: &'a str, pipes: &[usize]) -> Vec<&'a str> {
    let mut segs: Vec<&str> = Vec::with_capacity(pipes.len() + 1);
    let mut start = 0;
    for &p in pipes {
        segs.push(&trimmed[start..p]);
        start = p + 1;
    }
    segs.push(&trimmed[start..]);
    if segs.first().is_some_and(|s| s.is_empty()) {
        segs.remove(0);
    }
    if segs.last().is_some_and(|s| s.is_empty()) {
        segs.pop();
    }
    segs
}

fn pipe_row_ncols(trimmed: &str, pipes: &[usize]) -> usize {
    pipe_row_cells(trimmed, pipes).len()
}

/// Whether a trimmed line is a GFM table delimiter row (only `| - : ` chars, with
/// at least one `-`).
fn looks_like_delimiter_row(trimmed: &str) -> bool {
    !trimmed.is_empty()
        && trimmed.contains('-')
        && trimmed
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

/// Core editing state that can be used without GPUI context.
/// This contains the buffer and selection, and all editing logic.
pub struct EditorState {
    pub buffer: Buffer,
    pub selection: Selection,
    /// Cached tab cycle states to avoid recalculating mid-cycle.
    tab_cycle_cache: Option<TabCycleCache>,
    /// Sticky vertical-movement goal: `(grapheme_column, offset_it_landed_at)`. Reused by
    /// the next Up/Down only if the cursor is still at that offset, so passing through a
    /// short line doesn't lose the original column; any other move invalidates it for free.
    goal_column: Option<(usize, usize)>,
}

impl EditorState {
    pub fn new(content: &str) -> Self {
        let buffer: Buffer = content.parse().unwrap_or_default();
        Self {
            buffer,
            selection: Selection::new(0, 0),
            tab_cycle_cache: None,
            goal_column: None,
        }
    }

    pub fn cursor(&self) -> Cursor {
        self.selection.cursor()
    }

    pub fn text(&self) -> String {
        self.buffer.text()
    }

    /// Set cursor position by byte offset.
    pub fn set_cursor(&mut self, offset: usize) {
        let offset = offset.min(self.buffer.len_bytes());
        self.selection = Selection::new(offset, offset);
    }

    /// Collapse the selection onto a single cursor.
    fn set_cursor_to(&mut self, c: Cursor) {
        self.selection = Selection::new(c.offset, c.offset);
    }

    /// Compute the cursor after moving one step in `direction`. Vertical moves keep a
    /// sticky goal column (`&mut self` to read/update it); other moves leave it stale,
    /// and it self-invalidates because the cursor no longer sits at its landing offset.
    pub fn cursor_in_direction(&mut self, direction: Direction) -> Cursor {
        if let Some(offset) = self.grid_move(direction) {
            self.goal_column = None;
            return Cursor {
                offset: self.snap_to_grid_cell(offset),
            };
        }
        let c = self.cursor();
        if matches!(direction, Direction::Up | Direction::Down) {
            let line = self.buffer.byte_to_line(c.offset);
            let column = match self.goal_column {
                Some((col, at)) if at == c.offset => col,
                _ => grapheme_column(&self.buffer, c.offset),
            };
            let offset = if direction == Direction::Up {
                if line == 0 {
                    Cursor::start().offset
                } else {
                    offset_at_column(&self.buffer, line - 1, column)
                }
            } else if line >= self.buffer.line_count().saturating_sub(1) {
                Cursor::end(&self.buffer).offset
            } else {
                offset_at_column(&self.buffer, line + 1, column)
            };
            // Entering a grid table lands in a cell, not on its pipes.
            let offset = self.snap_to_grid_cell(offset);
            self.goal_column = Some((column, offset));
            return Cursor { offset };
        }
        let c = match direction {
            Direction::Left => c.move_left(&self.buffer),
            Direction::Right => c.move_right(&self.buffer),
            Direction::LineStart => c.move_to_line_start(&self.buffer),
            Direction::LineEnd => c.move_to_line_end(&self.buffer),
            Direction::DocStart => Cursor::start(),
            Direction::DocEnd => Cursor::end(&self.buffer),
            Direction::Up | Direction::Down => unreachable!("handled above"),
        };
        Cursor {
            offset: self.snap_to_grid_cell(c.offset),
        }
    }

    /// Move cursor left by one character.
    pub fn move_left(&mut self) {
        let c = self.cursor_in_direction(Direction::Left);
        self.set_cursor_to(c);
    }

    /// Move cursor right by one character.
    pub fn move_right(&mut self) {
        let c = self.cursor_in_direction(Direction::Right);
        self.set_cursor_to(c);
    }

    /// Move cursor up by one line.
    pub fn move_up(&mut self) {
        let c = self.cursor_in_direction(Direction::Up);
        self.set_cursor_to(c);
    }

    /// Move cursor down by one line.
    pub fn move_down(&mut self) {
        let c = self.cursor_in_direction(Direction::Down);
        self.set_cursor_to(c);
    }

    /// Move cursor to start of current line.
    pub fn move_to_line_start(&mut self) {
        self.set_cursor_to(self.cursor().move_to_line_start(&self.buffer));
    }

    /// Move cursor to end of current line.
    pub fn move_to_line_end(&mut self) {
        self.set_cursor_to(self.cursor().move_to_line_end(&self.buffer));
    }

    /// Insert text at the current cursor position.
    pub fn insert_text(&mut self, text: &str) {
        // Clear tab cycle cache since content is changing
        self.tab_cycle_cache = None;
        let cursor_before = self.cursor().offset;
        let insert_pos = if !self.selection.is_collapsed() {
            let range = self.selection.range();
            self.buffer.delete(range.clone(), cursor_before);
            range.start
        } else {
            cursor_before
        };
        self.buffer.insert(insert_pos, text, insert_pos);
        let new_pos = insert_pos + text.len();
        self.selection = Selection::new(new_pos, new_pos);

        // After inserting, propagate checkbox state if this line has a checkbox.
        // This handles the case where tab cycling created an incomplete checkbox line
        // (e.g., "- [ ] ") and typing content makes it parseable by tree-sitter.
        self.propagate_checkbox_after_edit();
        // Growing a table header's column count grows the rest of the table to match.
        // Only for single-line inserts (typing) — a multi-line paste isn't a column edit.
        if !text.contains('\n') {
            self.maybe_sync_table_columns(insert_pos);
        }
    }

    fn find_line_at(&self, byte_pos: usize) -> Option<(usize, LineMarkers)> {
        let idx = self.buffer.byte_to_line(byte_pos);
        if idx < self.buffer.line_count() {
            Some((idx, self.buffer.line_markers(idx)))
        } else {
            None
        }
    }

    /// Check if the cursor is inside a code block (between opening and closing fences,
    /// or after an opening fence with no closing fence yet).
    pub fn cursor_in_code_block(&self) -> bool {
        let Some(tree) = self.buffer.tree() else {
            return false;
        };

        let cursor_offset = self.cursor().offset;
        let root = tree.block_tree().root_node();

        // Find the deepest node at the cursor position and walk up looking for fenced_code_block
        let Some(node) = root.descendant_for_byte_range(cursor_offset, cursor_offset) else {
            return false;
        };

        ancestor_of_kind(node, "fenced_code_block").is_some()
    }

    /// Get context about the line at the cursor.
    /// Returns None if the cursor is not on a valid line.
    fn line_context(&self) -> Option<LineContext> {
        let cursor_offset = self.cursor().offset;
        let line_idx = self.buffer.byte_to_line(cursor_offset);
        if line_idx >= self.buffer.line_count() {
            return None;
        }
        let line = self.buffer.line_markers(line_idx);

        Some(LineContext { line })
    }

    /// Auto-insert space after `>` if it just became a blockquote marker.
    /// Returns true if a space was inserted.
    pub fn maybe_complete_blockquote_marker(&mut self) -> bool {
        let cursor_pos = self.cursor().offset;
        if cursor_pos == 0 {
            return false;
        }

        if self.buffer.byte_at(cursor_pos - 1) != Some(b'>') {
            return false;
        }

        if self.buffer.byte_at(cursor_pos) == Some(b' ') {
            return false;
        }

        let line_idx = self.buffer.byte_to_line(cursor_pos);
        if line_idx >= self.buffer.line_count() {
            return false;
        }
        let line = self.buffer.line_markers(line_idx);

        let has_blockquote = line
            .markers
            .iter()
            .any(|m| matches!(m.kind, MarkerKind::BlockQuote));

        if !has_blockquote {
            return false;
        }

        self.insert_text(" ");
        true
    }

    /// After typing ` or ~, check if we just completed "```" or "~~~" at line start
    /// and auto-insert the closing fence.
    pub fn maybe_complete_code_fence(&mut self) {
        let cursor_pos = self.cursor().offset;
        if cursor_pos < 3 {
            return;
        }

        // Check we just typed 3 of the same fence character
        let fence_char = self.buffer.byte_at(cursor_pos - 1);
        if fence_char != Some(b'`') && fence_char != Some(b'~') {
            return;
        }
        if self.buffer.byte_at(cursor_pos - 2) != fence_char
            || self.buffer.byte_at(cursor_pos - 3) != fence_char
        {
            return;
        }

        // Check this is at the start of a line (possibly after blockquote markers)
        let line_idx = self.buffer.byte_to_line(cursor_pos);
        let line_start = self.buffer.line_to_byte(line_idx);
        let before_fence = self.buffer.slice_cow(line_start..(cursor_pos - 3));
        let trimmed = before_fence.trim();

        // Allow only whitespace or blockquote markers before the fence
        if !trimmed.is_empty() && !trimmed.chars().all(|c| c == '>') {
            return;
        }

        // Insert newline + closing fence, cursor stays after opening fence
        let closing = if fence_char == Some(b'`') {
            "\n```"
        } else {
            "\n~~~"
        };
        self.buffer.insert(cursor_pos, closing, cursor_pos);
    }

    /// Try to insert a space. Returns false if space should be ignored
    /// (at line start, or at blockquote content start outside code blocks).
    pub fn try_insert_space(&mut self) -> bool {
        if self.cursor_in_code_block() {
            self.insert_text(" ");
            return true;
        }

        let cursor = self.cursor();
        let line_start = cursor.move_to_line_start(&self.buffer).offset;

        if cursor.offset == line_start || self.cursor_at_blockquote_content_start() {
            return false;
        }

        // `insert_typed`: a selection across a grid table's cells is emptied cell by cell.
        self.insert_typed(" ");
        true
    }

    /// Check if cursor is at the content start of a blockquote-only line.
    /// Used to prevent inserting spaces/tabs at the "beginning" of blockquote content.
    fn cursor_at_blockquote_content_start(&self) -> bool {
        let cursor_pos = self.cursor().offset;
        let line_idx = self.buffer.byte_to_line(cursor_pos);
        if line_idx >= self.buffer.line_count() {
            return false;
        }
        let line = self.buffer.line_markers(line_idx);

        if !line.is_blockquote_only() {
            return false;
        }

        if let Some(marker_range) = line.marker_range() {
            cursor_pos == marker_range.end
        } else {
            false
        }
    }

    /// Tab: cycle forward through nesting states based on tree-sitter context.
    pub fn tab(&mut self) {
        if self.table_cell_nav(true) {
            return;
        }
        let Some((states, current_idx, prefix_end)) = self.get_tab_cycle_state() else {
            return;
        };

        if states.len() <= 1 {
            return;
        }

        let next_idx = (current_idx + 1) % states.len();
        self.set_line_prefix(&states[next_idx], prefix_end);

        // After changing structure, propagate checkbox state if this line has a checkbox
        self.propagate_checkbox_after_edit();
    }

    /// Shift+Tab: cycle backward through nesting states.
    fn shift_tab_cycle(&mut self) {
        let Some((states, current_idx, prefix_end)) = self.get_tab_cycle_state() else {
            return;
        };

        if states.len() <= 1 {
            return;
        }

        let prev_idx = if current_idx == 0 {
            states.len() - 1
        } else {
            current_idx - 1
        };
        self.set_line_prefix(&states[prev_idx], prefix_end);

        // After changing structure, propagate checkbox state if this line has a checkbox
        self.propagate_checkbox_after_edit();
    }

    /// Get tab cycle states, using cache if available for current line.
    /// Returns (states, current_idx, prefix_end) where prefix_end is where the prefix ends.
    fn get_tab_cycle_state(&mut self) -> Option<(Vec<String>, usize, usize)> {
        let cursor_offset = self.cursor().offset;
        let line_idx = self.buffer.byte_to_line(cursor_offset);
        let line_start = self.buffer.line_to_byte(line_idx);

        // Get current line's checkbox state to pass to state builder
        let current_checkbox = self.buffer.line_markers(line_idx).checkbox();

        // Reuse the cache only when it's for this line; otherwise (wrong line or no
        // cache) rebuild and store.
        let states = if self
            .tab_cycle_cache
            .as_ref()
            .is_some_and(|c| c.line_idx == line_idx)
        {
            self.tab_cycle_cache.as_ref().unwrap().states.clone()
        } else {
            let states = self.build_cycle_states_from_tree(cursor_offset, current_checkbox);
            self.tab_cycle_cache = Some(TabCycleCache {
                line_idx,
                states: states.clone(),
            });
            states
        };

        if states.len() <= 1 {
            return None;
        }

        // Find which state matches the current line's prefix
        // We check if the line starts with each state (longest match wins)
        let line_end = self
            .buffer
            .line_to_byte(line_idx + 1)
            .min(self.buffer.len_bytes());
        let line_text = self.buffer.slice_cow(line_start..line_end);

        let mut best_match: Option<(usize, &str)> = None;
        for (idx, state) in states.iter().enumerate() {
            if line_text.starts_with(state)
                && (best_match.is_none() || state.len() > best_match.unwrap().1.len())
            {
                best_match = Some((idx, state));
            }
        }

        let (current_idx, prefix_end) = match best_match {
            Some((idx, state)) => (idx, line_start + state.len()),
            None => (0, line_start), // Default to empty prefix at index 0
        };

        Some((states, current_idx, prefix_end))
    }

    /// Build tab cycle states by walking up the tree-sitter parse tree.
    /// The cycle is determined by context ABOVE the current line, not by current line content.
    /// If `checkbox_state` is Some, task list markers will use that state instead of the parent's.
    pub fn build_cycle_states_from_tree(
        &self,
        cursor_offset: usize,
        checkbox_state: Option<bool>,
    ) -> Vec<String> {
        let Some(tree) = self.buffer.tree() else {
            return vec![String::new()];
        };

        let root = tree.block_tree().root_node();
        let cursor_line_idx = self.buffer.byte_to_line(cursor_offset);

        let line_start = self.buffer.line_to_byte(cursor_line_idx);
        let lookup_offset = if line_start > 0 { line_start - 1 } else { 0 };
        let node = root.descendant_for_byte_range(lookup_offset, lookup_offset);

        let Some(node) = node else {
            return vec![String::new()];
        };

        let context_node = if self.is_in_error_node(node) {
            self.find_context_from_error(node).unwrap_or(node)
        } else {
            node
        };

        let mut nodes_to_process: Vec<tree_sitter::Node> = Vec::new();
        let mut blockquote_prefix = String::new();
        let mut current = Some(context_node);

        while let Some(n) = current {
            if n.kind() == "block_quote" {
                if let Some(marker_node) = n
                    .children(&mut n.walk())
                    .find(|c| c.kind() == "block_quote_marker")
                {
                    let marker_text = self
                        .buffer
                        .slice_cow(marker_node.start_byte()..marker_node.end_byte());
                    blockquote_prefix = format!("{}{}", marker_text, blockquote_prefix);
                }
            } else if n.kind() == "list_item" {
                nodes_to_process.push(n);
            }
            current = n.parent();
        }

        let mut list_levels: Vec<(usize, String, usize, bool)> = Vec::new();

        for n in nodes_to_process {
            let mut marker_text = String::new();
            let mut list_marker_len = 0;
            let mut marker_start = 0;
            let mut is_ordered = false;

            for child in n.children(&mut n.walk()) {
                match child.kind() {
                    "list_marker_minus" | "list_marker_plus" | "list_marker_star" => {
                        marker_start = child.start_byte();
                        let text = self.buffer.slice_cow(child.start_byte()..child.end_byte());
                        list_marker_len = text.len();
                        marker_text.push_str(&text);
                    }
                    "list_marker_dot" | "list_marker_parenthesis" => {
                        marker_start = child.start_byte();
                        let text = self.buffer.slice_cow(child.start_byte()..child.end_byte());
                        list_marker_len = text.len();
                        marker_text.push_str(&text);
                        is_ordered = true;
                    }
                    "task_list_marker_checked" | "task_list_marker_unchecked" => {
                        // Use the current line's checkbox state if provided.
                        // If None (line has no checkbox yet), default to unchecked.
                        let checkbox_text = match checkbox_state {
                            Some(true) => "[x]",
                            Some(false) | None => "[ ]",
                        };
                        marker_text.push_str(checkbox_text);
                        marker_text.push(' ');
                    }
                    _ => {}
                }
            }

            if !marker_text.is_empty() {
                let line_idx = self.buffer.byte_to_line(marker_start);
                let line_start = self.buffer.line_to_byte(line_idx);
                let absolute_indent = marker_start - line_start;
                let indent = absolute_indent.saturating_sub(blockquote_prefix.len());
                list_levels.push((indent, marker_text, list_marker_len, is_ordered));
            }
        }

        if list_levels.is_empty() && blockquote_prefix.is_empty() {
            return vec![String::new()];
        }

        list_levels.reverse();

        let mut states = Vec::new();

        if !blockquote_prefix.is_empty() {
            states.push(blockquote_prefix.clone());
        }

        for (indent, marker, list_marker_len, is_ordered) in &list_levels {
            let sibling_marker = if *is_ordered {
                Self::increment_ordered_marker(marker)
            } else {
                marker.clone()
            };
            states.push(format!(
                "{}{}{}",
                blockquote_prefix,
                " ".repeat(*indent),
                sibling_marker
            ));

            states.push(format!(
                "{}{}",
                blockquote_prefix,
                " ".repeat(indent + list_marker_len)
            ));
        }

        if let Some((deepest_indent, deepest_marker, list_marker_len, is_ordered)) =
            list_levels.last()
        {
            let deeper_indent = deepest_indent + list_marker_len;
            let nested_marker = if *is_ordered {
                Self::reset_ordered_marker(deepest_marker)
            } else {
                deepest_marker.clone()
            };
            states.push(format!(
                "{}{}{}",
                blockquote_prefix,
                " ".repeat(deeper_indent),
                nested_marker
            ));
        }

        states.push(String::new());
        states
    }

    /// Split an ordered marker (`1.`, `10)`) into its number and the suffix after the digits
    /// (`.`/`)`). `None` when there's no leading digit run.
    fn split_ordered_marker(marker: &str) -> Option<(usize, &str)> {
        let num_end = marker
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(marker.len());
        if num_end == 0 {
            return None;
        }
        let num = marker[..num_end].parse().unwrap_or(1);
        Some((num, &marker[num_end..]))
    }

    fn increment_ordered_marker(marker: &str) -> String {
        match Self::split_ordered_marker(marker) {
            Some((num, suffix)) => format!("{}{suffix}", num + 1),
            None => marker.to_string(),
        }
    }

    fn reset_ordered_marker(marker: &str) -> String {
        match Self::split_ordered_marker(marker) {
            Some((_, suffix)) => format!("1{suffix}"),
            None => marker.to_string(),
        }
    }

    fn is_in_error_node(&self, node: tree_sitter::Node) -> bool {
        ancestor_of_kind(node, "ERROR").is_some()
    }

    fn find_context_from_error<'a>(
        &self,
        node: tree_sitter::Node<'a>,
    ) -> Option<tree_sitter::Node<'a>> {
        let mut current = Some(node);
        while let Some(n) = current {
            if n.kind() == "ERROR" {
                if let Some(prev) = n.prev_sibling() {
                    return self.find_last_list_item(prev);
                }
                return None;
            }
            current = n.parent();
        }
        None
    }

    fn find_last_list_item<'a>(
        &self,
        node: tree_sitter::Node<'a>,
    ) -> Option<tree_sitter::Node<'a>> {
        let mut result: Option<tree_sitter::Node<'a>> = None;
        if node.kind() == "list_item" {
            result = Some(node);
        }
        let child_count = node.child_count();
        for i in (0..child_count).rev() {
            if let Some(child) = node.child(i as u32)
                && let Some(found) = self.find_last_list_item(child)
            {
                return Some(found);
            }
        }
        result
    }

    /// Find the list_item node containing the given byte offset.
    fn list_item_at(&self, byte_offset: usize) -> Option<tree_sitter::Node<'_>> {
        let tree = self.buffer.tree()?;
        let root = tree.block_tree().root_node();
        let node = root.descendant_for_byte_range(byte_offset, byte_offset)?;

        ancestor_of_kind(node, "list_item")
    }

    /// Find the checkbox marker among a list_item's direct children, if any.
    /// Returns (checkbox_byte_offset, is_checked).
    fn direct_checkbox(&self, list_item: tree_sitter::Node) -> Option<(usize, bool)> {
        let mut cursor = list_item.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                match child.kind() {
                    "task_list_marker_checked" => return Some((child.start_byte(), true)),
                    "task_list_marker_unchecked" => return Some((child.start_byte(), false)),
                    _ => {}
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        None
    }

    /// Find all checkboxes nested within a list_item node.
    /// Returns Vec of (checkbox_byte_offset, is_checked).
    fn find_nested_checkboxes(&self, list_item_node: tree_sitter::Node) -> Vec<(usize, bool)> {
        let mut checkboxes = Vec::new();
        let mut cursor = list_item_node.walk();

        loop {
            let node = cursor.node();
            match node.kind() {
                "task_list_marker_checked" => {
                    checkboxes.push((node.start_byte(), true));
                }
                "task_list_marker_unchecked" => {
                    checkboxes.push((node.start_byte(), false));
                }
                _ => {}
            }

            if cursor.goto_first_child() {
                continue;
            }
            if cursor.goto_next_sibling() {
                continue;
            }
            loop {
                if !cursor.goto_parent() {
                    return checkboxes;
                }
                if cursor.node().id() == list_item_node.id() {
                    return checkboxes;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    /// Build full nested context markers by walking up the tree-sitter tree.
    /// Returns markers from outermost to innermost (e.g., `> - [x] - [ ]`).
    /// The `(list_marker, checkbox)` markers of a `list_item` node (each `None` if
    /// absent), from its direct children. Ordered/unordered marker style and the ordinal
    /// number are read off the marker token.
    fn list_item_markers(
        &self,
        item: tree_sitter::Node,
    ) -> (Option<MarkerKind>, Option<MarkerKind>) {
        let mut list_marker: Option<MarkerKind> = None;
        let mut checkbox: Option<MarkerKind> = None;
        let mut cursor = item.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                match child.kind() {
                    "task_list_marker_checked" => {
                        checkbox = Some(MarkerKind::Checkbox { checked: true });
                    }
                    "task_list_marker_unchecked" => {
                        checkbox = Some(MarkerKind::Checkbox { checked: false });
                    }
                    "list_marker_minus" => {
                        list_marker = Some(MarkerKind::ListItem {
                            ordered: false,
                            unordered_marker: Some(UnorderedMarker::Minus),
                            ordered_marker: None,
                            number: None,
                        });
                    }
                    "list_marker_star" => {
                        list_marker = Some(MarkerKind::ListItem {
                            ordered: false,
                            unordered_marker: Some(UnorderedMarker::Star),
                            ordered_marker: None,
                            number: None,
                        });
                    }
                    "list_marker_plus" => {
                        list_marker = Some(MarkerKind::ListItem {
                            ordered: false,
                            unordered_marker: Some(UnorderedMarker::Plus),
                            ordered_marker: None,
                            number: None,
                        });
                    }
                    "list_marker_dot" | "list_marker_parenthesis" => {
                        let marker_text =
                            self.buffer.slice_cow(child.start_byte()..child.end_byte());
                        let number = marker_text
                            .trim()
                            .chars()
                            .take_while(|c| c.is_ascii_digit())
                            .collect::<String>()
                            .parse::<u32>()
                            .ok();
                        let ordered_marker = Some(if child.kind() == "list_marker_dot" {
                            OrderedMarker::Dot
                        } else {
                            OrderedMarker::Parenthesis
                        });
                        list_marker = Some(MarkerKind::ListItem {
                            ordered: true,
                            unordered_marker: None,
                            ordered_marker,
                            number,
                        });
                    }
                    _ => {}
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        (list_marker, checkbox)
    }

    pub fn build_nested_context(&self, cursor_offset: usize) -> Vec<MarkerKind> {
        let Some(tree) = self.buffer.tree() else {
            return Vec::new();
        };

        let root = tree.block_tree().root_node();

        // Handle edge case: cursor at end of file
        let lookup_offset = if cursor_offset > 0
            && root
                .descendant_for_byte_range(cursor_offset, cursor_offset)
                .map(|n| n.kind() == "document")
                .unwrap_or(true)
        {
            cursor_offset - 1
        } else {
            cursor_offset
        };

        let Some(node) = root.descendant_for_byte_range(lookup_offset, lookup_offset) else {
            return Vec::new();
        };

        // Walk up from current node, collecting context from each relevant ancestor
        let mut markers_reversed = Vec::new();
        let mut current = Some(node);

        while let Some(n) = current {
            match n.kind() {
                "block_quote" => {
                    markers_reversed.push(MarkerKind::BlockQuote);
                }
                "list_item" => {
                    // Reversed order (checkbox then list_marker); the whole vec is
                    // reversed at the end, yielding list_marker then checkbox (i.e. "- [x]").
                    let (list_marker, checkbox) = self.list_item_markers(n);
                    if let Some(cb) = checkbox {
                        markers_reversed.push(cb);
                    }
                    if let Some(lm) = list_marker {
                        markers_reversed.push(lm);
                    }
                }
                "fenced_code_block" => {
                    // Find info_string for language
                    let mut cursor = n.walk();
                    let mut language = None;
                    if cursor.goto_first_child() {
                        loop {
                            let child = cursor.node();
                            if child.kind() == "info_string" {
                                language = Some(
                                    self.buffer
                                        .slice_cow(child.start_byte()..child.end_byte())
                                        .to_string(),
                                );
                                break;
                            }
                            if !cursor.goto_next_sibling() {
                                break;
                            }
                        }
                    }
                    markers_reversed.push(MarkerKind::CodeBlockFence {
                        language,
                        is_opening: true,
                    });
                }
                _ => {}
            }
            current = n.parent();
        }

        // Reverse to get outermost-to-innermost order
        markers_reversed.reverse();
        markers_reversed
    }

    /// Find the parent list_item's checkbox, if any.
    /// Returns (checkbox_byte_offset, is_checked).
    fn find_parent_checkbox(&self, list_item_start: usize) -> Option<(usize, bool)> {
        let our_list_item = self.list_item_at(list_item_start)?;

        // Walk up to find parent list_item, then read its direct checkbox
        let parent = ancestor_of_kind(our_list_item.parent()?, "list_item")?;
        self.direct_checkbox(parent)
    }

    /// Find all sibling checkboxes (same nesting level).
    /// Returns Vec of (checkbox_byte_offset, is_checked).
    fn find_sibling_checkboxes(&self, list_item_start: usize) -> Vec<(usize, bool)> {
        let our_list_item = match self.list_item_at(list_item_start) {
            Some(n) => n,
            None => return Vec::new(),
        };

        // Get parent list node
        let parent_list = match our_list_item.parent() {
            Some(p) if p.kind() == "list" => p,
            _ => return Vec::new(),
        };

        // Iterate all list_item children and collect their (direct) checkboxes
        let mut siblings = Vec::new();
        let mut cursor = parent_list.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "list_item"
                    && let Some(cb) = self.direct_checkbox(child)
                {
                    siblings.push(cb);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        siblings
    }

    /// Set the line prefix, replacing current markers up to prefix_end.
    /// Preserves any content after prefix_end and adjusts cursor position.
    fn set_line_prefix(&mut self, new_prefix: &str, prefix_end: usize) {
        let cursor_offset = self.cursor().offset;
        let line_idx = self.buffer.byte_to_line(cursor_offset);
        let line_start = self.buffer.line_to_byte(line_idx);

        let old_prefix_len = prefix_end - line_start;
        let new_prefix_len = new_prefix.len();
        let len_diff = new_prefix_len as isize - old_prefix_len as isize;

        // Delete old prefix
        if prefix_end > line_start {
            self.buffer.delete(line_start..prefix_end, cursor_offset);
        }

        // Insert new prefix
        if !new_prefix.is_empty() {
            self.buffer.insert(line_start, new_prefix, line_start);
        }

        // Adjust cursor: if cursor was after prefix, shift by the length difference
        // If cursor was in the prefix area, move to end of new prefix
        let new_cursor = if cursor_offset >= prefix_end {
            (cursor_offset as isize + len_diff) as usize
        } else {
            line_start + new_prefix_len
        };
        self.selection = Selection::new(new_cursor, new_cursor);
    }

    /// Resolve the table (if any) whose row occupies `line_idx`, plus that line's
    /// role in it. Mirrors `RenderSnapshot::table_row_at_line` but works off the
    /// buffer directly so editor input can query it.
    fn table_context_at_line(&self, line_idx: usize) -> Option<(&TableInfo, RowKind)> {
        if line_idx >= self.buffer.line_count() {
            return None;
        }
        let line_start = self.buffer.line_to_byte(line_idx);
        let table = self
            .buffer
            .parsed()
            .tables
            .iter()
            .find(|t| line_start >= t.block.start && line_start < t.block.end)?;
        // Line-number match (not byte containment) — the twin of `table_row_at_line`, so a
        // tree-sitter error-recovery row starting mid-line still resolves (else Tab cell-nav
        // and row insertion silently no-op while the grid still renders).
        let kind = row_kind_at_line(table, line_idx, |b| self.buffer.byte_to_line(b))?;
        Some((table, kind))
    }

    /// The table context at the cursor's line, cloned so callers can mutate the
    /// buffer afterward without holding a borrow.
    fn table_context_at_cursor(&self) -> Option<(TableInfo, RowKind)> {
        let line_idx = self.buffer.byte_to_line(self.cursor().offset);
        self.table_context_at_line(line_idx)
            .map(|(t, k)| (t.clone(), k))
    }

    /// The table holding `offset`'s line when it is edited as a grid of cells (small
    /// enough to grid-render, see [`grid_ok`]), plus that line's role in it.
    fn grid_at(&self, offset: usize) -> Option<(&TableInfo, RowKind)> {
        let line = self
            .buffer
            .byte_to_line(offset.min(self.buffer.len_bytes()));
        self.table_context_at_line(line).filter(|(t, _)| grid_ok(t))
    }

    /// The cell holding `offset` in a grid-edited table (`None` on the delimiter row).
    fn grid_cell_at(&self, offset: usize) -> Option<(&TableInfo, CellPos)> {
        let (t, _) = self.grid_at(offset)?;
        Some((t, cell_pos_at(t, self.buffer.rope(), offset)?))
    }

    /// The table and cell holding `offset`, in any table (grid-edited or not); on the
    /// delimiter row, the header's first cell. For the context menu's Table commands.
    pub fn table_cell_at(&self, offset: usize) -> Option<(TableInfo, CellPos)> {
        let line = self
            .buffer
            .byte_to_line(offset.min(self.buffer.len_bytes()));
        let (t, kind) = self.table_context_at_line(line)?;
        let pos = match kind {
            RowKind::Delimiter => CellPos { row: 0, col: 0 },
            _ => cell_pos_at(t, self.buffer.rope(), offset)?,
        };
        Some((t.clone(), pos))
    }

    /// The caret's extent in cell `pos` of `t` (see [`cell_caret_range`]).
    fn grid_caret_range(&self, t: &TableInfo, pos: CellPos) -> Option<Range<usize>> {
        Some(cell_caret_range(self.buffer.rope(), t.cell(pos)?))
    }

    /// `offset` moved into the nearest cell's text when it sits on a grid table's
    /// structure (pipes, padding, the hidden delimiter row); unchanged anywhere else.
    pub fn snap_to_grid_cell(&self, offset: usize) -> usize {
        let Some((t, kind)) = self.grid_at(offset) else {
            return offset;
        };
        let pos = match kind {
            RowKind::Delimiter => CellPos {
                row: usize::from(!t.body.is_empty()),
                col: 0,
            },
            _ => match cell_pos_at(t, self.buffer.rope(), offset) {
                Some(p) => p,
                None => return offset,
            },
        };
        match self.grid_caret_range(t, pos) {
            Some(r) => offset.clamp(r.start, r.end),
            None => offset,
        }
    }

    /// A caret move inside a grid table, cell by cell: Left/Right walk the cell's text and
    /// step into the neighbouring cell at its edges (out of the table past the first or
    /// last cell), Home/End go to the cell's start/end, Up/Down to the same column of the
    /// row above/below (out of the table past the first or last row). `None` outside grid
    /// tables.
    fn grid_move(&self, direction: Direction) -> Option<usize> {
        let off = self.cursor().offset;
        let (t, pos) = self.grid_cell_at(off)?;
        let here = self.grid_caret_range(t, pos)?;
        let ncells = |row: usize| t.row(row).map_or(0, |r| r.cells.len());
        let range_of = |row: usize, col: usize| self.grid_caret_range(t, CellPos { row, col });
        let header_line = self.buffer.byte_to_line(t.header.line.start);
        let last_line = t.last_line(|b| self.buffer.byte_to_line(b));
        // Just outside the table: the end of the line above, the start of the line below.
        let above = || {
            let start = self.buffer.line_to_byte(header_line);
            if start > 0 { start - 1 } else { off }
        };
        let below = || {
            if last_line + 1 < self.buffer.line_count() {
                self.buffer.line_to_byte(last_line + 1)
            } else {
                Cursor::end(&self.buffer).offset
            }
        };
        match direction {
            Direction::Left => {
                if off > here.start {
                    // A `<br>` shows as one line break: step over it whole.
                    if let Some(br) = line_break_before(self.buffer.rope(), off)
                        && br >= here.start
                    {
                        return Some(br);
                    }
                    let left = self.cursor().move_left(&self.buffer).offset;
                    return Some(left.max(here.start));
                }
                if pos.col > 0 {
                    return range_of(pos.row, pos.col - 1).map(|r| r.end);
                }
                if pos.row > 0 && ncells(pos.row - 1) > 0 {
                    return range_of(pos.row - 1, ncells(pos.row - 1) - 1).map(|r| r.end);
                }
                Some(above())
            }
            Direction::Right => {
                if off < here.end {
                    if let Some(br) = line_break_after(self.buffer.rope(), off)
                        && br <= here.end
                    {
                        return Some(br);
                    }
                    let right = self.cursor().move_right(&self.buffer).offset;
                    return Some(right.min(here.end));
                }
                if pos.col + 1 < ncells(pos.row) {
                    return range_of(pos.row, pos.col + 1).map(|r| r.start);
                }
                if pos.row + 1 < t.nrows() && ncells(pos.row + 1) > 0 {
                    return range_of(pos.row + 1, 0).map(|r| r.start);
                }
                Some(below())
            }
            Direction::LineStart => Some(here.start),
            Direction::LineEnd => Some(here.end),
            Direction::Up | Direction::Down => {
                let row = if direction == Direction::Up {
                    match pos.row.checked_sub(1) {
                        Some(r) => r,
                        None if header_line > 0 => {
                            return Some(self.buffer.line_to_byte(header_line - 1));
                        }
                        None => return Some(off),
                    }
                } else {
                    pos.row + 1
                };
                if row >= t.nrows() {
                    return Some(below());
                }
                match ncells(row) {
                    0 => None,
                    n => range_of(row, pos.col.min(n - 1)).map(|r| r.start),
                }
            }
            Direction::DocStart | Direction::DocEnd => None,
        }
    }

    /// Delete a selection spanning several cells of one grid table by emptying the
    /// selected part of each cell, so the table keeps its shape. False (nothing done) when
    /// the selection is inside one cell or reaches outside the table: the ordinary delete
    /// handles those.
    fn grid_clear_selection(&mut self) -> bool {
        let sel = self.selection.range();
        let (cuts, landing) = {
            let (Some((t, a)), Some((t2, b))) =
                (self.grid_cell_at(sel.start), self.grid_cell_at(sel.end))
            else {
                return false;
            };
            if t.block.start != t2.block.start || a == b {
                return false;
            }
            let cuts: Vec<Range<usize>> = (0..t.nrows())
                .filter_map(|r| t.row(r))
                .flat_map(|row| row.cells.iter())
                .filter_map(|cell| {
                    let s = sel.start.max(cell.content.start);
                    let e = sel.end.min(cell.content.end);
                    (s < e).then_some(s..e)
                })
                .collect();
            // The cuts all start at or after this spot, so it stays put.
            (cuts, self.snap_to_grid_cell(sel.start))
        };
        if let (Some(first), Some(last)) = (cuts.first(), cuts.last()) {
            let (lo, hi) = (first.start, last.end);
            let mut kept = String::new();
            let mut at = lo;
            for c in &cuts {
                kept.push_str(&self.buffer.slice_cow(at..c.start));
                at = c.end;
            }
            let cursor = self.cursor().offset;
            self.buffer.replace(lo..hi, &kept, cursor);
        }
        self.selection = Selection::new(landing, landing);
        true
    }

    /// Insert typed or pasted text. Inside a grid table's cell the text is kept on one
    /// line and its `|` are escaped, so it can't break the table apart; a selection across
    /// cells is emptied cell by cell first.
    pub fn insert_typed(&mut self, text: &str) {
        if self.grid_cell_at(self.selection.head).is_none() {
            self.insert_text(text);
            return;
        }
        if !self.selection.is_collapsed() {
            self.grid_clear_selection();
        }
        let at = self.selection.range().start;
        let mut escaped = at > 0 && self.buffer.byte_at(at - 1) == Some(b'\\');
        let mut out = String::with_capacity(text.len() + 1);
        for ch in text.chars() {
            match ch {
                '\r' => continue,
                // A table row is one line: a line break in a cell is `<br>`.
                '\n' => out.push_str("<br>"),
                '\t' => out.push(' '),
                '|' if !escaped => out.push_str("\\|"),
                c => out.push(c),
            }
            escaped = ch == '\\' && !escaped;
        }
        self.insert_text(&out);
    }

    /// Enter in a grid table's cell: go to the same column of the next row, adding a row
    /// after the last one. Returns true if it fired.
    fn grid_enter(&mut self) -> bool {
        let Some((t, pos)) = self.grid_cell_at(self.selection.head) else {
            return false;
        };
        if pos.row + 1 < t.nrows() {
            let n = t.row(pos.row + 1).map_or(0, |r| r.cells.len());
            let target = (n > 0)
                .then(|| {
                    self.grid_caret_range(
                        t,
                        CellPos {
                            row: pos.row + 1,
                            col: pos.col.min(n - 1),
                        },
                    )
                })
                .flatten();
            if let Some(r) = target {
                self.set_cursor(r.start);
            }
            return true;
        }
        let ncols = t.ncols.max(1);
        let last = t.last_line(|b| self.buffer.byte_to_line(b));
        let first = self.insert_table_row_after_line(last, ncols);
        // The new row is `|  |  |…`: cell `c` is typed into 3 bytes after cell `c - 1`.
        self.set_cursor(first + 3 * pos.col.min(ncols - 1));
        true
    }

    /// Insert a new empty body row (`|  |  |…`, `ncols` cells) after `line_idx`,
    /// keeping any trailing newline. Returns the offset of the new row's first
    /// cell's typing position (just after its `| `).
    fn insert_table_row_after_line(&mut self, line_idx: usize, ncols: usize) -> usize {
        let content_end = self.line_content_end(line_idx);
        let row = format!("|{}", "  |".repeat(ncols));
        self.set_cursor(content_end);
        self.insert_text(&format!("\n{row}"));

        // First cell typing position = new_row_start (content_end + 1) + "| " (2).
        // Computed directly, not from a reparse: an all-empty trailing row + newline
        // momentarily parses with an ERROR node, so the model may not see it yet.
        content_end + 3
    }

    /// Smart-Enter table creation: on a "lone pipe-header row" (a bounded pipe row
    /// not yet part of a table), complete it into a real GFM table by inserting a
    /// delimiter row and one empty body row, landing the cursor in the first body
    /// cell. Returns true if it fired.
    pub fn maybe_create_table(&mut self) -> bool {
        if self.cursor_in_code_block() {
            return false;
        }
        let cursor = self.cursor().offset;
        let line_idx = self.buffer.byte_to_line(cursor);
        if line_idx >= self.buffer.line_count() {
            return false;
        }

        // Only when the cursor sits at the end of the line.
        if cursor != self.cursor().move_to_line_end(&self.buffer).offset {
            return false;
        }
        // Not already inside a table.
        if self.table_context_at_cursor().is_some() {
            return false;
        }

        let line_start = self.buffer.line_to_byte(line_idx);
        let ncols = {
            let line_text = self.buffer.slice_cow(line_start..cursor);
            let trimmed = line_text.trim();
            let pipes = unescaped_pipe_indices(trimmed);
            if pipes.is_empty() {
                return false;
            }
            let ncols = pipe_row_ncols(trimmed, &pipes);
            // Only a properly-bounded header row `| … |` triggers creation, so prose or
            // list lines that merely contain a pipe never become tables.
            let bounded = trimmed.starts_with('|') && trimmed.ends_with('|');
            if ncols == 0 || !bounded {
                return false;
            }
            ncols
        };

        // Don't fire if the next line is already a delimiter row.
        if line_idx + 1 < self.buffer.line_count() {
            let next_start = self.buffer.line_to_byte(line_idx + 1);
            let next_end = if line_idx + 2 < self.buffer.line_count() {
                self.buffer.line_to_byte(line_idx + 2).saturating_sub(1)
            } else {
                self.buffer.len_bytes()
            };
            let next_text = self.buffer.slice_cow(next_start..next_end);
            if looks_like_delimiter_row(next_text.trim()) {
                return false;
            }
        }

        let delim = format!("|{}", " --- |".repeat(ncols));
        let body = format!("|{}", "  |".repeat(ncols));
        self.insert_text(&format!("\n{delim}\n{body}"));

        // Land in the first body cell (just after its "| "). Computed directly from
        // the inserted layout: header end + "\n" + delimiter + "\n" + "| ".
        let body_start = cursor + 1 + delim.len() + 1;
        self.set_cursor(body_start + 2);
        true
    }

    /// Caret landing offset for entering `cell` via Tab/Shift+Tab: the first content
    /// char for a non-empty cell, or a spot one space in past the opening `|` for an
    /// empty cell. An empty cell's trimmed content range collapses onto the closing-pipe
    /// boundary, so landing at `content.start` would sit the caret on that pipe; instead
    /// walk back to the opening `|` and land strictly between the two pipes.
    fn table_cell_landing(&self, cell: &TableCell) -> usize {
        crate::table::cell_landing(self.buffer.rope(), cell)
    }

    /// Cell navigation for Tab/Shift+Tab when the cursor is inside a table.
    /// Returns false (letting the caller fall back to list cycling) when not in a
    /// table. Forward: next cell → next row's first cell → append a new row at the
    /// last cell of the last row. Backward: previous cell → previous row's last
    /// cell; the header's first cell is a no-op.
    fn table_cell_nav(&mut self, forward: bool) -> bool {
        let cursor = self.cursor().offset;
        let Some((table, kind)) = self.table_context_at_cursor() else {
            return false;
        };

        let nrows = 1 + table.body.len();
        let cells_of = |row_pos: usize| -> &Vec<TableCell> {
            if row_pos == 0 {
                &table.header.cells
            } else {
                &table.body[row_pos - 1].cells
            }
        };

        let row_pos = match kind {
            RowKind::Header => Some(0),
            RowKind::Body(i) => Some(i + 1),
            RowKind::Delimiter => None,
        };

        // Some(offset) → move there; None → append a new row.
        let target: Option<usize> = match row_pos {
            None => {
                if forward {
                    // Delimiter row: forward goes to the first body cell (None → append).
                    table
                        .body
                        .first()
                        .and_then(|r| r.cells.first())
                        .map(|c| self.table_cell_landing(c))
                } else {
                    Some(
                        table
                            .header
                            .cells
                            .last()
                            .map_or(cursor, |c| self.table_cell_landing(c)),
                    )
                }
            }
            Some(row_pos) => {
                let cells = cells_of(row_pos);
                // The cell whose span (content + padding) holds the caret, so a caret in
                // the padding after the text still counts as in its cell.
                let cell_idx = table
                    .row(row_pos)
                    .and_then(|r| crate::table::col_at(self.buffer.rope(), r, cursor))
                    .unwrap_or(0);
                if forward {
                    if cell_idx + 1 < cells.len() {
                        Some(self.table_cell_landing(&cells[cell_idx + 1]))
                    } else if row_pos + 1 < nrows {
                        cells_of(row_pos + 1)
                            .first()
                            .map(|c| self.table_cell_landing(c))
                    } else {
                        None
                    }
                } else if cell_idx > 0 {
                    Some(self.table_cell_landing(&cells[cell_idx - 1]))
                } else if row_pos > 0 {
                    cells_of(row_pos - 1)
                        .last()
                        .map(|c| self.table_cell_landing(c))
                } else {
                    Some(cursor)
                }
            }
        };

        match target {
            Some(off) => self.set_cursor(off),
            None => {
                let anchor = table
                    .body
                    .last()
                    .map(|r| r.line.start)
                    .unwrap_or(table.delimiter_line.start);
                let anchor_line = self.buffer.byte_to_line(anchor);
                let off = self.insert_table_row_after_line(anchor_line, table.ncols);
                self.set_cursor(off);
            }
        }
        true
    }

    /// Shift+Enter inside a table: add an empty body row below the current one
    /// (header/delimiter rows insert as the first body row) and land in its first
    /// cell. Returns true if it fired.
    fn maybe_add_table_row(&mut self) -> bool {
        let line_idx = self.buffer.byte_to_line(self.cursor().offset);
        let Some((table, kind)) = self.table_context_at_cursor() else {
            return false;
        };
        let anchor_line = match kind {
            RowKind::Body(_) => line_idx,
            RowKind::Header | RowKind::Delimiter => {
                self.buffer.byte_to_line(table.delimiter_line.start)
            }
        };
        let off = self.insert_table_row_after_line(anchor_line, table.ncols);
        self.set_cursor(off);
        true
    }

    /// Backspace convenience: if the caret sits in an all-empty table body row (e.g. one
    /// just added with Shift+Enter/Tab that you changed your mind about), remove the whole
    /// row in one press and land at the end of the previous row. Returns true if it fired.
    fn maybe_remove_empty_table_row(&mut self) -> bool {
        let Some((table, RowKind::Body(i))) = self.table_context_at_cursor() else {
            return false;
        };
        // Only when every cell is empty — otherwise backspace deletes a character normally.
        if !table.body[i]
            .cells
            .iter()
            .all(|c| c.content.start >= c.content.end)
        {
            return false;
        }
        let line_idx = self.buffer.byte_to_line(self.cursor().offset);
        let line_start = self.buffer.line_to_byte(line_idx);
        // First body row must still have a real row above it (header/delimiter), so
        // `line_start` is never 0 — the caret lands at the previous line's content end.
        let line_end = if line_idx + 1 < self.buffer.line_count() {
            self.buffer.line_to_byte(line_idx + 1)
        } else {
            self.buffer.len_bytes()
        };
        // Delete the row's line plus its own trailing newline, leaving the previous row's
        // newline intact so it joins cleanly with whatever followed the table.
        let cursor = self.cursor().offset;
        self.buffer.delete(line_start..line_end, cursor);
        let landing = self.snap_to_grid_cell(line_start.saturating_sub(1));
        self.selection = Selection::new(landing, landing);
        true
    }

    /// When the caret's line is a table header that has gained a column (more cells than
    /// its delimiter row on the next line), append matching empty cells to the delimiter
    /// and every following body row so the whole table grows the column. Additive only —
    /// deleting a header column never removes body data.
    ///
    /// Detected on the RAW lines, not the parsed table: a header wider than its delimiter
    /// isn't a valid GFM table, so tree-sitter stops recognizing it the instant you type
    /// the new column — we have to spot the mismatch ourselves and heal it back to a table.
    /// Called after `insert_text`; a no-op away from that exact situation.
    /// Byte offset of the end of line `l`'s content (before its trailing newline), or
    /// `len_bytes` for the last line. Insertions land here to append to the line.
    fn line_content_end(&self, l: usize) -> usize {
        if l + 1 < self.buffer.line_count() {
            self.buffer.line_to_byte(l + 1).saturating_sub(1)
        } else {
            self.buffer.len_bytes()
        }
    }

    /// A line's text (including its trailing newline, if any).
    fn line_str(&self, l: usize) -> String {
        let ls = self.buffer.line_to_byte(l);
        let le = if l + 1 < self.buffer.line_count() {
            self.buffer.line_to_byte(l + 1)
        } else {
            self.buffer.len_bytes()
        };
        self.buffer.slice_cow(ls..le).into_owned()
    }

    fn maybe_sync_table_columns(&mut self, insert_pos: usize) {
        let hl = self.buffer.byte_to_line(self.cursor().offset);
        let dl = hl + 1;
        if dl >= self.buffer.line_count() {
            return;
        }
        let hls = self.buffer.line_to_byte(hl);
        let header_line = self.line_str(hl);
        let header = header_line.trim();
        let hpipes = unescaped_pipe_indices(header);
        // A bounded pipe row followed by a delimiter row is the header-of-a-table signal.
        if hpipes.is_empty() || !header.starts_with('|') {
            return;
        }
        let delim = self.line_str(dl);
        if !looks_like_delimiter_row(delim.trim()) {
            return;
        }
        let target = pipe_row_ncols(header, &hpipes);
        let delim_trim = delim.trim();
        if target <= pipe_row_ncols(delim_trim, &unescaped_pipe_indices(delim_trim)) {
            return; // header no wider than the delimiter → nothing to grow
        }

        // Which column the user added. Base guess: the cell just left of the edit position
        // (`hp` pipes precede it), which is right when the name was typed before the pipe
        // (`X |`). Inserting a bare `|` instead leaves an empty cell on the *other* side, so
        // prefer a lone empty header cell — but ONLY when it's adjacent to the edit (at the
        // base column or one past it). Requiring adjacency stops a *pre-existing* empty cell
        // (e.g. `| A |  | C |` then appending `D`) from stealing the insert position.
        let hp = unescaped_pipe_indices(&header_line)
            .iter()
            .filter(|&&p| hls + p < insert_pos)
            .count();
        let positional = hp.saturating_sub(1);
        let hcells = pipe_row_cells(header, &hpipes);
        let empties: Vec<usize> = hcells
            .iter()
            .enumerate()
            .filter(|(_, c)| c.trim().is_empty())
            .map(|(i, _)| i)
            .collect();
        let col = match empties.as_slice() {
            [e] if *e == positional || *e == positional + 1 => *e,
            _ => positional,
        };

        // The delimiter, then every following body row (until a blank / pipe-less line).
        let mut rows: Vec<(usize, bool)> = vec![(dl, true)];
        let mut l = dl + 1;
        while l < self.buffer.line_count() {
            let t = self.line_str(l);
            if t.trim().is_empty() || !t.contains('|') {
                break;
            }
            rows.push((l, false));
            l += 1;
        }

        // For each short row, insert the missing cell(s) at column `col`: right after that
        // row's `col`-th pipe (0-indexed), so the new cell lands at the same position as
        // the header's. If the row has no pipe there (shorter/unbounded), append at the end.
        let mut edits: Vec<(usize, String)> = Vec::new();
        for (line, is_delim) in rows {
            let text = self.line_str(line);
            let trimmed = text.trim();
            let cells = pipe_row_ncols(trimmed, &unescaped_pipe_indices(trimmed));
            if cells >= target {
                continue;
            }
            let ls = self.buffer.line_to_byte(line);
            let pipes: Vec<usize> = unescaped_pipe_indices(&text)
                .iter()
                .map(|p| ls + p)
                .collect();
            let cell = if is_delim { " --- |" } else { "   |" };
            let body = cell.repeat(target - cells);
            let (off, ins) = if col < pipes.len() {
                (pipes[col] + 1, body) // right after the col-th pipe
            } else {
                // Append at content end, opening a pipe first if the row lacks a trailing one.
                let content_end = self.line_content_end(line);
                let ins = if trimmed.ends_with('|') {
                    body
                } else {
                    format!("|{body}")
                };
                (content_end, ins)
            };
            edits.push((off, ins));
        }

        // Apply bottom-up so earlier offsets stay valid. Every edit is below the caret (in
        // the header), so the caret's byte position is unchanged.
        edits.sort_by_key(|(off, _)| std::cmp::Reverse(*off));
        let cursor = self.cursor().offset;
        for (off, ins) in edits {
            self.buffer.insert(off, &ins, cursor);
        }
    }

    /// Enter, as word processors and Markdown editors (Zettlr) do it: a new item in a
    /// list or quote (the next number, an unchecked box), and an empty item or quote line
    /// ends its list or quote instead; in a code block, the next line keeps the indent; in
    /// a table cell, a line break inside the cell (`<br>`: a table row is one line). A
    /// lone pipe-header row becomes a table.
    pub fn enter(&mut self) {
        if self.grid_cell_at(self.selection.head).is_some() {
            self.insert_typed("<br>");
            return;
        }
        if self.maybe_create_table() {
            return;
        }
        if self.cursor_in_code_block() {
            let indent = self.current_line_leading_whitespace();
            self.insert_text("\n");
            if !indent.is_empty() {
                self.insert_text(&indent);
            }
            return;
        }
        if self.end_empty_container() {
            return;
        }
        let Some(ctx) = self.line_context() else {
            self.insert_text("\n");
            return;
        };
        // Before the text (in or ahead of the markers): just move the line down.
        if self.cursor().offset < ctx.line.content_start() {
            self.insert_text("\n");
            return;
        }
        let continuation = ctx.line.continuation_rope(self.buffer.rope());
        self.insert_text("\n");
        if !continuation.is_empty() {
            self.insert_text(&continuation);
        }
    }

    /// Enter on a list item or quote line with no text: end it, like a word processor.
    /// A list item loses its marker (and checkbox), keeping the quotes around it; a bare
    /// quote line loses its innermost `>`. Returns true if it fired.
    fn end_empty_container(&mut self) -> bool {
        let Some(ctx) = self.line_context() else {
            return false;
        };
        let line = &ctx.line;
        let has_list = line
            .markers
            .iter()
            .any(|m| matches!(m.kind, MarkerKind::ListItem { .. }));
        // Markers are innermost first: reversed, the quotes in document order.
        let quotes: Vec<Range<usize>> = line
            .markers
            .iter()
            .rev()
            .filter(|m| matches!(m.kind, MarkerKind::BlockQuote))
            .map(|m| m.range.clone())
            .collect();
        if !has_list && quotes.is_empty() {
            return false;
        }
        let content_start = line.content_start().min(line.range.end);
        if !self
            .buffer
            .slice_cow(content_start..line.range.end)
            .trim()
            .is_empty()
        {
            return false;
        }
        let keep = if has_list {
            quotes.len()
        } else {
            quotes.len() - 1
        };
        let prefix: String = quotes[..keep]
            .iter()
            .map(|r| self.buffer.slice_cow(r.clone()).into_owned())
            .collect();
        let mut end = line.range.end;
        if self.buffer.byte_at(end.saturating_sub(1)) == Some(b'\n') {
            end -= 1;
        }
        // A line right after an item continues its paragraph (a "lazy" line): a blank
        // line before this one makes whatever is typed on it a paragraph of its own. (A
        // numbered list right under that paragraph then starts over from 1: see
        // `Buffer::normalize_ordered_lists`.)
        let idx = self.buffer.byte_to_line(line.range.start);
        let before = if has_list && idx > 0 && !self.buffer.is_line_empty(idx - 1) {
            format!("{}\n", prefix.trim_end())
        } else {
            String::new()
        };
        self.selection = Selection::new(line.range.start, end);
        self.insert_text(&format!("{before}{prefix}"));
        let caret = line.range.start + before.len() + prefix.len();
        self.selection = Selection::new(caret, caret);
        true
    }

    /// Shift+Enter: a new line in the same item, without a new marker: under a list
    /// item's text (indented to it), inside the quote, or a line break in a table cell.
    pub fn shift_enter(&mut self) {
        if self.grid_cell_at(self.selection.head).is_some() {
            self.insert_typed("<br>");
            return;
        }
        let indent = match self.line_context() {
            Some(ctx) if !self.cursor_in_code_block() => {
                let has_list = ctx
                    .line
                    .markers
                    .iter()
                    .any(|m| matches!(m.kind, MarkerKind::ListItem { .. }));
                if has_list {
                    ctx.line.nested_paragraph_indent(self.buffer.rope())
                } else {
                    ctx.line.continuation_rope(self.buffer.rope())
                }
            }
            _ => String::new(),
        };
        self.insert_text("\n");
        if !indent.is_empty() {
            self.insert_text(&indent);
        }
    }

    /// Ctrl+Enter: in a table, the next row (the same column; a new row after the last
    /// one); elsewhere, Enter.
    pub fn ctrl_enter(&mut self) {
        if self.grid_enter() || self.maybe_add_table_row() {
            return;
        }
        self.enter();
    }

    /// Get leading whitespace (spaces/tabs) from the current line.
    fn current_line_leading_whitespace(&self) -> String {
        let cursor = self.cursor();
        let line_start = cursor.move_to_line_start(&self.buffer).offset;
        let line_end = cursor.move_to_line_end(&self.buffer).offset;
        let line_text = self.buffer.slice_cow(line_start..line_end);

        line_text
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect()
    }

    /// Shift+Tab: cycle backward through nesting states.
    pub fn shift_tab(&mut self) {
        if self.table_cell_nav(false) {
            return;
        }
        self.shift_tab_cycle();
    }

    fn backspace_range_with_type(
        &self,
        cursor_pos: usize,
    ) -> Option<(std::ops::Range<usize>, bool)> {
        let (_, line) = self.find_line_at(cursor_pos)?;

        for marker in &line.markers {
            if cursor_pos == marker.range.end {
                let is_indent = matches!(marker.kind, MarkerKind::Indent);
                return Some((marker.range.clone(), is_indent));
            }
        }

        None
    }

    /// If cursor is at end of an opening code fence and the code block contains
    /// only whitespace, return the full block range to delete.
    fn find_empty_code_block_range(&self, cursor_pos: usize) -> Option<std::ops::Range<usize>> {
        let tree = self.buffer.tree()?;
        let root = tree.block_tree().root_node();

        // Find the node at cursor position (look slightly before since cursor is at end of fence)
        let node = root.descendant_for_byte_range(cursor_pos.saturating_sub(1), cursor_pos)?;

        // Walk up to find fenced_code_block
        let code_block = ancestor_of_kind(node, "fenced_code_block")?;

        let block_start = code_block.start_byte();
        let block_end = code_block.end_byte();

        // Find where content starts (after first line / opening fence)
        let block_text = self.buffer.slice_cow(block_start..block_end);
        let first_newline = block_text.find('\n')?;
        let content_start = block_start + first_newline + 1;

        // Check if content (between opening fence and end) is only whitespace + closing fence
        let content = self.buffer.slice_cow(content_start..block_end);
        let trimmed = content.trim();

        if trimmed == "```" || trimmed == "~~~" {
            // Don't include trailing newline after closing fence
            let mut end = block_end;
            if self.buffer.byte_at(end.saturating_sub(1)) == Some(b'\n') {
                end -= 1;
            }
            Some(block_start..end)
        } else {
            None
        }
    }

    /// Delete backward (backspace). Simple: delete one unit.
    /// Markers and indents are atomic - deleted as a whole.
    pub fn delete_backward(&mut self) {
        // Clear tab cycle cache since content is changing
        self.tab_cycle_cache = None;
        if !self.selection.is_collapsed() {
            if !self.grid_clear_selection() {
                self.delete_selection();
            }
            self.propagate_checkbox_after_edit();
            return;
        }

        if self.cursor().offset == 0 {
            return;
        }

        // One backspace removes a just-added (still-empty) table row wholesale.
        if self.maybe_remove_empty_table_row() {
            self.propagate_checkbox_after_edit();
            return;
        }

        let cursor_pos = self.cursor().offset;

        // In a grid table's cell, Backspace stops at the cell's start: it never eats a pipe.
        // A line break (`<br>`) goes whole.
        if let Some((t, pos)) = self.grid_cell_at(cursor_pos)
            && let Some(cell) = self.grid_caret_range(t, pos)
        {
            if cursor_pos <= cell.start {
                return;
            }
            if let Some(br) = line_break_before(self.buffer.rope(), cursor_pos)
                && br >= cell.start
            {
                self.buffer.delete(br..cursor_pos, cursor_pos);
                self.selection = Selection::new(br, br);
                return;
            }
        }

        if let Some((marker_range, _is_indent)) = self.backspace_range_with_type(cursor_pos) {
            // Check if we're deleting an opening code fence of an empty code block
            if let Some(block_range) = self.find_empty_code_block_range(cursor_pos) {
                // Delete the entire empty code block
                self.buffer.delete(block_range.clone(), cursor_pos);
                self.selection = Selection::new(block_range.start, block_range.start);
                self.propagate_checkbox_after_edit();
                return;
            }

            // Otherwise just delete the marker
            self.buffer.delete(marker_range.clone(), cursor_pos);
            self.selection = Selection::new(marker_range.start, marker_range.start);
            self.propagate_checkbox_after_edit();
            return;
        }

        // One grapheme cluster back, not one byte — so a codepoint or an emoji/combining
        // cluster is deleted whole rather than split.
        let new_pos = prev_grapheme_boundary(&self.buffer, cursor_pos);
        self.buffer.delete(new_pos..cursor_pos, cursor_pos);
        self.selection = Selection::new(new_pos, new_pos);
        self.propagate_checkbox_after_edit();
    }

    fn delete_selection(&mut self) {
        let range = self.selection.range();
        let cursor_before = self.cursor().offset;
        self.buffer.delete(range.clone(), cursor_before);
        self.selection = Selection::new(range.start, range.start);
    }

    /// Delete the character after the cursor, or the selection if active.
    pub fn delete_forward(&mut self) {
        // Clear tab cycle cache since content is changing
        self.tab_cycle_cache = None;
        let cursor = self.cursor().offset;
        // In a grid table's cell, Delete stops at the cell's end: it never eats a pipe.
        let at_cell_end = self.grid_cell_at(cursor).is_some_and(|(t, pos)| {
            self.grid_caret_range(t, pos)
                .is_some_and(|r| cursor >= r.end)
        });
        if !self.selection.is_collapsed() {
            if !self.grid_clear_selection() {
                self.delete_selection();
            }
        } else if at_cell_end {
            // Nothing to delete within the cell.
        } else if let Some(br) = line_break_after(self.buffer.rope(), cursor)
            .filter(|_| self.grid_cell_at(cursor).is_some())
        {
            // A line break (`<br>`) in a cell goes whole.
            self.buffer.delete(cursor..br, cursor);
        } else if self.cursor().offset < self.buffer.len_bytes() {
            let cursor_before = self.cursor().offset;
            let next = self.cursor().move_right(&self.buffer);
            self.buffer
                .delete(cursor_before..next.offset, cursor_before);
        }
        self.propagate_checkbox_after_edit();
    }

    pub fn handle_click(&mut self, buffer_offset: usize, shift_held: bool, click_count: usize) {
        if shift_held {
            self.selection = self.selection.extend_to(buffer_offset);
        } else {
            match click_count {
                2 => {
                    self.selection = Selection::select_word_at(buffer_offset, &self.buffer);
                }
                3 => {
                    // In a grid table, the line is a row: select the clicked cell instead.
                    self.selection = match self.grid_cell_at(buffer_offset) {
                        Some((t, pos)) => {
                            let r = self
                                .grid_caret_range(t, pos)
                                .unwrap_or(buffer_offset..buffer_offset);
                            Selection::new(r.start, r.end)
                        }
                        None => Selection::select_line_at(buffer_offset, &self.buffer),
                    };
                }
                _ => {
                    let off = self.snap_out_of_list_prefix(buffer_offset);
                    self.selection = Selection::new(off, off);
                }
            }
        }
    }

    /// A list item's leading marker whitespace (indent + bullet, before the checkbox or
    /// content) is structural, not text: a click there snaps to the nearer of the line
    /// start or the content boundary, so the caret can't land mid-prefix in a space.
    fn snap_out_of_list_prefix(&self, offset: usize) -> usize {
        let line = self.buffer.byte_to_line(offset);
        let markers = self.buffer.line_markers(line);
        if !markers
            .markers
            .iter()
            .any(|m| matches!(m.kind, MarkerKind::ListItem { .. }))
        {
            return offset;
        }
        let Some(prefix) = markers.marker_range() else {
            return offset;
        };
        if offset > prefix.start && offset < prefix.end {
            if offset - prefix.start <= prefix.end - offset {
                prefix.start
            } else {
                prefix.end
            }
        } else {
            offset
        }
    }

    pub fn handle_drag(&mut self, buffer_offset: usize) {
        self.selection = self.selection.extend_to(buffer_offset);
    }

    /// Toggle a checkbox on the given line, propagating to children and parents.
    /// Byte offset of the checkbox marker (`[ ]` / `[x]` / `[X]`) within `line`,
    /// given its checked state. `None` if the pattern isn't present in the text.
    fn checkbox_byte_offset(&self, line: &LineMarkers, is_checked: bool) -> Option<usize> {
        let line_text = self.buffer.slice_cow(line.range.clone());
        let pattern = if is_checked { "[x]" } else { "[ ]" };
        // Checked boxes may use an uppercase X.
        let relative = line_text
            .find(pattern)
            .or_else(|| is_checked.then(|| line_text.find("[X]")).flatten())?;
        Some(line.range.start + relative)
    }

    pub fn toggle_checkbox(&mut self, line_number: usize) {
        // Capture pre-toggle state so the whole cascade (the box, its children and the
        // parents it checks or unchecks) collapses to a single undo entry at the end.
        let head_before = self.buffer.undo_head();
        let cursor_before = self.cursor().offset;
        let text_before = self.buffer.text();

        let (is_checked, checkbox_byte_start) = {
            if line_number >= self.buffer.line_count() {
                return;
            }
            let line = self.buffer.line_markers(line_number);

            let Some(is_checked) = line.checkbox() else {
                return;
            };

            let Some(checkbox_byte_start) = self.checkbox_byte_offset(&line, is_checked) else {
                return;
            };
            (is_checked, checkbox_byte_start)
        };

        let new_checked = !is_checked;
        let mut cursor_pos = self.cursor().offset;

        // Find the list_item node for this checkbox - use checkbox_byte_start for accurate node finding
        let list_item_node = self.list_item_at(checkbox_byte_start);

        // Collect all checkboxes to toggle (clicked + nested children)
        let mut checkboxes_to_toggle: Vec<(usize, bool)> = Vec::new();

        if let Some(node) = list_item_node {
            // Get all nested checkboxes within this list_item
            let nested = self.find_nested_checkboxes(node);
            for (offset, currently_checked) in nested {
                // Only toggle if state differs from target
                if currently_checked != new_checked {
                    checkboxes_to_toggle.push((offset, currently_checked));
                }
            }
        } else {
            // No list_item found, just toggle the clicked checkbox
            checkboxes_to_toggle.push((checkbox_byte_start, is_checked));
        }

        // Sort by offset descending so we can modify without invalidating earlier offsets
        checkboxes_to_toggle.sort_by_key(|c| std::cmp::Reverse(c.0));

        // Toggle each checkbox (a length-preserving replace, so the order is free; kept
        // descending all the same).
        for (offset, _currently_checked) in &checkboxes_to_toggle {
            self.set_checkbox(*offset, new_checked, &mut cursor_pos);
        }

        // Propagate upward: if checking and all siblings are now checked, check parent
        // If unchecking, uncheck parent if it was checked
        self.propagate_checkbox_up(checkbox_byte_start, new_checked, &mut cursor_pos);

        let text_after = self.buffer.text();
        self.buffer.coalesce_since(
            head_before,
            &text_before,
            &text_after,
            cursor_before,
            cursor_pos,
        );

        self.selection = Selection::new(cursor_pos, cursor_pos);
    }

    /// Propagate checkbox state upward through parent list items.
    fn propagate_checkbox_up(
        &mut self,
        list_item_start: usize,
        checked: bool,
        cursor_pos: &mut usize,
    ) {
        // Find parent checkbox
        let parent_info = self.find_parent_checkbox(list_item_start);
        let Some((parent_offset, parent_checked)) = parent_info else {
            return;
        };

        if checked {
            // When checking: only auto-check parent if ALL siblings are now checked
            let siblings = self.find_sibling_checkboxes(list_item_start);
            let all_checked = siblings.iter().all(|(_, is_checked)| *is_checked);

            if all_checked && !parent_checked {
                self.set_checkbox(parent_offset, true, cursor_pos);
                // Recursively propagate up
                self.propagate_checkbox_up(parent_offset, true, cursor_pos);
            }
        } else {
            // When unchecking: uncheck parent if it was checked
            if parent_checked {
                self.set_checkbox(parent_offset, false, cursor_pos);
                // Recursively propagate up
                self.propagate_checkbox_up(parent_offset, false, cursor_pos);
            }
        }
    }

    /// Propagate checkbox state after editing (insert/delete): if the current line has a
    /// checkbox, propagate from it; otherwise re-evaluate the enclosing parent checkbox.
    fn propagate_checkbox_after_edit(&mut self) {
        let cursor_offset = self.cursor().offset;
        let line_idx = self.buffer.byte_to_line(cursor_offset);
        let markers = self.buffer.line_markers(line_idx);

        if let Some(is_checked) = markers.checkbox() {
            // Current line has a checkbox - propagate from it
            if let Some(checkbox_byte_start) = self.checkbox_byte_offset(&markers, is_checked) {
                let mut cursor_pos = cursor_offset;
                self.propagate_checkbox_up(checkbox_byte_start, is_checked, &mut cursor_pos);
                self.selection = Selection::new(cursor_pos, cursor_pos);
            }
        } else {
            // No checkbox on current line - maybe we deleted one.
            // Check if there's a parent checkbox that needs re-evaluation.
            self.propagate_from_parent_checkbox();
        }
    }

    /// When current line has no checkbox, find parent checkbox and re-evaluate it.
    fn propagate_from_parent_checkbox(&mut self) {
        let cursor_offset = self.cursor().offset;

        // Try to find a parent checkbox using tree-sitter.
        // If cursor is at end of file or outside a node, try one position back.
        let parent_info = self.find_parent_checkbox(cursor_offset).or_else(|| {
            if cursor_offset > 0 {
                self.find_parent_checkbox(cursor_offset - 1)
            } else {
                None
            }
        });

        let Some(parent_info) = parent_info else {
            return;
        };

        // Also need to find siblings from a valid position
        let sibling_offset =
            if self.find_sibling_checkboxes(cursor_offset).is_empty() && cursor_offset > 0 {
                cursor_offset - 1
            } else {
                cursor_offset
            };

        let (parent_checkbox_offset, parent_checked) = parent_info;

        // Find siblings using the adjusted offset
        let siblings = self.find_sibling_checkboxes(sibling_offset);

        // If no siblings with checkboxes, nothing to propagate
        if siblings.is_empty() {
            // No sibling checkboxes - if parent was checked, it should stay checked
            // (the deleted item wasn't affecting the parent's state)
            return;
        }

        let all_siblings_checked = siblings.iter().all(|(_, checked)| *checked);
        let mut cursor_pos = cursor_offset;

        if all_siblings_checked && !parent_checked {
            // All remaining siblings are checked, check the parent
            self.set_checkbox(parent_checkbox_offset, true, &mut cursor_pos);
            self.propagate_checkbox_up(parent_checkbox_offset, true, &mut cursor_pos);
            self.selection = Selection::new(cursor_pos, cursor_pos);
        } else if !all_siblings_checked && parent_checked {
            // Some siblings unchecked, uncheck the parent
            self.set_checkbox(parent_checkbox_offset, false, &mut cursor_pos);
            self.propagate_checkbox_up(parent_checkbox_offset, false, &mut cursor_pos);
            self.selection = Selection::new(cursor_pos, cursor_pos);
        }
    }

    /// Flip a single checkbox's state byte (`[ ]`↔`[x]`). The replace is length-preserving,
    /// so `cursor_pos` stays valid. The task's text is left alone: a completed task reads as
    /// done by its dimmed rendering, not by `~~` written into the document.
    fn set_checkbox(&mut self, checkbox_offset: usize, checked: bool, cursor_pos: &mut usize) {
        let content_start = checkbox_offset + 1; // skip '['
        let content_end = content_start + 1;
        let new_content = if checked { "x" } else { " " };
        self.buffer
            .replace(content_start..content_end, new_content, *cursor_pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trim leading newline from raw string literals for readability.
    /// Allows writing:
    /// ```
    /// r#"
    /// - item one
    /// - item two
    /// "#
    /// ```
    fn trim_raw(s: &str) -> &str {
        s.strip_prefix('\n').unwrap_or(s)
    }

    /// Helper to create an EditorState with cursor at a specific position.
    /// The cursor position is indicated by | in the input string.
    fn editor_with_cursor(input: &str) -> EditorState {
        let input = trim_raw(input);
        let cursor_pos = input
            .find('|')
            .expect("Input must contain | for cursor position");
        let content = input.replace('|', "");
        let mut state = EditorState::new(&content);
        state.set_cursor(cursor_pos);
        state
    }

    /// Helper to check editor state matches expected content with cursor.
    fn assert_editor_eq(state: &EditorState, expected: &str) {
        let expected = trim_raw(expected);
        let text = state.text();
        let cursor = state.cursor().offset;
        let mut actual = String::new();
        actual.push_str(&text[..cursor]);
        actual.push('|');
        actual.push_str(&text[cursor..]);
        assert_eq!(actual, expected);
    }

    /// Helper to check editor state with selection.
    /// Format: `<` marks start of selection, `|` marks head (cursor), `>` marks end.
    /// Examples:
    ///   - `|hello` - cursor at start, no selection
    ///   - `<hello|>` - "hello" selected, cursor at end
    ///   - `<|hello>` - "hello" selected, cursor at start
    fn assert_selection_eq(state: &EditorState, expected: &str) {
        let expected = trim_raw(expected);
        let text = state.text();
        let selection = &state.selection;

        let anchor = selection.anchor;
        let head = selection.head;
        let start = anchor.min(head);
        let end = anchor.max(head);
        let is_collapsed = anchor == head;

        let mut actual = String::new();
        let mut byte_pos = 0;

        for c in text.chars() {
            if !is_collapsed && byte_pos == start {
                actual.push('<');
            }
            if byte_pos == head {
                actual.push('|');
            }
            if !is_collapsed && byte_pos == end {
                actual.push('>');
            }
            actual.push(c);
            byte_pos += c.len_utf8();
        }

        // Handle markers at end of text
        if !is_collapsed && byte_pos == start {
            actual.push('<');
        }
        if byte_pos == head {
            actual.push('|');
        }
        if !is_collapsed && byte_pos == end {
            actual.push('>');
        }

        assert_eq!(actual, expected, "Selection mismatch");
    }

    mod click_tests {
        use super::*;

        #[test]
        fn click_sets_cursor() {
            let mut state = editor_with_cursor("hello| world");
            state.handle_click(0, false, 1);
            assert_editor_eq(&state, "|hello world");
        }

        #[test]
        fn click_middle() {
            let mut state = editor_with_cursor("|hello world");
            state.handle_click(6, false, 1);
            assert_editor_eq(&state, "hello |world");
        }

        #[test]
        fn shift_click_extends_selection() {
            let mut state = editor_with_cursor("hello| world");
            state.handle_click(11, true, 1);
            assert_selection_eq(&state, "hello< world|>");
        }

        #[test]
        fn shift_click_backward() {
            let mut state = editor_with_cursor("hello| world");
            state.handle_click(0, true, 1);
            assert_selection_eq(&state, "<|hello> world");
        }

        #[test]
        fn double_click_selects_word() {
            let mut state = editor_with_cursor("|hello world");
            state.handle_click(2, false, 2);
            assert_selection_eq(&state, "<hello|> world");
        }

        #[test]
        fn double_click_second_word() {
            let mut state = editor_with_cursor("|hello world");
            state.handle_click(8, false, 2);
            assert_selection_eq(&state, "hello <world|>");
        }

        #[test]
        fn triple_click_selects_line() {
            let mut state = editor_with_cursor("|hello world");
            state.handle_click(2, false, 3);
            assert_selection_eq(&state, "<hello world|>");
        }

        #[test]
        fn drag_extends_selection() {
            let mut state = editor_with_cursor("|hello world");
            state.handle_click(0, false, 1);
            state.handle_drag(5);
            assert_selection_eq(&state, "<hello|> world");
        }

        #[test]
        fn drag_backward() {
            let mut state = editor_with_cursor("hello world|");
            state.handle_click(11, false, 1);
            state.handle_drag(6);
            assert_selection_eq(&state, "hello <|world>");
        }
    }

    mod cursor_movement_tests {
        use super::*;

        #[test]
        fn move_left() {
            let mut state = editor_with_cursor("hel|lo");
            state.move_left();
            assert_editor_eq(&state, "he|llo");
        }

        #[test]
        fn move_left_at_start() {
            let mut state = editor_with_cursor("|hello");
            state.move_left();
            assert_editor_eq(&state, "|hello");
        }

        #[test]
        fn move_right() {
            let mut state = editor_with_cursor("he|llo");
            state.move_right();
            assert_editor_eq(&state, "hel|lo");
        }

        #[test]
        fn move_right_at_end() {
            let mut state = editor_with_cursor("hello|");
            state.move_right();
            assert_editor_eq(&state, "hello|");
        }

        #[test]
        fn move_up() {
            let mut state = editor_with_cursor("line one\nline |two\nline three");
            state.move_up();
            assert_editor_eq(&state, "line |one\nline two\nline three");
        }

        #[test]
        fn sticky_goal_column_survives_short_line() {
            // Down through a 2-char line then Down again restores the original column 7,
            // instead of clamping permanently to the short line's end.
            let mut state = editor_with_cursor("0123456|789\nab\nXYZ0123456");
            state.move_down(); // clamps to end of "ab"
            state.move_down(); // restores column 7 on the long last line
            assert_editor_eq(&state, "0123456789\nab\nXYZ0123|456");
        }

        #[test]
        fn goal_column_reset_by_horizontal_move() {
            // A horizontal move between vertical moves invalidates the goal column.
            let mut state = editor_with_cursor("0123456|789\nab\nXYZ0123456");
            state.move_down(); // end of "ab" (column 2)
            state.move_left(); // column 1 — invalidates the goal
            state.move_down(); // uses column 1, not 7
            assert_editor_eq(&state, "0123456789\nab\nX|YZ0123456");
        }

        #[test]
        fn move_up_from_first_line() {
            let mut state = editor_with_cursor("hel|lo\nworld");
            state.move_up();
            assert_editor_eq(&state, "|hello\nworld");
        }

        #[test]
        fn move_down() {
            let mut state = editor_with_cursor("line |one\nline two\nline three");
            state.move_down();
            assert_editor_eq(&state, "line one\nline |two\nline three");
        }

        #[test]
        fn move_down_from_last_line() {
            let mut state = editor_with_cursor("hello\nwor|ld");
            state.move_down();
            assert_editor_eq(&state, "hello\nworld|");
        }

        #[test]
        fn move_up_preserves_column() {
            let mut state = editor_with_cursor("short\nlonger line|");
            state.move_up();
            assert_editor_eq(&state, "short|\nlonger line");
        }

        #[test]
        fn move_to_line_start() {
            let mut state = editor_with_cursor("hello\nwor|ld");
            state.move_to_line_start();
            assert_editor_eq(&state, "hello\n|world");
        }

        #[test]
        fn move_to_line_end() {
            let mut state = editor_with_cursor("hello\nwor|ld");
            state.move_to_line_end();
            assert_editor_eq(&state, "hello\nworld|");
        }
    }

    // ========================================================================
    // New "raw markdown" behavior tests
    // These test the simplified, non-controlling editing paradigm.
    // ========================================================================

    mod enter_tests {
        use super::*;

        // --- Enter: a new item, or the end of an empty one (like Zettlr) ---

        #[test]
        fn enter_on_paragraph_inserts_newline() {
            let mut state = editor_with_cursor("Hello world|");
            state.enter();
            assert_editor_eq(&state, "Hello world\n|");
        }

        #[test]
        fn enter_on_heading_inserts_newline() {
            let mut state = editor_with_cursor("# Hello|");
            state.enter();
            assert_editor_eq(&state, "# Hello\n|");
        }

        #[test]
        fn enter_on_list_item_continues_list() {
            let mut state = editor_with_cursor("- item one|");
            state.enter();
            assert_editor_eq(&state, "- item one\n- |");
        }

        #[test]
        fn enter_on_numbered_item_continues_with_next_number() {
            let mut state = editor_with_cursor("1. one\n2. two|");
            state.enter();
            assert_editor_eq(&state, "1. one\n2. two\n3. |");
        }

        #[test]
        fn enter_on_task_continues_with_unchecked_box() {
            let mut state = editor_with_cursor("- [x] done|");
            state.enter();
            assert_editor_eq(&state, "- [x] done\n- [ ] |");
        }

        #[test]
        fn enter_mid_item_splits_it() {
            let mut state = editor_with_cursor("- one|two");
            state.enter();
            assert_editor_eq(&state, "- one\n- |two");
        }

        #[test]
        fn enter_before_the_marker_moves_the_item_down() {
            let mut state = editor_with_cursor("x\n|- item");
            state.enter();
            assert_editor_eq(&state, "x\n\n|- item");
        }

        #[test]
        fn enter_on_blockquote_continues_blockquote() {
            let mut state = editor_with_cursor("> quote|");
            state.enter();
            assert_editor_eq(&state, "> quote\n> |");
        }

        #[test]
        fn enter_on_nested_container_continues_all() {
            let mut state = editor_with_cursor("> - item|");
            state.enter();
            assert_editor_eq(&state, "> - item\n> - |");
        }

        #[test]
        fn enter_on_empty_list_item_ends_the_list() {
            let mut state = editor_with_cursor("- item one\n- |");
            state.enter();
            assert_editor_eq(&state, "- item one\n\n|");
        }

        #[test]
        fn enter_on_empty_task_ends_the_list() {
            let mut state = editor_with_cursor("- [ ] a\n- [ ] |");
            state.enter();
            assert_editor_eq(&state, "- [ ] a\n\n|");
        }

        #[test]
        fn enter_on_empty_item_in_quote_keeps_the_quote() {
            let mut state = editor_with_cursor("> - a\n> - |");
            state.enter();
            assert_editor_eq(&state, "> - a\n>\n> |");
        }

        /// Ending an item amid a list splits it: text typed on the line left is a
        /// paragraph, and the items after it a list of their own, starting over from 1
        /// (the only number a list right under a paragraph may start with).
        #[test]
        fn enter_on_empty_item_mid_list_splits_the_list() {
            let mut state = editor_with_cursor("1. a\n2. |\n3. c\n4. d\n");
            state.enter();
            assert_editor_eq(&state, "1. a\n\n|\n2. c\n3. d\n");
            state.insert_typed("x");
            assert_editor_eq(&state, "1. a\n\nx|\n1. c\n2. d\n");
            // Another number under the paragraph goes back to 1.
            let at = state.buffer.text().find("1. c").unwrap();
            state.selection = Selection::new(at, at + 1);
            state.insert_typed("5");
            assert_eq!(state.buffer.text(), "1. a\n\nx\n1. c\n2. d\n");
            // After a blank line, a list may start anywhere.
            let at = state.buffer.text().find("1. c").unwrap();
            state.selection = Selection::new(at, at);
            state.insert_text("\n");
            assert_eq!(state.buffer.text(), "1. a\n\nx\n\n1. c\n2. d\n");
            state.selection = Selection::new(at + 1, at + 2);
            state.insert_typed("5");
            assert_eq!(state.buffer.text(), "1. a\n\nx\n\n5. c\n6. d\n");
        }

        #[test]
        fn enter_on_empty_item_after_a_blank_line_adds_no_other() {
            let mut state = editor_with_cursor("- a\n\n- |");
            state.enter();
            assert_editor_eq(&state, "- a\n\n|");
        }

        #[test]
        fn enter_on_empty_blockquote_line_ends_the_quote() {
            let mut state = editor_with_cursor("> quote one\n> |");
            state.enter();
            assert_editor_eq(&state, "> quote one\n|");
        }

        #[test]
        fn enter_in_code_block_keeps_indent() {
            let mut state = editor_with_cursor("```rust\n    let x = 1;|\n```");
            state.enter();
            assert_editor_eq(&state, "```rust\n    let x = 1;\n    |\n```");
        }

        #[test]
        fn enter_on_code_fence_inserts_newline() {
            let mut state = editor_with_cursor("```rust|");
            state.enter();
            assert_editor_eq(&state, "```rust\n|");
        }

        #[test]
        fn enter_preserves_soft_wrap_style() {
            // Adjacent lines without blank line between them
            let mut state = editor_with_cursor("First sentence.\nSecond sentence.|");
            state.enter();
            assert_editor_eq(&state, "First sentence.\nSecond sentence.\n|");
        }

        // --- Shift+Enter: a new line in the same item, no marker ---

        #[test]
        fn shift_enter_on_list_item_indents_under_its_text() {
            let mut state = editor_with_cursor("- item one|");
            state.shift_enter();
            assert_editor_eq(&state, "- item one\n  |");
        }

        #[test]
        fn shift_enter_on_numbered_item_indents_under_its_text() {
            let mut state = editor_with_cursor("1. item|");
            state.shift_enter();
            assert_editor_eq(&state, "1. item\n   |");
        }

        #[test]
        fn shift_enter_on_nested_container_stays_inside() {
            let mut state = editor_with_cursor("> - item|");
            state.shift_enter();
            assert_editor_eq(&state, "> - item\n>   |");
        }

        #[test]
        fn shift_enter_on_blockquote_stays_in_the_quote() {
            let mut state = editor_with_cursor("> quote|");
            state.shift_enter();
            assert_editor_eq(&state, "> quote\n> |");
        }

        #[test]
        fn shift_enter_on_paragraph_just_inserts_newline() {
            let mut state = editor_with_cursor("Hello world|");
            state.shift_enter();
            assert_editor_eq(&state, "Hello world\n|");
        }

        #[test]
        fn shift_enter_in_code_block_just_inserts_newline() {
            let mut state = editor_with_cursor("```\n  code|");
            state.shift_enter();
            assert_editor_eq(&state, "```\n  code\n|");
        }

        // --- Ctrl+Enter: Enter outside tables ---

        #[test]
        fn ctrl_enter_outside_tables_is_enter() {
            let mut state = editor_with_cursor("- item|");
            state.ctrl_enter();
            assert_editor_eq(&state, "- item\n- |");
        }
    }

    mod raw_backspace_tests {
        use super::*;

        #[test]
        fn backspace_deletes_char() {
            let mut state = editor_with_cursor("hello|");
            state.delete_backward();
            assert_editor_eq(&state, "hell|");
        }

        #[test]
        fn backspace_at_line_start_joins_lines() {
            let mut state = editor_with_cursor("line one\n|line two");
            state.delete_backward();
            assert_editor_eq(&state, "line one|line two");
        }

        #[test]
        fn backspace_deletes_whole_zwj_emoji() {
            // A family emoji is 7 codepoints but one grapheme — deleted whole, not split.
            let mut state = editor_with_cursor("a👨‍👩‍👧‍👦|b");
            state.delete_backward();
            assert_editor_eq(&state, "a|b");
        }

        #[test]
        fn backspace_deletes_whole_combining_accent() {
            // "é" as base 'e' + combining acute (2 codepoints, 1 grapheme).
            let mut state = editor_with_cursor("e\u{301}|");
            state.delete_backward();
            assert_editor_eq(&state, "|");
        }

        #[test]
        fn backspace_deletes_entire_list_marker() {
            let mut state = editor_with_cursor("- |");
            state.delete_backward();
            assert_editor_eq(&state, "|");
        }

        #[test]
        fn backspace_deletes_innermost_marker_first() {
            let mut state = editor_with_cursor("> - |");
            state.delete_backward();
            assert_editor_eq(&state, "> |");
        }

        #[test]
        fn backspace_then_deletes_outer_marker() {
            let mut state = editor_with_cursor("> |");
            state.delete_backward();
            assert_editor_eq(&state, "|");
        }

        #[test]
        fn backspace_deletes_entire_indent() {
            // Indent after list item is atomic - need context for tree-sitter to recognize it
            let mut state = editor_with_cursor("- item\n  |text");
            state.delete_backward();
            assert_editor_eq(&state, "- item\n|text");
        }

        #[test]
        fn backspace_in_middle_of_text_deletes_char() {
            let mut state = editor_with_cursor("- item o|ne");
            state.delete_backward();
            assert_editor_eq(&state, "- item |ne");
        }

        #[test]
        fn backspace_on_empty_line_after_list_joins() {
            let mut state = editor_with_cursor("- item one\n|");
            state.delete_backward();
            assert_editor_eq(&state, "- item one|");
        }

        #[test]
        fn backspace_sequence_through_markers_and_join() {
            // Start: "- item one\n- |"
            // Backspace 1: delete "- " marker -> "- item one\n|"
            // Backspace 2: join lines -> "- item one|"
            let mut state = editor_with_cursor("- item one\n- |");
            state.delete_backward();
            assert_editor_eq(&state, "- item one\n|");
            state.delete_backward();
            assert_editor_eq(&state, "- item one|");
        }

        #[test]
        fn backspace_with_content_after_cursor_deletes_marker() {
            let mut state = editor_with_cursor("- |two");
            state.delete_backward();
            assert_editor_eq(&state, "|two");
        }

        #[test]
        fn backspace_deletes_entire_task_list_marker() {
            // Task list now has separate Checkbox and ListItem markers
            // First backspace deletes the checkbox, second deletes the list marker
            let mut state = editor_with_cursor("- [ ] |");
            state.delete_backward();
            assert_editor_eq(&state, "- |");
            state.delete_backward();
            assert_editor_eq(&state, "|");
        }

        #[test]
        fn backspace_deletes_checked_task_list_marker() {
            let mut state = editor_with_cursor("- [x] |");
            state.delete_backward();
            assert_editor_eq(&state, "- |");
            state.delete_backward();
            assert_editor_eq(&state, "|");
        }
    }

    mod raw_tab_tests {
        use super::*;

        // --- Tab cycling through states ---
        // Tree-based: cycle is marker → (para indent if blank) → nested marker → empty

        #[test]
        fn tab_on_empty_line_after_list_adds_marker() {
            // Blank line cycle: ["- ", "  ", "  - ", ""]
            let mut state = editor_with_cursor("- item\n|");
            state.tab();
            assert_editor_eq(&state, "- item\n- |");
        }

        #[test]
        fn tab_twice_after_list_adds_nested_marker() {
            // Cycle is: "" -> "- " -> "  " -> "  - " -> ""
            let mut state = editor_with_cursor("- item\n|");
            state.tab();
            state.tab();
            assert_editor_eq(&state, "- item\n  |"); // para indent
            state.tab();
            assert_editor_eq(&state, "- item\n  - |"); // nested marker
        }

        #[test]
        fn tab_three_times_cycles_back() {
            // Cycle is: "" -> "- " -> "  " -> "  - " -> "" (4 states)
            let mut state = editor_with_cursor("- item\n|");
            state.tab();
            state.tab();
            state.tab();
            state.tab();
            assert_editor_eq(&state, "- item\n|");
        }

        #[test]
        fn tab_cycles_ordered_list_after_checkbox() {
            // Bug case: ordered list preceded by checkbox content
            // Cycle should be: "" -> "2. " -> "   " -> "   1. " -> "" (4 states)
            let mut state = editor_with_cursor("## Writ\n- [ ] item\n\n1. hey\n|");

            state.tab();
            assert_editor_eq(&state, "## Writ\n- [ ] item\n\n1. hey\n2. |");

            state.tab();
            assert_editor_eq(&state, "## Writ\n- [ ] item\n\n1. hey\n   |"); // para indent

            state.tab();
            assert_editor_eq(&state, "## Writ\n- [ ] item\n\n1. hey\n   1. |");

            state.tab();
            assert_editor_eq(&state, "## Writ\n- [ ] item\n\n1. hey\n|");
        }

        #[test]
        fn tab_indents_line_with_content() {
            // Tab should cycle the prefix even when there's content after it
            // Content is preserved and cursor stays in place relative to content
            let mut state = editor_with_cursor("1. hey\n2. asdf|");
            state.tab();
            assert_editor_eq(&state, "1. hey\n   asdf|"); // para indent, content preserved
            state.tab();
            assert_editor_eq(&state, "1. hey\n   1. asdf|"); // nested, content preserved
        }

        #[test]
        fn tab_preserves_unchecked_checkbox_state() {
            // Tab cycling preserves the current line's checkbox state
            // Propagation doesn't happen because tree-sitter can't parse incomplete lines
            // Cycle: "" -> "- [ ] " -> "  " -> "  - [ ] " -> ""
            let mut state = editor_with_cursor("- [x] hey\n- [ ] |");
            state.tab();
            // Checkbox stays unchecked (from current line), no propagation
            assert_editor_eq(&state, "- [x] hey\n  |"); // para indent
            state.tab();
            assert_editor_eq(&state, "- [x] hey\n  - [ ] |"); // nested
            state.tab();
            assert_editor_eq(&state, "- [x] hey\n|");
            state.tab();
            assert_editor_eq(&state, "- [x] hey\n- [ ] |");
        }

        #[test]
        fn tab_preserves_checked_checkbox_state() {
            // Tab cycling preserves the current line's checkbox state
            // Cycle: "" -> "- [x] " -> "  " -> "  - [x] " -> ""
            let mut state = editor_with_cursor("- [ ] hey\n- [x] |");
            state.tab();
            // Checkbox stays checked (from current line), no propagation
            assert_editor_eq(&state, "- [ ] hey\n  |"); // para indent
            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n  - [x] |"); // nested
            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n|");
            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n- [x] |");
        }

        #[test]
        fn tab_new_checkbox_defaults_unchecked() {
            // Starting from empty line, new checkboxes default to unchecked
            // Cycle: "" -> "- [ ] " -> "  " -> "  - [ ] " -> ""
            let mut state = editor_with_cursor("- [x] ~~hey~~\n|");
            state.tab(); // sibling: - [ ] |
            assert_editor_eq(&state, "- [x] ~~hey~~\n- [ ] |");
            state.tab(); // para indent
            assert_editor_eq(&state, "- [x] ~~hey~~\n  |");
            state.tab(); // nested: - [ ] |
            assert_editor_eq(&state, "- [x] ~~hey~~\n  - [ ] |");
        }

        #[test]
        fn typing_after_tab_propagates_checkbox() {
            // Tab creates incomplete line "- [ ] |" which tree-sitter can't parse.
            // Once we type content, tree-sitter recognizes it and propagation happens.
            // Cycle: "" -> "- [ ] " -> "  " -> "  - [ ] " -> ""
            let mut state = editor_with_cursor("- [x] hey\n|");
            state.tab(); // "- [ ] |" - incomplete, no propagation yet
            assert_editor_eq(&state, "- [x] hey\n- [ ] |");
            state.tab(); // para indent
            assert_editor_eq(&state, "- [x] hey\n  |");
            state.tab(); // nest it: "  - [ ] |"
            assert_editor_eq(&state, "- [x] hey\n  - [ ] |");
            // Type a character - now tree-sitter can parse, propagation unchecks parent
            state.insert_text("a");
            assert_editor_eq(&state, "- [ ] hey\n  - [ ] a|");
        }

        #[test]
        fn delete_backward_propagates_checkbox() {
            // Deleting content can affect checkbox propagation
            let mut state = editor_with_cursor("- [x] hey\n  - [ ] ab|");
            // Delete 'b' - still has content, propagation runs (parent stays unchecked)
            state.delete_backward();
            assert_editor_eq(&state, "- [ ] hey\n  - [ ] a|");
        }

        #[test]
        fn delete_forward_propagates_checkbox() {
            // Deleting content forward can affect checkbox propagation
            let mut state = editor_with_cursor("- [x] hey\n  - [ ] |ab");
            // Delete 'a' - still has content, propagation runs
            state.delete_forward();
            assert_editor_eq(&state, "- [ ] hey\n  - [ ] |b");
        }

        #[test]
        fn delete_checkbox_marker_rechecks_parent() {
            // Start with checked parent and one checked nested child
            // Cycle: "" -> "- [ ] " -> "  " -> "  - [ ] " -> ""
            let mut state = editor_with_cursor("- [x] parent\n  - [x] nested\n|");
            // Tab three times to create a new nested unchecked checkbox (with para indent now in cycle)
            state.tab();
            state.tab();
            state.tab();
            assert_editor_eq(&state, "- [x] parent\n  - [x] nested\n  - [ ] |");
            // Type to make it parseable - this should uncheck the parent
            state.insert_text("new");
            assert_editor_eq(&state, "- [ ] parent\n  - [x] nested\n  - [ ] new|");
            // Now delete backwards to remove the unchecked child entirely
            // First delete the content
            state.delete_backward();
            state.delete_backward();
            state.delete_backward();
            assert_editor_eq(&state, "- [ ] parent\n  - [x] nested\n  - [ ] |");
            // Delete the checkbox marker
            state.delete_backward();
            assert_editor_eq(&state, "- [ ] parent\n  - [x] nested\n  - |");
            // Delete the list marker
            state.delete_backward();
            assert_editor_eq(&state, "- [x] parent\n  - [x] nested\n  |");
        }

        #[test]
        fn tab_with_blank_line_between_still_works() {
            // Tree-sitter includes blank lines in list_item
            let mut state = editor_with_cursor("- item\n\n|");
            state.tab();
            assert_editor_eq(&state, "- item\n\n- |");
        }

        #[test]
        fn tab_with_two_blank_lines_still_works() {
            // Tree-sitter includes multiple blank lines in list_item
            let mut state = editor_with_cursor("- item\n\n\n|");
            state.tab();
            assert_editor_eq(&state, "- item\n\n\n- |");
        }

        #[test]
        fn tab_on_blockquote_context_adds_marker() {
            let mut state = editor_with_cursor("> quote\n|");
            state.tab();
            assert_editor_eq(&state, "> quote\n> |");
        }

        #[test]
        fn tab_twice_on_blockquote_context_cycles_back() {
            let mut state = editor_with_cursor("> quote\n|");
            state.tab();
            state.tab();
            assert_editor_eq(&state, "> quote\n|");
        }

        #[test]
        fn tab_on_nested_context_cycles() {
            // Cycle: ["> ", "> - ", ">   ", ">   - ", ""]
            let mut state = editor_with_cursor("> - item\n|");

            state.tab();
            assert_editor_eq(&state, "> - item\n> |");

            state.tab();
            assert_editor_eq(&state, "> - item\n> - |");

            state.tab();
            assert_editor_eq(&state, "> - item\n>   |"); // para indent

            state.tab();
            assert_editor_eq(&state, "> - item\n>   - |");

            state.tab();
            assert_editor_eq(&state, "> - item\n|");
        }

        // --- Shift+Tab cycling backwards ---

        #[test]
        fn shift_tab_cycles_backwards() {
            // Cycle: ["- ", "  ", "  - ", ""]
            // Backwards from "" goes to "  - "
            let mut state = editor_with_cursor("- item\n|");
            state.shift_tab();
            assert_editor_eq(&state, "- item\n  - |");
        }

        #[test]
        fn shift_tab_from_marker_goes_to_empty() {
            let mut state = editor_with_cursor("- item\n- |");
            state.shift_tab();
            assert_editor_eq(&state, "- item\n|");
        }

        #[test]
        fn shift_tab_from_nested_marker_goes_to_marker() {
            // "  - " is nested list, cycle found via ERROR handling
            // Cycle backwards: "  - " -> "  " -> "- " -> ""
            let mut state = editor_with_cursor("- item\n  - |");
            state.shift_tab();
            assert_editor_eq(&state, "- item\n  |"); // para indent
            state.shift_tab();
            assert_editor_eq(&state, "- item\n- |");
        }

        #[test]
        fn tab_after_blank_line_includes_para_indent() {
            // With blank line, para indent should be in cycle
            // Cycle: ["- ", "  ", "  - ", "    ", "    - ", ""]
            let mut state = editor_with_cursor("- parent\n  - nested\n\n|");

            state.tab();
            assert_editor_eq(&state, "- parent\n  - nested\n\n- |");

            state.tab();
            assert_editor_eq(&state, "- parent\n  - nested\n\n  |"); // para indent

            state.tab();
            assert_editor_eq(&state, "- parent\n  - nested\n\n  - |");

            state.tab();
            assert_editor_eq(&state, "- parent\n  - nested\n\n    |"); // nested para indent

            state.tab();
            assert_editor_eq(&state, "- parent\n  - nested\n\n    - |");

            state.tab();
            assert_editor_eq(&state, "- parent\n  - nested\n\n|"); // back to empty
        }

        #[test]
        fn tab_no_blank_line_includes_para_indent() {
            // Para indent is now always in cycle, even without blank line
            // Cycle: ["- ", "  ", "  - ", "    ", "    - ", ""]
            let mut state = editor_with_cursor("- parent item\n  - nested with tab\n|");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n- |");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n  |"); // para indent

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n  - |");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n    |"); // nested para indent

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n    - |");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n|");
        }

        #[test]
        fn tab_with_trailing_newline() {
            // Cursor on line with newline after it - should still cycle correctly
            // Cycle: ["- ", "  ", "  - ", "    ", "    - ", ""]
            let mut state = editor_with_cursor("- parent item\n  - nested with tab\n|\n");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n- |\n");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n  |\n"); // para indent

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n  - |\n");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n    |\n"); // nested para indent

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n    - |\n");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n|\n");
        }

        #[test]
        fn tab_task_list_uses_list_marker_width_not_full_marker() {
            // Task list "- [ ] " is 6 chars, but para indent should use list marker width (2)
            // Cycle: ["- [ ] ", "  ", "  - [ ] ", ""]
            let mut state = editor_with_cursor("- [ ] hey\n\n|");

            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n\n- [ ] |");

            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n\n  |"); // 2 spaces, not 6

            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n\n  - [ ] |"); // nested at 2 spaces

            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n\n|");
        }
    }

    mod table_editing_tests {
        use super::*;

        /// 2-col table with cell content offsets:
        /// header cells "a"@2, "b"@6; body[0] "1"@26, "2"@30.
        const TABLE_2X1: &str = "| a | b |\n| --- | --- |\n| 1 | 2 |\n";

        // --- Stage 5: creation via smart Enter ---

        #[test]
        fn shift_enter_on_lone_pipe_header_just_inserts_newline() {
            // Table creation is Enter's: Shift+Enter stays a plain new line.
            let mut state = EditorState::new("| a | b |");
            state.set_cursor(9); // end of line
            state.shift_enter();
            assert_eq!(state.text(), "| a | b |\n");
        }

        #[test]
        fn enter_completes_lone_pipe_header_into_table() {
            let mut state = EditorState::new("| a | b |");
            state.set_cursor(9);
            state.enter();
            assert_eq!(state.text(), "| a | b |\n| --- | --- |\n|  |  |");
            assert_eq!(state.cursor().offset, 26);
        }

        #[test]
        fn create_rejects_unbounded_pipe_row() {
            // Not bounded by leading + trailing pipes → not treated as a table header.
            let mut state = EditorState::new("a | b");
            state.set_cursor(5);
            assert!(!state.maybe_create_table());
            assert_eq!(state.text(), "a | b");
        }

        #[test]
        fn create_does_nothing_on_existing_table_row() {
            // Header row already followed by a delimiter (a real table).
            let mut state = EditorState::new("| a | b |\n| --- | --- |\n");
            state.set_cursor(9);
            assert!(!state.maybe_create_table());
            assert_eq!(state.text(), "| a | b |\n| --- | --- |\n");
        }

        #[test]
        fn create_does_nothing_inside_code_block() {
            let mut state = EditorState::new("```\n| a | b |\n```\n");
            state.set_cursor(13); // end of "| a | b |"
            assert!(!state.maybe_create_table());
            assert_eq!(state.text(), "```\n| a | b |\n```\n");
        }

        #[test]
        fn create_does_nothing_on_plain_paragraph() {
            let mut state = EditorState::new("hello world");
            state.set_cursor(11);
            assert!(!state.maybe_create_table());
            assert_eq!(state.text(), "hello world");
        }

        #[test]
        fn create_does_nothing_when_cursor_not_at_line_end() {
            let mut state = EditorState::new("| a | b |");
            state.set_cursor(4); // middle of the line
            assert!(!state.maybe_create_table());
            assert_eq!(state.text(), "| a | b |");
        }

        // --- Stage 6: cell navigation via Tab / Shift+Tab ---

        #[test]
        fn tab_moves_to_next_cell_in_row() {
            let mut state = EditorState::new(TABLE_2X1);
            state.set_cursor(2); // header cell (0,0), before "a"
            state.tab();
            assert_eq!(state.cursor().offset, 6); // header cell (0,1), before "b"
        }

        #[test]
        fn tab_at_row_end_moves_to_next_row_first_cell() {
            let mut state = EditorState::new(TABLE_2X1);
            state.set_cursor(6); // header last cell
            state.tab();
            assert_eq!(state.cursor().offset, 26); // body[0] first cell, before "1"
        }

        #[test]
        fn tab_on_last_cell_of_last_row_creates_new_row() {
            let mut state = EditorState::new(TABLE_2X1);
            state.set_cursor(30); // body[0] last cell, before "2"
            state.tab();
            assert_eq!(
                state.text(),
                "| a | b |\n| --- | --- |\n| 1 | 2 |\n|  |  |\n"
            );
            assert_eq!(state.cursor().offset, 36); // new row first empty cell
        }

        #[test]
        fn shift_tab_moves_to_previous_cell() {
            let mut state = EditorState::new(TABLE_2X1);
            state.set_cursor(6); // header cell (0,1)
            state.shift_tab();
            assert_eq!(state.cursor().offset, 2); // header cell (0,0)
        }

        #[test]
        fn shift_tab_at_row_start_moves_to_previous_row_last_cell() {
            let mut state = EditorState::new(TABLE_2X1);
            state.set_cursor(26); // body[0] first cell
            state.shift_tab();
            assert_eq!(state.cursor().offset, 6); // header last cell
        }

        #[test]
        fn shift_tab_on_header_first_cell_is_noop() {
            let mut state = EditorState::new(TABLE_2X1);
            state.set_cursor(2);
            state.shift_tab();
            assert_eq!(state.cursor().offset, 2);
            assert_eq!(state.text(), TABLE_2X1);
        }

        #[test]
        fn ctrl_enter_in_last_row_adds_matched_row() {
            let mut state = EditorState::new(TABLE_2X1);
            state.set_cursor(26); // inside body[0]
            state.ctrl_enter();
            assert_eq!(
                state.text(),
                "| a | b |\n| --- | --- |\n| 1 | 2 |\n|  |  |\n"
            );
            assert_eq!(state.cursor().offset, 36);
        }

        #[test]
        fn typing_a_header_column_grows_the_whole_table() {
            let mut state = EditorState::new("| A | B |\n| - | - |\n| 1 | 2 |\n| 3 | 4 |\n");
            state.set_cursor(9); // end of the header line, after the final '|'
            state.insert_text(" C"); // add a third header cell
            assert_eq!(
                state.text(),
                "| A | B | C\n| - | - | --- |\n| 1 | 2 |   |\n| 3 | 4 |   |\n"
            );
            // And it's a valid 3-column table again.
            let t = &state.buffer.parsed().tables[0];
            assert_eq!(t.ncols, 3);
            assert_eq!(t.body.len(), 2);
        }

        #[test]
        fn adding_a_middle_header_column_inserts_body_columns_in_place() {
            let mut state = EditorState::new("| A | B | C |\n| - | - | - |\n| 1 | 2 | 3 |\n");
            state.set_cursor(9); // right after the pipe following B, before " C"
            state.insert_text(" X |"); // insert a new column between B and C
            assert_eq!(
                state.text(),
                "| A | B | X | C |\n| - | - | --- | - |\n| 1 | 2 |   | 3 |\n"
            );
        }

        #[test]
        fn middle_column_via_empty_cell_lands_in_place() {
            // A bare pipe creates an empty header cell between B and C; the body column
            // must land at that same index, not on the far right.
            let mut state = EditorState::new("| A | B | C |\n| - | - | - |\n| 1 | 2 | 3 |\n");
            state.set_cursor(9); // right after the pipe following B
            state.insert_text("|");
            assert_eq!(
                state.text(),
                "| A | B || C |\n| - | - | --- | - |\n| 1 | 2 |   | 3 |\n"
            );
        }

        #[test]
        fn column_grow_ignores_preexisting_empty_header_cell() {
            // Header already has an empty middle cell; appending a column at the right must
            // grow the body at the RIGHT, not steal the pre-existing empty cell's position.
            let mut state = EditorState::new("| A |  | C |\n| - | - | - |\n| 1 | 2 | 3 |\n");
            let header_end = state.text().find('\n').unwrap();
            state.set_cursor(header_end);
            state.insert_text(" D");
            assert_eq!(
                state.text(),
                "| A |  | C | D\n| - | - | - | --- |\n| 1 | 2 | 3 |   |\n"
            );
        }

        #[test]
        fn typing_in_a_body_cell_does_not_grow_columns() {
            // Editing a body cell must never trigger column growth.
            let mut state = EditorState::new("| A | B |\n| - | - |\n| 1 | 2 |\n");
            let pos = state.text().rfind('1').unwrap() + 1;
            state.set_cursor(pos);
            state.insert_text("x");
            assert_eq!(state.text(), "| A | B |\n| - | - |\n| 1x | 2 |\n");
        }

        #[test]
        fn backspace_removes_just_added_empty_table_row() {
            let mut state = EditorState::new(TABLE_2X1);
            state.set_cursor(26); // inside body[0]
            state.ctrl_enter(); // adds an empty row, caret inside it
            assert!(state.text().ends_with("|  |  |\n"));
            state.delete_backward(); // one backspace removes the whole empty row
            assert_eq!(state.text(), TABLE_2X1);
        }

        #[test]
        fn backspace_in_nonempty_table_row_deletes_char_not_row() {
            let mut state = EditorState::new("| a | b |\n| --- | --- |\n| xy | 2 |\n");
            let pos = state.text().find('y').unwrap() + 1; // just after 'y'
            state.set_cursor(pos);
            state.delete_backward();
            // The 'y' is deleted; the row (and its cells) survive.
            assert!(state.text().contains("| x | 2 |"));
        }

        // --- Issue 2: Tab lands on cell content, never on a pipe ---

        #[test]
        fn tab_lands_on_content_char_in_nonempty_cell() {
            // "| ab | cd |": cell (0,1) content "cd" starts at byte 7 ('c').
            let mut state = EditorState::new("| ab | cd |\n| --- | --- |\n");
            state.set_cursor(2); // cell (0,0), before "ab"
            state.tab();
            assert_eq!(state.cursor().offset, 7);
            assert_eq!(state.buffer.byte_at(7), Some(b'c'));
        }

        #[test]
        fn tab_into_empty_cell_lands_between_pipes() {
            // Body row "|  |  |" starts at byte 24: pipes at 24, 27, 30.
            let mut state = EditorState::new("| a | b |\n| --- | --- |\n|  |  |\n");
            state.set_cursor(6); // header cell (0,1) → Tab → empty body[0] cell 0
            state.tab();
            let off = state.cursor().offset;
            assert!(off > 24 && off < 27, "strictly between the cell's pipes");
            assert_ne!(state.buffer.byte_at(off), Some(b'|'), "not on a pipe");
        }

        #[test]
        fn tab_through_freshly_created_empty_row_never_on_pipe() {
            // Every empty cell reached by tabbing must sit the caret off a pipe.
            let mut state = EditorState::new("| a | b |");
            state.set_cursor(9);
            state.shift_enter(); // create table, land in body cell 0
            for _ in 0..2 {
                let off = state.cursor().offset;
                assert_ne!(
                    state.buffer.byte_at(off),
                    Some(b'|'),
                    "caret never on a pipe"
                );
                state.tab();
            }
        }

        // --- Issue 3: vertical movement into/through a table; left edge counts ---

        #[test]
        fn move_down_from_paragraph_enters_table_block() {
            let mut state =
                EditorState::new("para\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\nafter\n");
            state.set_cursor(0); // paragraph above the table
            state.move_down();
            let (table, _) = state
                .table_context_at_cursor()
                .expect("cursor is in the table after move_down");
            assert!(
                table.block.contains(&state.cursor().offset),
                "cursor offset is within the table block"
            );
        }

        #[test]
        fn move_down_passes_through_every_table_row() {
            let mut state =
                EditorState::new("para\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\nafter\n");
            state.set_cursor(0);
            let mut in_table_lines = 0;
            for _ in 0..5 {
                state.move_down();
                if state.table_context_at_cursor().is_some() {
                    in_table_lines += 1;
                }
            }
            // The header and the body row are passed through; the delimiter row (hidden in
            // the grid) is skipped.
            assert_eq!(in_table_lines, 2, "caret lands on each grid row in turn");
        }

        #[test]
        fn cursor_at_first_table_line_start_is_in_table() {
            let mut state =
                EditorState::new("para\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\nafter\n");
            let start = state.buffer.line_to_byte(1); // first table line (header), column 0
            state.set_cursor(start);
            assert!(
                state.table_context_at_cursor().is_some(),
                "the left edge / line start of a table row counts as in-table"
            );
        }

        // --- Grid editing: the caret moves and edits cell by cell ---

        #[test]
        fn arrows_walk_cell_by_cell() {
            // Header "a"@2, "b"@6; body "1"@26, "2"@30.
            let mut state = EditorState::new(TABLE_2X1);
            state.set_cursor(3); // after "a"
            state.move_right();
            assert_eq!(
                state.cursor().offset,
                6,
                "into the next cell, skipping the pipe"
            );
            state.move_right();
            state.move_right();
            assert_eq!(
                state.cursor().offset,
                26,
                "past the row's end: the next row"
            );
            state.move_left();
            assert_eq!(
                state.cursor().offset,
                7,
                "back to the previous row's last cell"
            );
            state.set_cursor(26);
            state.move_up();
            assert_eq!(state.cursor().offset, 2, "up: same column, header row");
            state.set_cursor(7);
            state.move_down();
            assert_eq!(state.cursor().offset, 30, "down skips the delimiter row");
        }

        #[test]
        fn home_end_stay_in_the_cell() {
            let mut state = EditorState::new("| abc | de |\n| --- | --- |\n");
            state.set_cursor(9); // inside "de"
            let start = state.cursor_in_direction(Direction::LineStart).offset;
            let end = state.cursor_in_direction(Direction::LineEnd).offset;
            assert_eq!((start, end), (8, 10));
        }

        #[test]
        fn moves_from_outside_land_in_a_cell() {
            let mut state = EditorState::new("para\n| a | b |\n| --- | --- |\n");
            state.set_cursor(4); // end of "para"
            state.move_right();
            assert_eq!(
                state.cursor().offset,
                7,
                "on the first cell's text, not the pipe"
            );
        }

        #[test]
        fn backspace_and_delete_stop_at_the_cell_edges() {
            let mut state = EditorState::new(TABLE_2X1);
            state.set_cursor(6); // start of "b"
            state.delete_backward();
            state.set_cursor(3); // end of "a"
            state.delete_forward();
            assert_eq!(state.text(), TABLE_2X1);
        }

        #[test]
        fn typed_pipe_and_newline_stay_in_the_cell() {
            let mut state = EditorState::new(TABLE_2X1);
            state.set_cursor(3);
            state.insert_typed("|x\ny");
            assert_eq!(
                state.text(),
                "| a\\|x<br>y | b |\n| --- | --- |\n| 1 | 2 |\n"
            );
        }

        #[test]
        fn ctrl_enter_goes_down_and_adds_a_row_at_the_end() {
            let mut state = EditorState::new(TABLE_2X1);
            state.set_cursor(7); // header, "b"
            state.ctrl_enter();
            assert_eq!(state.cursor().offset, 30, "same column, next row");
            state.ctrl_enter();
            assert_eq!(
                state.text(),
                "| a | b |\n| --- | --- |\n| 1 | 2 |\n|  |  |\n"
            );
            let off = state.cursor().offset;
            assert_eq!(off, 39, "between the new row's second cell's pipes");
        }

        #[test]
        fn deleting_across_cells_empties_them() {
            let mut state = EditorState::new(TABLE_2X1);
            state.selection = Selection::new(2, 31); // "a" … "2"
            state.delete_backward();
            assert_eq!(state.text(), "|  |  |\n| --- | --- |\n|  |  |\n");
            assert_eq!(state.cursor().offset, 2);
        }

        #[test]
        fn triple_click_selects_the_cell() {
            let mut state = EditorState::new("| abc | de |\n| --- | --- |\n");
            state.handle_click(9, false, 3);
            assert_eq!(state.selection.range(), 8..10);
        }

        // --- Regressions: not in a table ---

        #[test]
        fn tab_outside_table_still_cycles_list() {
            let mut state = editor_with_cursor("- item\n|");
            state.tab();
            assert_editor_eq(&state, "- item\n- |");
        }

        #[test]
        fn enter_outside_table_still_continues_list() {
            let mut state = editor_with_cursor("- item one|");
            state.enter();
            assert_editor_eq(&state, "- item one\n- |");
        }

        /// Enter (and Shift+Enter) in a cell breaks the line inside the cell: `<br>`,
        /// as a table row is one line. The caret steps over it, and Backspace or Delete
        /// remove it, as the one character it shows as.
        #[test]
        fn enter_in_a_cell_breaks_the_line_inside_it() {
            let mut state = EditorState::new(TABLE_2X1);
            state.set_cursor(3); // after "a"
            state.enter();
            assert_eq!(state.text(), "| a<br> | b |\n| --- | --- |\n| 1 | 2 |\n");
            assert_eq!(state.cursor().offset, 7);
            state.shift_enter();
            assert_eq!(
                state.text(),
                "| a<br><br> | b |\n| --- | --- |\n| 1 | 2 |\n"
            );
            state.move_left();
            assert_eq!(state.cursor().offset, 7, "one step back over a `<br>`");
            state.move_left();
            assert_eq!(state.cursor().offset, 3);
            state.move_right();
            assert_eq!(state.cursor().offset, 7);
            state.delete_backward();
            assert_eq!(state.text(), "| a<br> | b |\n| --- | --- |\n| 1 | 2 |\n");
            assert_eq!(state.cursor().offset, 3);
            state.delete_forward();
            assert_eq!(state.text(), TABLE_2X1);
        }
    }

    mod raw_cursor_movement_tests {
        use super::*;

        #[test]
        fn move_left_through_marker_is_atomic() {
            let mut state = editor_with_cursor("- |item");
            state.move_left();
            assert_editor_eq(&state, "|- item");
        }

        #[test]
        fn move_right_through_marker_is_atomic() {
            let mut state = editor_with_cursor("|- item");
            state.move_right();
            assert_editor_eq(&state, "- |item");
        }

        #[test]
        fn move_left_through_nested_markers_one_at_a_time() {
            let mut state = editor_with_cursor("> - |item");
            state.move_left();
            assert_editor_eq(&state, "> |- item");
            state.move_left();
            assert_editor_eq(&state, "|> - item");
        }

        #[test]
        fn move_left_does_not_skip_blank_lines() {
            let mut state = editor_with_cursor("line one\n\n|line three");
            state.move_left();
            assert_editor_eq(&state, "line one\n|\nline three");
        }

        #[test]
        fn move_left_from_blank_line_goes_to_previous() {
            let mut state = editor_with_cursor("line one\n|\nline three");
            state.move_left();
            assert_editor_eq(&state, "line one|\n\nline three");
        }

        #[test]
        fn move_up_maintains_column_in_content_area() {
            let mut state = editor_with_cursor("- item one\n- item |two");
            state.move_up();
            assert_editor_eq(&state, "- item |one\n- item two");
        }

        #[test]
        fn move_left_through_blockquote_ordered_list() {
            let mut state = editor_with_cursor("> 1. |");
            state.move_left();
            assert_editor_eq(&state, "> |1. ");
            state.move_left();
            assert_editor_eq(&state, "|> 1. ");
        }
    }

    mod checkbox_propagation_tests {
        use super::*;

        #[test]
        fn check_parent_checks_all_children() {
            let mut state = editor_with_cursor("- [ ] |parent\n  - [ ] child1\n  - [ ] child2\n");
            state.toggle_checkbox(0);
            let text = state.text();
            assert!(text.contains("[x] parent"), "parent should be checked");
            assert!(text.contains("[x] child1"), "child1 should be checked");
            assert!(text.contains("[x] child2"), "child2 should be checked");
        }

        #[test]
        fn uncheck_parent_unchecks_all_children() {
            let mut state = editor_with_cursor("- [x] |parent\n  - [x] child1\n  - [x] child2\n");
            state.toggle_checkbox(0);
            let text = state.text();
            assert!(text.contains("[ ] parent"), "parent should be unchecked");
            assert!(text.contains("[ ] child1"), "child1 should be unchecked");
            assert!(text.contains("[ ] child2"), "child2 should be unchecked");
        }

        #[test]
        fn check_all_siblings_checks_parent() {
            let mut state = editor_with_cursor("- [ ] parent\n  - [x] child1\n  - [ ] |child2\n");
            state.toggle_checkbox(2);
            let text = state.text();
            assert!(text.contains("[x] parent"), "parent should be auto-checked");
            assert!(text.contains("[x] child1"), "child1 should remain checked");
            assert!(text.contains("[x] child2"), "child2 should be checked");
        }

        #[test]
        fn uncheck_child_unchecks_parent() {
            let mut state = editor_with_cursor("- [x] parent\n  - [x] |child1\n  - [x] child2\n");
            state.toggle_checkbox(1);
            let text = state.text();
            assert!(text.contains("[ ] parent"), "parent should be unchecked");
            assert!(text.contains("[ ] child1"), "child1 should be unchecked");
            assert!(text.contains("[x] child2"), "child2 should remain checked");
        }

        #[test]
        fn checkbox_cascade_caches_match_fresh_parse() {
            // The suspend_caches batching must leave the derived inline styles identical
            // to a from-scratch parse of the resulting text (rebuilt once at the end).
            let mut state = editor_with_cursor("- [ ] |parent\n  - [ ] child1\n  - [ ] child2\n");
            state.toggle_checkbox(0); // cascades to children
            let mut fresh: Buffer = state.text().parse().unwrap();
            assert_eq!(
                *state.buffer.render_snapshot().inline_styles,
                *fresh.render_snapshot().inline_styles,
                "styles after the cascade equal a fresh parse"
            );
        }

        #[test]
        fn caches_not_frozen_after_cascade() {
            // suspend_caches must reset on every exit, so a later edit re-derives caches.
            let mut state = editor_with_cursor("- [ ] |task\nplain line\n");
            state.toggle_checkbox(0);
            let at = state.text().find("plain").unwrap();
            state.set_cursor(at);
            state.insert_text("**bold** ");
            let mut fresh: Buffer = state.text().parse().unwrap();
            assert_eq!(
                *state.buffer.render_snapshot().inline_styles,
                *fresh.render_snapshot().inline_styles,
                "caches re-derived after a post-cascade edit (not frozen)"
            );
        }

        #[test]
        fn deeply_nested_propagation_down() {
            let mut state = editor_with_cursor("- [ ] |level1\n  - [ ] level2\n    - [ ] level3\n");
            state.toggle_checkbox(0);
            let text = state.text();
            assert!(text.contains("[x] level1"), "level1 should be checked");
            assert!(text.contains("[x] level2"), "level2 should be checked");
            assert!(text.contains("[x] level3"), "level3 should be checked");
        }

        #[test]
        fn deeply_nested_propagation_up() {
            let mut state = editor_with_cursor("- [ ] level1\n  - [ ] level2\n    - [ ] |level3\n");
            state.toggle_checkbox(2);
            let text = state.text();
            assert!(text.contains("[x] level1"), "level1 should be auto-checked");
            assert!(text.contains("[x] level2"), "level2 should be auto-checked");
            assert!(text.contains("[x] level3"), "level3 should be checked");
        }

        #[test]
        fn mixed_siblings_parent_stays_unchecked() {
            let mut state = editor_with_cursor("- [ ] parent\n  - [ ] |child1\n  - [ ] child2\n");
            state.toggle_checkbox(1);
            let text = state.text();
            assert!(text.contains("[ ] parent"), "parent should stay unchecked");
            assert!(text.contains("[x] child1"), "child1 should be checked");
            assert!(text.contains("[ ] child2"), "child2 should stay unchecked");
        }
    }

    mod checkbox_undo_tests {
        use super::*;

        #[test]
        fn single_undo_reverts_cascade_to_children_and_parent() {
            // Checking child2 checks child2 AND auto-checks the parent — a cascade
            // spanning multiple lines. One undo must revert the entire toggle.
            let before = trim_raw("- [ ] parent\n  - [x] child1\n  - [ ] child2\n");
            let mut state = editor_with_cursor("- [ ] parent\n  - [x] child1\n  - [ ] |child2\n");
            state.toggle_checkbox(2);
            assert!(state.text().contains("[x] parent"), "parent auto-checked");
            assert!(state.text().contains("[x] child2"), "child2 checked");

            state.buffer.undo();
            assert_eq!(state.text(), before, "one undo reverts the whole cascade");
            assert!(!state.buffer.can_undo(), "toggle was a single undo entry");
        }

        #[test]
        fn toggle_leaves_the_task_text_alone() {
            // Only the box changes: the text (here with the user's own strikethrough,
            // and without one) is never rewritten.
            let mut state = editor_with_cursor("- [ ] |~~old~~ task\n- [x] done\n");
            state.toggle_checkbox(0);
            assert_eq!(state.text(), "- [x] ~~old~~ task\n- [x] done\n");
            state.toggle_checkbox(1);
            state.toggle_checkbox(0);
            assert_eq!(state.text(), "- [ ] ~~old~~ task\n- [ ] done\n");
        }

        #[test]
        fn redo_reapplies_full_cascade() {
            let mut state = editor_with_cursor("- [ ] parent\n  - [x] child1\n  - [ ] |child2\n");
            state.toggle_checkbox(2);
            let after = state.text();

            state.buffer.undo();
            state.buffer.redo();
            assert_eq!(state.text(), after, "one redo re-applies the whole cascade");
        }

        #[test]
        fn toggle_leaf_box_text_and_single_entry() {
            let mut state = editor_with_cursor("- [ ] |task\n");
            state.toggle_checkbox(0);
            assert_eq!(state.text(), "- [x] task\n");
            state.buffer.undo();
            assert_eq!(state.text(), "- [ ] task\n");
            assert!(!state.buffer.can_undo(), "leaf toggle is one undo entry");
        }

        #[test]
        fn toggle_parent_all_children_text() {
            let mut state = editor_with_cursor("- [ ] |parent\n  - [ ] child1\n  - [ ] child2\n");
            state.toggle_checkbox(0);
            assert_eq!(
                state.text(),
                "- [x] parent\n  - [x] child1\n  - [x] child2\n"
            );
        }

        #[test]
        fn uncheck_cascades_to_parent_text() {
            let mut state = editor_with_cursor("- [x] parent\n  - [x] |child1\n  - [x] child2\n");
            state.toggle_checkbox(1);
            assert_eq!(
                state.text(),
                "- [ ] parent\n  - [ ] child1\n  - [x] child2\n"
            );
        }
    }

    mod undo_tests {
        use super::*;

        #[test]
        fn typing_coalesces_into_word_undo_steps() {
            let mut state = EditorState::new("");
            for c in "hi there".chars() {
                state.insert_text(&c.to_string());
            }
            assert_eq!(state.text(), "hi there");
            // Word-granular undo: "there", then the space, then "hi".
            state.buffer.undo();
            assert_eq!(state.text(), "hi ", "undo removes the whole last word");
            state.buffer.undo();
            assert_eq!(state.text(), "hi", "undo removes the space");
            state.buffer.undo();
            assert_eq!(state.text(), "", "undo removes the first word");
        }

        #[test]
        fn backspace_coalesces_into_one_undo() {
            let mut state = EditorState::new("word\n");
            state.set_cursor(4); // end of "word"
            for _ in 0..4 {
                state.delete_backward();
            }
            assert_eq!(state.text(), "\n");
            state.buffer.undo();
            assert_eq!(
                state.text(),
                "word\n",
                "one undo restores the backspaced word"
            );
        }

        #[test]
        fn paste_is_its_own_undo_step() {
            let mut state = EditorState::new("");
            state.insert_text("ab"); // multi-char (paste-like) — not coalescable
            for c in "cd".chars() {
                state.insert_text(&c.to_string());
            }
            assert_eq!(state.text(), "abcd");
            state.buffer.undo();
            assert_eq!(state.text(), "ab", "typing after a paste undoes separately");
            state.buffer.undo();
            assert_eq!(state.text(), "", "the paste is its own step");
        }
    }
}

#[cfg(test)]
mod nested_context_tests {
    use super::*;

    #[test]
    fn nested_context_simple_list() {
        let state = EditorState::new("- item\n");
        let cursor_offset = 2; // on "item"
        let markers = state.build_nested_context(cursor_offset);
        assert_eq!(markers.len(), 1);
        assert!(matches!(
            markers[0],
            MarkerKind::ListItem { ordered: false, .. }
        ));
    }

    #[test]
    fn nested_context_nested_list() {
        let state = EditorState::new("- parent\n  - child\n");
        let cursor_offset = 14; // on "child"
        let markers = state.build_nested_context(cursor_offset);
        // Should show: - -
        assert_eq!(markers.len(), 2);
        assert!(matches!(
            markers[0],
            MarkerKind::ListItem { ordered: false, .. }
        ));
        assert!(matches!(
            markers[1],
            MarkerKind::ListItem { ordered: false, .. }
        ));
    }

    #[test]
    fn nested_context_checkbox_nested() {
        let state = EditorState::new("- [x] parent\n  - [ ] child\n");
        let cursor_offset = 20; // on "child"
        let markers = state.build_nested_context(cursor_offset);
        // Should show: - [x] - [ ]
        assert_eq!(markers.len(), 4);
        assert!(matches!(
            markers[0],
            MarkerKind::ListItem { ordered: false, .. }
        ));
        assert!(matches!(markers[1], MarkerKind::Checkbox { checked: true }));
        assert!(matches!(
            markers[2],
            MarkerKind::ListItem { ordered: false, .. }
        ));
        assert!(matches!(
            markers[3],
            MarkerKind::Checkbox { checked: false }
        ));
    }

    #[test]
    fn nested_context_blockquote_list() {
        let state = EditorState::new("> - item\n");
        let cursor_offset = 4; // on "item"
        let markers = state.build_nested_context(cursor_offset);
        // Should show: > -
        assert_eq!(markers.len(), 2);
        assert!(matches!(markers[0], MarkerKind::BlockQuote));
        assert!(matches!(
            markers[1],
            MarkerKind::ListItem { ordered: false, .. }
        ));
    }

    #[test]
    fn nested_context_ordered_list() {
        let state = EditorState::new("1. first\n2. second\n");
        let cursor_offset = 12; // on "second"
        let markers = state.build_nested_context(cursor_offset);
        assert_eq!(markers.len(), 1);
        assert!(matches!(
            markers[0],
            MarkerKind::ListItem { ordered: true, .. }
        ));
    }

    #[test]
    fn nested_context_empty_line() {
        let state = EditorState::new("hello\n");
        let cursor_offset = 2; // on "llo"
        let markers = state.build_nested_context(cursor_offset);
        assert_eq!(markers.len(), 0);
    }
}

#[cfg(test)]
mod debug_tree_structure {
    use super::*;

    #[test]
    fn check_blockquote_list_paragraph() {
        let state = EditorState::new("> - hey\n>   paragraph\n");

        if let Some(tree) = state.buffer.tree() {
            let root = tree.block_tree().root_node();
            eprintln!("Tree: {}", root.to_sexp());
        }
    }

    #[test]
    fn check_simple_list_paragraph() {
        let state = EditorState::new("- hey\n  paragraph\n");

        if let Some(tree) = state.buffer.tree() {
            let root = tree.block_tree().root_node();
            eprintln!("Tree: {}", root.to_sexp());
        }
    }
}

#[cfg(test)]
mod debug_tree_detail {
    use super::*;

    #[test]
    fn show_tree_detail() {
        let content = "> - hey\n>   paragraph\n";
        eprintln!("Content: {:?}", content);
        eprintln!("Bytes:");
        for (i, b) in content.bytes().enumerate() {
            eprintln!("  {}: {:?} ({})", i, b as char, b);
        }

        let state = EditorState::new(content);

        if let Some(tree) = state.buffer.tree() {
            let root = tree.block_tree().root_node();
            eprintln!("\nTree: {}", root.to_sexp());

            // Show each node with byte ranges
            fn print_node(node: tree_sitter::Node, indent: usize) {
                eprintln!(
                    "{}{} [{}-{}]",
                    "  ".repeat(indent),
                    node.kind(),
                    node.start_byte(),
                    node.end_byte()
                );
                for child in node.children(&mut node.walk()) {
                    print_node(child, indent + 1);
                }
            }
            print_node(root, 0);
        }
    }
}

use std::sync::Arc;

use crate::inline::StyledRegion;

use tree_sitter::{InputEdit, Language, Node, Parser, Point, Range, Tree, TreeCursor};
use tree_sitter_md::{INLINE_LANGUAGE, LANGUAGE};

pub struct MarkdownParser {
    parser: Parser,
    block_language: Language,
    inline_language: Language,
}

/// Byte ranges an inline leaf (a paragraph's or heading's content, a table cell) was
/// parsed over (its `set_included_ranges`). An inline parse depends only on these ranges
/// and the text inside them, so this is the key under which a leaf's styles can be reused
/// on the next parse.
type InlineKey = Vec<(usize, usize)>;

/// Styled regions extracted from one inline leaf, with the leaf's span start at
/// extraction time. Unchanged text gives the same regions shifted by (current span start
/// − `.0`): see [`MarkdownTree::inline_styles`].
pub type CachedInlineStyles = (usize, Arc<Vec<StyledRegion>>);

/// A document's block tree, plus the inline styles of each inline leaf.
///
/// The inline trees themselves are not kept: their styles are all the editor reads from
/// them, and tree-sitter trees are heavy (some 300 bytes per node, a node every few
/// bytes of text): keeping one per paragraph and cell doubled the memory a document takes.
#[derive(Debug, Clone)]
pub struct MarkdownTree {
    block_tree: Tree,
    /// The included byte ranges of each inline leaf, in document order, kept in
    /// current-text coordinates by `edit`, or `None` once an edit touched them (the
    /// styles are then stale and the leaf must be re-parsed, not reused).
    inline_keys: Vec<Option<InlineKey>>,
    /// Parallel to `inline_keys`: each leaf's styles.
    inline_styles: Vec<CachedInlineStyles>,
}

impl MarkdownTree {
    pub fn edit(&mut self, edit: &InputEdit) {
        self.block_tree.edit(edit);
        let delta = edit.new_end_byte as isize - edit.old_end_byte as isize;

        for key in self.inline_keys.iter_mut() {
            let Some(ranges) = key.as_mut() else { continue };
            let (Some(first), Some(last)) = (ranges.first(), ranges.last()) else {
                *key = None;
                continue;
            };
            let (span_start, span_end) = (first.0, last.1);
            if span_end < edit.start_byte {
                // Entirely before the edit: unchanged.
            } else if span_start > edit.old_end_byte {
                // Entirely after the edit: same text, shifted.
                for r in ranges.iter_mut() {
                    r.0 = (r.0 as isize + delta) as usize;
                    r.1 = (r.1 as isize + delta) as usize;
                }
            } else {
                // Touches the edit (boundaries included): content may have changed.
                *key = None;
            }
        }
    }

    pub fn block_tree(&self) -> &Tree {
        &self.block_tree
    }

    /// Each inline leaf's styles with its current span start (the first byte of its
    /// included ranges), in document order. Leaves stale since an edit are left out.
    pub fn inline_styles(&self) -> impl Iterator<Item = (usize, &CachedInlineStyles)> {
        self.inline_keys
            .iter()
            .zip(&self.inline_styles)
            .filter_map(|(key, styles)| Some((key.as_ref()?.first()?.0, styles)))
    }

    /// The included ranges of each inline leaf, in document order (`None`: stale).
    #[cfg(test)]
    fn inline_keys(&self) -> &[Option<InlineKey>] {
        &self.inline_keys
    }
}

/// Call `f` with the cursor on each inline leaf (a node of one of the `leaf` kinds: the
/// nodes whose text the inline grammar parses), in document order, from the cursor's
/// node down. Node kinds are compared by id: `Node::kind` measures and checks a C string.
fn visit_leaves(cursor: &mut TreeCursor, leaf: &[u16], f: &mut dyn FnMut(&mut TreeCursor)) {
    if leaf.contains(&cursor.node().kind_id()) {
        f(cursor);
        return;
    }
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        visit_leaves(cursor, leaf, f);
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    cursor.goto_parent();
}

/// The ranges the inline grammar parses for the leaf under the cursor: its text minus
/// its named children (block continuation markers such as a quote's `>` on its later
/// lines). The cursor is left on the leaf.
fn leaf_ranges(cursor: &mut TreeCursor, ranges: &mut Vec<Range>) {
    let node = cursor.node();
    let mut range = node.range();
    ranges.clear();
    // Most children are punctuation tokens: skip them unless a named one is there.
    if node.named_child_count() > 0 && cursor.goto_first_child() {
        while cursor.goto_next_sibling() {
            if !cursor.node().is_named() {
                continue;
            }
            let child_range = cursor.node().range();
            ranges.push(Range {
                start_byte: range.start_byte,
                start_point: range.start_point,
                end_byte: child_range.start_byte,
                end_point: child_range.start_point,
            });
            range.start_byte = child_range.end_byte;
            range.start_point = child_range.end_point;
        }
        cursor.goto_parent();
    }
    ranges.push(range);
}

impl Default for MarkdownParser {
    fn default() -> Self {
        let block_language = LANGUAGE.into();
        let inline_language = INLINE_LANGUAGE.into();
        let parser = Parser::new();
        MarkdownParser {
            parser,
            block_language,
            inline_language,
        }
    }
}

impl MarkdownParser {
    /// Parse the document whose text `callback` reads, reusing `old_tree` (edited to the
    /// new text): the block tree incrementally, and the styles of every inline leaf whose
    /// ranges are unchanged. A new or changed leaf is parsed and `styles_of` extracts its
    /// styles from the inline tree's root.
    pub fn parse_with<T: AsRef<[u8]>, F: FnMut(usize, Point) -> T>(
        &mut self,
        callback: &mut F,
        old_tree: Option<MarkdownTree>,
        styles_of: &mut dyn FnMut(Node) -> Vec<StyledRegion>,
    ) -> Option<MarkdownTree> {
        let MarkdownParser {
            parser,
            block_language,
            inline_language,
        } = self;
        let t_block = std::time::Instant::now();
        parser
            .set_included_ranges(&[])
            .expect("Can not set included ranges to whole document");
        parser
            .set_language(block_language)
            .expect("Could not load block grammar");
        let block_tree = parser.parse_with_options(
            callback,
            old_tree.as_ref().map(|tree| &tree.block_tree),
            None, // No progress callback
        )?;
        let t_inline = std::time::Instant::now();

        // Every leaf is looked for again: tree-sitter's `changed_ranges` misses some block
        // changes here (a table turning into a paragraph reports nothing), so the walk
        // can't be narrowed to where the text changed. Unchanged leaves reuse their styles.
        let (old_keys, old_styles) = match old_tree {
            Some(old) => (old.inline_keys, old.inline_styles),
            None => (Vec::new(), Vec::new()),
        };
        let mut inline_keys = Vec::with_capacity(old_keys.len());
        let mut inline_styles = Vec::with_capacity(old_keys.len());
        let mut reparsed = 0usize;
        let mut old_i = 0;
        let mut failed = false;
        parser
            .set_language(inline_language)
            .expect("Could not load inline grammar");
        let leaf_kinds = [
            block_language.id_for_node_kind("inline", true),
            block_language.id_for_node_kind("pipe_table_cell", true),
        ];
        {
            let mut cursor = block_tree.walk();
            let mut ranges: Vec<Range> = Vec::new();
            visit_leaves(&mut cursor, &leaf_kinds, &mut |cursor| {
                if failed {
                    return;
                }
                leaf_ranges(cursor, &mut ranges);
                let key: InlineKey = ranges.iter().map(|r| (r.start_byte, r.end_byte)).collect();
                // Skip the old leaves that start before this one (gone, or stale), then
                // reuse the next if it covers exactly the same ranges.
                let start = key[0].0;
                while old_i < old_keys.len()
                    && old_keys[old_i]
                        .as_ref()
                        .is_none_or(|k| k.first().is_none_or(|f| f.0 < start))
                {
                    old_i += 1;
                }
                let leaf_styles = match old_keys.get(old_i) {
                    Some(Some(k)) if *k == key => {
                        old_i += 1;
                        old_styles[old_i - 1].clone()
                    }
                    _ => {
                        reparsed += 1;
                        let tree = parser
                            .set_included_ranges(&ranges)
                            .ok()
                            .and_then(|()| parser.parse_with_options(callback, None, None));
                        let Some(tree) = tree else {
                            failed = true;
                            return;
                        };
                        (start, Arc::new(styles_of(tree.root_node())))
                    }
                };
                inline_styles.push(leaf_styles);
                inline_keys.push(Some(key));
            });
        }
        if failed {
            return None;
        }
        if crate::buffer::edit_profile_enabled() {
            eprintln!(
                "[markdone-edit]   block {:6.2} ms | inline {:6.2} ms ({} leaves, {reparsed} parsed)",
                (t_inline - t_block).as_secs_f64() * 1e3,
                t_inline.elapsed().as_secs_f64() * 1e3,
                inline_keys.len(),
            );
        }
        Some(MarkdownTree {
            block_tree,
            inline_keys,
            inline_styles,
        })
    }

    /// Parse `text` into its block tree alone (no inline leaves).
    pub fn parse_blocks(&mut self, text: &[u8]) -> Option<Tree> {
        self.parser
            .set_included_ranges(&[])
            .expect("Can not set included ranges to whole document");
        self.parser
            .set_language(&self.block_language)
            .expect("Could not load block grammar");
        self.parser.parse(text, None)
    }

    #[cfg(test)]
    pub fn parse(&mut self, text: &[u8], old_tree: Option<MarkdownTree>) -> Option<MarkdownTree> {
        let rope = ropey::Rope::from_str(std::str::from_utf8(text).unwrap());
        self.parse_with(&mut |byte, _| &text[byte..], old_tree, &mut |root| {
            crate::inline::inline_tree_styles(root, &rope)
        })
    }

    pub fn parse_rope(
        &mut self,
        rope: &ropey::Rope,
        old_tree: Option<MarkdownTree>,
    ) -> Option<MarkdownTree> {
        self.parse_with(
            &mut |byte, _| {
                let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
                &chunk.as_bytes()[byte - chunk_start..]
            },
            old_tree,
            &mut |root| crate::inline::inline_tree_styles(root, rope),
        )
    }
}

#[cfg(test)]
mod tests {
    use tree_sitter::{InputEdit, Point};

    use super::*;

    /// Full structural dump of a markdown tree: the block tree plus, for every inline
    /// tree in document order, its S-expression and every node's byte range. Two trees
    /// with equal dumps are indistinguishable to every consumer.
    fn dump(tree: &MarkdownTree) -> String {
        fn walk(node: tree_sitter::Node, out: &mut String) {
            out.push_str(&format!(
                "{}[{}..{}]",
                node.kind(),
                node.start_byte(),
                node.end_byte()
            ));
            let mut c = node.walk();
            for child in node.children(&mut c) {
                out.push('(');
                walk(child, out);
                out.push(')');
            }
        }
        let mut out = String::new();
        walk(tree.block_tree().root_node(), &mut out);
        for key in tree.inline_keys() {
            out.push_str(&format!("\n--inline: {key:?}"));
        }
        out
    }

    /// The inline leaves' ranges as the walk computed them before it was optimized: a
    /// plain recursive traversal of every node, and each leaf's ranges cut at its named
    /// children but the first (the original loop moved to the first child and then on to
    /// each next sibling, so the first child was never looked at).
    fn naive_keys(tree: &MarkdownTree) -> Vec<Option<InlineKey>> {
        fn walk(node: tree_sitter::Node, out: &mut Vec<Option<InlineKey>>) {
            if node.kind() == "inline" || node.kind() == "pipe_table_cell" {
                let mut key = Vec::new();
                let mut start = node.start_byte();
                let mut c = node.walk();
                for child in node.children(&mut c).skip(1).filter(|n| n.is_named()) {
                    key.push((start, child.start_byte()));
                    start = child.end_byte();
                }
                key.push((start, node.end_byte()));
                out.push(Some(key));
                return;
            }
            let mut c = node.walk();
            for child in node.children(&mut c) {
                walk(child, out);
            }
        }
        let mut out = Vec::new();
        walk(tree.block_tree().root_node(), &mut out);
        out
    }

    fn point_at(text: &str, byte: usize) -> Point {
        let before = &text[..byte];
        let row = before.matches('\n').count();
        let col = byte - before.rfind('\n').map_or(0, |i| i + 1);
        Point { row, column: col }
    }

    /// Incremental parsing with inline-tree reuse must produce exactly the tree a fresh
    /// parse produces, across random inserts/deletes — including ones landing on node
    /// boundaries and ones that change block structure (newlines, pipes, fences).
    #[test]
    fn incremental_inline_reuse_matches_fresh_parse() {
        let base = "# Titolo\n\nUn paragrafo con **grassetto** e `codice` e [link](x).\n\n\
| Col A | Col B |\n|---|---|\n| uno *x* | due |\n| tre | quattro `q` |\n\n\
> citazione con _enfasi_\n> seconda riga\n\n- voce **uno**\n- voce due\n  continua\n\n\
```rust\nfn main() {}\n```\n\nUltimo paragrafo\nsu due righe.\n";
        let mut text = std::env::var("MARKDONE_PARSE_TEST_FILE")
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_else(|| base.repeat(4));
        let mut parser = MarkdownParser::default();
        let mut tree = parser.parse(text.as_bytes(), None).unwrap();
        let inserts = [
            "a", " ", "\n", "|", "*", "`", "**b**", "\n\n", "# ", "> ", "x | y",
        ];
        let mut seed: u64 = 0x9e3779b97f4a7c15;
        let mut rnd = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for step in 0..400 {
            let len = text.len();
            let mut start = rnd(len + 1);
            while !text.is_char_boundary(start) {
                start -= 1;
            }
            let (old_end, insert) = if rnd(3) == 0 && start < len {
                let mut end = (start + 1 + rnd(8)).min(len);
                while !text.is_char_boundary(end) {
                    end += 1;
                }
                (end, "")
            } else {
                (start, inserts[rnd(inserts.len())])
            };
            let new_end = start + insert.len();
            let start_position = point_at(&text, start);
            let old_end_position = point_at(&text, old_end);
            text.replace_range(start..old_end, insert);
            let new_end_position = point_at(&text, new_end);
            tree.edit(&InputEdit {
                start_byte: start,
                old_end_byte: old_end,
                new_end_byte: new_end,
                start_position,
                old_end_position,
                new_end_position,
            });
            tree = parser.parse(text.as_bytes(), Some(tree)).unwrap();
            let fresh = MarkdownParser::default()
                .parse(text.as_bytes(), None)
                .unwrap();
            assert_eq!(
                tree.inline_keys(),
                naive_keys(&fresh).as_slice(),
                "step {step}: the inline leaves differ from a plain traversal"
            );
            assert_eq!(
                dump(&tree),
                dump(&fresh),
                "step {step}: edit {start}..{old_end} -> {insert:?}"
            );
            // Cached (shifted) inline styles must equal a from-scratch extraction.
            let rope = ropey::Rope::from_str(&text);
            assert_eq!(
                crate::inline::extract_all_inline_styles(&tree, &rope),
                crate::inline::extract_all_inline_styles(&fresh, &rope),
                "styles, step {step}: edit {start}..{old_end} -> {insert:?}"
            );
        }
    }

    #[test]
    fn inline_ranges() {
        // The paragraph's `[content]` is a shortcut link, before and after an edit above
        // it (its styles are then reused, shifted).
        let code = "# title\n\nInline [content].\n";
        let mut parser = MarkdownParser::default();
        let mut tree = parser.parse(code.as_bytes(), None).unwrap();
        let rope = ropey::Rope::from_str(code);
        let link = |tree: &MarkdownTree, rope: &ropey::Rope| {
            crate::inline::extract_all_inline_styles(tree, rope)
                .into_iter()
                .map(|r| r.full_range)
                .collect::<Vec<_>>()
        };
        assert_eq!(link(&tree, &rope), vec![16..25]);

        let code = "# Title!\n\nInline [content].\n";
        tree.edit(&InputEdit {
            start_byte: 7,
            old_end_byte: 7,
            new_end_byte: 8,
            start_position: Point { row: 0, column: 7 },
            old_end_position: Point { row: 0, column: 7 },
            new_end_position: Point { row: 0, column: 8 },
        });
        let tree = parser.parse(code.as_bytes(), Some(tree)).unwrap();
        let rope = ropey::Rope::from_str(code);
        assert_eq!(link(&tree, &rope), vec![17..26]);
    }

    #[test]
    fn changed_ranges_blockquote() {
        // Test: what does changed_ranges report when we remove a blockquote marker?
        let code1 = "> line one\n> line two\n> line three\n";
        let mut parser = MarkdownParser::default();
        let tree1 = parser.parse(code1.as_bytes(), None).unwrap();

        // Remove the `> ` from line one, making it a plain paragraph
        let code2 = "line one\n> line two\n> line three\n";
        let mut tree1_edited = parser.parse(code1.as_bytes(), None).unwrap();
        tree1_edited.edit(&InputEdit {
            start_byte: 0,
            old_end_byte: 2,
            new_end_byte: 0,
            start_position: Point { row: 0, column: 0 },
            old_end_position: Point { row: 0, column: 2 },
            new_end_position: Point { row: 0, column: 0 },
        });
        let tree2 = parser.parse(code2.as_bytes(), Some(tree1_edited)).unwrap();

        let ranges: Vec<_> = tree2
            .block_tree()
            .changed_ranges(tree1.block_tree())
            .collect();
        // Removing the first blockquote marker must report at least line 0 changed.
        assert!(!ranges.is_empty());
    }
}

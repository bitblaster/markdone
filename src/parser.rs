use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::inline::StyledRegion;

use tree_sitter::{InputEdit, Language, Node, Parser, Point, Range, Tree};
use tree_sitter_md::{INLINE_LANGUAGE, LANGUAGE};

pub struct MarkdownParser {
    parser: Parser,
    block_language: Language,
    inline_language: Language,
}

/// Byte ranges an inline tree was parsed over (its `set_included_ranges`). An inline
/// parse depends only on these ranges and the text inside them, so this is the key under
/// which an unchanged inline tree can be reused on the next parse.
type InlineKey = Vec<(usize, usize)>;

/// Styled regions extracted from one inline tree, with the tree's span start at
/// extraction time. A reused tree has identical text, so its regions are these shifted
/// by (current span start − `.0`): see [`MarkdownTree::inline_styles_cache`].
pub type CachedInlineStyles = (usize, Arc<Vec<StyledRegion>>);

#[derive(Debug, Clone)]
pub struct MarkdownTree {
    block_tree: Tree,
    inline_trees: Vec<Tree>,
    inline_indices: HashMap<usize, usize>,
    /// Parallel to `inline_trees`: the included byte ranges each one was parsed over,
    /// kept in current-text coordinates by `edit`, or `None` once an edit touched them
    /// (the tree's content is then stale and it must be re-parsed, not reused).
    inline_keys: Vec<Option<InlineKey>>,
    /// Parallel to `inline_trees`: lazily extracted styles, carried over with the tree
    /// when it is reused so unchanged paragraphs/cells are not re-walked on every edit.
    inline_styles: Vec<OnceLock<CachedInlineStyles>>,
}

impl MarkdownTree {
    pub fn edit(&mut self, edit: &InputEdit) {
        self.block_tree.edit(edit);
        for inline_tree in self.inline_trees.iter_mut() {
            inline_tree.edit(edit);
        }
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

    pub fn inline_tree(&self, parent: &Node) -> Option<&Tree> {
        let index = *self.inline_indices.get(&parent.id())?;
        Some(&self.inline_trees[index])
    }

    pub fn inline_trees(&self) -> &[Tree] {
        &self.inline_trees
    }

    /// For inline tree `i`: its style cache slot and current span start (the first byte
    /// of its included ranges), or `None` when the key is unknown (stale since an edit).
    pub fn inline_styles_cache(&self, i: usize) -> Option<(&OnceLock<CachedInlineStyles>, usize)> {
        let start = self.inline_keys.get(i)?.as_ref()?.first()?.0;
        Some((self.inline_styles.get(i)?, start))
    }
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
    pub fn parse_with<T: AsRef<[u8]>, F: FnMut(usize, Point) -> T>(
        &mut self,
        callback: &mut F,
        old_tree: Option<&MarkdownTree>,
    ) -> Option<MarkdownTree> {
        let MarkdownParser {
            parser,
            block_language,
            inline_language,
        } = self;
        parser
            .set_included_ranges(&[])
            .expect("Can not set included ranges to whole document");
        parser
            .set_language(block_language)
            .expect("Could not load block grammar");
        let block_tree = parser.parse_with_options(
            callback,
            old_tree.map(|tree| &tree.block_tree),
            None, // No progress callback
        )?;
        let len = old_tree.map_or(0, |t| t.inline_trees.len());
        let mut inline_trees = Vec::with_capacity(len);
        let mut inline_indices = HashMap::with_capacity(len);
        let mut inline_keys = Vec::with_capacity(len);
        let mut inline_styles = Vec::with_capacity(len);
        // Old inline trees untouched by the edits since the last parse, by their included
        // ranges. A new leaf with the exact same ranges has identical inline input, so its
        // tree is reused (`Tree::clone` is a cheap ref-counted copy) instead of re-parsed.
        // Without this every keystroke re-ran the inline grammar over every paragraph and
        // table cell of the document.
        let reusable: HashMap<&[(usize, usize)], usize> = old_tree
            .map(|old| {
                old.inline_keys
                    .iter()
                    .enumerate()
                    .filter_map(|(i, k)| k.as_deref().map(|k| (k, i)))
                    .collect()
            })
            .unwrap_or_default();
        let mut key: InlineKey = Vec::new();
        parser
            .set_language(inline_language)
            .expect("Could not load inline grammar");
        let mut tree_cursor = block_tree.walk();

        // The leaf nodes whose inner text gets a second (inline) parse pass.
        let is_leaf = |kind: &str| kind == "inline" || kind == "pipe_table_cell";

        // Reused across nodes: the interstitial ranges fed to `set_included_ranges`.
        let mut ranges: Vec<Range> = Vec::new();
        let mut i = 0;
        'outer: loop {
            let node = loop {
                let kind = tree_cursor.node().kind();
                if is_leaf(kind) || !tree_cursor.goto_first_child() {
                    while !tree_cursor.goto_next_sibling() {
                        if !tree_cursor.goto_parent() {
                            break 'outer;
                        }
                    }
                }
                if is_leaf(tree_cursor.node().kind()) {
                    break tree_cursor.node();
                }
            };
            // Carve the node's range into the gaps BETWEEN its named children: each pushed
            // range runs from the current cursor to the next child's start, then advances
            // past that child. This excludes the children's own spans (which get their own
            // injection) so the block highlighter only re-parses the interstitial text.
            let mut range = node.range();
            ranges.clear();
            if tree_cursor.goto_first_child() {
                while tree_cursor.goto_next_sibling() {
                    if !tree_cursor.node().is_named() {
                        continue;
                    }
                    let child_range = tree_cursor.node().range();
                    ranges.push(Range {
                        start_byte: range.start_byte,
                        start_point: range.start_point,
                        end_byte: child_range.start_byte,
                        end_point: child_range.start_point,
                    });
                    range.start_byte = child_range.end_byte;
                    range.start_point = child_range.end_point;
                }
                tree_cursor.goto_parent();
            }
            ranges.push(range);
            key.clear();
            key.extend(ranges.iter().map(|r| (r.start_byte, r.end_byte)));
            let (inline_tree, styles) = match (reusable.get(key.as_slice()), old_tree) {
                (Some(&j), Some(old)) => (old.inline_trees[j].clone(), old.inline_styles[j].clone()),
                _ => {
                    parser.set_included_ranges(&ranges).ok()?;
                    let tree = parser.parse_with_options(
                        callback,
                        None,
                        None, // No progress callback
                    )?;
                    (tree, OnceLock::new())
                }
            };
            inline_trees.push(inline_tree);
            inline_styles.push(styles);
            inline_keys.push(Some(key.clone()));
            inline_indices.insert(node.id(), i);
            i += 1;
        }
        drop(tree_cursor);
        inline_trees.shrink_to_fit();
        inline_indices.shrink_to_fit();
        inline_keys.shrink_to_fit();
        inline_styles.shrink_to_fit();
        Some(MarkdownTree {
            block_tree,
            inline_trees,
            inline_indices,
            inline_keys,
            inline_styles,
        })
    }

    #[cfg(test)]
    pub fn parse(&mut self, text: &[u8], old_tree: Option<&MarkdownTree>) -> Option<MarkdownTree> {
        self.parse_with(&mut |byte, _| &text[byte..], old_tree)
    }

    pub fn parse_rope(
        &mut self,
        rope: &ropey::Rope,
        old_tree: Option<&MarkdownTree>,
    ) -> Option<MarkdownTree> {
        self.parse_with(
            &mut |byte, _| {
                let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
                &chunk.as_bytes()[byte - chunk_start..]
            },
            old_tree,
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
            out.push_str(&format!("{}[{}..{}]", node.kind(), node.start_byte(), node.end_byte()));
            let mut c = node.walk();
            for child in node.children(&mut c) {
                out.push('(');
                walk(child, out);
                out.push(')');
            }
        }
        let mut out = String::new();
        walk(tree.block_tree().root_node(), &mut out);
        for t in tree.inline_trees() {
            out.push_str("\n--inline: ");
            walk(t.root_node(), &mut out);
        }
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
        let mut text = std::env::var("WRIT_PARSE_TEST_FILE")
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_else(|| base.repeat(4));
        let mut parser = MarkdownParser::default();
        let mut tree = parser.parse(text.as_bytes(), None).unwrap();
        let inserts = ["a", " ", "\n", "|", "*", "`", "**b**", "\n\n", "# ", "> ", "x | y"];
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
            tree = parser.parse(text.as_bytes(), Some(&tree)).unwrap();
            let fresh = MarkdownParser::default().parse(text.as_bytes(), None).unwrap();
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
        let code = "# title\n\nInline [content].\n";
        let mut parser = MarkdownParser::default();
        let mut tree = parser.parse(code.as_bytes(), None).unwrap();

        let section = tree.block_tree().root_node().child(0).unwrap();
        assert_eq!(section.kind(), "section");
        let heading = section.child(0).unwrap();
        assert_eq!(heading.kind(), "atx_heading");
        let paragraph = section.child(1).unwrap();
        assert_eq!(paragraph.kind(), "paragraph");
        let inline = paragraph.child(0).unwrap();
        assert_eq!(inline.kind(), "inline");
        assert_eq!(
            tree.inline_tree(&inline)
                .unwrap()
                .root_node()
                .child(0)
                .unwrap()
                .kind(),
            "shortcut_link"
        );

        let code = "# Title\n\nInline [content].\n";
        tree.edit(&InputEdit {
            start_byte: 2,
            old_end_byte: 3,
            new_end_byte: 3,
            start_position: Point { row: 0, column: 2 },
            old_end_position: Point { row: 0, column: 3 },
            new_end_position: Point { row: 0, column: 3 },
        });
        let tree = parser.parse(code.as_bytes(), Some(&tree)).unwrap();

        let section = tree.block_tree().root_node().child(0).unwrap();
        assert_eq!(section.kind(), "section");
        let heading = section.child(0).unwrap();
        assert_eq!(heading.kind(), "atx_heading");
        let paragraph = section.child(1).unwrap();
        assert_eq!(paragraph.kind(), "paragraph");
        let inline = paragraph.child(0).unwrap();
        assert_eq!(inline.kind(), "inline");
        assert_eq!(
            tree.inline_tree(&inline)
                .unwrap()
                .root_node()
                .named_child(0)
                .unwrap()
                .kind(),
            "shortcut_link"
        );
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
        let tree2 = parser.parse(code2.as_bytes(), Some(&tree1_edited)).unwrap();

        let ranges: Vec<_> = tree2
            .block_tree()
            .changed_ranges(tree1.block_tree())
            .collect();
        // Removing the first blockquote marker must report at least line 0 changed.
        assert!(!ranges.is_empty());
    }
}

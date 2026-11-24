//! Headless memory probe: open a file and report the heap (glibc `mallinfo2`, which sees
//! both Rust's and tree-sitter's C allocations) taken by each structure the editor keeps
//! for it, per byte of Markdown. Linux only.

#[cfg(all(unix, not(target_os = "macos")))]
fn main() {
    probe::run();
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn main() {
    eprintln!("memprobe needs glibc's mallinfo2 (Linux)");
}

#[cfg(all(unix, not(target_os = "macos")))]
mod probe {
    use markdone::inline::{StyledRegion, extract_all_inline_styles};
    use markdone::marker::{NodeInfo, collect_node_infos};
    use markdone::parser::MarkdownParser;
    use ropey::Rope;

    fn heap() -> usize {
        // SAFETY: plain glibc statistics call.
        let i = unsafe { libc::mallinfo2() };
        i.uordblks + i.hblkhd
    }

    struct Meter {
        last: usize,
        bytes: f64,
    }

    impl Meter {
        fn step(&mut self, label: &str) {
            let now = heap();
            let d = now as f64 - self.last as f64;
            println!(
                "{label:<34} {:>9.1} KB  {:>6.1} B/byte",
                d / 1024.0,
                d / self.bytes
            );
            self.last = now;
        }
    }

    pub fn run() {
        let path = std::env::args().nth(1).expect("usage: memprobe <file.md>");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut m = Meter {
            last: heap(),
            bytes: text.len() as f64,
        };
        println!("{} bytes of Markdown", text.len());
        let rope = Rope::from_str(&text);
        m.step("rope");

        let mut block = tree_sitter::Parser::new();
        block
            .set_language(&tree_sitter_md::LANGUAGE.into())
            .unwrap();
        let block_only = block.parse(text.as_bytes(), None).unwrap();
        m.step("block tree + parser");
        drop(block);
        m.step("  parser dropped");
        let block_nodes = block_only.root_node().descendant_count();
        drop(block_only);
        m.step("  tree dropped");

        // One small paragraph parsed as an inline tree, many times (kept alive).
        let mut inl = tree_sitter::Parser::new();
        inl.set_language(&tree_sitter_md::INLINE_LANGUAGE.into())
            .unwrap();
        let para = b"Some *emphasis* and `code` and a [link](x) here.";
        let _ = inl.parse(para, None);
        m.step("inline parser warm");
        let trees: Vec<_> = (0..1000).map(|_| inl.parse(para, None).unwrap()).collect();
        let d = heap() as f64 - m.last as f64;
        println!(
            "1000 small inline trees: {:.0} B each ({} nodes each)",
            d / 1000.0,
            trees[0].root_node().descendant_count()
        );
        drop(trees);
        drop(inl);
        m.last = heap();

        let mut parser = MarkdownParser::default();
        let tree = parser.parse_rope(&rope, None).unwrap();
        m.step("block tree + inline styles");

        let nodes = collect_node_infos(&tree.block_tree().root_node(), &rope);
        m.step("ParsedNodes (collect_node_infos)");

        let styles = extract_all_inline_styles(&tree, &rope);
        m.step("inline styles (flattened)");

        println!(
            "block nodes {block_nodes}, NodeInfo {} x {} B; inline leaves {}; StyledRegion {} x {} B",
            nodes.nodes.len(),
            std::mem::size_of::<NodeInfo>(),
            tree.inline_styles().count(),
            styles.len(),
            std::mem::size_of::<StyledRegion>(),
        );
        drop((styles, nodes, tree, parser, rope));
        m.last = heap();
        let editor = markdone::core::Editor::open(std::path::Path::new(&path));
        m.step("Editor::open total");
        drop(editor);
    }
}

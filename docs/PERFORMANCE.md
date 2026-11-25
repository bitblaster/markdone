# Performance notes

How Markdone's editing speed and memory are measured, what has been done, and the
improvements measured but not made yet, with how to make them.

## Measuring

Use a large document. Twenty copies of the README make one of about 400 KB and
8,000 lines, with 100 fenced code blocks:

```bash
for i in $(seq 1 20); do cat README.md; echo; done > /tmp/big.md
```

- **`examples/typebench`** opens a file, puts the caret mid-document and types N
  characters, timing each keystroke as the app does it without a window: the editor core
  (`type_char`), the link and math scan of the lines in view (`refresh_detection`), and
  the document layout (`DocLayout::build`).

  ```bash
  cargo build --profile release-fast --example typebench
  target/release-fast/examples/typebench /tmp/big.md 40
  ```

- **`MARKDONE_PROFILE=1`** also prints each phase of an edit (`[markdone-edit]`): the
  block parse and the inline-leaf walk, `collect_node_infos`, code highlighting, link and
  math detection. In the app it also logs frame and rebuild times.
- **`examples/memprobe`** reports the memory a document takes.
- **`perf`** needs symbols, which the `release-fast` profile strips. Building with them
  recompiles every dependency, about 12 minutes:

  ```bash
  CARGO_PROFILE_RELEASE_FAST_STRIP=false CARGO_PROFILE_RELEASE_FAST_DEBUG=1 \
    cargo build --profile release-fast --example typebench
  perf record -F 2000 -g -o /tmp/perf.data target/release-fast/examples/typebench /tmp/big.md 80
  perf report -i /tmp/perf.data --no-children --percent-limit 1 --stdio -g none
  ```

  Call stacks don't unwind through tree-sitter's C code, so read `--children` totals for
  Markdone's functions, or use the phase timings above.

Timings vary by about 10% between runs: compare the means of a few runs of 30 keys or
more, and measure the code before a change the same way (`git stash`, rebuild, run).

## Where a keystroke's time goes

On the 400 KB document, after the work below (November 2026), about 16 ms:

| Phase | ms | Where |
|---|---|---|
| Block parse, incremental | ~2 | `MarkdownParser::parse_with` (tree-sitter) |
| Inline-leaf walk | ~4 | `parse_with`: `visit_leaves`, `leaf_ranges` |
| Marker walk | ~5 | `collect_node_infos_with`, from `BufferContent::update_caches` |
| Link and math scan of the lines in view | ~2.7 | `Editor::refresh_detection` |
| Layout | ~2.7 | `DocLayout::build` |

In the app each key also redraws, about 27 ms per key in all before this work.

## Done

- **Half the memory per document** (`7cb56b4`): inline trees aren't kept, only their
  styles, reused while a leaf's ranges are unchanged; the block-tree walks skip the
  punctuation tokens inside inline leaves; grammars compile once per process, on first
  use. 50 → 24 MB, opening 730 → 410 ms.
- **A line table** (`a27b912`): every line's byte range, computed once per text version
  and shared by the render snapshot. Layout per keystroke 60.7 → 4.4 ms.
- **Per-version memo in the render snapshot** (`RenderSnapshot::memo`): the `$$` math
  blocks and the per-line bucketing of inline styles are computed on first use and
  shared by every snapshot of the same version. The layout and the link scan each asked
  on every keystroke, about 0.8 ms each.
- **The line table scans bytes** (`line_ranges`) instead of ropey's `lines()` iterator,
  which looks every line up from the root: ~1.5 → ~0.3 ms. It recognizes every line
  break ropey does (LF, CR LF, CR, VT, FF, NEL, LS, PS); `line_ranges_match_per_line_lookups`
  checks it against per-line lookups.
- **Code highlighting is cached per block** (`CodeHighlightCache::by_content`): spans
  relative to the block, keyed by a hash of its language and content, so an edit
  re-highlights only the blocks it changed. Identical blocks share one entry. 1.6 →
  0.16 ms.

- **Memory goes back to the system after a document is opened or closed** (Linux):
  glibc keeps freed memory to reuse it, and gives back on its own only the top of its
  heap, not the holes a closed document leaves among live data. Closing a 400 KB
  document freed 25 MB but returned 1 MB. `release_free_memory` (`src/shell.rs`) calls
  `malloc_trim` after the next frame (0.4-2 ms); the process then measures as after a
  fresh start. `MARKDONE_PROFILE=1` logs each trim and `MARKDONE_DROPTEST=1` measures
  closing every document but the first.

`random_edits_keep_text_and_state_exact` (in `buffer.rs`) guards all of this: after
each of 1,500 random edits, the text and everything derived from it (markers, styles,
line ranges, code highlights) must equal a freshly opened document's. Run it after any
change to the incremental machinery, and check that it catches a bug seeded on purpose
in the code changed.

## Not done yet

### Merging the two walks of the block tree (saves ~2-4 ms a key)

Each keystroke walks the whole block tree twice:

1. `MarkdownParser::parse_with` (`src/parser.rs`) walks it with `visit_leaves` to find
   the inline leaves (`inline`, `pipe_table_cell`), computes each one's ranges with
   `leaf_ranges` and reuses or reparses its styles.
2. `BufferContent::update_caches` (`src/buffer.rs`) then walks it again with
   `collect_node_infos` (`src/marker.rs`) for the markers, code blocks, tables,
   headings, list items and callouts.

Both descend the same structural nodes, and both iterate the children of inline leaves
that have named children (the block continuations, such as a quote's `>` on a
paragraph's later lines). `ts_tree_cursor_goto_first_child_internal` and
`goto_sibling_internal` are the top two symbols in `perf`, about 22% of the time.

How: one cursor walk with two visitors. Do the walk in `parse_with` right after the block
parse, and have it also collect the `NodeInfo`s and the per-kind lists
`collect_node_infos_with` builds. Return them with the `MarkdownTree`, so
`update_caches` reuses them instead of walking again. For a leaf, read its children once
and feed both its ranges and its continuation markers from that one pass.
`collect_node_infos_with` keeps a stack of ancestor kinds and some per-kind state (the
open checked task, the current fenced block): carry it into the merged walk as is.

Also: `collect_node_infos_with` converts byte offsets to lines through the rope
(`char_to_line(byte_to_char(...))`) for each list item, heading and quote. The line
table, if it is computed before the walk, answers that with a binary search.

Guard: `skipping_leaf_punctuation_changes_no_marker` and the random-edit test. Don't
narrow the walk to tree-sitter's `changed_ranges`: on tree-sitter-md it misses some
changes (a table turning into a paragraph reports none).

### Coalescing rebuilds in a burst of keys (saves ~5.5 ms per extra key)

Every key event edits the buffer and then rebuilds at once: `after_edit` in
`src/shell.rs` calls `DocEngine::refresh`, which runs `refresh_detection` and
`DocLayout::build`. Keys that arrive within one frame (fast typing, key repeat, a paste
split into events) each pay that rebuild (~5.5 ms). Only the last one is ever drawn. A
burst of 20 characters costs about 320 ms; coalesced, about 210.

How: for printable text only (the `event.text` branch of the key handler and IME
commits), apply the edit and mark the layout dirty instead of rebuilding. Rebuild once
in `WindowEvent::RedrawRequested`, before drawing. Every other event (arrows, clicks,
commands, scrolling, resize) must first flush a pending rebuild, since it may read the
layout (caret moves by visual line, hit testing, scroll anchoring). The safe way is a
check at the top of `window_event`, before its dispatch, so nothing reads a stale layout
whatever its path.

The editor core (parse and markers, ~10 ms) still runs per key, since each command reads
the markers to compute its edit. Batching that would need lazily recomputed caches,
riskier for the document's integrity.

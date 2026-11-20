//! Export a document to PDF: Markdown → a self-contained HTML page with a print style
//! sheet → a headless Chromium-family browser prints it (`--print-to-pdf`).
//!
//! What the editor renders specially is carried over: syntax-highlighted code (the
//! editor's tree-sitter highlighter, light colors), Mermaid diagrams (inline SVG), LaTeX
//! math (PNG, embedded), Obsidian-style callouts, task lists, tables, footnotes, and
//! heading ids matching the editor's `#anchor` links. Local images are referenced by
//! absolute `file://` URLs; remote ones are fetched by the browser.
//!
//! Runs blocking: call it on a worker thread.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd, TextMergeStream};

use crate::callout::{CalloutKind, parse_callout_header};
use crate::editor::EditorTheme;
use crate::highlight::Highlighter;
use crate::links::heading_slug;

/// How long the browser may take to print before it is stopped.
const BROWSER_TIMEOUT: Duration = Duration::from_secs(120);
/// Math is typeset at this size (CSS px; the body text is 11pt ≈ 14.7px).
const MATH_PX: f32 = 15.0;

/// What to export.
pub struct ExportJob {
    /// The document text (the editor buffer, unsaved changes included).
    pub markdown: String,
    /// The document's folder, for relative image paths (`None` for an untitled one).
    pub base_dir: Option<PathBuf>,
    /// The document title (the HTML `<title>`, i.e. the PDF metadata title).
    pub title: String,
    pub output: PathBuf,
}

/// Render the document and print it to `job.output`. The error is a sentence for the
/// user.
pub fn export_pdf(job: &ExportJob) -> Result<(), String> {
    let browser = find_browser().ok_or(
        "No Chromium-based browser found (Chromium, Google Chrome, Brave or Microsoft \
         Edge). Install one to export PDF files.",
    )?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let work = std::env::temp_dir().join(format!("markdone-export-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| format!("Cannot create a temporary folder: {e}"))?;
    let result = (|| {
        let html = markdown_to_html(&job.markdown, job.base_dir.as_deref(), &job.title);
        let page = work.join("document.html");
        std::fs::write(&page, html).map_err(|e| format!("Cannot write the page: {e}"))?;
        print_to_pdf(&browser, &page, &job.output, &work)
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// Chromium-family browsers, in order of preference.
fn find_browser() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let roots = ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"]
            .iter()
            .filter_map(|v| std::env::var_os(v).map(PathBuf::from));
        let apps = [
            "Google\\Chrome\\Application\\chrome.exe",
            "Chromium\\Application\\chrome.exe",
            "Microsoft\\Edge\\Application\\msedge.exe",
            "BraveSoftware\\Brave-Browser\\Application\\brave.exe",
        ];
        for root in roots {
            for app in apps {
                let path = root.join(app);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        let names = [
            "chromium",
            "chromium-browser",
            "google-chrome",
            "google-chrome-stable",
            "brave-browser",
            "brave",
            "microsoft-edge",
            "microsoft-edge-stable",
        ];
        let dirs: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH")?).collect();
        names
            .iter()
            .flat_map(|name| dirs.iter().map(move |d| d.join(name)))
            .find(|p| p.is_file())
    }
}

/// Run the browser headless on `page`, printing to `output`. A throwaway profile keeps
/// it apart from a running browser session (which would otherwise take the command).
fn print_to_pdf(browser: &Path, page: &Path, output: &Path, work: &Path) -> Result<(), String> {
    let _ = std::fs::remove_file(output);
    let mut child = Command::new(browser)
        .arg("--headless")
        .arg("--disable-gpu")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-extensions")
        .arg("--hide-scrollbars")
        .arg("--no-pdf-header-footer")
        .arg("--print-to-pdf-no-header")
        // Wait for images (local and remote) before printing.
        .arg("--virtual-time-budget=15000")
        .arg(format!(
            "--user-data-dir={}",
            work.join("profile").display()
        ))
        .arg(format!("--print-to-pdf={}", output.display()))
        .arg(crate::platform::file_url(page))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Cannot start {}: {e}", browser.display()))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > BROWSER_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("The browser took too long to print the document.".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("Waiting for the browser failed: {e}")),
        }
    };
    let printed = std::fs::metadata(output).is_ok_and(|m| m.len() > 0);
    if status.success() && printed {
        return Ok(());
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
    }
    let detail = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    Err(if detail.is_empty() {
        format!(
            "The browser could not write {} ({status}).",
            output.display()
        )
    } else {
        format!("The browser could not write {}: {detail}", output.display())
    })
}

/// The complete HTML page for a document.
pub fn markdown_to_html(markdown: &str, base_dir: Option<&Path>, title: &str) -> String {
    let body = render_body(markdown, base_dir);
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\">\n<title>{}</title>\n\
         <style>{STYLE}</style>\n</head><body>\n{body}\n</body></html>\n",
        escape(title)
    )
}

/// Rich clipboard content (HTML from a browser or an office suite) as Markdown in the
/// editor's own style: `#` headings, `-` bullets, fenced code. `None` if it converts to
/// nothing.
pub fn html_to_markdown(html: &str) -> Option<String> {
    use htmd::options::{BulletListMarker, CodeBlockStyle, HeadingStyle, Options};
    let converter = htmd::HtmlToMarkdown::builder()
        .options(Options {
            heading_style: HeadingStyle::Atx,
            code_block_style: CodeBlockStyle::Fenced,
            bullet_list_marker: BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            ol_number_spacing: 1,
            ..Default::default()
        })
        .build();
    converter
        .convert(html)
        .ok()
        .filter(|md| !md.trim().is_empty())
}

/// The HTML of a Markdown fragment (no page around it), e.g. for copying as HTML.
pub fn markdown_fragment(markdown: &str, base_dir: Option<&Path>) -> String {
    render_body(markdown, base_dir)
}

fn render_body(markdown: &str, base_dir: Option<&Path>) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH;
    let mut input = TextMergeStream::new(Parser::new_ext(markdown, options)).peekable();
    let mut out: Vec<Event> = Vec::new();
    let mut highlighter = Highlighter::new();
    let code_theme = EditorTheme::catppuccin_latte();
    let mut slugs: std::collections::HashMap<String, usize> = Default::default();
    // One entry per open blockquote: whether it is a callout (closed with two divs).
    let mut quotes: Vec<bool> = Vec::new();

    while let Some(event) = input.next() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let mut inner = Vec::new();
                for e in input.by_ref() {
                    if matches!(e, Event::End(TagEnd::Heading(_))) {
                        break;
                    }
                    inner.push(e);
                }
                let text: String = inner
                    .iter()
                    .filter_map(|e| match e {
                        Event::Text(t) | Event::Code(t) => Some(t.as_ref()),
                        _ => None,
                    })
                    .collect();
                // GitHub ids, de-duplicated the same way (`-1`, `-2`, …).
                let base = heading_slug(&text);
                let n = slugs.entry(base.clone()).or_insert(0);
                let id = if *n == 0 { base } else { format!("{base}-{n}") };
                *n += 1;
                out.push(Event::Html(
                    format!("<{level} id=\"{}\">", escape(&id)).into(),
                ));
                out.extend(inner.into_iter().map(|e| rewrite_image(e, base_dir)));
                out.push(Event::Html(format!("</{level}>\n").into()));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let mut code = String::new();
                for e in input.by_ref() {
                    match e {
                        Event::End(TagEnd::CodeBlock) => break,
                        Event::Text(t) => code.push_str(&t),
                        _ => {}
                    }
                }
                let lang = match &kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or(""),
                    CodeBlockKind::Indented => "",
                };
                let html = if lang.eq_ignore_ascii_case("mermaid") {
                    mermaid_html(&code)
                } else {
                    code_html(&mut highlighter, &code_theme, &code, lang)
                };
                out.push(Event::Html(html.into()));
            }
            Event::InlineMath(latex) => {
                out.push(Event::InlineHtml(math_html(&latex, false).into()))
            }
            Event::DisplayMath(latex) => out.push(Event::Html(math_html(&latex, true).into())),
            Event::Start(Tag::BlockQuote(kind)) => {
                // Obsidian callout: `> [!type] Title` on the quote's first line.
                let header = match (input.peek(), kind.is_none()) {
                    (Some(Event::Start(Tag::Paragraph)), true) => {
                        let _paragraph = input.next();
                        match input.peek() {
                            Some(Event::Text(t)) => {
                                parse_callout_header(&format!("> {t}")).map(|h| (h, true))
                            }
                            _ => None,
                        }
                        .or(Some((
                            (CalloutKind::Note, String::new(), false, false),
                            false,
                        )))
                    }
                    _ => None,
                };
                match header {
                    Some(((callout, title, _, _), true)) => {
                        let _header_text = input.next();
                        quotes.push(true);
                        out.push(Event::Html(
                            format!(
                                "<div class=\"callout callout-{}\"><div class=\"callout-title\">\
                                 <span class=\"callout-icon\">{}</span>{}</div>\
                                 <div class=\"callout-body\">\n",
                                callout_class(callout),
                                callout.icon(),
                                escape(&title)
                            )
                            .into(),
                        ));
                        // The rest of the first paragraph, if any, opens the body.
                        if matches!(input.peek(), Some(Event::SoftBreak | Event::HardBreak)) {
                            input.next();
                        }
                        if matches!(input.peek(), Some(Event::End(TagEnd::Paragraph))) {
                            input.next();
                        } else {
                            out.push(Event::Start(Tag::Paragraph));
                        }
                    }
                    Some((_, false)) => {
                        // A plain quote whose paragraph we already took.
                        quotes.push(false);
                        out.push(Event::Start(Tag::BlockQuote(None)));
                        out.push(Event::Start(Tag::Paragraph));
                    }
                    None => {
                        quotes.push(false);
                        out.push(Event::Start(Tag::BlockQuote(kind)));
                    }
                }
            }
            Event::End(TagEnd::BlockQuote(kind)) => {
                if quotes.pop() == Some(true) {
                    out.push(Event::Html("</div></div>\n".into()));
                } else {
                    out.push(Event::End(TagEnd::BlockQuote(kind)));
                }
            }
            other => out.push(rewrite_image(other, base_dir)),
        }
    }
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, out.into_iter());
    html
}

/// Point a relative image at its absolute `file://` location.
fn rewrite_image<'a>(event: Event<'a>, base_dir: Option<&Path>) -> Event<'a> {
    let Event::Start(Tag::Image {
        link_type,
        dest_url,
        title,
        id,
    }) = event
    else {
        return event;
    };
    let url = dest_url.as_ref();
    let remote = url.contains("://") || url.starts_with("data:") || url.starts_with('#');
    let local = if remote {
        None
    } else {
        let path = PathBuf::from(crate::links::percent_decode(url));
        if path.is_absolute() {
            Some(path)
        } else {
            base_dir.map(|dir| dir.join(path))
        }
    };
    let dest_url = match local {
        Some(path) => CowStr::from(crate::platform::file_url(&path)),
        None => dest_url,
    };
    Event::Start(Tag::Image {
        link_type,
        dest_url,
        title,
        id,
    })
}

fn mermaid_html(source: &str) -> String {
    match crate::mermaid::render_svg_for_print(source) {
        Ok(svg) => format!("<figure class=\"diagram\">{svg}</figure>\n"),
        Err(e) => format!(
            "<pre class=\"render-error\">Mermaid: {}</pre>\n<pre><code>{}</code></pre>\n",
            escape(&e),
            escape(source)
        ),
    }
}

fn math_html(latex: &str, display: bool) -> String {
    match crate::math::render_png_for_print(latex, display, MATH_PX, 3.0) {
        Ok(m) => {
            let img = format!(
                "<img class=\"math\" alt=\"{}\" src=\"data:image/png;base64,{}\" \
                 style=\"width:{:.2}px;height:{:.2}px;vertical-align:-{:.2}px\">",
                escape(latex),
                base64(&m.png),
                m.width,
                m.height,
                m.depth
            );
            if display {
                format!("<div class=\"math-display\">{img}</div>\n")
            } else {
                img
            }
        }
        Err(e) => format!(
            "<code class=\"render-error\" title=\"{}\">{}</code>",
            escape(&e),
            escape(latex)
        ),
    }
}

/// A code block with the editor's syntax colors (a light theme for paper).
fn code_html(highlighter: &mut Highlighter, theme: &EditorTheme, code: &str, lang: &str) -> String {
    let mut html = String::from("<pre><code>");
    let spans = if lang.is_empty() {
        Vec::new()
    } else {
        highlighter.highlight(code, lang)
    };
    let mut pos = 0;
    for span in spans {
        if span.range.start < pos || span.range.end > code.len() {
            continue;
        }
        html.push_str(&escape(&code[pos..span.range.start]));
        let c = theme.color_for_highlight(span.highlight_id).to_rgba8();
        let _ = write!(
            html,
            "<span style=\"color:#{:02x}{:02x}{:02x}\">{}</span>",
            c.r,
            c.g,
            c.b,
            escape(&code[span.range.clone()])
        );
        pos = span.range.end;
    }
    html.push_str(&escape(&code[pos..]));
    html.push_str("</code></pre>\n");
    html
}

fn callout_class(kind: CalloutKind) -> &'static str {
    match kind {
        CalloutKind::Note | CalloutKind::Info | CalloutKind::Todo => "info",
        CalloutKind::Abstract | CalloutKind::Tip | CalloutKind::Success => "success",
        CalloutKind::Question => "question",
        CalloutKind::Warning => "warning",
        CalloutKind::Failure | CalloutKind::Danger | CalloutKind::Bug => "danger",
        CalloutKind::Example => "example",
        CalloutKind::Quote => "quote",
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Print style sheet: A4, readable serif-free body, light code blocks, tables and
/// callouts that do not split across pages where avoidable.
const STYLE: &str = r#"
@page { size: A4; margin: 18mm 16mm 20mm 16mm; }
html { -webkit-print-color-adjust: exact; print-color-adjust: exact; }
body { font-family: "Noto Sans", "Segoe UI", "Helvetica Neue", Arial, sans-serif;
  font-size: 11pt; line-height: 1.5; color: #1f2328; margin: 0; }
h1, h2, h3, h4, h5, h6 { line-height: 1.25; margin: 1.4em 0 0.5em; break-after: avoid; }
h1 { font-size: 1.9em; border-bottom: 1px solid #d0d7de; padding-bottom: 0.25em; }
h2 { font-size: 1.45em; border-bottom: 1px solid #e5e8eb; padding-bottom: 0.2em; }
h3 { font-size: 1.2em; } h4 { font-size: 1.05em; } h5, h6 { font-size: 1em; color: #59636e; }
body > :first-child { margin-top: 0; }
p, ul, ol, table, pre, blockquote, .callout, figure { margin: 0 0 0.9em; }
a { color: #0969da; text-decoration: none; }
code, pre { font-family: "JetBrains Mono", "Hack", "DejaVu Sans Mono", Consolas, monospace;
  font-size: 0.86em; }
:not(pre) > code { background: #eff1f3; border-radius: 4px; padding: 0.1em 0.35em; }
pre { background: #f6f8fa; border: 1px solid #e5e8eb; border-radius: 6px;
  padding: 0.8em 1em; white-space: pre-wrap; word-break: break-word; break-inside: avoid; }
blockquote { margin-left: 0; padding: 0 1em; color: #59636e; border-left: 4px solid #d0d7de; }
table { border-collapse: collapse; width: auto; max-width: 100%; }
th, td { border: 1px solid #d0d7de; padding: 0.35em 0.7em; vertical-align: top; }
th { background: #f6f8fa; } tr { break-inside: avoid; }
img { max-width: 100%; }
img.math { max-width: none; }
.math-display { text-align: center; margin: 0.8em 0; }
figure.diagram { text-align: center; break-inside: avoid; }
figure.diagram svg { max-width: 100%; height: auto; }
li:has(> input[type=checkbox]) { list-style: none; }
li > input[type=checkbox] { margin: 0 0.5em 0 -1.4em; }
hr { border: 0; border-top: 1px solid #d0d7de; margin: 1.5em 0; }
.footnote-definition { font-size: 0.9em; color: #59636e; }
.callout { border-left: 4px solid var(--c); background: var(--bg); border-radius: 4px;
  padding: 0.6em 1em; break-inside: avoid; }
.callout-title { font-weight: 600; color: var(--c); margin-bottom: 0.3em; }
.callout-icon { margin-right: 0.45em; }
.callout-body > :last-child { margin-bottom: 0; }
.callout-info { --c: #0969da; --bg: #ddf4ff; }
.callout-success { --c: #1a7f37; --bg: #dafbe1; }
.callout-question { --c: #9a6700; --bg: #fff8c5; }
.callout-warning { --c: #bc4c00; --bg: #fff1e5; }
.callout-danger { --c: #cf222e; --bg: #ffebe9; }
.callout-example { --c: #8250df; --bg: #fbefff; }
.callout-quote { --c: #59636e; --bg: #f6f8fa; }
.render-error { color: #cf222e; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_paste_becomes_editor_style_markdown() {
        let md = html_to_markdown(
            "<h2>Titolo</h2><p>Testo <b>forte</b> e <a href=\"https://e.com\">link</a>.</p>\
             <ul><li>uno</li><li>due</li></ul><pre><code>let x = 1;</code></pre>",
        )
        .unwrap();
        assert!(md.contains("## Titolo"), "{md}");
        assert!(
            md.contains("**forte**") && md.contains("[link](https://e.com)"),
            "{md}"
        );
        assert!(md.contains("- uno\n- due"), "{md}");
        assert!(md.contains("```\nlet x = 1;\n```"), "{md}");
        assert_eq!(html_to_markdown("<p> </p>"), None);
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn headings_get_github_ids_deduplicated() {
        let html = render_body("# Stato attuale\n\n## Note\n\n## Note\n", None);
        assert!(
            html.contains("<h1 id=\"stato-attuale\">Stato attuale</h1>"),
            "{html}"
        );
        assert!(html.contains("<h2 id=\"note\">Note</h2>"));
        assert!(html.contains("<h2 id=\"note-1\">Note</h2>"));
    }

    #[test]
    fn callouts_become_styled_boxes() {
        let html = render_body("> [!warning] Attenzione\n> Testo del corpo.\n", None);
        assert!(html.contains("callout callout-warning"), "{html}");
        assert!(html.contains("Attenzione</div>"));
        assert!(html.contains("<p>Testo del corpo.</p>"), "{html}");
        assert!(!html.contains("[!warning]"));
        // Default title, header-only first paragraph.
        let html = render_body("> [!tip]\n>\n> Corpo.\n", None);
        assert!(
            html.contains("callout-success") && html.contains("Tip</div>"),
            "{html}"
        );
        // A plain quote stays a quote, its text intact.
        let html = render_body("> Solo una citazione.\n", None);
        assert!(
            html.contains("<blockquote>\n<p>Solo una citazione.</p>"),
            "{html}"
        );
    }

    #[test]
    fn code_is_highlighted_and_escaped() {
        let html = render_body("```rust\nfn main() { let a = 1 < 2; }\n```\n", None);
        assert!(html.contains("<span style=\"color:#"), "{html}");
        assert!(html.contains("&lt;"));
        let plain = render_body("```\n<b>x</b>\n```\n", None);
        assert!(plain.contains("&lt;b&gt;x&lt;/b&gt;"));
    }

    #[test]
    fn math_and_mermaid_are_embedded() {
        let html = render_body("Inline $x^2$ and\n\n$$\\frac{a}{b}$$\n", None);
        assert_eq!(html.matches("data:image/png;base64,").count(), 2, "{html}");
        assert!(html.contains("math-display"));
        let html = render_body("```mermaid\nflowchart TD\n  A --> B\n```\n", None);
        assert!(html.contains("<figure class=\"diagram\"><svg"), "{html}");
    }

    #[test]
    fn relative_images_become_absolute_file_urls() {
        let html = render_body(
            "![x](<img/a b.png>) ![w](img/c%20d.png) ![y](https://e.com/z.png)",
            Some(Path::new("/docs")),
        );
        assert!(
            html.contains("src=\"file:///docs/img/a%20b.png\""),
            "{html}"
        );
        assert!(
            html.contains("src=\"file:///docs/img/c%20d.png\""),
            "{html}"
        );
        assert!(html.contains("src=\"https://e.com/z.png\""));
    }

    #[test]
    fn task_lists_tables_and_footnotes() {
        let html = render_body(
            "- [x] fatto\n- [ ] da fare\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nNota[^1]\n\n[^1]: testo\n",
            None,
        );
        assert!(html.contains("type=\"checkbox\""));
        assert!(html.contains("<table>"));
        assert!(html.contains("footnote-definition"));
    }

    /// End to end with the installed browser (ignored by default: needs Chrome or
    /// Chromium). `MARKDONE_EXPORT_SAMPLE=<dir>` keeps the PDF there for inspection.
    #[test]
    #[ignore]
    fn exports_a_real_pdf() {
        let dir = std::env::var_os("MARKDONE_EXPORT_SAMPLE")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("markdone-export-test"));
        std::fs::create_dir_all(&dir).unwrap();
        let output = dir.join("sample.pdf");
        let markdown = "# Esempio di esportazione\n\nTesto con **grassetto**, `codice` e una \
            formula $e^{i\\pi} + 1 = 0$.\n\n> [!warning] Attenzione\n> Un callout di avviso.\n\n\
            ## Tabella\n\n| Campo | Tipo |\n|---|---|\n| id | int |\n| nome | text |\n\n\
            ```rust\nfn main() {\n    println!(\"ciao\");\n}\n```\n\n$$\\int_0^1 x^2\\,dx = \\frac{1}{3}$$\n\n\
            ```mermaid\nflowchart LR\n  A[Input] --> B{Valido?}\n  B -->|sì| C[Salva]\n  B -->|no| D[Errore]\n```\n\n\
            - [x] fatto\n- [ ] da fare\n";
        let job = ExportJob {
            markdown: markdown.to_string(),
            base_dir: None,
            title: "Esempio".into(),
            output: output.clone(),
        };
        export_pdf(&job).unwrap();
        let bytes = std::fs::read(&output).unwrap();
        assert!(bytes.starts_with(b"%PDF"), "not a PDF");
    }
}

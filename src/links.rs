//! Where a Ctrl+clicked link goes: a heading of the same document (`#anchor`), another
//! Markdown document (opened in a tab, optionally at an anchor), or anything else
//! (handed to the desktop: browser, file manager, default application).
//!
//! Anchors follow GitHub's heading ids, the de facto convention for Markdown links:
//! lowercase, punctuation and symbols (emoji included) dropped, spaces as `-`.

use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub enum LinkTarget {
    /// `#anchor` in the current document.
    Anchor(String),
    /// A Markdown file, with an optional `#anchor`.
    Document {
        path: PathBuf,
        anchor: Option<String>,
    },
    /// Opened by the desktop (web URL, mail, any other file).
    External(String),
}

/// File extensions opened as Markdown documents.
const MARKDOWN_EXTENSIONS: [&str; 5] = ["md", "markdown", "mdown", "mkd", "mdx"];

/// Classify a link target as returned by `Editor::link_at` (relative paths already
/// resolved against the document's folder).
pub fn classify(url: &str) -> LinkTarget {
    if let Some(anchor) = url.strip_prefix('#') {
        return LinkTarget::Anchor(percent_decode(anchor));
    }
    let local = if let Some(rest) = url.strip_prefix("file://") {
        // file:///abs/path (an empty host); a host other than localhost is not local.
        match rest.strip_prefix("localhost") {
            Some(path) => path,
            None if rest.starts_with('/') => rest,
            None => return LinkTarget::External(url.to_string()),
        }
    } else if url.contains("://") || url.starts_with("mailto:") {
        return LinkTarget::External(url.to_string());
    } else {
        url
    };
    let (path, anchor) = match local.split_once('#') {
        Some((p, a)) => (p, Some(percent_decode(a))),
        None => (local, None),
    };
    let path = PathBuf::from(percent_decode(path));
    let is_markdown = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| MARKDOWN_EXTENSIONS.iter().any(|m| m.eq_ignore_ascii_case(e)));
    if is_markdown {
        LinkTarget::Document {
            path,
            anchor: anchor.filter(|a| !a.is_empty()),
        }
    } else {
        LinkTarget::External(path.to_string_lossy().into_owned())
    }
}

/// GitHub's id for a heading: lowercase; letters, digits, `-` and `_` kept; spaces
/// become `-`; everything else (punctuation, symbols, emoji, Markdown markers) dropped.
pub fn heading_slug(text: &str) -> String {
    text.trim()
        .chars()
        .flat_map(char::to_lowercase)
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

/// Whether `anchor` (from a link) designates the heading `text`. Exact GitHub id
/// first; then, leniently, ignoring case and leading/trailing dashes (an emoji at the
/// start of a heading leaves a leading `-` that hand-written links often omit).
pub fn anchor_matches(text: &str, anchor: &str) -> bool {
    let slug = heading_slug(text);
    if slug == anchor {
        return true;
    }
    let anchor = anchor.to_lowercase();
    slug.trim_matches('-') == anchor.trim_matches('-')
}

/// Decode `%XX` escapes (UTF-8); malformed escapes are kept as they are.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_links() {
        assert_eq!(classify("#stato-attuale"), LinkTarget::Anchor("stato-attuale".into()));
        assert_eq!(
            classify("/docs/a b/Analisi%20Match.md#regole-g3"),
            LinkTarget::Document {
                path: "/docs/a b/Analisi Match.md".into(),
                anchor: Some("regole-g3".into()),
            }
        );
        assert_eq!(
            classify("file:///docs/README.MD"),
            LinkTarget::Document { path: "/docs/README.MD".into(), anchor: None }
        );
        assert_eq!(
            classify("/docs/x.md#"),
            LinkTarget::Document { path: "/docs/x.md".into(), anchor: None }
        );
        assert_eq!(
            classify("https://example.com/a.md"),
            LinkTarget::External("https://example.com/a.md".into())
        );
        assert_eq!(classify("mailto:a@b.c"), LinkTarget::External("mailto:a@b.c".into()));
        assert_eq!(classify("/docs/schema.png"), LinkTarget::External("/docs/schema.png".into()));
        assert_eq!(
            classify("file://server/share/x.md"),
            LinkTarget::External("file://server/share/x.md".into())
        );
    }

    #[test]
    fn slugs_follow_github() {
        assert_eq!(heading_slug("Stato attuale"), "stato-attuale");
        assert_eq!(heading_slug("1. Regole G3 (v2)"), "1-regole-g3-v2");
        assert_eq!(heading_slug("Perché è così?"), "perché-è-così");
        assert_eq!(heading_slug("🟢 Completato"), "-completato");
        assert_eq!(heading_slug("`code` and **bold**"), "code-and-bold");
        assert_eq!(heading_slug("snake_case-name"), "snake_case-name");
    }

    #[test]
    fn anchors_match_headings() {
        assert!(anchor_matches("Stato attuale", "stato-attuale"));
        assert!(anchor_matches("🟢 Completato", "-completato"));
        assert!(anchor_matches("🟢 Completato", "completato"));
        assert!(anchor_matches("Stato Attuale", "Stato-Attuale"));
        assert!(!anchor_matches("Stato attuale", "stato"));
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b%C3%A8"), "a bè");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%2"), "%zz%2");
    }
}

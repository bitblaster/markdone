//! Pasting an image from the clipboard: the file name and the folder the dialog
//! proposes, the checks on what the user typed, and writing the PNG.
//!
//! The shell owns the dialog and the clipboard; this module has no UI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

/// An image read from the clipboard: `rgba` holds `width × height` RGBA8 pixels.
#[derive(Clone)]
pub struct PastedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The name proposed for a pasted image taken at `now`: `image-YYYYMMDD-HHMMSS.png`,
/// with `-2`, `-3`… added when `dir` already has a file of that name.
pub fn proposed_name(now: &jiff::Zoned, dir: &Path) -> String {
    let stem = now.strftime("image-%Y%m%d-%H%M%S").to_string();
    let mut name = format!("{stem}.png");
    let mut n = 2;
    while dir.join(&name).exists() {
        name = format!("{stem}-{n}.png");
        n += 1;
    }
    name
}

/// The folder proposed for a pasted image: the one most of the document's local images
/// live in (the first such folder on a tie), when it exists; otherwise the document's
/// own folder `doc_dir`.
pub fn proposed_dir(text: &str, doc_dir: &Path) -> PathBuf {
    static IMAGE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"!\[[^\]]*\]\(\s*(?:<([^>]+)>|([^)\s]+))").expect("valid regex")
    });
    let mut counts: HashMap<PathBuf, (usize, usize)> = HashMap::new();
    for (order, caps) in IMAGE.captures_iter(text).enumerate() {
        let Some(target) = caps.get(1).or_else(|| caps.get(2)).map(|m| m.as_str()) else {
            continue;
        };
        if target.contains("://") || target.starts_with("data:") {
            continue;
        }
        let target = target.replace("%20", " ");
        let path = Path::new(&target);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            doc_dir.join(path)
        };
        let Some(dir) = path.parent().map(normalize) else {
            continue;
        };
        let entry = counts.entry(dir).or_insert((0, order));
        entry.0 += 1;
    }
    counts
        .into_iter()
        .filter(|(dir, _)| dir.is_dir())
        // Most images first; on a tie, the folder that appears first in the document.
        .max_by(|(_, (n1, o1)), (_, (n2, o2))| n1.cmp(n2).then(o2.cmp(o1)))
        .map_or_else(|| doc_dir.to_path_buf(), |(dir, _)| dir)
}

/// `path` with its `.` and `..` components resolved lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The file name to write for what the user typed: trimmed, with `.png` added unless it
/// already ends with it. Errors (as shown in the dialog) on an empty name or one with a
/// path separator or characters no file system accepts.
pub fn checked_name(typed: &str) -> Result<String, &'static str> {
    let name = typed.trim();
    if name.is_empty() || name == ".png" {
        return Err("Enter a file name.");
    }
    if name.contains(['/', '\\']) {
        return Err("The name cannot contain / or \\: choose the folder with Browse.");
    }
    if name.contains(['<', '>', ':', '"', '|', '?', '*']) || name.chars().any(char::is_control) {
        return Err("The name contains characters that are not allowed in file names.");
    }
    if name.to_ascii_lowercase().ends_with(".png") {
        Ok(name.to_string())
    } else {
        Ok(format!("{name}.png"))
    }
}

/// Write `image` as a PNG at `path`, creating its folder if needed. Never overwrites: an
/// existing file is an error.
pub fn save_png(image: &PastedImage, path: &Path) -> Result<(), String> {
    if path.exists() {
        return Err("A file with this name already exists in the folder.".to_string());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create the folder: {e}"))?;
    }
    let buffer = image::RgbaImage::from_raw(image.width, image.height, image.rgba.clone())
        .ok_or_else(|| "The clipboard image is not valid.".to_string())?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("Cannot create the file: {e}"))?;
    let mut writer = std::io::BufWriter::new(file);
    buffer
        .write_to(&mut writer, image::ImageFormat::Png)
        .map_err(|e| {
            let _ = std::fs::remove_file(path);
            format!("Cannot write the image: {e}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("markdone_paste_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn proposed_name_is_timestamped_and_unique() {
        let dir = temp_dir("name");
        let now: jiff::Zoned = "2026-11-21T15:30:12[Europe/Rome]".parse().unwrap();
        assert_eq!(proposed_name(&now, &dir), "image-20261121-153012.png");
        std::fs::write(dir.join("image-20261121-153012.png"), b"x").unwrap();
        assert_eq!(proposed_name(&now, &dir), "image-20261121-153012-2.png");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn proposed_dir_follows_the_documents_images() {
        let doc = temp_dir("dir");
        std::fs::create_dir_all(doc.join("assets")).unwrap();
        std::fs::create_dir_all(doc.join("img")).unwrap();
        let text = "![a](assets/a.png)\n![b](img/b.png) ![c](<img/c d.png>)\n\
                    ![web](https://example.com/x.png) ![gone](missing/x.png)\n";
        assert_eq!(proposed_dir(text, &doc), doc.join("img"));
        // A tie goes to the folder seen first; no local image → the document's folder.
        assert_eq!(
            proposed_dir("![a](assets/a.png) ![b](img/b.png)", &doc),
            doc.join("assets")
        );
        assert_eq!(proposed_dir("no images", &doc), doc);
        assert_eq!(
            proposed_dir("![x](./assets/../img/x.png)", &doc),
            doc.join("img")
        );
        std::fs::remove_dir_all(&doc).unwrap();
    }

    #[test]
    fn checked_name_adds_png_and_rejects_bad_names() {
        assert_eq!(checked_name(" shot ").unwrap(), "shot.png");
        assert_eq!(checked_name("shot.PNG").unwrap(), "shot.PNG");
        assert!(checked_name("  ").is_err());
        assert!(checked_name("a/b").is_err());
        assert!(checked_name("a:b").is_err());
    }

    #[test]
    fn save_png_writes_and_never_overwrites() {
        let dir = temp_dir("save");
        let image = PastedImage {
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 0, 255, 128],
        };
        let path = dir.join("sub").join("x.png");
        save_png(&image, &path).unwrap();
        let back = image::open(&path).unwrap().into_rgba8();
        assert_eq!(back.dimensions(), (2, 1));
        assert_eq!(back.into_raw(), image.rgba);
        assert!(save_png(&image, &path).is_err(), "an existing file is kept");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

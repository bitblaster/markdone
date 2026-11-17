//! Platform-specific integration, kept in one place so the rest of the code stays
//! portable (Linux is the release target; Windows/macOS must keep compiling).

use std::path::Path;
use std::process::{Command, Stdio};

/// Show `path` selected in the system file manager. Falls back to opening its folder
/// when the file manager cannot select items.
pub fn reveal_in_file_manager(path: &Path) {
    if reveal_selected(path) {
        return;
    }
    if let Some(parent) = path.parent()
        && let Err(e) = open::that_detached(parent)
    {
        eprintln!("[writ] cannot open {}: {e}", parent.display());
    }
}

/// Freedesktop `org.freedesktop.FileManager1.ShowItems` (Dolphin, Nautilus, Nemo…).
#[cfg(all(unix, not(target_os = "macos")))]
fn reveal_selected(path: &Path) -> bool {
    let Ok(abs) = std::fs::canonicalize(path) else {
        return false;
    };
    let uri = format!("file://{}", percent_encode_path(&abs.to_string_lossy()));
    Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.freedesktop.FileManager1",
            "--type=method_call",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1.ShowItems",
        ])
        .arg(format!("array:string:{uri}"))
        .arg("string:")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(target_os = "windows")]
fn reveal_selected(path: &Path) -> bool {
    let mut arg = std::ffi::OsString::from("/select,");
    arg.push(path.as_os_str());
    Command::new("explorer")
        .arg(arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(target_os = "macos")]
fn reveal_selected(path: &Path) -> bool {
    Command::new("open")
        .arg("-R")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// Percent-encode a filesystem path for a `file://` URI (keeps `/` and unreserved
/// characters; encodes spaces, non-ASCII bytes and everything else).
#[cfg_attr(any(target_os = "windows", target_os = "macos"), allow(dead_code))]
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encodes_spaces_and_accents() {
        assert_eq!(
            percent_encode_path("/home/u/Documenti/perché sì.md"),
            "/home/u/Documenti/perch%C3%A9%20s%C3%AC.md"
        );
    }
}

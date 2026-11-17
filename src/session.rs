//! Session state kept between runs: the documents open at exit (restored when Markdone
//! starts without files) and the recently opened files (File → Open Recent).
//!
//! Stored as TOML in the platform state directory, e.g.
//! `~/.local/state/markdone/session.toml` on Linux (`%LOCALAPPDATA%\markdone` on
//! Windows). The file is rewritten whenever its content changes, so a crash loses at
//! most the last change. A missing or unreadable file is an empty session.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How many recent files File → Open Recent keeps.
pub const MAX_RECENT: usize = 10;

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Documents open at exit, in tab order (untitled documents are not kept).
    #[serde(default)]
    pub open: Vec<PathBuf>,
    /// Index in `open` of the active document.
    #[serde(default)]
    pub active: usize,
    /// Recently opened files, most recent first.
    #[serde(default)]
    pub recent: Vec<PathBuf>,
}

impl Session {
    /// The default session file location.
    pub fn default_path() -> Option<PathBuf> {
        dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .map(|d| d.join("markdone").join("session.toml"))
    }

    /// Read a session file; a missing or invalid file gives an empty session.
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("[markdone] ignoring invalid session file {}: {e}", path.display());
            Self::default()
        })
    }

    /// Write the session file (via a temporary file and a rename, so a crash mid-write
    /// never leaves a truncated session).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }

    /// Record `path` as the most recently opened file.
    pub fn touch_recent(&mut self, path: &Path) {
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(MAX_RECENT);
    }

    /// The documents to restore at startup: those of `open` that still exist, with the
    /// index of the active one among them.
    pub fn restorable(&self) -> (Vec<PathBuf>, usize) {
        let active = self.open.get(self.active);
        let paths: Vec<PathBuf> = self.open.iter().filter(|p| p.is_file()).cloned().collect();
        let index = active
            .and_then(|a| paths.iter().position(|p| p == a))
            .unwrap_or(0);
        (paths, index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_moves_to_front_without_duplicates_and_is_capped() {
        let mut s = Session::default();
        for i in 0..12 {
            s.touch_recent(Path::new(&format!("/d/{i}.md")));
        }
        assert_eq!(s.recent.len(), MAX_RECENT);
        assert_eq!(s.recent[0], Path::new("/d/11.md"));
        s.touch_recent(Path::new("/d/5.md"));
        assert_eq!(s.recent[0], Path::new("/d/5.md"));
        assert_eq!(s.recent.iter().filter(|p| *p == Path::new("/d/5.md")).count(), 1);
        assert_eq!(s.recent.len(), MAX_RECENT);
    }

    #[test]
    fn save_and_load_round_trip_and_bad_files_are_empty() {
        let dir = std::env::temp_dir().join(format!("markdone_session_{}", std::process::id()));
        let path = dir.join("sub").join("session.toml");
        let s = Session {
            open: vec!["/a/x.md".into(), "/b/y.md".into()],
            active: 1,
            recent: vec!["/b/y.md".into()],
        };
        s.save(&path).unwrap();
        assert_eq!(Session::load(&path), s);

        std::fs::write(&path, "open = 3").unwrap();
        assert_eq!(Session::load(&path), Session::default());
        assert_eq!(Session::load(&dir.join("missing.toml")), Session::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restorable_skips_missing_files_and_follows_the_active_one() {
        let dir = std::env::temp_dir().join(format!("markdone_restore_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.md");
        let c = dir.join("c.md");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&c, "c").unwrap();
        let s = Session {
            open: vec![a.clone(), dir.join("gone.md"), c.clone()],
            active: 2,
            recent: Vec::new(),
        };
        assert_eq!(s.restorable(), (vec![a.clone(), c.clone()], 1));
        // The active document is gone: the first remaining one is active.
        let s = Session { active: 1, ..s };
        assert_eq!(s.restorable(), (vec![a, c], 0));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

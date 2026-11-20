//! Application commands: what a menu item (and, progressively, a keyboard shortcut)
//! asks the shell to do. The menu emits [`Command`] values; the shell executes them in
//! one place, so a menu entry and its shortcut cannot drift apart.

use crate::format::{Block, Inline};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    New,
    Open,
    /// Open entry `i` of File → Open Recent.
    OpenRecent(usize),
    ClearRecent,
    Save,
    SaveAs,
    /// Re-read the active document from disk (asks first if it has unsaved changes).
    Reload,
    ExportPdf,
    CloseTab,
    Quit,
    Undo,
    Redo,
    Cut,
    Copy,
    /// Copy the selection rendered as HTML (with the Markdown as plain text).
    CopyAsHtml,
    /// Paste, converting rich (HTML) clipboard content to Markdown.
    Paste,
    /// Paste the clipboard's plain text as is.
    PasteWithoutStyle,
    SelectAll,
    /// Toggle an inline style on the selection (or the word at the caret).
    Inline(Inline),
    /// Set a paragraph style on the selected lines.
    Block(Block),
    InsertLink,
    InsertImage,
    InsertCodeBlock,
    Preferences,
    Find,
    Replace,
    ToggleOutline,
    ToggleLineNumbers,
    ToggleGitDiff,
    /// Switch between the light and the dark theme (and stop following the system).
    ToggleDarkMode,
    /// Jump to heading `i` of the active document (table of contents).
    GoToHeading(usize),
    ZoomIn,
    ZoomOut,
    ResetZoom,
    NextTab,
    PrevTab,
    /// Tab-strip actions, by tab index.
    SwitchTab(usize),
    CloseTabAt(usize),
    CloseOtherTabs(usize),
    CloseAllTabs,
    CopyPath(usize),
    RevealInFileManager(usize),
    MoveTab { from: usize, to: usize },
}

impl Command {
    /// Shortcut label shown next to the menu entry.
    pub fn shortcut(self) -> &'static str {
        match self {
            Command::New => "Ctrl+N",
            Command::Open => "Ctrl+O",
            Command::Save => "Ctrl+S",
            Command::SaveAs => "Ctrl+Shift+S",
            Command::ExportPdf => "Ctrl+E",
            Command::Reload => "F5",
            Command::CloseTab => "Ctrl+W",
            Command::Quit => "Ctrl+Q",
            Command::Undo => "Ctrl+Z",
            Command::Redo => "Ctrl+Shift+Z",
            Command::Cut => "Ctrl+X",
            Command::Copy => "Ctrl+C",
            Command::Paste => "Ctrl+V",
            Command::CopyAsHtml => "Ctrl+Shift+C",
            Command::PasteWithoutStyle => "Ctrl+Shift+V",
            Command::Inline(Inline::Bold) => "Ctrl+B",
            Command::Inline(Inline::Italic) => "Ctrl+I",
            Command::Inline(Inline::Underline) => "Ctrl+U",
            Command::Inline(Inline::Strikethrough) => "Ctrl+Shift+X",
            Command::Inline(Inline::Highlight) => "Ctrl+Shift+H",
            Command::Inline(Inline::Code) => "Ctrl+`",
            Command::Block(Block::Normal) => "Ctrl+Alt+0",
            Command::Block(Block::Heading(1)) => "Ctrl+Alt+1",
            Command::Block(Block::Heading(2)) => "Ctrl+Alt+2",
            Command::Block(Block::Heading(3)) => "Ctrl+Alt+3",
            Command::Block(Block::Heading(4)) => "Ctrl+Alt+4",
            Command::Block(Block::Heading(5)) => "Ctrl+Alt+5",
            Command::Block(Block::Numbered) => "Ctrl+Shift+7",
            Command::Block(Block::Bulleted) => "Ctrl+Shift+8",
            Command::Block(Block::Quote) => "Ctrl+Shift+9",
            Command::InsertLink => "Ctrl+K",
            Command::InsertImage => "Ctrl+Shift+I",
            Command::InsertCodeBlock => "Ctrl+Shift+K",
            Command::SelectAll => "Ctrl+A",
            Command::Preferences => "Ctrl+,",
            Command::ZoomIn => "Ctrl++",
            Command::ZoomOut => "Ctrl+-",
            Command::ResetZoom => "Ctrl+0",
            Command::Find => "Ctrl+F",
            Command::Replace => "Ctrl+H",
            Command::ToggleOutline => "Ctrl+Shift+O",
            Command::NextTab => "Ctrl+Tab",
            Command::PrevTab => "Ctrl+Shift+Tab",
            _ => "",
        }
    }
}

/// The formatting / clipboard command for a key press in the document, if any.
/// `key` is the logical key, `code` the physical one (digits and the backquote are read
/// physically: with Shift or AltGr layouts turn them into other characters).
pub fn format_shortcut(
    key: &winit::keyboard::Key,
    code: winit::keyboard::PhysicalKey,
    ctrl: bool,
    shift: bool,
    alt: bool,
) -> Option<Command> {
    use winit::keyboard::{Key, KeyCode, PhysicalKey};
    if !ctrl {
        return None;
    }
    if let PhysicalKey::Code(code) = code {
        let digit = match code {
            KeyCode::Digit0 => Some(0),
            KeyCode::Digit1 => Some(1),
            KeyCode::Digit2 => Some(2),
            KeyCode::Digit3 => Some(3),
            KeyCode::Digit4 => Some(4),
            KeyCode::Digit5 => Some(5),
            KeyCode::Digit7 => Some(7),
            KeyCode::Digit8 => Some(8),
            KeyCode::Digit9 => Some(9),
            _ => None,
        };
        match (digit, shift, alt) {
            (Some(0), false, true) => return Some(Command::Block(Block::Normal)),
            (Some(n @ 1..=5), false, true) => return Some(Command::Block(Block::Heading(n))),
            (Some(7), true, false) => return Some(Command::Block(Block::Numbered)),
            (Some(8), true, false) => return Some(Command::Block(Block::Bulleted)),
            (Some(9), true, false) => return Some(Command::Block(Block::Quote)),
            _ => {}
        }
        if code == KeyCode::Backquote && !alt {
            return Some(Command::Inline(Inline::Code));
        }
    }
    if alt {
        return None;
    }
    let Key::Character(c) = key else {
        return None;
    };
    Some(match (c.to_ascii_lowercase().as_str(), shift) {
        ("b", false) => Command::Inline(Inline::Bold),
        ("i", false) => Command::Inline(Inline::Italic),
        ("u", false) => Command::Inline(Inline::Underline),
        ("x", true) => Command::Inline(Inline::Strikethrough),
        ("h", true) => Command::Inline(Inline::Highlight),
        ("k", false) => Command::InsertLink,
        ("k", true) => Command::InsertCodeBlock,
        ("i", true) => Command::InsertImage,
        ("c", true) => Command::CopyAsHtml,
        ("v", true) => Command::PasteWithoutStyle,
        ("v", false) => Command::Paste,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::{Key, KeyCode, PhysicalKey, SmolStr};

    fn ch(c: &str) -> Key {
        Key::Character(SmolStr::new(c))
    }

    #[test]
    fn format_shortcuts_map_and_leave_existing_ones_alone() {
        let code = |c| PhysicalKey::Code(c);
        let none = PhysicalKey::Code(KeyCode::KeyQ);
        let f = |k: &Key, p, shift, alt| format_shortcut(k, p, true, shift, alt);
        assert_eq!(f(&ch("b"), code(KeyCode::KeyB), false, false), Some(Command::Inline(Inline::Bold)));
        assert_eq!(f(&ch("X"), code(KeyCode::KeyX), true, false), Some(Command::Inline(Inline::Strikethrough)));
        assert_eq!(f(&ch("V"), code(KeyCode::KeyV), true, false), Some(Command::PasteWithoutStyle));
        assert_eq!(f(&ch("v"), code(KeyCode::KeyV), false, false), Some(Command::Paste));
        // Digits are physical: Ctrl+Shift+8 is "(" on an Italian layout.
        assert_eq!(f(&ch("("), code(KeyCode::Digit8), true, false), Some(Command::Block(Block::Bulleted)));
        assert_eq!(f(&ch("3"), code(KeyCode::Digit3), false, true), Some(Command::Block(Block::Heading(3))));
        assert_eq!(f(&ch("0"), code(KeyCode::Digit0), false, true), Some(Command::Block(Block::Normal)));
        // Not ours: Ctrl+0 (reset zoom), Ctrl+Shift+1..6 (fold levels), plain Ctrl+X/C.
        assert_eq!(f(&ch("0"), code(KeyCode::Digit0), false, false), None);
        assert_eq!(f(&ch("!"), code(KeyCode::Digit1), true, false), None);
        assert_eq!(f(&ch("x"), code(KeyCode::KeyX), false, false), None);
        assert_eq!(f(&ch("c"), code(KeyCode::KeyC), false, false), None);
        assert_eq!(f(&ch("s"), none, false, false), None);
        // Without Ctrl nothing is a shortcut.
        assert_eq!(format_shortcut(&ch("b"), code(KeyCode::KeyB), false, false, false), None);
    }
}

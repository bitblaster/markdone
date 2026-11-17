//! Application commands: what a menu item (and, progressively, a keyboard shortcut)
//! asks the shell to do. The menu emits [`Command`] values; the shell executes them in
//! one place, so a menu entry and its shortcut cannot drift apart.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    New,
    Open,
    Save,
    SaveAs,
    CloseTab,
    Quit,
    Undo,
    Redo,
    Find,
    Replace,
    ToggleOutline,
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
            Command::CloseTab => "Ctrl+W",
            Command::Quit => "Ctrl+Q",
            Command::Undo => "Ctrl+Z",
            Command::Redo => "Ctrl+Shift+Z",
            Command::Find => "Ctrl+F",
            Command::Replace => "Ctrl+H",
            Command::ToggleOutline => "Ctrl+Shift+O",
            Command::NextTab => "Ctrl+Tab",
            Command::PrevTab => "Ctrl+Shift+Tab",
            _ => "",
        }
    }
}

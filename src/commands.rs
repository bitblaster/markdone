//! Application commands: what a menu item (and, progressively, a keyboard shortcut)
//! asks the shell to do. The menu emits [`Command`] values; the shell executes them in
//! one place, so a menu entry and its shortcut cannot drift apart.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    New,
    Open,
    /// Open entry `i` of File → Open Recent.
    OpenRecent(usize),
    ClearRecent,
    Save,
    SaveAs,
    CloseTab,
    Quit,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Preferences,
    Find,
    Replace,
    ToggleOutline,
    ToggleLineNumbers,
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
            Command::CloseTab => "Ctrl+W",
            Command::Quit => "Ctrl+Q",
            Command::Undo => "Ctrl+Z",
            Command::Redo => "Ctrl+Shift+Z",
            Command::Cut => "Ctrl+X",
            Command::Copy => "Ctrl+C",
            Command::Paste => "Ctrl+V",
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

//! The Markdone editor binary.
//! Thin wrapper; all logic lives in `markdone::shell`.

// A GUI app on Windows: without this, launching it also opens a console window. Debug
// builds keep the console, for the `[markdone]` log lines.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    markdone::shell::run()
}

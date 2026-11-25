//! The top menu bar.

use crate::commands::Command;
use crate::format::{Block, Inline};
use crate::table::Align;
use crate::table_edit::TableOp;

use super::MENU_BAR_H;

/// State the menu needs to enable/check its entries.
pub struct MenuState {
    pub outline_open: bool,
    pub line_numbers: bool,
    pub git_diff: bool,
    /// The dark theme is shown.
    pub dark_mode: bool,
    /// The active document has a file (Reload is possible).
    pub has_file: bool,
    pub tab_count: usize,
    /// Recent files: (file name, shortened folder), most recent first.
    pub recent: Vec<(String, String)>,
    /// The document has a selection (Cut / Copy enabled).
    pub has_selection: bool,
    /// The caret is in a table: paragraph styles and block inserts are off (a table
    /// cell holds inline text only).
    pub in_table: bool,
}

/// Lay out the menu bar in a top panel; returns the command picked this frame, if any.
pub fn menu_bar(ui: &mut egui::Ui, state: &MenuState) -> Option<Command> {
    let mut picked = None;
    egui::Panel::top("writ-menu-bar")
        .exact_size(MENU_BAR_H)
        .resizable(false)
        .show(ui, |ui| {
            let config = egui::containers::menu::MenuConfig::new().style(super::menu_entry_style);
            egui::MenuBar::new()
                .style(super::menu_entry_style)
                .config(config)
                .ui(ui, |ui| {
                    let mut titles: Vec<egui::Response> = Vec::with_capacity(5);
                    titles.push(
                        ui.menu_button("File", |ui| {
                            item(ui, "New", Command::New, true, &mut picked);
                            item(ui, "Open…", Command::Open, true, &mut picked);
                            ui.menu_button("Open Recent", |ui| {
                                if state.recent.is_empty() {
                                    ui.add_enabled(false, egui::Button::new("No Recent Files"));
                                }
                                for (i, (name, dir)) in state.recent.iter().enumerate() {
                                    let button = egui::Button::new(name.as_str())
                                        .shortcut_text(dir.as_str());
                                    if ui.add(button).clicked() {
                                        picked = Some(Command::OpenRecent(i));
                                        ui.close();
                                    }
                                }
                                ui.separator();
                                let any = !state.recent.is_empty();
                                item(ui, "Clear Recent", Command::ClearRecent, any, &mut picked);
                            });
                            ui.separator();
                            item(ui, "Save", Command::Save, true, &mut picked);
                            item(ui, "Save As…", Command::SaveAs, true, &mut picked);
                            item(ui, "Reload", Command::Reload, state.has_file, &mut picked);
                            item(ui, "Export as PDF…", Command::ExportPdf, true, &mut picked);
                            ui.separator();
                            item(ui, "Close Tab", Command::CloseTab, true, &mut picked);
                            item(ui, "Quit", Command::Quit, true, &mut picked);
                        })
                        .response,
                    );
                    titles.push(
                        ui.menu_button("Edit", |ui| {
                            item(ui, "Undo", Command::Undo, true, &mut picked);
                            item(ui, "Redo", Command::Redo, true, &mut picked);
                            ui.separator();
                            item(ui, "Cut", Command::Cut, true, &mut picked);
                            item(ui, "Copy", Command::Copy, true, &mut picked);
                            item(ui, "Paste", Command::Paste, true, &mut picked);
                            item(ui, "Select All", Command::SelectAll, true, &mut picked);
                            ui.separator();
                            item(ui, "Find…", Command::Find, true, &mut picked);
                            item(ui, "Replace…", Command::Replace, true, &mut picked);
                            ui.separator();
                            item(ui, "Preferences…", Command::Preferences, true, &mut picked);
                        })
                        .response,
                    );
                    titles.push(
                        ui.menu_button("Format", |ui| {
                            format_items(ui, &mut picked, state.in_table);
                        })
                        .response,
                    );
                    titles.push(
                        ui.menu_button("View", |ui| {
                            let toc =
                                egui::Button::selectable(state.outline_open, "Table of Contents")
                                    .shortcut_text(Command::ToggleOutline.shortcut_label());
                            if ui.add(toc).clicked() {
                                picked = Some(Command::ToggleOutline);
                                ui.close();
                            }
                            let numbers =
                                egui::Button::selectable(state.line_numbers, "Show Line Numbers");
                            if ui.add(numbers).clicked() {
                                picked = Some(Command::ToggleLineNumbers);
                                ui.close();
                            }
                            let diff = egui::Button::selectable(state.git_diff, "Git Differences");
                            if ui.add(diff).clicked() {
                                picked = Some(Command::ToggleGitDiff);
                                ui.close();
                            }
                            let dark = egui::Button::selectable(state.dark_mode, "Dark Mode");
                            if ui.add(dark).clicked() {
                                picked = Some(Command::ToggleDarkMode);
                                ui.close();
                            }
                            ui.separator();
                            item(ui, "Zoom In", Command::ZoomIn, true, &mut picked);
                            item(ui, "Zoom Out", Command::ZoomOut, true, &mut picked);
                            item(ui, "Reset Zoom", Command::ResetZoom, true, &mut picked);
                            ui.separator();
                            let multi = state.tab_count > 1;
                            item(ui, "Next Tab", Command::NextTab, multi, &mut picked);
                            item(ui, "Previous Tab", Command::PrevTab, multi, &mut picked);
                        })
                        .response,
                    );
                    titles.push(
                        ui.menu_button("Help", |ui| {
                            item(ui, "About Markdone", Command::About, true, &mut picked);
                        })
                        .response,
                    );
                    switch_on_hover(ui.ctx(), &titles);
                });
        });
    picked
}

/// Native menu bar behavior, which egui's `MenuBar` lacks: while one menu is open,
/// moving the pointer onto another title opens that one instead.
fn switch_on_hover(ctx: &egui::Context, titles: &[egui::Response]) {
    let id = egui::Popup::default_response_id;
    let Some(open) = titles
        .iter()
        .position(|t| egui::Popup::is_id_open(ctx, id(t)))
    else {
        return;
    };
    if let Some(hovered) = titles.iter().position(egui::Response::hovered)
        && hovered != open
    {
        egui::Popup::close_id(ctx, id(&titles[open]));
        egui::Popup::open_id(ctx, id(&titles[hovered]));
        ctx.request_repaint();
    }
}

fn item(ui: &mut egui::Ui, label: &str, cmd: Command, enabled: bool, picked: &mut Option<Command>) {
    let button = egui::Button::new(label).shortcut_text(cmd.shortcut_label());
    if ui.add_enabled(enabled, button).clicked() {
        *picked = Some(cmd);
        ui.close();
    }
}

/// The Text / Paragraph / Insert submenus (the Format menu and the context menu).
/// `in_table`: the caret is in a table, whose cells hold inline text only, so the
/// paragraph styles and the code block (which would break the row) are off.
pub fn format_items(ui: &mut egui::Ui, picked: &mut Option<Command>, in_table: bool) {
    let blocks = !in_table;
    ui.menu_button("Text", |ui| {
        for (label, style) in [
            ("Bold", Inline::Bold),
            ("Italic", Inline::Italic),
            ("Strikethrough", Inline::Strikethrough),
            ("Highlight", Inline::Highlight),
            ("Inline Code", Inline::Code),
        ] {
            item(ui, label, Command::Inline(style), true, picked);
        }
    });
    ui.menu_button("Paragraph", |ui| {
        item(ui, "Normal", Command::Block(Block::Normal), blocks, picked);
        for level in 1..=5u8 {
            let label = format!("Heading {level}");
            item(
                ui,
                &label,
                Command::Block(Block::Heading(level)),
                blocks,
                picked,
            );
        }
        ui.separator();
        item(
            ui,
            "Bulleted List",
            Command::Block(Block::Bulleted),
            blocks,
            picked,
        );
        item(
            ui,
            "Numbered List",
            Command::Block(Block::Numbered),
            blocks,
            picked,
        );
        item(ui, "Quote", Command::Block(Block::Quote), blocks, picked);
    });
    ui.menu_button("Insert", |ui| {
        item(ui, "Link", Command::InsertLink, true, picked);
        item(ui, "Image…", Command::InsertImage, true, picked);
        item(ui, "Code Block", Command::InsertCodeBlock, blocks, picked);
        item(ui, "Footnote", Command::InsertFootnote, true, picked);
    });
}

/// The document's right-click menu at `pos` (points). `open`: open it this frame.
/// Returns the command picked, if any.
pub fn context_menu(
    ctx: &egui::Context,
    pos: egui::Pos2,
    open: bool,
    has_selection: bool,
    // Right-clicked on a misspelled word: the suggestions to offer (maybe none).
    spelling: Option<&[String]>,
    // Right-clicked in a table: offer the Table submenu.
    in_table: bool,
) -> Option<Command> {
    let mut picked = None;
    egui::Popup::new(
        egui::Id::new("markdone-context-menu"),
        ctx.clone(),
        egui::PopupAnchor::Position(pos),
        egui::LayerId::background(),
    )
    .kind(egui::PopupKind::Menu)
    .layout(egui::Layout::top_down_justified(egui::Align::Min))
    .style(super::menu_entry_style)
    .gap(0.0)
    .open_memory(open.then_some(egui::SetOpenCommand::Bool(true)))
    .show(|ui| {
        ui.set_min_width(240.0);
        if let Some(suggestions) = spelling {
            if suggestions.is_empty() {
                ui.add_enabled(false, egui::Button::new("No Suggestions"));
            }
            for (i, word) in suggestions.iter().enumerate() {
                let label = egui::RichText::new(word).strong();
                if ui.button(label).clicked() {
                    picked = Some(Command::SpellReplace(i));
                    ui.close();
                }
            }
            item(
                ui,
                "Add to Dictionary",
                Command::SpellAddWord,
                true,
                &mut picked,
            );
            ui.separator();
        }
        item(ui, "Cut", Command::Cut, has_selection, &mut picked);
        item(ui, "Copy", Command::Copy, has_selection, &mut picked);
        item(
            ui,
            "Copy as HTML",
            Command::CopyAsHtml,
            has_selection,
            &mut picked,
        );
        item(ui, "Paste", Command::Paste, true, &mut picked);
        item(
            ui,
            "Paste without Style",
            Command::PasteWithoutStyle,
            true,
            &mut picked,
        );
        ui.separator();
        format_items(ui, &mut picked, in_table);
        if in_table {
            ui.separator();
            table_items(ui, &mut picked);
        }
    });
    picked
}

/// The context menu's Table submenu: edits of the table at the caret.
fn table_items(ui: &mut egui::Ui, picked: &mut Option<Command>) {
    ui.menu_button("Table", |ui| {
        item(ui, "Clear", Command::Table(TableOp::Clear), true, picked);
        item(ui, "Delete", Command::Table(TableOp::Delete), true, picked);
        ui.separator();
        ui.menu_button("Row", |ui| {
            for (label, op) in [
                ("Insert Above", TableOp::InsertRowAbove),
                ("Insert Below", TableOp::InsertRowBelow),
                ("Delete", TableOp::DeleteRow),
            ] {
                item(ui, label, Command::Table(op), true, picked);
            }
        });
        ui.menu_button("Column", |ui| {
            for (label, op) in [
                ("Insert Left", TableOp::InsertColLeft),
                ("Insert Right", TableOp::InsertColRight),
            ] {
                item(ui, label, Command::Table(op), true, picked);
            }
            ui.separator();
            for (label, align) in [
                ("Align Left", Align::Left),
                ("Align Center", Align::Center),
                ("Align Right", Align::Right),
            ] {
                item(
                    ui,
                    label,
                    Command::Table(TableOp::Align(align)),
                    true,
                    picked,
                );
            }
            ui.separator();
            item(
                ui,
                "Delete",
                Command::Table(TableOp::DeleteCol),
                true,
                picked,
            );
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One frame of a two-title menu bar; returns the titles' rects and which menu is
    /// open after it.
    fn bar_frame(ctx: &egui::Context, events: Vec<egui::Event>) -> (Vec<egui::Rect>, Vec<bool>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let mut rects = Vec::new();
        let mut open = Vec::new();
        let _ = ctx.run_ui(input, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                let titles = vec![
                    ui.menu_button("File", |ui| ui.label("file entry")).response,
                    ui.menu_button("Edit", |ui| ui.label("edit entry")).response,
                ];
                switch_on_hover(ui.ctx(), &titles);
                rects = titles.iter().map(|t| t.rect).collect();
                open = titles
                    .iter()
                    .map(|t| egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(t)))
                    .collect();
            });
        });
        (rects, open)
    }

    #[test]
    fn hovering_another_title_switches_the_open_menu() {
        let ctx = egui::Context::default();
        let (rects, _) = bar_frame(&ctx, Vec::new());
        let (file, edit) = (rects[0].center(), rects[1].center());
        let click = |pos| {
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                },
            ]
        };
        bar_frame(&ctx, click(file));
        let (_, open) = bar_frame(&ctx, Vec::new());
        assert_eq!(open, vec![true, false], "File opens on click");
        bar_frame(&ctx, vec![egui::Event::PointerMoved(edit)]);
        let (_, open) = bar_frame(&ctx, Vec::new());
        assert_eq!(open, vec![false, true], "hovering Edit switches to it");
        // With no menu open, hovering a title opens nothing.
        let ctx = egui::Context::default();
        bar_frame(&ctx, Vec::new());
        bar_frame(&ctx, vec![egui::Event::PointerMoved(edit)]);
        let (_, open) = bar_frame(&ctx, Vec::new());
        assert_eq!(open, vec![false, false]);
    }
}

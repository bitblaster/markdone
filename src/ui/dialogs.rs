//! Modal dialogs: unsaved changes on close, a file changed on disk under unsaved edits,
//! and plain messages (errors, notices).
//!
//! These functions only draw and report the user's choice. The shell owns the state
//! behind a dialog (which tabs are closing, the disk content waiting to be applied) and
//! acts on the returned [`Answer`].

/// What a dialog shows.
pub enum DialogView {
    /// Documents with unsaved changes are about to be closed (file names, in tab order).
    Unsaved {
        names: Vec<String>,
        quitting: bool,
    },
    Message {
        title: String,
        text: String,
    },
    /// A PDF export finished (`name`: the file name).
    Exported {
        name: String,
    },
}

/// The user's choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Save the documents, then close.
    Save,
    /// Close without saving.
    Discard,
    /// Abort: nothing is closed.
    Cancel,
    /// Replace the buffer with the disk version.
    Reload,
    /// Keep the buffer; the disk version is ignored (the next save overwrites it).
    KeepMine,
    /// Dismiss a message.
    Ok,
    /// Open the exported file with the default application.
    OpenFile,
    /// Show the exported file in the file manager.
    Reveal,
}

const WIDTH: f32 = 420.0;

/// Draw the dialog; returns the answer once the user picks one.
pub fn show(ctx: &egui::Context, view: &DialogView) -> Option<Answer> {
    let mut answer = None;
    let response = egui::Modal::new(egui::Id::new("writ-dialog")).show(ctx, |ui| {
        ui.set_width(WIDTH);
        ui.spacing_mut().item_spacing.y = 10.0;
        match view {
            DialogView::Unsaved { names, quitting } => {
                let title = if names.len() == 1 {
                    format!("Save changes to “{}”?", names[0])
                } else {
                    format!("Save changes to {} documents?", names.len())
                };
                heading(ui, &title);
                if names.len() > 1 {
                    egui::ScrollArea::vertical()
                        .max_height(160.0)
                        .show(ui, |ui| {
                            for name in names {
                                ui.label(format!("•  {name}"));
                            }
                        });
                }
                ui.label(if *quitting {
                    "Your changes will be lost if you quit without saving."
                } else {
                    "Your changes will be lost if you close without saving."
                });
                let save = if names.len() == 1 { "Save" } else { "Save All" };
                let discard = if *quitting {
                    "Quit Without Saving"
                } else {
                    "Don't Save"
                };
                answer = buttons(
                    ui,
                    &[(discard, Answer::Discard)],
                    &[("Cancel", Answer::Cancel), (save, Answer::Save)],
                );
            }
            DialogView::Message { title, text } => {
                heading(ui, title);
                ui.label(text);
                answer = buttons(ui, &[], &[("OK", Answer::Ok)]);
            }
            DialogView::Exported { name } => {
                heading(ui, "PDF exported");
                ui.label(format!("“{name}” is ready."));
                answer = buttons(
                    ui,
                    &[("Show in Folder", Answer::Reveal)],
                    &[("Close", Answer::Ok), ("Open", Answer::OpenFile)],
                );
            }
        }
    });
    // Escape takes the choice that loses nothing. A click outside does nothing: the
    // dialog waits for a button.
    if answer.is_none()
        && response.is_top_modal
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        answer = Some(match view {
            DialogView::Unsaved { .. } => Answer::Cancel,
            DialogView::Message { .. } | DialogView::Exported { .. } => Answer::Ok,
        });
    }
    answer
}

/// The file of the active document changed on disk. Shown over the document area only
/// (`doc_rect`, in points): the menu and the tabs stay usable, and the question waits
/// on its tab until a button is pressed. `unsaved`: the document has unsaved changes,
/// which a reload would lose.
pub fn disk_conflict(
    ctx: &egui::Context,
    doc_rect: egui::Rect,
    name: &str,
    unsaved: bool,
) -> Option<Answer> {
    // Backdrop over the document: dims it and takes the pointer.
    egui::Area::new(egui::Id::new("writ-conflict-backdrop"))
        .order(egui::Order::Middle)
        .fixed_pos(doc_rect.min)
        .constrain(false)
        .show(ctx, |ui| {
            let (rect, _) = ui.allocate_exact_size(doc_rect.size(), egui::Sense::click_and_drag());
            ui.painter()
                .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(100));
        });
    let mut answer = None;
    egui::Area::new(egui::Id::new("writ-conflict"))
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::CENTER_CENTER)
        .fixed_pos(doc_rect.center())
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(WIDTH);
                ui.spacing_mut().item_spacing.y = 10.0;
                heading(ui, &format!("“{name}” changed on disk"));
                if unsaved {
                    ui.label(
                        "Another program modified this file, but it also has unsaved \
                         changes here. Reloading replaces your changes with the version on \
                         disk; keeping yours marks the document as modified, and the next \
                         save overwrites the disk version.",
                    );
                    answer = buttons(
                        ui,
                        &[("Reload from Disk", Answer::Reload)],
                        &[("Keep My Changes", Answer::KeepMine)],
                    );
                } else {
                    ui.label(
                        "Another program modified this file. Reload it to see the changes, \
                         or keep the current version (marked as modified: the next save \
                         overwrites the disk version).",
                    );
                    answer = buttons(
                        ui,
                        &[],
                        &[
                            ("Keep Current Version", Answer::KeepMine),
                            ("Reload", Answer::Reload),
                        ],
                    );
                }
            });
        });
    answer
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).heading().strong());
}

/// A button row: `left` aligned left, `right` aligned right. The last right button is
/// the default one: highlighted and focused, so Enter picks it.
fn buttons(ui: &mut egui::Ui, left: &[(&str, Answer)], right: &[(&str, Answer)]) -> Option<Answer> {
    let mut answer = None;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        for &(label, a) in left {
            if ui.button(label).clicked() {
                answer = Some(a);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            for (i, &(label, a)) in right.iter().rev().enumerate() {
                let default = i == 0;
                let mut button = egui::Button::new(label);
                if default {
                    button = button.fill(ui.visuals().selection.bg_fill);
                }
                let response = ui.add(button);
                if default && ui.memory(|m| m.focused().is_none()) {
                    response.request_focus();
                }
                if response.clicked() {
                    answer = Some(a);
                }
            }
        });
    });
    answer
}

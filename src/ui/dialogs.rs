//! Modal dialogs: unsaved changes on close, a file changed on disk under unsaved edits,
//! pasting an image, plain messages (errors, notices), updates, and Help → About.
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
    /// Reloading would discard the document's unsaved changes.
    ConfirmReload {
        name: String,
    },
    /// An image is being pasted into a document that was never saved.
    SaveBeforePaste,
    /// Help → About.
    About,
    /// A newer release is published. `problem`: why it can't be installed here (the
    /// download page is offered instead), or why installing it failed.
    UpdateAvailable {
        version: String,
        problem: Option<String>,
    },
    /// The update is downloading and being installed.
    UpdateInstalling {
        version: String,
    },
    /// The update is installed: restart now?
    UpdateInstalled {
        version: String,
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
    /// Open the project's website (About).
    OpenWebsite,
    /// Open the release's page (notes, downloads).
    OpenReleasePage,
    /// Download and install the update.
    Install,
    /// Restart into the installed update.
    Restart,
}

const WIDTH: f32 = 420.0;
/// The About dialog is wider: its components table has three columns.
const ABOUT_WIDTH: f32 = 520.0;

/// The project's home page, linked from About.
pub const WEBSITE: &str = "https://github.com/bitblaster/markdone";

/// The main components About lists: the name shown, the crate whose version is shown
/// (from `Cargo.lock`, see `build.rs`), and what it does here.
const COMPONENTS: &[(&str, &str, &str)] = &[
    ("egui", "egui", "Menus, dialogs and panels"),
    ("Vello", "vello", "Drawing the document on the GPU"),
    ("wgpu", "wgpu", "Graphics (Vulkan, Metal, DirectX 12)"),
    ("Parley", "parley", "Text layout and line breaking"),
    ("winit", "winit", "Windows and input"),
    (
        "tree-sitter",
        "tree-sitter",
        "Markdown parsing, code highlighting",
    ),
    ("Ropey", "ropey", "The text being edited"),
    ("pulldown-cmark", "pulldown-cmark", "HTML and PDF export"),
    ("merman", "merman", "Mermaid diagrams"),
    ("RaTeX", "ratex-render", "Math formulas"),
    (
        "Spellbook",
        "spellbook",
        "Spell checking (Hunspell dictionaries)",
    ),
    ("resvg", "resvg", "SVG images"),
    ("gitoxide", "gix", "Git differences"),
];

/// The version `build.rs` found locked for `krate`.
fn component_version(krate: &str) -> Option<&'static str> {
    env!("MARKDONE_COMPONENT_VERSIONS")
        .split(',')
        .filter_map(|pair| pair.split_once(' '))
        .find(|(name, _)| *name == krate)
        .map(|(_, version)| version)
}

/// The application icon as a texture, decoded once and kept in egui's memory.
fn app_icon(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let id = egui::Id::new("about-app-icon");
    if let Some(icon) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(icon);
    }
    let png = include_bytes!("../../packaging/icons/markdone-128.png");
    let rgba = image::load_from_memory(png).ok()?.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let pixels = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    let icon = ctx.load_texture("markdone-icon", pixels, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, icon.clone()));
    Some(icon)
}

/// Help → About: name, version, what Markdone is, its license and its main components.
fn about(ui: &mut egui::Ui) -> Option<Answer> {
    let mut answer = None;
    ui.horizontal(|ui| {
        if let Some(icon) = app_icon(ui.ctx()) {
            let size = egui::vec2(64.0, 64.0);
            ui.add(egui::Image::from_texture(egui::load::SizedTexture::new(
                icon.id(),
                size,
            )));
        }
        ui.vertical(|ui| {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Markdone").heading().strong());
            ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
        });
    });
    ui.label(
        "A fast, lightweight Markdown editor by Roberto Mozzicato (bitblaster).\nDocuments render as you type: headings, \
         lists and tasks, tables edited as a grid, links and images, code with syntax \
         highlighting, math, Mermaid diagrams, footnotes and callouts. The file stays \
         plain Markdown.",
    );
    ui.label(
        "Free software, released under the MIT License.\nMarkdone is a fork of writ, by \
         Wilfred Denton.",
    );
    if ui.link(WEBSITE.trim_start_matches("https://")).clicked() {
        answer = Some(Answer::OpenWebsite);
    }
    ui.add_space(4.0);
    ui.label(egui::RichText::new("Built with").strong());
    egui::Grid::new("about-components")
        .num_columns(3)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for &(name, krate, role) in COMPONENTS {
                ui.label(name);
                ui.weak(component_version(krate).unwrap_or(""));
                ui.label(role);
                ui.end_row();
            }
        });
    ui.weak("Each component is distributed under its own license.");
    answer.or(buttons(ui, &[], &[("Close", Answer::Ok)]))
}

/// Draw the dialog; returns the answer once the user picks one.
pub fn show(ctx: &egui::Context, view: &DialogView) -> Option<Answer> {
    let mut answer = None;
    let response = egui::Modal::new(egui::Id::new("writ-dialog")).show(ctx, |ui| {
        ui.set_width(if matches!(view, DialogView::About) {
            ABOUT_WIDTH
        } else {
            WIDTH
        });
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
            DialogView::ConfirmReload { name } => {
                heading(ui, &format!("Reload “{name}”?"));
                ui.label("The unsaved changes in this document will be lost.");
                answer = buttons(
                    ui,
                    &[],
                    &[
                        ("Cancel", Answer::Cancel),
                        ("Discard and Reload", Answer::Reload),
                    ],
                );
            }
            DialogView::SaveBeforePaste => {
                heading(ui, "Save the document first");
                ui.label(
                    "A pasted image is saved as a file next to the document and linked \
                     from it, so the document needs a location first.",
                );
                answer = buttons(
                    ui,
                    &[],
                    &[("Cancel", Answer::Cancel), ("Save As…", Answer::Save)],
                );
            }
            DialogView::About => answer = about(ui),
            DialogView::UpdateAvailable { version, problem } => {
                heading(ui, &format!("Markdone {version} is available"));
                ui.label(format!("You have version {}.", crate::update::CURRENT));
                answer = match problem {
                    Some(problem) => {
                        ui.label(problem.as_str());
                        ui.label("You can download the new version from its release page.");
                        buttons(
                            ui,
                            &[],
                            &[
                                ("Later", Answer::Cancel),
                                ("Open Download Page", Answer::OpenReleasePage),
                            ],
                        )
                    }
                    None => {
                        ui.label("Download and install it now? Markdone can then restart into it.");
                        buttons(
                            ui,
                            &[("What's New", Answer::OpenReleasePage)],
                            &[("Later", Answer::Cancel), ("Install", Answer::Install)],
                        )
                    }
                };
            }
            DialogView::UpdateInstalling { version } => {
                heading(ui, &format!("Installing Markdone {version}"));
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.label("Downloading…");
                });
            }
            DialogView::UpdateInstalled { version } => {
                heading(ui, &format!("Markdone {version} is installed"));
                ui.label("Restart Markdone to use it? Your open documents are reopened.");
                answer = buttons(
                    ui,
                    &[],
                    &[("Later", Answer::Cancel), ("Restart Now", Answer::Restart)],
                );
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
        answer = match view {
            DialogView::Unsaved { .. }
            | DialogView::ConfirmReload { .. }
            | DialogView::SaveBeforePaste
            | DialogView::UpdateAvailable { .. }
            | DialogView::UpdateInstalled { .. } => Some(Answer::Cancel),
            DialogView::Message { .. } | DialogView::Exported { .. } | DialogView::About => {
                Some(Answer::Ok)
            }
            // The download goes on; the dialog closes by itself.
            DialogView::UpdateInstalling { .. } => None,
        };
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

/// The editable state of the paste-image dialog (kept by the shell between frames).
pub struct PasteImageForm {
    /// The file name, as typed.
    pub name: String,
    /// The folder, as typed or picked with Browse.
    pub dir: String,
    /// Why the last Insert failed, shown under the fields.
    pub error: Option<String>,
    /// A folder picker opened by Browse is showing: the dialog waits for it.
    pub browsing: bool,
    /// The preview: RGBA pixels (at most [`PREVIEW_SIDE`] px a side) and their size.
    pub preview: Vec<u8>,
    pub preview_size: [usize; 2],
    /// The image's real size, shown under the preview.
    pub image_size: [u32; 2],
    texture: Option<egui::TextureHandle>,
}

/// The largest side of the preview texture, in pixels.
pub const PREVIEW_SIDE: u32 = 800;

impl PasteImageForm {
    pub fn new(
        name: String,
        dir: String,
        preview: Vec<u8>,
        preview_size: [usize; 2],
        image_size: [u32; 2],
    ) -> Self {
        Self {
            name,
            dir,
            error: None,
            browsing: false,
            preview,
            preview_size,
            image_size,
            texture: None,
        }
    }
}

/// What the user did in the paste-image dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteImageAction {
    Cancel,
    Insert,
    /// Pick the folder with the system's folder picker.
    Browse,
}

/// The dialog shown when an image is pasted: its preview, the file name and the folder
/// to save it in.
pub fn paste_image(ctx: &egui::Context, form: &mut PasteImageForm) -> Option<PasteImageAction> {
    let mut action = None;
    let texture = form
        .texture
        .get_or_insert_with(|| {
            let image = egui::ColorImage::from_rgba_unmultiplied(form.preview_size, &form.preview);
            ctx.load_texture("paste-image-preview", image, egui::TextureOptions::LINEAR)
        })
        .clone();
    let response = egui::Modal::new(egui::Id::new("paste-image-dialog")).show(ctx, |ui| {
        ui.set_width(WIDTH + 60.0);
        ui.spacing_mut().item_spacing.y = 10.0;
        heading(ui, "Paste Image");
        // The preview, scaled down to fit (never up).
        let [w, h] = form.preview_size.map(|v| v as f32);
        let scale = ((WIDTH + 60.0) / w).min(260.0 / h).min(1.0);
        ui.vertical_centered(|ui| {
            ui.add(egui::Image::new((
                texture.id(),
                egui::vec2(w * scale, h * scale),
            )));
            ui.label(
                egui::RichText::new(format!(
                    "{} × {} px",
                    form.image_size[0], form.image_size[1]
                ))
                .small()
                .weak(),
            );
        });
        egui::Grid::new("paste-image-fields")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                ui.label("File name");
                ui.add(egui::TextEdit::singleline(&mut form.name).desired_width(f32::INFINITY));
                ui.end_row();
                ui.label("Folder");
                ui.horizontal(|ui| {
                    let browse_w = 80.0;
                    ui.add(
                        egui::TextEdit::singleline(&mut form.dir)
                            .desired_width(ui.available_width() - browse_w - 8.0),
                    );
                    if ui
                        .add_enabled(!form.browsing, egui::Button::new("Browse…"))
                        .clicked()
                    {
                        action = Some(PasteImageAction::Browse);
                    }
                });
                ui.end_row();
            });
        if let Some(error) = &form.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        ui.add_enabled_ui(!form.browsing, |ui| {
            let picked = buttons(
                ui,
                &[],
                &[("Cancel", Answer::Cancel), ("Insert", Answer::Ok)],
            );
            match picked {
                Some(Answer::Ok) => action = Some(PasteImageAction::Insert),
                Some(_) => action = Some(PasteImageAction::Cancel),
                None => {}
            }
        });
    });
    if action.is_none() && !form.browsing && response.is_top_modal {
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                action = Some(PasteImageAction::Cancel);
            } else if i.consume_key(egui::Modifiers::NONE, egui::Key::Enter) {
                action = Some(PasteImageAction::Insert);
            }
        });
    }
    action
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

//! The "Select Files or Folders" screen.

use eframe::egui;
use std::path::{Path, PathBuf};

const MAX_RECENT: usize = 20;
const LABEL_WIDTH: f32 = 48.0;

/// State remembered between runs.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Persisted {
    left: String,
    right: String,
    recent: Vec<String>,
    include_subfolders: bool,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            left: String::new(),
            right: String::new(),
            recent: Vec::new(),
            include_subfolders: true,
        }
    }
}

/// What the user asked to compare.
pub enum Request {
    Files(PathBuf, PathBuf),
    Folders { left: PathBuf, right: PathBuf, include_subfolders: bool },
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Left,
    Right,
}

impl Side {
    fn name(self) -> &'static str {
        match self {
            Side::Left => "Left",
            Side::Right => "Right",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum PathKind {
    Empty,
    Missing,
    File,
    Folder,
}

fn path_kind(text: &str) -> PathKind {
    let text = text.trim();
    if text.is_empty() {
        return PathKind::Empty;
    }
    let path = Path::new(text);
    if path.is_dir() {
        PathKind::Folder
    } else if path.is_file() {
        PathKind::File
    } else {
        PathKind::Missing
    }
}

/// What the Compare button would do with the current inputs.
enum Plan {
    Files(PathBuf, PathBuf),
    Folders(PathBuf, PathBuf),
    Invalid(String),
}

impl Plan {
    fn from_inputs(left: &str, right: &str) -> Self {
        let (l, r) = (left.trim(), right.trim());
        match (path_kind(l), path_kind(r)) {
            (PathKind::Empty, _) | (_, PathKind::Empty) => {
                Plan::Invalid("Enter, browse to, or drop two files or folders.".into())
            }
            (PathKind::Missing, _) => Plan::Invalid("Left path does not exist.".into()),
            (_, PathKind::Missing) => Plan::Invalid("Right path does not exist.".into()),
            (PathKind::File, PathKind::File) => Plan::Files(l.into(), r.into()),
            (PathKind::Folder, PathKind::Folder) => Plan::Folders(l.into(), r.into()),
            // Like WinMerge: a file against a folder compares with the same-named file in that folder.
            (PathKind::File, PathKind::Folder) => match same_name_in(Path::new(l), Path::new(r)) {
                Some(r) => Plan::Files(l.into(), r),
                None => Plan::Invalid("Right folder has no file with the same name as the left file.".into()),
            },
            (PathKind::Folder, PathKind::File) => match same_name_in(Path::new(r), Path::new(l)) {
                Some(l) => Plan::Files(l, r.into()),
                None => Plan::Invalid("Left folder has no file with the same name as the right file.".into()),
            },
        }
    }

    fn is_valid(&self) -> bool {
        !matches!(self, Plan::Invalid(_))
    }
}

fn same_name_in(file: &Path, folder: &Path) -> Option<PathBuf> {
    let candidate = folder.join(file.file_name()?);
    candidate.is_file().then_some(candidate)
}

/// Starting directory for a browse dialog, based on what is typed in the field.
fn browse_start(text: &str) -> Option<PathBuf> {
    let path = Path::new(text.trim());
    if path.is_dir() {
        Some(path.to_path_buf())
    } else {
        path.parent().filter(|p| p.is_dir()).map(Path::to_path_buf)
    }
}

pub struct OpenDialog {
    state: Persisted,
    left_rect: egui::Rect,
    right_rect: egui::Rect,
}

impl OpenDialog {
    pub fn new(state: Persisted) -> Self {
        Self {
            state,
            left_rect: egui::Rect::NOTHING,
            right_rect: egui::Rect::NOTHING,
        }
    }

    pub fn state(&self) -> &Persisted {
        &self.state
    }

    pub fn set_paths(&mut self, left: &Path, right: &Path) {
        self.state.left = left.display().to_string();
        self.state.right = right.display().to_string();
    }

    fn field(&mut self, side: Side) -> &mut String {
        match side {
            Side::Left => &mut self.state.left,
            Side::Right => &mut self.state.right,
        }
    }

    fn remember(&mut self, path: &str) {
        let path = path.trim().to_string();
        self.state.recent.retain(|p| p != &path);
        self.state.recent.insert(0, path);
        self.state.recent.truncate(MAX_RECENT);
    }

    fn path_row(&mut self, ui: &mut egui::Ui, side: Side) {
        ui.horizontal(|ui| {
            ui.add_sized([LABEL_WIDTH, 20.0], egui::Label::new(format!("{}:", side.name())));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Folder…").on_hover_text("Browse for a folder").clicked() {
                    let mut dialog = rfd::FileDialog::new().set_title(format!("{} folder", side.name()));
                    if let Some(dir) = browse_start(self.field(side)) {
                        dialog = dialog.set_directory(dir);
                    }
                    if let Some(p) = dialog.pick_folder() {
                        *self.field(side) = p.display().to_string();
                    }
                }
                if ui.button("File…").on_hover_text("Browse for a file").clicked() {
                    let mut dialog = rfd::FileDialog::new().set_title(format!("{} file", side.name()));
                    if let Some(dir) = browse_start(self.field(side)) {
                        dialog = dialog.set_directory(dir);
                    }
                    if let Some(p) = dialog.pick_file() {
                        *self.field(side) = p.display().to_string();
                    }
                }
                ui.add_enabled_ui(!self.state.recent.is_empty(), |ui| {
                    ui.menu_button("⏷", |ui| {
                        let mut picked = None;
                        for recent in &self.state.recent {
                            if ui.button(recent).clicked() {
                                picked = Some(recent.clone());
                                ui.close();
                            }
                        }
                        if let Some(p) = picked {
                            *self.field(side) = p;
                        }
                    })
                    .response
                    .on_hover_text("Recent paths");
                });

                let kind = path_kind(self.field(side));
                let hint = format!("{} file or folder path", side.name());
                let edit = egui::TextEdit::singleline(self.field(side))
                    .hint_text(hint)
                    .desired_width(f32::INFINITY);
                let response = ui.add(edit).on_hover_text(match kind {
                    PathKind::Empty => "",
                    PathKind::Missing => "Path does not exist",
                    PathKind::File => "File",
                    PathKind::Folder => "Folder",
                });
                match side {
                    Side::Left => self.left_rect = response.rect,
                    Side::Right => self.right_rect = response.rect,
                }
            });
        });
    }

    /// Dropping one item fills the field under the pointer (or the first empty one);
    /// dropping two fills both.
    fn handle_drops(&mut self, ctx: &egui::Context) {
        let (dropped, pointer) = ctx.input(|i| {
            let paths: Vec<PathBuf> = i
                .raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect();
            (paths, i.pointer.latest_pos())
        });
        match dropped.as_slice() {
            [] => {}
            [one] => {
                let over = |rect: egui::Rect| pointer.is_some_and(|p| rect.expand(8.0).contains(p));
                let side = if over(self.right_rect) {
                    Side::Right
                } else if over(self.left_rect) || self.state.left.trim().is_empty() || !self.state.right.trim().is_empty() {
                    Side::Left
                } else {
                    Side::Right
                };
                *self.field(side) = one.display().to_string();
            }
            [first, second, ..] => {
                self.state.left = first.display().to_string();
                self.state.right = second.display().to_string();
            }
        }
    }

    fn submit(&mut self, plan: Plan) -> Option<Request> {
        let (left, right) = (self.state.left.clone(), self.state.right.clone());
        let request = match plan {
            Plan::Files(l, r) => Request::Files(l, r),
            Plan::Folders(l, r) => Request::Folders { left: l, right: r, include_subfolders: self.state.include_subfolders },
            Plan::Invalid(_) => return None,
        };
        self.remember(&right);
        self.remember(&left);
        Some(request)
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<Request> {
        let ctx = ui.ctx().clone();
        self.handle_drops(&ctx);
        let plan = Plan::from_inputs(&self.state.left, &self.state.right);
        let mut request = None;

        ui.add_space(4.0);
        ui.heading("Select files or folders to compare");
        ui.add_space(8.0);

        self.path_row(ui, Side::Left);
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
            if ui.small_button("Swap").on_hover_text("Swap left and right").clicked() {
                std::mem::swap(&mut self.state.left, &mut self.state.right);
            }
        });
        ui.add_space(2.0);
        self.path_row(ui, Side::Right);

        ui.add_space(8.0);
        ui.add_enabled(
            matches!(plan, Plan::Folders(..)),
            egui::Checkbox::new(&mut self.state.include_subfolders, "Include subfolders"),
        );

        ui.add_space(8.0);
        ui.separator();
        ui.horizontal(|ui| {
            let (text, color) = match &plan {
                Plan::Files(..) => ("Ready to compare two files.".to_string(), ui.visuals().text_color()),
                Plan::Folders(..) => ("Ready to compare two folders.".to_string(), ui.visuals().text_color()),
                Plan::Invalid(msg) => (msg.clone(), ui.visuals().warn_fg_color),
            };
            ui.colored_label(color, text);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let compare = ui.add_enabled(plan.is_valid(), egui::Button::new("Compare"));
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                if compare.clicked() || (enter && plan.is_valid()) {
                    request = self.submit(plan);
                }
            });
        });

        // Overlay while files are dragged over the window.
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let screen = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, "drop".into()));
            painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(160));
            painter.text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                "Drop onto a field, or drop two items to fill both",
                egui::FontId::proportional(18.0),
                egui::Color32::WHITE,
            );
        }
        request
    }
}

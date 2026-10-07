// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod compare;
mod diff;
mod folder_scan;
mod folder_view;
mod open_dialog;
mod text_file;

use compare::FileCompare;
use folder_view::{FolderAction, FolderCompare, FolderOptions};
use eframe::egui;
use open_dialog::{OpenDialog, Request};
use std::path::{Path, PathBuf};
use text_file::Loaded;

const APP_NAME: &str = "rsMerge";
const STORAGE_KEY: &str = "open_dialog";
const FOLDER_OPTIONS_KEY: &str = "folder_options";

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([520.0, 300.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(APP_NAME, options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

enum Tab {
    Compare(Box<FileCompare>),
    Folder(Box<FolderCompare>),
    Notice { title: String, text: String },
}

impl Tab {
    fn title(&self) -> String {
        match self {
            Tab::Compare(c) => c.title(),
            Tab::Folder(f) => f.title(),
            Tab::Notice { title, .. } => title.clone(),
        }
    }

    fn modified(&self) -> bool {
        matches!(self, Tab::Compare(c) if c.modified())
    }
}

/// What an "unsaved changes" prompt is asking about.
#[derive(Clone, Copy)]
enum Confirm {
    CloseTab(usize),
    Refresh(usize),
    Quit,
}

struct App {
    dialog: OpenDialog,
    /// Options for new folder comparisons (follows the last one changed).
    folder_options: FolderOptions,
    tabs: Vec<Tab>,
    /// Index into `tabs`, or `None` for the Open screen.
    active: Option<usize>,
    confirm: Option<Confirm>,
    quitting: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let state = cc.storage.and_then(|s| eframe::get_value(s, STORAGE_KEY)).unwrap_or_default();
        let folder_options = cc.storage.and_then(|s| eframe::get_value(s, FOLDER_OPTIONS_KEY)).unwrap_or_default();
        let mut app = Self {
            dialog: OpenDialog::new(state),
            folder_options,
            tabs: Vec::new(),
            active: None,
            confirm: None,
            quitting: false,
        };

        // `rsmerge LEFT RIGHT` opens two files straight away (or prefills two folders).
        let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
        if let [left, right] = args.as_slice() {
            app.dialog.set_paths(left, right);
            if left.is_file() && right.is_file() {
                app.open_file_compare(left, right);
            } else if left.is_dir() && right.is_dir() {
                app.open_folder_compare(left.clone(), right.clone(), true);
            }
        }
        app
    }

    /// Opens two files side by side in a new tab. This is the single entry point for every
    /// route into the file view (the Open screen now, folder compare later).
    pub fn open_file_compare(&mut self, left: &Path, right: &Path) {
        // Reuse a tab that already compares these two files.
        let existing = self.tabs.iter().position(|t| matches!(t, Tab::Compare(c) if c.paths() == [left, right]));
        if let Some(i) = existing {
            self.active = Some(i);
            return;
        }
        // A side that doesn't exist (a file only in one folder) opens empty; saving creates it.
        let tab = match (text_file::load_or_missing(left), text_file::load_or_missing(right)) {
            (Ok(Loaded::Text(l)), Ok(Loaded::Text(r))) => Tab::Compare(Box::new(FileCompare::new(l, r))),
            (Ok(_), Ok(_)) => {
                let same = std::fs::read(left).ok() == std::fs::read(right).ok();
                Tab::Notice {
                    title: file_name(left),
                    text: format!(
                        "Binary files {}:\n\n{}\n{}",
                        if same { "are identical" } else { "differ" },
                        left.display(),
                        right.display()
                    ),
                }
            }
            (Err(e), _) | (_, Err(e)) => Tab::Notice { title: "Error".into(), text: format!("Couldn't open files: {e}") },
        };
        self.tabs.push(tab);
        self.active = Some(self.tabs.len() - 1);
    }

    fn handle_request(&mut self, request: Request) {
        match request {
            Request::Files(left, right) => self.open_file_compare(&left, &right),
            Request::Folders { left, right, include_subfolders } => {
                self.open_folder_compare(left, right, include_subfolders);
            }
        }
    }

    fn open_folder_compare(&mut self, left: PathBuf, right: PathBuf, recursive: bool) {
        let view = FolderCompare::new(left, right, recursive, self.folder_options.clone());
        self.tabs.push(Tab::Folder(Box::new(view)));
        self.active = Some(self.tabs.len() - 1);
    }

    fn close_tab(&mut self, index: usize) {
        self.tabs.remove(index);
        self.active = match self.active {
            Some(a) if a > index => Some(a - 1),
            Some(a) if a == index => index.checked_sub(1).or((!self.tabs.is_empty()).then_some(0)),
            other => other,
        };
    }

    fn request_close_tab(&mut self, index: usize) {
        if self.tabs[index].modified() {
            self.active = Some(index);
            self.confirm = Some(Confirm::CloseTab(index));
        } else {
            self.close_tab(index);
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let mut close = None;
        ui.horizontal_wrapped(|ui| {
            if ui.selectable_label(self.active.is_none(), "Open…").clicked() {
                self.active = None;
            }
            for (i, tab) in self.tabs.iter().enumerate() {
                ui.separator();
                let mut label = tab.title();
                if label.chars().count() > 48 {
                    label = format!("{}…", label.chars().take(47).collect::<String>());
                }
                let response = ui.selectable_label(self.active == Some(i), label);
                if response.clicked() {
                    self.active = Some(i);
                }
                if response.middle_clicked() || ui.small_button("×").on_hover_text(if cfg!(target_os = "macos") { "Close (Cmd+W)" } else { "Close (Ctrl+W)" }).clicked() {
                    close = Some(i);
                }
            }
        });
        if let Some(i) = close {
            self.request_close_tab(i);
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(confirm) = self.confirm else { return };
        let message = match confirm {
            Confirm::CloseTab(i) => match &self.tabs[i] {
                Tab::Compare(c) => format!("Save changes to {}?", c.modified_names().join(" and ")),
                Tab::Folder(_) | Tab::Notice { .. } => String::new(),
            },
            Confirm::Refresh(i) => match &self.tabs[i] {
                Tab::Compare(c) => format!(
                    "Refreshing reloads both files from disk. Save changes to {} first?",
                    c.modified_names().join(" and ")
                ),
                Tab::Folder(_) | Tab::Notice { .. } => String::new(),
            },
            Confirm::Quit => "Some comparisons have unsaved changes. Save them before quitting?".to_string(),
        };
        let mut choice = None;
        egui::Modal::new(egui::Id::new("unsaved_changes")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.heading("Unsaved changes");
            ui.add_space(6.0);
            ui.label(message);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    choice = Some(true);
                }
                if ui.button("Don't Save").clicked() {
                    choice = Some(false);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.confirm = None;
                }
            });
        });
        let Some(save) = choice else { return };
        self.confirm = None;
        match confirm {
            Confirm::CloseTab(i) => {
                let saved = !save
                    || match &mut self.tabs[i] {
                        Tab::Compare(c) => c.save(),
                        Tab::Folder(_) | Tab::Notice { .. } => true,
                    };
                if saved {
                    self.close_tab(i);
                }
            }
            Confirm::Refresh(i) => {
                if let Tab::Compare(c) = &mut self.tabs[i]
                    && (!save || c.save())
                {
                    c.reload();
                }
            }
            Confirm::Quit => {
                for (i, tab) in self.tabs.iter_mut().enumerate() {
                    if save && let Tab::Compare(c) = tab
                        && !c.save()
                    {
                        // Leave the failed tab open with its error showing.
                        self.active = Some(i);
                        return;
                    }
                }
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.tabs.iter().any(Tab::modified) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::Quit);
        }

        // Ctrl+W (Cmd+W on macOS) closes the current tab, asking first if it has unsaved changes.
        if self.confirm.is_none()
            && let Some(i) = self.active
            && ctx.input_mut(|inp| inp.consume_key(egui::Modifiers::COMMAND, egui::Key::W))
        {
            self.request_close_tab(i);
        }

        egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(2.0);
            self.tab_bar(ui);
            ui.add_space(2.0);
        });

        let mut open_files = None;
        egui::CentralPanel::default().show(ui, |ui| match self.active {
            None => {
                if let Some(request) = self.dialog.ui(ui) {
                    self.handle_request(request);
                }
            }
            Some(i) => match &mut self.tabs[i] {
                Tab::Compare(view) => {
                    if ui.input(|i| i.key_pressed(egui::Key::F5)) {
                        view.request_refresh();
                    }
                    view.ui(ui);
                    if view.take_refresh_request() && self.confirm.is_none() {
                        if view.modified() {
                            self.confirm = Some(Confirm::Refresh(i));
                        } else {
                            view.reload();
                        }
                    }
                }
                Tab::Folder(view) => {
                    if ui.input(|i| i.key_pressed(egui::Key::F5)) {
                        view.rescan();
                    }
                    if let Some(FolderAction::Open(left, right)) = view.ui(ui) {
                        open_files = Some((left, right));
                    }
                    self.folder_options = view.options.clone();
                }
                Tab::Notice { text, .. } => {
                    ui.add_space(8.0);
                    ui.label(text.as_str());
                }
            },
        });

        if let Some((left, right)) = open_files {
            self.open_file_compare(&left, &right);
        }
        self.confirm_dialog(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, STORAGE_KEY, self.dialog.state());
        eframe::set_value(storage, FOLDER_OPTIONS_KEY, &self.folder_options);
    }
}

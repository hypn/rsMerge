//! Folder comparison view: a tree (or flat list) of entries with show/hide filters.

use crate::folder_scan::{self, CompareMethod, Counts, Progress, ScanOptions, Status, Tree};
use eframe::egui::{self, Color32, Key, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

const ROW_H: f32 = 20.0;
const INDENT: f32 = 16.0;

/// Display options, remembered between runs.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FolderOptions {
    pub show_identical: bool,
    pub show_different: bool,
    pub show_left_only: bool,
    pub show_right_only: bool,
    pub tree_view: bool,
    pub method: CompareMethod,
    /// Comma-separated wildcard patterns for names to skip.
    pub exclude: String,
}

impl Default for FolderOptions {
    fn default() -> Self {
        Self {
            show_identical: true,
            show_different: true,
            show_left_only: true,
            show_right_only: true,
            tree_view: true,
            method: CompareMethod::Contents,
            exclude: ".git".into(),
        }
    }
}

pub enum FolderAction {
    /// Open two files side by side (either may not exist).
    Open(PathBuf, PathBuf),
}

struct Job {
    progress: Arc<Progress>,
    handle: JoinHandle<Tree>,
    started: Instant,
}

pub struct FolderCompare {
    roots: [PathBuf; 2],
    recursive: bool,
    pub options: FolderOptions,
    exclude_edit: String,
    tree: Option<Tree>,
    job: Option<Job>,
    took: Duration,
    counts: Counts,
    index_of: HashMap<PathBuf, usize>,
    expanded: HashSet<PathBuf>,
    /// Visible node indices, in display order.
    rows: Vec<usize>,
    rows_dirty: bool,
    selected: Option<PathBuf>,
    scroll_to_selected: bool,
    /// Files opened from here, re-checked when coming back (they may have been edited).
    opened: HashSet<PathBuf>,
    last_frame: u64,
}

impl FolderCompare {
    pub fn new(left: PathBuf, right: PathBuf, recursive: bool, options: FolderOptions) -> Self {
        let mut view = Self {
            roots: [left, right],
            recursive,
            exclude_edit: options.exclude.clone(),
            options,
            tree: None,
            job: None,
            took: Duration::ZERO,
            counts: Counts::default(),
            index_of: HashMap::new(),
            expanded: HashSet::new(),
            rows: Vec::new(),
            rows_dirty: true,
            selected: None,
            scroll_to_selected: false,
            opened: HashSet::new(),
            last_frame: 0,
        };
        view.rescan();
        view
    }

    pub fn title(&self) -> String {
        let name = |p: &PathBuf| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
        format!("{} | {}", name(&self.roots[0]), name(&self.roots[1]))
    }

    /// Starts (or restarts) the comparison in the background.
    pub fn rescan(&mut self) {
        if let Some(job) = self.job.take() {
            job.progress.cancel.store(true, Ordering::Relaxed);
        }
        let progress = Arc::new(Progress::default());
        let options = ScanOptions {
            method: self.options.method,
            recursive: self.recursive,
            exclude: folder_scan::parse_patterns(&self.options.exclude),
        };
        let [left, right] = self.roots.clone();
        let p = progress.clone();
        let handle = std::thread::spawn(move || folder_scan::scan(&left, &right, &options, &p));
        self.job = Some(Job { progress, handle, started: Instant::now() });
    }

    fn poll(&mut self) {
        if !self.job.as_ref().is_some_and(|j| j.handle.is_finished()) {
            return;
        }
        let job = self.job.take().expect("checked above");
        let Ok(tree) = job.handle.join() else { return };
        self.took = job.started.elapsed();
        let first = self.tree.is_none();
        self.index_of = tree.nodes.iter().enumerate().map(|(i, n)| (n.rel.clone(), i)).collect();
        if first {
            // Start with the folders that contain differences opened up.
            self.expanded = tree
                .nodes
                .iter()
                .filter(|n| n.is_dir && n.status != Status::Identical)
                .map(|n| n.rel.clone())
                .collect();
        }
        self.counts = tree.counts();
        self.tree = Some(tree);
        self.rows_dirty = true;
    }

    fn recheck_opened(&mut self) {
        let Some(tree) = &mut self.tree else { return };
        for rel in &self.opened {
            if let Some(&i) = self.index_of.get(rel) {
                tree.recheck(i, self.options.method);
            }
        }
        self.counts = tree.counts();
        self.rows_dirty = true;
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let Some(tree) = &self.tree else { return };
        let o = &self.options;
        let mut visible = vec![false; tree.nodes.len()];
        for i in (0..tree.nodes.len()).rev() {
            let node = &tree.nodes[i];
            visible[i] = if node.is_dir {
                node.children.iter().any(|&c| visible[c])
                    || match node.status {
                        Status::LeftOnly => o.show_left_only,
                        Status::RightOnly => o.show_right_only,
                        Status::Identical => o.show_identical && node.children.is_empty(),
                        Status::NotScanned => o.show_identical || o.show_different,
                        Status::Error => o.show_different,
                        Status::Different => false,
                    }
            } else {
                match node.status {
                    Status::Identical => o.show_identical,
                    Status::Different | Status::Error => o.show_different,
                    Status::LeftOnly => o.show_left_only,
                    Status::RightOnly => o.show_right_only,
                    Status::NotScanned => true,
                }
            };
        }
        let mut stack: Vec<usize> = tree.top.iter().rev().copied().collect();
        while let Some(i) = stack.pop() {
            if !visible[i] {
                continue;
            }
            let node = &tree.nodes[i];
            let descend = node.is_dir && (!o.tree_view || self.expanded.contains(&node.rel));
            if o.tree_view || !node.is_dir || node.children.is_empty() {
                self.rows.push(i);
            }
            if descend {
                stack.extend(node.children.iter().rev());
            }
        }
        self.rows_dirty = false;
    }

    fn toggle(&mut self, index: usize) {
        let Some(tree) = &self.tree else { return };
        let rel = &tree.nodes[index].rel;
        if !self.expanded.remove(rel) {
            self.expanded.insert(rel.clone());
        }
        self.rows_dirty = true;
    }

    /// Double-click / Enter: folders expand or collapse, files open side by side.
    fn activate(&mut self, index: usize) -> Option<FolderAction> {
        let tree = self.tree.as_ref()?;
        let node = &tree.nodes[index];
        if node.is_dir {
            if self.options.tree_view {
                self.toggle(index);
            }
            return None;
        }
        self.opened.insert(node.rel.clone());
        Some(FolderAction::Open(tree.path(0, index), tree.path(1, index)))
    }

    pub fn ui(&mut self, ui: &mut Ui) -> Option<FolderAction> {
        self.poll();
        if self.job.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        let frame = ui.ctx().cumulative_frame_nr();
        if self.last_frame != 0 && frame > self.last_frame + 1 {
            self.recheck_opened();
        }
        self.last_frame = frame;

        self.toolbar(ui);
        ui.label(RichText::new(format!("Left:  {}", self.roots[0].display())).weak());
        ui.label(RichText::new(format!("Right: {}", self.roots[1].display())).weak());
        ui.add_space(2.0);
        if self.rows_dirty {
            self.rebuild_rows();
        }

        egui::Panel::bottom(ui.id().with("folder_status")).show(ui, |ui| self.status_bar(ui));

        let mut action = self.keyboard(ui);
        if let Some(a) = self.table(ui) {
            action = Some(a);
        }
        action
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        let dark = ui.visuals().dark_mode;
        let c = self.counts;
        let mut filters_changed = false;
        let mut rescan = false;
        ui.horizontal_wrapped(|ui| {
            if ui.button("Refresh").on_hover_text("Compare again (F5)").clicked() {
                rescan = true;
            }
            ui.separator();
            ui.label("Show:");
            let o = &mut self.options;
            for (flag, label, status) in [
                (&mut o.show_identical, format!("Identical ({})", c.identical), Status::Identical),
                (&mut o.show_different, format!("Different ({})", c.different), Status::Different),
                (&mut o.show_left_only, format!("Left only ({})", c.left_only), Status::LeftOnly),
                (&mut o.show_right_only, format!("Right only ({})", c.right_only), Status::RightOnly),
            ] {
                let text = RichText::new(label).color(status_color(status, dark, ui));
                filters_changed |= ui.toggle_value(flag, text).changed();
            }
            ui.separator();
            filters_changed |= ui.checkbox(&mut o.tree_view, "Tree").changed();
            let tree_view = o.tree_view;
            if ui.add_enabled(tree_view, egui::Button::new("Expand all")).clicked()
                && let Some(tree) = &self.tree
            {
                self.expanded = tree.nodes.iter().filter(|n| n.is_dir).map(|n| n.rel.clone()).collect();
                filters_changed = true;
            }
            if ui.add_enabled(tree_view, egui::Button::new("Collapse all")).clicked() {
                self.expanded.clear();
                filters_changed = true;
            }
            ui.separator();
            ui.label("Compare by:");
            let method = self.options.method;
            ui.radio_value(&mut self.options.method, CompareMethod::Contents, "Contents")
                .on_hover_text("Byte-for-byte file contents");
            ui.radio_value(&mut self.options.method, CompareMethod::SizeAndDate, "Size & date")
                .on_hover_text("Same size and modified time (faster)");
            rescan |= self.options.method != method;
            ui.separator();
            ui.label("Exclude:");
            let edit = egui::TextEdit::singleline(&mut self.exclude_edit)
                .hint_text(".git, node_modules, *.tmp")
                .desired_width(200.0);
            let response = ui.add(edit).on_hover_text("Names to skip, comma-separated. * and ? wildcards. Applied on Enter.");
            if response.lost_focus() && self.exclude_edit != self.options.exclude {
                self.options.exclude = self.exclude_edit.clone();
                rescan = true;
            }
        });
        if filters_changed {
            self.rows_dirty = true;
        }
        if rescan {
            self.rescan();
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if let Some(job) = &self.job {
                ui.spinner();
                ui.label(format!("Comparing… {} items", job.progress.items.load(Ordering::Relaxed)));
                if ui.button("Cancel").clicked() {
                    job.progress.cancel.store(true, Ordering::Relaxed);
                }
                return;
            }
            let Some(tree) = &self.tree else { return };
            let c = self.counts;
            let total = c.identical + c.different + c.left_only + c.right_only;
            ui.label(format!(
                "{total} files: {} identical, {} different, {} left only, {} right only  ·  {:.1}s",
                c.identical,
                c.different,
                c.left_only,
                c.right_only,
                self.took.as_secs_f32()
            ));
            if tree.cancelled {
                ui.colored_label(ui.visuals().warn_fg_color, "Cancelled: results are incomplete.");
            }
            let errors: Vec<&str> = tree
                .errors
                .iter()
                .map(String::as_str)
                .chain(tree.nodes.iter().filter_map(|n| n.error.as_deref()))
                .collect();
            if !errors.is_empty() {
                ui.colored_label(ui.visuals().error_fg_color, format!("{} errors", errors.len()))
                    .on_hover_text(errors.join("\n"));
            }
        });
    }

    fn keyboard(&mut self, ui: &Ui) -> Option<FolderAction> {
        if ui.memory(|m| m.focused().is_some()) || self.rows.is_empty() {
            return None;
        }
        let pos = self.selected.as_ref().and_then(|s| {
            let tree = self.tree.as_ref()?;
            self.rows.iter().position(|&i| &tree.nodes[i].rel == s)
        });
        let (up, down, enter, left, right) = ui.input(|i| {
            (
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
            )
        });
        let target = match pos {
            None if up || down => Some(0),
            Some(p) if up => Some(p.saturating_sub(1)),
            Some(p) if down => Some((p + 1).min(self.rows.len() - 1)),
            _ => None,
        };
        if let Some(t) = target {
            self.select(self.rows[t]);
            self.scroll_to_selected = true;
        }
        let index = self.rows[pos?];
        if enter {
            return self.activate(index);
        }
        if !self.options.tree_view {
            return None;
        }
        let node = &self.tree.as_ref()?.nodes[index];
        let (is_dir, parent, open) = (node.is_dir, node.parent, self.expanded.contains(&node.rel));
        if is_dir && ((left && open) || (right && !open)) {
            self.toggle(index);
        } else if left && let Some(parent) = parent {
            self.select(parent);
            self.scroll_to_selected = true;
        }
        None
    }

    fn select(&mut self, index: usize) {
        self.selected = self.tree.as_ref().map(|t| t.nodes[index].rel.clone());
    }

    fn table(&mut self, ui: &mut Ui) -> Option<FolderAction> {
        let Some(tree) = &self.tree else {
            ui.centered_and_justified(|ui| ui.spinner());
            return None;
        };
        if self.rows.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                let all_same = self.counts.different + self.counts.left_only + self.counts.right_only == 0;
                ui.label(if all_same && !tree.nodes.is_empty() {
                    "The folders are identical."
                } else {
                    "Nothing matches the current Show filters."
                });
            });
            return None;
        }

        let dark = ui.visuals().dark_mode;
        let tree_view = self.options.tree_view;
        let selected_row = self.selected.as_ref().and_then(|s| self.rows.iter().position(|&i| &tree.nodes[i].rel == s));
        let mut builder = TableBuilder::new(ui)
            .id_salt("folder_table")
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(380.0).at_least(120.0).clip(true))
            .column(Column::initial(170.0).at_least(60.0).clip(true))
            .column(Column::initial(80.0).at_least(40.0))
            .column(Column::initial(150.0).at_least(60.0))
            .column(Column::initial(80.0).at_least(40.0))
            .column(Column::remainder().at_least(60.0));
        if std::mem::take(&mut self.scroll_to_selected)
            && let Some(row) = selected_row
        {
            builder = builder.scroll_to_row(row, None);
        }

        let mut clicked = None;
        let mut activated = None;
        let mut toggled = None;
        builder
            .header(22.0, |mut header| {
                for title in ["Name", "Status", "Left size", "Left modified", "Right size", "Right modified"] {
                    header.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(ROW_H, self.rows.len(), |mut row| {
                    let index = self.rows[row.index()];
                    let node = &tree.nodes[index];
                    row.set_selected(selected_row == Some(row.index()));
                    row.col(|ui| {
                        if tree_view {
                            ui.add_space(node.depth as f32 * INDENT);
                            if node.is_dir && !node.children.is_empty() {
                                let arrow = if self.expanded.contains(&node.rel) { "⏷" } else { "⏵" };
                                if ui.add(egui::Label::new(arrow).sense(Sense::click())).clicked() {
                                    toggled = Some(index);
                                }
                            } else {
                                ui.add_space(INDENT - ui.spacing().item_spacing.x);
                            }
                        }
                        let icon = if node.is_dir { "📁" } else { "📄" };
                        let name = if tree_view { node.name.clone() } else { node.rel.display().to_string() };
                        ui.add(egui::Label::new(format!("{icon} {name}")).selectable(false).truncate());
                    });
                    row.col(|ui| {
                        let label = ui.add(
                            egui::Label::new(RichText::new(status_text(node.status, node.is_dir)).color(status_color(node.status, dark, ui)))
                                .selectable(false)
                                .truncate(),
                        );
                        if let Some(e) = &node.error {
                            label.on_hover_text(e);
                        }
                    });
                    for side in 0..2 {
                        let meta = node.sides[side].filter(|_| !node.is_dir);
                        row.col(|ui| {
                            if let Some(m) = meta {
                                ui.label(human_size(m.size));
                            }
                        });
                        row.col(|ui| {
                            if let Some(t) = meta.and_then(|m| m.modified) {
                                ui.label(format_time(t));
                            }
                        });
                    }

                    let response = row.response();
                    if response.clicked() || response.secondary_clicked() {
                        clicked = Some(index);
                    }
                    if response.double_clicked() {
                        activated = Some(index);
                    }
                    response.context_menu(|ui| {
                        if node.is_dir {
                            let open = self.expanded.contains(&node.rel);
                            if ui.add_enabled(tree_view, egui::Button::new(if open { "Collapse" } else { "Expand" })).clicked() {
                                toggled = Some(index);
                                ui.close();
                            }
                        } else if ui.button("Compare").clicked() {
                            activated = Some(index);
                            ui.close();
                        }
                    });
                });
            });

        if let Some(i) = clicked {
            self.select(i);
        }
        if let Some(i) = toggled {
            self.toggle(i);
        }
        activated.and_then(|i| self.activate(i))
    }
}

fn status_text(status: Status, is_dir: bool) -> &'static str {
    match (status, is_dir) {
        (Status::Identical, _) => "Identical",
        (Status::Different, false) => "Different",
        (Status::Different, true) => "Contains differences",
        (Status::LeftOnly, _) => "Left only",
        (Status::RightOnly, _) => "Right only",
        (Status::Error, _) => "Error",
        (Status::NotScanned, _) => "Not compared",
    }
}

fn status_color(status: Status, dark: bool, ui: &Ui) -> Color32 {
    let rgb = Color32::from_rgb;
    match (status, dark) {
        (Status::Identical | Status::NotScanned, _) => ui.visuals().weak_text_color(),
        (Status::Different, true) => rgb(235, 175, 70),
        (Status::Different, false) => rgb(185, 115, 0),
        (Status::LeftOnly, true) => rgb(240, 115, 115),
        (Status::LeftOnly, false) => rgb(195, 40, 40),
        (Status::RightOnly, true) => rgb(115, 210, 125),
        (Status::RightOnly, false) => rgb(30, 135, 50),
        (Status::Error, _) => ui.visuals().error_fg_color,
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

fn format_time(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn names(view: &FolderCompare) -> Vec<String> {
        let tree = view.tree.as_ref().unwrap();
        view.rows.iter().map(|&i| tree.nodes[i].rel.display().to_string().replace('\\', "/")).collect()
    }

    #[test]
    fn filters_and_views() {
        let base = std::env::temp_dir().join(format!("rsmerge-view-{}", std::process::id()));
        let (l, r) = (base.join("l"), base.join("r"));
        write(&l, "a/same.txt", "x");
        write(&r, "a/same.txt", "x");
        write(&l, "a/diff.txt", "1");
        write(&r, "a/diff.txt", "2");
        write(&l, "gone.txt", "g");
        write(&r, "added/new.txt", "n");

        let mut view = FolderCompare::new(l.clone(), r.clone(), true, FolderOptions::default());
        while view.tree.is_none() {
            std::thread::sleep(Duration::from_millis(5));
            view.poll();
        }
        view.rebuild_rows();
        // Folders containing differences start expanded.
        assert_eq!(names(&view), ["a", "a/diff.txt", "a/same.txt", "added", "added/new.txt", "gone.txt"]);

        view.options.show_identical = false;
        view.options.show_left_only = false;
        view.rebuild_rows();
        assert_eq!(names(&view), ["a", "a/diff.txt", "added", "added/new.txt"]);

        view.options.tree_view = false;
        view.rebuild_rows();
        assert_eq!(names(&view), ["a/diff.txt", "added/new.txt"]);

        view.options = FolderOptions { tree_view: true, ..FolderOptions::default() };
        view.expanded.clear();
        view.rebuild_rows();
        assert_eq!(names(&view), ["a", "added", "gone.txt"]);

        // Opening a right-only file gives the missing left path, and saving it later is picked up.
        let index = view.index_of[Path::new("added/new.txt")];
        let Some(FolderAction::Open(left, _)) = view.activate(index) else { panic!("expected open") };
        assert!(!left.exists());
        write(&l, "added/new.txt", "n");
        view.recheck_opened();
        let tree = view.tree.as_ref().unwrap();
        assert_eq!(tree.nodes[index].status, Status::Identical);
        assert_eq!(tree.nodes[view.index_of[Path::new("added")]].status, Status::Identical);

        std::fs::remove_dir_all(&base).unwrap();
    }
}

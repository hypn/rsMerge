//! Editable side-by-side file comparison view.
//!
//! Both panes are drawn from one [`DiffModel`], row by row, so they always share the same
//! vertical and horizontal scroll position. Every edit is a replacement of a range of lines,
//! which keeps undo/redo and "copy difference" on one code path; the diff is recomputed after
//! each edit.

use crate::diff::{DiffModel, DiffOptions, inline_diff};
use crate::text_file::{self, Loaded, TextFile};
use eframe::egui::{
    self, Color32, CursorIcon, Event, FontId, Galley, Id, Key, Modifiers, Pos2, Rect, Sense, Stroke, Ui, Vec2,
    pos2, text::CCursor, text::LayoutJob, text::TextFormat, vec2,
};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};

const TAB_WIDTH: usize = 4;
const FONT_SIZE: f32 = 13.5;
const LOCATION_W: f32 = 18.0;
const SCROLLBAR_W: f32 = 12.0;
const HEADER_H: f32 = 22.0;
const STATUS_H: f32 = 20.0;
const PANE_GAP: f32 = 4.0;
const TEXT_PAD: f32 = 4.0;

const SIDE_NAMES: [&str; 2] = ["Left", "Right"];

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Pos {
    pub line: usize,
    /// Column in chars (not bytes, not display columns).
    pub col: usize,
}

impl Pos {
    fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// A cursor parked on a filler row: typing there inserts new lines at `line`, filling the gap.
#[derive(Clone, Copy, PartialEq, Debug)]
struct FillerSlot {
    line: usize,
    row: usize,
}

struct Pane {
    file: TextFile,
    cursor: Pos,
    anchor: Pos,
    filler: Option<FillerSlot>,
    /// Column to aim for when moving up/down through shorter lines.
    want_col: Option<usize>,
    /// Number of this side's edits currently applied (undo entries).
    depth: usize,
    /// `depth` when last saved; `None` once that state can't be returned to by undo/redo.
    saved_depth: Option<usize>,
}

impl Pane {
    fn new(file: TextFile) -> Self {
        Self {
            file,
            cursor: Pos::default(),
            anchor: Pos::default(),
            filler: None,
            want_col: None,
            depth: 0,
            saved_depth: Some(0),
        }
    }

    fn line(&self, i: usize) -> &str {
        self.file.lines.get(i).map_or("", String::as_str)
    }

    /// An empty file still has one (virtual) line to put the cursor on.
    fn last_line(&self) -> usize {
        self.file.lines.len().saturating_sub(1)
    }

    fn line_len(&self, i: usize) -> usize {
        self.line(i).chars().count()
    }

    fn modified(&self) -> bool {
        self.saved_depth != Some(self.depth)
    }

    fn selection(&self) -> (Pos, Pos) {
        (self.anchor.min(self.cursor), self.anchor.max(self.cursor))
    }

    fn has_selection(&self) -> bool {
        self.anchor != self.cursor
    }

    fn clamp(&self, p: Pos) -> Pos {
        let line = p.line.min(self.last_line());
        Pos::new(line, p.col.min(self.line_len(line)))
    }

    fn text(&self, a: Pos, b: Pos) -> String {
        if a.line == b.line {
            let l = self.line(a.line);
            return l[byte_at(l, a.col)..byte_at(l, b.col)].to_string();
        }
        let first = self.line(a.line);
        let mut out = first[byte_at(first, a.col)..].to_string();
        for i in a.line + 1..b.line {
            out.push('\n');
            out.push_str(self.line(i));
        }
        let last = self.line(b.line);
        out.push('\n');
        out.push_str(&last[..byte_at(last, b.col)]);
        out
    }

    fn word_at(&self, p: Pos) -> (Pos, Pos) {
        let chars: Vec<char> = self.line(p.line).chars().collect();
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let Some(&c) = chars.get(p.col).or(chars.get(p.col.wrapping_sub(1))) else {
            return (p, p);
        };
        let same = |x: char| is_word(x) == is_word(c) && x.is_whitespace() == c.is_whitespace();
        let at = p.col.min(chars.len() - 1);
        let start = (0..=at).rev().take_while(|&i| same(chars[i])).last().unwrap_or(at);
        let end = (at..chars.len()).take_while(|&i| same(chars[i])).last().map_or(at, |i| i + 1);
        (Pos::new(p.line, start), Pos::new(p.line, end))
    }
}

fn byte_at(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map_or(s.len(), |(b, _)| b)
}

#[derive(Clone, Copy, PartialEq)]
enum EditKind {
    /// Single-line typing and deleting; consecutive ones merge into one undo step.
    Typing,
    Other,
}

/// Lines `start..start + removed.len()` were replaced by `inserted`.
struct Edit {
    side: usize,
    start: usize,
    removed: Vec<String>,
    inserted: Vec<String>,
    /// Anchor and cursor before the edit.
    before: (Pos, Pos),
    after: Pos,
    kind: EditKind,
}

struct Metrics {
    font: FontId,
    row_h: f32,
    char_w: f32,
    gutter_w: f32,
}

pub struct FileCompare {
    id: Id,
    panes: [Pane; 2],
    model: DiffModel,
    options: DiffOptions,
    inline: HashMap<usize, [Vec<Range<usize>>; 2]>,
    current: Option<usize>,
    active: usize,
    scroll: Vec2,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    reveal_cursor: bool,
    center_row: Option<usize>,
    /// Widest line in display columns, for horizontal scrolling.
    widest: usize,
    visible_rows: usize,
    message: Option<(String, bool)>,
    /// Set by the Refresh button; the app decides whether to ask about unsaved changes.
    refresh_requested: bool,
}

impl FileCompare {
    pub fn new(left: TextFile, right: TextFile) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let mut view = Self {
            id: Id::new(("file_compare", NEXT_ID.fetch_add(1, Ordering::Relaxed))),
            panes: [Pane::new(left), Pane::new(right)],
            model: DiffModel::default(),
            options: DiffOptions::default(),
            inline: HashMap::new(),
            current: None,
            active: 0,
            scroll: Vec2::ZERO,
            undo: Vec::new(),
            redo: Vec::new(),
            reveal_cursor: false,
            center_row: None,
            widest: 0,
            visible_rows: 30,
            message: None,
            refresh_requested: false,
        };
        view.rediff();
        if !view.model.blocks.is_empty() {
            view.select_diff(0);
        }
        view
    }

    pub fn title(&self) -> String {
        let name = |p: &Pane| {
            let n = p.file.path.file_name().map_or_else(|| p.file.path.display().to_string(), |n| n.to_string_lossy().into_owned());
            if p.modified() { format!("{n}*") } else { n }
        };
        format!("{} | {}", name(&self.panes[0]), name(&self.panes[1]))
    }

    pub fn modified(&self) -> bool {
        self.panes.iter().any(Pane::modified)
    }

    pub fn paths(&self) -> [&std::path::Path; 2] {
        [&self.panes[0].file.path, &self.panes[1].file.path]
    }

    /// File names of the sides with unsaved changes.
    pub fn modified_names(&self) -> Vec<String> {
        self.panes
            .iter()
            .filter(|p| p.modified())
            .map(|p| p.file.path.file_name().map_or_else(|| p.file.path.display().to_string(), |n| n.to_string_lossy().into_owned()))
            .collect()
    }

    /// Saves every modified side. Returns false (and shows the error) if any save failed.
    pub fn save(&mut self) -> bool {
        let mut saved = Vec::new();
        for (pane, name) in self.panes.iter_mut().zip(SIDE_NAMES) {
            if !pane.modified() {
                continue;
            }
            if let Err(e) = pane.file.save() {
                self.message = Some((e, true));
                return false;
            }
            pane.saved_depth = Some(pane.depth);
            saved.push(name);
        }
        if !saved.is_empty() {
            self.message = Some((format!("Saved {}.", saved.join(" and ").to_lowercase()), false));
        }
        true
    }

    /// Asks for a reload (F5 is handled by the app so it works wherever focus is).
    pub fn request_refresh(&mut self) {
        self.refresh_requested = true;
    }

    pub fn take_refresh_request(&mut self) -> bool {
        std::mem::take(&mut self.refresh_requested)
    }

    /// Reloads both files from disk, discarding edits and undo history. Keeps the scroll
    /// position and cursors where possible.
    pub fn reload(&mut self) {
        let mut files = Vec::with_capacity(2);
        for pane in &self.panes {
            let path = &pane.file.path;
            match text_file::load_or_missing(path) {
                Ok(Loaded::Text(file)) => files.push(file),
                Ok(Loaded::Binary) => {
                    self.message = Some((format!("{} is now a binary file", path.display()), true));
                    return;
                }
                Err(e) => {
                    self.message = Some((format!("Couldn't reload {}: {e}", path.display()), true));
                    return;
                }
            }
        }
        for (pane, file) in self.panes.iter_mut().zip(files) {
            let (cursor, anchor) = (pane.cursor, pane.anchor);
            *pane = Pane::new(file);
            pane.cursor = pane.clamp(cursor);
            pane.anchor = pane.clamp(anchor);
        }
        self.undo.clear();
        self.redo.clear();
        self.rediff();
        self.message = Some(("Reloaded from disk.".to_string(), false));
    }

    pub fn focus(&self, ctx: &egui::Context) {
        ctx.memory_mut(|m| m.request_focus(self.id));
    }

    // ----- Diff state -----

    fn rediff(&mut self) {
        // Rows are renumbered, so filler slots no longer point anywhere meaningful.
        for pane in &mut self.panes {
            pane.filler = None;
        }
        let [l, r] = &self.panes;
        self.model = DiffModel::compute(&l.file.lines, &r.file.lines, self.options);
        self.inline.clear();
        self.widest = self
            .panes
            .iter()
            .flat_map(|p| p.file.lines.iter())
            .map(|l| display_width(l))
            .max()
            .unwrap_or(0);
        self.sync_current();
    }

    fn cursor_row(&self, side: usize) -> usize {
        if let Some(slot) = self.panes[side].filler {
            return slot.row;
        }
        let line = self.panes[side].cursor.line;
        let rows = &self.model.row_of_line[side];
        rows.get(line).copied().unwrap_or_else(|| rows.last().map_or(0, |r| r + 1).min(self.model.rows.len().saturating_sub(1)))
    }

    /// The current difference follows the cursor of the active side.
    fn sync_current(&mut self) {
        self.current = self.model.rows.get(self.cursor_row(self.active)).and_then(|r| r.block);
    }

    fn next_diff_index(&self) -> Option<usize> {
        let target = match self.current {
            Some(c) => c + 1,
            None => {
                let row = self.cursor_row(self.active);
                self.model.blocks.iter().position(|b| b.rows.start > row)?
            }
        };
        (target < self.model.blocks.len()).then_some(target)
    }

    fn prev_diff_index(&self) -> Option<usize> {
        match self.current {
            Some(c) => c.checked_sub(1),
            None => {
                let row = self.cursor_row(self.active);
                self.model.blocks.iter().rposition(|b| b.rows.end <= row)
            }
        }
    }

    fn select_diff(&mut self, index: usize) {
        let rows = self.model.blocks[index].rows.clone();
        for side in 0..2 {
            let pos = self.pos_for_row(side, rows.start, 0);
            let filler = self.filler_slot(side, rows.start);
            let pane = &mut self.panes[side];
            pane.cursor = pos;
            pane.anchor = pos;
            pane.filler = filler;
            pane.want_col = None;
        }
        self.current = Some(index);
        self.center_row = Some(rows.start);
    }

    /// Where the cursor goes for a row on one side. A filler row puts it at the end of the
    /// side's last line before the gap, so typing Enter there adds a line into the gap.
    fn pos_for_row(&self, side: usize, row: usize, col: usize) -> Pos {
        let pane = &self.panes[side];
        let Some(r) = self.model.rows.get(row) else {
            return pane.clamp(Pos::new(usize::MAX, usize::MAX));
        };
        if let Some(line) = r.lines[side] {
            return pane.clamp(Pos::new(line, col));
        }
        let block = r.block.map(|b| &self.model.blocks[b]);
        let first_row_line = block.and_then(|b| {
            let lines = &b.lines[side];
            if lines.is_empty() { lines.start.checked_sub(1) } else { Some(lines.end - 1) }
        });
        match first_row_line {
            Some(line) => Pos::new(line, pane.line_len(line)),
            None => Pos::default(),
        }
    }

    /// The slot for typing into a difference's filler rows on one side: new lines go after
    /// the side's own lines in that difference, shown on its first filler row.
    fn filler_slot(&self, side: usize, row: usize) -> Option<FillerSlot> {
        let r = self.model.rows.get(row)?;
        if r.lines[side].is_some() {
            return None;
        }
        let block = &self.model.blocks[r.block?];
        let lines = &block.lines[side];
        Some(FillerSlot { line: lines.end, row: block.rows.start + lines.len() })
    }

    // ----- Editing -----

    fn apply(&mut self, side: usize, start: usize, remove: usize, insert: &[String]) {
        let lines = &mut self.panes[side].file.lines;
        let end = (start + remove).min(lines.len());
        lines.splice(start..end, insert.iter().cloned());
    }

    /// Replaces lines `start..end` of a side, recording it for undo.
    fn edit(&mut self, side: usize, start: usize, end: usize, new: Vec<String>, after: Pos, kind: EditKind) {
        let pane = &self.panes[side];
        let before = (pane.anchor, pane.cursor);
        let coalesce = kind == EditKind::Typing
            && pane.modified()
            && self.redo.is_empty()
            && end == start + 1
            && new.len() == 1
            && self.undo.last().is_some_and(|last| {
                last.side == side
                    && last.kind == EditKind::Typing
                    && last.start == start
                    && last.inserted.len() == 1
                    && last.after == pane.cursor
            });

        if coalesce {
            self.panes[side].file.lines[start] = new[0].clone();
            let last = self.undo.last_mut().expect("checked by coalesce");
            last.inserted = new;
            last.after = after;
        } else {
            // Discarding redo steps makes any save point beyond them unreachable.
            for pane in &mut self.panes {
                if pane.saved_depth.is_some_and(|d| d > pane.depth) {
                    pane.saved_depth = None;
                }
            }
            self.redo.clear();
            let removed = self.panes[side].file.lines[start..end.min(self.panes[side].file.lines.len())].to_vec();
            self.apply(side, start, end - start, &new);
            self.panes[side].depth += 1;
            self.undo.push(Edit { side, start, removed, inserted: new, before, after, kind });
        }

        let pane = &mut self.panes[side];
        pane.cursor = after;
        pane.anchor = after;
        pane.filler = None;
        pane.want_col = None;
        self.message = None;
        self.reveal_cursor = true;
        self.rediff();
    }

    /// Replaces the text between two positions on a side.
    fn replace_range(&mut self, side: usize, a: Pos, b: Pos, text: &str, kind: EditKind) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let pane = &self.panes[side];
        let first = pane.line(a.line);
        let last = pane.line(b.line);
        let combined = format!("{}{}{}", &first[..byte_at(first, a.col)], text, &last[byte_at(last, b.col)..]);
        let new: Vec<String> = combined.split('\n').map(String::from).collect();
        let end = (b.line + 1).min(pane.file.lines.len());
        let newlines = text.matches('\n').count();
        let after = match text.rsplit_once('\n') {
            None => Pos::new(a.line, a.col + text.chars().count()),
            Some((_, tail)) => Pos::new(a.line + newlines, tail.chars().count()),
        };
        self.edit(side, a.line, end, new, after, kind);
    }

    fn insert_text(&mut self, text: &str) {
        let pane = &self.panes[self.active];
        if let Some(slot) = pane.filler {
            // Typing on a filler row creates the line(s) there.
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            let new: Vec<String> = text.split('\n').map(String::from).collect();
            let last = new.last().map_or(0, |l| l.chars().count());
            let after = Pos::new(slot.line + new.len() - 1, last);
            let kind = if new.len() == 1 { EditKind::Typing } else { EditKind::Other };
            return self.edit(self.active, slot.line, slot.line, new, after, kind);
        }
        let (a, b) = pane.selection();
        let kind = if a == b && !text.contains(['\n', '\r']) { EditKind::Typing } else { EditKind::Other };
        self.replace_range(self.active, a, b, text, kind);
    }

    fn delete_selection(&mut self) {
        let (a, b) = self.panes[self.active].selection();
        self.replace_range(self.active, a, b, "", EditKind::Other);
    }

    fn backspace(&mut self) {
        if self.panes[self.active].filler.take().is_some() {
            return;
        }
        let pane = &self.panes[self.active];
        if pane.has_selection() {
            return self.delete_selection();
        }
        let c = pane.cursor;
        if c.col > 0 {
            self.replace_range(self.active, Pos::new(c.line, c.col - 1), c, "", EditKind::Typing);
        } else if c.line > 0 {
            let prev = Pos::new(c.line - 1, pane.line_len(c.line - 1));
            self.replace_range(self.active, prev, c, "", EditKind::Other);
        }
    }

    fn delete_forward(&mut self) {
        if self.panes[self.active].filler.take().is_some() {
            return;
        }
        let pane = &self.panes[self.active];
        if pane.has_selection() {
            return self.delete_selection();
        }
        let c = pane.cursor;
        if c.col < pane.line_len(c.line) {
            self.replace_range(self.active, c, Pos::new(c.line, c.col + 1), "", EditKind::Typing);
        } else if c.line < pane.last_line() {
            self.replace_range(self.active, c, Pos::new(c.line + 1, 0), "", EditKind::Other);
        }
    }

    fn copy_selection(&self, ctx: &egui::Context) -> bool {
        let pane = &self.panes[self.active];
        if !pane.has_selection() {
            return false;
        }
        let (a, b) = pane.selection();
        ctx.copy_text(pane.text(a, b));
        true
    }

    /// Copies the current difference from one side over the other.
    fn copy_diff(&mut self, from: usize) {
        let Some(c) = self.current else { return };
        let block = &self.model.blocks[c];
        let to = 1 - from;
        let source = self.panes[from].file.lines[block.lines[from].clone()].to_vec();
        let target = block.lines[to].clone();
        let after = self.panes[to].clamp(Pos::new(target.start, 0));
        self.edit(to, target.start, target.end, source, after, EditKind::Other);
        let after = self.panes[to].clamp(after);
        self.panes[to].cursor = after;
        self.panes[to].anchor = after;
        self.sync_current();
    }

    fn undo(&mut self) {
        let Some(e) = self.undo.pop() else { return };
        self.apply(e.side, e.start, e.inserted.len(), &e.removed);
        let pane = &mut self.panes[e.side];
        pane.depth -= 1;
        (pane.anchor, pane.cursor) = e.before;
        self.active = e.side;
        self.redo.push(e);
        self.after_history_step();
    }

    fn redo(&mut self) {
        let Some(e) = self.redo.pop() else { return };
        self.apply(e.side, e.start, e.removed.len(), &e.inserted);
        let pane = &mut self.panes[e.side];
        pane.depth += 1;
        pane.cursor = e.after;
        pane.anchor = e.after;
        self.active = e.side;
        self.undo.push(e);
        self.after_history_step();
    }

    fn after_history_step(&mut self) {
        let pane = &mut self.panes[self.active];
        pane.cursor = pane.clamp(pane.cursor);
        pane.anchor = pane.clamp(pane.anchor);
        pane.want_col = None;
        self.message = None;
        self.reveal_cursor = true;
        self.rediff();
    }

    // ----- Cursor movement -----

    fn move_to(&mut self, p: Pos, extend: bool) {
        let pane = &mut self.panes[self.active];
        pane.filler = None;
        pane.cursor = pane.clamp(p);
        if !extend {
            pane.anchor = pane.cursor;
        }
        self.reveal_cursor = true;
        self.sync_current();
    }

    fn move_vertical(&mut self, delta: isize, extend: bool) {
        let pane = &mut self.panes[self.active];
        let want = *pane.want_col.get_or_insert(pane.cursor.col);
        let line = pane.cursor.line.saturating_add_signed(delta).min(pane.last_line());
        self.move_to(Pos::new(line, want), extend);
        self.panes[self.active].want_col = Some(want);
    }

    fn move_horizontal(&mut self, forward: bool, word: bool, extend: bool) {
        let pane = &self.panes[self.active];
        if pane.has_selection() && !extend && !word {
            let (a, b) = pane.selection();
            return self.move_to(if forward { b } else { a }, false);
        }
        let c = pane.cursor;
        let target = if forward {
            if c.col >= pane.line_len(c.line) {
                if c.line < pane.last_line() { Pos::new(c.line + 1, 0) } else { c }
            } else if word {
                Pos::new(c.line, word_boundary(pane.line(c.line), c.col, true))
            } else {
                Pos::new(c.line, c.col + 1)
            }
        } else if c.col == 0 {
            if c.line > 0 { Pos::new(c.line - 1, pane.line_len(c.line - 1)) } else { c }
        } else if word {
            Pos::new(c.line, word_boundary(pane.line(c.line), c.col, false))
        } else {
            Pos::new(c.line, c.col - 1)
        };
        self.move_to(target, extend);
    }

    fn home(&mut self, extend: bool) {
        let pane = &self.panes[self.active];
        let c = pane.cursor;
        let indent = pane.line(c.line).chars().take_while(|ch| ch.is_whitespace()).count();
        let col = if c.col == indent { 0 } else { indent };
        self.move_to(Pos::new(c.line, col), extend);
    }

    // ----- Input -----

    fn handle_events(&mut self, ui: &Ui) {
        let events = ui.input(|i| i.events.clone());
        for event in events {
            match event {
                Event::Text(text) | Event::Ime(egui::ImeEvent::Commit(text)) if !text.is_empty() => {
                    self.insert_text(&text);
                }
                Event::Paste(text) if !text.is_empty() => self.insert_text(&text),
                Event::Copy => {
                    self.copy_selection(ui.ctx());
                }
                Event::Cut => {
                    if self.copy_selection(ui.ctx()) {
                        self.delete_selection();
                    }
                }
                Event::Key { key, pressed: true, modifiers, .. } => self.on_key(key, modifiers),
                _ => {}
            }
        }
    }

    fn on_key(&mut self, key: Key, m: Modifiers) {
        let mac = cfg!(target_os = "macos");
        let shift = m.shift;
        let word = if mac { m.alt && !m.mac_cmd } else { m.ctrl };
        let copy_diff = if mac { m.alt && m.mac_cmd } else { m.alt && !m.ctrl };
        let line_jump = mac && m.mac_cmd;
        let last = self.panes[self.active].last_line();
        match key {
            Key::ArrowDown if m.alt => {
                if let Some(i) = self.next_diff_index() {
                    self.select_diff(i);
                }
            }
            Key::ArrowUp if m.alt => {
                if let Some(i) = self.prev_diff_index() {
                    self.select_diff(i);
                }
            }
            Key::ArrowRight if copy_diff => self.copy_diff(0),
            Key::ArrowLeft if copy_diff => self.copy_diff(1),
            Key::ArrowLeft if line_jump => self.home(shift),
            Key::ArrowRight if line_jump => {
                let line = self.panes[self.active].cursor.line;
                self.move_to(Pos::new(line, usize::MAX), shift);
            }
            Key::ArrowUp if line_jump => self.move_to(Pos::default(), shift),
            Key::ArrowDown if line_jump => self.move_to(Pos::new(last, usize::MAX), shift),
            Key::ArrowLeft => self.move_horizontal(false, word, shift),
            Key::ArrowRight => self.move_horizontal(true, word, shift),
            Key::ArrowUp => self.move_vertical(-1, shift),
            Key::ArrowDown => self.move_vertical(1, shift),
            Key::PageUp => self.move_vertical(-(self.visible_rows as isize - 1).max(1), shift),
            Key::PageDown => self.move_vertical((self.visible_rows as isize - 1).max(1), shift),
            Key::Home if m.command => self.move_to(Pos::default(), shift),
            Key::End if m.command => self.move_to(Pos::new(last, usize::MAX), shift),
            Key::Home => self.home(shift),
            Key::End => {
                let line = self.panes[self.active].cursor.line;
                self.move_to(Pos::new(line, usize::MAX), shift);
            }
            Key::Backspace => self.backspace(),
            Key::Delete => self.delete_forward(),
            Key::Enter => self.insert_text("\n"),
            Key::Tab if !shift => self.insert_text("\t"),
            Key::A if m.command => {
                let pane = &mut self.panes[self.active];
                pane.anchor = Pos::default();
                pane.cursor = pane.clamp(Pos::new(usize::MAX, usize::MAX));
            }
            Key::Z if m.command && shift => self.redo(),
            Key::Z if m.command => self.undo(),
            Key::Y if m.command => self.redo(),
            Key::S if m.command => {
                self.save();
            }
            _ => {}
        }
    }

    // ----- UI -----

    pub fn ui(&mut self, ui: &mut Ui) {
        if ui.memory(|m| m.focused().is_none()) {
            self.focus(ui.ctx());
        }
        let focused = ui.memory(|m| m.has_focus(self.id));
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    self.id,
                    egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: false },
                );
            });
            self.handle_events(ui);
        }

        self.toolbar(ui);
        ui.add_space(4.0);

        let area = ui.available_rect_before_wrap();
        ui.allocate_rect(area, Sense::hover());
        let status = Rect::from_min_max(pos2(area.left(), area.bottom() - STATUS_H), area.max);
        let body = Rect::from_min_max(area.min, pos2(area.right(), status.top() - 2.0));

        let font = FontId::monospace(FONT_SIZE);
        let (row_h, char_w) = ui.ctx().fonts_mut(|f| (f.row_height(&font).ceil() + 2.0, f.glyph_width(&font, 'M')));
        let max_lines = self.panes.iter().map(|p| p.file.lines.len()).max().unwrap_or(0).max(1);
        let digits = max_lines.to_string().len() as f32;
        let metrics = Metrics { font, row_h, char_w, gutter_w: digits * char_w + 14.0 };

        let header_bottom = body.top() + HEADER_H;
        let loc = Rect::from_min_max(pos2(body.left(), header_bottom), pos2(body.left() + LOCATION_W, body.bottom() - SCROLLBAR_W));
        let vbar = Rect::from_min_max(pos2(body.right() - SCROLLBAR_W, header_bottom), pos2(body.right(), loc.bottom()));
        let panes_left = loc.right() + PANE_GAP;
        let pane_w = ((vbar.left() - PANE_GAP - panes_left - PANE_GAP) / 2.0).max(50.0);
        let pane_rects = [0, 1].map(|side| {
            let left = panes_left + side as f32 * (pane_w + PANE_GAP);
            Rect::from_min_max(pos2(left, header_bottom), pos2(left + pane_w, loc.bottom()))
        });
        let hbar = Rect::from_min_max(pos2(panes_left, loc.bottom()), pos2(vbar.left() - PANE_GAP, body.bottom()));

        let view_h = pane_rects[0].height();
        let text_w = pane_w - metrics.gutter_w - TEXT_PAD;
        let content_h = self.model.rows.len() as f32 * row_h;
        let content_w = (self.widest + 4) as f32 * char_w;
        self.visible_rows = (view_h / row_h).floor().max(1.0) as usize;

        // Scrolling: wheel, then pending jumps, then clamp.
        if ui.rect_contains_pointer(body) {
            self.scroll -= ui.input(|i| i.smooth_scroll_delta);
        }
        if let Some(row) = self.center_row.take() {
            self.scroll.y = row as f32 * row_h - view_h / 3.0;
            self.reveal_cursor = false;
        }
        if std::mem::take(&mut self.reveal_cursor) {
            self.reveal(&metrics, view_h, text_w);
        }
        self.clamp_scroll(content_w, content_h, text_w, view_h);

        for (side, &rect) in pane_rects.iter().enumerate() {
            self.pane_input(ui, side, rect, &metrics);
        }
        self.location_strip(ui, loc, row_h, view_h);
        let max_y = (content_h - view_h).max(0.0);
        let max_x = (content_w - text_w).max(0.0);
        scrollbar(ui, vbar, self.id.with("vbar"), true, &mut self.scroll.y, max_y, view_h / content_h.max(1.0));
        scrollbar(ui, hbar, self.id.with("hbar"), false, &mut self.scroll.x, max_x, text_w / content_w.max(1.0));
        self.clamp_scroll(content_w, content_h, text_w, view_h);

        self.compute_visible_inline(view_h, row_h);
        for (side, &rect) in pane_rects.iter().enumerate() {
            let header = Rect::from_min_max(pos2(rect.left(), body.top()), pos2(rect.right(), header_bottom - 2.0));
            self.header(ui, side, header);
            self.paint_pane(ui, side, rect, &metrics, focused);
        }
        self.status_bar(ui, status);
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        let mac = cfg!(target_os = "macos");
        let (alt, cmd) = if mac { ("Option", "Cmd") } else { ("Alt", "Ctrl") };
        let copy_mod = if mac { "Cmd+Option" } else { "Alt" };
        let mut acted = false;
        ui.horizontal(|ui| {
            let prev = self.prev_diff_index();
            if ui.add_enabled(prev.is_some(), egui::Button::new("Prev Diff")).on_hover_text(format!("{alt}+Up")).clicked() {
                self.select_diff(prev.expect("enabled only when some"));
                acted = true;
            }
            let next = self.next_diff_index();
            if ui.add_enabled(next.is_some(), egui::Button::new("Next Diff")).on_hover_text(format!("{alt}+Down")).clicked() {
                self.select_diff(next.expect("enabled only when some"));
                acted = true;
            }
            ui.separator();
            let has_current = self.current.is_some();
            if ui
                .add_enabled(has_current, egui::Button::new("Copy to Right"))
                .on_hover_text(format!("Copy the current difference from left to right ({copy_mod}+Right)"))
                .clicked()
            {
                self.copy_diff(0);
                acted = true;
            }
            if ui
                .add_enabled(has_current, egui::Button::new("Copy to Left"))
                .on_hover_text(format!("Copy the current difference from right to left ({copy_mod}+Left)"))
                .clicked()
            {
                self.copy_diff(1);
                acted = true;
            }
            ui.separator();
            if ui.add_enabled(!self.undo.is_empty(), egui::Button::new("Undo")).on_hover_text(format!("{cmd}+Z")).clicked() {
                self.undo();
                acted = true;
            }
            if ui.add_enabled(!self.redo.is_empty(), egui::Button::new("Redo")).on_hover_text(format!("{cmd}+Y")).clicked() {
                self.redo();
                acted = true;
            }
            ui.separator();
            if ui.add_enabled(self.modified(), egui::Button::new("Save")).on_hover_text(format!("Save changed sides ({cmd}+S)")).clicked() {
                self.save();
                acted = true;
            }
            if ui.button("Refresh").on_hover_text("Reload both files from disk (F5)").clicked() {
                self.refresh_requested = true;
            }
            ui.separator();
            let before = self.options;
            ui.checkbox(&mut self.options.ignore_whitespace, "Ignore whitespace");
            ui.checkbox(&mut self.options.ignore_case, "Ignore case");
            if self.options != before {
                self.rediff();
                acted = true;
            }
        });
        if acted {
            self.focus(ui.ctx());
        }
    }

    fn reveal(&mut self, m: &Metrics, view_h: f32, text_w: f32) {
        let row = self.cursor_row(self.active) as f32;
        let top = row * m.row_h;
        if top < self.scroll.y {
            self.scroll.y = top;
        } else if top + m.row_h > self.scroll.y + view_h {
            self.scroll.y = top + m.row_h - view_h;
        }
        let pane = &self.panes[self.active];
        let line = pane.line(pane.cursor.line);
        let x = display_col(line, pane.cursor.col) as f32 * m.char_w;
        let margin = 4.0 * m.char_w;
        if x < self.scroll.x + margin {
            self.scroll.x = (x - margin).max(0.0);
        } else if x > self.scroll.x + text_w - margin {
            self.scroll.x = x - text_w + margin;
        }
    }

    fn clamp_scroll(&mut self, content_w: f32, content_h: f32, view_w: f32, view_h: f32) {
        self.scroll.x = self.scroll.x.clamp(0.0, (content_w - view_w).max(0.0));
        self.scroll.y = self.scroll.y.clamp(0.0, (content_h - view_h).max(0.0));
    }

    fn row_at(&self, rect: Rect, y: f32, row_h: f32) -> usize {
        let row = ((y - rect.top() + self.scroll.y) / row_h).floor().max(0.0) as usize;
        row.min(self.model.rows.len().saturating_sub(1))
    }

    fn pos_at(&self, ui: &Ui, side: usize, rect: Rect, p: Pos2, m: &Metrics) -> Pos {
        let row = self.row_at(rect, p.y, m.row_h);
        let Some(line) = self.model.rows.get(row).and_then(|r| r.lines[side]) else {
            return self.pos_for_row(side, row, 0);
        };
        let text = self.panes[side].line(line);
        let (display, map) = expand_tabs(text);
        let galley = ui.painter().layout_no_wrap(display, m.font.clone(), Color32::WHITE);
        let x = p.x - (rect.left() + m.gutter_w + TEXT_PAD) + self.scroll.x;
        let index = galley.cursor_from_pos(vec2(x, m.row_h / 2.0)).index.0;
        Pos::new(line, col_from_display(&map, index))
    }

    fn pane_input(&mut self, ui: &mut Ui, side: usize, rect: Rect, m: &Metrics) {
        let response = ui.interact(rect, self.id.with(("pane", side)), Sense::click_and_drag());
        if response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Text);
        }
        if response.secondary_clicked()
            && let Some(pointer) = response.interact_pointer_pos()
        {
            // Right-click selects the difference under the pointer (keeping a selection
            // if the click lands inside it) so the menu acts on it.
            self.focus(ui.ctx());
            self.active = side;
            let pos = self.pos_at(ui, side, rect, pointer, m);
            let filler = self.filler_slot(side, self.row_at(rect, pointer.y, m.row_h));
            let pane = &mut self.panes[side];
            let (a, b) = pane.selection();
            if filler.is_some() || !(a <= pos && pos <= b) {
                pane.cursor = pos;
                pane.anchor = pos;
                pane.filler = filler;
            }
            self.sync_current();
        }
        response.context_menu(|ui| self.context_menu(ui));

        // A quick tap can press and release within one frame, so a click counts as a press too.
        let pressed =
            (response.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed())) || response.clicked();
        if !(pressed || response.dragged() || response.double_clicked()) {
            return;
        }
        let Some(pointer) = ui.input(|i| i.pointer.interact_pos()) else { return };
        self.focus(ui.ctx());
        self.active = side;

        if response.dragged() {
            // Scroll while dragging a selection past the edges.
            if pointer.y < rect.top() {
                self.scroll.y -= m.row_h;
            } else if pointer.y > rect.bottom() {
                self.scroll.y += m.row_h;
            }
            if pointer.x < rect.left() + m.gutter_w {
                self.scroll.x -= m.char_w * 2.0;
            } else if pointer.x > rect.right() {
                self.scroll.x += m.char_w * 2.0;
            }
            ui.ctx().request_repaint();
        }

        let pos = self.pos_at(ui, side, rect, pointer, m);
        let shift = ui.input(|i| i.modifiers.shift);
        let filler = if pressed && !shift { self.filler_slot(side, self.row_at(rect, pointer.y, m.row_h)) } else { None };
        let pane = &mut self.panes[side];
        pane.filler = None;
        if let Some(slot) = filler {
            pane.cursor = pos;
            pane.anchor = pos;
            pane.filler = Some(slot);
        } else if response.double_clicked() {
            (pane.anchor, pane.cursor) = pane.word_at(pos);
        } else {
            pane.cursor = pos;
            if pressed && !shift {
                pane.anchor = pos;
            }
        }
        pane.want_col = None;
        self.sync_current();
    }

    fn context_menu(&mut self, ui: &mut Ui) {
        let mac = cfg!(target_os = "macos");
        let copy_mod = if mac { "Cmd+Option" } else { "Alt" };
        let cmd = if mac { "Cmd" } else { "Ctrl" };
        let has_diff = self.current.is_some();
        let mut acted = false;
        if ui.add_enabled(has_diff, egui::Button::new("Copy to Right").shortcut_text(format!("{copy_mod}+Right"))).clicked() {
            self.copy_diff(0);
            acted = true;
        }
        if ui.add_enabled(has_diff, egui::Button::new("Copy to Left").shortcut_text(format!("{copy_mod}+Left"))).clicked() {
            self.copy_diff(1);
            acted = true;
        }
        ui.separator();
        let has_selection = self.panes[self.active].has_selection();
        if ui.add_enabled(has_selection, egui::Button::new("Copy").shortcut_text(format!("{cmd}+C"))).clicked() {
            self.copy_selection(ui.ctx());
            acted = true;
        }
        if acted {
            ui.close();
            self.focus(ui.ctx());
        }
    }

    fn compute_visible_inline(&mut self, view_h: f32, row_h: f32) {
        let first = (self.scroll.y / row_h).floor() as usize;
        let last = (((self.scroll.y + view_h) / row_h).ceil() as usize + 1).min(self.model.rows.len());
        for row in first..last {
            let r = self.model.rows[row];
            if let (Some(_), [Some(l), Some(rr)]) = (r.block, r.lines) {
                self.inline
                    .entry(row)
                    .or_insert_with(|| inline_diff(self.panes[0].line(l), self.panes[1].line(rr)));
            }
        }
    }

    fn header(&self, ui: &Ui, side: usize, rect: Rect) {
        let visuals = ui.visuals();
        let painter = ui.painter_at(rect);
        let active = side == self.active;
        painter.rect_filled(rect, 3.0, if active { visuals.widgets.active.weak_bg_fill } else { visuals.faint_bg_color });
        let pane = &self.panes[side];
        let info = format!("{} · {}", pane.file.encoding_name(), pane.file.eol.name());
        let info_galley = painter.layout_no_wrap(info, FontId::proportional(12.0), visuals.weak_text_color());
        let info_pos = pos2(rect.right() - info_galley.size().x - 6.0, rect.center().y - info_galley.size().y / 2.0);
        let state = match (pane.file.exists, pane.modified()) {
            (false, _) => "  (missing: saving creates it)",
            (true, true) => " *",
            (true, false) => "",
        };
        let path = format!("{}{state}", pane.file.path.display());
        let max_w = info_pos.x - rect.left() - 14.0;
        let font = FontId::proportional(13.0);
        let mut galley = painter.layout_no_wrap(path.clone(), font.clone(), visuals.strong_text_color());
        let mut skip = 0;
        while galley.size().x > max_w && skip < path.len() {
            skip = path.ceil_char_boundary(skip + 1);
            galley = painter.layout_no_wrap(format!("…{}", &path[skip..]), font.clone(), visuals.strong_text_color());
        }
        painter.galley(pos2(rect.left() + 6.0, rect.center().y - galley.size().y / 2.0), galley, visuals.strong_text_color());
        painter.galley(info_pos, info_galley, visuals.weak_text_color());
    }

    fn paint_pane(&self, ui: &Ui, side: usize, rect: Rect, m: &Metrics, focused: bool) {
        let colors = Colors::new(ui.visuals().dark_mode);
        let visuals = ui.visuals();
        let painter = ui.painter_at(rect);
        let gutter = Rect::from_min_max(rect.min, pos2(rect.left() + m.gutter_w, rect.bottom()));
        painter.rect_filled(rect, 0.0, visuals.extreme_bg_color);
        painter.rect_filled(gutter, 0.0, visuals.faint_bg_color);
        let text_left = gutter.right() + TEXT_PAD;
        let text_painter = painter.with_clip_rect(Rect::from_min_max(pos2(gutter.right(), rect.top()), rect.max));

        let pane = &self.panes[side];
        let (sel_a, sel_b) = pane.selection();
        let show_caret = focused && side == self.active;
        let caret_row = self.cursor_row(side);
        let first = (self.scroll.y / m.row_h).floor() as usize;
        let last = (((self.scroll.y + rect.height()) / m.row_h).ceil() as usize + 1).min(self.model.rows.len());

        for row in first..last {
            let r = self.model.rows[row];
            let y = rect.top() + row as f32 * m.row_h - self.scroll.y;
            let row_rect = Rect::from_min_max(pos2(gutter.right(), y), pos2(rect.right(), y + m.row_h));
            let current = r.block.is_some() && r.block == self.current;
            if r.block.is_some() {
                let fill = match (r.lines[side].is_some(), current) {
                    (true, false) => colors.diff[side],
                    (true, true) => colors.diff_current[side],
                    (false, false) => colors.filler,
                    (false, true) => colors.filler_current,
                };
                painter.rect_filled(row_rect, 0.0, fill);
            }

            let Some(line) = r.lines[side] else { continue };
            painter.text(
                pos2(gutter.right() - 6.0, y + m.row_h / 2.0),
                egui::Align2::RIGHT_CENTER,
                (line + 1).to_string(),
                m.font.clone(),
                visuals.weak_text_color(),
            );

            let text = pane.line(line);
            let (display, map) = expand_tabs(text);
            let ranges = self.inline.get(&row).map_or(&[][..], |r| &r[side][..]);
            let word_bg = if current { colors.word_current[side] } else { colors.word[side] };
            let job = line_job(&display, &map, ranges, &m.font, visuals.text_color(), visuals.strong_text_color(), word_bg);
            let galley = ui.painter().layout_job(job);
            let origin = pos2(text_left - self.scroll.x, y + (m.row_h - galley.size().y) / 2.0);

            if sel_a != sel_b && (sel_a.line..=sel_b.line).contains(&line) {
                let start = if line == sel_a.line { sel_a.col } else { 0 };
                let end = if line == sel_b.line { sel_b.col } else { pane.line_len(line) };
                let x0 = x_of(&galley, &map, start);
                let mut x1 = x_of(&galley, &map, end);
                if line != sel_b.line {
                    x1 += m.char_w / 2.0;
                }
                let mut fill = visuals.selection.bg_fill;
                if !show_caret {
                    fill = fill.gamma_multiply(0.5);
                }
                text_painter.rect_filled(
                    Rect::from_min_max(pos2(origin.x + x0, y), pos2(origin.x + x1, y + m.row_h)),
                    0.0,
                    fill,
                );
            }
            text_painter.galley(origin, galley.clone(), visuals.text_color());

            if show_caret && row == caret_row && line == pane.cursor.line {
                let x = origin.x + x_of(&galley, &map, pane.cursor.col);
                self.paint_caret(ui, &text_painter, rect, x, y, m.row_h);
            }
        }

        // Caret on a filler row or in an empty file.
        if show_caret && self.model.rows.get(caret_row).is_none_or(|r| r.lines[side] != Some(pane.cursor.line)) {
            let y = rect.top() + caret_row as f32 * m.row_h - self.scroll.y;
            self.paint_caret(ui, &text_painter, rect, text_left - self.scroll.x, y, m.row_h);
        }
    }

    fn paint_caret(&self, ui: &Ui, painter: &egui::Painter, rect: Rect, x: f32, y: f32, row_h: f32) {
        let caret = Rect::from_min_max(pos2(x, y + 1.0), pos2(x + 2.0, y + row_h - 1.0));
        painter.rect_filled(caret, 0.0, ui.visuals().text_cursor.stroke.color);
        ui.ctx().output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                purpose: Default::default(),
                rect,
                cursor_rect: caret,
                should_interrupt_composition: false,
            });
        });
    }

    /// Overview of all differences on the left edge; click or drag to jump.
    fn location_strip(&mut self, ui: &mut Ui, rect: Rect, row_h: f32, view_h: f32) {
        let colors = Colors::new(ui.visuals().dark_mode);
        let response = ui.interact(rect, self.id.with("location"), Sense::click_and_drag());
        let total = self.model.rows.len().max(1) as f32;
        let scale = rect.height() / total;
        if (response.clicked() || response.dragged())
            && let Some(p) = response.interact_pointer_pos()
        {
            let row = ((p.y - rect.top()) / scale).max(0.0);
            self.scroll.y = row * row_h - view_h / 2.0;
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, ui.visuals().faint_bg_color);
        let half = (rect.width() - 4.0) / 2.0;
        for (i, block) in self.model.blocks.iter().enumerate() {
            let y0 = rect.top() + block.rows.start as f32 * scale;
            let y1 = (rect.top() + block.rows.end as f32 * scale).max(y0 + 2.0);
            let current = self.current == Some(i);
            for side in 0..2 {
                let x0 = rect.left() + 1.0 + side as f32 * (half + 2.0);
                let fill = match (block.lines[side].is_empty(), current) {
                    (false, false) => colors.word[side],
                    (false, true) => colors.word_current[side],
                    (true, false) => colors.filler_mark,
                    (true, true) => colors.filler_current,
                };
                painter.rect_filled(Rect::from_min_max(pos2(x0, y0), pos2(x0 + half, y1)), 0.0, fill);
            }
        }
        let v0 = rect.top() + self.scroll.y / row_h * scale;
        let v1 = (rect.top() + (self.scroll.y + view_h) / row_h * scale).min(rect.bottom());
        painter.rect_stroke(
            Rect::from_min_max(pos2(rect.left(), v0), pos2(rect.right(), v1.max(v0 + 4.0))),
            0.0,
            Stroke::new(1.0, ui.visuals().strong_text_color().gamma_multiply(0.6)),
            egui::StrokeKind::Inside,
        );
    }

    fn status_bar(&self, ui: &Ui, rect: Rect) {
        let visuals = ui.visuals();
        let painter = ui.painter_at(rect);
        let pane = &self.panes[self.active];
        let pos = format!("{}  Ln {}, Col {}", SIDE_NAMES[self.active], pane.cursor.line + 1, pane.cursor.col + 1);
        painter.text(pos2(rect.left() + 4.0, rect.center().y), egui::Align2::LEFT_CENTER, pos, FontId::proportional(13.0), visuals.text_color());

        let n = self.model.blocks.len();
        let diffs = match (n, self.current) {
            (0, _) if self.options != DiffOptions::default() => "No differences (with ignore options)".to_string(),
            (0, _) => "Files are identical".to_string(),
            (_, Some(c)) => format!("Difference {} of {n}", c + 1),
            (1, None) => "1 difference".to_string(),
            (_, None) => format!("{n} differences"),
        };
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, diffs, FontId::proportional(13.0), visuals.text_color());

        if let Some((msg, error)) = &self.message {
            let color = if *error { visuals.error_fg_color } else { visuals.weak_text_color() };
            painter.text(pos2(rect.right() - 4.0, rect.center().y), egui::Align2::RIGHT_CENTER, msg, FontId::proportional(13.0), color);
        }
    }
}

/// Left side reads as removed (red), right as added (green); filler rows are grey.
struct Colors {
    diff: [Color32; 2],
    diff_current: [Color32; 2],
    word: [Color32; 2],
    word_current: [Color32; 2],
    filler: Color32,
    filler_current: Color32,
    filler_mark: Color32,
}

impl Colors {
    fn new(dark: bool) -> Self {
        let rgb = Color32::from_rgb;
        if dark {
            Self {
                diff: [rgb(64, 28, 34), rgb(24, 58, 36)],
                diff_current: [rgb(96, 34, 44), rgb(28, 86, 48)],
                word: [rgb(140, 48, 60), rgb(40, 122, 62)],
                word_current: [rgb(178, 58, 74), rgb(46, 156, 76)],
                filler: rgb(40, 40, 44),
                filler_current: rgb(58, 58, 64),
                filler_mark: rgb(90, 90, 96),
            }
        } else {
            Self {
                diff: [rgb(255, 232, 232), rgb(226, 248, 228)],
                diff_current: [rgb(255, 208, 208), rgb(196, 238, 200)],
                word: [rgb(250, 168, 168), rgb(150, 222, 158)],
                word_current: [rgb(240, 130, 130), rgb(110, 205, 122)],
                filler: rgb(232, 232, 232),
                filler_current: rgb(212, 212, 212),
                filler_mark: rgb(180, 180, 180),
            }
        }
    }
}

/// Draws a scrollbar along `rect`; `visible` is the fraction of the content in view.
fn scrollbar(ui: &mut Ui, rect: Rect, id: Id, vertical: bool, offset: &mut f32, max: f32, visible: f32) {
    let visuals = ui.visuals().clone();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, visuals.faint_bg_color);
    if max <= 0.0 {
        return;
    }
    let along = |r: Rect| if vertical { (r.top(), r.height()) } else { (r.left(), r.width()) };
    let (start, len) = along(rect);
    let thumb_len = (visible * len).clamp(20.0_f32.min(len), len);
    let travel = (len - thumb_len).max(1.0);
    let thumb_start = start + *offset / max * travel;
    let thumb = if vertical {
        Rect::from_min_max(pos2(rect.left() + 2.0, thumb_start), pos2(rect.right() - 2.0, thumb_start + thumb_len))
    } else {
        Rect::from_min_max(pos2(thumb_start, rect.top() + 2.0), pos2(thumb_start + thumb_len, rect.bottom() - 2.0))
    };

    let response = ui.interact(rect, id, Sense::click_and_drag());
    if response.dragged() {
        let d = response.drag_delta();
        *offset += (if vertical { d.y } else { d.x }) * max / travel;
    } else if response.clicked()
        && let Some(p) = response.interact_pointer_pos()
        && !thumb.contains(p)
    {
        let at = if vertical { p.y } else { p.x };
        *offset = (at - start - thumb_len / 2.0) / travel * max;
    }
    *offset = offset.clamp(0.0, max);

    let style = if response.dragged() {
        &visuals.widgets.active
    } else if response.hovered() {
        &visuals.widgets.hovered
    } else {
        &visuals.widgets.inactive
    };
    painter.rect_filled(thumb, 4.0, style.bg_fill);
}

/// Expands tabs to spaces and makes control characters visible. Returns the display
/// string and, for each char column (plus one past the end), its display column.
fn expand_tabs(line: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(line.len());
    let mut map = Vec::with_capacity(line.len() + 1);
    let mut col = 0;
    for c in line.chars() {
        map.push(col);
        if c == '\t' {
            let n = TAB_WIDTH - col % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(if c.is_control() { '·' } else { c });
            col += 1;
        }
    }
    map.push(col);
    (out, map)
}

fn display_width(line: &str) -> usize {
    display_col(line, usize::MAX)
}

fn display_col(line: &str, col: usize) -> usize {
    let mut width = 0;
    for c in line.chars().take(col) {
        width += if c == '\t' { TAB_WIDTH - width % TAB_WIDTH } else { 1 };
    }
    width
}

/// Maps a display column back to the nearest char column.
fn col_from_display(map: &[usize], display: usize) -> usize {
    match map.binary_search(&display) {
        Ok(col) => col,
        Err(0) => 0,
        Err(col) if col >= map.len() => map.len() - 1,
        Err(col) => {
            if display - map[col - 1] <= map[col] - display { col - 1 } else { col }
        }
    }
}

fn x_of(galley: &Galley, map: &[usize], col: usize) -> f32 {
    let display = map[col.min(map.len() - 1)];
    galley.pos_from_cursor(CCursor::new(display)).min.x
}

fn line_job(
    display: &str,
    map: &[usize],
    ranges: &[Range<usize>],
    font: &FontId,
    color: Color32,
    marked_color: Color32,
    highlight: Color32,
) -> LayoutJob {
    let bytes: Vec<usize> = display.char_indices().map(|(b, _)| b).chain([display.len()]).collect();
    let plain = TextFormat::simple(font.clone(), color);
    let marked = TextFormat { background: highlight, ..TextFormat::simple(font.clone(), marked_color) };
    let mut job = LayoutJob::default();
    let mut at = 0;
    for r in ranges {
        let start = map[r.start.min(map.len() - 1)];
        let end = map[r.end.min(map.len() - 1)];
        if start > at {
            job.append(&display[bytes[at]..bytes[start]], 0.0, plain.clone());
        }
        if end > start {
            job.append(&display[bytes[start]..bytes[end]], 0.0, marked.clone());
        }
        at = at.max(end);
    }
    job.append(&display[bytes[at]..], 0.0, plain);
    job
}

fn word_boundary(line: &str, col: usize, forward: bool) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let class = |c: char| {
        if c.is_whitespace() {
            0
        } else if c.is_alphanumeric() || c == '_' {
            1
        } else {
            2
        }
    };
    let mut i = col;
    if forward {
        while i < chars.len() && class(chars[i]) == 0 {
            i += 1;
        }
        if let Some(&c) = chars.get(i) {
            let k = class(c);
            while i < chars.len() && class(chars[i]) == k {
                i += 1;
            }
        }
    } else {
        while i > 0 && class(chars[i - 1]) == 0 {
            i -= 1;
        }
        if i > 0 {
            let k = class(chars[i - 1]);
            while i > 0 && class(chars[i - 1]) == k {
                i -= 1;
            }
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_file::Eol;

    fn file(text: &str) -> TextFile {
        TextFile {
            path: "test.txt".into(),
            lines: text.lines().map(String::from).collect(),
            encoding: encoding_rs::UTF_8,
            bom: false,
            eol: Eol::Lf,
            final_newline: true,
            exists: true,
        }
    }

    fn lines(view: &FileCompare, side: usize) -> Vec<&str> {
        view.panes[side].file.lines.iter().map(String::as_str).collect()
    }

    #[test]
    fn typing_coalesces_and_undoes() {
        let mut v = FileCompare::new(file("abc"), file("abc"));
        v.panes[0].cursor = Pos::new(0, 3);
        v.panes[0].anchor = v.panes[0].cursor;
        v.insert_text("x");
        v.insert_text("y");
        assert_eq!(lines(&v, 0), ["abcxy"]);
        assert_eq!(v.undo.len(), 1);
        assert!(v.modified());
        v.undo();
        assert_eq!(lines(&v, 0), ["abc"]);
        assert!(!v.modified());
        v.redo();
        assert_eq!(lines(&v, 0), ["abcxy"]);
    }

    #[test]
    fn newline_and_backspace_join() {
        let mut v = FileCompare::new(file("ab"), file("ab"));
        v.panes[0].cursor = Pos::new(0, 1);
        v.panes[0].anchor = v.panes[0].cursor;
        v.insert_text("\n");
        assert_eq!(lines(&v, 0), ["a", "b"]);
        assert_eq!(v.panes[0].cursor, Pos::new(1, 0));
        v.backspace();
        assert_eq!(lines(&v, 0), ["ab"]);
    }

    #[test]
    fn copy_diff_both_ways() {
        let mut v = FileCompare::new(file("a\nb\nc"), file("a\nx\ny\nc"));
        assert_eq!(v.current, Some(0));
        v.copy_diff(0);
        assert_eq!(lines(&v, 1), ["a", "b", "c"]);
        assert!(v.model.blocks.is_empty());
        v.undo();
        assert_eq!(lines(&v, 1), ["a", "x", "y", "c"]);
        v.select_diff(0);
        v.copy_diff(1);
        assert_eq!(lines(&v, 0), ["a", "x", "y", "c"]);
    }

    #[test]
    fn typing_into_empty_file() {
        let mut v = FileCompare::new(file(""), file("hello"));
        v.active = 0;
        v.insert_text("h");
        v.insert_text("ello");
        assert_eq!(lines(&v, 0), ["hello"]);
        assert!(v.model.blocks.is_empty());
    }

    #[test]
    fn typing_into_filler_gap() {
        let mut v = FileCompare::new(file("a\nc"), file("a\nb1\nb2\nc"));
        // The first (and only) difference is right-only, so the left side is parked in the gap.
        assert_eq!(v.panes[0].filler, Some(FillerSlot { line: 1, row: 1 }));
        v.active = 0;
        v.insert_text("/");
        v.insert_text("/ note");
        assert_eq!(lines(&v, 0), ["a", "// note", "c"]);
        assert_eq!(v.panes[0].cursor, Pos::new(1, 7));
        assert_eq!(v.undo.len(), 1);
        v.undo();
        assert_eq!(lines(&v, 0), ["a", "c"]);
    }

    #[test]
    fn reload_discards_edits() {
        let dir = std::env::temp_dir().join(format!("rsmerge-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (lp, rp) = (dir.join("l.txt"), dir.join("r.txt"));
        std::fs::write(&lp, "one\n").unwrap();
        std::fs::write(&rp, "two\n").unwrap();
        let load = |p: &std::path::Path| match text_file::load(p).unwrap() {
            Loaded::Text(f) => f,
            Loaded::Binary => panic!("binary"),
        };
        let mut v = FileCompare::new(load(&lp), load(&rp));
        v.insert_text("x");
        assert!(v.modified());
        std::fs::write(&rp, "one\n").unwrap();
        v.reload();
        assert!(!v.modified());
        assert!(v.undo.is_empty());
        assert!(v.model.blocks.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tab_display_mapping() {
        let (display, map) = expand_tabs("a\tb");
        assert_eq!(display, "a   b");
        assert_eq!(map, vec![0, 1, 4, 5]);
        assert_eq!(col_from_display(&map, 3), 2);
        assert_eq!(col_from_display(&map, 2), 1);
    }
}

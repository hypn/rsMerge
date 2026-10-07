//! Read-only image comparison view: two panes (or one blended overlay) sharing zoom and pan, with
//! differing pixels tinted and Prev/Next stepping through regions of change.

use crate::image_diff::{self, Image, ImageDiff};
use eframe::egui::{
    self, Color32, ColorImage, FontId, Id, Key, Modifiers, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle,
    TextureOptions, Ui, Vec2, pos2, vec2,
};
use std::path::{Path, PathBuf};

const HEADER_H: f32 = 22.0;
const STATUS_H: f32 = 20.0;
const PANE_GAP: f32 = 4.0;
const MIN_ZOOM: f32 = 0.01;
const MAX_ZOOM: f32 = 64.0;
/// Screen size of a transparency checkerboard square.
const CHECKER: f32 = 8.0;

const SIDE_NAMES: [&str; 2] = ["Left", "Right"];

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    SideBySide,
    /// Right drawn over left with adjustable opacity.
    Overlay,
}

/// GPU copies of the images and diff mask, possibly shrunk by `scale` to fit the texture limit.
struct Textures {
    images: [Option<TextureHandle>; 2],
    mask: Option<TextureHandle>,
    /// Image pixels covered by the mask texture (a multiple of `scale`, may overhang).
    mask_size: Vec2,
    checker: TextureHandle,
}

pub struct ImageCompare {
    id: Id,
    paths: [PathBuf; 2],
    images: [Option<Image>; 2],
    errors: [Option<String>; 2],
    diff: ImageDiff,
    threshold: u8,
    textures: Option<Textures>,
    mode: Mode,
    /// Opacity of the right image in overlay mode.
    blend: f32,
    show_diff: bool,
    /// Screen points per image pixel; `None` fits the whole image in the pane.
    zoom: Option<f32>,
    /// Image position shown at the centre of each pane.
    center: Pos2,
    pane_size: Vec2,
    current: Option<usize>,
    hover: Option<(usize, usize)>,
    message: Option<(String, bool)>,
    refresh_requested: bool,
}

impl ImageCompare {
    pub fn new(left: &Path, right: &Path) -> Self {
        let mut view = Self {
            id: Id::new(("image_compare", left, right)),
            paths: [left.to_path_buf(), right.to_path_buf()],
            images: [None, None],
            errors: [None, None],
            diff: image_diff::compare(None, None, 0),
            threshold: 0,
            textures: None,
            mode: Mode::SideBySide,
            blend: 0.5,
            show_diff: true,
            zoom: None,
            center: Pos2::ZERO,
            pane_size: vec2(400.0, 400.0),
            current: None,
            hover: None,
            message: None,
            refresh_requested: false,
        };
        view.load();
        view
    }

    fn load(&mut self) {
        for side in 0..2 {
            (self.images[side], self.errors[side]) = match Image::load(&self.paths[side]) {
                Ok(img) => (img, None),
                Err(e) => (None, Some(e)),
            };
        }
        self.rediff();
    }

    fn rediff(&mut self) {
        self.diff = image_diff::compare(self.images[0].as_ref(), self.images[1].as_ref(), self.threshold);
        self.textures = None;
        self.current = None;
    }

    pub fn title(&self) -> String {
        format!("{} | {}", file_name(&self.paths[0]), file_name(&self.paths[1]))
    }

    pub fn paths(&self) -> [&Path; 2] {
        [&self.paths[0], &self.paths[1]]
    }

    pub fn request_refresh(&mut self) {
        self.refresh_requested = true;
    }

    /// Reloads both images from disk, keeping zoom and position.
    fn reload(&mut self) {
        self.load();
        self.message = Some(("Reloaded from disk.".to_string(), false));
    }

    fn select_diff(&mut self, i: usize) {
        let r = self.diff.regions[i];
        self.current = Some(i);
        self.center = pos2(r.x as f32 + r.w as f32 / 2.0, r.y as f32 + r.h as f32 / 2.0);
        // Keep the zoom unless the region wouldn't fit, then zoom out just enough.
        let fits = (self.pane_size / vec2(r.w as f32, r.h as f32)).min_elem() * 0.9;
        self.zoom = Some(self.effective_zoom().min(fits).clamp(MIN_ZOOM, MAX_ZOOM));
    }

    fn effective_zoom(&self) -> f32 {
        self.zoom.unwrap_or_else(|| {
            let size = vec2(self.diff.width.max(1) as f32, self.diff.height.max(1) as f32);
            ((self.pane_size - Vec2::splat(16.0)) / size).min_elem().clamp(MIN_ZOOM, MAX_ZOOM)
        })
    }

    fn content_center(&self) -> Pos2 {
        pos2(self.diff.width as f32 / 2.0, self.diff.height as f32 / 2.0)
    }

    // ----- UI -----

    pub fn ui(&mut self, ui: &mut Ui) {
        if std::mem::take(&mut self.refresh_requested) {
            self.reload();
        }
        self.handle_keys(ui);
        self.toolbar(ui);
        ui.add_space(4.0);
        if self.textures.is_none() {
            self.textures = Some(self.build_textures(ui.ctx()));
        }

        let area = ui.available_rect_before_wrap();
        ui.allocate_rect(area, Sense::hover());
        let status = Rect::from_min_max(pos2(area.left(), area.bottom() - STATUS_H), area.max);
        let body = Rect::from_min_max(pos2(area.left(), area.top() + HEADER_H), pos2(area.right(), status.top() - 2.0));
        let panes: Vec<Rect> = match self.mode {
            Mode::Overlay => vec![body],
            Mode::SideBySide => {
                let w = ((body.width() - PANE_GAP) / 2.0).max(50.0);
                (0..2)
                    .map(|s| Rect::from_min_size(pos2(body.left() + s as f32 * (w + PANE_GAP), body.top()), vec2(w, body.height())))
                    .collect()
            }
        };
        self.pane_size = panes[0].size();

        self.pan_and_zoom(ui, body, &panes);
        if self.zoom.is_none() {
            self.center = self.content_center();
        }

        for (i, &rect) in panes.iter().enumerate() {
            let header = Rect::from_min_max(pos2(rect.left(), area.top()), pos2(rect.right(), body.top() - 2.0));
            match self.mode {
                Mode::SideBySide => {
                    self.header(ui, i, header);
                    self.paint_pane(ui, rect, &[(i, 1.0)], i);
                }
                Mode::Overlay => {
                    self.overlay_header(ui, header);
                    self.paint_pane(ui, rect, &[(0, 1.0), (1, self.blend)], 2);
                }
            }
        }
        self.status_bar(ui, status);
    }

    fn handle_keys(&mut self, ui: &Ui) {
        let (prev, next) = (self.prev_diff(), self.next_diff());
        ui.input_mut(|i| {
            if i.consume_key(Modifiers::ALT, Key::ArrowDown) && let Some(d) = next {
                self.select_diff(d);
            } else if i.consume_key(Modifiers::ALT, Key::ArrowUp) && let Some(d) = prev {
                self.select_diff(d);
            }
        });
    }

    fn next_diff(&self) -> Option<usize> {
        let i = self.current.map_or(0, |c| c + 1);
        (i < self.diff.regions.len()).then_some(i)
    }

    fn prev_diff(&self) -> Option<usize> {
        match self.current {
            Some(c) => c.checked_sub(1),
            None => self.diff.regions.len().checked_sub(1),
        }
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        let alt = if cfg!(target_os = "macos") { "Option" } else { "Alt" };
        ui.horizontal(|ui| {
            let prev = self.prev_diff();
            if ui.add_enabled(prev.is_some(), egui::Button::new("Prev Diff")).on_hover_text(format!("{alt}+Up")).clicked() {
                self.select_diff(prev.expect("enabled only when some"));
            }
            let next = self.next_diff();
            if ui.add_enabled(next.is_some(), egui::Button::new("Next Diff")).on_hover_text(format!("{alt}+Down")).clicked() {
                self.select_diff(next.expect("enabled only when some"));
            }
            ui.separator();
            if ui.selectable_label(self.zoom.is_none(), "Fit").on_hover_text("Fit the whole image (double-click)").clicked() {
                self.zoom = None;
            }
            if ui.selectable_label(self.zoom == Some(1.0), "1:1").on_hover_text("Actual pixels").clicked() {
                self.zoom = Some(1.0);
            }
            ui.label(format!("{:.0}%", self.effective_zoom() * 100.0))
                .on_hover_text("Ctrl+scroll or pinch to zoom, drag or scroll to pan");
            ui.separator();
            ui.checkbox(&mut self.show_diff, "Highlight differences");
            ui.label("Tolerance");
            let tolerance = ui
                .add(egui::DragValue::new(&mut self.threshold).range(0..=255))
                .on_hover_text("How far a colour channel may differ (0–255) before a pixel counts as changed");
            if tolerance.changed() {
                self.rediff();
            }
            ui.separator();
            ui.selectable_value(&mut self.mode, Mode::SideBySide, "Side by side");
            ui.selectable_value(&mut self.mode, Mode::Overlay, "Overlay");
            if self.mode == Mode::Overlay {
                ui.add(egui::Slider::new(&mut self.blend, 0.0..=1.0).show_value(false))
                    .on_hover_text("Fade between left and right");
            }
        });
    }

    fn build_textures(&self, ctx: &egui::Context) -> Textures {
        let max_side = ctx.input(|i| i.max_texture_side).max(1);
        let scale = self.diff.width.max(self.diff.height).div_ceil(max_side).max(1);
        // Smooth when shrunk, crisp pixels when zoomed in.
        let options = TextureOptions { magnification: egui::TextureFilter::Nearest, ..TextureOptions::LINEAR };
        let images = [0, 1].map(|side| {
            let img = self.images[side].as_ref()?;
            let color = if scale == 1 {
                ColorImage::from_rgba_unmultiplied([img.width, img.height], &img.rgba)
            } else {
                let buf = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.rgba.clone())?;
                let (w, h) = ((img.width / scale).max(1) as u32, (img.height / scale).max(1) as u32);
                let small = image::imageops::resize(&buf, w, h, image::imageops::FilterType::Triangle);
                ColorImage::from_rgba_unmultiplied([w as usize, h as usize], small.as_raw())
            };
            Some(ctx.load_texture(format!("{}", self.paths[side].display()), color, options))
        });
        let (mw, mh, mask) = self.diff.downscaled_mask(scale);
        let mask = (self.diff.differing > 0).then(|| {
            let pixels = mask.iter().map(|&m| if m { Color32::WHITE } else { Color32::TRANSPARENT }).collect();
            ctx.load_texture("image_diff_mask", ColorImage::new([mw, mh], pixels), TextureOptions::NEAREST)
        });
        let (light, dark) = (Color32::from_gray(204), Color32::from_gray(153));
        let checker = ctx.load_texture(
            "checker",
            ColorImage::new([2, 2], vec![light, dark, dark, light]),
            TextureOptions::NEAREST_REPEAT,
        );
        Textures { images, mask, mask_size: vec2((mw * scale) as f32, (mh * scale) as f32), checker }
    }

    /// Drag or scroll pans every pane together; Ctrl+scroll/pinch zooms around the pointer.
    fn pan_and_zoom(&mut self, ui: &mut Ui, body: Rect, panes: &[Rect]) {
        let response = ui.interact(body, self.id.with("panes"), Sense::click_and_drag());
        if response.double_clicked() {
            self.zoom = None;
            return;
        }
        let zoom = self.effective_zoom();
        let pointer = response.hover_pos();
        self.hover = pointer.and_then(|p| {
            let pane = panes.iter().find(|r| r.contains(p))?;
            let img = self.center + (p - pane.center()) / zoom;
            (img.x >= 0.0 && img.y >= 0.0 && (img.x as usize) < self.diff.width && (img.y as usize) < self.diff.height)
                .then_some((img.x as usize, img.y as usize))
        });
        let mut pan = if response.dragged() { response.drag_delta() } else { Vec2::ZERO };
        let (scroll, zoom_delta) = if pointer.is_some() {
            ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()))
        } else {
            (Vec2::ZERO, 1.0)
        };
        pan += scroll;
        if zoom_delta != 1.0
            && let Some(p) = pointer
            && let Some(pane) = panes.iter().find(|r| r.contains(p))
        {
            // Keep the pixel under the pointer where it is.
            let anchor = self.center + (p - pane.center()) / zoom;
            let new_zoom = (zoom * zoom_delta).clamp(MIN_ZOOM, MAX_ZOOM);
            self.center = anchor - (p - pane.center()) / new_zoom;
            self.zoom = Some(new_zoom);
        }
        if pan != Vec2::ZERO {
            self.zoom = Some(self.effective_zoom());
            self.center -= pan / self.effective_zoom();
        }
        if self.zoom.is_some() {
            self.center = self.center.clamp(Pos2::ZERO, pos2(self.diff.width as f32, self.diff.height as f32));
        }
    }

    fn header(&self, ui: &Ui, side: usize, rect: Rect) {
        let info = match (&self.images[side], &self.errors[side]) {
            (Some(img), _) => (format!("{} × {}", img.width, img.height), false),
            (None, Some(_)) => ("can't load".to_string(), true),
            (None, None) => ("missing".to_string(), false),
        };
        paint_header(ui, rect, &self.paths[side].display().to_string(), info);
    }

    fn overlay_header(&self, ui: &Ui, rect: Rect) {
        let text = format!("{}  over  {}", file_name(&self.paths[1]), file_name(&self.paths[0]));
        let info = format!("{} {:.0}%", SIDE_NAMES[1], self.blend * 100.0);
        paint_header(ui, rect, &text, (info, false));
    }

    /// Draws `layers` (side, opacity) in order; `tint_side` picks the highlight colour (2 = overlay).
    fn paint_pane(&self, ui: &Ui, rect: Rect, layers: &[(usize, f32)], tint_side: usize) {
        let Some(tex) = &self.textures else { return };
        let visuals = ui.visuals();
        let dark = visuals.dark_mode;
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, visuals.extreme_bg_color);
        let zoom = self.effective_zoom();
        let to_screen = |x: f32, y: f32| rect.center() + (pos2(x, y) - self.center) * zoom;
        let area = |w: f32, h: f32| Rect::from_min_max(to_screen(0.0, 0.0), to_screen(w, h));
        let uv = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));

        // The part of the combined area this side doesn't cover reads as filler.
        let union = area(self.diff.width as f32, self.diff.height as f32);
        painter.rect_filled(union, 0.0, if dark { Color32::from_gray(40) } else { Color32::from_gray(232) });

        for &(side, opacity) in layers {
            let (Some(img), Some(texture)) = (&self.images[side], &tex.images[side]) else { continue };
            let r = area(img.width as f32, img.height as f32);
            let tint = Color32::from_white_alpha((opacity * 255.0) as u8);
            if layers[0].0 == side {
                let checker_uv = Rect::from_min_size(Pos2::ZERO, r.size() / (2.0 * CHECKER));
                painter.image(tex.checker.id(), r, checker_uv, tint);
            }
            painter.image(texture.id(), r, uv, tint);
        }

        if self.show_diff
            && let Some(mask) = &tex.mask
        {
            let color = match tint_side {
                0 => Color32::from_rgba_unmultiplied(235, 40, 60, 150),
                1 => Color32::from_rgba_unmultiplied(40, 210, 80, 150),
                _ => Color32::from_rgba_unmultiplied(255, 0, 255, 150),
            };
            painter.image(mask.id(), area(tex.mask_size.x, tex.mask_size.y), uv, color);
        }

        if let Some(c) = self.current {
            let reg = self.diff.regions[c];
            let r = Rect::from_min_max(
                to_screen(reg.x as f32, reg.y as f32),
                to_screen((reg.x + reg.w) as f32, (reg.y + reg.h) as f32),
            )
            .expand(3.0);
            painter.rect_stroke(r, 2.0, Stroke::new(2.0, Color32::from_rgb(255, 196, 0)), StrokeKind::Outside);
        }

        if tint_side < 2 && self.images[tint_side].is_none() {
            let text = match &self.errors[tint_side] {
                Some(e) => format!("Couldn't load image:\n{e}"),
                None => "File doesn't exist".to_string(),
            };
            let color = if self.errors[tint_side].is_some() { visuals.error_fg_color } else { visuals.weak_text_color() };
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, text, FontId::proportional(14.0), color);
        }
    }

    fn status_bar(&self, ui: &Ui, rect: Rect) {
        let visuals = ui.visuals();
        let painter = ui.painter_at(rect);
        let font = FontId::proportional(13.0);
        if let Some((x, y)) = self.hover {
            let px = |side: usize| {
                let p = self.images[side].as_ref().filter(|i| x < i.width && y < i.height).map(|i| {
                    let k = (y * i.width + x) * 4;
                    format!("#{:02X}{:02X}{:02X}{:02X}", i.rgba[k], i.rgba[k + 1], i.rgba[k + 2], i.rgba[k + 3])
                });
                p.unwrap_or_else(|| "—".to_string())
            };
            let text = format!("{x}, {y}   L {}   R {}", px(0), px(1));
            painter.text(pos2(rect.left() + 4.0, rect.center().y), egui::Align2::LEFT_CENTER, text, font.clone(), visuals.text_color());
        }

        let n = self.diff.regions.len();
        let percent = self.diff.fraction() * 100.0;
        let diffs = match (n, self.current) {
            (0, _) if self.threshold > 0 => "No differences (within tolerance)".to_string(),
            (0, _) => "Images are identical".to_string(),
            (_, Some(c)) => format!("Difference {} of {n} · {percent:.2}% of pixels differ", c + 1),
            (1, None) => format!("1 difference · {percent:.2}% of pixels differ"),
            (_, None) => format!("{n} differences · {percent:.2}% of pixels differ"),
        };
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, diffs, font.clone(), visuals.text_color());

        if let Some((msg, error)) = &self.message {
            let color = if *error { visuals.error_fg_color } else { visuals.weak_text_color() };
            painter.text(pos2(rect.right() - 4.0, rect.center().y), egui::Align2::RIGHT_CENTER, msg, font, color);
        }
    }
}

/// A pane header: `text` on the left (trimmed from the start to fit), `info` on the right.
fn paint_header(ui: &Ui, rect: Rect, text: &str, (info, error): (String, bool)) {
    let visuals = ui.visuals();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, visuals.faint_bg_color);
    let info_color = if error { visuals.error_fg_color } else { visuals.weak_text_color() };
    let info_galley = painter.layout_no_wrap(info, FontId::proportional(12.0), info_color);
    let info_pos = pos2(rect.right() - info_galley.size().x - 6.0, rect.center().y - info_galley.size().y / 2.0);
    let max_w = info_pos.x - rect.left() - 14.0;
    let font = FontId::proportional(13.0);
    let mut galley = painter.layout_no_wrap(text.to_string(), font.clone(), visuals.strong_text_color());
    let mut skip = 0;
    while galley.size().x > max_w && skip < text.len() {
        skip = text.ceil_char_boundary(skip + 1);
        galley = painter.layout_no_wrap(format!("…{}", &text[skip..]), font.clone(), visuals.strong_text_color());
    }
    painter.galley(pos2(rect.left() + 6.0, rect.center().y - galley.size().y / 2.0), galley, visuals.strong_text_color());
    painter.galley(info_pos, info_galley, info_color);
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

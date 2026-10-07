//! Pixel-level image comparison (no egui).
//!
//! Both images are laid over each other from the top-left corner; a pixel only one image covers
//! counts as different. Differing pixels are grouped into rectangular regions (via a coarse cell
//! grid) so the view can step through them like line differences.

use std::path::Path;

/// Side of the grid cells differing pixels are grouped by; nearby changes merge into one region.
const CELL: usize = 16;

const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "ico"];

/// True for files that should open in the image view (decided by extension).
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

#[derive(Clone, PartialEq, Debug)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// Unpremultiplied RGBA, row-major.
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn new(width: usize, height: usize, rgba: Vec<u8>) -> Self {
        assert_eq!(rgba.len(), width * height * 4);
        Self { width, height, rgba }
    }

    /// Loads an image file (first frame of an animation), or `None` if it doesn't exist.
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        if !path.exists() {
            return Ok(None);
        }
        let img = image::ImageReader::open(path)
            .and_then(|r| r.with_guessed_format())
            .map_err(|e| e.to_string())?
            .decode()
            .map_err(|e| e.to_string())?
            .into_rgba8();
        let (w, h) = img.dimensions();
        Ok(Some(Self::new(w as usize, h as usize, img.into_raw())))
    }

    fn pixel(&self, x: usize, y: usize) -> Option<[u8; 4]> {
        (x < self.width && y < self.height).then(|| {
            let i = (y * self.width + x) * 4;
            [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
        })
    }
}

/// A rectangle in image pixels, `x..x + w` by `y..y + h`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Region {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

pub struct ImageDiff {
    /// Size of the area covered by either image.
    pub width: usize,
    pub height: usize,
    /// One flag per pixel of `width` x `height`.
    pub mask: Vec<bool>,
    pub differing: usize,
    /// Groups of differing pixels, top to bottom then left to right.
    pub regions: Vec<Region>,
}

impl ImageDiff {
    pub fn fraction(&self) -> f64 {
        let total = self.width * self.height;
        if total == 0 { 0.0 } else { self.differing as f64 / total as f64 }
    }

    /// The mask shrunk by an integer `scale`; a cell is set if any pixel in it differs.
    pub fn downscaled_mask(&self, scale: usize) -> (usize, usize, Vec<bool>) {
        let (w, h) = (self.width.div_ceil(scale), self.height.div_ceil(scale));
        let mut out = vec![false; w * h];
        for y in 0..self.height {
            for x in 0..self.width {
                if self.mask[y * self.width + x] {
                    out[(y / scale) * w + x / scale] = true;
                }
            }
        }
        (w, h, out)
    }
}

/// Two pixels match if every channel is within `threshold`; fully transparent pixels always match
/// each other whatever their colour.
fn same(a: [u8; 4], b: [u8; 4], threshold: u8) -> bool {
    (a[3] == 0 && b[3] == 0) || a.iter().zip(b).all(|(&p, q)| p.abs_diff(q) <= threshold)
}

pub fn compare(left: Option<&Image>, right: Option<&Image>, threshold: u8) -> ImageDiff {
    let dims = |i: Option<&Image>| i.map_or((0, 0), |i| (i.width, i.height));
    let ((lw, lh), (rw, rh)) = (dims(left), dims(right));
    let (width, height) = (lw.max(rw), lh.max(rh));
    let mut mask = vec![false; width * height];
    let mut differing = 0;
    for y in 0..height {
        for x in 0..width {
            let a = left.and_then(|i| i.pixel(x, y));
            let b = right.and_then(|i| i.pixel(x, y));
            let differs = match (a, b) {
                (Some(a), Some(b)) => !same(a, b, threshold),
                (None, None) => false,
                _ => true,
            };
            if differs {
                mask[y * width + x] = true;
                differing += 1;
            }
        }
    }
    let regions = regions(&mask, width, height);
    ImageDiff { width, height, mask, differing, regions }
}

/// Groups differing pixels: marks grid cells containing any, joins touching cells (including
/// diagonally) and returns the tight pixel bounds of each group.
fn regions(mask: &[bool], width: usize, height: usize) -> Vec<Region> {
    let (cw, ch) = (width.div_ceil(CELL), height.div_ceil(CELL));
    let mut cells = vec![false; cw * ch];
    for y in 0..height {
        for x in 0..width {
            if mask[y * width + x] {
                cells[(y / CELL) * cw + x / CELL] = true;
            }
        }
    }

    let mut seen = vec![false; cw * ch];
    let mut out = Vec::new();
    for start in 0..cells.len() {
        if !cells[start] || seen[start] {
            continue;
        }
        seen[start] = true;
        let mut stack = vec![start];
        let mut group = Vec::new();
        while let Some(c) = stack.pop() {
            group.push(c);
            let (cx, cy) = ((c % cw) as isize, (c / cw) as isize);
            for (dx, dy) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
                let (nx, ny) = (cx + dx, cy + dy);
                if nx < 0 || ny < 0 || nx >= cw as isize || ny >= ch as isize {
                    continue;
                }
                let n = ny as usize * cw + nx as usize;
                if cells[n] && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
        }

        // Tight bounds of the actual differing pixels inside the group's cells.
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        for c in group {
            let (cx, cy) = ((c % cw) * CELL, (c / cw) * CELL);
            for y in cy..(cy + CELL).min(height) {
                for x in cx..(cx + CELL).min(width) {
                    if mask[y * width + x] {
                        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                    }
                }
            }
        }
        out.push(Region { x: x0, y: y0, w: x1 - x0 + 1, h: y1 - y0 + 1 });
    }
    out.sort_by_key(|r| (r.y, r.x));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, px: [u8; 4]) -> Image {
        Image::new(w, h, px.repeat(w * h))
    }

    fn set(img: &mut Image, x: usize, y: usize, px: [u8; 4]) {
        let i = (y * img.width + x) * 4;
        img.rgba[i..i + 4].copy_from_slice(&px);
    }

    const WHITE: [u8; 4] = [255, 255, 255, 255];
    const BLACK: [u8; 4] = [0, 0, 0, 255];

    #[test]
    fn identical_images_have_no_regions() {
        let a = solid(40, 30, WHITE);
        let d = compare(Some(&a), Some(&a.clone()), 0);
        assert_eq!((d.width, d.height, d.differing), (40, 30, 0));
        assert!(d.regions.is_empty());
    }

    #[test]
    fn single_pixel_gives_tight_region() {
        let a = solid(40, 30, WHITE);
        let mut b = a.clone();
        set(&mut b, 21, 7, BLACK);
        let d = compare(Some(&a), Some(&b), 0);
        assert_eq!(d.differing, 1);
        assert_eq!(d.regions, vec![Region { x: 21, y: 7, w: 1, h: 1 }]);
    }

    #[test]
    fn nearby_changes_merge_and_distant_ones_dont() {
        let a = solid(100, 100, WHITE);
        let mut b = a.clone();
        set(&mut b, 2, 2, BLACK);
        set(&mut b, 20, 20, BLACK); // neighbouring cell (diagonal): same region
        set(&mut b, 90, 5, BLACK); // far away: own region, sorted by y first
        let d = compare(Some(&a), Some(&b), 0);
        assert_eq!(
            d.regions,
            vec![Region { x: 2, y: 2, w: 19, h: 19 }, Region { x: 90, y: 5, w: 1, h: 1 }]
        );
    }

    #[test]
    fn threshold_and_transparency() {
        let a = solid(4, 4, [100, 100, 100, 255]);
        let b = solid(4, 4, [103, 98, 100, 255]);
        assert_eq!(compare(Some(&a), Some(&b), 2).differing, 16);
        assert_eq!(compare(Some(&a), Some(&b), 3).differing, 0);

        let clear_red = solid(4, 4, [255, 0, 0, 0]);
        let clear_blue = solid(4, 4, [0, 0, 255, 0]);
        assert_eq!(compare(Some(&clear_red), Some(&clear_blue), 0).differing, 0);
    }

    #[test]
    fn size_mismatch_and_missing_side() {
        let a = solid(10, 10, WHITE);
        let b = solid(12, 10, WHITE);
        let d = compare(Some(&a), Some(&b), 0);
        assert_eq!((d.width, d.height, d.differing), (12, 10, 20));
        assert_eq!(d.regions, vec![Region { x: 10, y: 0, w: 2, h: 10 }]);

        let d = compare(None, Some(&a), 0);
        assert_eq!(d.differing, 100);
        assert_eq!(d.regions, vec![Region { x: 0, y: 0, w: 10, h: 10 }]);
        assert!((d.fraction() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn downscaled_mask_keeps_any_difference() {
        let a = solid(5, 5, WHITE);
        let mut b = a.clone();
        set(&mut b, 4, 4, BLACK);
        let (w, h, m) = compare(Some(&a), Some(&b), 0).downscaled_mask(2);
        assert_eq!((w, h), (3, 3));
        assert_eq!(m.iter().filter(|&&x| x).count(), 1);
        assert!(m[8]);
    }

    #[test]
    fn recognises_image_extensions() {
        assert!(is_image_path(Path::new("a/b.PNG")));
        assert!(is_image_path(Path::new("x.jpeg")));
        assert!(!is_image_path(Path::new("x.txt")));
        assert!(!is_image_path(Path::new("png")));
    }
}

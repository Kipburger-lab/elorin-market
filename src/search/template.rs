//! Template matching: slide a small image across a captured `Frame` and return
//! the positions where every *unmasked* template pixel matches within
//! `tolerance`. `find` returns the first match; `find_all_with_error` returns
//! every match with an error score (used to read digit glyphs).

use rayon::prelude::*;

use crate::capture::{Frame, Rect};
use crate::search::Match;

/// A decoded template image, stored as R (not BGRA) for clear comparison.
#[derive(Debug, Clone)]
pub struct Template {
    pub width: i32,
    pub height: i32,
    /// RGB triples, row-major, top-down.
    pub rgb: Vec<(u8, u8, u8)>,
    /// Parallel alpha mask: false = ignore this template pixel.
    pub mask: Vec<bool>,
}

impl Template {
    /// Decode a PNG (or any `image`-supported format) into a Template.
    /// Fully-transparent template pixels are masked out (ignored).
    pub fn from_image_rgba(img: &image::RgbaImage) -> Self {
        let width = img.width() as i32;
        let height = img.height() as i32;
        let mut rgb = Vec::with_capacity(img.len());
        let mut mask = Vec::with_capacity(img.len());
        for px in img.pixels() {
            let [r, g, b, a] = px.0;
            rgb.push((r, g, b));
            mask.push(a > 0);
        }
        Self {
            width,
            height,
            rgb,
            mask,
        }
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let img = image::open(path)?.to_rgba8();
        Ok(Self::from_image_rgba(&img))
    }

    /// A template whose buffers are shorter than its declared size is a
    /// placeholder (an empty sprite standing in for one that failed to load);
    /// it must never be indexed.
    fn is_placeholder(&self) -> bool {
        let needed = (self.width.max(0) * self.height.max(0)) as usize;
        self.width <= 0
            || self.height <= 0
            || self.rgb.len() < needed
            || self.mask.len() < needed
    }
}

/// A match with an error score (sum of per-channel absolute differences over the
/// masked pixels, weighted by green). Lower = better.
#[derive(Debug, Clone, Copy)]
pub struct MatchWithError {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub error: u32,
}

/// Slide `template` across `frame`, return the first position where every
/// *unmasked* template pixel matches within `tolerance`. Returns the match's
/// top-left in **frame-local** coordinates.
pub fn find(frame: &Frame, template: &Template, tolerance: u8) -> Option<Match> {
    let fw = frame.width;
    let fh = frame.height;
    let tw = template.width;
    let th = template.height;
    if tw > fw || th > fh {
        return None;
    }
    if template.is_placeholder() {
        return None;
    }

    let last_x = fw - tw;
    let last_y = fh - th;

    // For small templates, thread spawn overhead exceeds the benefit.
    let pixel_count = (tw * th) as usize;
    let use_threads = pixel_count >= 64;

    let threads = if use_threads {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(last_y.saturating_add(1) as usize)
            .max(1)
    } else {
        1
    };
    if threads == 1 {
        return find_serial(frame, template, fw, tw, th, tolerance, last_x, last_y, 0, last_y);
    }

    let total_rows = last_y + 1;
    let rows_per_thread = (total_rows + threads as i32 - 1) / threads as i32;
    let mut bands = Vec::with_capacity(threads);
    for t in 0..threads {
        let start = t as i32 * rows_per_thread;
        let end = (start + rows_per_thread).min(total_rows);
        if start >= end {
            continue;
        }
        bands.push((start, end));
    }

    let mut band_matches: Vec<Option<Match>> = Vec::with_capacity(bands.len());
    std::thread::scope(|s| {
        let handles: Vec<_> = bands
            .iter()
            .map(|&(start, end)| {
                s.spawn(move || {
                    find_serial(frame, template, fw, tw, th, tolerance, last_x, last_y, start, end)
                })
            })
            .collect();
        for h in handles {
            band_matches.push(h.join().unwrap_or(None));
        }
    });

    for m in band_matches {
        if m.is_some() {
            return m;
        }
    }
    None
}

/// Every match of `template` inside `rect` whose masked pixels all fall within
/// `tolerance`, each carrying an error score. Parallelised across CPU threads.
///
/// Used by the market reader: it needs *all* digit placements in a price band
/// (not just the first), then picks the best candidate per digit slot by error.
pub fn find_all_with_error(
    frame: &Frame,
    template: &Template,
    rect: Rect,
    tolerance: u8,
    step: i32,
) -> Vec<MatchWithError> {
    if template.is_placeholder() {
        return Vec::new();
    }
    let fw = frame.width;
    let fh = frame.height;
    let tw = template.width;
    let th = template.height;

    let x1 = rect.x1.max(0).min(fw);
    let y1 = rect.y1.max(0).min(fh);
    let x2 = rect.x2.max(0).min(fw);
    let y2 = rect.y2.max(0).min(fh);
    if x2 <= x1 || y2 <= y1 || tw > (x2 - x1) || th > (y2 - y1) {
        return Vec::new();
    }

    let tol = tolerance as i32;
    let last_x = x2 - tw;
    let last_y = y2 - th;
    let step_val = step.max(1);

    let rows: Vec<i32> = (y1..=last_y).step_by(step_val as usize).collect();

    let mut matches: Vec<MatchWithError> = rows
        .into_par_iter()
        .flat_map(|ty| {
            let mut row_matches = Vec::new();
            let mut tx = x1;
            while tx <= last_x {
                let mut matched = true;
                let mut total_error: u32 = 0;
                'check: for ry in 0..th {
                    let f_row = (ty + ry) * fw;
                    let t_row = ry * tw;
                    for rx in 0..tw {
                        let i = (t_row + rx) as usize;
                        if !template.mask[i] {
                            continue;
                        }
                        let off = ((f_row + tx + rx) * 4) as usize;
                        if off + 2 >= frame.bgra.len() {
                            matched = false;
                            break 'check;
                        }
                        let (tr, tg, tb) = template.rgb[i];
                        let fb = frame.bgra[off];
                        let fg = frame.bgra[off + 1];
                        let fr = frame.bgra[off + 2];
                        let dr = (tr as i32 - fr as i32).abs();
                        let dg = (tg as i32 - fg as i32).abs();
                        let db = (tb as i32 - fb as i32).abs();
                        if dr > tol || dg > tol || db > tol {
                            matched = false;
                            break 'check;
                        }
                        total_error += (dr + dg + db) as u32;
                    }
                }
                if matched {
                    row_matches.push(MatchWithError {
                        x: tx,
                        y: ty,
                        w: tw,
                        h: th,
                        error: total_error,
                    });
                }
                tx += step_val;
            }
            row_matches
        })
        .collect();

    matches.sort_by(|a, b| a.y.cmp(&b.y).then_with(|| a.x.cmp(&b.x)));
    matches
}

fn find_serial(
    frame: &Frame,
    template: &Template,
    fw: i32,
    tw: i32,
    th: i32,
    tolerance: u8,
    last_x: i32,
    _last_y: i32,
    row_start: i32,
    row_end: i32,
) -> Option<Match> {
    let tol = tolerance as i32;

    // Key pixel for early-exit: the first unmasked pixel.
    let key_pixel = template.mask.iter().position(|&m| m);

    for ty in row_start..=row_end {
        'outer: for tx in 0..=last_x {
            if let Some(ki) = key_pixel {
                let kx = (ki as i32) % tw;
                let ky = (ki as i32) / tw;
                let off = (((ty + ky) * fw + tx + kx) * 4) as usize;
                if off + 2 < frame.bgra.len() {
                    let (tr, tg, tb) = template.rgb[ki];
                    let fb = frame.bgra[off];
                    let fg = frame.bgra[off + 1];
                    let fr = frame.bgra[off + 2];
                    if (tr as i32 - fr as i32).abs() > tol
                        || (tg as i32 - fg as i32).abs() > tol
                        || (tb as i32 - fb as i32).abs() > tol
                    {
                        continue;
                    }
                }
            }

            for ry in 0..th {
                let f_row = (ty + ry) * fw;
                let t_row = ry * tw;
                for rx in 0..tw {
                    let i = (t_row + rx) as usize;
                    if !template.mask[i] {
                        continue;
                    }
                    let off = ((f_row + tx + rx) * 4) as usize;
                    let (tr, tg, tb) = template.rgb[i];
                    let fb = frame.bgra[off];
                    let fg = frame.bgra[off + 1];
                    let fr = frame.bgra[off + 2];
                    if (tr as i32 - fr as i32).abs() > tol
                        || (tg as i32 - fg as i32).abs() > tol
                        || (tb as i32 - fb as i32).abs() > tol
                    {
                        continue 'outer;
                    }
                }
            }
            return Some(Match {
                x: tx,
                y: ty,
                w: tw,
                h: th,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_template(w: i32, h: i32, rgb: (u8, u8, u8)) -> Template {
        Template {
            width: w,
            height: h,
            rgb: vec![rgb; (w * h) as usize],
            mask: vec![true; (w * h) as usize],
        }
    }

    fn frame_with_rect(
        fw: i32,
        fh: i32,
        rx: i32,
        ry: i32,
        rw: i32,
        rh: i32,
        rgb: (u8, u8, u8),
    ) -> Frame {
        let mut bgra = vec![0u8; (fw * fh * 4) as usize];
        for y in ry..(ry + rh) {
            for x in rx..(rx + rw) {
                let off = ((y * fw + x) * 4) as usize;
                // BGRA
                bgra[off] = rgb.2;
                bgra[off + 1] = rgb.1;
                bgra[off + 2] = rgb.0;
                bgra[off + 3] = 255;
            }
        }
        Frame {
            width: fw,
            height: fh,
            bgra,
        }
    }

    #[test]
    fn finds_exact_block() {
        let frame = frame_with_rect(20, 20, 5, 5, 3, 3, (10, 20, 30));
        let tpl = solid_template(2, 2, (10, 20, 30));
        let m = find(&frame, &tpl, 0).expect("should match");
        assert_eq!((m.x, m.y), (5, 5));
        assert_eq!(m.center(), (6, 6));
    }

    #[test]
    fn tolerance_allows_near_match() {
        let frame = frame_with_rect(10, 10, 0, 0, 10, 10, (100, 100, 100));
        let tpl = solid_template(2, 2, (110, 95, 105));
        assert!(find(&frame, &tpl, 10).is_some());
        assert!(find(&frame, &tpl, 9).is_none());
    }

    #[test]
    fn placeholder_template_is_skipped_not_indexed() {
        let frame = frame_with_rect(20, 20, 0, 0, 20, 20, (0, 0, 0));
        let placeholder = Template {
            width: 30,
            height: 30,
            rgb: Vec::new(),
            mask: Vec::new(),
        };
        assert!(find(&frame, &placeholder, 15).is_none());
        assert!(find_all_with_error(&frame, &placeholder, frame.rect(), 15, 1).is_empty());
    }

    #[test]
    fn find_all_returns_every_placement_with_error() {
        // Two 4x4 green blocks in a black frame.
        let mut frame = frame_with_rect(40, 12, 3, 3, 4, 4, (10, 200, 10));
        for y in 3..7 {
            for x in 25..29 {
                let off = ((y * 40 + x) * 4) as usize;
                frame.bgra[off] = 10;
                frame.bgra[off + 1] = 200;
                frame.bgra[off + 2] = 10;
            }
        }
        let tpl = solid_template(4, 4, (10, 200, 10));
        let all = find_all_with_error(&frame, &tpl, frame.rect(), 0, 1);
        // Each block yields exactly one exact match (error 0).
        let zero: Vec<_> = all.iter().filter(|m| m.error == 0).collect();
        assert!(zero.iter().any(|m| (m.x, m.y) == (3, 3)));
        assert!(zero.iter().any(|m| (m.x, m.y) == (25, 3)));
    }

    #[test]
    fn find_all_respects_rect_clipping() {
        let frame = frame_with_rect(40, 12, 25, 3, 4, 4, (10, 200, 10));
        let tpl = solid_template(4, 4, (10, 200, 10));
        // A rect that excludes the block must find nothing.
        let left = Rect { x1: 0, y1: 0, x2: 20, y2: 12 };
        assert!(find_all_with_error(&frame, &tpl, left, 0, 1).is_empty());
        // A rect containing it finds it.
        let right = Rect { x1: 20, y1: 0, x2: 40, y2: 12 };
        assert!(!find_all_with_error(&frame, &tpl, right, 0, 1).is_empty());
    }
}

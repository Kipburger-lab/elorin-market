//! Draw the regions and decisions onto a captured frame.
//!
//! The raw recording shows *what* happened; these annotations show *why* — the
//! row it scanned, the band it searched, the item it picked, the button it
//! clicked. Drawn straight into the BGRA buffer before the frame is saved, so a
//! debug image needs no side-car explanation to be read.

use crate::capture::{Frame, Rect};

/// Scanned row / Open button.
pub const MAGENTA: [u8; 4] = [255, 0, 255, 255];
/// Item-icon region.
pub const ORANGE: [u8; 4] = [255, 140, 0, 255];
/// Price band / amount region.
pub const CYAN: [u8; 4] = [0, 255, 255, 255];
/// Searched band / menu search area.
pub const YELLOW: [u8; 4] = [255, 255, 0, 255];
/// Chosen match.
pub const GREEN: [u8; 4] = [0, 255, 0, 255];
/// Rejected match.
pub const RED: [u8; 4] = [255, 0, 0, 255];

/// One box to draw over a frame.
#[derive(Debug, Clone, Copy)]
pub struct Stroke {
    pub rect: Rect,
    pub rgba: [u8; 4],
    pub thickness: i32,
}

impl Stroke {
    pub fn new(rect: Rect, rgba: [u8; 4], thickness: i32) -> Self {
        Self {
            rect,
            rgba,
            thickness,
        }
    }
}

/// Stroke a rectangle outline onto `frame`, clipped to its bounds.
pub fn stroke_rect(frame: &mut Frame, r: Rect, rgba: [u8; 4], thickness: i32) {
    let x1 = r.x1.min(r.x2);
    let x2 = r.x1.max(r.x2);
    let y1 = r.y1.min(r.y2);
    let y2 = r.y1.max(r.y2);
    let t = thickness.max(1);
    for i in 0..t {
        hline(frame, x1, x2, y1 + i, rgba);
        hline(frame, x1, x2, y2 - i, rgba);
        vline(frame, y1, y2, x1 + i, rgba);
        vline(frame, y1, y2, x2 - i, rgba);
    }
}

/// A small cross centred on a point, for a coordinate we settled on.
pub fn draw_marker(frame: &mut Frame, x: i32, y: i32, rgba: [u8; 4], size: i32) {
    let s = size.max(1);
    hline(frame, x - s, x + s, y, rgba);
    vline(frame, y - s, y + s, x, rgba);
}

/// A copy of `frame` with every stroke drawn on it.
pub fn annotated(frame: &Frame, strokes: &[Stroke]) -> Frame {
    let mut f = frame.clone();
    for s in strokes {
        stroke_rect(&mut f, s.rect, s.rgba, s.thickness);
    }
    f
}

fn hline(frame: &mut Frame, x1: i32, x2: i32, y: i32, rgba: [u8; 4]) {
    for x in x1..=x2 {
        put(frame, x, y, rgba);
    }
}

fn vline(frame: &mut Frame, y1: i32, y2: i32, x: i32, rgba: [u8; 4]) {
    for y in y1..=y2 {
        put(frame, x, y, rgba);
    }
}

#[inline]
fn put(frame: &mut Frame, x: i32, y: i32, rgba: [u8; 4]) {
    if x < 0 || y < 0 || x >= frame.width || y >= frame.height {
        return;
    }
    let off = ((y * frame.width + x) * 4) as usize;
    if off + 3 >= frame.bgra.len() {
        return;
    }
    frame.bgra[off] = rgba[2]; // B
    frame.bgra[off + 1] = rgba[1]; // G
    frame.bgra[off + 2] = rgba[0]; // R
    frame.bgra[off + 3] = 255;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank() -> Frame {
        Frame {
            width: 10,
            height: 10,
            bgra: vec![0u8; 10 * 10 * 4],
        }
    }

    #[test]
    fn stroke_sets_the_outline_only() {
        let mut f = blank();
        stroke_rect(&mut f, Rect { x1: 0, y1: 0, x2: 5, y2: 5 }, MAGENTA, 1);
        assert_eq!(f.pixel(0, 0).unwrap(), [255, 0, 255, 255], "corner is on the outline");
        assert_eq!(f.pixel(2, 2).unwrap(), [0, 0, 0, 0], "interior is untouched");
        assert_eq!(f.pixel(9, 9).unwrap(), [0, 0, 0, 0], "outside is untouched");
    }

    #[test]
    fn stroke_clips_at_the_frame_edge() {
        let mut f = blank();
        // Runs off the top-left; must not panic and must paint the in-view edge.
        stroke_rect(&mut f, Rect { x1: -5, y1: -5, x2: 5, y2: 5 }, GREEN, 1);
        assert_eq!(f.pixel(5, 0).unwrap(), [0, 255, 0, 255], "right edge is visible");
    }
}

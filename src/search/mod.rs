//! Image/pixel search: alpha-masked template matching against a captured `Frame`.

pub mod template;

use crate::capture::Frame;

/// A successful match: top-left of the template, in client coordinates.
#[derive(Debug, Clone, Copy)]
pub struct Match {
    pub x: i32,
    pub y: i32,
    /// Matched template width/height (for computing the click center).
    pub w: i32,
    pub h: i32,
}

impl Match {
    /// Client coordinate of the template's center pixel.
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// Convenience: get a frame pixel at a frame-local offset, as (R, G, B).
/// Frame is BGRA, so index 0 = B, 1 = G, 2 = R.
#[inline]
pub fn frame_rgb(frame: &Frame, x: i32, y: i32) -> Option<(u8, u8, u8)> {
    frame.pixel(x, y).map(|bgra| (bgra[2], bgra[1], bgra[0]))
}

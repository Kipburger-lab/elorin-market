//! Screen capture. A `Capturer` produces a `Frame` for a given client rect.
//! GDI (BitBlt) is the proven default, matching Elorin's own capturer.

pub mod gdi;

use anyhow::Result;

/// Axis-aligned rectangle in client coordinates (x1..x2, y1..y2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl Rect {
    pub const fn width(&self) -> i32 {
        self.x2 - self.x1
    }
    pub const fn height(&self) -> i32 {
        self.y2 - self.y1
    }
    /// Half-open containment test (used for debug hit-testing).
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x1 && x < self.x2 && y >= self.y1 && y < self.y2
    }
}

/// A captured BGRA pixel buffer + its size. Top-left = the requested rect's
/// origin, so pixel `(px, py)` in the frame is client coordinate
/// `(rect.x1 + px, rect.y1 + py)`.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: i32,
    pub height: i32,
    /// BGRA, row-major, top-down, `width * height * 4` bytes.
    pub bgra: Vec<u8>,
}

impl Frame {
    #[inline]
    pub fn pixel(&self, x: i32, y: i32) -> Option<[u8; 4]> {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return None;
        }
        let off = ((y * self.width + x) * 4) as usize;
        if off + 3 >= self.bgra.len() {
            return None;
        }
        let s = &self.bgra[off..off + 4];
        Some([s[0], s[1], s[2], s[3]])
    }

    /// The whole-frame rect (client coordinates).
    pub fn rect(&self) -> Rect {
        Rect {
            x1: 0,
            y1: 0,
            x2: self.width,
            y2: self.height,
        }
    }
}

/// Captures rectangles of a target window's client area.
pub trait Capturer {
    fn capture(&self, rect: Rect) -> Result<Frame>;
}

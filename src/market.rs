//! Market price reader.
//!
//! Finds every `Open` button in the "Most Recent Offers" list, and for each one
//! reads the price digits in the band to its left — assembling them left-to-right
//! into the full number (comma separators produce no candidate, so they are
//! ignored automatically).
//!
//! The reading algorithm mirrors ABI TRADER: collect every digit-template match
//! in the band, then keep the best candidate per glyph. Instead of ABI's
//! fixed-x-distance clustering (which merges neighbours when glyphs are tightly
//! spaced), we use greedy non-max suppression by error on the glyph box, which is
//! robust to tight spacing and to the many near-duplicate offsets a tolerant
//! match produces.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::capture::{Frame, Rect};
use crate::search::template::{self, MatchWithError, Template};
use crate::search::Match;

/// Per-run market scanner configuration (`market.toml`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MarketConfig {
    pub window: WindowCfg,
    pub scan: ScanCfg,
    #[serde(default)]
    pub name: TextRegion,
    #[serde(default = "TextRegion::seller")]
    pub seller: TextRegion,
    #[serde(default = "TextRegion::quantity")]
    pub quantity: TextRegion,
    #[serde(default)]
    pub icon: IconCfg,
    #[serde(default)]
    pub watch: RectCfg,
    #[serde(default)]
    pub refresh: RefreshCfg,
    pub output: OutputCfg,
    pub hotkeys: HotkeysCfg,
    /// Optional push of the collected offers to the online dashboard.
    #[serde(default)]
    pub cloud: crate::cloud::CloudConfig,
    /// Optional auto-buy controller (reads the rules the manager writes).
    #[serde(default)]
    pub buy: crate::buy::BuyConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WindowCfg {
    #[serde(rename = "title_contains")]
    pub title_contains: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScanCfg {
    /// Per-channel pixel tolerance for the Open button match.
    pub open_tolerance: u8,
    /// Per-channel pixel tolerance for the digit glyphs.
    pub digit_tolerance: u8,
    /// Digit band: pixels left of the Open button to begin scanning.
    pub digit_left: i32,
    /// Digit band: gap (px) between the band's right edge and the button.
    pub digit_gap: i32,
    /// Digit band: vertical padding (px) above/below the button row.
    pub pad_y: i32,
    /// Open matches within this (x,y) distance collapse into one button.
    pub open_merge_px: i32,
    /// Two digit candidates whose boxes overlap by more than this fraction are
    /// treated as the same glyph (0.0..1.0).
    pub overlap_ratio: f64,
}

/// Which colour of text a region holds, for binarization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextRule {
    /// Orange item text (item names) on the dark panel.
    Orange,
    /// Bright/white text (seller/shop names).
    Bright,
    /// Bright-yellow stack count drawn on the item icon.
    Yellow,
}

impl Default for TextRule {
    fn default() -> Self {
        TextRule::Orange
    }
}

/// A text region relative to each Open button's matched rect (client px):
///   x1 = open.x - left     x2 = open.x - right
///   y1 = open.y + y_offset  y2 = y1 + height
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextRegion {
    pub enable: bool,
    pub left: i32,
    pub right: i32,
    pub y_offset: i32,
    pub height: i32,
    /// Integer upscale factor applied before OCR (helps the tiny bitmap font).
    pub scale: i32,
    /// White margin (px, before upscaling) added around the binarized text.
    /// Measurably improves OCR on this client's font (recovers the "$" in "$5 Bond").
    pub pad: i32,
    #[serde(default)]
    pub rule: TextRule,
}

impl TextRegion {
    /// Item-name region (orange text, measured at x 96..192, y open.y+2..+15).
    pub const fn item_name() -> Self {
        Self {
            enable: true,
            left: 305,   // x1 = 92
            right: 195,  // x2 = 202
            y_offset: 2, // y1 = open.y + 2
            height: 18,
            scale: 4,
            pad: 6,
            rule: TextRule::Orange,
        }
    }

    /// Stack-count region (yellow text drawn on the icon, top-left of the cell).
    /// Measured at x 56..71, y = open.y-8 .. open.y-1.
    pub const fn quantity() -> Self {
        Self {
            enable: true,
            left: 372,    // x1 = open.x - 372 = 25 (multi-digit counts reach this far left)
            right: 308,   // x2 = 89 (wide enough not to clip a long count)
            // The count's offset from the Open button drifts between frames (I've
            // seen it at open.y-8 and at open.y+1), so cover the whole top half of
            // the cell rather than trusting a tight offset. The strict yellow rule
            // is what keeps the item icon out of the mask.
            y_offset: -12,
            height: 22,
            scale: 4,
            pad: 6,
            rule: TextRule::Yellow,
        }
    }

    /// Seller/shop region (bright text), to the right of the price column.
    pub const fn seller() -> Self {
        Self {
            enable: true,
            left: 95,    // x1 = 302
            right: 15,   // x2 = 382
            y_offset: 2, // y1 = open.y + 2
            height: 15,
            scale: 4,
            pad: 6,
            rule: TextRule::Bright,
        }
    }
}

impl Default for TextRegion {
    fn default() -> Self {
        Self::item_name()
    }
}

/// Item-icon region (relative to the button) + where to save the crops.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IconCfg {
    pub enable: bool,
    pub left: i32,
    pub right: i32,
    pub y_offset: i32,
    pub height: i32,
    /// Directory (relative to the exe) for `<item name>.png` crops.
    pub dir: String,
    /// Trim the crop to the icon's non-background bounding box.
    pub trim: bool,
    /// Make the panel background transparent in the saved PNG.
    pub remove_bg: bool,
    /// Per-channel tolerance for treating a pixel as background. The panel has two
    /// flat shades ~8 apart, so this single tolerance covers both.
    pub bg_tolerance: u8,
}

impl Default for IconCfg {
    fn default() -> Self {
        Self {
            enable: true,
            // The icon cell, started just INSIDE its border lines so the crop
            // doesn't drag a strip of frame along: the cell's light border sits at
            // x=44 and at open.y-10, so we begin at x=45 / open.y-9. Icons bottom
            // out at open.y+21. Trim then tightens to the sprite.
            left: 352,     // x1 = 45
            right: 302,    // x2 = 95
            y_offset: -9,  // y1 = open.y - 9
            height: 31,    // y2 = open.y + 21
            dir: "data/icons".to_string(),
            trim: true,
            remove_bg: true,
            bg_tolerance: 6,
        }
    }
}

/// A plain client-coordinate rectangle (used for the change-watch region).
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RectCfg {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl Default for RectCfg {
    fn default() -> Self {
        // The offers list text columns only (names + prices + shops) — never the
        // animated game world, chat or minimap.
        Self {
            x1: 92,
            y1: 100,
            x2: 390,
            y2: 326,
        }
    }
}

/// Refresh-loop timing + the Refresh button match tolerance.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshCfg {
    /// How often to poll for a change after clicking Refresh.
    pub poll_ms: u64,
    /// Give up waiting for a change after this long and click Refresh again.
    pub timeout_ms: u64,
    /// Per-channel tolerance for matching Refresh.png.
    pub tolerance: u8,
}

impl Default for RefreshCfg {
    fn default() -> Self {
        Self {
            poll_ms: 5,
            timeout_ms: 2000,
            tolerance: 25,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputCfg {
    /// Latest-scan snapshot, relative to the exe dir.
    pub file: String,
    /// Append-one-line-per-scan history, relative to the exe dir.
    pub history: String,
    /// Append-one-line-per-new-row change log (for dashboards).
    #[serde(default = "default_changes_path")]
    pub changes: String,
    /// Per-row candidate dump for diagnosing missed/misread digits.
    #[serde(default = "default_debug_path")]
    pub debug: String,
    /// Save the captured (overlay-free) frame here as a PNG.
    #[serde(default = "default_frame_path")]
    pub frame: String,
    /// Whether to save the captured frame PNG each scan.
    #[serde(default = "default_true")]
    pub save_frame: bool,
}

fn default_changes_path() -> String {
    "data/market_changes.jsonl".to_string()
}
fn default_debug_path() -> String {
    "data/market_debug.json".to_string()
}
fn default_frame_path() -> String {
    "data/last_scan.png".to_string()
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HotkeysCfg {
    pub scan: String,
    /// Toggle the refresh→wait→scan loop.
    #[serde(rename = "loop")]
    pub loop_key: String,
    pub debug: String,
    pub quit: String,
}

impl MarketConfig {
    pub fn load_from(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let cfg: MarketConfig = toml::from_str(&text).context("invalid market.toml")?;
        Ok(cfg)
    }

    pub fn default_config() -> Self {
        Self {
            window: WindowCfg {
                title_contains: "Elorin".to_string(),
            },
            scan: ScanCfg {
                open_tolerance: 20,
                digit_tolerance: 35,
                digit_left: 520,
                digit_gap: 4,
                pad_y: 3,
                open_merge_px: 6,
                overlap_ratio: 0.5,
            },
            name: TextRegion::item_name(),
            seller: TextRegion::seller(),
            quantity: TextRegion::quantity(),
            icon: IconCfg::default(),
            watch: RectCfg::default(),
            refresh: RefreshCfg::default(),
            cloud: crate::cloud::CloudConfig::default(),
            buy: crate::buy::BuyConfig::default(),
            output: OutputCfg {
                file: "data/market_prices.json".to_string(),
                history: "data/market_history.jsonl".to_string(),
                changes: default_changes_path(),
                debug: default_debug_path(),
                frame: default_frame_path(),
                save_frame: true,
            },
            hotkeys: HotkeysCfg {
                scan: "f8".to_string(),
                loop_key: "f7".to_string(),
                debug: "f9".to_string(),
                quit: "f10".to_string(),
            },
        }
    }
}

/// Loaded market sprites: the Open button, the (optional) Refresh button, and
/// the ten digit glyphs, plus the sprites the buy flow needs.
pub struct MarketTemplates {
    pub open: Template,
    pub refresh: Option<Template>,
    pub digits: Vec<(u8, Template)>,
    /// Digit templates for the stack count: (value, scale-normalised coverage,
    /// number of enclosed holes). Holes are a scale-independent cue that
    /// separates look-alikes such as `8` (two) from `3` (none).
    pub quantity_shapes: Vec<(u8, Vec<f32>, u32)>,
    /// Chat-price confirmation digits (`{d}.{d}.png`). Drawn in the same font as
    /// the market prices but at a different colour/scale, so they need their own
    /// templates. Used by the buy path to verify the glowing item's price before
    /// the right-click.
    pub chat_digits: Vec<(u8, Template)>,
    /// `Buttons/Buy 1.png` — buy a single item.
    pub buy1: Option<Template>,
    /// `Buttons/Buy X.png` — buy a stack.
    pub buy_x: Option<Template>,
    /// `Validations/Enter amount.png` — the prompt that appears for a Buy X.
    pub enter_amount: Option<Template>,
}

/// Load `Sprites/Utility/Buttons/{Open,Refresh}.png` and
/// `Sprites/Utility/Market digits/0-9.png`.
pub fn load_templates(sprites_dir: &Path) -> Result<MarketTemplates> {
    let buttons = sprites_dir.join("Utility").join("Buttons");
    let digits_dir = sprites_dir.join("Utility").join("Market digits");

    let open_path = buttons.join("Open.png");
    let open = Template::load(&open_path)
        .with_context(|| format!("failed to load {}", open_path.display()))?;
    let refresh = Template::load(&buttons.join("Refresh.png")).ok();

    let mut digits = Vec::with_capacity(10);
    for d in 0..=9u8 {
        let path = digits_dir.join(format!("{d}.png"));
        let tpl =
            Template::load(&path).with_context(|| format!("failed to load {}", path.display()))?;
        digits.push((d, tpl));
    }

    // Chat confirmation digits are in the same folder but named `{d}.{d}.png`.
    // They are optional for scanning prices, but required for chat verification
    // in the buy path; load them best-effort and let callers fail explicitly if
    // they try to use an empty set.
    let mut chat_digits = Vec::with_capacity(10);
    for d in 0..=9u8 {
        let path = digits_dir.join(format!("{d}.{d}.png"));
        if let Ok(tpl) = Template::load(&path) {
            chat_digits.push((d, tpl));
        }
    }

    // Prefer sprites cropped from the COUNT font itself
    // (Sprites/Utility/Market count digits/0-9.png). It is a smaller font than the
    // price digits, and matching one against the other is only ~half reliable at
    // this size — the count font's own glyphs make the read exact.
    let count_digits = load_count_digits(sprites_dir);
    let shape_source: &[(u8, Template)] = count_digits.as_deref().unwrap_or(&digits);
    let quantity_shapes = shape_source
        .iter()
        .filter_map(|(d, t)| shape_from_template(t).map(|(s, holes)| (*d, s, holes)))
        .collect();

    Ok(MarketTemplates {
        open,
        refresh,
        digits,
        quantity_shapes,
        chat_digits,
        buy1: Template::load(&buttons.join("Buy 1.png")).ok(),
        buy_x: Template::load(&buttons.join("Buy X.png")).ok(),
        enter_amount: Template::load(&sprites_dir.join("Utility").join("Validations").join("Enter amount.png")).ok(),
    })
}

/// Load `Sprites/Utility/Market count digits/0-9.png` if all ten are present.
fn load_count_digits(sprites_dir: &Path) -> Option<Vec<(u8, Template)>> {
    let dir = sprites_dir.join("Utility").join("Market count digits");
    let mut out = Vec::with_capacity(10);
    for d in 0..=9u8 {
        let t = Template::load(&dir.join(format!("{d}.png"))).ok()?;
        out.push((d, t));
    }
    Some(out)
}

// -- Stack-count shape reader ------------------------------------------------
//
// The count is the same typeface as the price digits but drawn much smaller
// (~5x8, 1px strokes). Windows OCR ignores such tiny/isolated glyphs entirely
// (verified), so instead we segment the yellow glyphs and match their
// scale-normalised silhouette against the price digit sprites.

const QW: i32 = 16;
const QH: i32 = 24;

/// The ink bounding box of a mask, or None when empty.
fn ink_bbox(mask: &[bool], w: i32, h: i32) -> Option<(i32, i32, i32, i32)> {
    let (mut minx, mut miny, mut maxx, mut maxy) = (w, h, -1i32, -1i32);
    for y in 0..h {
        for x in 0..w {
            if mask[(y * w + x) as usize] {
                minx = minx.min(x);
                maxx = maxx.max(x);
                miny = miny.min(y);
                maxy = maxy.max(y);
            }
        }
    }
    if maxx < minx || maxy < miny {
        None
    } else {
        Some((minx, miny, maxx - minx + 1, maxy - miny + 1))
    }
}

/// Resample a bbox of a mask onto a QW x QH grid of ink coverage (0..1).
fn normalize(mask: &[bool], w: i32, h: i32, bbox: (i32, i32, i32, i32)) -> Vec<f32> {
    let (bx, by, bw, bh) = bbox;
    let mut out = vec![0f32; (QW * QH) as usize];
    for gy in 0..QH {
        for gx in 0..QW {
            let sx0 = bx + gx * bw / QW;
            let sx1 = (bx + (gx + 1) * bw / QW).max(sx0 + 1);
            let sy0 = by + gy * bh / QH;
            let sy1 = (by + (gy + 1) * bh / QH).max(sy0 + 1);
            let (mut ink, mut total) = (0i32, 0i32);
            for y in sy0..sy1 {
                for x in sx0..sx1 {
                    if x >= 0 && y >= 0 && x < w && y < h {
                        total += 1;
                        if mask[(y * w + x) as usize] {
                            ink += 1;
                        }
                    }
                }
            }
            out[(gy * QW + gx) as usize] = if total > 0 { ink as f32 / total as f32 } else { 0.0 };
        }
    }
    blur3(&out)
}

/// 3x3 box blur of a coverage grid. Smooths 1-cell misalignments (and the thin
/// gaps in the slashed-zero sprite) so shape comparison keys on structure rather
/// than exact cell positions.
fn blur3(v: &[f32]) -> Vec<f32> {
    let mut out = vec![0f32; v.len()];
    for y in 0..QH {
        for x in 0..QW {
            let (mut sum, mut count) = (0f32, 0f32);
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx >= 0 && ny >= 0 && nx < QW && ny < QH {
                        sum += v[(ny * QW + nx) as usize];
                        count += 1.0;
                    }
                }
            }
            out[(y * QW + x) as usize] = sum / count;
        }
    }
    out
}

/// Chamfer distance transform: for every cell, the approximate distance to the
/// nearest inked cell (2-pass, 8-connected).
fn chamfer(ink: &[bool]) -> Vec<f32> {
    const INF: f32 = 1e9;
    let mut d: Vec<f32> = ink.iter().map(|&i| if i { 0.0 } else { INF }).collect();
    let at = |x: i32, y: i32| -> Option<usize> {
        if x >= 0 && y >= 0 && x < QW && y < QH {
            Some((y * QW + x) as usize)
        } else {
            None
        }
    };
    for y in 0..QH {
        for x in 0..QW {
            let i = (y * QW + x) as usize;
            let mut v = d[i];
            if let Some(j) = at(x - 1, y) {
                v = v.min(d[j] + 1.0);
            }
            if let Some(j) = at(x, y - 1) {
                v = v.min(d[j] + 1.0);
            }
            if let Some(j) = at(x - 1, y - 1) {
                v = v.min(d[j] + 1.414);
            }
            if let Some(j) = at(x + 1, y - 1) {
                v = v.min(d[j] + 1.414);
            }
            d[i] = v;
        }
    }
    for y in (0..QH).rev() {
        for x in (0..QW).rev() {
            let i = (y * QW + x) as usize;
            let mut v = d[i];
            if let Some(j) = at(x + 1, y) {
                v = v.min(d[j] + 1.0);
            }
            if let Some(j) = at(x, y + 1) {
                v = v.min(d[j] + 1.0);
            }
            if let Some(j) = at(x + 1, y + 1) {
                v = v.min(d[j] + 1.414);
            }
            if let Some(j) = at(x - 1, y + 1) {
                v = v.min(d[j] + 1.414);
            }
            d[i] = v;
        }
    }
    d
}

/// Chamfer shape distance between a glyph and a template.
///
/// This is thickness-tolerant, which matters here: the count font is a smaller
/// bitmap font, so after scaling to the comparison grid its 1px strokes come out
/// noticeably fatter than the price sprite's. A coverage-difference metric
/// therefore punishes the *correct* template (it "misses" at the glyph's fat
/// strokes); nearest-ink distance does not care how thick either stroke is, only
/// where the shape runs. The reverse term is down-weighted so a template's extra
/// interior ink (the slashed zero) stays cheap.
fn shape_distance(g: &[f32], t: &[f32]) -> f32 {
    let gb: Vec<bool> = g.iter().map(|v| *v > 0.4).collect();
    let tb: Vec<bool> = t.iter().map(|v| *v > 0.4).collect();
    let (ng, nt) = (
        gb.iter().filter(|v| **v).count() as f32,
        tb.iter().filter(|v| **v).count() as f32,
    );
    if ng <= 0.0 || nt <= 0.0 {
        return f32::MAX;
    }
    let dt = chamfer(&tb);
    let dg = chamfer(&gb);
    let (mut fwd, mut bwd) = (0f32, 0f32);
    for i in 0..gb.len() {
        if gb[i] {
            fwd += dt[i].min(3.0);
        }
        if tb[i] {
            bwd += dg[i].min(3.0);
        }
    }
    fwd / ng + 0.6 * (bwd / nt)
}

fn shape_from_template(t: &Template) -> Option<(Vec<f32>, u32)> {
    let (w, h) = (t.width, t.height);
    if t.mask.len() < (w * h) as usize {
        return None;
    }
    let bbox = ink_bbox(&t.mask, w, h)?;
    Some((normalize(&t.mask, w, h, bbox), 0))
}

/// Read the stack count from the quantity region. Returns 1 when no digits are
/// present (the client shows no number for a single item).
pub fn read_quantity(frame: &Frame, region: Rect, shapes: &[(u8, Vec<f32>, u32)]) -> u32 {
    if shapes.is_empty() {
        return 1;
    }
    let w = region.width();
    let h = region.height();
    if w <= 0 || h <= 0 {
        return 1;
    }

    let mut mask = vec![false; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            if let Some(p) = frame.pixel(region.x1 + x, region.y1 + y) {
                mask[(y * w + x) as usize] = is_text_pixel(p, TextRule::Yellow);
            }
        }
    }

    // Segment into glyphs by column projection (a run of ink columns is a glyph).
    let mut col_ink = vec![false; w as usize];
    for x in 0..w {
        for y in 0..h {
            if mask[(y * w + x) as usize] {
                col_ink[x as usize] = true;
                break;
            }
        }
    }
    let mut runs: Vec<(i32, i32)> = Vec::new();
    let mut start = -1i32;
    for x in 0..w {
        if col_ink[x as usize] {
            if start < 0 {
                start = x;
            }
        } else if start >= 0 {
            runs.push((start, x));
            start = -1;
        }
    }
    if start >= 0 {
        runs.push((start, w));
    }
    if runs.is_empty() {
        return 1;
    }

    let mut digits = String::new();
    for (gx0, gx1) in runs {
        let mut sub = vec![false; (w * h) as usize];
        for y in 0..h {
            for x in gx0..gx1 {
                sub[(y * w + x) as usize] = mask[(y * w + x) as usize];
            }
        }
        let Some(bbox) = ink_bbox(&sub, w, h) else {
            continue;
        };
        let shape = normalize(&sub, w, h, bbox);
        let mut best = (0u8, f32::MAX);
        for (d, s, _) in shapes {
            let dist = shape_distance(&shape, s);
            if dist < best.1 {
                best = (*d, dist);
            }
        }
        digits.push((b'0' + best.0) as char);
    }
    if digits.is_empty() {
        return 1;
    }
    let n: u32 = digits.parse().unwrap_or(1);
    if n == 0 {
        1
    } else {
        n
    }
}

/// One detected digit glyph.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DigitHit {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub value: u8,
    pub error: u32,
}

/// The result of reading one row's price band.
#[derive(Debug, Clone)]
pub struct PriceRead {
    pub price: u64,
    /// The chosen glyph per position (left to right).
    pub chosen: Vec<DigitHit>,
    /// Every raw candidate found (for diagnostics).
    pub candidates: Vec<DigitHit>,
}

/// One market row: the Open button, the digit band scanned, and the price read.
#[derive(Debug, Clone)]
pub struct RowReading {
    pub open: Match,
    pub band: Rect,
    pub read: PriceRead,
    /// Item name read via OCR (empty when OCR is disabled or failed).
    pub name: String,
    /// Seller/shop name read via OCR (empty when disabled or failed).
    pub seller: String,
    /// Stack size drawn on the icon. 1 when the client shows no number.
    pub quantity: u32,
}

/// The result of one scan.
#[derive(Debug, Clone)]
pub struct ScanResult {
    pub rows: Vec<RowReading>,
    pub elapsed_ms: u128,
}

/// Fraction of the smaller box's width that two candidates overlap in x.
fn x_overlap_ratio(a: &DigitHit, b: &DigitHit) -> f64 {
    let overlap = ((a.x + a.w).min(b.x + b.w) - a.x.max(b.x)) as f64;
    if overlap <= 0.0 {
        return 0.0;
    }
    let min_w = a.w.min(b.w) as f64;
    if min_w <= 0.0 {
        0.0
    } else {
        overlap / min_w
    }
}

/// Collapse near-duplicate matches (one glyph matches at several offsets) into
/// unique buttons, sorted top-to-bottom then left-to-right.
pub fn merge_buttons(matches: &[MatchWithError], merge_px: i32) -> Vec<Match> {
    let mut out: Vec<Match> = Vec::new();
    for m in matches {
        let near = out
            .iter()
            .any(|k| (k.x - m.x).abs() <= merge_px && (k.y - m.y).abs() <= merge_px);
        if !near {
            out.push(Match {
                x: m.x,
                y: m.y,
                w: m.w,
                h: m.h,
            });
        }
    }
    out.sort_by(|a, b| a.y.cmp(&b.y).then_with(|| a.x.cmp(&b.x)));
    out
}

/// Read the price inside `band` using the market's digit glyphs.
/// Greedy non-max suppression by error: the best-scoring candidate is accepted
/// first and suppresses any candidate whose box overlaps it; survivors are
/// sorted by x and concatenated.
pub fn read_price(
    frame: &Frame,
    tpls: &MarketTemplates,
    band: Rect,
    digit_tolerance: u8,
    overlap_ratio: f64,
) -> PriceRead {
    read_price_with_digits(frame, &tpls.digits, band, digit_tolerance, overlap_ratio)
}

/// Generic price reader over any digit set.
fn read_price_with_digits(
    frame: &Frame,
    digits: &[(u8, Template)],
    band: Rect,
    digit_tolerance: u8,
    overlap_ratio: f64,
) -> PriceRead {
    let mut candidates: Vec<DigitHit> = Vec::new();
    for (value, tpl) in digits {
        for m in template::find_all_with_error(frame, tpl, band, digit_tolerance, 1) {
            candidates.push(DigitHit {
                x: m.x,
                y: m.y,
                w: m.w,
                h: m.h,
                value: *value,
                error: m.error,
            });
        }
    }

    // Best (lowest error) first; ties keep the earlier (left-most) candidate.
    candidates.sort_by_key(|c| c.error);

    let mut chosen: Vec<DigitHit> = Vec::new();
    for c in &candidates {
        let overlaps = chosen
            .iter()
            .any(|a| x_overlap_ratio(a, c) > overlap_ratio);
        if !overlaps {
            chosen.push(*c);
        }
    }

    chosen.sort_by_key(|c| c.x);
    let mut price: u64 = 0;
    for d in &chosen {
        price = price * 10 + d.value as u64;
    }

    PriceRead {
        price,
        chosen,
        candidates,
    }
}

/// Scan the chat box bottom-up in horizontal strips and return every price
/// found, ordered from bottom (newest) to top (oldest). Notifications may push
/// the price line upward; the caller should prefer the first entry.
///
/// Only the **rightmost digit cluster** of each strip is returned. The price is
/// always the last number on the line ("currently costs X coins"), and digits
/// earlier in the line — the "1" in "1 Hour", news killcounts, timestamps —
/// would otherwise be concatenated into it or suppress its glyphs during NMS.
/// A cluster is a run of chosen glyphs with no gap wider than `max_gap_px`;
/// the rightmost run wins.
pub fn read_chat_prices(
    frame: &Frame,
    tpls: &MarketTemplates,
    band: Rect,
    row_height: i32,
    tolerance: u8,
    overlap: f64,
) -> Vec<(i32, u64)> {
    read_chat_prices_with_gap(frame, tpls, band, row_height, tolerance, overlap, 24)
}

/// [`read_chat_prices`] with an explicit cluster gap (px). Exposed for tests.
pub fn read_chat_prices_with_gap(
    frame: &Frame,
    tpls: &MarketTemplates,
    band: Rect,
    row_height: i32,
    tolerance: u8,
    overlap: f64,
    max_gap_px: i32,
) -> Vec<(i32, u64)> {
    let mut out = Vec::new();
    let bw = band.x2 - band.x1;
    let bh = band.y2 - band.y1;
    if bw <= 0 || bh <= 0 {
        return out;
    }

    let mut y = band.y2;
    while y - row_height >= band.y1 {
        let strip = Rect {
            x1: band.x1,
            y1: y - row_height,
            x2: band.x2,
            y2: y,
        };
        let local = Rect {
            x1: 0,
            y1: strip.y1 - band.y1,
            x2: bw,
            y2: strip.y2 - band.y1,
        };
        let read = read_price_with_digits(frame, &tpls.chat_digits, local, tolerance, overlap);
        if let Some(price) = rightmost_cluster(&read.chosen, max_gap_px) {
            out.push((strip.y1, price));
        }
        y -= row_height;
    }
    out
}

/// The number formed by the rightmost run of glyphs with no internal gap wider
/// than `max_gap_px`. Returns None when `chosen` is empty.
fn rightmost_cluster(chosen: &[DigitHit], max_gap_px: i32) -> Option<u64> {
    if chosen.is_empty() {
        return None;
    }
    // `chosen` arrives sorted by x (see `read_price_with_digits`). Walk from the
    // right until a gap wider than the cluster allows.
    let mut start = chosen.len() - 1;
    while start > 0 {
        let gap = chosen[start].x - (chosen[start - 1].x + chosen[start - 1].w);
        if gap > max_gap_px {
            break;
        }
        start -= 1;
    }
    let mut price: u64 = 0;
    for d in &chosen[start..] {
        price = price * 10 + d.value as u64;
    }
    Some(price)
}

/// The text region for a button, in client coordinates.
pub fn text_region(open: &Match, cfg: &TextRegion) -> Rect {
    Rect {
        x1: open.x - cfg.left,
        y1: open.y + cfg.y_offset,
        x2: open.x - cfg.right,
        y2: open.y + cfg.y_offset + cfg.height,
    }
}

/// The item-icon region for a button, in client coordinates.
pub fn icon_region(open: &Match, cfg: &IconCfg) -> Rect {
    Rect {
        x1: open.x - cfg.left,
        y1: open.y + cfg.y_offset,
        x2: open.x - cfg.right,
        y2: open.y + cfg.y_offset + cfg.height,
    }
}

/// Whether a frame pixel is text under the given colour rule.
fn is_text_pixel(p: [u8; 4], rule: TextRule) -> bool {
    let b = p[0] as i32;
    let g = p[1] as i32;
    let r = p[2] as i32;
    match rule {
        // Orange item text over a dark brown panel.
        TextRule::Orange => r > 140 && g > 80 && (r - b) > 50,
        // Bright/white text (seller names).
        TextRule::Bright => r > 150 && g > 150 && b > 150,
        // The stack count is PURE yellow (255,255,0). Keep this strict: a looser
        // rule also matched golden item icons (e.g. a gilded helm), inventing
        // counts that weren't there.
        TextRule::Yellow => r > 235 && g > 235 && b < 70,
    }
}

/// Binarize a text region (text -> black, background -> white), add a white
/// margin, and upscale it, returning an opaque BGRA8 buffer ready for OCR.
fn render_text(frame: &Frame, r: Rect, scale: i32, pad: i32, rule: TextRule) -> (i32, i32, Vec<u8>) {
    let x1 = r.x1.max(0).min(frame.width);
    let y1 = r.y1.max(0).min(frame.height);
    let x2 = r.x2.max(0).min(frame.width);
    let y2 = r.y2.max(0).min(frame.height);
    let w = (x2 - x1).max(1);
    let h = (y2 - y1).max(1);
    let pad = pad.max(0);

    let pw = w + 2 * pad;
    let ph = h + 2 * pad;
    let mut img = image::RgbImage::from_pixel(pw as u32, ph as u32, image::Rgb([255, 255, 255]));
    for yy in 0..h {
        for xx in 0..w {
            let is_text = frame
                .pixel(x1 + xx, y1 + yy)
                .map(|p| is_text_pixel(p, rule))
                .unwrap_or(false);
            if is_text {
                img.put_pixel((xx + pad) as u32, (yy + pad) as u32, image::Rgb([0, 0, 0]));
            }
        }
    }

    let s = scale.max(1);
    let big = image::imageops::resize(
        &img,
        (pw * s) as u32,
        (ph * s) as u32,
        image::imageops::FilterType::CatmullRom,
    );
    let (bw, bh) = (big.width() as i32, big.height() as i32);
    let mut out = Vec::with_capacity((bw * bh * 4) as usize);
    for px in big.pixels() {
        out.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    (bw, bh, out)
}

/// Parse a stack-count string. The client shows no number for a single item, so
/// an empty/unreadable result means 1.
fn parse_quantity(s: &str) -> u32 {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().unwrap_or(1)
}

/// OCR one text region for a row (empty string when disabled or it fails).
fn read_region(frame: &Frame, ocr: Option<&crate::ocr::Ocr>, open: &Match, cfg: &TextRegion) -> String {
    let Some(ocr) = ocr else {
        return String::new();
    };
    if !cfg.enable {
        return String::new();
    }
    let r = text_region(open, cfg);
    let (w, h, buf) = render_text(frame, r, cfg.scale, cfg.pad, cfg.rule);
    match ocr.recognize_bgra(w, h, &buf) {
        Ok(t) => t.trim().to_string(),
        Err(e) => {
            warn!(y = open.y, region = ?r, error = %e, "OCR call failed");
            String::new()
        }
    }
}

/// Full scan: find the Open buttons, read each price, and (when an OCR engine is
/// supplied) read each item name.
pub fn scan(
    frame: &Frame,
    tpls: &MarketTemplates,
    cfg: &MarketConfig,
    ocr: Option<&crate::ocr::Ocr>,
) -> ScanResult {
    let start = std::time::Instant::now();

    let opens =
        template::find_all_with_error(frame, &tpls.open, frame.rect(), cfg.scan.open_tolerance, 1);
    let buttons = merge_buttons(&opens, cfg.scan.open_merge_px);

    let mut rows = Vec::with_capacity(buttons.len());
    for b in buttons {
        let band = Rect {
            x1: (b.x - cfg.scan.digit_left).max(0),
            y1: (b.y - cfg.scan.pad_y).max(0),
            x2: (b.x - cfg.scan.digit_gap).max(0),
            y2: (b.y + b.h + cfg.scan.pad_y).min(frame.height),
        };
        let read = read_price(
            frame,
            tpls,
            band,
            cfg.scan.digit_tolerance,
            cfg.scan.overlap_ratio,
        );

        let name = read_region(frame, ocr, &b, &cfg.name);
        let seller = read_region(frame, ocr, &b, &cfg.seller);
        // Stack count: shape-match the tiny yellow digits against the price digit
        // sprites. OCR is useless here (it ignores such tiny isolated glyphs), so
        // it is only a fallback when the sprites didn't load.
        let quantity = if tpls.quantity_shapes.is_empty() {
            parse_quantity(&read_region(frame, ocr, &b, &cfg.quantity))
        } else {
            read_quantity(frame, text_region(&b, &cfg.quantity), &tpls.quantity_shapes)
        };
        if cfg.name.enable && ocr.is_some() && name.is_empty() {
            warn!(y = b.y, "OCR returned an empty item name");
        }

        rows.push(RowReading {
            open: b,
            band,
            read,
            name,
            seller,
            quantity,
        });
    }

    ScanResult {
        rows,
        elapsed_ms: start.elapsed().as_millis(),
    }
}

#[derive(Serialize)]
struct RowOut {
    y: i32,
    open_x: i32,
    name: String,
    seller: String,
    price: u64,
    quantity: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
}

#[derive(Serialize)]
struct SnapshotOut {
    scanned_at_unix_ms: u128,
    row_count: usize,
    rows: Vec<RowOut>,
}

#[derive(Serialize)]
struct RowDebug {
    y: i32,
    open_x: i32,
    band: [i32; 4],
    name: String,
    seller: String,
    price: u64,
    quantity: u32,
    chosen: Vec<DigitHit>,
    candidates: Vec<DigitHit>,
}

#[derive(Serialize)]
struct DebugOut {
    scanned_at_unix_ms: u128,
    open_tolerance: u8,
    digit_tolerance: u8,
    rows: Vec<RowDebug>,
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Save a captured BGRA frame as a PNG (for offline diagnosis).
pub fn save_frame_png(path: &Path, frame: &Frame) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut img = image::RgbaImage::new(frame.width as u32, frame.height as u32);
    for (i, px) in img.pixels_mut().enumerate() {
        let off = i * 4;
        if off + 3 >= frame.bgra.len() {
            break;
        }
        *px = image::Rgba([
            frame.bgra[off + 2], // R
            frame.bgra[off + 1], // G
            frame.bgra[off],     // B
            frame.bgra[off + 3], // A
        ]);
    }
    img.save(path)
        .with_context(|| format!("failed to save {}", path.display()))?;
    Ok(())
}

/// Write the latest-scan snapshot (JSON), append one history line (JSONL), dump
/// per-row diagnostics, and (optionally) save the captured frame PNG.
/// `dir` is the base directory (the exe dir); config paths are relative to it.
pub fn write_outputs(
    dir: &Path,
    cfg: &MarketConfig,
    frame: &Frame,
    result: &ScanResult,
) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    let ts = now_ms();

    // Snapshot + history.
    // Screenshot each unique item name's icon (kept as-is if it already exists).
    let icon_dir = dir.join(&cfg.icon.dir);
    let rows: Vec<RowOut> = result
        .rows
        .iter()
        .map(|r| {
            let icon = if cfg.icon.enable && !r.name.is_empty() {
                crop_icon(frame, &r.open, &cfg.icon)
                    .and_then(|(w, h, buf)| save_icon(&icon_dir, &r.name, w, h, &buf).ok())
                    .and_then(|p| {
                        p.strip_prefix(dir)
                            .ok()
                            .map(|q| q.to_string_lossy().replace('\\', "/"))
                    })
            } else {
                None
            };
            RowOut {
                y: r.open.y,
                open_x: r.open.x,
                name: r.name.clone(),
                seller: r.seller.clone(),
                price: r.read.price,
                quantity: r.quantity,
                icon,
            }
        })
        .collect();
    let snapshot = SnapshotOut {
        scanned_at_unix_ms: ts,
        row_count: rows.len(),
        rows,
    };

    let snapshot_path = dir.join(&cfg.output.file);
    if let Some(parent) = snapshot_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&snapshot_path, serde_json::to_string_pretty(&snapshot)?)?;
    written.push(snapshot_path);

    let history_path = dir.join(&cfg.output.history);
    if let Some(parent) = history_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&history_path)?;
    writeln!(f, "{}", serde_json::to_string(&snapshot)?)?;
    written.push(history_path);

    // Debug dump of every candidate.
    let debug = DebugOut {
        scanned_at_unix_ms: ts,
        open_tolerance: cfg.scan.open_tolerance,
        digit_tolerance: cfg.scan.digit_tolerance,
        rows: result
            .rows
            .iter()
            .map(|r| RowDebug {
                y: r.open.y,
                open_x: r.open.x,
                band: [r.band.x1, r.band.y1, r.band.x2, r.band.y2],
                name: r.name.clone(),
                seller: r.seller.clone(),
                price: r.read.price,
                quantity: r.quantity,
                chosen: r.read.chosen.clone(),
                candidates: r.read.candidates.clone(),
            })
            .collect(),
    };
    let debug_path = dir.join(&cfg.output.debug);
    if let Some(parent) = debug_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&debug_path, serde_json::to_string_pretty(&debug)?)?;
    written.push(debug_path);

    // Captured frame PNG.
    if cfg.output.save_frame {
        let frame_path = dir.join(&cfg.output.frame);
        save_frame_png(&frame_path, frame)?;
        written.push(frame_path);
    }

    // Same rows for the online dashboard (already-queued offers are skipped).
    enqueue_cloud(&result.rows.iter().collect::<Vec<_>>(), ts as u64);

    Ok(written)
}

// ---------------------------------------------------------------------------
// Change detection, refresh button, item icons, and the change log
// ---------------------------------------------------------------------------

/// Cheap FNV-1a signature of a client-coordinate region. Stable while the offers
/// list is unchanged; differs the moment the list is updated.
pub fn signature(frame: &Frame, r: RectCfg) -> u64 {
    let x1 = r.x1.max(0).min(frame.width);
    let y1 = r.y1.max(0).min(frame.height);
    let x2 = r.x2.max(0).min(frame.width);
    let y2 = r.y2.max(0).min(frame.height);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for y in y1..y2 {
        for x in x1..x2 {
            if let Some(p) = frame.pixel(x, y) {
                for b in p {
                    h ^= b as u64;
                    h = h.wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
        }
    }
    h
}

/// The change-watch region as a capture `Rect`.
pub fn watch_rect(cfg: &RectCfg) -> Rect {
    Rect {
        x1: cfg.x1,
        y1: cfg.y1,
        x2: cfg.x2,
        y2: cfg.y2,
    }
}

/// Locate the Refresh button in a frame (whole-client search).
pub fn find_refresh(frame: &Frame, tpls: &MarketTemplates, tolerance: u8) -> Option<Match> {
    let tpl = tpls.refresh.as_ref()?;
    template::find(frame, tpl, tolerance)
}

/// Identity of a row for duplicate detection: (name, seller, price).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RowKey {
    pub name: String,
    pub seller: String,
    pub price: u64,
}

pub fn row_key(r: &RowReading) -> RowKey {
    RowKey {
        name: r.name.clone(),
        seller: r.seller.clone(),
        price: r.read.price,
    }
}

/// Indices of rows present now but not in `prev` (the previous scan).
pub fn new_row_indices(
    prev: &std::collections::HashSet<RowKey>,
    rows: &[RowReading],
) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| !prev.contains(&row_key(r)))
        .map(|(i, _)| i)
        .collect()
}

/// Crop the item icon for a row, optionally trimming to the sprite and keying out
/// the panel background (alpha 0 where the pixel is a panel shade).
/// Returns `(width, height, RGBA)` or None when disabled / out of bounds.
pub fn crop_icon(frame: &Frame, open: &Match, cfg: &IconCfg) -> Option<(i32, i32, Vec<u8>)> {
    if !cfg.enable {
        return None;
    }
    let r = icon_region(open, cfg);
    let x1 = r.x1.max(0).min(frame.width);
    let y1 = r.y1.max(0).min(frame.height);
    let x2 = r.x2.max(0).min(frame.width);
    let y2 = r.y2.max(0).min(frame.height);
    let w = x2 - x1;
    let h = y2 - y1;
    if w <= 0 || h <= 0 {
        return None;
    }

    // Background = the most common quantized colour in the box (the panel behind
    // the icon). Robust even when the icon fills much of the box — a corner sample
    // can land on the icon and would then clip it during the trim. The panel has
    // two flat shades only a few levels apart, so one tolerance covers both.
    // Palette of background colours, taken from the box's BORDER RING. The ring is
    // always panel (the item doesn't reach the edges), and because the crop spans
    // the row's shade boundary the ring contains both panel shades — so both get
    // keyed without the palette ever including the item's own colours.
    let mut ring: std::collections::HashMap<u32, (u32, [u8; 3])> =
        std::collections::HashMap::new();
    for yy in 0..h {
        for xx in 0..w {
            let on_border = xx == 0 || yy == 0 || xx == w - 1 || yy == h - 1;
            if !on_border {
                continue;
            }
            if let Some(p) = frame.pixel(x1 + xx, y1 + yy) {
                let key = ((p[2] as u32) << 16) | ((p[1] as u32) << 8) | p[0] as u32;
                let e = ring.entry(key).or_insert((0, [p[2], p[1], p[0]]));
                e.0 += 1;
            }
        }
    }
    let mut buckets: Vec<(u32, [u8; 3])> = ring.values().map(|&(count, col)| (count, col)).collect();
    buckets.sort_by(|a, b| b.0.cmp(&a.0));
    let total: u64 = buckets.iter().map(|b| b.0 as u64).sum();
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut acc: u64 = 0;
    for (count, col) in &buckets {
        palette.push(*col);
        acc += *count as u64;
        if palette.len() >= 24 || acc * 100 / total.max(1) >= 95 {
            break;
        }
    }

    let tol = cfg.bg_tolerance as i32;
    let is_bg = |p: [u8; 4]| -> bool {
        palette.iter().any(|c| {
            (p[2] as i32 - c[0] as i32).abs() <= tol
                && (p[1] as i32 - c[1] as i32).abs() <= tol
                && (p[0] as i32 - c[2] as i32).abs() <= tol
        })
    };

    let n = (w * h) as usize;
    let mut candidate = vec![false; n];
    for yy in 0..h {
        for xx in 0..w {
            let p = frame.pixel(x1 + xx, y1 + yy)?;
            candidate[(yy * w + xx) as usize] = cfg.remove_bg && is_bg(p);
        }
    }

    // Flood fill the background inward from the box border (4-connected). Only
    // background pixels CONNECTED to the border become transparent, so an item
    // whose own pixels happen to match the panel colour (a brown scroll) stays
    // intact — the sprite's outline stops the fill.
    let mut outside = vec![false; n];
    let mut stack: Vec<(i32, i32)> = Vec::with_capacity(n);
    for x in 0..w {
        stack.push((x, 0));
        stack.push((x, h - 1));
    }
    for y in 0..h {
        stack.push((0, y));
        stack.push((w - 1, y));
    }
    while let Some((x, y)) = stack.pop() {
        if x < 0 || y < 0 || x >= w || y >= h {
            continue;
        }
        let i = (y * w + x) as usize;
        if outside[i] || !candidate[i] {
            continue;
        }
        outside[i] = true;
        stack.push((x + 1, y));
        stack.push((x - 1, y));
        stack.push((x, y + 1));
        stack.push((x, y - 1));
    }

    // Trim to the item. A plain bbox is too fragile: a stray background-texture
    // pixel at the box edge would keep the whole box. Require at least 2 content
    // pixels on a row/column for it to count.
    let mut row_hits = vec![0u32; h as usize];
    let mut col_hits = vec![0u32; w as usize];
    for yy in 0..h {
        for xx in 0..w {
            if !outside[(yy * w + xx) as usize] {
                row_hits[yy as usize] += 1;
                col_hits[xx as usize] += 1;
            }
        }
    }
    const MIN_HITS: u32 = 2;
    let miny = (0..h).find(|&y| row_hits[y as usize] >= MIN_HITS);
    let maxy = (0..h).rev().find(|&y| row_hits[y as usize] >= MIN_HITS);
    let minx = (0..w).find(|&x| col_hits[x as usize] >= MIN_HITS);
    let maxx = (0..w).rev().find(|&x| col_hits[x as usize] >= MIN_HITS);

    let (cx1, cy1, cx2, cy2) = match (minx, miny, maxx, maxy) {
        (Some(a), Some(b), Some(c), Some(d)) if cfg.trim => (a, b, c + 1, d + 1),
        _ => (0, 0, w, h),
    };

    let cw = cx2 - cx1;
    let ch = cy2 - cy1;
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for yy in cy1..cy2 {
        for xx in cx1..cx2 {
            let p = frame.pixel(x1 + xx, y1 + yy)?;
            let a = if outside[(yy * w + xx) as usize] { 0 } else { 255 };
            out.extend_from_slice(&[p[2], p[1], p[0], a]); // RGBA
        }
    }
    Some((cw, ch, out))
}

/// A filesystem-safe file stem for an item name.
pub fn sanitize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    let s = out.trim_matches('_').to_string();
    if s.is_empty() {
        "unknown".to_string()
    } else {
        s
    }
}

/// Save an icon crop (RGBA) as `<dir>/<sanitized name>.png` (no-op if it exists).
pub fn save_icon(dir: &Path, name: &str, w: i32, h: i32, rgba: &[u8]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.png", sanitize_name(name)));
    if path.exists() {
        return Ok(path);
    }
    let mut img = image::RgbaImage::new(w as u32, h as u32);
    for (i, px) in img.pixels_mut().enumerate() {
        let off = i * 4;
        *px = image::Rgba([rgba[off], rgba[off + 1], rgba[off + 2], rgba[off + 3]]);
    }
    img.save(&path)?;
    Ok(path)
}

#[derive(Serialize)]
struct ChangeRecord {
    ts_unix_ms: u128,
    name: String,
    seller: String,
    price: u64,
    quantity: u32,
    y: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
}

/// Append one JSON line per new row to the changes log, saving its item icon.
pub fn append_changes(
    dir: &Path,
    cfg: &MarketConfig,
    frame: &Frame,
    rows: &[&RowReading],
) -> Result<Vec<PathBuf>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ts = now_ms();
    let icon_dir = dir.join(&cfg.icon.dir);
    let changes_path = dir.join(&cfg.output.changes);
    if let Some(parent) = changes_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&changes_path)?;

    for r in rows {
        let icon = if r.name.is_empty() {
            None
        } else {
            crop_icon(frame, &r.open, &cfg.icon)
                .and_then(|(w, h, buf)| save_icon(&icon_dir, &r.name, w, h, &buf).ok())
                .and_then(|p| {
                    p.strip_prefix(dir)
                        .ok()
                        .map(|q| q.to_string_lossy().replace('\\', "/"))
                })
        };
        let rec = ChangeRecord {
            ts_unix_ms: ts,
            name: r.name.clone(),
            seller: r.seller.clone(),
            price: r.read.price,
            quantity: r.quantity,
            y: r.open.y,
            icon,
        };
        writeln!(f, "{}", serde_json::to_string(&rec)?)?;
    }

    enqueue_cloud(rows, ts as u64);
    Ok(vec![changes_path])
}

/// Queue rows (and their icons) for the online dashboard. No-op unless the cloud
/// push is configured and enabled.
fn enqueue_cloud(rows: &[&RowReading], ts_ms: u64) {
    if !crate::cloud::enabled() {
        return;
    }
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        if r.name.is_empty() {
            continue;
        }
        let file = format!("{}.png", sanitize_name(&r.name));
        let _ = crate::cloud::enqueue_icon(&file);
        out.push(crate::cloud::CloudRow {
            ts_ms,
            name: r.name.clone(),
            seller: (!r.seller.is_empty()).then(|| r.seller.clone()),
            price: r.read.price,
            quantity: r.quantity,
            icon: Some(file),
        });
    }
    match crate::cloud::enqueue_rows(&out) {
        Ok(0) => {}
        Ok(n) => tracing::info!(rows = n, "queued offers for the cloud"),
        Err(e) => warn!(error = %e, "cloud enqueue failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Diagnostic: run the real icon crop on the last captured frame and write
    /// the results to `target/icons_sim/` for inspection.
    #[test]
    fn dump_icon_crops_if_frame_present() {
        let p = Path::new("data/last_scan.png");
        if !p.exists() {
            return;
        }
        let img = image::open(p).expect("open frame").to_rgba8();
        let (w, h) = (img.width() as i32, img.height() as i32);
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for (i, px) in img.pixels().enumerate() {
            let o = i * 4;
            bgra[o] = px[2];
            bgra[o + 1] = px[1];
            bgra[o + 2] = px[0];
            bgra[o + 3] = 255;
        }
        let frame = Frame { width: w, height: h, bgra };
        // Probe: x 38..100, y = open.y-22 .. open.y+22.
        let (bx1, bx2) = (38, 100);
        for y in [107, 140, 173, 206, 239, 272, 305] {
            let (by1, by2) = (y - 22, y + 22);
            // mode background colour over the probe box
            let mut hist: std::collections::HashMap<u32, (u32, u32, u32, u32)> =
                std::collections::HashMap::new();
            for py in by1..by2 {
                for px in bx1..bx2 {
                    if let Some(p) = frame.pixel(px, py) {
                        let key = (((p[0] >> 3) as u32) << 10)
                            | (((p[1] >> 3) as u32) << 5)
                            | ((p[2] >> 3) as u32);
                        let e = hist.entry(key).or_insert((0, 0, 0, 0));
                        e.0 += 1;
                        e.1 += p[2] as u32;
                        e.2 += p[1] as u32;
                        e.3 += p[0] as u32;
                    }
                }
            }
            let e = hist.values().max_by_key(|e| e.0).unwrap();
            let bg = [(e.1 / e.0) as u8, (e.2 / e.0) as u8, (e.3 / e.0) as u8, 255];

            let _ = bg;
        }

        // Save the real crop_icon output for inspection.
        let cfg = IconCfg::default();
        std::fs::create_dir_all("target/icons_sim").ok();
        for y in [107, 140, 173, 206, 239, 272, 305] {
            let open = Match { x: 397, y, w: 32, h: 19 };
            if let Some((cw, ch, buf)) = crop_icon(&frame, &open, &cfg) {
                let mut im = image::RgbaImage::new(cw as u32, ch as u32);
                for (i, px) in im.pixels_mut().enumerate() {
                    let o = i * 4;
                    *px = image::Rgba([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
                }
                // Composite over magenta so transparent areas are obvious.
                let mut comp = image::RgbaImage::from_pixel(
                    cw as u32,
                    ch as u32,
                    image::Rgba([255, 0, 255, 255]),
                );
                image::imageops::overlay(&mut comp, &im, 0, 0);
                let big = image::imageops::resize(
                    &comp,
                    (cw * 4) as u32,
                    (ch * 4) as u32,
                    image::imageops::FilterType::Nearest,
                );
                big.save(format!("target/icons_sim/y{y}.png")).ok();
                let opaque = im.pixels().filter(|p| p.0[3] > 0).count();
                eprintln!("y={y} icon {cw}x{ch}  opaque {opaque}/{}", cw * ch);
            }
        }
    }

    /// Diagnostic: read the stack count for each row of the last captured frame.
    #[test]
    fn dump_quantities_if_frame_present() {
        let p = Path::new("data/last_scan.png");
        if !p.exists() {
            return;
        }
        let img = image::open(p).expect("open frame").to_rgba8();
        let (w, h) = (img.width() as i32, img.height() as i32);
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for (i, px) in img.pixels().enumerate() {
            let o = i * 4;
            bgra[o] = px[2];
            bgra[o + 1] = px[1];
            bgra[o + 2] = px[0];
            bgra[o + 3] = 255;
        }
        let frame = Frame { width: w, height: h, bgra };
        let sprites = Path::new("Sprites");
        let tpls = match load_templates(sprites) {
            Ok(t) => t,
            Err(_) => return,
        };
        let cfg = MarketConfig::default_config();
        let open = Match { x: 397, y: 0, w: 32, h: 19 };
        for y in [107, 140, 173, 206, 239, 272, 305] {
            let b = Match { y, ..open };
            let region = text_region(&b, &cfg.quantity);
            let q = read_quantity(&frame, region, &tpls.quantity_shapes);

            // Ranked match margins for the first glyph, to judge confidence.
            let w = region.width();
            let h = region.height();
            let mut mask = vec![false; (w * h) as usize];
            for yy in 0..h {
                for xx in 0..w {
                    if let Some(p) = frame.pixel(region.x1 + xx, region.y1 + yy) {
                        mask[(yy * w + xx) as usize] = is_text_pixel(p, TextRule::Yellow);
                    }
                }
            }
            let mut ranks: Vec<(u8, f32)> = Vec::new();
            if let Some(bbox) = ink_bbox(&mask, w, h) {
                let shape = normalize(&mask, w, h, bbox);
                for (d, s, _) in &tpls.quantity_shapes {
                    ranks.push((*d, shape_distance(&shape, s)));
                }
                ranks.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            }
            let best = ranks.first().map(|r| format!("{}@{:.2}", r.0, r.1)).unwrap_or_default();
            let next = ranks.get(1).map(|r| format!("{}@{:.2}", r.0, r.1)).unwrap_or_default();
            eprintln!("y={y} quantity={q}   best={best} next={next}");
        }
    }

    /// Diagnostic: dump the yellow count mask for every row.
    #[test]
    fn dump_count_mask_if_frame_present() {
        let p = Path::new("data/last_scan.png");
        if !p.exists() {
            return;
        }
        let img = image::open(p).expect("open frame").to_rgba8();
        let (w, h) = (img.width() as i32, img.height() as i32);
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for (i, px) in img.pixels().enumerate() {
            let o = i * 4;
            bgra[o] = px[2];
            bgra[o + 1] = px[1];
            bgra[o + 2] = px[0];
            bgra[o + 3] = 255;
        }
        let frame = Frame { width: w, height: h, bgra };
        let cfg = MarketConfig::default_config();
        let open = Match { x: 397, y: 0, w: 32, h: 19 };
        for y in [107, 140, 173, 206, 239, 272, 305] {
            let b = Match { y, ..open };
            let region = text_region(&b, &cfg.quantity);
            eprintln!("=== y={y} region x {}..{} y {}..{} ===", region.x1, region.x2, region.y1, region.y2);
            for yy in region.y1..region.y2 {
                let mut line = String::from("  ");
                for xx in region.x1..region.x2 {
                    let hit = frame
                        .pixel(xx, yy)
                        .map(|p| is_text_pixel(p, TextRule::Yellow))
                        .unwrap_or(false);
                    line.push(if hit { '#' } else { '.' });
                }
                eprintln!("{line}");
            }
        }
    }

    /// Diagnostic: per-glyph masks + ranked matches for two known rows.
    #[test]
    fn dump_glyph_matches_if_frame_present() {
        let p = Path::new("data/last_scan.png");
        if !p.exists() {
            return;
        }
        let img = image::open(p).expect("open frame").to_rgba8();
        let (w, h) = (img.width() as i32, img.height() as i32);
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for (i, px) in img.pixels().enumerate() {
            let o = i * 4;
            bgra[o] = px[2];
            bgra[o + 1] = px[1];
            bgra[o + 2] = px[0];
            bgra[o + 3] = 255;
        }
        let frame = Frame { width: w, height: h, bgra };
        let tpls = match load_templates(Path::new("Sprites")) {
            Ok(t) => t,
            Err(_) => return,
        };
        let cfg = MarketConfig::default_config();
        let open = Match { x: 397, y: 0, w: 32, h: 19 };

        for y in [107] {
            let b = Match { y, ..open };
            let region = text_region(&b, &cfg.quantity);
            let rw = region.width();
            let rh = region.height();
            let mut mask = vec![false; (rw * rh) as usize];
            for yy in 0..rh {
                for xx in 0..rw {
                    if let Some(px) = frame.pixel(region.x1 + xx, region.y1 + yy) {
                        mask[(yy * rw + xx) as usize] = is_text_pixel(px, TextRule::Yellow);
                    }
                }
            }
            eprintln!("=== row y={y} ===");
            let mut runs: Vec<(i32, i32)> = Vec::new();
            let mut start = -1i32;
            let mut col_ink = vec![false; rw as usize];
            for x in 0..rw {
                for yy in 0..rh {
                    if mask[(yy * rw + x) as usize] {
                        col_ink[x as usize] = true;
                        break;
                    }
                }
            }
            for x in 0..rw {
                if col_ink[x as usize] {
                    if start < 0 {
                        start = x;
                    }
                } else if start >= 0 {
                    runs.push((start, x));
                    start = -1;
                }
            }
            if start >= 0 {
                runs.push((start, rw));
            }
            for (i, (gx0, gx1)) in runs.iter().enumerate() {
                let mut sub = vec![false; (rw * rh) as usize];
                for yy in 0..rh {
                    for xx in *gx0..*gx1 {
                        sub[(yy * rw + xx) as usize] = mask[(yy * rw + xx) as usize];
                    }
                }
                let Some(bbox) = ink_bbox(&sub, rw, rh) else { continue };
                for yy in bbox.1..bbox.1 + bbox.3 {
                    let mut line = String::from("    ");
                    for xx in bbox.0..bbox.0 + bbox.2 {
                        line.push(if sub[(yy * rw + xx) as usize] { '#' } else { '.' });
                    }
                    eprintln!("{line}");
                }
                let shape = normalize(&sub, rw, rh, bbox);
                let mut ranks: Vec<(u8, f32)> = tpls
                    .quantity_shapes
                    .iter()
                    .map(|(d, s, _)| (*d, shape_distance(&shape, s)))
                    .collect();
                ranks.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                eprintln!("    glyph{i} top: {:?}", &ranks[..4.min(ranks.len())]);

                if y == 107 && i == 2 {
                    let show = |label: &str, v: &[f32]| {
                        eprintln!("    {label}:");
                        for gy in 0..QH {
                            let mut line = String::from("      ");
                            for gx in 0..QW {
                                let c = v[(gy * QW + gx) as usize];
                                line.push(if c > 0.6 {
                                    '#'
                                } else if c > 0.25 {
                                    '+'
                                } else if c > 0.05 {
                                    '.'
                                } else {
                                    ' '
                                });
                            }
                            eprintln!("{line}");
                        }
                    };
                    show("glyph", &shape);
                    for (d, s, _) in &tpls.quantity_shapes {
                        if *d == 8 || *d == 3 {
                            show(&format!("template {d}"), s);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn shipped_market_toml_parses() {
        // The config the app loads at runtime must match the schema (deny_unknown_fields).
        let p = Path::new("market.toml");
        if p.exists() {
            MarketConfig::load_from(p).expect("market.toml must parse");
        }
    }

    #[test]
    fn new_row_indices_detects_only_new_rows() {
        let open = Match { x: 0, y: 0, w: 1, h: 1 };
        let mk = |name: &str, seller: &str, price: u64| RowReading {
            open,
            band: Rect { x1: 0, y1: 0, x2: 0, y2: 0 },
            read: PriceRead { price, chosen: vec![], candidates: vec![] },
            name: name.to_string(),
            seller: seller.to_string(),
            quantity: 1,
        };
        let rows = vec![mk("Ruby", "sellerA", 100), mk("Ruby", "sellerB", 100)];
        let mut prev: std::collections::HashSet<RowKey> = std::collections::HashSet::new();
        prev.insert(row_key(&rows[0]));
        // Only the second row (different seller) is new.
        assert_eq!(new_row_indices(&prev, &rows), vec![1]);
        // Same seller but different price also counts as new.
        prev.insert(row_key(&rows[1]));
        let rows2 = vec![mk("Ruby", "sellerA", 100), mk("Ruby", "sellerA", 150)];
        assert_eq!(new_row_indices(&prev, &rows2), vec![1]);
    }

    #[test]
    fn signature_changes_when_a_pixel_changes() {
        let cfg = RectCfg { x1: 0, y1: 0, x2: 4, y2: 4 };
        let mk = |v: u8| Frame { width: 4, height: 4, bgra: vec![v; 4 * 4 * 4] };
        assert_eq!(signature(&mk(0), cfg), signature(&mk(0), cfg));
        assert_ne!(signature(&mk(0), cfg), signature(&mk(1), cfg));
    }

    fn solid(w: i32, h: i32, rgb: (u8, u8, u8)) -> Template {
        Template {
            width: w,
            height: h,
            rgb: vec![rgb; (w * h) as usize],
            mask: vec![true; (w * h) as usize],
        }
    }

    fn frame(w: i32, h: i32) -> Frame {
        Frame {
            width: w,
            height: h,
            bgra: vec![0u8; (w * h * 4) as usize],
        }
    }

    fn put(frame: &mut Frame, x: i32, y: i32, w: i32, h: i32, rgb: (u8, u8, u8)) {
        for yy in y..y + h {
            for xx in x..x + w {
                let off = ((yy * frame.width + xx) * 4) as usize;
                frame.bgra[off] = rgb.2;
                frame.bgra[off + 1] = rgb.1;
                frame.bgra[off + 2] = rgb.0;
                frame.bgra[off + 3] = 255;
            }
        }
    }

    #[test]
    fn merge_buttons_collapses_duplicates() {
        let ms = vec![
            MatchWithError { x: 10, y: 10, w: 4, h: 4, error: 0 },
            MatchWithError { x: 12, y: 11, w: 4, h: 4, error: 5 },
            MatchWithError { x: 10, y: 50, w: 4, h: 4, error: 0 },
        ];
        let merged = merge_buttons(&ms, 6);
        assert_eq!(merged.len(), 2);
        assert_eq!((merged[0].x, merged[0].y), (10, 10));
        assert_eq!((merged[1].x, merged[1].y), (10, 50));
    }

    #[test]
    fn read_price_concatenates_left_to_right_with_gaps() {
        let mut f = frame(60, 20);
        put(&mut f, 5, 2, 3, 16, (255, 0, 0)); // digit 1
        put(&mut f, 20, 2, 3, 16, (0, 255, 0)); // digit 3
        put(&mut f, 40, 2, 3, 16, (0, 0, 255)); // digit 7

        let tpls = MarketTemplates {
            open: solid(4, 4, (1, 1, 1)),
            refresh: None,
            quantity_shapes: Vec::new(),
            chat_digits: Vec::new(),
            buy1: None,
            buy_x: None,
            enter_amount: None,
            digits: vec![
                (1, solid(3, 16, (255, 0, 0))),
                (3, solid(3, 16, (0, 255, 0))),
                (7, solid(3, 16, (0, 0, 255))),
            ],
        };
        let read = read_price(&f, &tpls, f.rect(), 0, 0.5);
        assert_eq!(read.price, 137);
        assert_eq!(read.chosen.len(), 3);
        assert!(read.chosen[0].x < read.chosen[1].x && read.chosen[1].x < read.chosen[2].x);
    }

    #[test]
    fn read_price_keeps_tightly_spaced_digits() {
        // Two adjacent glyphs only 4px apart (their boxes do NOT overlap, since
        // each is 3px wide) must both survive — the old x-distance clustering
        // would have merged them and dropped a digit.
        let mut f = frame(30, 20);
        put(&mut f, 5, 2, 3, 16, (255, 0, 0)); // "1"
        put(&mut f, 9, 2, 3, 16, (0, 255, 0)); // "3" (advance 4)

        let tpls = MarketTemplates {
            open: solid(4, 4, (1, 1, 1)),
            refresh: None,
            quantity_shapes: Vec::new(),
            chat_digits: Vec::new(),
            buy1: None,
            buy_x: None,
            enter_amount: None,
            digits: vec![
                (1, solid(3, 16, (255, 0, 0))),
                (3, solid(3, 16, (0, 255, 0))),
            ],
        };
        let read = read_price(&f, &tpls, f.rect(), 0, 0.5);
        assert_eq!(read.price, 13);
        assert_eq!(read.chosen.len(), 2);
    }

    #[test]
    fn read_price_picks_best_candidate_per_slot() {
        let mut f = frame(20, 20);
        put(&mut f, 5, 2, 3, 16, (100, 100, 100));
        let exact = solid(3, 16, (100, 100, 100)); // error 0
        let near = solid(3, 16, (110, 110, 110)); // within tol 35, error > 0
        let tpls = MarketTemplates {
            open: solid(4, 4, (1, 1, 1)),
            refresh: None,
            quantity_shapes: Vec::new(),
            chat_digits: Vec::new(),
            buy1: None,
            buy_x: None,
            enter_amount: None,
            digits: vec![(2, exact), (9, near)],
        };
        let read = read_price(&f, &tpls, f.rect(), 35, 0.5);
        assert_eq!(read.chosen.len(), 1);
        assert_eq!(read.price, 2); // exact match (digit 2) beats the near match (digit 9)
    }

    #[test]
    fn scan_finds_buttons_and_reads_prices() {
        let mut f = frame(200, 80);
        let open_rgb = (200, 180, 40);
        put(&mut f, 150, 10, 32, 19, open_rgb);
        put(&mut f, 100, 12, 4, 14, (255, 0, 0));
        put(&mut f, 112, 12, 4, 14, (0, 255, 0));
        put(&mut f, 150, 50, 32, 19, open_rgb);
        put(&mut f, 120, 52, 4, 14, (0, 0, 255));

        let tpls = MarketTemplates {
            open: solid(32, 19, open_rgb),
            refresh: None,
            quantity_shapes: Vec::new(),
            chat_digits: Vec::new(),
            buy1: None,
            buy_x: None,
            enter_amount: None,
            digits: vec![
                (5, solid(4, 14, (255, 0, 0))),
                (0, solid(4, 14, (0, 255, 0))),
                (7, solid(4, 14, (0, 0, 255))),
            ],
        };
        let mut cfg = MarketConfig::default_config();
        cfg.scan.digit_left = 120;
        cfg.scan.pad_y = 2;
        let res = scan(&f, &tpls, &cfg, None);
        assert_eq!(res.rows.len(), 2, "two Open buttons");
        assert_eq!(res.rows[0].read.price, 50);
        assert_eq!(res.rows[1].read.price, 7);
    }

    fn hit(x: i32, w: i32, value: u8) -> DigitHit {
        DigitHit { x, y: 0, w, h: 10, value, error: 0 }
    }

    #[test]
    fn rightmost_cluster_ignores_leading_digits() {
        // "1 Hour ... costs 9,000,000": the leading "1" sits 60px left of the
        // price cluster. The rightmost run must be 9000000, not 19000000.
        let chosen = vec![
            hit(0, 8, 1),
            hit(60, 9, 9),
            hit(70, 9, 0),
            hit(80, 9, 0),
            hit(90, 9, 0),
            hit(100, 9, 0),
            hit(110, 9, 0),
            hit(120, 9, 0),
        ];
        assert_eq!(rightmost_cluster(&chosen, 24), Some(9_000_000));
    }

    #[test]
    fn rightmost_cluster_keeps_tight_price_digits() {
        // Glyph advance ~10px: internal gaps of ~1px must not split the price.
        let chosen =
            vec![hit(60, 9, 9), hit(70, 9, 0), hit(80, 9, 0), hit(90, 9, 0)];
        assert_eq!(rightmost_cluster(&chosen, 24), Some(9_000));
    }

    #[test]
    fn rightmost_cluster_empty_is_none() {
        assert_eq!(rightmost_cluster(&[], 24), None);
    }
}

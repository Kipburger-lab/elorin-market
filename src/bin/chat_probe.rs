//! Chat-value probe — calibration for reading the price the game prints in the
//! chat when you left-click a shop item.
//!
//! Deliberately separate from `market_scanner`: the F8 path is tangled with the
//! market scan, and this needs to run while you click items in a shop and show
//! what it reads, live.
//!
//!     chat_probe [tolerance] [overlap] [row_height]
//!
//! Defaults are 30, 0.5 and 18. Run it, left-click items in a shop, and watch
//! the readings. Any strip that reads non-zero is written to `data/chat/`, so a
//! wrong reading can be diagnosed from the pixels rather than guessed at.
//!
//! The band is scanned **bottom-up** in row-height strips: the chat scrolls
//! upward, so the newest message is the lowest, and anything above it may be a
//! previous item's sale. Reading the band as one block would happily sum those
//! together.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use elorin_bot::capture::gdi::GdiCapturer;
use elorin_bot::capture::Rect;
use elorin_bot::market::{self, MarketConfig};
use elorin_bot::search::template::Template;
use elorin_bot::window;

/// Full visible chat box in fixed-mode RuneLite, in client coordinates.
const BAND: Rect = Rect { x1: 7, y1: 345, x2: 516, y2: 498 };

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let tolerance: u8 = args.next().and_then(|a| a.parse().ok()).unwrap_or(30);
    let overlap: f64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(0.5);
    let row_height: i32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(18);

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let cfg: MarketConfig = [exe_dir.join("market.toml"), PathBuf::from("market.toml")]
        .iter()
        .find(|p| p.exists())
        .and_then(|p| MarketConfig::load_from(p).ok())
        .unwrap_or_else(MarketConfig::default_config);

    let sprites = exe_dir.join("Sprites");
    let mut tpls = market::load_templates(&sprites)?;
    // Swap in the chat's own digits: a different rendering from the market's, so
    // they need their own templates. Anything missing is reported rather than
    // silently skipped — a half-loaded set reads nonsense.
    let digits_dir = sprites.join("Utility").join("Market digits");
    let mut chat_digits = Vec::new();
    for d in 0..=9u8 {
        let path = digits_dir.join(format!("{d}.{d}.png"));
        match Template::load(&path) {
            Ok(t) => chat_digits.push((d, t)),
            Err(e) => eprintln!("missing {}: {e}", path.display()),
        }
    }
    if chat_digits.is_empty() {
        anyhow::bail!("no chat digits loaded from {}", digits_dir.display());
    }
    println!("chat digits loaded: {}", chat_digits.len());
    // The reader takes its glyphs from the template set, so swap the chat's in.
    tpls.digits = chat_digits;

    let hwnd = unsafe { window::find_window(&cfg.window.title_contains) }
        .ok_or_else(|| anyhow::anyhow!("no Elorin window found"))?;
    let cap = GdiCapturer::new(hwnd);

    let out_dir = exe_dir.join("data").join("chat");
    let _ = std::fs::create_dir_all(&out_dir);

    println!(
        "watching the chat band {:?} bottom-up in {row_height}px strips \
         (tolerance {tolerance}, overlap {overlap})",
        BAND
    );
    println!("left-click items in a shop. every strip that reads is saved to {}\n", out_dir.display());

    let mut last: Option<(i32, u64)> = None;
    let mut last_save = Instant::now() - Duration::from_secs(5);
    // The buy path reads the chat with the chat digits and the market with the
    // market digits; compare both here so a wrong glyph set shows as a bad
    // read rather than a wrong number.
    let chat_digits = tpls.chat_digits.clone();
    let market_digits = tpls.digits.clone();
    loop {
        let Ok(frame) = cap.capture_region(BAND) else {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };

        // Report the *whole* band every poll, not only changes: the chat scrolls,
        // so the row holding the value moves upward as messages arrive, and
        // printing only when the number changed made a moving row look stuck.
        let mut hits: Vec<String> = Vec::new();
        // Bottom-up: the newest line is the lowest.
        let mut y = BAND.y2;
        while y - row_height >= BAND.y1 {
            let strip = Rect { x1: BAND.x1, y1: y - row_height, x2: BAND.x2, y2: y };
            let local = Rect {
                x1: 0,
                y1: strip.y1 - BAND.y1,
                x2: BAND.x2 - BAND.x1,
                y2: strip.y2 - BAND.y1,
            };
            for (label, digits) in [("chat", &chat_digits), ("mkt", &market_digits)] {
                let saved = std::mem::replace(&mut tpls.digits, digits.clone());
                let read = market::read_price(&frame, &tpls, local, tolerance, overlap);
                tpls.digits = saved;
                if read.price > 0 {
                    let glyphs: String =
                        read.chosen.iter().map(|d| char::from(b'0' + d.value)).collect();
                    hits.push(format!(
                        "{label} y{}..{}={} [{}] n={}",
                        strip.y1,
                        strip.y2,
                        read.price,
                        glyphs,
                        read.candidates.len()
                    ));

                    // Keep the pixels whenever a reading appears or changes: enough to
                    // diagnose a wrong reading, not one PNG per poll.
                    let changed = last != Some((strip.y1, read.price));
                    if changed && last_save.elapsed() > Duration::from_millis(500) {
                        last_save = Instant::now();
                        let tag2 = market::now_ms();
                        let path = out_dir.join(format!("{tag2}_y{}.png", strip.y1));
                        match market::save_frame_png(&path, &frame) {
                            Ok(()) => println!("      saved {}", path.display()),
                            Err(e) => eprintln!("      could not save: {e}"),
                        }
                    }
                    last = Some((strip.y1, read.price));
                }
            }
            y -= row_height;
        }
        if !hits.is_empty() {
            println!("{}", hits.join("   |   "));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

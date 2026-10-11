//! Elorin Bot — Market Scanner.
//!
//! - **F8** scans once: finds every `Open` button, reads the price digits to its
//!   left, OCRs the item name (left) and seller (right of the price), and writes a
//!   snapshot + debug dump + the captured frame.
//! - **F7** toggles the refresh loop: click Refresh → poll the offers list every
//!   few ms until it changes (or 2 s pass) → on a change, store only the rows that
//!   are new vs the previous scan (with the change timestamp and the item icon) →
//!   refresh again. No change ⇒ nothing is stored.
//! - **F9** toggles the debug boxes, **F10** quits.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use tracing::{debug, info, level_filters::LevelFilter, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use elorin_bot::capture::gdi::GdiCapturer;
use elorin_bot::capture::{Frame, Rect};
use elorin_bot::hotkeys::{self, Debounce};
use elorin_bot::input;
use elorin_bot::market::{self, MarketConfig, MarketTemplates, RowKey, RowReading, ScanResult};
use elorin_bot::ocr::Ocr;
use elorin_bot::overlay::{self, Painter, CYAN, GRAY, GREEN, MAGENTA, ORANGE, WHITE, YELLOW};
use elorin_bot::{request_exit, window, EXIT_REQUESTED};

struct ScanState {
    result: Option<ScanResult>,
    status: String,
    show_debug: bool,
}

static STATE: OnceLock<Mutex<ScanState>> = OnceLock::new();
static LOOP_ACTIVE: AtomicBool = AtomicBool::new(false);

fn state() -> &'static Mutex<ScanState> {
    STATE.get_or_init(|| {
        Mutex::new(ScanState {
            result: None,
            status: "ready - F8 scan, F7 loop".to_string(),
            show_debug: true,
        })
    })
}

struct App {
    cfg: MarketConfig,
    tpls: MarketTemplates,
    exe_dir: PathBuf,
    ocr: Option<Ocr>,
}

static APP: OnceLock<App> = OnceLock::new();

fn main() {
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::SetProcessDPIAware();
    }
    let _ = init_tracing();

    let outcome = run();
    if let Err(e) = &outcome {
        eprintln!("ERROR: {e:#}");
    }

    println!();
    println!("Elorin market scanner stopped.");
    println!("Press Enter to close this window...");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

fn run() -> Result<()> {
    info!("market scanner starting");

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let cwd = std::env::current_dir().ok();

    let cfg_path = resolve_config(&exe_dir, cwd.as_ref());
    let cfg = match cfg_path.as_deref() {
        Some(p) => MarketConfig::load_from(p).context("failed to load market.toml")?,
        None => MarketConfig::default_config(),
    };
    info!(?cfg, "config loaded");

    let sprites_dir = exe_dir.join("Sprites");
    let tpls = market::load_templates(&sprites_dir)
        .with_context(|| format!("failed to load sprites from {}", sprites_dir.display()))?;
    info!(
        digits = tpls.digits.len(),
        refresh = tpls.refresh.is_some(),
        buy1 = tpls.buy1.is_some(),
        buy_x = tpls.buy_x.is_some(),
        enter_amount = tpls.enter_amount.is_some(),
        "sprites loaded"
    );
    println!(
        "  sprites: {} digits · refresh {} · Buy 1 {} · Buy X {} · Enter amount {}",
        tpls.digits.len(),
        mark(tpls.refresh.is_some()),
        mark(tpls.buy1.is_some()),
        mark(tpls.buy_x.is_some()),
        tpls.enter_amount.is_some().then_some("ok").unwrap_or("MISSING"),
    );

    let needle = cfg.window.title_contains.clone();
    let scan_vk = hotkeys::parse_vk(&cfg.hotkeys.scan)
        .ok_or_else(|| anyhow!("unknown scan hotkey '{}'", cfg.hotkeys.scan))?;
    let loop_vk = hotkeys::parse_vk(&cfg.hotkeys.loop_key)
        .ok_or_else(|| anyhow!("unknown loop hotkey '{}'", cfg.hotkeys.loop_key))?;
    let debug_vk = hotkeys::parse_vk(&cfg.hotkeys.debug)
        .ok_or_else(|| anyhow!("unknown debug hotkey '{}'", cfg.hotkeys.debug))?;
    let quit_vk = hotkeys::parse_vk(&cfg.hotkeys.quit)
        .ok_or_else(|| anyhow!("unknown quit hotkey '{}'", cfg.hotkeys.quit))?;

    let ocr = match Ocr::new() {
        Ok(o) => {
            info!("Windows OCR engine ready");
            Some(o)
        }
        Err(e) => {
            warn!(error = %e, "OCR unavailable; item names will be blank");
            None
        }
    };

    // Online dashboard feed: rows are queued to disk and pushed by a background
    // thread, so scanning never waits on the network.
    let cloud_on = elorin_bot::cloud::configure(
        cfg.cloud.clone(),
        exe_dir.clone(),
        exe_dir.join(&cfg.icon.dir),
    );
    if !cloud_on && cfg.cloud.enable {
        warn!("cloud.enable is set but cloud.url / cloud.service_key is missing");
    }

    let _ = APP.set(App {
        cfg: cfg.clone(),
        tpls,
        exe_dir,
        ocr,
    });

    if cloud_on {
        elorin_bot::cloud::spawn_pusher();
        println!("  cloud:  pushing offers to {}", cfg.cloud.url);
    }

    // Auto-buy controller: reads the rules the manager wrote. Nothing is clicked
    // while dry_run is on.
    if elorin_bot::buy::configure(cfg.buy.clone()) {
        println!("  buy:    {}", elorin_bot::buy::arm_status());
        if !cfg.buy.dry_run {
            // A session is one scanner run: start it with fresh counters.
            if let Err(e) = elorin_bot::buy::begin_session() {
                warn!(error = %e, "could not reset the buy session counters");
            }
        }
    }

    // Refresh loop on its own thread.
    std::thread::Builder::new()
        .name("market-loop".into())
        .spawn(loop_thread)
        .expect("spawn loop thread");

    let paint: Arc<overlay::PaintFn> = Arc::new(|p: &Painter| paint(p));
    overlay::spawn(
        "ElorinBotMarketOverlayClass",
        "Elorin Bot Market Overlay",
        needle.clone(),
        paint,
    );

    println!("Elorin market scanner running.");
    println!(
        "  scan={}  loop={}  debug={}  quit={}",
        cfg.hotkeys.scan, cfg.hotkeys.loop_key, cfg.hotkeys.debug, cfg.hotkeys.quit
    );

    let debounce = Debounce::new();
    hotkeys::run(
        needle,
        vec![scan_vk, loop_vk, debug_vk, quit_vk],
        move |vk| {
            if vk == scan_vk {
                if debounce.ready(vk, 300) {
                    do_scan();
                }
            } else if vk == loop_vk {
                if debounce.ready(vk, 300) {
                    let now = !LOOP_ACTIVE.fetch_xor(true, Ordering::SeqCst);
                    info!(looping = now, "refresh loop toggled");
                    set_status(if now {
                        "loop ON - refreshing"
                    } else {
                        "loop OFF"
                    });
                }
            } else if vk == debug_vk {
                if debounce.ready(vk, 300) {
                    let mut st = state().lock();
                    st.show_debug = !st.show_debug;
                }
            } else if vk == quit_vk {
                request_exit();
            }
        },
    )
}

// ---------------------------------------------------------------------------
// One-shot scan (F8)
// ---------------------------------------------------------------------------

fn do_scan() {
    let app = match APP.get() {
        Some(a) => a,
        None => return,
    };
    let hwnd = match unsafe { window::find_window(&app.cfg.window.title_contains) } {
        Some(h) => h,
        None => {
            set_status("no Elorin window found");
            return;
        }
    };

    let capturer = GdiCapturer::new(hwnd);
    overlay::set_suspended(true);
    std::thread::sleep(Duration::from_millis(40));
    let capture = capturer.capture_full_client();
    overlay::set_suspended(false);

    let frame = match capture {
        Ok(f) => f,
        Err(e) => {
            set_status(&format!("capture failed: {e}"));
            return;
        }
    };

    let result = market::scan(&frame, &app.tpls, &app.cfg, app.ocr.as_ref());
    log_rows(&result);
    elorin_bot::buy::report(&result.rows);

    match market::write_outputs(&app.exe_dir, &app.cfg, &frame, &result) {
        Ok(paths) => info!(?paths, "wrote output"),
        Err(e) => warn!(error = %e, "failed to write output"),
    }

    let mut st = state().lock();
    st.status = format!("scanned {} rows in {}ms", result.rows.len(), result.elapsed_ms);
    st.result = Some(result);
}

fn log_rows(result: &ScanResult) {
    info!(
        rows = result.rows.len(),
        ms = result.elapsed_ms,
        "scan complete"
    );
    println!("scan: {} rows in {}ms", result.rows.len(), result.elapsed_ms);
    for r in &result.rows {
        println!(
            "  y={:>3}  price={:>12}  qty={:>5}  seller='{}'  name='{}'",
            r.open.y, r.read.price, r.quantity, r.seller, r.name
        );
    }
}

// ---------------------------------------------------------------------------
// Refresh -> wait for change -> store -> repeat loop (F7)
// ---------------------------------------------------------------------------

/// Evaluate the armed items against these rows and make at most one purchase.
///
/// Split out of the loop so the **first** pass can run it too. Seeding only
/// means "don't store the whole list as changes"; an armed item already on
/// screen when the scanner starts is every bit as buyable as one that arrives
/// later, and no "the list changed" signal ever fires for it.
fn try_buy(app: &App, hwnd: HWND, cap: &GdiCapturer, full: &Frame, rows: &[RowReading]) {
    elorin_bot::buy::report(rows);
    if app.cfg.buy.dry_run {
        return;
    }
    // At most one purchase per pass: clicking Open replaces the offers list, so
    // we buy, then let the next iteration rescan.
    let next = elorin_bot::buy::plan(rows).into_iter().next();
    if let Some((i, v)) = next {
        set_status(&format!("BUYING {} @ {}", v.name, v.price));
        let outcome = elorin_bot::buy::execute(
            &app.exe_dir,
            hwnd,
            cap,
            full,
            &rows[i],
            &v,
            &app.tpls,
            &app.cfg,
            app.ocr.as_ref(),
            &app.cfg.icon,
            &app.cfg.buy,
            &app.cfg.discord,
        );
        println!("  {}", outcome.describe());
        info!(outcome = %outcome.describe(), "auto-buy");
        set_status(&outcome.describe());
        // The **returning state**, run on every attempt — bought, gone, or
        // aborted half-way. It is what guarantees the client is back somewhere
        // the scanning state can recognise before the loop resumes.
        elorin_bot::buy::return_to_offers(&app.exe_dir, hwnd, cap, &app.tpls, &app.cfg.buy);
    }
}

/// A cheap fingerprint of a frame, to notice when the picture changes.
///
/// Samples a slice of the buffer rather than hashing all of it: this only has to
/// answer "is this the same picture as last time", and a stuck client's frames
/// are byte-identical.
fn frame_signature(f: &Frame) -> u64 {
    let mut h = 1469598103934665603u64;
    for b in f.bgra.iter().step_by(97) {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Record the client for `seconds`, into `data/stuck/<tag>/`.
///
/// Called when the loop finds itself somewhere it does not recognise and stays
/// there. A single still shows where we ended up; a sequence shows *how* we got
/// there, which is what a stuck state actually needs — the transition is the
/// interesting part, and it is gone by the time anyone looks.
///
/// Only frames that differ from the previous one are written: in a stuck state
/// most are identical, and repeated copies are pure waste. They are then folded
/// into a single small MP4 when ffmpeg is available.
fn record_client(cap: &GdiCapturer, exe_dir: &Path, seconds: u64, title_contains: &str) -> Option<PathBuf> {
    let tag = market::now_ms();
    let dir = exe_dir.join("data").join("stuck").join(tag.to_string());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(error = %e, "could not create the stuck recording dir");
        return None;
    }

    const FPS: u64 = 4;
    warn!(tag, seconds, dir = %dir.display(), "recording the client — the loop is stuck");

    let mut previous: Option<u64> = None;
    let mut saved = 0u32;
    let mut skipped = 0u32;
    for _ in 0..seconds * FPS {
        // The capturer BitBlts the screen at the client's rectangle, so a frame
        // only *is* the client while the client is on top. Skip anything else —
        // and count it, because the client losing focus is itself a candidate
        // cause of a stuck loop, and worth knowing about afterwards.
        let fg = unsafe { GetForegroundWindow() };
        if !unsafe { window::is_elorin_window(fg, title_contains) } {
            skipped += 1;
            std::thread::sleep(Duration::from_millis(1000 / FPS));
            continue;
        }

        if let Ok(f) = cap.capture_full_client() {
            let sig = frame_signature(&f);
            if previous != Some(sig) {
                previous = Some(sig);
                // Numbered contiguously from zero so ffmpeg's %04d pattern
                // reads them in order.
                let path = dir.join(format!("{tag}_{saved:04}.png"));
                match market::save_frame_png(&path, &f) {
                    Ok(()) => saved += 1,
                    Err(e) => {
                        warn!(error = %e, "could not save a stuck frame");
                        break;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1000 / FPS));
    }

    warn!(tag, frames = saved, skipped, "stuck recording finished");
    if skipped > 0 {
        warn!(
            "the client was not the focused window for {skipped} frame(s) — that alone can be why the loop stalled"
        );
    }
    if saved > 1 {
        if elorin_bot::record::encode_with_ffmpeg(&dir, &tag.to_string(), 4) {
            return Some(dir.join(format!("{tag}.mp4")));
        }
    }
    None
}

fn loop_thread() {
    let mut prev: HashSet<RowKey> = HashSet::new();
    let mut seeded = false;
    // Consecutive passes that couldn't find the Refresh button — the loop's
    // signal that it is somewhere it doesn't recognise. At ~300ms a pass this
    // is a few seconds, long enough to be a genuine stuck state rather than a
    // single bad frame.
    const STUCK_AFTER: u32 = 20;
    // Before recording a stuck episode, give recovery a chance: the market is
    // usually just closed, not broken.
    const REOPEN_AFTER: u32 = 4;
    let mut stuck: u32 = 0;

    while !EXIT_REQUESTED.load(Ordering::Relaxed) {
        // Tell the manager we're alive and which mode we're in (throttled).
        elorin_bot::buy::maybe_heartbeat();

        if !LOOP_ACTIVE.load(Ordering::SeqCst) {
            if seeded {
                overlay::set_suspended(false);
                seeded = false;
                prev.clear();
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        // The overlay must be hidden while we capture (it would otherwise end up
        // in the frames and change the change-signature).
        overlay::set_suspended(true);

        let app = match APP.get() {
            Some(a) => a,
            None => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };

        let hwnd = match unsafe { window::find_window(&app.cfg.window.title_contains) } {
            Some(h) => h,
            None => {
                set_status("loop: no Elorin window");
                std::thread::sleep(Duration::from_millis(300));
                continue;
            }
        };
        let fg = unsafe { GetForegroundWindow() };
        if !unsafe { window::is_elorin_window(fg, &app.cfg.window.title_contains) } {
            set_status("loop paused - focus the Elorin client");
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }

        let cap = GdiCapturer::new(hwnd);
        // Keep the OS pointer off the panel for every capture this pass.
        park_cursor(hwnd, &app.cfg);

        // Seed the "previous" set once so the first change doesn't store the
        // whole list that was already there.
        if !seeded {
            if let Ok(f) = cap.capture_full_client() {
                let r = market::scan(&f, &app.tpls, &app.cfg, app.ocr.as_ref());
                prev = r.rows.iter().map(market::row_key).collect();
                seeded = true;
                // Check what's already on screen before seeding ends the pass:
                // the opening scan is often the most useful one.
                try_buy(&app, hwnd, &cap, &f, &r.rows);
                set_result(r, "loop seeded - refreshing");
            }
            continue;
        }

        // 1. Click Refresh.
        let full = match cap.capture_full_client() {
            Ok(f) => f,
            Err(e) => {
                set_status(&format!("capture failed: {e}"));
                std::thread::sleep(Duration::from_millis(300));
                continue;
            }
        };
        match market::find_refresh(&full, &app.tpls, app.cfg.refresh.tolerance) {
            Some(m) => {
                stuck = 0;
                let (cx, cy) = m.center();
                match unsafe { window::client_to_screen(hwnd, cx, cy) } {
                    Some((sx, sy)) => {
                        input::click_screen(sx, sy);
                        // Lift the pointer off the button the instant the click
                        // lands, so its sprite is back before the next find.
                        park_cursor(hwnd, &app.cfg);
                    }
                    None => {
                        warn!("client_to_screen failed for refresh");
                        std::thread::sleep(Duration::from_millis(300));
                        continue;
                    }
                }
            }
            None => {
                set_status("loop: Refresh button not found");
                stuck += 1;
                // The client drops out of the market window when a purchase
                // empties a shop or the buy is refused. Before recording that as
                // a stuck episode, try to put the offers list back: click
                // "View Recent Offers" if the market is on another page, or
                // reopen the market through a clerk if it closed entirely.
                if stuck >= REOPEN_AFTER {
                    if elorin_bot::buy::recover_market(hwnd, &cap, &app.tpls, &app.cfg.buy) {
                        info!("loop: market recovery attempted; rescanning");
                        stuck = 0;
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                }
                // Not recoverable and not moving: record a window of the client
                // so whatever happened can be reviewed afterwards. Once per
                // episode, not on every pass.
                if stuck == STUCK_AFTER {
                    let video = record_client(&cap, &app.exe_dir, 30, &app.cfg.window.title_contains);
                    elorin_bot::discord::notify_stuck(app.cfg.discord.clone(), video);
                    stuck = 0;
                }
                std::thread::sleep(Duration::from_millis(300));
                continue;
            }
        }

        // 2. Poll the watch region until it changes (or the timeout expires).
        //    No change means the refresh brought nothing new, so the loop clicks
        //    Refresh again and this window is what paces the idle refresh rate:
        //    a shorter window refreshes more often, but must still be long enough
        //    for a render to land, or the change is missed and clicked over.
        let wr = market::watch_rect(&app.cfg.watch);
        let baseline = match cap.capture_region(wr) {
            Ok(f) => market::signature(&f, app.cfg.watch),
            Err(_) => continue,
        };
        let poll = Duration::from_millis(app.cfg.refresh.poll_ms.max(1));
        let started = Instant::now();
        let deadline = started + Duration::from_millis(app.cfg.refresh.timeout_ms);
        let mut changed = false;
        while Instant::now() < deadline
            && LOOP_ACTIVE.load(Ordering::SeqCst)
            && !EXIT_REQUESTED.load(Ordering::Relaxed)
        {
            std::thread::sleep(poll);
            if let Ok(f) = cap.capture_region(wr) {
                if market::signature(&f, app.cfg.watch) != baseline {
                    changed = true;
                    break;
                }
            }
        }

        if !changed {
            debug!(ms = started.elapsed().as_millis() as u64, "refresh window closed with no change");
            set_status("loop: no change in time - refreshing again");
            continue;
        }
        debug!(ms = started.elapsed().as_millis() as u64, "refresh changed");

        // 3. A change landed: full scan and store only the new rows.
        let full = match cap.capture_full_client() {
            Ok(f) => f,
            Err(_) => continue,
        };
        let r = market::scan(&full, &app.tpls, &app.cfg, app.ocr.as_ref());
        try_buy(&app, hwnd, &cap, &full, &r.rows);
        // Keep the frame we actually read, so a wrong reading can be diagnosed
        // later (the loop doesn't otherwise write a frame).
        let _ = market::save_frame_png(&app.exe_dir.join(&app.cfg.output.frame), &full);
        let idxs = market::new_row_indices(&prev, &r.rows);
        let new_rows: Vec<&RowReading> = idxs.iter().map(|&i| &r.rows[i]).collect();

        if !new_rows.is_empty() {
            match market::append_changes(&app.exe_dir, &app.cfg, &full, &new_rows) {
                Ok(paths) => info!(count = new_rows.len(), ?paths, "stored changes"),
                Err(e) => warn!(error = %e, "append_changes failed"),
            }
            println!("change: {} new row(s)", new_rows.len());
            for row in &new_rows {
                println!(
                    "  price={:>12}  qty={:>5}  seller='{}'  name='{}'",
                    row.read.price, row.quantity, row.seller, row.name
                );
            }
        }

        prev = r.rows.iter().map(market::row_key).collect();
        let status = format!("loop: {} new row(s) stored", new_rows.len());
        set_result(r, &status);
    }

    overlay::set_suspended(false);
}

fn set_result(result: ScanResult, status: &str) {
    let mut st = state().lock();
    st.status = status.to_string();
    st.result = Some(result);
}

fn set_status(msg: &str) {
    state().lock().status = msg.to_string();
}

/// Park the pointer on the configured inert spot.
///
/// BitBlt captures the OS cursor too, so a pointer left resting on the Refresh
/// button can cover enough of its sprite that the next `find_refresh` misses it
/// — and one left over the offer rows bleeds into the watch region and reads as
/// a change that never happened. Run this before each capture, and again after
/// the Refresh click, so the pointer is never on the panel.
fn park_cursor(hwnd: HWND, cfg: &MarketConfig) {
    let [px, py] = cfg.buy.recover_client;
    if let Some((rx, ry)) = unsafe { window::client_to_screen(hwnd, px, py) } {
        input::set_recover_pos(rx, ry);
        input::recover();
    }
}

// ---------------------------------------------------------------------------
// Overlay
// ---------------------------------------------------------------------------

fn paint(p: &Painter) {
    let st = state().lock();
    let loop_txt = if LOOP_ACTIVE.load(Ordering::Relaxed) {
        "LOOP ON"
    } else {
        "loop off"
    };
    p.text(
        8,
        6,
        &format!("Elorin market scanner  [{loop_txt}]   F8 scan   F7 loop   F9 debug   F10 quit"),
        WHITE,
    );
    p.text(8, 22, &st.status, CYAN);

    if let Some(result) = &st.result {
        for row in &result.rows {
            let o = row.open;
            // Open button.
            p.stroke(
                Rect { x1: o.x, y1: o.y, x2: o.x + o.w, y2: o.y + o.h },
                MAGENTA,
                2,
            );
            // Digit band (thin gray) + item-icon + name + seller regions.
            p.stroke(row.band, GRAY, 1);
            if let Some(app) = APP.get() {
                if app.cfg.icon.enable {
                    p.stroke(market::icon_region(&o, &app.cfg.icon), ORANGE, 1);
                }
                p.stroke(market::text_region(&o, &app.cfg.name), CYAN, 1);
                p.stroke(market::text_region(&o, &app.cfg.seller), GREEN, 1);
            }
            if st.show_debug {
                for d in &row.read.chosen {
                    p.stroke(
                        Rect { x1: d.x, y1: d.y, x2: d.x + d.w, y2: d.y + d.h },
                        GREEN,
                        1,
                    );
                }
            }
            // Price + qty + OCR'd name/seller, right of the button.
            p.text(
                o.x + o.w + 6,
                o.y,
                &format!("{}  x{}", row.read.price, row.quantity),
                YELLOW,
            );
            p.text(
                o.x + o.w + 6,
                o.y + 16,
                &format!("{} | {}", row.name, row.seller),
                CYAN,
            );
        }
    }
}

/// "ok" / "MISSING", for the startup sprite report.
fn mark(present: bool) -> &'static str {
    if present {
        "ok"
    } else {
        "MISSING"
    }
}

fn resolve_config(exe_dir: &Path, cwd: Option<&PathBuf>) -> Option<PathBuf> {
    let p = exe_dir.join("market.toml");
    if p.exists() {
        return Some(p);
    }
    if let Some(d) = cwd {
        let p = d.join("market.toml");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn init_tracing() -> Result<()> {
    let file_appender = tracing_appender::rolling::daily("logs", "market_scanner.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    std::mem::forget(guard);

    let env = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(env)
        .with(fmt::layer().with_target(false).with_writer(std::io::stderr))
        .with(fmt::layer().with_ansi(false).with_writer(non_blocking))
        .init();
    Ok(())
}

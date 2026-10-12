//! Send a test evidence bundle to the configured Discord webhook, using an
//! existing run's footage and debug frames.
//!
//! Usage: `cargo run --release --example discord_test [run_id]`
//!
//! It drives the same `discord::notify_run` the buy loop uses, so it exercises
//! the real formatting: the readable timeline embed, the `<run_id>.log`
//! attachment, the debug images, and the separate recording post.

use std::path::{Path, PathBuf};
use std::time::Duration;

use elorin_bot::annotate;
use elorin_bot::capture::{Frame, Rect};
use elorin_bot::discord::{self, RunSummary};
use elorin_bot::runlog::RunLog;

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let cfg = elorin_bot::market::MarketConfig::load_from(Path::new("market.toml"))
        .expect("market.toml must load");
    let disco = cfg.discord.clone();
    if !disco.is_configured() {
        eprintln!("discord is not configured in market.toml");
        return;
    }

    let tag = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1791820864160".to_string());
    let base = PathBuf::from("data/buy");

    // The run's existing debug frames (`<tag>_NN_stage.png`).
    let mut images: Vec<PathBuf> = std::fs::read_dir(&base)
        .expect("data/buy must exist")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&tag) && n.ends_with(".png"))
                .unwrap_or(false)
        })
        .collect();
    images.sort();

    // Annotate the shop frame so the test also shows the debug-image style.
    if let Some(i) = images
        .iter()
        .position(|p| p.to_string_lossy().ends_with("_02_shop.png"))
    {
        if let Some(frame) = load_frame(&images[i]) {
            let band = Rect {
                x1: cfg.buy.glow_rect[0],
                y1: cfg.buy.glow_rect[1],
                x2: cfg.buy.glow_rect[2],
                y2: cfg.buy.glow_rect[3],
            };
            let out = annotate::annotated(
                &frame,
                &[annotate::Stroke::new(band, annotate::YELLOW, 1)],
            );
            let p = std::env::temp_dir().join(format!("elorin_test_{tag}_02_shop.png"));
            let _ = elorin_bot::market::save_frame_png(&p, &out);
            images[i] = p;
        }
    }

    // The run's existing footage, if present.
    let video = {
        let p = base.join(&tag).join(format!("{tag}.mp4"));
        p.exists().then_some(p)
    };

    // A representative timeline in the new format.
    let run = RunLog::new(&tag);
    run.context("item", "Inquisitor's plateskirt");
    run.context("price", "4000000");
    run.context("max price", "5000000");
    run.context("session", "qty_limit 10 / bought 0");
    run.step("start", "buy attempt for \"Inquisitor's plateskirt\" @ 4000000");
    run.note("crop", "icon template 32x19 cropped from the market row");
    run.step("open", "clicking Open at (429,105)");
    run.step("shop", "seller's shop open; locating the item");
    run.step("locate", "item located at (389,94) 33x32");
    run.step("menu", "right-clicking the item for Buy 1");
    run.step("menu", "menu up; clicking Buy 1 at (401,120)");
    run.step("buy", "Buy 1 clicked");
    run.note("record", "recorded 1 against the session cap");
    run.step("result", "bought 1 x Inquisitor's plateskirt (Buy 1)");

    let summary = RunSummary {
        id: tag.clone(),
        name: "Inquisitor's plateskirt".to_string(),
        title: "TEST · Bought 1 × Inquisitor's plateskirt (Buy 1)".to_string(),
        outcome: "bought 1 x Inquisitor's plateskirt (Buy 1)".to_string(),
        color: 0x2ECC71,
        price: 4_000_000,
        units: 1,
        max_price: 5_000_000,
        elapsed_ms: run.elapsed_ms(),
        snipe: false,
    };

    println!(
        "sending run {tag}: {} image(s), video = {:?}",
        images.len(),
        video
    );
    // Detailed evidence to the main webhook.
    discord::notify_run(
        disco.clone(),
        summary.clone(),
        run.render(),
        run.timeline(1500),
        images,
        video,
    );

    // The short summary to the second webhook: a normal buy and a snipe, so both
    // styles are visible.
    let icon = std::fs::read_dir("data/icons")
        .ok()
        .and_then(|mut it| it.find_map(|e| e.ok()))
        .and_then(|e| std::fs::read(e.path()).ok());
    discord::notify_summary(disco.clone(), summary.clone(), icon.clone());
    let mut snipe = summary.clone();
    snipe.snipe = true;
    snipe.title = "Bought 3 × Voting token (Buy X)".to_string();
    snipe.price = 4_100_000;
    snipe.color = 0xFFD700;
    discord::notify_summary(disco, snipe, icon);

    // notify_run/notify_summary post from detached threads; keep the process up.
    std::thread::sleep(Duration::from_secs(25));
}

fn load_frame(p: &Path) -> Option<Frame> {
    let img = image::open(p).ok()?.to_rgba8();
    let (w, h) = (img.width() as i32, img.height() as i32);
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    for (i, px) in img.pixels().enumerate() {
        let o = i * 4;
        bgra[o] = px[2];
        bgra[o + 1] = px[1];
        bgra[o + 2] = px[0];
        bgra[o + 3] = 255;
    }
    Some(Frame {
        width: w,
        height: h,
        bgra,
    })
}

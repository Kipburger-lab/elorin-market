//! Auto-buy controller.
//!
//! Phase 2a: fetch the rules the manager writes, decide what *would* be bought,
//! and log it. Nothing is clicked while `dry_run` is on (the default) — this
//! exists so the whole decision path can be verified against real data first.
//!
//! Rules live in Supabase (`watchlist` + `buy_settings`); the manager page owns
//! them. The scanner only ever reads.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::HWND;

use crate::annotate::{self, Stroke};
use crate::capture::gdi::GdiCapturer;
use crate::capture::{Frame, Rect};
use crate::market::{IconCfg, MarketConfig, MarketTemplates, RowReading};
use crate::runlog::RunLog;
use crate::search::template::{self, Template};
use crate::search::Match;

/// Market fee taken off the sale price (the game's 10%).
const FEE: f64 = 0.10;

/// `[buy]` section of `market.toml`. Uses `[cloud]`'s url + key to reach Supabase.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuyConfig {
    /// Read the rules and report (does not imply buying).
    pub enable: bool,
    /// Never click while this is true.
    pub dry_run: bool,
    /// How often to refetch the rules from Supabase.
    pub poll_ms: u64,
    /// Price context window (the median/p10 come from this many days).
    pub lookback_days: u32,
    /// Where to write a PNG at every step of a purchase ("" disables).
    pub debug_dir: String,
    /// Record every purchase attempt to `<debug_dir>/<tag>/<tag>.mp4`, from just
    /// before the shop is opened until the purchase lands. Needs ffmpeg.
    pub record_purchases: bool,
    /// Recording frame rate (1-30).
    pub record_fps: u64,
    /// Search-until budgets, in the order the purchase needs them.
    pub find_item_ms: u64,
    /// Re-read window before calling an item gone: it can be there one capture
    /// and not the next while the shop settles.
    pub item_retry_ms: u64,
    pub find_button_ms: u64,
    pub amount_wait_ms: u64,
    /// Where the "Enter amount" prompt appears, in client px — the chatbox ban
    /// at the bottom of the client. Searching just this strip is faster and far
    /// less likely to match something else than scanning the whole frame.
    pub amount_rect: [i32; 4],
    /// Beat after the prompt is seen, before the digits go in.
    pub amount_pause_ms: u64,
    /// Beat between the last digit and Enter, so the digits land first.
    pub enter_delay_ms: u64,
    /// How long the seller's shop may take to replace the market panel.
    pub shop_wait_ms: u64,
    /// How long the market may take to come back after a purchase.
    pub market_back_ms: u64,
    /// Beat after a purchase lands, before clicking Return to leave the shop.
    pub after_buy_ms: u64,
    /// How long the "Most Recent Offers" header may take to reappear.
    pub recent_offers_ms: u64,
    /// Delay between synthesised keystrokes.
    pub key_delay_ms: u64,
    /// Per-channel tolerance for finding the item icon / the buy buttons.
    pub item_tolerance: u8,
    pub button_tolerance: u8,
    /// Where the seller's shop lists its items (client px). The shop replaces
    /// the market panel, so nothing may be matched there before it's open.
    pub shop_item_rect: [i32; 4],
    /// The shop panel, searched for the glow that marks the item you looked for.
    ///
    /// Must cover the item grid **to its bottom edge** — a tall shop scrolls, and
    /// a searched item on a low row is still drawn on screen. Trimming this to
    /// the top rows is why a highlight at the bottom was never found; the header
    /// divider and panel border it also read yellow are handled by the blob
    /// filters, not by a short rect.
    pub glow_rect: [i32; 4],
    /// Minimum bright-yellow pixels before we call it the glow.
    pub glow_min_px: i32,
    /// The glow **pulses**: measured brightness on the same item ranged from
    /// RGB(131,125,35) to RGB(236,235,5) across frames, so any absolute
    /// threshold fails on some frames. Yellowness (R + G − 2B) is stable — the
    /// panel measures ~32 and the glow 186 at its dimmest — so this threshold
    /// sits in that gap.
    pub glow_yellowness: i32,
    /// And green must exceed blue, so a bright red isn't mistaken for the glow.
    pub glow_green_min: i32,
    /// Minimum red. The glow is *yellow*: red and green both high, blue low.
    /// Green shares the low-blue signature — a bright green pixel (50,200,50)
    /// passes the yellowness and green tests outright — and a big green item
    /// (a pouch, a scroll) then out-sizes the real glow and gets bought by
    /// mistake. Requiring real red keeps green out.
    pub glow_red_min: i32,
    /// How far apart red and green may be. In the glow they track each other
    /// (measured 131/125 dim, 236/235 bright); in a green pixel red is far
    /// below green, so this rejects it outright.
    pub glow_rg_delta: i32,
    /// Biggest a glow blob may be, per side, to count as one item. The panel's
    /// own border also reads as "yellow", and that artifact is taller than any
    /// item, so an item-sized blob is picked rather than simply the largest.
    pub glow_max_size: i32,
    /// Smallest a glow candidate may be, per side, to be scored as an item.
    ///
    /// The shop draws each item's stack count in the same yellow as the glow, and
    /// the dotted glow itself can break into fragments — both give small yellow
    /// blobs that averaged onto a 4x4 grid can *coincidentally* sit closer to our
    /// icon than the real item does. A real highlight wraps the item, so it is
    /// never this small; dropping the small ones removes that whole failure.
    pub glow_min_size: i32,
    /// Most of its bounding box a glow blob may fill.
    ///
    /// **Left off deliberately.** The obvious idea — a glow is a hollow ring, an
    /// item's own yellow pixels are solid, so cap the fill — does not survive
    /// measurement: the glow *pulses*, and in its bright phase the ring's ink
    /// fills ~60% of its box against ~11% in the dim phase. Any cap tight enough
    /// to exclude a solid item rejects most real glows (44 of 62 captured frames
    /// when tried). Telling a glow from a yellow item needs the icon comparison,
    /// not ink density.
    pub glow_max_fill: f32,
    /// How far the glowing item's colour profile may differ from the icon
    /// cropped out of the market row, before we refuse to buy it. `0` disables
    /// the check.
    ///
    /// The glow narrows it to a candidate; this confirms it. Without it, an item
    /// with a large yellow-ish area can out-size the real outline and be clicked
    /// instead — a bond in a shop full of keys did exactly that. The shop draws
    /// icons a couple of pixels off from the market's copy, so this compares a
    /// coarse 4x4 grid of mean colours, which averages that shift away and still
    /// tells a key from a bond.
    pub icon_profile_max_dist: f32,
    /// How far the Buy 1 / Buy X entry may sit from the item we right-clicked.
    pub menu_radius: i32,
    /// Where the pointer parks between actions, in **client** coordinates.
    /// (629, 265) is the empty part of the inventory: nothing there reacts to a
    /// hover, so the pointer never comes to rest on a shop slot or a menu entry
    /// where it could trigger something later.
    pub recover_client: [i32; 2],
    /// Minimum pause after the right-click before looking for the menu. Only a
    /// floor: the menu is then polled every few ms and clicked the moment it is
    /// complete, so a fast menu is not made to wait for a fixed sleep.
    pub menu_settle_ms: u64,
    /// How long to give a shop that buys on the right-click itself to make the
    /// item disappear (player shops work this way).
    pub direct_buy_ms: u64,
    /// Require both Buy 1 and Buy X to be on screen before clicking —
    /// a single match can be a coincidence somewhere else on the panel.
    pub require_menu_pair: bool,
    /// Shrink the icon silhouette by this many pixels before matching: the game
    /// draws a glow *over* the searched item's edge, which no longer resembles
    /// the plain icon we cropped from the market.
    pub icon_erode_px: i32,
    /// Fraction of the icon's top-left corner to ignore (a quantity badge sits
    /// there in the shop, and in the market when the stack is > 1).
    pub icon_corner_pct: f32,
    /// Stacks smaller than this are taken with a Shift + left-click (the swapped
    /// "Buy 10" entry) instead of the right-click menu: one click, no amount
    /// prompt. `0` disables the fast path. Only used when the session cap covers
    /// the whole stack — the bulk click can't be trimmed.
    pub bulk_shift_below: i32,

    /// Where the cyan-outlined market clerks can be searched for, in client px.
    /// Kept clear of the chatbox and the sidebar, whose own text is cyan too.
    pub hull_rect: [i32; 4],
    /// A cyan (00FFFF) hull pixel: red at or below this…
    pub hull_red_max: i32,
    /// …green at or above this, blue at or above this…
    pub hull_green_min: i32,
    pub hull_blue_min: i32,
    /// …and green/blue within this of each other (so a pure blue isn't it).
    pub hull_gb_delta: i32,
    /// Smallest/largest an NPC hull may be, per side, in client px.
    pub hull_min_size: i32,
    pub hull_max_size: i32,
    /// Most of its bounding box a hull's cyan ink may fill. The hull is a thin
    /// outline, so a dense cyan blob is something else (chat text, a spell).
    pub hull_max_fill: f32,
    /// Join dashes within this many pixels before blob-finding, so a dotted hull
    /// reads as one shape instead of fragments. Deliberately small: a bigger
    /// value merges the two adjacent clerks into one blob whose centre lands
    /// between them, where a right-click misses both.
    pub hull_join_px: i32,
    /// How long the market UI may take to appear after clicking the clerk.
    pub reopen_wait_ms: u64,
}

impl Default for BuyConfig {
    fn default() -> Self {
        Self {
            enable: false,
            dry_run: true,
            poll_ms: 5_000,
            lookback_days: 30,
            debug_dir: "data/buy".to_string(),
            record_purchases: true,
            record_fps: 8,
            find_item_ms: 5_000,
            item_retry_ms: 1_500,
            find_button_ms: 7_000,
            amount_wait_ms: 3_000,
            amount_rect: [0, 330, 811, 571],
            amount_pause_ms: 300,
            enter_delay_ms: 300,
            shop_wait_ms: 1_200,
            market_back_ms: 2_500,
            after_buy_ms: 1_000,
            recent_offers_ms: 3_000,
            key_delay_ms: 25,
            item_tolerance: 30,
            button_tolerance: 25,
            shop_item_rect: [40, 60, 330, 240],
            glow_rect: [24, 84, 503, 348],
            glow_min_px: 25,
            glow_yellowness: 100,
            glow_green_min: 40,
            glow_red_min: 100,
            glow_rg_delta: 60,
            glow_max_size: 40,
            glow_min_size: 16,
            glow_max_fill: 1.0,
            icon_profile_max_dist: 25.0,
            menu_radius: 140,
            recover_client: [629, 265],
            menu_settle_ms: 150,
            direct_buy_ms: 2_000,
            require_menu_pair: true,
            icon_erode_px: 2,
            icon_corner_pct: 0.45,
            bulk_shift_below: 10,
            hull_rect: [0, 0, 655, 372],
            hull_red_max: 90,
            hull_green_min: 170,
            hull_blue_min: 170,
            hull_gb_delta: 60,
            hull_min_size: 16,
            hull_max_size: 140,
            hull_max_fill: 0.5,
            hull_join_px: 2,
            reopen_wait_ms: 1_500,
        }
    }
}

/// One row of `buy_rules` (already filtered to ticked items).
#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub name: String,
    #[serde(default)]
    pub max_price: Option<i64>,
    #[serde(default)]
    pub qty_limit: i32,
    #[serde(default)]
    pub bought: i32,
    #[serde(default)]
    pub median: Option<f64>,
    #[serde(default)]
    pub p10: Option<i64>,
    #[serde(default)]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub min_margin: i64,
    #[serde(default)]
    pub max_snipes: i32,
    #[serde(default)]
    pub snipes_used: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: false, min_margin: 1_000_000_000, max_snipes: 2, snipes_used: 0 }
    }
}

/// Why a quote did or did not qualify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    WouldBuy,
    /// Qualifies, but it is a big snipe and the session's ration is spent.
    SnipeCapped,
    /// The master switch is off.
    Paused,
}

/// One qualifying quote with its numbers.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub name: String,
    pub price: i64,
    pub max_price: i64,
    pub profit: i64,
    pub pct: f64,
    pub snipe: bool,
    pub decision: Decision,
    pub snipes_used: i32,
    pub max_snipes: i32,
    /// What the session's cap allows, needed to size a Buy X.
    pub qty_limit: i32,
    pub bought: i32,
}

impl Verdict {
    pub fn line(&self) -> String {
        let fee_net = self.price + self.profit; // what we expect back after the fee
        let mut s = format!(
            "{} @ {} (max {}) · resale~{} · profit {} ({:+.0}%)",
            self.name,
            self.price,
            self.max_price,
            fee_net,
            self.profit,
            self.pct
        );
        if self.snipe {
            s.push_str(&format!(" · BIG SNIPE {}/{}", self.snipes_used, self.max_snipes));
        }
        match self.decision {
            Decision::WouldBuy => {}
            Decision::SnipeCapped => s.push_str(" · BLOCKED: snipe ration spent"),
            Decision::Paused => s.push_str(" · BLOCKED: master switch off"),
        }
        s
    }
}

/// Estimate profit after the market fee, given the observed median.
pub fn profit_for(median: f64, price: i64) -> i64 {
    (median * (1.0 - FEE)).round() as i64 - price
}

/// Decide, for a set of live quotes, which ones the rules allow.
///
/// Pure: no I/O, so the whole policy is unit-testable.
pub fn evaluate(quotes: &[(String, i64)], rules: &[Rule], s: &Settings) -> Vec<Verdict> {
    let mut out = Vec::new();
    for (name, price) in quotes {
        let Some(r) = rules.iter().find(|r| r.name == *name) else {
            continue;
        };
        let Some(max_price) = r.max_price else { continue };
        if *price > max_price {
            continue;
        }
        if r.qty_limit > 0 && r.bought >= r.qty_limit {
            continue;
        }
        let median = r.median.unwrap_or(0.0);
        if median <= 0.0 {
            continue;
        }
        let profit = profit_for(median, *price);
        let pct = if *price > 0 { profit as f64 / *price as f64 * 100.0 } else { 0.0 };
        let snipe = profit >= s.min_margin;
        let decision = if !s.enabled {
            Decision::Paused
        } else if snipe && s.snipes_used >= s.max_snipes {
            Decision::SnipeCapped
        } else {
            Decision::WouldBuy
        };
        out.push(Verdict {
            name: name.clone(),
            price: *price,
            max_price,
            profit,
            pct,
            snipe,
            decision,
            snipes_used: s.snipes_used,
            max_snipes: s.max_snipes,
            qty_limit: r.qty_limit,
            bought: r.bought,
        });
    }
    // Best profit first, so the log reads like a shopping list.
    out.sort_by(|a, b| b.profit.cmp(&a.profit));
    out
}

// ── Remote rules + cache ──────────────────────────────────────────────────
struct Cache {
    at: Option<Instant>,
    rules: Vec<Rule>,
    settings: Settings,
    last_error: Option<String>,
    /// Last heartbeat we published, so we don't write one per loop tick.
    last_beat: Option<Instant>,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
static CFG: OnceLock<BuyConfig> = OnceLock::new();

fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(|| {
        Mutex::new(Cache {
            at: None,
            rules: Vec::new(),
            settings: Settings::default(),
            last_error: None,
            last_beat: None,
        })
    })
}

/// Publish our mode + a liveness stamp, throttled to roughly every 20 s. Runs
/// in dry run too — that's exactly the state the manager needs to show.
pub fn maybe_heartbeat() {
    let Some(cfg) = CFG.get() else {
        return;
    };
    let dry = cfg.dry_run;
    {
        let mut c = cache().lock();
        if let Some(t) = c.last_beat {
            if t.elapsed() < Duration::from_secs(20) {
                return;
            }
        }
        c.last_beat = Some(Instant::now());
    }
    let Some((url, key)) = crate::cloud::endpoint() else {
        return;
    };
    let base = url.trim_end_matches('/');
    let body = format!("{{\"p_dry_run\":{dry}}}");
    if let Err(e) = post(&format!("{base}/rest/v1/rpc/scanner_heartbeat"), key, &body) {
        // Quiet: the next tick retries, and a missing heartbeat is not fatal.
        debug!(error = %e, "scanner heartbeat failed");
    }
}

/// Enable the auto-buy controller. Returns false when it's off or unconfigured.
pub fn configure(cfg: BuyConfig) -> bool {
    if !cfg.enable {
        return false;
    }
    if crate::cloud::endpoint().is_none() {
        warn!("buy.enable is set but [cloud] url/service_key is missing");
        return false;
    }
    CFG.set(cfg).is_ok()
}

fn post(url: &str, key: &str, body: &str) -> Result<String> {
    let resp = ureq::post(url)
        .set("apikey", key)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", crate::cloud::USER_AGENT)
        .timeout(Duration::from_secs(10))
        .send_string(body)
        .map_err(|e| anyhow!("POST {url}: {e}"))?;
    resp.into_string().map_err(|e| anyhow!("read {url}: {e}"))
}

fn fetch_rules(since_ms: u64) -> Result<(Vec<Rule>, Settings)> {
    let (url, key) = crate::cloud::endpoint().ok_or_else(|| anyhow!("cloud not configured"))?;
    let base = url.trim_end_matches('/');

    let body = format!("{{\"since_ms\":{since_ms}}}");
    let rules: Vec<Rule> = serde_json::from_str(&post(
        &format!("{base}/rest/v1/rpc/buy_rules"),
        key,
        &body,
    )?)?;

    let resp = ureq::get(&format!("{base}/rest/v1/buy_settings?select=*&id=eq.1"))
        .set("apikey", key)
        .set("Authorization", &format!("Bearer {key}"))
        .set("User-Agent", crate::cloud::USER_AGENT)
        .timeout(Duration::from_secs(10))
        .call()
        .map_err(|e| anyhow!("GET buy_settings: {e}"))?;
    let list: Vec<Settings> = serde_json::from_str(&resp.into_string()?)?;
    Ok((rules, list.into_iter().next().unwrap_or_default()))
}

/// Refresh the cached rules if they're older than `poll_ms`.
fn refresh(force: bool) -> Result<()> {
    let cfg = CFG.get().ok_or_else(|| anyhow!("buy controller not configured"))?;
    let mut c = cache().lock();
    let stale = match c.at {
        None => true,
        Some(t) => force || t.elapsed() >= Duration::from_millis(cfg.poll_ms.max(500)),
    };
    if !stale {
        return Ok(());
    }
    let since = crate::market::now_ms() as u64
        - (cfg.lookback_days.max(1) as u64) * 86_400_000;
    match fetch_rules(since) {
        Ok((rules, settings)) => {
            info!(
                rules = rules.len(),
                enabled = settings.enabled,
                snipes_used = settings.snipes_used,
                "buy rules refreshed"
            );
            c.rules = rules;
            c.settings = settings;
            c.last_error = None;
            c.at = Some(Instant::now());
            Ok(())
        }
        Err(e) => {
            c.at = Some(Instant::now());
            c.last_error = Some(e.to_string());
            Err(e)
        }
    }
}

/// Log what the rules would do with the rows we just read. Click-free.
pub fn report(rows: &[RowReading]) {
    if CFG.get().is_none() {
        return;
    }
    if let Err(e) = refresh(false) {
        warn!(error = %e, "could not refresh buy rules");
        return;
    }
    let (rules, settings, err) = {
        let c = cache().lock();
        (c.rules.clone(), c.settings.clone(), c.last_error.clone())
    };
    if let Some(e) = err {
        warn!(error = %e, "buy rules stale");
    }
    if rules.is_empty() {
        return;
    }

    let quotes: Vec<(String, i64)> = rows
        .iter()
        .filter(|r| !r.name.is_empty())
        .map(|r| (r.name.clone(), r.read.price as i64))
        .collect();
    let verdicts = evaluate(&quotes, &rules, &settings);
    if verdicts.is_empty() {
        return;
    }

    let dry = CFG.get().map(|c| c.dry_run).unwrap_or(true);
    let tag = if dry { "DRY RUN" } else { "LIVE" };
    for v in &verdicts {
        match v.decision {
            Decision::WouldBuy if dry => println!("  BUY? [{}] {}", tag, v.line()),
            Decision::WouldBuy => println!("  BUY  [{}] {}", tag, v.line()),
            _ => println!("  skip [{}] {}", tag, v.line()),
        }
    }
    // An item with a rule that produced no verdict was dropped silently, which
    // leaves "the item is right there and it did nothing" with no explanation.
    // Say why, per item.
    for (name, price) in &quotes {
        if verdicts.iter().any(|v| v.name == *name) {
            continue;
        }
        let Some(r) = rules.iter().find(|r| r.name == *name) else {
            continue;
        };
        let why = match r.max_price {
            None => "no max price set".to_string(),
            Some(max) if *price > max => format!("{price} is above the {max} max"),
            _ if r.qty_limit > 0 && r.bought >= r.qty_limit => {
                format!("session cap reached ({}/{})", r.bought, r.qty_limit)
            }
            _ if r.median.unwrap_or(0.0) <= 0.0 => "no price history in the lookback window".to_string(),
            _ => "paused".to_string(),
        };
        println!("  skip [{}] {name} @ {price} — {why}", tag);
    }
    info!(candidates = verdicts.len(), dry_run = dry, "auto-buy evaluation");
}

// ── Execution (phase 2b) ──────────────────────────────────────────────────

/// How many units a Buy X should ask for: never more than the stack on offer,
/// and never more than the session's remaining allowance.
pub fn amount_for(stack: i64, qty_limit: i32, bought: i32) -> i64 {
    let remaining = qty_limit as i64 - bought as i64;
    stack.min(remaining).max(0)
}

/// A stack we can take part of needs Buy X; a lone item needs Buy 1.
pub fn use_buy_x(stack: i64) -> bool {
    stack > 1
}

/// What a purchase attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Bought { name: String, units: i64, buy_x: bool },
    /// The shop no longer lists the item: it was bought out from under us while
    /// we were getting there. Not an error — the caller just goes back to the
    /// offers list and resumes scanning.
    Gone { name: String },
    Failed(String),
}

impl Outcome {
    pub fn describe(&self) -> String {
        match self {
            Outcome::Bought { name, units, buy_x } => {
                format!("bought {units} x {name} ({})", if *buy_x { "Buy X" } else { "Buy 1" })
            }
            Outcome::Gone { name } => format!("{name} was gone before we could buy it"),
            Outcome::Failed(why) => format!("purchase aborted: {why}"),
        }
    }

    /// True when the shop needs no further work — either we bought, or there was
    /// nothing left to buy. Both mean "go back to the offers list".
    pub fn attempted(&self) -> bool {
        !matches!(self, Outcome::Failed(_))
    }
}

/// The rows we're cleared to buy, best profit first, with their index in `rows`.
/// Recomputed from the cached rules, so it costs nothing after `report()`.
pub fn plan(rows: &[RowReading]) -> Vec<(usize, Verdict)> {
    if CFG.get().is_none() {
        return Vec::new();
    }
    if let Err(e) = refresh(false) {
        warn!(error = %e, "could not refresh buy rules");
        return Vec::new();
    }
    let (rules, settings) = {
        let c = cache().lock();
        (c.rules.clone(), c.settings.clone())
    };
    if rules.is_empty() {
        return Vec::new();
    }
    let quotes: Vec<(String, i64)> = rows
        .iter()
        .filter(|r| !r.name.is_empty())
        .map(|r| (r.name.clone(), r.read.price as i64))
        .collect();

    let mut used = vec![false; rows.len()];
    let mut out = Vec::new();
    for v in evaluate(&quotes, &rules, &settings) {
        if v.decision != Decision::WouldBuy {
            continue;
        }
        for (i, r) in rows.iter().enumerate() {
            if !used[i] && r.name == v.name && r.read.price as i64 == v.price {
                used[i] = true;
                out.push((i, v));
                break;
            }
        }
    }
    out
}

/// Sample a `w x h` rectangle onto an `n x n` grid of mean colours.
///
/// Deliberately low resolution: the shop draws its icons a couple of pixels off
/// from the market's copy, so anything pixel-exact fails, while a small grid
/// averages that shift away and still separates a key from a bond. `get` returns
/// packed RGB for a point inside the rectangle.
fn colour_grid<F>(w: i32, h: i32, n: i32, get: F) -> Vec<[f32; 3]>
where
    F: Fn(i32, i32) -> Option<[u8; 3]>,
{
    let mut out = Vec::with_capacity((n * n) as usize);
    for gy in 0..n {
        for gx in 0..n {
            let (mut r, mut g, mut b, mut count) = (0f32, 0f32, 0f32, 0f32);
            for y in (gy * h / n)..((gy + 1) * h / n) {
                for x in (gx * w / n)..((gx + 1) * w / n) {
                    if let Some(p) = get(x, y) {
                        r += p[0] as f32;
                        g += p[1] as f32;
                        b += p[2] as f32;
                        count += 1.0;
                    }
                }
            }
            out.push(if count > 0.0 {
                [r / count, g / count, b / count]
            } else {
                [0.0; 3]
            });
        }
    }
    out
}

/// Mean per-cell colour distance between two grids, in RGB units (0..~441).
fn grid_distance(a: &[[f32; 3]], b: &[[f32; 3]]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return f32::MAX;
    }
    let total: f32 = a
        .iter()
        .zip(b)
        .map(|(p, q)| {
            ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt()
        })
        .sum();
    total / a.len() as f32
}

/// The market row's icon as a plain RGB crop, with no background keying or
/// trimming — the colours exactly as the market drew them.
fn plain_icon(frame: &Frame, row: &RowReading, icfg: &IconCfg) -> Option<(i32, i32, Vec<u8>)> {
    let mut plain = icfg.clone();
    plain.remove_bg = false;
    plain.trim = false;
    crate::market::crop_icon(frame, &row.open, &plain)
}

/// The market row's icon as an untrimmed template: colours from the raw crop,
/// silhouette from the background-keyed one, so the two align pixel for pixel.
///
/// Deliberately *not* the stored `data/icons/*.png`: that crop can have the
/// yellow stack count baked in, which the shop doesn't render.
fn icon_template_raw(frame: &Frame, row: &RowReading, icfg: &IconCfg) -> Option<Template> {
    let mut plain = icfg.clone();
    plain.remove_bg = false;
    plain.trim = false;
    let (w, h, rgb) = crate::market::crop_icon(frame, &row.open, &plain)?;
    if w < 3 || h < 3 {
        return None;
    }

    let mut keyed = icfg.clone();
    keyed.trim = false;
    let alpha = crate::market::crop_icon(frame, &row.open, &keyed)
        .map(|(_, _, rgba)| rgba)
        .unwrap_or_else(|| rgb.clone());

    let mut img = image::RgbaImage::new(w as u32, h as u32);
    for y in 0..h {
        for x in 0..w {
            let o = ((y * w + x) * 4) as usize;
            let a = if alpha.get(o + 3).copied().unwrap_or(0) > 0 { 255 } else { 0 };
            img.put_pixel(x as u32, y as u32, image::Rgba([rgb[o], rgb[o + 1], rgb[o + 2], a]));
        }
    }
    Some(Template::from_image_rgba(&img))
}

/// The glow locator's template: eroded and reduced so the highlight painted over
/// the item's rim can't spoil the match.
pub fn icon_template(frame: &Frame, row: &RowReading, icfg: &IconCfg, buy_cfg: &BuyConfig) -> Option<Template> {
    let mut tpl = icon_template_raw(frame, row, icfg)?;
    prepare_icon_template(&mut tpl, buy_cfg);
    Some(tpl)
}

/// The template for the no-highlight fallback.
///
/// **No erosion.** There is no glow to survive, and eroding a thin icon (a bow,
/// a key) can leave a handful of pixels that match nothing at all. Only the
/// quantity-badge corner is cleared, since the badge digits can differ between
/// the market row and the shop.
fn icon_template_plain(frame: &Frame, row: &RowReading, icfg: &IconCfg) -> Option<Template> {
    let mut tpl = icon_template_raw(frame, row, icfg)?;
    drop_icon_corner(&mut tpl);
    Some(tpl)
}

/// Make a template that survives how the shop draws the searched item.
///
/// The shop wraps the item in a glow painted *over* its silhouette, and can put a
/// quantity badge next to it. Rather than special-casing any particular item —
/// every icon has a different outline — we use one shape-agnostic rule: **keep
/// the item's largest solid region, pulled in from its edge**.
///
/// That naturally drops the glow-corrupted rim, thin appendages, and the badge
/// (a separate blob), leaving pixels that look identical in the market and in the
/// shop, whatever the icon's shape is.
fn prepare_icon_template(tpl: &mut Template, cfg: &BuyConfig) {
    let (w, h) = (tpl.width, tpl.height);
    if w <= 0 || h <= 0 {
        return;
    }
    let at = |x: i32, y: i32| (y * w + x) as usize;

    // 1. Pull the silhouette in by `icon_erode_px`: the glow's thickness.
    let glow = cfg.icon_erode_px.clamp(0, 8);
    if glow > 0 {
        let src = tpl.mask.clone();
        for y in 0..h {
            for x in 0..w {
                let i = at(x, y);
                if !src[i] {
                    continue;
                }
                let mut keep = true;
                'ring: for dy in -glow..=glow {
                    for dx in -glow..=glow {
                        let (nx, ny) = (x + dx, y + dy);
                        if nx < 0 || ny < 0 || nx >= w || ny >= h || !src[(ny * w + nx) as usize] {
                            keep = false;
                            break 'ring;
                        }
                    }
                }
                if !keep {
                    tpl.mask[i] = false;
                }
            }
        }
    }

    // 2. Keep only the biggest connected blob (the item's body).
    keep_largest_component(&mut tpl.mask, w, h);

    // 3. Belt and braces: never compare the top-left corner, where a badge can
    //    touch the body and so survive step 2.
    drop_icon_corner(tpl);
}

/// Clear a template's top-left corner, where a quantity badge can touch the
/// item's body (its digits can differ between the market row and the shop).
fn drop_icon_corner(tpl: &mut Template) {
    let (w, h) = (tpl.width, tpl.height);
    if w <= 0 || h <= 0 {
        return;
    }
    let cw = (w / 3).clamp(0, 10);
    let ch = (h / 3).clamp(0, 12);
    for y in 0..h.min(ch) {
        for x in 0..w.min(cw) {
            tpl.mask[(y * w + x) as usize] = false;
        }
    }
}

/// Do two client-space rectangles overlap?
fn overlaps(a: Match, b: Match) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// The bounding box of a blob, in the mask's own coordinates.
fn blob_bbox(blob: &[usize], w: i32) -> (i32, i32, i32, i32) {
    let (mut x1, mut y1, mut x2, mut y2) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for i in blob {
        let x = (*i as i32) % w;
        let y = (*i as i32) / w;
        x1 = x1.min(x);
        x2 = x2.max(x);
        y1 = y1.min(y);
        y2 = y2.max(y);
    }
    (x1, y1, x2, y2)
}

/// Every 4-connected blob, largest first.
fn all_blobs(mask: &[bool], w: i32, h: i32) -> Vec<Vec<usize>> {
    let n = (w * h) as usize;
    let mut seen = vec![false; n];
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();

    for start in 0..n {
        if seen[start] || !mask[start] {
            continue;
        }
        let mut blob = Vec::new();
        stack.push(start);
        seen[start] = true;
        while let Some(i) = stack.pop() {
            blob.push(i);
            let x = (i as i32) % w;
            let y = (i as i32) / w;
            for (nx, ny) in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] {
                if nx < 0 || ny < 0 || nx >= w || ny >= h {
                    continue;
                }
                let j = (ny * w + nx) as usize;
                if !seen[j] && mask[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            }
        }
        out.push(blob);
    }
    out.sort_by(|a, b| b.len().cmp(&a.len()));
    out
}

/// Every blob that could plausibly be a single item: enough ink, not so large
/// that it's part of the panel furniture, biggest first.
fn all_item_blobs(mask: &[bool], w: i32, h: i32, cfg: &BuyConfig) -> Vec<Vec<usize>> {
    let min_ink = cfg.glow_min_px.max(1) as usize;
    let min_side = cfg.glow_min_size.max(1);
    let max_side = cfg.glow_max_size.max(8);
    all_blobs(mask, w, h)
        .into_iter()
        .filter(|b| {
            if b.len() < min_ink {
                return false;
            }
            let (x1, y1, x2, y2) = blob_bbox(b, w);
            let (bw, bh) = (x2 - x1 + 1, y2 - y1 + 1);
            if bw > max_side || bh > max_side {
                return false;
            }
            // A stack-count digit or a glow fragment is small; only a real
            // highlight wraps an item, so drop the small ones.
            if bw < min_side || bh < min_side {
                return false;
            }
            (b.len() as f32 / (bw * bh) as f32) <= cfg.glow_max_fill
        })
        .collect()
}

/// The bounding box of the largest 4-connected blob of `mask`.
fn largest_blob(mask: &[bool], w: i32, h: i32) -> Vec<usize> {
    let n = (w * h) as usize;
    let mut seen = vec![false; n];
    let mut best: Vec<usize> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();

    for start in 0..n {
        if seen[start] || !mask[start] {
            continue;
        }
        let mut blob = Vec::new();
        stack.push(start);
        seen[start] = true;
        while let Some(i) = stack.pop() {
            blob.push(i);
            let x = (i as i32) % w;
            let y = (i as i32) / w;
            for (nx, ny) in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] {
                if nx < 0 || ny < 0 || nx >= w || ny >= h {
                    continue;
                }
                let j = (ny * w + nx) as usize;
                if !seen[j] && mask[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            }
        }
        if blob.len() > best.len() {
            best = blob;
        }
    }
    best
}

/// Zero every masked pixel that isn't part of the largest blob.
fn keep_largest_component(mask: &mut [bool], w: i32, h: i32) {
    let best = largest_blob(mask, w, h);
    if best.is_empty() {
        return;
    }
    let keep: std::collections::HashSet<usize> = best.into_iter().collect();
    for (i, m) in mask.iter_mut().enumerate() {
        if *m && !keep.contains(&i) {
            *m = false;
        }
    }
}

/// Is this pixel part of the item's glow? See [`BuyConfig::glow_yellowness`].
///
/// Yellow, specifically — red and green together, low blue, and red tracking
/// green. Each part rules out something that would otherwise pass: the panel is
/// too dim, a red is too green-poor, and a green is too red-poor.
fn is_glow_pixel(p: &[u8], cfg: &BuyConfig) -> bool {
    let (b, g, r) = (p[0] as i32, p[1] as i32, p[2] as i32);
    r >= cfg.glow_red_min
        && (r - g).abs() <= cfg.glow_rg_delta
        && (r + g - 2 * b) > cfg.glow_yellowness
        && (g - b) > cfg.glow_green_min
}

/// Find every item-sized glow candidate in `rect`, biggest first.
///
/// The game highlights the item it matched with a glowing yellow marker, but
/// the highlight can sit on whatever the client matched — and a large yellow
/// *item* can out-size the real glow. Returning them all lets the icon check
/// ([`best_candidate`]) decide which one is actually ours.
fn glow_candidates(frame: &Frame, rect: Rect, cfg: &BuyConfig) -> Vec<Match> {
    let x1 = rect.x1.max(0).min(frame.width);
    let y1 = rect.y1.max(0).min(frame.height);
    let x2 = rect.x2.max(0).min(frame.width);
    let y2 = rect.y2.max(0).min(frame.height);
    let (w, h) = (x2 - x1, y2 - y1);
    if w <= 0 || h <= 0 {
        return Vec::new();
    }

    let mut mask = vec![false; (w * h) as usize];
    for y in y1..y2 {
        for x in x1..x2 {
            if let Some(p) = frame.pixel(x, y) {
                if is_glow_pixel(&p, cfg) {
                    mask[((y - y1) * w + (x - x1)) as usize] = true;
                }
            }
        }
    }

    // The glow is a dithered ring, so its pixels don't all touch. Join anything
    // within a couple of pixels before looking for the blob, or a single glow
    // breaks into fragments that each look too small to be an item.
    let join = 2i32;
    let mut joined = mask.clone();
    for y in 0..h {
        for x in 0..w {
            if !mask[(y * w + x) as usize] {
                continue;
            }
            for dy in -join..=join {
                for dx in -join..=join {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx >= 0 && ny >= 0 && nx < w && ny < h {
                        joined[(ny * w + nx) as usize] = true;
                    }
                }
            }
        }
    }

    all_item_blobs(&joined, w, h, cfg)
        .into_iter()
        .map(|blob| {
            let (bx1, by1, bx2, by2) = blob_bbox(&blob, w);
            Match {
                x: x1 + bx1,
                y: y1 + by1,
                w: bx2 - bx1 + 1,
                h: by2 - by1 + 1,
            }
        })
        .collect()
}

/// Find the item the shop is highlighting.
///
/// The game wraps the item you searched for in a glowing yellow marker, and that
/// marker is the same for every item — unlike the icon itself, which the shop
/// draws a few pixels off from the market's copy, so a pixel-perfect comparison
/// can't work for *any* item. This is the shape-agnostic locator.
fn find_glow(frame: &Frame, rect: Rect, cfg: &BuyConfig) -> Option<Match> {
    glow_candidates(frame, rect, cfg).into_iter().next()
}

/// The market row's icon as a 4x4 grid of mean colours, untrimmed and with no
/// background keying — exactly what the market drew.
fn market_icon_grid(frame: &Frame, row: &RowReading, icfg: &IconCfg) -> Option<Vec<[f32; 3]>> {
    let (w, h, rgb) = plain_icon(frame, row, icfg)?;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(colour_grid(w, h, 4, |x, y| {
        let o = ((y * w + x) * 4) as usize;
        Some([rgb[o], rgb[o + 1], rgb[o + 2]])
    }))
}

/// A shop candidate's icon as a 4x4 grid of mean colours over its bounding box,
/// in the same orientation as [`market_icon_grid`]. Pixels that read as glow are
/// skipped.
fn candidate_grid(shop: &Frame, m: Match, cfg: &BuyConfig) -> Vec<[f32; 3]> {
    let (w, h) = (m.w.max(1), m.h.max(1));
    colour_grid(w, h, 4, |x, y| {
        // The highlight is painted over the item's rim, so leaving it in drags
        // every real match toward the glow's yellow — which is exactly how a
        // stray yellow fragment used to out-score the item it was next to.
        shop.pixel(m.x + x, m.y + y)
            .filter(|p| !is_glow_pixel(p, cfg))
            .map(|p| [p[2], p[1], p[0]])
    })
}

/// Choose the shop slot that best matches the item we scanned.
///
/// Every glow candidate is scored by how closely its icon resembles the market
/// row's own icon — a coarse 4x4 grid of mean colours, which averages away the
/// few-pixel offset between the market's copy and the shop's and survives the
/// glow drawn over the item. The lowest distance wins. When even the best is
/// beyond `icon_profile_max_dist`, the buy is refused rather than taking
/// whatever happened to glow: that is how a yellow item or a stale highlight
/// turns into a wrong purchase.
fn best_candidate(
    shop: &Frame,
    market: &Frame,
    row: &RowReading,
    icfg: &IconCfg,
    candidates: &[Match],
    cfg: &BuyConfig,
) -> Option<Match> {
    if candidates.is_empty() {
        return None;
    }
    let Some(target) = market_icon_grid(market, row, icfg) else {
        // No reference icon to compare against: fall back to the biggest glow.
        return candidates.first().copied();
    };
    let mut best: Option<(f32, Match)> = None;
    for &c in candidates {
        let d = grid_distance(&target, &candidate_grid(shop, c, cfg));
        info!(x = c.x, y = c.y, w = c.w, h = c.h, dist = d, "buy: shop candidate");
        if best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, c));
        }
    }
    let (dist, m) = best?;
    let limit = cfg.icon_profile_max_dist;
    if limit > 0.0 && dist > limit {
        warn!(dist, limit, "buy: no shop item matches the scanned icon; refusing");
        return None;
    }
    Some(m)
}

/// Is this pixel part of a market clerk's cyan (00FFFF) selection hull?
///
/// The clerks the market UI hangs off are outlined in cyan when the client marks
/// them. Cyan specifically — strong green *and* strong blue together — so the
/// sidebar's blues (green well below blue) and the chat's greens are both out.
fn is_hull_pixel(p: &[u8], cfg: &BuyConfig) -> bool {
    let (b, g, r) = (p[0] as i32, p[1] as i32, p[2] as i32);
    r <= cfg.hull_red_max
        && g >= cfg.hull_green_min
        && b >= cfg.hull_blue_min
        && (g - b).abs() <= cfg.hull_gb_delta
}

/// The bounding boxes of the cyan clerk hulls inside `rect`, biggest first.
///
/// The hull is a dotted outline, so the mask is closed a couple of pixels before
/// blobs are taken; the box is then the *outline's* extent, whose centre is the
/// clerk. Candidates are filtered to clerk size and to a thin outline, so the
/// cyan chatbox text and other UI don't survive. Each hull is returned
/// separately: merging the two adjacent clerks would put the centre between
/// them, where a right-click hits neither.
fn find_hulls(frame: &Frame, rect: Rect, cfg: &BuyConfig) -> Vec<Match> {
    let x1 = rect.x1.max(0).min(frame.width);
    let y1 = rect.y1.max(0).min(frame.height);
    let x2 = rect.x2.max(0).min(frame.width);
    let y2 = rect.y2.max(0).min(frame.height);
    let (w, h) = (x2 - x1, y2 - y1);
    if w <= 0 || h <= 0 {
        return Vec::new();
    }

    let mut ink = vec![false; (w * h) as usize];
    for y in y1..y2 {
        for x in x1..x2 {
            if let Some(p) = frame.pixel(x, y) {
                if is_hull_pixel(&p, cfg) {
                    ink[((y - y1) * w + (x - x1)) as usize] = true;
                }
            }
        }
    }

    let mut mask = ink.clone();
    let join = cfg.hull_join_px.clamp(0, 6);
    if join > 0 {
        for y in 0..h {
            for x in 0..w {
                if !ink[(y * w + x) as usize] {
                    continue;
                }
                for dy in -join..=join {
                    for dx in -join..=join {
                        let (nx, ny) = (x + dx, y + dy);
                        if nx >= 0 && ny >= 0 && nx < w && ny < h {
                            mask[(ny * w + nx) as usize] = true;
                        }
                    }
                }
            }
        }
    }

    let mut out = Vec::new();
    for blob in all_blobs(&mask, w, h) {
        let (bx1, by1, bx2, by2) = blob_bbox(&blob, w);
        let (bw, bh) = (bx2 - bx1 + 1, by2 - by1 + 1);
        if bw < cfg.hull_min_size || bh < cfg.hull_min_size {
            continue;
        }
        if bw > cfg.hull_max_size || bh > cfg.hull_max_size {
            continue;
        }
        // Fill is measured on the *raw* mask: the closed one is dilated, which
        // would make even a thin outline read as solid.
        let mut filled = 0i32;
        for y in by1..=by2 {
            for x in bx1..=bx2 {
                if ink[(y * w + x) as usize] {
                    filled += 1;
                }
            }
        }
        if filled as f32 / (bw * bh) as f32 > cfg.hull_max_fill {
            continue;
        }
        out.push(Match { x: x1 + bx1, y: y1 + by1, w: bw, h: bh });
    }
    out.sort_by(|a, b| (b.w * b.h).cmp(&(a.w * a.h)));
    out
}

/// Reopen the market after a purchase throws the client out of the interface.
///
/// When a shop is emptied or the buy is refused, the client can close the whole
/// market window, leaving the character in the world. The market hangs off a
/// clerk NPC outlined in cyan; right-clicking one offers "Open Marketplace
/// clerk". Find a hull, right-click its centre, click that entry. Best-effort:
/// returns true when the entry was clicked, not when the market is confirmed.
pub fn reopen_market(hwnd: HWND, cap: &GdiCapturer, tpls: &MarketTemplates, cfg: &BuyConfig) -> bool {
    let Some(clerk) = tpls.open_marketplace.as_ref() else {
        return false;
    };
    let area = Rect {
        x1: cfg.hull_rect[0],
        y1: cfg.hull_rect[1],
        x2: cfg.hull_rect[2],
        y2: cfg.hull_rect[3],
    };
    let Ok(frame) = cap.capture_full_client() else {
        return false;
    };
    let Some(hull) = find_hulls(&frame, area, cfg).into_iter().next() else {
        return false;
    };
    let (cx, cy) = hull.center();
    let Some((sx, sy)) = (unsafe { crate::window::client_to_screen(hwnd, cx, cy) }) else {
        return false;
    };
    if let Some((rx, ry)) = (unsafe {
        crate::window::client_to_screen(hwnd, cfg.recover_client[0], cfg.recover_client[1])
    }) {
        crate::input::set_recover_pos(rx, ry);
    }
    info!(x = cx, y = cy, w = hull.w, h = hull.h, "market: right-clicking the clerk");
    // Keep the pointer on the clerk: the menu belongs to its position, and
    // moving straight off is what makes a click pass through the UI.
    crate::input::right_click_screen_keep(sx, sy);

    let near = Rect {
        x1: (hull.x - cfg.menu_radius).max(0),
        y1: (hull.y - cfg.menu_radius).max(0),
        x2: hull.x + hull.w + cfg.menu_radius,
        y2: hull.y + hull.h + cfg.menu_radius,
    };
    std::thread::sleep(Duration::from_millis(cfg.menu_settle_ms));
    let Some((menu, true)) = wait_for_every(cap, cfg.find_button_ms, 5, |f| {
        best_match(f, clerk, near, cfg.button_tolerance, 1).is_some()
    }) else {
        warn!("market: the clerk's menu never showed \"Open Marketplace\"");
        crate::input::recover();
        return false;
    };
    let Some(bm) = best_match(&menu, clerk, near, cfg.button_tolerance, 1) else {
        crate::input::recover();
        return false;
    };
    let (bx, by) = bm.center();
    let Some((bsx, bsy)) = (unsafe { crate::window::client_to_screen(hwnd, bx, by) }) else {
        crate::input::recover();
        return false;
    };
    info!("market: clicking \"Open Marketplace\"");
    crate::input::click_screen_keep(bsx, bsy);
    std::thread::sleep(Duration::from_millis(cfg.reopen_wait_ms));
    crate::input::recover();
    true
}

/// Put the client back on the offers list after the loop loses it.
///
/// First try the cheap case: the market is up but on another page (Featured
/// Shops), where "View Recent Offers" is one click away. Otherwise the market is
/// closed entirely and has to be reopened through a clerk. Returns true when an
/// action was taken, so the caller can rescan straight away.
pub fn recover_market(hwnd: HWND, cap: &GdiCapturer, tpls: &MarketTemplates, cfg: &BuyConfig) -> bool {
    let Ok(frame) = cap.capture_full_client() else {
        return false;
    };
    if let Some(t) = tpls.recent_offers.as_ref() {
        if let Some(m) = template::find(&frame, t, cfg.button_tolerance) {
            let (cx, cy) = m.center();
            if let Some((sx, sy)) = (unsafe { crate::window::client_to_screen(hwnd, cx, cy) }) {
                info!("market: clicking \"View Recent Offers\" to restore the list");
                crate::input::click_screen(sx, sy);
                return true;
            }
        }
    }
    reopen_market(hwnd, cap, tpls, cfg)
}

/// Capture repeatedly until `pred` accepts a frame, or `timeout_ms` elapses.
/// Returns the accepted frame (`true`) or the last one seen (`false`).
fn wait_for<F>(cap: &GdiCapturer, timeout_ms: u64, pred: F) -> Option<(Frame, bool)>
where
    F: Fn(&Frame) -> bool,
{
    wait_for_every(cap, timeout_ms, 100, pred)
}

/// As [`wait_for`], but with an explicit poll interval. The "Enter amount"
/// prompt is the case that needs this: it is the one step with no menu or panel
/// to anchor on, and it can appear and need answering inside a fraction of a
/// second, so it is polled as fast as the capture allows.
fn wait_for_every<F>(
    cap: &GdiCapturer,
    timeout_ms: u64,
    poll_ms: u64,
    pred: F,
) -> Option<(Frame, bool)>
where
    F: Fn(&Frame) -> bool,
{
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let mut last: Option<Frame> = None;
    loop {
        if let Ok(f) = cap.capture_full_client() {
            if pred(&f) {
                return Some((f, true));
            }
            last = Some(f);
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(poll_ms.max(1)));
    }
    last.map(|frame| (frame, false))
}

/// Lowest-error match of `tpl` inside `rect`.
fn best_match(frame: &Frame, tpl: &Template, rect: Rect, tol: u8, step: i32) -> Option<Match> {
    let mut ms = template::find_all_with_error(frame, tpl, rect, tol, step);
    ms.sort_by_key(|m| m.error);
    ms.first()
        .map(|m| Match { x: m.x, y: m.y, w: m.w, h: m.h })
}

/// The market panel is gone — i.e. the seller's shop has taken over. The Refresh
/// button only ever exists in the market, so it's our signal. This check is what
/// stops the item search from matching the icon in the market list itself.
fn shop_is_open(frame: &Frame, tpls: &MarketTemplates, tol: u8) -> bool {
    match tpls.refresh.as_ref() {
        Some(r) => template::find(frame, r, tol).is_none(),
        None => false, // no Refresh sprite → we can't tell, so don't assume
    }
}

/// The market is back (an Open or Refresh button is on screen).
fn market_is_back(frame: &Frame, tpls: &MarketTemplates, tol: u8) -> bool {
    template::find(frame, &tpls.open, tol).is_some()
        || tpls.refresh.as_ref().map(|r| template::find(frame, r, tol).is_some()).unwrap_or(false)
}

/// The client-space rect of a template match.
fn rect_of(m: Match) -> Rect {
    Rect {
        x1: m.x,
        y1: m.y,
        x2: m.x + m.w,
        y2: m.y + m.h,
    }
}

/// Save an (optionally annotated) debug frame to `<debug_dir>/<run_id>/<stage>.png`.
///
/// Everything a run produces — the annotated frames, the recording, and the
/// timeline `.log` — lives under one run-ID directory, so a finished run is a
/// single folder to open when something has to be explained.
fn save_debug(exe_dir: &Path, sub: &str, run_id: &str, stage: &str, frame: &Frame, strokes: &[Stroke]) {
    if sub.trim().is_empty() || run_id.is_empty() {
        return;
    }
    // Frames live in a `frames/` subdir of the run so the recorder's own
    // `<run_id>_NNNN.png` frames (and its cleanup) never touch them.
    let dir = exe_dir.join(sub).join(run_id).join("frames");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(error = %e, "could not create the buy debug dir");
        return;
    }
    let path = dir.join(format!("{stage}.png"));
    let image = if strokes.is_empty() {
        frame.clone()
    } else {
        annotate::annotated(frame, strokes)
    };
    match crate::market::save_frame_png(&path, &image) {
        Ok(()) => info!(path = %path.display(), "buy debug frame"),
        Err(e) => warn!(error = %e, "could not save a buy debug frame"),
    }
}

/// The annotated frames for a run, in stage order, capped at `max`.
fn collect_pngs(dir: &Path, max: usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")))
        .collect();
    out.sort();
    out.truncate(max.max(1));
    out
}

/// Build the Discord summary for a finished run.
fn run_summary(
    id: &str,
    row: &RowReading,
    verdict: &Verdict,
    outcome: &Outcome,
    elapsed_ms: u128,
) -> crate::discord::RunSummary {
    let (title, color) = match outcome {
        Outcome::Bought { units, buy_x, .. } => (
            format!(
                "Bought {units} × {} ({})",
                row.name,
                if *buy_x { "Buy X" } else { "Buy 1" }
            ),
            if verdict.snipe { 0xFFD700 } else { 0x2ECC71 },
        ),
        Outcome::Gone { .. } => (format!("{} gone before buying", row.name), 0x3498DB),
        Outcome::Failed(_) => (format!("{} aborted", row.name), 0xE74C3C),
    };
    let units = match outcome {
        Outcome::Bought { units, .. } => *units,
        _ => 0,
    };
    crate::discord::RunSummary {
        id: id.to_string(),
        name: row.name.clone(),
        title,
        outcome: outcome.describe(),
        color,
        price: verdict.price,
        units,
        max_price: verdict.max_price,
        elapsed_ms,
        snipe: verdict.snipe,
    }
}

fn patch(url: &str, key: &str, body: &str) -> Result<()> {
    ureq::patch(url)
        .set("apikey", key)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .set("Prefer", "return=minimal")
        .set("User-Agent", crate::cloud::USER_AGENT)
        .timeout(Duration::from_secs(10))
        .send_string(body)
        .map(|_| ())
        .map_err(|e| anyhow!("PATCH {url}: {e}"))
}

/// Percent-encode a query-string value (item names contain spaces and quotes).
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Record a completed purchase: units against the item's session cap, and the
/// big-snipe ration if it was one. Unticks the item once the cap is reached.
fn record_purchase(name: &str, units: i64, v: &Verdict) -> Result<()> {
    let (url, key) = crate::cloud::endpoint().ok_or_else(|| anyhow!("cloud not configured"))?;
    let base = url.trim_end_matches('/');

    let new_bought = v.bought + units as i32;
    let done = new_bought >= v.qty_limit;
    patch(
        &format!("{base}/rest/v1/watchlist?name=eq.{}", enc(name)),
        key,
        &format!("{{\"bought\":{new_bought},\"buy\":{}}}", if done { "false" } else { "true" }),
    )?;
    info!(name, bought = new_bought, qty_limit = v.qty_limit, unticked = done, "purchase recorded");

    if v.snipe {
        patch(
            &format!("{base}/rest/v1/buy_settings?id=eq.1"),
            key,
            &format!("{{\"snipes_used\":{}}}", v.snipes_used + 1),
        )?;
        info!(snipes_used = v.snipes_used + 1, "big-snipe ration consumed");
    }
    Ok(())
}

/// Zero the per-session counters. Called once when the scanner starts, so a new
/// run gets a fresh allowance (the "session" is one scanner run).
pub fn begin_session() -> Result<()> {
    if CFG.get().is_none() {
        return Ok(());
    }
    let (url, key) = crate::cloud::endpoint().ok_or_else(|| anyhow!("cloud not configured"))?;
    let base = url.trim_end_matches('/');
    // The RPC is the race-free way to do this, but it only exists once the
    // schema has been applied. Until then fall back to the table API — without
    // it a single count left over from a previous run caps an item out for good,
    // and the item looks armed in the manager while never being bought.
    if post(&format!("{base}/rest/v1/rpc/begin_session"), key, "{}").is_err() {
        patch(
            &format!("{base}/rest/v1/buy_settings?id=eq.1"),
            key,
            "{\"snipes_used\":0}",
        )?;
        patch(
            &format!("{base}/rest/v1/watchlist?buy=is.true"),
            key,
            "{\"bought\":0}",
        )?;
        info!("buy session started (counters reset through the table API)");
    } else {
        info!("buy session started (counters reset)");
    }
    cache().lock().at = None; // refetch, so what we report next is the fresh counters
    Ok(())
}

/// The **returning state**: get back to the offers list after a buy attempt.
///
/// UI-driven rather than a keypress, deliberately: Escape also drops you out of
/// the market window, which leaves the loop staring at the game world. So: give
/// the purchase a beat, click the shop's Return, click "Recent offers" to get
/// back to the list, then wait for the offer rows themselves before handing
/// back to the scanning loop. Every step is best-effort — the point is to end up
/// somewhere the scanning state can recognise, and each step says so in the log.
pub fn return_to_offers(
    exe_dir: &Path,
    hwnd: HWND,
    cap: &GdiCapturer,
    tpls: &MarketTemplates,
    cfg: &BuyConfig,
) {
    let buttons = exe_dir.join("Sprites").join("Utility").join("Buttons");
    let return_btn = Template::load(&buttons.join("Return.png")).ok();
    let recent = Template::load(&buttons.join("Recent offers.png")).ok();

    // 1. Let the purchase settle before touching anything.
    std::thread::sleep(Duration::from_millis(cfg.after_buy_ms));

    // 2. Click Return, if the shop is showing one.
    if let Some(t) = return_btn.as_ref() {
        if let Ok(f) = cap.capture_full_client() {
            if let Some(m) = template::find(&f, t, cfg.button_tolerance) {
                let (cx, cy) = m.center();
                if let Some((sx, sy)) = (unsafe { crate::window::client_to_screen(hwnd, cx, cy) }) {
                    info!("buy: clicking Return");
                    crate::input::click_screen(sx, sy);
                }
            }
        }
    }

    // 3. Find the "Recent offers" button and click it to reach the offers list.
    //    Polled as fast as the capture allows (1ms) with the timeout as the
    //    breakout: click it the moment it appears, and if it never does, carry
    //    on after the window rather than hanging.
    if let Some(t) = recent.as_ref() {
        match wait_for_every(cap, cfg.recent_offers_ms, 1, |f| {
            template::find(f, t, cfg.button_tolerance).is_some()
        }) {
            Some((f, true)) => {
                if let Some(m) = template::find(&f, t, cfg.button_tolerance) {
                    let (cx, cy) = m.center();
                    if let Some((sx, sy)) = (unsafe { crate::window::client_to_screen(hwnd, cx, cy) })
                    {
                        info!("buy: clicking Recent offers");
                        crate::input::click_screen(sx, sy);
                    }
                }
            }
            _ => warn!(
                ms = cfg.recent_offers_ms,
                "\"Recent offers\" not found; carrying on without it"
            ),
        }
    }

    // 4. …then wait until the rows themselves are listed again.
    let rows = wait_for(cap, cfg.market_back_ms, |f| {
        market_is_back(f, tpls, cfg.button_tolerance)
    })
    .map(|(_, ok)| ok)
    .unwrap_or(false);
    if !rows {
        warn!("the offer rows did not come back; the loop will retry");
    }
}

/// Append one line to `data/purchases.jsonl`.
///
/// The scanner runs unattended, so this is the only way to review what it did —
/// and to catch a junk purchase after the fact. Each line names the run `tag`,
/// which is also the folder holding that attempt's evidence:
/// `data/buy/<tag>/` with the annotated `NN_*.png` frames, the `<tag>.mp4`
/// recording, and the timestamped `<tag>.log` timeline.
pub fn log_attempt(exe_dir: &Path, tag: &str, name: &str, price: i64, outcome: &Outcome) {
    let (units, result) = match outcome {
        Outcome::Bought { units, buy_x, .. } => (*units, if *buy_x { "buy_x" } else { "buy_1" }),
        Outcome::Gone { .. } => (0, "gone"),
        Outcome::Failed(_) => (0, "aborted"),
    };
    let line = serde_json::json!({
        "ts_ms": crate::market::now_ms(),
        "tag": tag,
        "name": name,
        "price": price,
        "units": units,
        "result": result,
        "detail": outcome.describe(),
    });
    let dir = exe_dir.join("data");
    let written = std::fs::create_dir_all(&dir).and_then(|_| {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("purchases.jsonl"))
            .and_then(|mut f| writeln!(f, "{line}"))
    });
    if let Err(e) = written {
        warn!(error = %e, "could not append to data/purchases.jsonl");
    }
}

/// Re-find this item's row on a freshly captured market frame.
///
/// Used for the *retry* after a missed Open click: re-clicking the same stale
/// pixel can hit a row that has since moved, opening a stranger's shop. Matching
/// by price (and name when both were read), nearest the row we scanned, finds
/// where our offer actually is now. `None` means it is no longer on the list.
fn relocate_row(
    frame: &Frame,
    tpls: &MarketTemplates,
    market_cfg: &MarketConfig,
    ocr: Option<&crate::ocr::Ocr>,
    row: &RowReading,
) -> Option<Match> {
    let res = crate::market::scan(frame, tpls, market_cfg, ocr);
    let mut best: Option<(i32, Match)> = None;
    for r in &res.rows {
        if r.read.price != row.read.price {
            continue;
        }
        if !row.name.is_empty() && !r.name.is_empty() && r.name != row.name {
            continue;
        }
        let d = (r.open.y - row.open.y).abs();
        if best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, r.open));
        }
    }
    best.map(|(_, m)| m)
}

/// Last-resort locator: match the icon cropped from our market row against the
/// shop, for when the game drew no highlight.
///
/// Strict by design — `find_all_with_error` needs every masked template pixel
/// within `item_tolerance` — so a hit means the shop is drawing *our* item, not
/// merely something with similar colours. An un-highlighted item is drawn
/// cleanly, which is exactly the case the ring search can't handle.
fn icon_fallback(shop: &Frame, market: &Frame, row: &RowReading, icfg: &IconCfg, cfg: &BuyConfig) -> Option<Match> {
    // No highlight means no glow to survive, so match the full, un-eroded icon.
    let tpl = icon_template_plain(market, row, icfg)?;
    // Widen the band by the template's size: the item sits inside the crop with
    // margins, so matching an item at the band's own edge needs the template's
    // top-left to fall *outside* the band. Clipped to the frame on the far side.
    let area = Rect {
        x1: (cfg.glow_rect[0] - tpl.width).max(0),
        y1: (cfg.glow_rect[1] - tpl.height).max(0),
        x2: cfg.glow_rect[2] + tpl.width,
        y2: cfg.glow_rect[3] + tpl.height,
    };
    let mut ms = template::find_all_with_error(shop, &tpl, area, cfg.item_tolerance, 1);
    ms.sort_by_key(|m| m.error);
    ms.first().map(|m| {
        info!(x = m.x, y = m.y, w = m.w, h = m.h, error = m.error, "buy: no highlight; matched the item icon in the shop");
        Match { x: m.x, y: m.y, w: m.w, h: m.h }
    })
}

/// Execute one purchase, start to finish. Blocking — call it from the loop
/// thread so nothing else is scanning or clicking meanwhile.
///
/// Wraps [`execute_inner`] purely so there is exactly one place that records an
/// attempt, whatever the outcome.
#[allow(clippy::too_many_arguments)]
pub fn execute(
    exe_dir: &Path,
    hwnd: HWND,
    cap: &GdiCapturer,
    frame: &Frame,
    row: &RowReading,
    verdict: &Verdict,
    tpls: &MarketTemplates,
    market_cfg: &MarketConfig,
    ocr: Option<&crate::ocr::Ocr>,
    icon_cfg: &IconCfg,
    cfg: &BuyConfig,
    discord_cfg: &crate::discord::DiscordConfig,
) -> Outcome {
    // One ID for the whole run: it names the footage dir, every debug frame, the
    // timeline log, and the Discord posts, so any artifact can be tied back to
    // this attempt later.
    let tag = crate::market::now_ms().to_string();
    let run = RunLog::new(&tag);
    run.context("item", &row.name);
    run.context("price", verdict.price.to_string());
    run.context("max price", verdict.max_price.to_string());
    run.context(
        "session",
        format!("qty_limit {} / bought {}", verdict.qty_limit, verdict.bought),
    );
    run.step("start", format!("buy attempt for \"{}\" @ {}", row.name, verdict.price));

    // Record the whole attempt — from just before the shop is opened until the
    // purchase lands — so a wrong buy can be reviewed, not guessed at. The dir
    // is per-attempt so encoding never touches another attempt's frames.
    let recorder = cfg.record_purchases.then(|| {
        crate::record::Recorder::start(
            hwnd,
            cfg.record_fps,
            exe_dir.join(&cfg.debug_dir).join(&tag),
            tag.clone(),
        )
    });
    let outcome = execute_inner(
        exe_dir, hwnd, cap, frame, row, verdict, tpls, market_cfg, ocr, icon_cfg, cfg, &tag, &run,
    );

    match &outcome {
        Outcome::Bought { .. } => run.step("result", outcome.describe()),
        Outcome::Gone { .. } => run.warn("result", outcome.describe()),
        Outcome::Failed(_) => run.fail("result", outcome.describe()),
    }
    let elapsed_ms = run.elapsed_ms();
    let log_text = run.render();
    let timeline = run.timeline(1500);

    // Persist the timeline next to the footage; this is the artifact to read when
    // a run has to be explained.
    let run_dir = exe_dir.join(&cfg.debug_dir).join(&tag);
    if !cfg.debug_dir.trim().is_empty() {
        let written = std::fs::create_dir_all(&run_dir)
            .and_then(|_| std::fs::write(run_dir.join(format!("{tag}.log")), &log_text));
        if let Err(e) = written {
            warn!(error = %e, "could not write the run log");
        }
    }

    // The item icon, shared by the summary post and the purchase embed.
    let icon_png = crate::market::crop_icon(frame, &row.open, icon_cfg)
        .and_then(|(w, h, rgba)| crate::discord::encode_icon_png(w, h, &rgba));
    let summary = run_summary(&tag, row, verdict, &outcome, elapsed_ms);

    // Summary channel: a short, skimmable index entry (headline + run ID + icon).
    crate::discord::notify_summary(discord_cfg.clone(), summary.clone(), icon_png.clone());

    // Post the evidence (timeline + annotated images) and, when recorded, the
    // video. Both messages carry the same run ID.
    if let Some(rec) = recorder {
        let disco = discord_cfg.clone();
        let images_dir = run_dir.join("frames");
        let summary = summary.clone();
        // Encode on its own thread: the next pass must not wait on an ffmpeg run.
        std::thread::spawn(move || {
            let video = rec.stop();
            match &video {
                Some(p) => info!(path = %p.display(), "buy: purchase recording saved"),
                None => warn!("buy: purchase recording could not be encoded"),
            }
            if disco.send_runs {
                let images = collect_pngs(&images_dir, disco.max_images);
                crate::discord::notify_run(disco, summary, log_text, timeline, images, video);
            }
        });
    } else if discord_cfg.send_runs {
        let images = collect_pngs(&run_dir.join("frames"), discord_cfg.max_images);
        crate::discord::notify_run(discord_cfg.clone(), summary, log_text, timeline, images, None);
    }

    if let Outcome::Bought { units, .. } = outcome {
        if discord_cfg.is_configured() {
            crate::discord::notify_purchase(
                discord_cfg.clone(),
                verdict.name.clone(),
                row.seller.clone(),
                verdict.price,
                units as i64,
                verdict.profit,
                verdict.pct,
                verdict.snipe,
                icon_png,
            );
        }
    }
    log_attempt(exe_dir, &tag, &row.name, verdict.price, &outcome);
    outcome
}

#[allow(clippy::too_many_arguments)]
fn execute_inner(
    exe_dir: &Path,
    hwnd: HWND,
    cap: &GdiCapturer,
    frame: &Frame,
    row: &RowReading,
    verdict: &Verdict,
    tpls: &MarketTemplates,
    market_cfg: &MarketConfig,
    ocr: Option<&crate::ocr::Ocr>,
    icon_cfg: &IconCfg,
    cfg: &BuyConfig,
    tag: &str,
    run: &RunLog,
) -> Outcome {
    let name = row.name.clone();
    let shot = |stage: &str, f: &Frame, strokes: &[Stroke]| {
        save_debug(exe_dir, &cfg.debug_dir, tag, stage, f, strokes)
    };

    // The icon cropped from our market row drives both locators below, so a crop
    // that fails here is fatal to the attempt.
    let Some(icon_tpl) = icon_template(frame, row, icon_cfg, cfg) else {
        run.fail("crop", "could not crop the item icon from the scan");
        return Outcome::Failed("could not crop the item icon from the scan".into());
    };
    run.note(
        "crop",
        format!(
            "icon template {}x{} cropped from the market row",
            icon_tpl.width, icon_tpl.height
        ),
    );
    shot(
        "01_market",
        frame,
        &[
            Stroke::new(rect_of(row.open), annotate::MAGENTA, 2),
            Stroke::new(row.band, annotate::CYAN, 1),
            Stroke::new(crate::market::icon_region(&row.open, icon_cfg), annotate::ORANGE, 1),
        ],
    );

    // The shop's item band, defined up front so both the shop-open shot and the
    // locator below can annotate it.
    let glow_area = Rect {
        x1: cfg.glow_rect[0],
        y1: cfg.glow_rect[1],
        x2: cfg.glow_rect[2],
        y2: cfg.glow_rect[3],
    };

    // 1. Open the seller's shop on this row.
    //
    // The list can re-render between the scan and this click — a new offer
    // shuffles the rows down — so the pixel our row sat on may now be a
    // different item. Re-find the row on a fresh frame first (price-anchored, no
    // OCR) and click *that* Open button; if the row is gone, abort rather than
    // open a stranger's shop on a stale coordinate. A false miss is harmless —
    // the caller rescans and retries.
    let (cx, cy) = row.open.center();
    let mut target = (cx, cy);
    if let Ok(fresh) = cap.capture_full_client() {
        match relocate_row(&fresh, tpls, market_cfg, None, row) {
            Some(m) => {
                let (mx, my) = m.center();
                target = (mx, my);
                if (mx, my) != (cx, cy) {
                    info!(from_x = cx, from_y = cy, to_x = mx, to_y = my, "buy: the row moved; clicking its new position");
                    run.note("open", format!("row moved from ({cx},{cy}) to ({mx},{my}); clicking the verified position"));
                } else {
                    run.note("open", format!("row verified in place at ({cx},{cy})"));
                }
            }
            None => {
                run.warn("open", "the offer left the list before we could click Open");
                return Outcome::Gone { name };
            }
        }
    }
    let Some((sx, sy)) = (unsafe { crate::window::client_to_screen(hwnd, target.0, target.1) }) else {
        return Outcome::Failed("client_to_screen failed for the Open button".into());
    };
    info!(name = %name, price = verdict.price, "buy: clicking Open");
    run.step("open", format!("clicking Open at ({},{})", target.0, target.1));
    // The pointer has to stay put through the whole purchase (menus track it),
    // so remember where the operator had it and put it back at the end.
    let cursor = crate::input::cursor_pos();
    // Park the pointer somewhere inert between actions, so it never comes to
    // rest hovering something the game reacts to.
    if let Some((rx, ry)) = (unsafe {
        crate::window::client_to_screen(hwnd, cfg.recover_client[0], cfg.recover_client[1])
    }) {
        crate::input::set_recover_pos(rx, ry);
    }
    // 2. Wait for the seller's shop to replace the market panel. Without this
    //    the icon match finds the item sitting in the market list itself — the
    //    one place it is guaranteed to be, and never the shop we meant to buy from.
    //    A missed click is retried once, and the retry re-finds the row first:
    //    the list can move during the wait, and re-clicking the stale pixel then
    //    opens a different offer's shop.
    let mut opened = None;
    for attempt in 0..2 {
        if attempt == 0 {
            crate::input::click_screen_keep(sx, sy);
        } else {
            let mut retry = (sx, sy);
            if let Ok(fresh) = cap.capture_full_client() {
                if let Some(m) = relocate_row(&fresh, tpls, market_cfg, ocr, row) {
                    let (mx, my) = m.center();
                    if let Some(s) = (unsafe { crate::window::client_to_screen(hwnd, mx, my) }) {
                        retry = s;
                    }
                }
            }
            info!(x = retry.0, y = retry.1, "buy: the shop did not open; re-finding the row and clicking Open again");
            run.warn("open", "the shop did not open; re-finding the row and clicking Open again");
            crate::input::click_screen_keep(retry.0, retry.1);
        }
        match wait_for(cap, cfg.shop_wait_ms, |f| shop_is_open(f, tpls, cfg.button_tolerance)) {
            Some((f, true)) => {
                opened = Some(f);
                break;
            }
            Some(_) | None => continue,
        }
    }
    let Some(shop) = opened else {
        run.fail(
            "shop",
            format!("the shop did not open within {}ms over two tries", cfg.shop_wait_ms),
        );
        return Outcome::Failed(format!(
            "the shop did not open within {}ms over two tries",
            cfg.shop_wait_ms
        ));
    };
    run.step("shop", "seller's shop open; locating the item");
    shot("02_shop", &shop, &[Stroke::new(glow_area, annotate::YELLOW, 1)]);

    // 3. Find the item in the shop.
    //
    //    Usually the game highlights what we searched for, and that ring is the
    //    locator: shape-agnostic, the same for every item, and immune to the few
    //    pixels of drift between the market's copy and the shop's. But the
    //    highlight is not always drawn — a shop reopened on the retry click can
    //    come up without it — and then the ring search finds nothing while the
    //    item sits plainly on screen. For that case, match the icon cropped from
    //    our own market row: an un-highlighted item is drawn cleanly, which is
    //    exactly when a direct match works.
    let locate = |f: &Frame| -> Option<Match> {
        let cands = glow_candidates(f, glow_area, cfg);
        if let Some(m) = best_candidate(f, frame, row, icon_cfg, &cands, cfg) {
            info!(x = m.x, y = m.y, w = m.w, h = m.h, "buy: best icon match, right-clicking");
            return Some(m);
        }
        icon_fallback(f, frame, row, icon_cfg, cfg)
    };

    let mut m = locate(&shop);
    if m.is_none() {
        // Nothing yet — the shop may still be drawing. Re-read once before
        // calling the offer gone.
        run.note("locate", "no item found on the first read; re-reading the shop");
        std::thread::sleep(Duration::from_millis(cfg.item_retry_ms));
        if let Ok(f) = cap.capture_full_client() {
            m = locate(&f);
        }
    }
    let Some(m) = m else {
        run.warn("locate", "the item is no longer highlighted or matchable; calling the offer gone");
        shot("02_shop", &shop, &[Stroke::new(glow_area, annotate::YELLOW, 1)]);
        return Outcome::Gone { name };
    };
    run.step("locate", format!("item located at ({},{}) {}x{}", m.x, m.y, m.w, m.h));
    shot(
        "02_locate",
        &shop,
        &[
            Stroke::new(glow_area, annotate::YELLOW, 1),
            Stroke::new(rect_of(m), annotate::GREEN, 2),
        ],
    );
    let (ix, iy) = m.center();
    let Some((isx, isy)) = (unsafe { crate::window::client_to_screen(hwnd, ix, iy) }) else {
        return Outcome::Failed("client_to_screen failed for the item".into());
    };
    // 4. A player shop buys on the right-click itself ("right-click on shop to
    //    buy item"); others raise a menu with Buy 1 / Buy X. Wait for the menu,
    //    and if it never appears, check whether the right-click already did it.
    let stack = row.quantity as i64;
    // Shift + left-click fires the swapped "Buy 10" entry: one click, no menu and
    // no amount prompt. Prefer it for everything whenever it is available — the
    // speed is what keeps a good trade from being sniped out from under us — and
    // take `min(10, stack)`, accepting that a small stack may pass the session
    // cap. Later passes pick up whatever is left, so a big stack still fills up,
    // just ten at a time. With the fast path off (bulk_shift_below = 0) the
    // right-click menu still handles Buy 1 / Buy X with the exact amount.
    let bulk_take = stack.min(cfg.bulk_shift_below.max(0) as i64);
    let fast_bulk = cfg.bulk_shift_below > 0 && bulk_take > 0;
    let buy_x = use_buy_x(stack);
    let label = if buy_x { "Buy X" } else { "Buy 1" };
    let near = Rect {
        x1: (m.x - cfg.menu_radius).max(0),
        y1: (m.y - cfg.menu_radius).max(0),
        x2: m.x + m.w + cfg.menu_radius,
        y2: m.y + m.h + cfg.menu_radius,
    };
    let sibling = if buy_x { tpls.buy1.as_ref() } else { tpls.buy_x.as_ref() };
    let menu_ready = |f: &Frame| {
        let Some(button) = (if buy_x { tpls.buy_x.as_ref() } else { tpls.buy1.as_ref() }) else {
            return false;
        };
        // Step 1, not 2: a 17px-tall sprite scanned every other row can fall
        // between the samples and never match at all, which looks exactly like
        // "the menu never appeared".
        if best_match(f, button, near, cfg.button_tolerance, 1).is_none() {
            return false;
        }
        match (cfg.require_menu_pair, sibling) {
            (true, Some(s)) => best_match(f, s, near, cfg.button_tolerance, 1).is_some(),
            _ => true,
        }
    };

    let units: i64 = if fast_bulk {
        info!(stack, take = bulk_take, bulk = cfg.bulk_shift_below, "buy: shift-clicking the stack");
        run.step("buy", format!("shift-clicking {bulk_take} of a {stack} stack (fast path)"));
        crate::input::shift_click_screen_keep(isx, isy);
        std::thread::sleep(Duration::from_millis(250));
        shot("05_after", &shop, &[]);
        bulk_take
    } else {
        // Right-click the item, then wait for the menu. The click can land before
        // the client holds the hover it resolves against, in which case no menu is
        // raised at all — so allow it a second right-click before giving up.
        // The pointer stays on the item throughout: the menu belongs to it.
        //
        // After the floor below, the menu is polled every few ms and clicked as soon
        // as it is *complete* (both Buy 1 and Buy X visible), rather than being made
        // to wait out a fixed sleep. That is both quicker and stricter than a sleep:
        // it clicks the instant the entries are all there, and never before.
        run.step("menu", format!("right-clicking the item for {label}"));
        crate::input::right_click_screen_keep(isx, isy);
        const MENU_POLL_MS: u64 = 5;
        let mut menu = None;
        for attempt in 0..2 {
            std::thread::sleep(Duration::from_millis(cfg.menu_settle_ms));
            let half = (cfg.find_button_ms / 2).max(1);
            match wait_for_every(cap, half, MENU_POLL_MS, menu_ready) {
                Some((f, true)) => {
                    menu = Some((f, true));
                    break;
                }
                other => {
                    if let Some((last, _)) = other {
                        shot("03_rc", &last, &[Stroke::new(near, annotate::YELLOW, 1)]);
                    }
                    if attempt == 0 {
                        // Move the pointer clear *before* clicking again: if a menu
                        // did open, a second right-click at the item's spot lands on
                        // the menu instead, and picks whatever is under the cursor.
                        crate::input::recover();
                        std::thread::sleep(Duration::from_millis(150));
                        // With the pointer clear, look once more before clicking —
                        // the menu may simply have been missed the first time.
                        if let Some((f, true)) = wait_for_every(cap, half, MENU_POLL_MS, menu_ready) {
                            menu = Some((f, true));
                            break;
                        }
                        info!("buy: no menu after the right-click; clicking it once more");
                        run.warn("menu", "no menu after the right-click; clicking once more");
                        crate::input::right_click_screen_keep(isx, isy);
                    }
                }
            }
        }

        match menu {
            Some((menu_frame, true)) => {
                // A menu: click the entry we want.
                let button = (if buy_x { tpls.buy_x.as_ref() } else { tpls.buy1.as_ref() })
                    .expect("button checked above");
                let Some(bm) = best_match(&menu_frame, button, near, cfg.button_tolerance, 1) else {
                    run.fail("menu", format!("{label} disappeared before the click"));
                    return Outcome::Failed(format!("{label} disappeared before the click"));
                };
                shot(
                    "03_menu",
                    &menu_frame,
                    &[
                        Stroke::new(near, annotate::YELLOW, 1),
                        Stroke::new(rect_of(bm), annotate::GREEN, 2),
                    ],
                );
                run.step("menu", format!("menu up; clicking {label} at ({},{})", bm.x, bm.y));
                let (bx, by) = bm.center();
                let Some((bsx, bsy)) = (unsafe { crate::window::client_to_screen(hwnd, bx, by) }) else {
                    return Outcome::Failed("client_to_screen failed for the buy button".into());
                };
                info!(button = label, "buy: clicking");
                crate::input::click_screen_keep(bsx, bsy);

                // 5. Buy X asks for an amount.
                if buy_x {
                    let Some(prompt) = tpls.enter_amount.as_ref() else {
                        return Outcome::Failed("Validations/Enter amount.png is missing".into());
                    };
                    let area = Rect {
                        x1: cfg.amount_rect[0],
                        y1: cfg.amount_rect[1],
                        x2: cfg.amount_rect[2],
                        y2: cfg.amount_rect[3],
                    };
                    // Polled as fast as the capture allows, then two beats: one for
                    // the prompt to take focus before the digits, one for the digits
                    // to land before Enter confirms them.
                    let Some((prompt_frame, seen)) =
                        wait_for_every(cap, cfg.amount_wait_ms, 1, |f| {
                            best_match(f, prompt, area, cfg.button_tolerance, 1).is_some()
                        })
                    else {
                        run.fail("amount", "screen capture failed while waiting for Enter amount");
                        return Outcome::Failed("screen capture failed while waiting for Enter amount".into());
                    };
                    shot("04_amount", &prompt_frame, &[Stroke::new(area, annotate::CYAN, 1)]);
                    if !seen {
                        run.fail("amount", format!("\"Enter amount\" not found within {}ms", cfg.amount_wait_ms));
                        return Outcome::Failed(format!(
                            "\"Enter amount\" not found within {}ms",
                            cfg.amount_wait_ms
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(cfg.amount_pause_ms));
                    let want = amount_for(stack, verdict.qty_limit, verdict.bought);
                    if want <= 0 {
                        run.fail("amount", "nothing left within the session cap");
                        return Outcome::Failed("nothing left within the session cap".into());
                    }
                    info!(want, stack, limit = verdict.qty_limit, bought = verdict.bought, "buy: typing the amount");
                    run.step("amount", format!("typing {want} and pressing Enter"));
                    crate::input::type_digits(&want.to_string(), cfg.key_delay_ms);
                    std::thread::sleep(Duration::from_millis(cfg.enter_delay_ms));
                    crate::input::tap_enter(cfg.key_delay_ms);
                    want
                } else {
                    info!("buy: Buy 1 clicked");
                    run.step("buy", "Buy 1 clicked");
                    1
                }
            }
            _ => {
                // No menu within the window. Either the right-click bought the item
                // outright (player shops buy on right-click), or the menu is simply
                // late — it took ~4s in a live run. Once it opens it **covers** the
                // item, hiding the glow, so a bare glow check would read that as a
                // successful purchase. Look for the menu on the last frame first.
                if let Some((last, _)) = &menu {
                    if menu_ready(last) {
                        run.warn("menu", "the buy menu appeared after the window; retrying");
                        return Outcome::Failed(
                            "the buy menu appeared after the window; retrying".into(),
                        );
                    }
                }
                let bought = wait_for(cap, cfg.direct_buy_ms, |f| {
                    // Only believe "it's gone" while the shop is still on screen. If
                    // the client went away — quit, minimised, alt-tabbed — every
                    // capture is empty, no glow is found, and that reads as a
                    // purchase that never happened.
                    shop_is_open(f, tpls, cfg.button_tolerance)
                        && match find_glow(f, glow_area, cfg) {
                            None => true,
                            Some(g) => !overlaps(g, m),
                        }
                })
                .map(|(_, ok)| ok)
                .unwrap_or(false);
                if !bought {
                    run.fail(
                        "menu",
                        format!("right-click produced no {label} menu and the item is still listed"),
                    );
                    return Outcome::Failed(format!(
                        "right-click produced no {label} menu and the item is still listed"
                    ));
                }
                shot("05_after", &shop, &[]);
                let want = amount_for(stack, verdict.qty_limit, verdict.bought).max(1);
                info!(items = want, "buy: right-click bought the item directly");
                run.step("buy", "right-click bought the item directly");
                want
            }
        }
    };

    std::thread::sleep(Duration::from_millis(400));
    if let Ok(f) = cap.capture_full_client() {
        shot("05_after", &f, &[]);
    }

    std::thread::sleep(Duration::from_millis(400));
    if let Ok(f) = cap.capture_full_client() {
        shot("05_after", &f, &[]);
    }

    match record_purchase(&name, units, verdict) {
        Ok(()) => run.note("record", format!("recorded {units} against the session cap")),
        Err(e) => {
            warn!(error = %e, "could not record the purchase in Supabase");
            run.warn("record", format!("could not record the purchase: {e}"));
        }
    }
    // Leaving the shop is the caller's job (see `return_to_offers`): it has to
    // happen whatever the outcome, including a purchase that aborted half-way.
    // What state we actually left the game in — the market, or something else.
    if let Ok(f) = cap.capture_full_client() {
        shot("06_back", &f, &[]);
    }
    // Park the pointer somewhere inert, then put it back where the operator had
    // it. Done here rather than per click: a right-click's menu belongs to the
    // item's position, so the pointer has to stay until the menu work is over.
    crate::input::recover();
    if let Some((x, y)) = cursor {
        crate::input::set_cursor_pos(x, y);
    }

    Outcome::Bought { name, units, buy_x }
}

/// How many rows the watchlist holds, ticked or not.
///
/// The rules endpoint only returns ticked rows, so this is the only way to tell
/// "nothing is armed" from "the item I meant isn't armed".
fn watchlist_total() -> Result<i64> {
    let (url, key) = crate::cloud::endpoint().ok_or_else(|| anyhow!("cloud not configured"))?;
    let base = url.trim_end_matches('/');
    let resp = ureq::get(&format!("{base}/rest/v1/watchlist?select=name"))
        .set("apikey", key)
        .set("Authorization", &format!("Bearer {key}"))
        .set("User-Agent", crate::cloud::USER_AGENT)
        .timeout(Duration::from_secs(10))
        .call()
        .map_err(|e| anyhow!("GET watchlist: {e}"))?;
    let list: Vec<serde_json::Value> = serde_json::from_str(&resp.into_string()?)?;
    Ok(list.len() as i64)
}

/// A one-line summary of the two switches that gate a click, for startup.
/// Forces a rules refresh, so what it reports is current — asking the manager
/// "why didn't it buy?" starts here.
pub fn arm_status() -> String {
    let dry = CFG.get().map(|c| c.dry_run).unwrap_or(true);
    let (settings, rules) = match refresh(true) {
        Ok(()) => {
            let c = cache().lock();
            (c.settings.clone(), c.rules.len())
        }
        Err(e) => return format!("DRY RUN (cannot reach the rules: {e})"),
    };
    // "N of M": the rules endpoint only returns ticked rows, so a forgotten Buy
    // toggle would otherwise read exactly like a broken bot.
    let total = watchlist_total().unwrap_or(0);
    let master = if settings.enabled { "ON" } else { "OFF" };
    let mut s = format!(
        "dry_run = {dry} · manager master switch {master} · {rules} of {total} item(s) ticked"
    );
    if rules == 0 && total > 0 {
        s.push_str("  → nothing is ticked, so nothing can be bought");
    }
    if dry {
        s.push_str("  → REPORTS ONLY: set dry_run = false in market.toml to arm it");
    } else if !settings.enabled {
        s.push_str("  → armed, but the master switch is off in the manager");
    } else {
        s.push_str("  → ARMED");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(name: &str, max_price: i64, qty: i32, bought: i32, median: f64) -> Rule {
        Rule {
            name: name.to_string(),
            max_price: Some(max_price),
            qty_limit: qty,
            bought,
            median: Some(median),
            p10: None,
            icon: None,
        }
    }

    fn settings(min_margin: i64, max_snipes: i32, used: i32) -> Settings {
        Settings { enabled: true, min_margin, max_snipes, snipes_used: used }
    }

    #[test]
    fn buys_below_threshold_with_profit_after_fee() {
        // median 1000 → resale 900; buying at 400 leaves 500.
        let rules = vec![rule("Ruby", 500, 1, 0, 1000.0)];
        let v = evaluate(&[("Ruby".to_string(), 400)], &rules, &settings(1e12 as i64, 2, 0));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].profit, 500);
        assert_eq!(v[0].decision, Decision::WouldBuy);
        assert!(!v[0].snipe);
    }

    #[test]
    fn ignores_quotes_above_the_threshold() {
        let rules = vec![rule("Ruby", 500, 1, 0, 1000.0)];
        assert!(evaluate(&[("Ruby".to_string(), 501)], &rules, &settings(0, 2, 0)).is_empty());
    }

    #[test]
    fn ignores_items_that_reached_their_quantity_cap() {
        let rules = vec![rule("Ruby", 500, 2, 2, 1000.0)];
        assert!(evaluate(&[("Ruby".to_string(), 100)], &rules, &settings(0, 2, 0)).is_empty());
    }

    #[test]
    fn rations_big_snipes_per_session() {
        // 22B profit on a 25B item: a big snipe.
        let rules = vec![rule("Osmumten's fang", 250_000_000, 1, 0, 21_950_000_000.0)];
        let s = settings(1_000_000_000, 2, 2); // ration spent
        let v = evaluate(&[("Osmumten's fang".to_string(), 250_000_000)], &rules, &s);
        assert_eq!(v[0].snipe, true);
        assert_eq!(v[0].decision, Decision::SnipeCapped);

        // …but with 0 allowed it is never taken, and with room left it goes.
        let v0 = evaluate(&[("Osmumten's fang".to_string(), 250_000_000)], &rules, &settings(1_000_000_000, 0, 0));
        assert_eq!(v0[0].decision, Decision::SnipeCapped);
        let v1 = evaluate(&[("Osmumten's fang".to_string(), 250_000_000)], &rules, &settings(1_000_000_000, 2, 1));
        assert_eq!(v1[0].decision, Decision::WouldBuy);
    }

    #[test]
    fn percentage_alone_would_have_mis_ranked_these() {
        // 45M profit (99.99% of a tiny buy) vs 22B profit (98.9%): the absolute
        // rule must classify only the second as a big snipe.
        let rules = vec![
            rule("Cheap thing", 1000, 1, 0, 50_000_000.0),
            rule("Osmumten's fang", 250_000_000, 1, 0, 21_950_000_000.0),
        ];
        let s = settings(1_000_000_000, 2, 0);
        let v = evaluate(
            &[("Cheap thing".to_string(), 1000), ("Osmumten's fang".to_string(), 250_000_000)],
            &rules,
            &s,
        );
        let cheap = v.iter().find(|x| x.name == "Cheap thing").unwrap();
        let fang = v.iter().find(|x| x.name == "Osmumten's fang").unwrap();
        assert!(!cheap.snipe, "45M profit must not consume a rationed slot");
        assert!(fang.snipe, "22B profit must consume a rationed slot");
    }

    #[test]
    fn master_switch_off_blocks_everything() {
        let rules = vec![rule("Ruby", 500, 1, 0, 1000.0)];
        let s = Settings { enabled: false, ..settings(0, 2, 0) };
        assert_eq!(evaluate(&[("Ruby".to_string(), 100)], &rules, &s)[0].decision, Decision::Paused);
    }

    #[test]
    fn amount_never_exceeds_the_stack_or_the_remaining_allowance() {
        // 667 on offer, 1000 allowed, none bought yet → the whole stack.
        assert_eq!(amount_for(667, 1000, 0), 667);
        // …but only what's left once we've been buying.
        assert_eq!(amount_for(667, 1000, 750), 250);
        // A stack larger than the cap is trimmed to the cap.
        assert_eq!(amount_for(5000, 1000, 0), 1000);
        // Nothing left → nothing to ask for (never a negative amount).
        assert_eq!(amount_for(10, 1000, 1000), 0);
        assert_eq!(amount_for(10, 5, 9), 0);
    }

    #[test]
    fn singles_use_buy_one_and_stacks_use_buy_x() {
        assert!(!use_buy_x(1), "a lone item is a Buy 1");
        assert!(use_buy_x(2));
        assert!(use_buy_x(667));
    }

    #[test]
    fn verdicts_carry_what_sizing_a_buy_x_needs() {
        let rules = vec![rule("Voting token", 4_100_000, 1000, 400, 7_000_000.0)];
        let v = evaluate(&[("Voting token".to_string(), 4_000_000)], &rules, &settings(2_000_000_000, 2, 0));
        assert_eq!(v[0].qty_limit, 1000);
        assert_eq!(v[0].bought, 400);
        // …so a 667 stack is trimmed to the 600 we may still take.
        assert_eq!(amount_for(667, v[0].qty_limit, v[0].bought), 600);
    }

    #[test]
    fn url_encoding_handles_real_item_names() {
        assert_eq!(enc("Voting token"), "Voting%20token");
        assert_eq!(enc("Morrigan's thro.."), "Morrigan%27s%20thro..");
        assert_eq!(enc("A+B"), "A%2BB");
    }

    /// Diagnostic: where does the shop's item actually sit, and how far off is
    /// our template there? Prints the best-error position for each erosion.
    #[test]
    fn dump_shop_match_errors() {
        let Ok(entries) = std::fs::read_dir("data/buy") else {
            return;
        };
        let mut markets: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with("_01_market.png"))
            .collect();
        markets.sort();
        let Some(market_path) = markets.pop() else { return };
        let shop_path = std::path::PathBuf::from(
            market_path.to_string_lossy().replace("_01_market.png", "_02_shop.png"),
        );
        if !shop_path.exists() {
            return;
        }
        let market = load_frame(&market_path);
        let shop = load_frame(&shop_path);
        let cfg = crate::market::MarketConfig::default_config();
        let tpls = crate::market::load_templates(std::path::Path::new("Sprites")).expect("sprites");
        let scan = crate::market::scan(&market, &tpls, &cfg, None);
        let Some(row) = scan.rows.first() else { return };

        let base = icon_template(&market, row, &cfg.icon, &cfg.buy).expect("template");
        let area = Rect { x1: 0, y1: 40, x2: shop.width, y2: 260 };
        eprintln!("=== market row: open=({},{}) size={}x{} qty={}", row.open.x, row.open.y, row.open.w, row.open.h, row.quantity);

        // How much of the crop survived background removal, before our masking?
        if let Some((cw, ch, rgba)) = crate::market::crop_icon(&market, &row.open, &cfg.icon) {
            let opaque = rgba.chunks(4).filter(|p| p[3] > 0).count();
            eprintln!("=== crop {cw}x{ch}: {opaque} opaque px of {} total", cw * ch);
        }
        // …and with background removal switched off, for comparison.
        {
            let mut c2 = cfg.icon.clone();
            c2.remove_bg = false;
            if let Some((cw, ch, rgba)) = crate::market::crop_icon(&market, &row.open, &c2) {
                let opaque = rgba.chunks(4).filter(|p| p[3] > 0).count();
                eprintln!("=== crop {cw}x{ch} (no bg removal): {opaque} opaque px of {}", cw * ch);
            }
        }

        // The keyed silhouette, as art: is it a thin outline or a solid shape?
        if let Some((cw, ch, rgba)) = crate::market::crop_icon(&market, &row.open, &cfg.icon) {
            eprintln!("=== keyed silhouette {cw}x{ch} ===");
            for y in 0..ch {
                let mut line = String::new();
                for x in 0..cw {
                    let a = rgba[((y * cw + x) * 4 + 3) as usize];
                    line.push(if a > 0 { '#' } else { '.' });
                }
                eprintln!("{line}");
            }
        }

        // The prepared template, as art, plus the best match in the shop.
        eprintln!("=== prepared template {}x{} ===", base.width, base.height);
        for y in 0..base.height {
            let mut line = String::new();
            for x in 0..base.width {
                let i = (y * base.width + x) as usize;
                line.push(if base.mask[i] { '#' } else { '.' });
            }
            eprintln!("{line}");
        }
        let ink = base.mask.iter().filter(|m| **m).count();
        let mut ms = template::find_all_with_error(&shop, &base, area, 255, 1);
        ms.sort_by_key(|m| m.error);
        for m in ms.iter().take(3) {
            eprintln!(
                "  candidate ({},{}) err={:.1}/px ink={}",
                m.x, m.y, m.error as f64 / ink.max(1) as f64, ink
            );
        }
        eprintln!(
            "  match within tolerance {}: {:?}",
            cfg.buy.item_tolerance,
            best_match(&shop, &base, area, cfg.buy.item_tolerance, 1).map(|m| (m.x, m.y))
        );

        // The shop's own rendering of the item, keyed the same way, so the two
        // silhouettes can be compared directly (and the glow's thickness seen).
        for (label, ox, oy) in [("shop item A", 542, 101), ("shop item B", 480, 101)] {
            let fake = Match { x: ox, y: oy, w: 32, h: 19 };
            if let Some((cw, ch, rgba)) = crate::market::crop_icon(&shop, &fake, &cfg.icon) {
                let ink = rgba.chunks(4).filter(|p| p[3] > 0).count();
                eprintln!("=== {label} keyed {cw}x{ch} ({ink} px) ===");
                for y in 0..ch.min(34) {
                    let mut line = String::new();
                    for x in 0..cw {
                        let a = rgba[((y * cw + x) * 4 + 3) as usize];
                        line.push(if a > 0 { '#' } else { '.' });
                    }
                    eprintln!("{line}");
                }
            }
        }

        // Is the glow itself the reliable signal? Look for bright-yellow blobs
        // at a few thresholds and report the overall extent of each.
        for (label, rmin, gmin, bmax) in [("tight", 200, 170, 120), ("loose", 170, 140, 150)] {
            let panel = Rect { x1: 45, y1: 55, x2: 500, y2: 340 };
            let mut n = 0;
            let (mut x1, mut y1, mut x2, mut y2) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
            for y in panel.y1..panel.y2.min(shop.height) {
                for x in panel.x1..panel.x2.min(shop.width) {
                    if let Some(p) = shop.pixel(x, y) {
                        let (b, g, r) = (p[0] as i32, p[1] as i32, p[2] as i32);
                        if r > rmin && g > gmin && b < bmax {
                            n += 1;
                            x1 = x1.min(x);
                            x2 = x2.max(x);
                            y1 = y1.min(y);
                            y2 = y2.max(y);
                        }
                    }
                }
            }
            eprintln!(
                "=== yellow ({label} r>{rmin} g>{gmin} b<{bmax}): {n} px, bbox x{x1}..{x2} y{y1}..{y2}"
            );
        }
    }

    /// Diagnostic: do the Buy sprites match the menu the game actually shows?
    /// Uses a captured frame that contains the right-click menu.
    #[test]
    fn dump_buy_button_matches_in_a_menu_frame() {
        let p = std::path::Path::new("data/buy/1790801828733_06_back.png");
        if !p.exists() {
            return;
        }
        let f = load_frame(p);
        let tpls = crate::market::load_templates(std::path::Path::new("Sprites")).expect("sprites");
        let area = Rect { x1: 0, y1: 0, x2: f.width, y2: f.height };
        for (label, t) in [("Buy 1", tpls.buy1.as_ref()), ("Buy X", tpls.buy_x.as_ref())] {
            match t {
                Some(t) => {
                    eprintln!("{label} sprite is {}x{}", t.width, t.height);
                    for tol in [10u8, 20, 30, 45, 60] {
                        let m = best_match(&f, t, area, tol, 1);
                        eprintln!("   tol {tol:>3}: {:?}", m.map(|m| (m.x, m.y, m.w, m.h)));
                    }
                }
                None => eprintln!("{label} sprite missing"),
            }
        }
    }

    /// Load a captured PNG as a BGRA frame, the way the scanner sees it.
    fn load_frame(p: &std::path::Path) -> Frame {
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
        Frame { width: w, height: h, bgra }
    }

    /// The glow is yellow. Green shares its low-blue signature and a big green
    /// item — a pouch, a scroll — then out-sizes the real glow and gets bought
    /// by mistake. This is the guard against that.
    #[test]
    fn the_glow_is_yellow_not_green() {
        let cfg = BuyConfig::default();
        let px = |r: u8, g: u8, b: u8| [b, g, r, 255]; // frame pixels are BGRA

        // Measured from real frames: the same item at its dimmest and brightest.
        assert!(is_glow_pixel(&px(131, 125, 35), &cfg), "the dim glow must pass");
        assert!(is_glow_pixel(&px(236, 235, 5), &cfg), "the bright glow must pass");

        // The things that fooled the old test.
        assert!(!is_glow_pixel(&px(50, 200, 50), &cfg), "bright green must not pass");
        assert!(!is_glow_pixel(&px(60, 180, 40), &cfg), "olive green must not pass");
        assert!(!is_glow_pixel(&px(90, 75, 50), &cfg), "the panel must not pass");
        assert!(!is_glow_pixel(&px(220, 30, 30), &cfg), "a red must not pass");
    }

    /// The glow pulses, and an absolute-brightness test missed it whenever the
    /// pulse was dim: this frame caught the same item at RGB(131,125,35) while
    /// others read RGB(236,235,5). Measured across six captured frames, the
    /// panel sits at yellowness ~32 and the glow never drops below ~186, so the
    /// relative test holds in every phase.
    #[test]
    fn finds_the_glow_in_its_dim_phase() {
        let p = std::path::Path::new("data/buy/1790801181700_02_shop.png");
        if !p.exists() {
            return;
        }
        let shop = load_frame(p);
        let cfg = BuyConfig::default();
        let area = Rect {
            x1: cfg.glow_rect[0],
            y1: cfg.glow_rect[1],
            x2: cfg.glow_rect[2],
            y2: cfg.glow_rect[3],
        };
        let g = find_glow(&shop, area, &cfg).expect("the dim glow must still be found");
        eprintln!("dim-frame glow at ({},{}) {}x{}", g.x, g.y, g.w, g.h);
        assert!(g.w >= 6 && g.h >= 6 && g.w <= 40 && g.h <= 40, "item-sized, got {}x{}", g.w, g.h);
        assert!(
            g.x >= area.x1 && g.y >= area.y1 && g.x + g.w <= area.x2 && g.y + g.h <= area.y2,
            "glow must sit inside the shop's item band, got ({},{}) {}x{}",
            g.x,
            g.y,
            g.w,
            g.h
        );
    }

    /// The amulet-of-avarice frame: the highlight sat on a bottom row the old
    /// `glow_rect` (y2 = 260) cut off, so the snipe was skipped while the item
    /// glowed on screen. The band must reach it.
    #[test]
    fn finds_a_highlight_on_a_low_row() {
        let p = std::path::Path::new("data/buy/1791755832461_02_shop.png");
        if !p.exists() {
            return;
        }
        let shop = load_frame(p);
        let cfg = BuyConfig::default();
        let area = Rect {
            x1: cfg.glow_rect[0],
            y1: cfg.glow_rect[1],
            x2: cfg.glow_rect[2],
            y2: cfg.glow_rect[3],
        };
        let cands = glow_candidates(&shop, area, &cfg);
        eprintln!(
            "low-row candidates: {:?}",
            cands.iter().map(|m| (m.x, m.y, m.w, m.h)).collect::<Vec<_>>()
        );
        assert!(!cands.is_empty(), "the bottom-row highlight must be found");
        let short = Rect { x1: 45, y1: 85, x2: 480, y2: 260 };
        assert!(
            glow_candidates(&shop, short, &cfg).is_empty(),
            "the old short band is what missed it"
        );
    }

    /// The Craw's bow frame: the offer was already gone when the shop opened (the
    /// shop now lists a *different* bow), so there is no highlight. The icon
    /// fallback must not latch onto that look-alike and buy it.
    #[test]
    fn icon_fallback_does_not_match_an_unrelated_item() {
        let market_p = std::path::Path::new("data/buy/1791515140771_01_market.png");
        let shop_p = std::path::Path::new("data/buy/1791515140771_02_shop.png");
        if !market_p.exists() || !shop_p.exists() {
            return;
        }
        let market = load_frame(market_p);
        let shop = load_frame(shop_p);
        let mcfg = crate::market::MarketConfig::default_config();
        let tpls = crate::market::load_templates(std::path::Path::new("Sprites")).expect("sprites");
        let scan = crate::market::scan(&market, &tpls, &mcfg, None);
        let Some(row) = scan.rows.first() else {
            return;
        };
        let area = Rect {
            x1: mcfg.buy.glow_rect[0],
            y1: mcfg.buy.glow_rect[1],
            x2: mcfg.buy.glow_rect[2],
            y2: mcfg.buy.glow_rect[3],
        };
        assert!(
            glow_candidates(&shop, area, &mcfg.buy).is_empty(),
            "this frame is the no-highlight case"
        );
        let hit = icon_fallback(&shop, &market, row, &mcfg.icon, &mcfg.buy);
        assert!(
            hit.is_none(),
            "the offer is gone; the fallback must not latch onto the look-alike bow: {hit:?}"
        );
    }

    /// Whatever a frame yields must be item-sized and on the item row — the
    /// guard against the panel's own border and header reading as "yellow".
    #[test]
    fn any_glow_found_is_item_sized_and_on_the_item_row() {
        let Ok(entries) = std::fs::read_dir("data/buy") else {
            return;
        };
        let cfg = BuyConfig::default();
        let area = Rect {
            x1: cfg.glow_rect[0],
            y1: cfg.glow_rect[1],
            x2: cfg.glow_rect[2],
            y2: cfg.glow_rect[3],
        };
        let mut frames: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with("_02_shop.png"))
            .collect();
        frames.sort();

        let mut found = 0;
        for f in frames {
            let shop = load_frame(&f);
            if let Some(g) = find_glow(&shop, area, &cfg) {
                eprintln!("{}: glow at ({},{}) {}x{}", f.file_name().unwrap().to_string_lossy(), g.x, g.y, g.w, g.h);
                assert!(g.w >= 6 && g.h >= 6 && g.w <= 40 && g.h <= 40, "item-sized, got {}x{}", g.w, g.h);
                // The band excludes the panel's border, header and chat, so a hit
                // inside it is the highlighted item — in whichever slot it sits.
                assert!(
                    g.x >= area.x1 && g.y >= area.y1 && g.x + g.w <= area.x2 && g.y + g.h <= area.y2,
                    "glow must sit inside the shop's item band, got ({},{}) {}x{}",
                    g.x,
                    g.y,
                    g.w,
                    g.h
                );
                found += 1;
            }
        }
        if found > 0 {
            eprintln!("{found} frame(s) had a glow");
        }
    }

    /// Diagnostic: render an annotated shop frame to `target/annotated_sample.png`
    /// so the boxes can be eyeballed against the region they are meant to mark.
    #[test]
    fn dump_annotated_sample_if_frame_present() {
        let cfg = BuyConfig::default();
        let glow = Rect {
            x1: cfg.glow_rect[0],
            y1: cfg.glow_rect[1],
            x2: cfg.glow_rect[2],
            y2: cfg.glow_rect[3],
        };
        let mut frames: Vec<std::path::PathBuf> = Vec::new();
        // Newest runs' raw "after the buy" shop frames (unannotated).
        if let Ok(runs) = std::fs::read_dir("data/buy") {
            for e in runs.flatten() {
                let p = e.path().join("frames").join("05_after.png");
                if p.exists() {
                    frames.push(p);
                }
            }
        }
        // Legacy flat shop frames.
        if let Ok(entries) = std::fs::read_dir("data/buy") {
            for e in entries.flatten() {
                let p = e.path();
                if p.to_string_lossy().ends_with("_02_shop.png") {
                    frames.push(p);
                }
            }
        }
        frames.sort();
        for p in frames.iter().rev() {
            let f = load_frame(p);
            let Some(g) = find_glow(&f, glow, &cfg) else {
                continue;
            };
            let strokes = vec![
                Stroke::new(glow, annotate::YELLOW, 1),
                Stroke::new(rect_of(g), annotate::GREEN, 2),
            ];
            let out = annotate::annotated(&f, &strokes);
            let _ = std::fs::create_dir_all("target");
            let _ = crate::market::save_frame_png(
                std::path::Path::new("target/annotated_sample.png"),
                &out,
            );
            eprintln!("annotated {} glow at ({},{}) {}x{}", p.display(), g.x, g.y, g.w, g.h);
            break;
        }
    }

    /// Diagnostic: draw the change-watch region (cyan) and the shop search band
    /// (yellow) over the last scanned frame, so the covered sections are clear.
    #[test]
    fn dump_scan_regions_if_frame_present() {
        let p = std::path::Path::new("data/last_scan.png");
        if !p.exists() {
            return;
        }
        let f = load_frame(p);
        let cfg = MarketConfig::default_config();
        let watch = crate::market::watch_rect(&cfg.watch);
        let glow = Rect {
            x1: cfg.buy.glow_rect[0],
            y1: cfg.buy.glow_rect[1],
            x2: cfg.buy.glow_rect[2],
            y2: cfg.buy.glow_rect[3],
        };
        let out = annotate::annotated(
            &f,
            &[
                Stroke::new(watch, annotate::CYAN, 2),
                Stroke::new(glow, annotate::YELLOW, 2),
            ],
        );
        let _ = crate::market::save_frame_png(std::path::Path::new("target/scan_regions.png"), &out);
        eprintln!(
            "frame {}x{}  watch x{}..{} y{}..{}  glow x{}..{} y{}..{}",
            f.width, f.height,
            watch.x1, watch.x2, watch.y1, watch.y2,
            glow.x1, glow.x2, glow.y1, glow.y2,
        );
    }
}

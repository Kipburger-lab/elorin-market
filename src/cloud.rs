//! Push collected offers to Supabase (free Postgres + REST) for the online
//! dashboard.
//!
//! Writes are **queued to disk** and flushed in batches by a background thread,
//! so the scanner never blocks on the network and nothing is lost while offline.
//! A failed flush leaves the rows in the queue for the next attempt.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// `[cloud]` section of `market.toml`. Disabled by default; every field has a
/// default so the section can be minimal.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CloudConfig {
    pub enable: bool,
    /// Supabase project URL, e.g. `https://abcd.supabase.co`.
    pub url: String,
    /// service_role key — write access; stays on this machine, never shipped.
    pub service_key: String,
    pub table: String,
    pub icons_bucket: String,
    /// Pending-work files, relative to the exe dir.
    pub queue: String,
    pub icon_queue: String,
    /// Flush cadence and max rows per request.
    pub flush_ms: u64,
    pub batch_max: usize,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            enable: false,
            url: String::new(),
            service_key: String::new(),
            table: "offers".to_string(),
            icons_bucket: "icons".to_string(),
            queue: "data/cloud_queue.jsonl".to_string(),
            icon_queue: "data/cloud_icons.txt".to_string(),
            flush_ms: 10_000,
            batch_max: 200,
        }
    }
}

/// One offer row as stored in Postgres.
///
/// Every field is always serialized (missing values become `null`): PostgREST
/// rejects a bulk insert whose objects have differing key sets with PGRST102
/// "All object keys must match".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudRow {
    pub ts_ms: u64,
    pub name: String,
    #[serde(default)]
    pub seller: Option<String>,
    pub price: u64,
    pub quantity: u32,
    #[serde(default)]
    pub icon: Option<String>,
}

#[derive(Default)]
struct State {
    /// (name, seller, price) already queued this session — the same standing offer
    /// re-seen by a later scan is not stored twice.
    seen: HashSet<(String, String, u64)>,
    queued_icons: HashSet<String>,
}

struct Cloud {
    cfg: CloudConfig,
    /// Exe dir (the queue paths are relative to it).
    dir: PathBuf,
    /// Where the icon PNGs live on disk.
    icons_dir: PathBuf,
    state: Mutex<State>,
}

static CLOUD: OnceLock<Cloud> = OnceLock::new();

/// Enable the cloud push. Returns false when it's off/misconfigured.
pub fn configure(cfg: CloudConfig, dir: PathBuf, icons_dir: PathBuf) -> bool {
    if !cfg.enable || cfg.url.trim().is_empty() || cfg.service_key.trim().is_empty() {
        return false;
    }
    CLOUD
        .set(Cloud {
            cfg,
            dir,
            icons_dir,
            state: Mutex::new(State::default()),
        })
        .is_ok()
}

pub fn enabled() -> bool {
    CLOUD.get().is_some()
}

/// Queue offer rows. Rows already queued this session are skipped.
pub fn enqueue_rows(rows: &[CloudRow]) -> Result<usize> {
    let Some(c) = CLOUD.get() else {
        return Ok(0);
    };
    let mut st = c.state.lock();
    let path = c.dir.join(&c.cfg.queue);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    let mut n = 0;
    for r in rows {
        let key = (r.name.clone(), r.seller.clone().unwrap_or_default(), r.price);
        if !st.seen.insert(key) {
            continue;
        }
        writeln!(f, "{}", serde_json::to_string(r)?)?;
        n += 1;
    }
    Ok(n)
}

/// Queue an icon file name (e.g. `Magic_fang.png`) for upload.
pub fn enqueue_icon(name: &str) -> Result<()> {
    let Some(c) = CLOUD.get() else {
        return Ok(());
    };
    if name.is_empty() {
        return Ok(());
    }
    let mut st = c.state.lock();
    if !st.queued_icons.insert(name.to_string()) {
        return Ok(());
    }
    let path = c.dir.join(&c.cfg.icon_queue);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{name}")?;
    Ok(())
}

/// Public URL of an uploaded icon (what the dashboard renders).
pub fn icon_public_url(name: &str) -> Option<String> {
    let c = CLOUD.get()?;
    Some(format!(
        "{}/storage/v1/object/public/{}/{}",
        c.cfg.url.trim_end_matches('/'),
        c.cfg.icons_bucket,
        name
    ))
}

/// Sent on every request. Supabase refuses `sb_secret_` keys from anything that
/// looks like a browser (its heuristic is the User-Agent), so identify honestly.
pub const USER_AGENT: &str = "elorin-market-scanner/0.1";

/// The project URL + write key, when the cloud push is configured. Lets other
/// modules (the buy controller) reach the same project without re-reading config.
pub fn endpoint() -> Option<(&'static str, &'static str)> {
    CLOUD
        .get()
        .map(|c| (c.cfg.url.as_str(), c.cfg.service_key.as_str()))
}

fn rows_url(cfg: &CloudConfig) -> String {
    format!("{}/rest/v1/{}", cfg.url.trim_end_matches('/'), cfg.table)
}

fn post_rows(cfg: &CloudConfig, body: &str) -> Result<()> {
    ureq::post(&rows_url(cfg))
        .set("apikey", &cfg.service_key)
        .set("Authorization", &format!("Bearer {}", cfg.service_key))
        .set("Content-Type", "application/json")
        .set("Prefer", "return=minimal")
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(15))
        .send_string(body)
        .map(|_| ())
        .map_err(|e| anyhow!("POST {}: {e}", rows_url(cfg)))
}

fn put_icon(cfg: &CloudConfig, name: &str, bytes: &[u8]) -> Result<()> {
    let url = format!(
        "{}/storage/v1/object/{}/{}",
        cfg.url.trim_end_matches('/'),
        cfg.icons_bucket,
        name
    );
    ureq::put(&url)
        .set("apikey", &cfg.service_key)
        .set("Authorization", &format!("Bearer {}", cfg.service_key))
        .set("Content-Type", "image/png")
        .set("x-upsert", "true")
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(15))
        .send_bytes(bytes)
        .map(|_| ())
        .map_err(|e| anyhow!("PUT {url}: {e}"))
}

/// Upload one pending icon per call (cheap when there is nothing to do).
fn flush_icons(c: &Cloud) -> Result<usize> {
    let path = c.dir.join(&c.cfg.icon_queue);
    if !path.exists() {
        return Ok(0);
    }
    // Take the work out of the queue without holding the lock over the network.
    let names: Vec<String> = {
        let _st = c.state.lock();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let names: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if names.is_empty() {
            return Ok(0);
        }
        std::fs::write(&path, "")?;
        names
    };

    let mut done = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for name in names {
        let file = c.icons_dir.join(&name);
        match std::fs::read(&file) {
            Ok(bytes) => match put_icon(&c.cfg, &name, &bytes) {
                Ok(()) => done += 1,
                Err(e) => {
                    warn!(error = %e, icon = %name, "icon upload failed");
                    failed.push(name);
                }
            },
            Err(_) => { /* icon vanished; drop it from the queue */ }
        }
    }
    if !failed.is_empty() {
        let _st = c.state.lock();
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            for name in failed {
                let _ = writeln!(f, "{name}");
            }
        }
    }
    Ok(done)
}

/// Flush pending icons and offer rows. Returns how many offer rows were pushed.
pub fn flush_once() -> Result<usize> {
    let Some(c) = CLOUD.get() else {
        return Ok(0);
    };
    let _ = flush_icons(c);

    let path = c.dir.join(&c.cfg.queue);
    if !path.exists() {
        return Ok(0);
    }

    // Drain the queue file first (never hold the lock across the network).
    let lines: Vec<String> = {
        let _st = c.state.lock();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let lines: Vec<String> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect();
        if lines.is_empty() {
            return Ok(0);
        }
        std::fs::write(&path, "")?;
        lines
    };

    let batch = c.cfg.batch_max.max(1);
    let mut pushed = 0usize;
    let mut consumed = 0usize;
    for chunk in lines.chunks(batch) {
        // Re-encode through `CloudRow` so every object in the request carries the
        // same keys — this also repairs rows queued by an older build.
        let mut objs = Vec::with_capacity(chunk.len());
        for line in chunk {
            match serde_json::from_str::<CloudRow>(line) {
                Ok(row) => objs.push(serde_json::to_string(&row).unwrap_or_default()),
                Err(e) => warn!(error = %e, row = %line, "dropping unreadable queued row"),
            }
        }
        if objs.is_empty() {
            consumed += chunk.len();
            continue;
        }
        let body = format!("[{}]", objs.join(","));
        match post_rows(&c.cfg, &body) {
            Ok(()) => {
                consumed += chunk.len();
                pushed += objs.len();
            }
            Err(e) => {
                warn!(error = %e, pushed, "cloud push failed; will retry");
                break;
            }
        }
    }

    // Put back whatever didn't make it (goes behind anything queued meanwhile).
    if consumed < lines.len() {
        let _st = c.state.lock();
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            for line in &lines[consumed..] {
                let _ = writeln!(f, "{line}");
            }
        }
    }
    Ok(pushed)
}

/// Background thread: flush every `flush_ms`, and once more on exit.
pub fn spawn_pusher() {
    let Some(c) = CLOUD.get() else {
        return;
    };
    let interval = Duration::from_millis(c.cfg.flush_ms.max(1000));
    std::thread::Builder::new()
        .name("cloud-pusher".into())
        .spawn(move || {
            info!(?interval, "cloud pusher started");
            while !crate::EXIT_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(interval);
                match flush_once() {
                    Ok(0) => {}
                    Ok(n) => info!(rows = n, "pushed offers to the cloud"),
                    Err(e) => warn!(error = %e, "cloud flush error"),
                }
            }
            let _ = flush_once();
            info!("cloud pusher stopped");
        })
        .expect("spawn cloud pusher");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_url_trims_trailing_slash() {
        let cfg = CloudConfig {
            url: "https://x.supabase.co/".to_string(),
            table: "offers".to_string(),
            ..Default::default()
        };
        assert_eq!(rows_url(&cfg), "https://x.supabase.co/rest/v1/offers");
    }

    /// Live push against a stand-in (or the real project). Skipped unless
    /// `ELORIN_CLOUD_TEST_URL` is set, e.g.
    ///   ELORIN_CLOUD_TEST_URL=http://127.0.0.1:8788 cargo test --lib pushes_queued
    ///   ELORIN_CLOUD_TEST_URL=https://xxxx.supabase.co \
    ///   ELORIN_CLOUD_TEST_KEY=sb_secret_…  cargo test --lib pushes_queued
    #[test]
    fn pushes_queued_rows_and_drains_the_queue() {
        let Ok(url) = std::env::var("ELORIN_CLOUD_TEST_URL") else {
            return;
        };
        let key = std::env::var("ELORIN_CLOUD_TEST_KEY").unwrap_or_else(|_| "test".to_string());
        let dir = std::env::temp_dir().join("elorin_cloud_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("icons")).unwrap();

        let cfg = CloudConfig {
            enable: true,
            url,
            service_key: key,
            queue: "q.jsonl".to_string(),
            icon_queue: "i.txt".to_string(),
            ..Default::default()
        };
        assert!(configure(cfg, dir.clone(), dir.join("icons")));

        let rows = vec![CloudRow {
            ts_ms: 1,
            name: "Ruby".to_string(),
            seller: Some("100".to_string()),
            price: 10,
            quantity: 1,
            icon: Some("Ruby.png".to_string()),
        }];
        assert_eq!(enqueue_rows(&rows).unwrap(), 1);
        assert_eq!(enqueue_rows(&rows).unwrap(), 0, "standing offer queued twice");
        assert_eq!(flush_once().unwrap(), 1);

        let left = std::fs::read_to_string(dir.join("q.jsonl")).unwrap_or_default();
        assert!(left.trim().is_empty(), "queue not drained: {left:?}");
    }

    #[test]
    fn bulk_rows_share_one_key_set() {
        // PostgREST rejects a bulk insert whose objects have different keys
        // (PGRST102), so a row with no seller/icon must still emit those keys.
        let sparse = CloudRow {
            ts_ms: 1,
            name: "Bonus xp".to_string(),
            seller: None,
            price: 100,
            quantity: 1,
            icon: None,
        };
        let full = CloudRow {
            ts_ms: 2,
            name: "Ruby".to_string(),
            seller: Some("100".to_string()),
            price: 200,
            quantity: 4,
            icon: Some("Ruby.png".to_string()),
        };
        let keys = |r: &CloudRow| {
            serde_json::to_value(r)
                .unwrap()
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&sparse), keys(&full));
        assert!(serde_json::to_string(&sparse).unwrap().contains("\"seller\":null"));
    }

    #[test]
    fn legacy_queued_rows_are_repaired_on_flush() {
        // A row written by an older build (no seller/icon keys) must still
        // round-trip into a uniform object.
        let legacy = r#"{"ts_ms":5,"name":"Yew logs","price":900,"quantity":3}"#;
        let row: CloudRow = serde_json::from_str(legacy).expect("legacy row parses");
        assert_eq!(row.name, "Yew logs");
        assert_eq!(row.seller, None);
        let json = serde_json::to_string(&row).unwrap();
        assert!(json.contains("\"seller\":null") && json.contains("\"icon\":null"));
    }
}

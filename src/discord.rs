//! Discord webhook notifications for purchases and stuck states.
//!
//! Notifications are sent from a detached thread so the buy loop never blocks on
//! the network. The stuck alert is the exception: it runs inline, because by the
//! time it fires the caller is about to shut the process down and a detached post
//! would die with it. Discord's webhook endpoint accepts a `payload_json` form field
//! plus file attachments; we build the multipart body manually so there are no
//! extra crate dependencies.
//!
//! Each purchase attempt posts **two** messages, both keyed by the run ID:
//! an *evidence* post (the readable timestamped timeline in the embed, the full
//! `<run_id>.log` file, and the annotated debug images) and the *recording* post
//! (the MP4). The run ID also names the footage directory and the
//! `purchases.jsonl` line, so any artifact can be tied back to the run.

use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Context, Result};
use serde::Serialize;
use tracing::{info, warn};

/// `[discord]` section of `market.toml`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct DiscordConfig {
    pub enable: bool,
    pub webhook_url: String,
    /// Avatar shown for the bot user.
    pub avatar_url: String,
    /// Post the per-run evidence bundle (timeline log + annotated images) after
    /// each purchase attempt.
    pub send_runs: bool,
    /// Cap on annotated images attached per run (Discord allows 10 files total).
    pub max_images: usize,
    /// A second webhook for a short, skimmable purchase summary (headline + run
    /// ID). Leave empty to disable. The detailed evidence stays on `webhook_url`;
    /// this one is just an index you scroll, then jump to by ID.
    pub summary_webhook_url: String,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            enable: false,
            webhook_url: String::new(),
            avatar_url: "https://cdn.discordapp.com/embed/avatars/0.png".to_string(),
            send_runs: true,
            max_images: 8,
            summary_webhook_url: String::new(),
        }
    }
}

impl DiscordConfig {
    pub fn is_configured(&self) -> bool {
        self.enable && !self.webhook_url.is_empty() && self.webhook_url.starts_with("http")
    }
}

#[derive(Serialize)]
struct Field {
    name: String,
    value: String,
    inline: bool,
}

#[derive(Serialize)]
struct EmbedImage {
    url: String,
}

#[derive(Serialize)]
struct Embed {
    title: String,
    color: u32,
    description: Option<String>,
    fields: Vec<Field>,
    image: Option<EmbedImage>,
    thumbnail: Option<EmbedImage>,
    footer: Option<Footer>,
}

#[derive(Serialize)]
struct Footer {
    text: String,
}

#[derive(Serialize)]
struct Payload {
    username: String,
    avatar_url: String,
    embeds: Vec<Embed>,
}

/// One file part of a multipart webhook body.
struct Attachment {
    filename: String,
    content_type: String,
    bytes: Vec<u8>,
}

/// Serialize `embeds` and POST them to the main webhook.
fn send(cfg: &DiscordConfig, embeds: Vec<Embed>, files: Vec<Attachment>, timeout_secs: u64) -> Result<()> {
    send_to(&cfg.webhook_url, &cfg.avatar_url, embeds, files, timeout_secs)
}

/// Serialize `embeds` and POST them to `url`, attaching `files` (empty ⇒ JSON body).
fn send_to(url: &str, avatar_url: &str, embeds: Vec<Embed>, files: Vec<Attachment>, timeout_secs: u64) -> Result<()> {
    let payload = Payload {
        username: "Elorin Bot".to_string(),
        avatar_url: avatar_url.to_string(),
        embeds,
    };
    let payload_json = serde_json::to_string(&payload)?;
    let (content_type, body) = multipart(&payload_json, &files);

    let resp = ureq::post(url)
        .set("Content-Type", &content_type)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .send_bytes(&body);

    match resp {
        Ok(r) => {
            info!(status = r.status(), "discord message sent");
            Ok(())
        }
        Err(ureq::Error::Status(code, _)) => {
            Err(anyhow::anyhow!("discord webhook returned status {code}"))
        }
        Err(e) => Err(anyhow::anyhow!("discord webhook request failed: {e}")),
    }
}

/// Fire a Discord notification for a successful purchase, with the item icon as
/// an attachment. Runs in its own thread.
pub fn notify_purchase(
    cfg: DiscordConfig,
    name: String,
    seller: String,
    price: i64,
    units: i64,
    profit: i64,
    pct: f64,
    snipe: bool,
    icon_png: Option<Vec<u8>>,
) {
    if !cfg.is_configured() {
        return;
    }
    thread::spawn(move || {
        if let Err(e) = send_purchase(&cfg, &name, &seller, price, units, profit, pct, snipe, icon_png) {
            warn!(error = %e, "discord purchase notification failed");
        }
    });
}

fn send_purchase(
    cfg: &DiscordConfig,
    name: &str,
    seller: &str,
    price: i64,
    units: i64,
    profit: i64,
    pct: f64,
    snipe: bool,
    icon_png: Option<Vec<u8>>,
) -> Result<()> {
    let clean_name = sanitize_filename(name);
    let filename = format!("{clean_name}.png");

    let title = if snipe {
        format!("BIG SNIPE: {name} bought")
    } else {
        format!("Bought: {name}")
    };
    let color = if snipe { 0xFFD700 } else { 0x2ECC71 };

    let mut fields = vec![
        Field {
            name: "Price".to_string(),
            value: format!("{} gp", comma_num(price)),
            inline: true,
        },
        Field {
            name: "Units".to_string(),
            value: units.to_string(),
            inline: true,
        },
        Field {
            name: "Seller".to_string(),
            value: if seller.is_empty() { "Unknown".to_string() } else { seller.to_string() },
            inline: true,
        },
        Field {
            name: "Potential margin".to_string(),
            value: format!("{} gp ({pct:+.0}%)", comma_num(profit)),
            inline: false,
        },
    ];
    if snipe {
        fields.push(Field {
            name: "Snipe".to_string(),
            value: "This crossed the big-snipe threshold".to_string(),
            inline: false,
        });
    }

    let embed = Embed {
        title,
        color,
        description: None,
        fields,
        image: icon_png.as_ref().map(|_| EmbedImage {
            url: format!("attachment://{filename}"),
        }),
        thumbnail: None,
        footer: Some(Footer {
            text: "Elorin Market Scanner".to_string(),
        }),
    };
    let files = match icon_png {
        Some(png) => vec![Attachment {
            filename,
            content_type: "image/png".to_string(),
            bytes: png,
        }],
        None => Vec::new(),
    };
    send(cfg, vec![embed], files, 20)
}

/// Summary of one purchase attempt, for the evidence and summary embeds.
#[derive(Clone)]
pub struct RunSummary {
    pub id: String,
    pub name: String,
    /// Headline, e.g. `Bought 3 × Voting token`.
    pub title: String,
    /// One-line outcome (`Outcome::describe()`).
    pub outcome: String,
    pub color: u32,
    pub price: i64,
    pub units: i64,
    pub max_price: i64,
    pub elapsed_ms: u128,
    /// A big-snipe purchase (crossed the manager's snipe margin).
    pub snipe: bool,
}

/// Post a run's evidence: an embed with the readable timeline, the full `.log`
/// file, and the annotated debug images. The recording, when present, follows as
/// its own message. Both are tagged with the run ID. Runs in its own thread.
pub fn notify_run(
    cfg: DiscordConfig,
    summary: RunSummary,
    log_text: String,
    timeline: String,
    images: Vec<PathBuf>,
    video_path: Option<PathBuf>,
) {
    if !cfg.is_configured() {
        return;
    }
    thread::spawn(move || {
        if let Err(e) = send_evidence(&cfg, &summary, &log_text, &timeline, &images) {
            warn!(error = %e, "discord evidence post failed");
        }
        if let Some(video) = &video_path {
            if let Err(e) = send_recording(&cfg, &summary.name, video, &summary.id) {
                warn!(error = %e, "discord recording post failed");
            }
        }
    });
}

/// Post a short, minimal purchase update to the **summary** webhook — a channel
/// meant to be skimmed, not read. One embed: the headline, the price, the run
/// ID (to jump to the detailed post), the item icon, and a snipes called out so
/// they stand apart from ordinary buys. The full evidence bundle still goes to
/// the main webhook.
pub fn notify_summary(cfg: DiscordConfig, summary: RunSummary, icon_png: Option<Vec<u8>>) {
    let url = cfg.summary_webhook_url.trim().to_string();
    if !cfg.enable || !url.starts_with("http") {
        return;
    }
    thread::spawn(move || {
        if let Err(e) = send_summary(&cfg.avatar_url, &url, &summary, icon_png) {
            warn!(error = %e, "discord summary notification failed");
        }
    });
}

fn send_summary(
    avatar_url: &str,
    url: &str,
    s: &RunSummary,
    icon_png: Option<Vec<u8>>,
) -> Result<()> {
    let filename = format!("{}.png", sanitize_filename(&s.name));
    let headline = if s.snipe {
        format!("BIG SNIPE · {}", s.title)
    } else {
        s.title.clone()
    };
    let embed = Embed {
        title: headline,
        color: s.color,
        description: Some(format!("{} gp · run `{}`", comma_num(s.price), s.id)),
        fields: Vec::new(),
        image: None,
        thumbnail: icon_png.as_ref().map(|_| EmbedImage {
            url: format!("attachment://{filename}"),
        }),
        footer: None,
    };
    let files = match icon_png {
        Some(png) => vec![Attachment {
            filename,
            content_type: "image/png".to_string(),
            bytes: png,
        }],
        None => Vec::new(),
    };
    send_to(url, avatar_url, vec![embed], files, 20)
}

fn send_evidence(
    cfg: &DiscordConfig,
    s: &RunSummary,
    log_text: &str,
    timeline: &str,
    images: &[PathBuf],
) -> Result<()> {
    let max = cfg.max_images.clamp(1, 10);
    let mut files = vec![Attachment {
        filename: format!("{}.log", s.id),
        content_type: "text/plain; charset=utf-8".to_string(),
        bytes: log_text.as_bytes().to_vec(),
    }];
    let mut names: Vec<String> = Vec::new();
    for p in images.iter().take(max) {
        let Some(filename) = p.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        names.push(filename.to_string());
        files.push(Attachment {
            filename: filename.to_string(),
            content_type: "image/png".to_string(),
            bytes,
        });
    }
    // Prefer the shop frame (where the item was located) for the embed thumbnail.
    let thumb = names
        .iter()
        .find(|n| n.contains("02_shop"))
        .or_else(|| names.first())
        .cloned();

    let embed = Embed {
        title: format!("Run {} · {}", s.id, s.title),
        color: s.color,
        description: Some(format!("```\n{}\n```", timeline.trim_end())),
        fields: vec![
            Field {
                name: "Outcome".to_string(),
                value: s.outcome.clone(),
                inline: false,
            },
            Field {
                name: "Item".to_string(),
                value: s.name.clone(),
                inline: true,
            },
            Field {
                name: "Price".to_string(),
                value: format!("{} gp", comma_num(s.price)),
                inline: true,
            },
            Field {
                name: "Units".to_string(),
                value: s.units.to_string(),
                inline: true,
            },
            Field {
                name: "Max price".to_string(),
                value: format!("{} gp", comma_num(s.max_price)),
                inline: true,
            },
            Field {
                name: "Elapsed".to_string(),
                value: format!("{} ms", s.elapsed_ms),
                inline: true,
            },
            Field {
                name: "Run ID".to_string(),
                value: format!("`{}`", s.id),
                inline: true,
            },
        ],
        image: thumb.map(|f| EmbedImage {
            url: format!("attachment://{f}"),
        }),
        thumbnail: None,
        footer: Some(Footer {
            text: format!("Elorin · run {}", s.id),
        }),
    };
    send(cfg, vec![embed], files, 60)
}

fn send_recording(cfg: &DiscordConfig, name: &str, path: &Path, id: &str) -> Result<()> {
    let embed = Embed {
        title: format!("Recording: {name} · run {id}"),
        color: 0x3498DB,
        description: None,
        fields: vec![Field {
            name: "Purchase attempt".to_string(),
            value: "From just before the shop opened until the purchase landed".to_string(),
            inline: false,
        }],
        image: None,
        thumbnail: None,
        footer: Some(Footer {
            text: format!("Elorin · run {id}"),
        }),
    };
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("recording.mp4")
        .to_string();
    let mp4 = std::fs::read(path).with_context(|| format!("reading {path:?}"))?;
    send(
        cfg,
        vec![embed],
        vec![Attachment {
            filename,
            content_type: "video/mp4".to_string(),
            bytes: mp4,
        }],
        120,
    )
}

/// Announce on the purchases webhook that the loop is taking a break.
///
/// A break is deliberate downtime, so it belongs in the same channel as the
/// purchases: an unexplained gap between buys is otherwise indistinguishable
/// from a scanner that has quietly died. Fire-and-forget — the caller is about
/// to sleep anyway.
pub fn notify_break(cfg: DiscordConfig, label: String, minutes: u64) {
    if !cfg.is_configured() {
        return;
    }
    thread::spawn(move || {
        if let Err(e) = send_break(&cfg, &label, minutes) {
            warn!(error = %e, "discord break notification failed");
        }
    });
}

fn send_break(cfg: &DiscordConfig, label: &str, minutes: u64) -> Result<()> {
    let embed = Embed {
        title: format!("On a {label} break · {minutes} min"),
        color: 0x95A5A6,
        description: Some(format!(
            "Parked on the recent offers list — no refreshes and no buys — resuming in about {minutes} min."
        )),
        fields: Vec::new(),
        image: None,
        thumbnail: None,
        footer: None,
    };
    send(cfg, vec![embed], Vec::new(), 20)
}

/// Notify that the scanner got stuck, optionally attaching the recorded MP4.
///
/// Runs on the **calling** thread and only returns once the post has been
/// attempted. The caller shuts the process down right after this, so a detached
/// post (which is right for the per-purchase messages) would be killed mid-flight
/// along with the recording.
pub fn notify_stuck(cfg: DiscordConfig, video_path: Option<PathBuf>) {
    if !cfg.is_configured() {
        return;
    }
    if let Err(e) = send_stuck(&cfg, video_path.as_deref()) {
        warn!(error = %e, "discord stuck notification failed");
    }
}

fn send_stuck(cfg: &DiscordConfig, video_path: Option<&Path>) -> Result<()> {
    // The recording's directory/file stem is the run ID for the stuck episode.
    let run_id = video_path
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("(none)")
        .to_string();

    let embed = Embed {
        title: format!("Scanner is stuck · run {run_id}"),
        color: 0xE74C3C,
        description: None,
        fields: vec![
            Field {
                name: "Status".to_string(),
                value: "The loop could not find the Refresh button for several seconds".to_string(),
                inline: false,
            },
            Field {
                name: "Recording".to_string(),
                value: if video_path.is_some() {
                    "MP4 attached".to_string()
                } else {
                    "No video (ffmpeg missing or only one frame)".to_string()
                },
                inline: false,
            },
        ],
        image: None,
        thumbnail: None,
        footer: Some(Footer {
            text: format!("Elorin · run {run_id}"),
        }),
    };

    let files = match video_path {
        Some(path) => {
            let filename = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("stuck.mp4")
                .to_string();
            let mp4 = std::fs::read(path).with_context(|| format!("reading {path:?}"))?;
            vec![Attachment {
                filename,
                content_type: "video/mp4".to_string(),
                bytes: mp4,
            }]
        }
        None => Vec::new(),
    };
    send(cfg, vec![embed], files, 60)
}

/// Build a multipart body: `payload_json` plus one `files[i]` part per attachment.
fn multipart(payload_json: &str, files: &[Attachment]) -> (String, Vec<u8>) {
    if files.is_empty() {
        return ("application/json".to_string(), payload_json.as_bytes().to_vec());
    }
    let boundary = generate_boundary();
    let b = boundary.as_str();

    let mut body = Vec::new();
    write_part_header(&mut body, b, "payload_json", None, Some("application/json"));
    body.extend_from_slice(payload_json.as_bytes());
    body.extend_from_slice(b"\r\n");

    for (i, f) in files.iter().enumerate() {
        let name = format!("files[{i}]");
        write_part_header(&mut body, b, &name, Some(&f.filename), Some(&f.content_type));
        body.extend_from_slice(&f.bytes);
        body.extend_from_slice(b"\r\n");
    }
    write_boundary_end(&mut body, b);

    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn generate_boundary() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("----elorin-{t:x}")
}

fn write_part_header(body: &mut Vec<u8>, boundary: &str, name: &str, filename: Option<&str>, content_type: Option<&str>) {
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"");
    body.extend_from_slice(name.as_bytes());
    if let Some(f) = filename {
        body.extend_from_slice(b"\"; filename=\"");
        body.extend_from_slice(f.as_bytes());
    }
    body.extend_from_slice(b"\"\r\n");
    if let Some(ct) = content_type {
        body.extend_from_slice(b"Content-Type: ");
        body.extend_from_slice(ct.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"\r\n");
}

fn write_boundary_end(body: &mut Vec<u8>, boundary: &str) {
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' { c } else { '_' })
        .collect::<String>()
        .replace(' ', "_")
}

/// Encode a raw RGBA icon crop into PNG bytes. Returns `None` when the crop is
/// empty or encoding fails.
pub fn encode_icon_png(w: i32, h: i32, rgba: &[u8]) -> Option<Vec<u8>> {
    if w <= 0 || h <= 0 || rgba.len() < (w * h * 4) as usize {
        return None;
    }
    let mut img = image::RgbaImage::new(w as u32, h as u32);
    for (i, px) in img.pixels_mut().enumerate() {
        let off = i * 4;
        *px = image::Rgba([rgba[off], rgba[off + 1], rgba[off + 2], rgba[off + 3]]);
    }
    let mut bytes = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).ok()?;
    Some(bytes)
}

fn comma_num(n: i64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_without_files_is_plain_json() {
        let (ct, body) = multipart("{\"a\":1}", &[]);
        assert_eq!(ct, "application/json");
        assert_eq!(body, b"{\"a\":1}");
    }

    #[test]
    fn multipart_carries_every_file_part() {
        let files = vec![
            Attachment {
                filename: "42.log".to_string(),
                content_type: "text/plain".to_string(),
                bytes: b"hello".to_vec(),
            },
            Attachment {
                filename: "02_shop.png".to_string(),
                content_type: "image/png".to_string(),
                bytes: vec![0x89, 0x50],
            },
        ];
        let (ct, body) = multipart("{}", &files);
        assert!(ct.starts_with("multipart/form-data; boundary="));
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"payload_json\""), "{text}");
        assert!(text.contains("name=\"files[0]\"; filename=\"42.log\""), "{text}");
        assert!(text.contains("name=\"files[1]\"; filename=\"02_shop.png\""), "{text}");
        assert!(text.contains("Content-Type: image/png"), "{text}");
    }
}

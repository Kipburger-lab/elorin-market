//! Discord webhook notifications for purchases and stuck states.
//!
//! Notifications are sent from a detached thread so the buy loop never blocks on
//! the network. Discord's webhook endpoint accepts a `payload_json` form field
//! plus file attachments; we build the multipart body manually so there are no
//! extra crate dependencies.

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
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            enable: false,
            webhook_url: String::new(),
            avatar_url: "https://cdn.discordapp.com/embed/avatars/0.png".to_string(),
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
    fields: Vec<Field>,
    image: Option<EmbedImage>,
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
        fields,
        image: icon_png.as_ref().map(|_| EmbedImage {
            url: format!("attachment://{filename}"),
        }),
        footer: Some(Footer {
            text: "Elorin Market Scanner".to_string(),
        }),
    };
    let payload = Payload {
        username: "Elorin Bot".to_string(),
        avatar_url: cfg.avatar_url.clone(),
        embeds: vec![embed],
    };

    let payload_json = serde_json::to_string(&payload)?;

    let (content_type, body) = if let Some(png) = icon_png {
        let mut body = Vec::new();
        let boundary = generate_boundary();
        let b = boundary.as_str();

        write_part_header(&mut body, b, "payload_json", None, Some("application/json"));
        body.extend_from_slice(payload_json.as_bytes());
        body.extend_from_slice(b"\r\n");

        write_part_header(&mut body, b, "files[0]", Some(&filename), Some("image/png"));
        body.extend_from_slice(&png);
        body.extend_from_slice(b"\r\n");
        write_boundary_end(&mut body, b);

        (format!("multipart/form-data; boundary={boundary}"), body)
    } else {
        ("application/json".to_string(), payload_json.into_bytes())
    };

    let resp = ureq::post(&cfg.webhook_url)
        .set("Content-Type", &content_type)
        .timeout(std::time::Duration::from_secs(20))
        .send_bytes(&body);

    match resp {
        Ok(r) => {
            info!(status = r.status(), "discord purchase notification sent");
            Ok(())
        }
        Err(ureq::Error::Status(code, _)) => {
            Err(anyhow::anyhow!("discord webhook returned status {code}"))
        }
        Err(e) => Err(anyhow::anyhow!("discord webhook request failed: {e}")),
    }
}

/// Notify that the scanner got stuck, optionally attaching the recorded MP4.
pub fn notify_stuck(cfg: DiscordConfig, video_path: Option<PathBuf>) {
    if !cfg.is_configured() {
        return;
    }
    thread::spawn(move || {
        if let Err(e) = send_stuck(&cfg, video_path.as_deref()) {
            warn!(error = %e, "discord stuck notification failed");
        }
    });
}

fn send_stuck(cfg: &DiscordConfig, video_path: Option<&Path>) -> Result<()> {
    let embed = Embed {
        title: "Scanner is stuck".to_string(),
        color: 0xE74C3C,
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
        footer: Some(Footer {
            text: "Elorin Market Scanner".to_string(),
        }),
    };
    let payload = Payload {
        username: "Elorin Bot".to_string(),
        avatar_url: cfg.avatar_url.clone(),
        embeds: vec![embed],
    };
    let payload_json = serde_json::to_string(&payload)?;

    let (content_type, body): (String, Vec<u8>) = if let Some(path) = video_path {
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("stuck.mp4")
            .to_string();
        let mp4 = std::fs::read(path).with_context(|| format!("reading {path:?}"))?;

        let mut body = Vec::new();
        let boundary = generate_boundary();
        let b = boundary.as_str();

        write_part_header(&mut body, b, "payload_json", None, Some("application/json"));
        body.extend_from_slice(payload_json.as_bytes());
        body.extend_from_slice(b"\r\n");

        write_part_header(&mut body, b, "files[0]", Some(&filename), Some("video/mp4"));
        body.extend_from_slice(&mp4);
        body.extend_from_slice(b"\r\n");
        write_boundary_end(&mut body, b);

        (format!("multipart/form-data; boundary={boundary}"), body)
    } else {
        ("application/json".to_string(), payload_json.into_bytes())
    };

    let resp = ureq::post(&cfg.webhook_url)
        .set("Content-Type", &content_type)
        .timeout(std::time::Duration::from_secs(60))
        .send_bytes(&body);

    match resp {
        Ok(r) => {
            info!(status = r.status(), "discord stuck notification sent");
            Ok(())
        }
        Err(ureq::Error::Status(code, _)) => {
            Err(anyhow::anyhow!("discord webhook returned status {code}"))
        }
        Err(e) => Err(anyhow::anyhow!("discord webhook request failed: {e}")),
    }
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

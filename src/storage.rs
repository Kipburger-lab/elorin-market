//! Cap-based storage cleanup.
//!
//! Recordings and logs accumulate on every run, so at startup the scanner prunes
//! the oldest files until each group is back under its cap.
//!
//! - **Footage** — every `.mp4` under `data/` (the buy and stuck recordings).
//! - **Logs** — everything under `logs/`, the per-run `.log` timelines under
//!   `data/`, and the per-run debug frames (`*.png` under `data/buy` and
//!   `data/stuck`). Those frames are diagnostic evidence, not footage and not the
//!   dashboard's `data/icons`, so they share the log budget rather than growing
//!   without bound.
//!
//! Deleting oldest-first keeps the most recent runs (the ones worth reviewing),
//! and the run ID in each file keeps whatever is pruned identifiable in the
//! `purchases.jsonl` history.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const MB: u64 = 1024 * 1024;

/// `[storage]` section of `market.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub enable: bool,
    /// Footage cap, in MB, across `data/**/*.mp4` (oldest deleted first).
    pub footage_cap_mb: u64,
    /// Log cap, in MB, across `logs/**`, the per-run `.log` timelines, and the
    /// per-run debug frames under `data/buy` and `data/stuck`.
    pub log_cap_mb: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            enable: true,
            footage_cap_mb: 5120, // 5 GB
            log_cap_mb: 2048,     // 2 GB
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct CleanupReport {
    pub removed_files: usize,
    pub removed_bytes: u64,
    pub footage_bytes: u64,
    pub log_bytes: u64,
}

/// Prune old footage and logs once, at startup. No-op when disabled.
pub fn enforce_once(dir: &Path, cfg: &StorageConfig) -> Result<CleanupReport> {
    if !cfg.enable {
        return Ok(CleanupReport::default());
    }
    enforce(
        dir,
        cfg.footage_cap_mb.saturating_mul(MB),
        cfg.log_cap_mb.saturating_mul(MB),
    )
}

/// Enforce the caps on a background thread so startup never waits on the sweep.
pub fn spawn_cleaner(dir: PathBuf, cfg: StorageConfig) {
    if !cfg.enable {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("storage-clean".into())
        .spawn(move || match enforce_once(&dir, &cfg) {
            Ok(r) if r.removed_files > 0 => {
                info!(
                    removed = r.removed_files,
                    freed_mb = r.removed_bytes / MB,
                    footage_mb = r.footage_bytes / MB,
                    log_mb = r.log_bytes / MB,
                    "storage cleanup pruned old files"
                );
                println!(
                    "  storage: pruned {} file(s), freed {} MB (footage {} MB, logs {} MB)",
                    r.removed_files,
                    r.removed_bytes / MB,
                    r.footage_bytes / MB,
                    r.log_bytes / MB
                );
            }
            Ok(_) => info!("storage cleanup: nothing to prune"),
            Err(e) => warn!(error = %e, "storage cleanup failed"),
        });
}

struct FileInfo {
    path: PathBuf,
    size: u64,
    mtime: SystemTime,
}

struct Pruned {
    removed: usize,
    freed: u64,
    remaining: u64,
}

fn enforce(dir: &Path, footage_cap: u64, log_cap: u64) -> Result<CleanupReport> {
    let mut footage = Vec::new();
    collect_by_ext(&dir.join("data"), "mp4", &mut footage);

    let mut logs = Vec::new();
    collect_all(&dir.join("logs"), &mut logs);
    collect_by_ext(&dir.join("data"), "log", &mut logs);
    // Per-run debug frames share the log budget. Scoped to the run dirs so the
    // dashboard's `data/icons` library is never swept.
    collect_by_ext(&dir.join("data").join("buy"), "png", &mut logs);
    collect_by_ext(&dir.join("data").join("stuck"), "png", &mut logs);

    let a = prune(&footage, footage_cap);
    let b = prune(&logs, log_cap);
    Ok(CleanupReport {
        removed_files: a.removed + b.removed,
        removed_bytes: a.freed + b.freed,
        footage_bytes: a.remaining,
        log_bytes: b.remaining,
    })
}

/// Delete the oldest files until the group is at or under `cap`.
fn prune(files: &[FileInfo], cap: u64) -> Pruned {
    let mut remaining: u64 = files.iter().map(|f| f.size).sum();
    if remaining <= cap {
        return Pruned { removed: 0, freed: 0, remaining };
    }
    let mut order: Vec<&FileInfo> = files.iter().collect();
    order.sort_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.path.cmp(&b.path)));

    let (mut removed, mut freed) = (0usize, 0u64);
    for f in order {
        if remaining <= cap {
            break;
        }
        match std::fs::remove_file(&f.path) {
            Ok(()) => {
                remaining -= f.size;
                removed += 1;
                freed += f.size;
                info!(path = %f.path.display(), bytes = f.size, "pruned old file");
            }
            Err(e) => warn!(path = %f.path.display(), error = %e, "could not prune file"),
        }
    }
    Pruned { removed, freed, remaining }
}

fn collect_by_ext(dir: &Path, ext: &str, out: &mut Vec<FileInfo>) {
    walk(dir, &mut |p| {
        if p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext)) {
            if let Some(fi) = info(p) {
                out.push(fi);
            }
        }
    });
}

fn collect_all(dir: &Path, out: &mut Vec<FileInfo>) {
    walk(dir, &mut |p| {
        if let Some(fi) = info(p) {
            out.push(fi);
        }
    });
}

fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, f);
        } else {
            f(&p);
        }
    }
}

fn info(p: &Path) -> Option<FileInfo> {
    let md = std::fs::metadata(p).ok()?;
    if !md.is_file() {
        return None;
    }
    Some(FileInfo {
        path: p.to_path_buf(),
        size: md.len(),
        mtime: md.modified().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "elorin_storage_{tag}_{}",
            crate::market::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn prunes_oldest_first_until_under_the_cap() {
        let dir = tmp_dir("prune");
        let footage = dir.join("data").join("buy").join("a");
        std::fs::create_dir_all(&footage).unwrap();
        for name in ["old.mp4", "mid.mp4", "new.mp4"] {
            std::fs::write(footage.join(name), vec![0u8; 1000]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("market_scanner.log.2026-01-01"), vec![0u8; 500]).unwrap();

        // 3000 bytes of footage against a 1500-byte cap → drop the two oldest.
        let r = enforce(&dir, 1500, 5000).unwrap();
        assert_eq!(r.removed_files, 2);
        assert!(!footage.join("old.mp4").exists());
        assert!(!footage.join("mid.mp4").exists());
        assert!(footage.join("new.mp4").exists());
        assert!(r.footage_bytes <= 1500, "over cap: {}", r.footage_bytes);
        // The 500-byte log is under its cap and stays.
        assert!(logs.join("market_scanner.log.2026-01-01").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_frames_count_toward_the_log_cap() {
        let dir = tmp_dir("frames");
        let shots = dir.join("data").join("buy").join("run1");
        std::fs::create_dir_all(&shots).unwrap();
        for name in ["a.png", "b.png", "c.png"] {
            std::fs::write(shots.join(name), vec![0u8; 1000]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // No footage; three 1000-byte frames against a 1500-byte log cap → two go.
        let r = enforce(&dir, 1, 1500).unwrap();
        assert_eq!(r.removed_files, 2);
        assert!(!shots.join("a.png").exists());
        assert!(shots.join("c.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_removes_nothing() {
        let dir = tmp_dir("disabled");
        let f = dir.join("data").join("x.mp4");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, vec![0u8; 100]).unwrap();

        let cfg = StorageConfig {
            enable: false,
            footage_cap_mb: 0,
            log_cap_mb: 0,
        };
        let r = enforce_once(&dir, &cfg).unwrap();
        assert_eq!(r.removed_files, 0);
        assert!(f.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

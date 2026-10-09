//! Screen recording: capture a run to PNGs and fold them into one MP4.
//!
//! A still shows where a purchase ended up; only a recording shows *how* it got
//! there, which is what a wrong buy or a mishandled click actually needs. The
//! frames are written by a background thread so the caller's timed clicks are
//! never delayed, then encoded with ffmpeg — optional, the PNGs are kept when it
//! is missing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};
use windows::Win32::Foundation::HWND;

use crate::capture::gdi::GdiCapturer;

/// Fold `<dir>/<tag>_%04d.png` into `<dir>/<tag>.mp4` at `fps`.
///
/// yuv420p needs even dimensions and the client is 811x571 — both odd — so pad
/// by a pixel; without it the encode fails outright, every time.
pub fn encode_with_ffmpeg(dir: &Path, tag: &str, fps: u64) -> bool {
    let pattern = dir.join(format!("{tag}_%04d.png"));
    let out = dir.join(format!("{tag}.mp4"));
    let encoded = std::process::Command::new("ffmpeg")
        .arg("-y")
        .args(["-framerate", &fps.max(1).to_string(), "-start_number", "0", "-i"])
        .arg(&pattern)
        .args([
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2",
            "-crf",
            "28",
            "-loglevel",
            "error",
        ])
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !encoded {
        warn!(dir = %dir.display(), "ffmpeg could not encode the recording; the frames are kept as PNGs");
        return false;
    }

    // The video supersedes the frames; keeping both doubles the space for no
    // extra information. The dir is per-recording, so this only removes ours.
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "png") {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    info!(video = %out.display(), "recording encoded");
    true
}

/// Captures the client to `<dir>/<tag>_NNNN.png` on a background thread until
/// [`Recorder::stop`] is called.
pub struct Recorder {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    dir: PathBuf,
    tag: String,
    fps: u64,
}

impl Recorder {
    /// Start capturing `hwnd` at `fps` into `dir`. Every frame is written (not
    /// just the changed ones) so the MP4's timing matches what really happened.
    pub fn start(hwnd: HWND, fps: u64, dir: PathBuf, tag: String) -> Self {
        let fps = fps.clamp(1, 30);
        // HWND is not Send, so carry the raw handle and rebuild it on the thread.
        let hwnd_raw = hwnd.0 as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread_dir = dir.clone();
        let thread_tag = tag.clone();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(error = %e, "could not create the recording dir");
        }
        let interval = Duration::from_millis((1000 / fps).max(1));
        let handle = std::thread::Builder::new()
            .name("buy-record".into())
            .spawn(move || {
                let cap = GdiCapturer::new(HWND(hwnd_raw as *mut core::ffi::c_void));
                let mut n: u32 = 0;
                while !flag.load(Ordering::Relaxed) {
                    if let Ok(f) = cap.capture_full_client() {
                        let path = thread_dir.join(format!("{thread_tag}_{n:04}.png"));
                        if crate::market::save_frame_png(&path, &f).is_err() {
                            break;
                        }
                        n += 1;
                    }
                    std::thread::sleep(interval);
                }
            })
            .ok();
        Self { stop, handle, dir, tag, fps }
    }

    /// Stop capturing, encode the frames, and return the MP4 path.
    pub fn stop(mut self) -> Option<PathBuf> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if encode_with_ffmpeg(&self.dir, &self.tag, self.fps) {
            Some(self.dir.join(format!("{}.mp4", self.tag)))
        } else {
            None
        }
    }
}

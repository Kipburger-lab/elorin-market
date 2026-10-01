//! Elorin Bot — Inventory Slot Overlay (calibration tool).
//!
//! Draws a magenta rectangle over each of the 28 inventory slots, but ONLY while
//! the Elorin client window is focused. Nudge the grid live until it is
//! pixel-perfect, then press S to save the resolved coordinates.

mod config;
mod grid;
mod state;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use tracing::{info, level_filters::LevelFilter};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use elorin_bot::capture::Rect;
use elorin_bot::hotkeys::{self, Debounce};
use elorin_bot::overlay::{self, Painter, MAGENTA, WHITE};
use elorin_bot::request_exit;

use config::OverlayConfig;
use grid::GridParams;

const OUTLINE: i32 = 2;
const PARAM_NAMES: [&str; 6] = ["base_x", "base_y", "slot_w", "slot_h", "step_x", "step_y"];

// Virtual-key codes owned by this tool.
const VK_OEM_4: u32 = 0xDB; // [
const VK_OEM_6: u32 = 0xDD; // ]
const VK_S: u32 = 0x53;
const VK_R: u32 = 0x52;
const VK_H: u32 = 0x48;
const VK_F10: u32 = 0x79;

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
    println!("Elorin Bot — inventory slot overlay stopped.");
    println!("Coordinates written to inventory_slots.json (next to the exe).");
    println!("Press Enter to close this window...");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

fn run() -> Result<()> {
    info!("Elorin Bot inventory overlay starting");

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from));
    let cwd = std::env::current_dir().ok();
    let (cfg_path, out_dir) = resolve_config(exe_dir, cwd);

    let cfg = match cfg_path.as_deref() {
        Some(p) => OverlayConfig::load_from(p).context("failed to load overlay.toml")?,
        None => OverlayConfig::default_config(),
    };
    info!(?cfg, "config loaded");

    *state::PARAMS.lock() = cfg.grid;
    state::HUD_ON.store(cfg.overlay.show_hud, Ordering::SeqCst);
    state::ACTIVE_PARAM.store(0, Ordering::SeqCst);
    let _ = state::NEEDLE.set(cfg.window.title_contains.clone());
    if let Some(p) = cfg_path {
        let _ = state::CONFIG_PATH.set(p);
    }
    let _ = state::OUT_DIR.set(out_dir);

    let paint: Arc<overlay::PaintFn> = Arc::new(|p: &Painter| paint(p));
    overlay::spawn(
        "ElorinBotInventoryOverlayClass",
        "Elorin Bot Inventory Overlay",
        cfg.window.title_contains.clone(),
        paint,
    );

    println!("Elorin Bot inventory overlay running.");
    println!("Focus the Elorin client to show the slot grid; F10 quits.");
    println!("[ / ] nudge   1-6 select param   S save   R reload   H hud");

    let owned = vec![
        0x31, 0x32, 0x33, 0x34, 0x35, 0x36, // 1..6
        VK_OEM_4, VK_OEM_6, VK_S, VK_R, VK_H, VK_F10,
    ];
    hotkeys::run(cfg.window.title_contains.clone(), owned, |vk| handle_key(vk))
}

fn paint(p: &Painter) {
    let params = *state::PARAMS.lock();
    for slot in grid::slots(&params) {
        p.stroke(
            Rect {
                x1: slot.x1,
                y1: slot.y1,
                x2: slot.x2,
                y2: slot.y2,
            },
            MAGENTA,
            OUTLINE,
        );
    }
    if state::HUD_ON.load(Ordering::Relaxed) {
        let active = state::ACTIVE_PARAM.load(Ordering::Relaxed);
        for (i, line) in hud_lines(&params, active).iter().enumerate() {
            p.text(8, 8 + (i as i32) * 16, line, WHITE);
        }
    }
}

fn hud_lines(p: &GridParams, active: usize) -> Vec<String> {
    let vals = [p.base_x, p.base_y, p.slot_w, p.slot_h, p.step_x, p.step_y];
    let mut out = Vec::with_capacity(PARAM_NAMES.len() + 2);
    for (i, name) in PARAM_NAMES.iter().enumerate() {
        let marker = if i == active { "  <" } else { "" };
        out.push(format!("{} {} : {}{}", i + 1, name, vals[i], marker));
    }
    out.push(String::from("( / ) nudge   Shift = x5   keys 1-6 select param"));
    out.push(String::from("S save   R reload   H hud   F10 quit"));
    out
}

fn handle_key(vk: u32) {
    match vk {
        0x31..=0x36 => {
            state::ACTIVE_PARAM.store((vk - 0x31) as usize, Ordering::SeqCst);
        }
        VK_OEM_4 => nudge(-1),
        VK_OEM_6 => nudge(1),
        VK_S => {
            if debounce().ready(vk, 400) {
                std::thread::spawn(|| match do_save() {
                    Ok(paths) => {
                        info!(?paths, "saved coordinates");
                        unsafe {
                            let _ = windows::Win32::System::Diagnostics::Debug::MessageBeep(
                                windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE(0x40),
                            );
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "save failed"),
                });
            }
        }
        VK_R => {
            if debounce().ready(vk, 400) {
                reload_config();
            }
        }
        VK_H => {
            if debounce().ready(vk, 400) {
                let now = !state::HUD_ON.load(Ordering::SeqCst);
                state::HUD_ON.store(now, Ordering::SeqCst);
                info!(hud = now, "hud toggled");
            }
        }
        VK_F10 => {
            info!("quit requested");
            request_exit();
        }
        _ => {}
    }
}

fn nudge(dir: i32) {
    let step = if shift_down() { 5 } else { 1 };
    let d = dir * step;
    {
        let mut p = state::PARAMS.lock();
        match state::ACTIVE_PARAM.load(Ordering::SeqCst) {
            0 => p.base_x += d,
            1 => p.base_y += d,
            2 => p.slot_w = (p.slot_w + d).max(1),
            3 => p.slot_h = (p.slot_h + d).max(1),
            4 => p.step_x = (p.step_x + d).max(1),
            5 => p.step_y = (p.step_y + d).max(1),
            _ => {}
        }
    }
}

fn shift_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_SHIFT};
    unsafe { (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0 }
}

fn reload_config() {
    let Some(path) = state::CONFIG_PATH.get().cloned() else {
        return;
    };
    match OverlayConfig::load_from(&path) {
        Ok(cfg) => {
            *state::PARAMS.lock() = cfg.grid;
            state::HUD_ON.store(cfg.overlay.show_hud, Ordering::SeqCst);
            info!("reloaded overlay.toml");
        }
        Err(e) => tracing::warn!(error = %e, "reload failed"),
    }
}

fn do_save() -> Result<Vec<PathBuf>> {
    let params = *state::PARAMS.lock();
    let title = state::NEEDLE
        .get()
        .cloned()
        .unwrap_or_else(|| "Elorin".to_string());
    let hud = state::HUD_ON.load(Ordering::SeqCst);
    grid::save_snapshot(&params, &title, hud)
}

fn debounce() -> &'static Debounce {
    static D: OnceLock<Debounce> = OnceLock::new();
    D.get_or_init(Debounce::new)
}

/// Find `overlay.toml` next to the exe first, then in the working directory.
fn resolve_config(exe_dir: Option<PathBuf>, cwd: Option<PathBuf>) -> (Option<PathBuf>, PathBuf) {
    let mut cfg = None;
    if let Some(d) = &exe_dir {
        let p = d.join("overlay.toml");
        if p.exists() {
            cfg = Some(p);
        }
    }
    if cfg.is_none() {
        if let Some(d) = &cwd {
            let p = d.join("overlay.toml");
            if p.exists() {
                cfg = Some(p);
            }
        }
    }

    let out = cfg
        .as_ref()
        .and_then(|p| p.parent().map(PathBuf::from))
        .or(exe_dir)
        .or(cwd)
        .unwrap_or_else(|| PathBuf::from("."));

    (cfg, out)
}

fn init_tracing() -> Result<()> {
    let file_appender = tracing_appender::rolling::daily("logs", "elorin-bot.log");
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

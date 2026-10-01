//! Global state shared between the overlay thread and the hotkey hook.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::OnceLock;

use parking_lot::Mutex;

use crate::grid::GridParams;

/// Whether the on-screen parameter HUD is drawn (toggled in-game with H).
pub static HUD_ON: AtomicBool = AtomicBool::new(true);
/// Which grid parameter the nudge keys currently act on (0..=5).
pub static ACTIVE_PARAM: AtomicUsize = AtomicUsize::new(0);

/// The live grid parameters. Written by the hotkey hook, read by the overlay.
pub static PARAMS: Mutex<GridParams> = Mutex::new(GridParams::DEFAULT);

/// Where `overlay.toml` was loaded from (rewritten on save).
pub static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();
/// Where `inventory_slots.json` is written (the exe's folder).
pub static OUT_DIR: OnceLock<PathBuf> = OnceLock::new();
/// The window-title needle to match against ("Elorin").
pub static NEEDLE: OnceLock<String> = OnceLock::new();

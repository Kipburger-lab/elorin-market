//! Elorin Bot — shared, reusable Win32 screen-reading primitives for the Elorin
//! client. Local only: no injection, no client launching.
//!
//! Modules:
//! - `window`   — find/identify the Elorin client window.
//! - `capture`  — BitBlt a window's client area into a BGRA `Frame`.
//! - `search`   — alpha-masked template matching (single + all-with-error).
//! - `overlay`  — transparent, click-through, topmost overlay window + painter.
//! - `hotkeys`  — low-level keyboard hook that consumes keys only while focused.
//! - `market`   — market price reading (Open buttons + digit OCR, ABI-style).

pub mod buy;
pub mod capture;
pub mod cloud;
pub mod hotkeys;
pub mod input;
pub mod market;
pub mod ocr;
pub mod overlay;
pub mod search;
pub mod window;

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by a quit hotkey; the overlays and message loops watch this to exit.
pub static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Request a clean shutdown: flag the exit and post WM_QUIT to the calling
/// thread's message queue (so its `GetMessageW` loop returns).
pub fn request_exit() {
    EXIT_REQUESTED.store(true, Ordering::SeqCst);
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
    }
}

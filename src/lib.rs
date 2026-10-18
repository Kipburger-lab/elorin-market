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

pub mod annotate;
pub mod buy;
pub mod capture;
pub mod cloud;
pub mod discord;
pub mod hotkeys;
pub mod input;
pub mod market;
pub mod ocr;
pub mod overlay;
pub mod record;
pub mod runlog;
pub mod search;
pub mod storage;
pub mod window;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Set by a quit hotkey; the overlays and message loops watch this to exit.
pub static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The thread that pumps messages, registered by `hotkeys::run`. Zero until then.
static MAIN_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// Tell `request_exit` which thread to wake. Called by the message pump itself.
pub fn set_main_thread_id(id: u32) {
    MAIN_THREAD_ID.store(id, Ordering::SeqCst);
}

/// Request a clean shutdown: flag the exit and wake the message pump.
///
/// The pump spends its life blocked in `GetMessageW`, which only returns when a
/// message arrives. That is fine when the request comes from a hotkey — the pump
/// is already awake, dispatching — but not when it comes from a background
/// thread (the loop going stuck), where the flag would otherwise sit unread until
/// the next unrelated keystroke. A no-op post to the pump's thread wakes it, and
/// it sees the flag and exits.
pub fn request_exit() {
    EXIT_REQUESTED.store(true, Ordering::SeqCst);
    unsafe {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{PostQuitMessage, PostThreadMessageW};
        let tid = MAIN_THREAD_ID.load(Ordering::SeqCst);
        if tid != 0 {
            let _ = PostThreadMessageW(tid, 0, WPARAM(0), LPARAM(0));
        }
        PostQuitMessage(0);
    }
}

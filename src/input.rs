//! Real OS mouse + keyboard input (SendInput) and cursor save/restore.
//!
//! Used to click the market's Refresh button and to drive a purchase (Open →
//! right-click the item → Buy 1 / Buy X → type the amount). The cursor is
//! restored afterwards so the operator's hand position isn't disturbed (same
//! approach as Elorin).

use std::time::Duration;

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

/// Virtual key codes we synthesise (u32, matching `hotkeys::parse_vk`).
///
/// Deliberately the only key this program presses. Escape is *not* here: it
/// belonged to the old ABI automation and has no place in the market project —
/// pressing it drops the client out of the market window, which is exactly the
/// stuck state we have been chasing.
pub const VK_RETURN: u32 = 0x0D;

/// Current cursor position in screen coordinates.
pub fn cursor_pos() -> Option<(i32, i32)> {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p).ok()? };
    Some((p.x, p.y))
}

pub fn set_cursor_pos(x: i32, y: i32) {
    unsafe {
        let _ = SetCursorPos(x, y);
    }
}

fn send_mouse(flags: MOUSE_EVENT_FLAGS) {
    let mut input = INPUT {
        r#type: INPUT_MOUSE,
        ..Default::default()
    };
    input.Anonymous.mi = MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: 0,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    unsafe {
        let _ = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

/// Click once at a screen coordinate, then restore the cursor to where it was.
pub fn click_screen(sx: i32, sy: i32) {
    let saved = cursor_pos();
    click_screen_keep(sx, sy);
    if let Some((x, y)) = saved {
        set_cursor_pos(x, y);
    }
}

/// Right-click once at a screen coordinate (opens the item's action menu).
pub fn right_click_screen(sx: i32, sy: i32) {
    let saved = cursor_pos();
    right_click_screen_keep(sx, sy);
    if let Some((x, y)) = saved {
        set_cursor_pos(x, y);
    }
}

/// Click and **leave the cursor there**.
///
/// Used while buying: a context menu tracks the pointer, so yanking it back to
/// where the operator had it can dismiss the menu we are about to click.
pub fn click_screen_keep(sx: i32, sy: i32) {
    click_with(MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, sx, sy);
}

/// Right-click and leave the cursor there (see [`click_screen_keep`]).
pub fn right_click_screen_keep(sx: i32, sy: i32) {
    click_with(MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, sx, sy);
}

/// Move the cursor with a real `SendInput` absolute move over the whole virtual
/// desktop.
///
/// `SetCursorPos` alone only moves the OS pointer: the client resolves its hover
/// from the moves sitting in its own input queue, so a click sent after a
/// `SetCursorPos` can arrive against a position the game never registered — the
/// click then passes through the UI. Rounding (not truncating) matters too: the
/// OS reverses this mapping with a truncating division, so truncating here would
/// land every warp up-left of the target.
fn move_cursor(sx: i32, sy: i32) {
    let (w, h) = virtual_desktop_size();
    if w <= 0 || h <= 0 {
        return;
    }
    let nx = ((sx as i64 * 65535 + w as i64 / 2) / w as i64) as i32;
    let ny = ((sy as i64 * 65535 + h as i64 / 2) / h as i64) as i32;

    let mut input = INPUT {
        r#type: INPUT_MOUSE,
        ..Default::default()
    };
    input.Anonymous.mi = MOUSEINPUT {
        dx: nx,
        dy: ny,
        mouseData: 0,
        dwFlags: MOUSE_EVENT_FLAGS(
            MOUSEEVENTF_MOVE.0 | MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0,
        ),
        time: 0,
        dwExtraInfo: 0,
    };
    unsafe {
        let _ = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

fn virtual_desktop_size() -> (i32, i32) {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN};
    unsafe { (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN)) }
}

/// Settle on the target before the button-down. The client needs to see the
/// hover before it populates the option the click depends on (a context-menu
/// entry, a shop slot); clicking sooner is how a click passes through the UI.
const CLICK_SETTLE: Duration = Duration::from_millis(60);
/// A fresh move immediately before the down: the client resolves a click against
/// the most recent move it has processed, and a move from the start of the
/// settle is not reliably still the one it is holding.
const CLICK_RELAND: Duration = Duration::from_millis(15);
/// Down/up gap. Too short and the client drops the press instead of registering
/// a real click.
const CLICK_GAP: Duration = Duration::from_millis(15);

/// Where the pointer is parked between actions, in screen coordinates.
///
/// The purchase sets this to client (629, 265) — the empty part of the
/// inventory — so the pointer never comes to rest on a shop slot, an item, or
/// anything else the game reacts to. Leaving it wherever the last click landed
/// is what makes a stray hover fire later.
static RECOVER_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(i32::MIN);
static RECOVER_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(i32::MIN);

pub fn set_recover_pos(x: i32, y: i32) {
    use std::sync::atomic::Ordering::Relaxed;
    RECOVER_X.store(x, Relaxed);
    RECOVER_Y.store(y, Relaxed);
}

pub fn recover() {
    use std::sync::atomic::Ordering::Relaxed;
    let x = RECOVER_X.load(Relaxed);
    let y = RECOVER_Y.load(Relaxed);
    if x == i32::MIN || y == i32::MIN {
        return;
    }
    move_cursor(x, y);
}

/// Move the pointer to `(sx, sy)` in a few steps rather than one jump.
///
/// The client keeps a cursor of its own and updates it from the moves it
/// receives; a single teleport moves the OS pointer while the game's cursor can
/// stay where it was, so the click resolves against the wrong position. A short
/// glide keeps the two in step. It is a handful of moves over ~20ms: fast, but a
/// real movement the client can follow.
fn glide(sx: i32, sy: i32) {
    let Some((x0, y0)) = cursor_pos() else {
        move_cursor(sx, sy);
        return;
    };
    if (x0 - sx).abs() <= 2 && (y0 - sy).abs() <= 2 {
        move_cursor(sx, sy);
        return;
    }
    const STEPS: i32 = 5;
    for i in 1..=STEPS {
        move_cursor(x0 + (sx - x0) * i / STEPS, y0 + (sy - y0) * i / STEPS);
        std::thread::sleep(Duration::from_millis(4));
    }
}

/// Glide onto the target, dwell, re-announce, then click.
///
/// Deliberately leaves the pointer on the target and does **not** recover: a
/// right-click opens a menu that belongs to that position, and moving away
/// straight afterwards is what stopped the menu ever appearing. The purchase
/// parks the pointer itself with [`recover`] once the menu work is finished.
fn click_with(down: MOUSE_EVENT_FLAGS, up: MOUSE_EVENT_FLAGS, sx: i32, sy: i32) {
    glide(sx, sy);
    std::thread::sleep(CLICK_SETTLE);
    move_cursor(sx, sy);
    std::thread::sleep(CLICK_RELAND);
    send_mouse(down);
    std::thread::sleep(CLICK_GAP);
    send_mouse(up);
    std::thread::sleep(Duration::from_millis(20));
}

fn send_key(vk: u32, release: bool) {
    let mut input = INPUT {
        r#type: INPUT_KEYBOARD,
        ..Default::default()
    };
    input.Anonymous.ki = KEYBDINPUT {
        wVk: VIRTUAL_KEY(vk as u16),
        wScan: 0,
        dwFlags: if release { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
        time: 0,
        dwExtraInfo: 0,
    };
    unsafe {
        let _ = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

/// Press and release a virtual key, pausing `delay_ms` around it.
pub fn press_key(vk: u32, delay_ms: u64) {
    send_key(vk, false);
    std::thread::sleep(Duration::from_millis(delay_ms.max(1)));
    send_key(vk, true);
    std::thread::sleep(Duration::from_millis(delay_ms.max(1)));
}

/// Type decimal digits (the only thing we ever need to type — a quantity).
/// Anything that isn't `0-9` is skipped rather than guessed at.
pub fn type_digits(digits: &str, delay_ms: u64) -> usize {
    let mut typed = 0;
    for ch in digits.chars() {
        if let Some(d) = ch.to_digit(10) {
            press_key(0x30 + d, delay_ms);
            typed += 1;
        }
    }
    typed
}

/// Press Enter once — the "Enter amount" prompt takes it to confirm.
pub fn tap_enter(delay_ms: u64) {
    press_key(VK_RETURN, delay_ms);
}

/// Type a quantity and submit it (the "Enter amount" prompt takes Enter).
pub fn type_amount_and_enter(amount: i64, delay_ms: u64) {
    type_digits(&amount.to_string(), delay_ms);
    std::thread::sleep(Duration::from_millis(delay_ms.max(1) * 3));
    press_key(VK_RETURN, delay_ms);
}

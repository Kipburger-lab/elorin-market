//! Generic low-level keyboard hook. Consumes the caller's keys and dispatches
//! them ONLY while the Elorin client window is foreground — so they never reach
//! the game and never affect other applications.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use tracing::info;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, MSG, WH_KEYBOARD_LL,
};

use crate::window;
use crate::EXIT_REQUESTED;

const WM_KEYDOWN: usize = 0x100;
const WM_KEYUP: usize = 0x101;
const WM_SYSKEYDOWN: usize = 0x104;
const WM_SYSKEYUP: usize = 0x105;

static NEEDLE: OnceLock<String> = OnceLock::new();
static OWNED: OnceLock<Vec<u32>> = OnceLock::new();
static ON_DOWN: OnceLock<Box<dyn Fn(u32) + Send + Sync>> = OnceLock::new();

/// Install the hook and pump messages until `EXIT_REQUESTED` / WM_QUIT.
///
/// `owned` lists the virtual-key codes this tool owns (they are consumed on both
/// key-down and key-up). `on_down` is invoked on key-down for owned keys.
pub fn run(
    needle: String,
    owned: Vec<u32>,
    on_down: impl Fn(u32) + Send + Sync + 'static,
) -> Result<()> {
    let _ = NEEDLE.set(needle);
    let _ = OWNED.set(owned);
    let _ = ON_DOWN.set(Box::new(on_down));

    let hinst = unsafe { GetModuleHandleW(None) }.map_err(|e| anyhow!("GetModuleHandleW: {e}"))?;
    let hook = unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(HINSTANCE(hinst.0)), 0)
    }
    .map_err(|e| anyhow!("SetWindowsHookExW: {e}"))?;

    info!("hotkeys armed (owned vks: {:?})", OWNED.get());

    let mut msg = MSG::default();
    while !EXIT_REQUESTED.load(Ordering::Acquire) {
        unsafe {
            if GetMessageW(&mut msg, None, 0, 0).into() {
                let _ = DispatchMessageW(&msg);
            } else {
                break;
            }
        }
    }

    unsafe {
        let _ = UnhookWindowsHookEx(hook);
    }
    info!("hotkeys unarmed");
    Ok(())
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == 0 {
        let is_key = matches!(
            wparam.0,
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
        );
        if is_key {
            let vk = (*(lparam.0 as *const KbdLlHookStruct)).vk_code;
            let down = wparam.0 == WM_KEYDOWN || wparam.0 == WM_SYSKEYDOWN;

            let needle = NEEDLE.get().map(String::as_str).unwrap_or("elorin");
            let fg = GetForegroundWindow();
            if window::is_elorin_window(fg, needle) {
                let owned = OWNED.get().map(Vec::as_slice).unwrap_or(&[]);
                if owned.contains(&vk) {
                    if down {
                        if let Some(f) = ON_DOWN.get() {
                            f(vk);
                        }
                    }
                    return LRESULT(1); // consume down AND up
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// One action per press (auto-repeat suppressed) for toggle/save-style keys.
pub struct Debounce {
    map: Mutex<HashMap<u32, Instant>>,
}

impl Debounce {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn ready(&self, vk: u32, ms: u128) -> bool {
        let now = Instant::now();
        let mut m = self.map.lock();
        if let Some(t) = m.get(&vk) {
            if now.duration_since(*t).as_millis() < ms {
                return false;
            }
        }
        m.insert(vk, now);
        true
    }
}

impl Default for Debounce {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a config key name into a Win32 virtual-key code:
/// `0`-`9`, `a`-`z`, `f1`-`f12`, `space`/`spacebar`.
pub fn parse_vk(s: &str) -> Option<u32> {
    let s = s.trim().to_ascii_lowercase();
    if s == "space" || s == "spacebar" {
        return Some(0x20);
    }
    if s.len() == 1 {
        let b = s.as_bytes()[0];
        if b.is_ascii_digit() {
            return Some(b as u32);
        }
        if b.is_ascii_alphabetic() {
            return Some(b.to_ascii_uppercase() as u32);
        }
    }
    if let Some(n) = s.strip_prefix('f') {
        if let Ok(n) = n.parse::<u32>() {
            if (1..=12).contains(&n) {
                return Some(0x6f + n); // VK_F1 = 0x70
            }
        }
    }
    None
}

// Minimal mirror of KBDLLHOOKSTRUCT for the one field we read.
#[repr(C)]
struct KbdLlHookStruct {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    extra: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_keys() {
        assert_eq!(parse_vk("f8"), Some(0x77));
        assert_eq!(parse_vk("F10"), Some(0x79));
        assert_eq!(parse_vk("space"), Some(0x20));
        assert_eq!(parse_vk("5"), Some(b'5' as u32));
        assert_eq!(parse_vk("e"), Some(b'E' as u32));
        assert_eq!(parse_vk("esc"), None);
    }
}

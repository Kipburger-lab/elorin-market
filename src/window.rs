//! Shared window-identification helpers.
//!
//! A window is the Elorin client if it is visible, has a non-empty client area,
//! its title matches the configured needle, and its window class is NOT a known
//! non-game app. The class blacklist exists because many apps carry "Elorin" in
//! their title (it's the project folder name): **Visual Studio Code, Discord and
//! Brave are all Chromium/Electron windows with class `Chrome_WidgetWin_1`**, so
//! excluding that one class removes all of them at once.
//!
//! IMPORTANT: do NOT blacklist Java AWT frame classes (`SunAwtFrame`, …) — the
//! game client is a Java/Swing window and would be excluded too.

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, POINT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetClientRect, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
};

/// Window classes that are never the Elorin game client: Chromium/Electron apps
/// (VS Code, Discord, Brave, Chrome, Edge), Firefox, terminals and the shell.
const NON_GAME_CLASSES: &[&str] = &[
    "Chrome_WidgetWin_1",            // Chromium/Electron: VS Code, Discord, Brave, Chrome, Edge
    "Chrome_WidgetWin_0",            // older Chromium
    "Chrome_WindowImpl_1",           // legacy Chrome
    "MozillaWindowClass",            // Firefox
    "CASCADIA_HOSTING_WINDOW_CLASS", // Windows Terminal
    "ConsoleWindowClass",            // classic conhost (the script's own console)
    "TerminalApp",                   // Windows Terminal app
    "CabinetWClass",                 // Explorer / file manager
];

/// Whether a window class name belongs to a known non-game app.
fn is_excluded_class(class_name: &str) -> bool {
    NON_GAME_CLASSES.iter().any(|c| class_name == *c)
}

/// Whether `hwnd` is the Elorin game client window: visible, a real top-level
/// window with a non-empty client area, a title matching `needle`, and a window
/// class that isn't a known non-game app (browser/Electron/terminal/shell).
///
/// `needle` should be lowercase. Title match: exact match of the text before a
/// `-` (e.g. "Elorin - Friendly"), else a substring match.
pub unsafe fn is_elorin_window(hwnd: HWND, needle: &str) -> bool {
    if hwnd.is_invalid() || !IsWindowVisible(hwnd).as_bool() {
        return false;
    }

    // Reject known non-game apps (VS Code, Discord, Brave, terminals, shell).
    let mut class_buf = [0u16; 256];
    let class_len = GetClassNameW(hwnd, &mut class_buf);
    if class_len > 0 {
        let class_name = String::from_utf16_lossy(&class_buf[..class_len as usize]);
        if is_excluded_class(&class_name) {
            return false;
        }
    }

    // The game window has a real, non-empty client area.
    let mut rc = Default::default();
    if GetClientRect(hwnd, &mut rc).is_ok() {
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        if w <= 0 || h <= 0 {
            return false;
        }
    }

    let len = GetWindowTextLengthW(hwnd);
    if len == 0 {
        return false;
    }
    let mut buf = vec![0u16; len as usize + 1];
    GetWindowTextW(hwnd, &mut buf);
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let title = String::from_utf16_lossy(&buf[..end]);
    match_title(&title, needle)
}

/// Find any visible window that looks like the Elorin client (first match,
/// top-to-bottom in Z order). Returns None when no client is running.
pub unsafe fn find_window(needle: &str) -> Option<HWND> {
    struct Target<'a> {
        needle: &'a str,
        hwnd: Option<HWND>,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let target = &mut *(lparam.0 as *mut Target);
        if is_elorin_window(hwnd, target.needle) {
            target.hwnd = Some(hwnd);
            return BOOL(0); // stop enumeration
        }
        BOOL(1)
    }

    let mut target = Target { needle, hwnd: None };
    let _ = EnumWindows(Some(enum_proc), LPARAM(&mut target as *mut Target as isize));
    target.hwnd
}

/// Map a client coordinate to a screen coordinate (for real mouse input).
pub unsafe fn client_to_screen(hwnd: HWND, x: i32, y: i32) -> Option<(i32, i32)> {
    let mut pt = POINT { x, y };
    if ClientToScreen(hwnd, &mut pt).as_bool() {
        Some((pt.x, pt.y))
    } else {
        None
    }
}

/// The client area size (width, height) in physical pixels.
pub unsafe fn client_size(hwnd: HWND) -> Option<(i32, i32)> {
    let mut rc = Default::default();
    if GetClientRect(hwnd, &mut rc).is_ok() {
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        if w > 0 && h > 0 {
            return Some((w, h));
        }
    }
    None
}

/// Case-insensitive title match: exact match of the text before a `-` (so
/// "Elorin - Friendly" matches needle "elorin"), else a substring match.
fn match_title(title: &str, needle: &str) -> bool {
    let title_lower = title.to_ascii_lowercase();
    let needle_lower = needle.to_ascii_lowercase();
    if let Some(idx) = title_lower.find('-') {
        let prefix = title_lower[..idx].trim();
        if prefix == needle_lower {
            return true;
        }
    }
    title_lower.contains(&needle_lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_electron_browsers_and_terminals() {
        // VS Code, Discord and Brave all share this Chromium/Electron class.
        assert!(is_excluded_class("Chrome_WidgetWin_1"));
        assert!(is_excluded_class("MozillaWindowClass"));
        assert!(is_excluded_class("ConsoleWindowClass"));
        assert!(is_excluded_class("CASCADIA_HOSTING_WINDOW_CLASS"));
        assert!(is_excluded_class("CabinetWClass"));
    }

    #[test]
    fn does_not_exclude_java_frames() {
        // The game client is a Java/Swing window — must NOT be blacklisted or
        // the script would never find the real client.
        assert!(!is_excluded_class("SunAwtFrame"));
        assert!(!is_excluded_class("SunAwtDialog"));
        assert!(!is_excluded_class(""));
        assert!(!is_excluded_class("SomeGameWindowClass"));
    }

    #[test]
    fn match_title_prefix_and_substring() {
        // "Elorin - Friendly" -> prefix before '-' matches.
        assert!(match_title("Elorin - Friendly", "elorin"));
        assert!(match_title("Elorin", "elorin"));
        // A VS Code window title would ALSO match by substring — which is why the
        // class blacklist (above) is required to reject it.
        assert!(match_title("config.rs - Elorin - Visual Studio Code", "elorin"));
        assert!(!match_title("Notepad", "elorin"));
    }
}

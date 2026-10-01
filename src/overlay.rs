//! A transparent, click-through, always-on-top overlay window that tracks the
//! Elorin client's client area and is shown only while that client is focused.
//!
//! The caller supplies a paint closure. Black background pixels are made
//! see-through with a GDI colour key, so the client stays fully visible and
//! playable underneath.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, CreateSolidBrush, DeleteObject, EndPaint, FillRect, HBRUSH, HDC,
    HGDIOBJ, RedrawWindow, SetBkMode, SetTextColor, TextOutW, PAINTSTRUCT, RDW_INVALIDATE,
    RDW_UPDATENOW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::capture::Rect;
use crate::window;
use crate::EXIT_REQUESTED;

/// While suspended the overlay hides itself entirely. Callers suspend it around
/// a screen capture so the overlay can never appear in the captured frame.
static SUSPENDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_suspended(v: bool) {
    SUSPENDED.store(v, std::sync::atomic::Ordering::SeqCst);
}

// COLORREF is 0x00BBGGRR.
pub const WHITE: u32 = 0x00FF_FFFF;
pub const MAGENTA: u32 = 0x00FF_00FF;
pub const GREEN: u32 = 0x0000_FF00;
pub const CYAN: u32 = 0x00FF_FF00;
pub const YELLOW: u32 = 0x0000_FFFF;
pub const RED: u32 = 0x0000_00FF;
pub const ORANGE: u32 = 0x0000_80FF;
pub const GRAY: u32 = 0x0080_8080;

/// The paint callback. Must be Send+Sync because it lives on the overlay thread.
pub type PaintFn = dyn Fn(&Painter) + Send + Sync + 'static;

thread_local! {
    static PAINT: RefCell<Option<Arc<PaintFn>>> = const { RefCell::new(None) };
}

/// Drawing helpers bound to the overlay's device context for one frame.
pub struct Painter {
    hdc: HDC,
    w: i32,
    h: i32,
}

impl Painter {
    pub fn size(&self) -> (i32, i32) {
        (self.w, self.h)
    }

    /// Fill a rectangle with a solid COLORREF colour.
    pub fn fill(&self, r: Rect, rgb: u32) {
        if r.x2 <= r.x1 || r.y2 <= r.y1 {
            return;
        }
        unsafe {
            let brush = CreateSolidBrush(COLORREF(rgb));
            let rc = RECT {
                left: r.x1,
                top: r.y1,
                right: r.x2,
                bottom: r.y2,
            };
            let _ = FillRect(self.hdc, &rc, brush);
            let _ = DeleteObject(HGDIOBJ(brush.0));
        }
    }

    /// Draw a rectangle outline `t` pixels thick (four filled bars).
    pub fn stroke(&self, r: Rect, rgb: u32, t: i32) {
        if r.x2 <= r.x1 || r.y2 <= r.y1 {
            return;
        }
        let t = t.max(1).min((r.x2 - r.x1).max(1)).min((r.y2 - r.y1).max(1));
        let (x1, y1, x2, y2) = (r.x1, r.y1, r.x2, r.y2);
        self.fill(Rect { x1, y1, x2, y2: y1 + t }, rgb);
        self.fill(Rect { x1, y1: y2 - t, x2, y2 }, rgb);
        self.fill(Rect { x1, y1, x2: x1 + t, y2 }, rgb);
        self.fill(Rect { x1: x2 - t, y1, x2, y2 }, rgb);
    }

    /// Draw a single line of text (default non-antialiased DC font).
    pub fn text(&self, x: i32, y: i32, s: &str, rgb: u32) {
        unsafe {
            let _ = SetBkMode(self.hdc, TRANSPARENT);
            let _ = SetTextColor(self.hdc, COLORREF(rgb));
            let wide: Vec<u16> = s.encode_utf16().collect();
            let _ = TextOutW(self.hdc, x, y, &wide);
        }
    }
}

/// Spawn the overlay on its own thread (own message pump; never blocks callers).
pub fn spawn(
    class_name: &'static str,
    window_title: &'static str,
    needle: String,
    paint: Arc<PaintFn>,
) {
    std::thread::Builder::new()
        .name("elorin-overlay".into())
        .spawn(move || unsafe { overlay_loop(class_name, window_title, needle, paint) })
        .expect("spawn overlay thread");
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);

            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let painter = Painter {
                hdc,
                w: rc.right - rc.left,
                h: rc.bottom - rc.top,
            };

            // Black background = transparent (colour key); callers draw on top.
            let bg = CreateSolidBrush(COLORREF(0));
            let full = RECT {
                left: 0,
                top: 0,
                right: painter.w,
                bottom: painter.h,
            };
            let _ = FillRect(hdc, &full, bg);
            let _ = DeleteObject(HGDIOBJ(bg.0));

            PAINT.with(|p| {
                if let Some(f) = p.borrow().as_ref() {
                    f(&painter);
                }
            });

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn overlay_loop(
    class_name: &str,
    window_title: &str,
    needle: String,
    paint: Arc<PaintFn>,
) {
    PAINT.with(|p| *p.borrow_mut() = Some(paint));

    let class_w: Vec<u16> = class_name.encode_utf16().chain(std::iter::once(0)).collect();
    let title_w: Vec<u16> = window_title.encode_utf16().chain(std::iter::once(0)).collect();
    let class_pcw = PCWSTR(class_w.as_ptr());
    let title_pcw = PCWSTR(title_w.as_ptr());

    let hmod = match GetModuleHandleW(None) {
        Ok(h) => h,
        Err(_) => return,
    };
    let hinst = HINSTANCE(hmod.0);

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        lpszClassName: class_pcw,
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        ..Default::default()
    };
    RegisterClassExW(&wc);

    let ex_style =
        WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT;
    let hwnd = match CreateWindowExW(
        ex_style,
        class_pcw,
        title_pcw,
        WS_POPUP,
        -2000,
        -2000,
        100,
        100,
        None,
        None,
        Some(hinst),
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_COLORKEY);

    let mut visible = false;
    let mut last_rect: Option<(i32, i32, i32, i32)> = None;
    let mut msg = MSG::default();

    while !EXIT_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Hidden while a caller is capturing, so it can't end up in the frame.
        if SUSPENDED.load(std::sync::atomic::Ordering::SeqCst) {
            if visible {
                let _ = ShowWindow(hwnd, SW_HIDE);
                visible = false;
                last_rect = None;
            }
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }

        let fg = GetForegroundWindow();
        if window::is_elorin_window(fg, &needle) {
            let mut rc = RECT::default();
            let ok = GetClientRect(fg, &mut rc).is_ok()
                && (rc.right - rc.left) > 0
                && (rc.bottom - rc.top) > 0;
            if ok {
                let w = rc.right - rc.left;
                let h = rc.bottom - rc.top;
                let mut origin = POINT { x: 0, y: 0 };
                if ClientToScreen(fg, &mut origin).as_bool() {
                    let cur = (origin.x, origin.y, w, h);
                    let mut force = !visible;
                    if last_rect != Some(cur) {
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            origin.x,
                            origin.y,
                            w,
                            h,
                            SWP_NOACTIVATE | SWP_SHOWWINDOW,
                        );
                        last_rect = Some(cur);
                        force = true;
                    }
                    visible = true;
                    if force {
                        let _ =
                            RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW);
                    }
                }
            }
        } else if visible {
            let _ = ShowWindow(hwnd, SW_HIDE);
            visible = false;
            last_rect = None;
        }

        // Repaint every tick so callers don't need a dirty flag.
        if visible {
            let _ = RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW);
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let _ = DestroyWindow(hwnd);
    let _ = UnregisterClassW(class_pcw, Some(hinst));
}

//! GDI-based capture: BitBlt a window's client area (or part of it) into a DIB.
//! Window-relative client coordinates (exactly like AHK's `CoordMode Pixel, Window`).

use anyhow::{anyhow, Result};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
    GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC,
    HGDIOBJ, ROP_CODE, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use crate::capture::{Capturer, Frame, Rect};

/// GDI capturer bound to a single window HWND.
pub struct GdiCapturer {
    hwnd: HWND,
}

unsafe impl Send for GdiCapturer {}
unsafe impl Sync for GdiCapturer {}

impl GdiCapturer {
    pub fn new(hwnd: HWND) -> Self {
        Self { hwnd }
    }

    /// Read the whole client area into a fresh BGRA buffer.
    pub fn capture_full_client(&self) -> Result<Frame> {
        unsafe {
            let mut rc = Default::default();
            GetClientRect(self.hwnd, &mut rc).map_err(|_| anyhow!("GetClientRect failed"))?;
            let w = rc.right - rc.left;
            let h = rc.bottom - rc.top;
            if w <= 0 || h <= 0 {
                return Err(anyhow!("client rect is empty"));
            }
            self.blit_client(0, 0, w, h)
        }
    }

    /// Read only the requested client rect (cheaper than a full capture — used
    /// for the high-rate change-polling loop).
    pub fn capture_region(&self, rect: Rect) -> Result<Frame> {
        unsafe {
            let mut rc = Default::default();
            GetClientRect(self.hwnd, &mut rc).map_err(|_| anyhow!("GetClientRect failed"))?;
            let cw = rc.right - rc.left;
            let ch = rc.bottom - rc.top;
            if cw <= 0 || ch <= 0 {
                return Err(anyhow!("client rect is empty"));
            }
            let x1 = rect.x1.max(0).min(cw);
            let y1 = rect.y1.max(0).min(ch);
            let x2 = rect.x2.max(0).min(cw);
            let y2 = rect.y2.max(0).min(ch);
            let w = x2 - x1;
            let h = y2 - y1;
            if w <= 0 || h <= 0 {
                return Err(anyhow!("capture region out of bounds"));
            }
            self.blit_client(x1, y1, w, h)
        }
    }

    /// BitBlt a client-space rectangle `(cx, cy, w, h)` into a BGRA frame.
    unsafe fn blit_client(&self, cx: i32, cy: i32, width: i32, height: i32) -> Result<Frame> {
        // Map the client origin (0,0) to screen coordinates.
        let mut origin = POINT { x: 0, y: 0 };
        if !ClientToScreen(self.hwnd, &mut origin).as_bool() {
            return Err(anyhow!("ClientToScreen failed"));
        }

        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return Err(anyhow!("GetDC(Desktop) failed"));
        }
        struct DcGuard(HDC);
        impl Drop for DcGuard {
            fn drop(&mut self) {
                unsafe {
                    ReleaseDC(None, self.0);
                }
            }
        }
        let _guard = DcGuard(screen_dc);

        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        if mem_dc.is_invalid() {
            return Err(anyhow!("CreateCompatibleDC failed"));
        }

        let bmp = CreateCompatibleBitmap(screen_dc, width, height);
        if bmp.is_invalid() {
            let _ = DeleteDC(mem_dc);
            return Err(anyhow!("CreateCompatibleBitmap failed"));
        }

        let prev_bmp = SelectObject(mem_dc, HGDIOBJ(bmp.0));

        // Plain SRCCOPY — deliberately WITHOUT CAPTUREBLT. CAPTUREBLT folds layered
        // windows into the grab, which here means our own transparent overlay
        // (boxes, digit markers, price text) would be captured and corrupt the very
        // pixels we are trying to read.
        let rop = ROP_CODE(SRCCOPY.0);
        BitBlt(
            mem_dc,
            0,
            0,
            width,
            height,
            Some(screen_dc),
            origin.x + cx,
            origin.y + cy,
            rop,
        )
        .ok();

        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // negative => top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [Default::default()],
        };
        let mut buf = vec![0u8; (width * height * 4) as usize];

        GetDIBits(
            mem_dc,
            bmp,
            0,
            height as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );

        SelectObject(mem_dc, prev_bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem_dc);

        Ok(Frame {
            width,
            height,
            bgra: buf,
        })
    }
}

impl Capturer for GdiCapturer {
    fn capture(&self, rect: Rect) -> Result<Frame> {
        // Full client, then crop in Rust (used by the one-shot scan path).
        let full = self.capture_full_client()?;

        let x1 = rect.x1.max(0).min(full.width);
        let y1 = rect.y1.max(0).min(full.height);
        let x2 = rect.x2.max(0).min(full.width);
        let y2 = rect.y2.max(0).min(full.height);
        let w = x2 - x1;
        let h = y2 - y1;
        if w <= 0 || h <= 0 {
            return Err(anyhow!("capture rect out of bounds"));
        }

        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for row in 0..h {
            let src = ((y1 + row) * full.width + x1) as usize * 4;
            let dst = (row * w) as usize * 4;
            let len = (w as usize) * 4;
            bgra[dst..dst + len].copy_from_slice(&full.bgra[src..src + len]);
        }

        Ok(Frame {
            width: w,
            height: h,
            bgra,
        })
    }
}

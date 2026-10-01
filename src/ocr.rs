//! Built-in Windows OCR (Windows.Media.Ocr).
//!
//! Local and dependency-free: uses the OS recognizer. Reads an opaque BGRA8
//! buffer — the caller crops + binarizes the region first, which measurably
//! helps on the client's tiny bitmap font.

use anyhow::{anyhow, Result};
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Storage::Streams::IBuffer;

pub struct Ocr {
    engine: OcrEngine,
}

// The engine is an agile WinRT object; we use it from a single thread anyway.
unsafe impl Send for Ocr {}
unsafe impl Sync for Ocr {}

impl Ocr {
    /// Create the OCR engine from the user's profile languages. Fails if no
    /// recognizer language is installed.
    pub fn new() -> Result<Self> {
        // WinRT async needs an initialized apartment on the calling thread.
        unsafe {
            use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr.is_err() {
                // RPC_E_CHANGED_MODE means the thread is already STA; workable.
                tracing::debug!(?hr, "CoInitializeEx(MTA) non-ok (continuing)");
            }
        }
        let engine = OcrEngine::TryCreateFromUserProfileLanguages()
            .map_err(|e| anyhow!("no Windows OCR engine available: {e}"))?;
        Ok(Self { engine })
    }

    /// Recognize text from an opaque BGRA8 buffer (`width * height * 4` bytes).
    pub fn recognize_bgra(&self, width: i32, height: i32, bgra: &[u8]) -> Result<String> {
        if width <= 0 || height <= 0 {
            return Ok(String::new());
        }
        let expected = (width as usize) * (height as usize) * 4;
        if bgra.len() < expected {
            return Err(anyhow!("bgra buffer too small for {width}x{height}"));
        }

        let buffer: IBuffer = CryptographicBuffer::CreateFromByteArray(&bgra[..expected])
            .map_err(|e| anyhow!("CreateFromByteArray: {e}"))?;
        // Opaque Bgra8 bitmaps are accepted directly (all our alpha bytes are 255).
        let bitmap =
            SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, width, height)
                .map_err(|e| anyhow!("CreateCopyFromBuffer: {e}"))?;

        let result = self
            .engine
            .RecognizeAsync(&bitmap)
            .map_err(|e| anyhow!("RecognizeAsync: {e}"))?
            .get()
            .map_err(|e| anyhow!("RecognizeAsync wait: {e}"))?;
        Ok(result.Text()?.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Windows OCR engine must be constructible (a recognizer language is
    /// installed). This is the real prerequisite for name reading.
    #[test]
    fn ocr_engine_is_available() {
        match Ocr::new() {
            Ok(_) => {}
            Err(e) => panic!("Windows OCR engine unavailable: {e}"),
        }
    }

    // NOTE: OCR itself is not exercised in unit tests. Windows.Media.Ocr (WinRT)
    // is not safe to drive from cargo's parallel test threads (it access-violates
    // when two tests recognize concurrently), and any frame-based assertion would
    // go stale as the captured frame changes. The name reader is verified live via
    // the scanner's console/log output.
}

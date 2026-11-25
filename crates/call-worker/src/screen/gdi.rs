//! GDI capture of one monitor (ADR-0053, amendment of 25 November 2026).
//!
//! `GetDC(NULL)` and `BitBlt` copy the monitor into a DIB section. The
//! measurement in `docs/evidence/screen-share-windows.md` shows that this
//! runs in the call worker's AppContainer with its current capabilities and
//! mitigations, where Windows.Graphics.Capture and DXGI Desktop Duplication
//! are denied. The worker makes no window and draws nothing.
//!
//! This file uses only `std`, the `windows` crate and the monitor list of
//! `wgc.rs`, so the call worker and the native containment gate compile the
//! same source. Limits: Windows draws no capture border and no cursor into a
//! GDI copy, and the copy is the same size as the monitor, at most
//! [`MAX_PIXELS`] pixels (the DIB section is one buffer of that size).

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};

use super::wgc;

/// The most pixels one capture copies (a 4K screen), as in the call
/// protocol's `MAX_CAPTURE_PIXELS`.
pub const MAX_PIXELS: u64 = 3840 * 2160;
/// The longest side one capture copies, as in the call protocol's
/// `MAX_CAPTURE_SIDE`. A larger monitor is refused at the start, because
/// the frame check would drop each of its frames.
pub const MAX_SIDE: u32 = 7680;
/// How often the monitor list is read again to see that the monitor is
/// still there with its size.
const RECHECK: Duration = Duration::from_millis(500);

/// Where a capture stopped. The numbers are the exit codes of the gate's
/// probe (`tests/platform/windows/src/screen_alt.rs`); keep them in step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Stage {
    /// No monitor is listed at the index.
    NoMonitor = 2,
    /// The monitor at the index has another size than the caller expects.
    SizeChanged = 3,
    /// The capture is not usable on this size.
    BadSize = 14,
    /// `GetDC(NULL)` or the memory DC failed.
    Dc = 20,
    /// The DIB section could not be made.
    Bitmap = 21,
    /// `BitBlt` failed (the screen is not readable, for example a locked
    /// or secure desktop).
    Blit = 22,
}

/// Whether a monitor of this size can be copied. Checked before the buffer
/// exists.
pub const fn size_ok(width: u32, height: u32) -> bool {
    width >= 2
        && height >= 2
        && width <= MAX_SIDE
        && height <= MAX_SIDE
        && (width as u64) * (height as u64) <= MAX_PIXELS
}

/// One running capture of a monitor. Use it on the thread that made it.
pub struct Capture {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *const u8,
    index: usize,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    checked: Instant,
}

/// Starts capturing the monitor at `index` of [`wgc::monitors`]. When
/// `expected` is set, a monitor of another size is [`Stage::SizeChanged`]:
/// the share asked for one screen, and it never takes another.
pub fn open(index: usize, expected: Option<(u32, u32)>) -> Result<Capture, Stage> {
    // The coordinates of the monitor list and of the screen DC must be
    // physical pixels, whatever the process manifest says. The setting is
    // for this thread only.
    // SAFETY: changes only the DPI awareness of the calling thread.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let monitor = *wgc::monitors().get(index).ok_or(Stage::NoMonitor)?;
    if expected.is_some_and(|size| size != (monitor.width, monitor.height)) {
        return Err(Stage::SizeChanged);
    }
    if !size_ok(monitor.width, monitor.height) {
        return Err(Stage::BadSize);
    }
    let (width, height) = (monitor.width, monitor.height);
    // SAFETY: each handle made below is owned by `Capture`, or released
    // here on a failure.
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return Err(Stage::Dc);
        }
        let memory = CreateCompatibleDC(Some(screen));
        if memory.is_invalid() {
            ReleaseDC(Some(HWND::default()), screen);
            return Err(Stage::Dc);
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // A negative height gives top-down rows.
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        {
            Ok(bitmap) if !bits.is_null() => bitmap,
            Ok(bitmap) => {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(memory);
                ReleaseDC(Some(HWND::default()), screen);
                return Err(Stage::Bitmap);
            }
            Err(_) => {
                let _ = DeleteDC(memory);
                ReleaseDC(Some(HWND::default()), screen);
                return Err(Stage::Bitmap);
            }
        };
        // A DIB section that is not selected would leave every copy black
        // and give `Drop` no object to put back.
        let old = SelectObject(memory, bitmap.into());
        if old.is_invalid() {
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(memory);
            ReleaseDC(Some(HWND::default()), screen);
            return Err(Stage::Bitmap);
        }
        Ok(Capture {
            screen,
            memory,
            bitmap,
            old,
            bits: bits.cast_const().cast(),
            index,
            left: monitor.left,
            top: monitor.top,
            width,
            height,
            checked: Instant::now(),
        })
    }
}

impl Capture {
    /// The monitor went away or changed its size. Reads the list at most
    /// twice a second.
    pub fn source_closed(&mut self) -> bool {
        if self.checked.elapsed() < RECHECK {
            return false;
        }
        self.checked = Instant::now();
        // SAFETY: changes only the DPI awareness of the calling thread.
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        wgc::monitors().get(self.index).is_none_or(|monitor| {
            (monitor.width, monitor.height, monitor.left, monitor.top)
                != (self.width, self.height, self.left, self.top)
        })
    }

    /// Copies the monitor when `admit` allows it and delivers the BGRX bytes
    /// as `(width, height, stride, bytes)`; they are valid only during the
    /// call. `Ok(false)`: nothing was copied (a frame `admit` refuses is
    /// dropped before the copy).
    pub fn poll(
        &mut self,
        admit: &mut dyn FnMut() -> bool,
        deliver: &mut wgc::Deliver<'_>,
    ) -> Result<bool, Stage> {
        if !admit() {
            return Ok(false);
        }
        // SAFETY: both DCs are live; the memory DC holds the DIB section.
        unsafe {
            BitBlt(
                self.memory,
                0,
                0,
                self.width as i32,
                self.height as i32,
                Some(self.screen),
                self.left,
                self.top,
                SRCCOPY | CAPTUREBLT,
            )
        }
        .map_err(|_| Stage::Blit)?;
        // The DIB section is read directly: finish any batched GDI work.
        // SAFETY: no argument.
        let _ = unsafe { GdiFlush() };
        let stride = self.width as usize * 4;
        // SAFETY: the DIB section holds `width * height * 4` bytes and lives
        // until `Drop`; the slice does not outlive this call.
        let bytes = unsafe { std::slice::from_raw_parts(self.bits, stride * self.height as usize) };
        deliver(self.width, self.height, stride as u32, bytes);
        Ok(true)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: releases the handles that `open` made, once.
        unsafe {
            SelectObject(self.memory, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.memory);
            ReleaseDC(Some(HWND::default()), self.screen);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN monitor sizes WHEN checked THEN a size the buffer can hold and
    /// the frame check takes passes, and a degenerate, huge, too wide or
    /// overflowing one does not.
    #[test]
    fn given_monitor_sizes_when_checked_then_only_bounded_sizes_pass() {
        assert!(size_ok(1920, 1080));
        assert!(size_ok(3840, 2160));
        assert!(size_ok(2, 2));
        assert!(size_ok(MAX_SIDE, 1000));
        assert!(!size_ok(1, 1080));
        assert!(!size_ok(1920, 0));
        assert!(!size_ok(3840, 2161));
        assert!(!size_ok(7680, 4320));
        assert!(!size_ok(u32::MAX, u32::MAX));
        // Under the pixel limit, but wider than any frame the worker sends.
        assert!(!size_ok(8000, 1000));
        assert!(!size_ok(MAX_SIDE + 1, 2));
    }
}

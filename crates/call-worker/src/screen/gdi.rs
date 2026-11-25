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
//!
//! The share names its monitor by the GDI device name (`\\.\DISPLAY1`)
//! and size the user saw. `open` takes only the listed monitor with that
//! name and size. The copy reads a rectangle of the whole desktop, so before
//! and after each copy the capture reads that monitor's name and rectangle
//! again: a monitor that moved, changed its size or went away ends the
//! share, and the copy made while it changed is dropped. Another monitor is
//! never sent as the chosen one.

use std::ffi::c_void;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, GetMonitorInfoW, HBITMAP, HDC,
    HGDIOBJ, HMONITOR, MONITORINFO, MONITORINFOEXW, ReleaseDC, SRCCOPY, SelectObject,
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

/// Where a capture stopped. The numbers are the exit codes of the gate's
/// probe (`tests/platform/windows/src/screen_alt.rs`); keep them in step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Stage {
    /// No listed monitor has the chosen name, or the monitor moved,
    /// changed or went away while it was shared.
    NoMonitor = 2,
    /// The monitor with the chosen name has another size than the caller
    /// expects.
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

/// Which monitor [`open`] takes.
#[derive(Clone, Copy, Debug)]
pub enum Choice<'a> {
    /// The first listed monitor, whatever its name and size: the gate's
    /// probe only. A share always names its monitor.
    #[allow(dead_code)]
    First,
    /// The monitor with this GDI device name (UTF-16 code units, no NUL)
    /// and this size in pixels.
    Named {
        device: &'a [u16],
        width: u32,
        height: u32,
    },
}

/// A GDI device name up to its NUL.
pub fn device_name(raw: &[u16]) -> &[u16] {
    let end = raw.iter().position(|&unit| unit == 0).unwrap_or(raw.len());
    &raw[..end]
}

/// The monitor of `list` that `choice` names. A name that no listed monitor
/// has is [`Stage::NoMonitor`] and another size is [`Stage::SizeChanged`]:
/// the share asked for one screen, and it never takes another.
pub fn choose(list: &[wgc::Monitor], choice: Choice<'_>) -> Result<wgc::Monitor, Stage> {
    match choice {
        Choice::First => list.first().copied().ok_or(Stage::NoMonitor),
        Choice::Named {
            device,
            width,
            height,
        } => {
            let monitor = list
                .iter()
                .find(|monitor| device_name(&monitor.device) == device)
                .copied()
                .ok_or(Stage::NoMonitor)?;
            if (monitor.width, monitor.height) == (width, height) {
                Ok(monitor)
            } else {
                Err(Stage::SizeChanged)
            }
        }
    }
}

/// Where a monitor is: its device name and its rectangle on the virtual
/// screen, in the pixels of the calling thread's DPI awareness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Place {
    device: [u16; wgc::DEVICE_NAME_UNITS],
    rect: (i32, i32, i32, i32),
}

/// The monitor's place now, or `None` when Windows no longer knows the
/// handle (the monitors changed).
fn place_of(monitor: HMONITOR) -> Option<Place> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: `info` is a valid MONITORINFOEXW with its size set; the
    // pointer cast is the documented way to pass the extended structure.
    // A handle that is no longer valid makes the call fail, nothing else.
    let known = unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()) };
    if !known.as_bool() {
        return None;
    }
    // Compare names up to the NUL only: Windows does not promise what the
    // rest of the buffer holds.
    let mut device = [0; wgc::DEVICE_NAME_UNITS];
    let name = device_name(&info.szDevice);
    device[..name.len()].copy_from_slice(name);
    let rect = info.monitorInfo.rcMonitor;
    Some(Place {
        device,
        rect: (rect.left, rect.top, rect.right, rect.bottom),
    })
}

/// One running capture of a monitor. Use it on the thread that made it.
pub struct Capture {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *const u8,
    monitor: HMONITOR,
    place: Place,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
}

/// Starts capturing the monitor that `choice` names (see [`choose`]).
pub fn open(choice: Choice<'_>) -> Result<Capture, Stage> {
    // The coordinates of the monitor list and of the screen DC must be
    // physical pixels, whatever the process manifest says. The setting is
    // for this thread only.
    // SAFETY: changes only the DPI awareness of the calling thread.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let monitor = choose(&wgc::monitors(), choice)?;
    if !size_ok(monitor.width, monitor.height) {
        return Err(Stage::BadSize);
    }
    // The place every copy is checked against. A monitor that changed since
    // the list was read is not taken. The copy is the mode's size from the
    // rectangle's corner, so the rectangle must have that size too: then the
    // copy never reaches past the chosen monitor.
    let place = place_of(monitor.handle)
        .filter(|place| {
            let (left, top, right, bottom) = place.rect;
            device_name(&place.device) == device_name(&monitor.device)
                && (left, top) == (monitor.left, monitor.top)
                && i64::from(right) - i64::from(left) == i64::from(monitor.width)
                && i64::from(bottom) - i64::from(top) == i64::from(monitor.height)
        })
        .ok_or(Stage::NoMonitor)?;
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
            monitor: monitor.handle,
            place,
            left: monitor.left,
            top: monitor.top,
            width,
            height,
        })
    }
}

impl Capture {
    /// The monitor went away, moved, or changed its name or size: it is no
    /// longer the place the capture copies. One `GetMonitorInfoW` call.
    pub fn source_closed(&self) -> bool {
        // SAFETY: changes only the DPI awareness of the calling thread.
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        place_of(self.monitor) != Some(self.place)
    }

    /// Copies the monitor when `admit` allows it and delivers the BGRX bytes
    /// as `(width, height, stride, bytes)`; they are valid only during the
    /// call. `Ok(false)`: nothing was copied (a frame `admit` refuses is
    /// dropped before the copy). [`Stage::NoMonitor`]: the monitor is not in
    /// its place before the copy, or no longer after it; nothing is
    /// delivered.
    pub fn poll(
        &mut self,
        admit: &mut dyn FnMut() -> bool,
        deliver: &mut wgc::Deliver<'_>,
    ) -> Result<bool, Stage> {
        if !admit() {
            return Ok(false);
        }
        if self.source_closed() {
            return Err(Stage::NoMonitor);
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
        // The monitors can change while the copy runs. A copy that ends with
        // the monitor in another place may hold another screen: drop it.
        if self.source_closed() {
            return Err(Stage::NoMonitor);
        }
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

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn listed(device: &str, left: i32, width: u32, height: u32) -> wgc::Monitor {
        let mut name = [0; wgc::DEVICE_NAME_UNITS];
        for (to, from) in name.iter_mut().zip(device.encode_utf16()) {
            *to = from;
        }
        wgc::Monitor {
            handle: HMONITOR::default(),
            device: name,
            left,
            top: 0,
            width,
            height,
        }
    }

    /// GIVEN two monitors of the same size that changed places WHEN the
    /// share names one by its device name THEN that monitor is taken, not
    /// the one that is now first; a name nobody has is not found, and
    /// another size is a changed size.
    #[test]
    fn given_monitors_when_one_is_named_then_only_that_one_is_taken() {
        let list = [
            listed(r"\\.\DISPLAY2", 0, 1920, 1080),
            listed(r"\\.\DISPLAY1", 1920, 1920, 1080),
        ];
        let device = wide(r"\\.\DISPLAY1");
        let named = |width, height| Choice::Named {
            device: &device,
            width,
            height,
        };
        let taken = choose(&list, named(1920, 1080)).expect("the named monitor");
        assert_eq!(taken.left, 1920);
        assert_eq!(device_name(&taken.device), device.as_slice());
        assert_eq!(
            choose(&list, named(2560, 1440)).err(),
            Some(Stage::SizeChanged)
        );
        let other = wide(r"\\.\DISPLAY9");
        let missing = Choice::Named {
            device: &other,
            width: 1920,
            height: 1080,
        };
        assert_eq!(choose(&list, missing).err(), Some(Stage::NoMonitor));
        // A prefix of a listed name is another name.
        let prefix = wide(r"\\.\DISPLAY");
        let short = Choice::Named {
            device: &prefix,
            width: 1920,
            height: 1080,
        };
        assert_eq!(choose(&list, short).err(), Some(Stage::NoMonitor));
        assert_eq!(choose(&[], Choice::First).err(), Some(Stage::NoMonitor));
        assert_eq!(choose(&list, Choice::First).expect("first").left, 0);
    }

    /// GIVEN a device name buffer WHEN read THEN it ends at the first NUL, or
    /// at the end of a buffer with none.
    #[test]
    fn given_a_device_name_buffer_when_read_then_it_ends_at_the_nul() {
        assert_eq!(device_name(&[65, 66, 0, 67]), &[65, 66]);
        assert_eq!(device_name(&[65, 66]), &[65, 66]);
        assert!(device_name(&[0, 65]).is_empty());
    }

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

//! GDI capture of one monitor (ADR-0053 amendment of 25 November 2026).
//!
//! The containment gate's measurement only. The call worker no longer
//! captures the screen (ADR-0067): the screen helper does, with
//! Windows.Graphics.Capture. This file stays because the gate measures what
//! the call AppContainer can do: `GetDC(NULL)` and `BitBlt` copy the monitor
//! into a DIB section there, with the worker's capabilities and mitigations,
//! where Windows.Graphics.Capture and DXGI Desktop Duplication are denied
//! (`docs/evidence/screen-share-windows.md`). The code draws nothing.
//!
//! This file uses only `std`, the `windows` crate and the helper's
//! `monitor.rs` and `wgc.rs`, so the gate compiles the same source that the
//! helper uses to choose a monitor. Windows draws no capture border and no
//! cursor into a GDI copy, and the copy is the same size as the monitor, at
//! most [`monitor::MAX_PIXELS`] pixels (the DIB section is one buffer of
//! that size).
//!
//! The copy reads a rectangle of the whole desktop, so before and after each
//! copy the capture reads that monitor's name and rectangle again
//! (`wgc::monitor_place`, the list's own reading): a monitor that moved,
//! changed its size or went away ends the capture, and the copy made while
//! it changed is dropped.

use std::ffi::c_void;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HBITMAP, HDC, HGDIOBJ, HMONITOR,
    ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};

use super::monitor::{self, Choice, Mismatch, device_name, size_ok};
use super::wgc::{self, Place};

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

/// The monitor list in physical pixels (see [`monitor::physical_monitors`]).
fn physical_monitors() -> Vec<wgc::Monitor> {
    monitor::physical_monitors()
}

/// The monitor's place now, or `None` when Windows no longer knows the
/// handle (the monitors changed). The same reading as the list's.
fn place_of(monitor: HMONITOR) -> Option<Place> {
    wgc::monitor_place(monitor)
}

/// The part of the desktop that a capture of `listed` copies, as `(left,
/// top, width, height)`: its rectangle now, when the handle still has the
/// listed name and rectangle. The copy is the rectangle, not the display
/// mode's size: a mode can differ from the rectangle, and a copy of the
/// rectangle never reaches past the monitor.
fn copy_area(listed: &wgc::Monitor, place: &Place) -> Result<(i32, i32, u32, u32), Stage> {
    if device_name(&place.device) != device_name(&listed.device) || place.rect != listed.rect {
        return Err(Stage::NoMonitor);
    }
    let (left, top, right, bottom) = place.rect;
    let side = |from: i32, to: i32| u32::try_from(i64::from(to) - i64::from(from)).ok();
    let (Some(width), Some(height)) = (side(left, right), side(top, bottom)) else {
        return Err(Stage::NoMonitor);
    };
    if !size_ok(width, height) {
        return Err(Stage::BadSize);
    }
    Ok((left, top, width, height))
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
    // for this thread only, and stays for the copies.
    let monitor =
        monitor::choose(&physical_monitors(), choice).map_err(|mismatch| match mismatch {
            Mismatch::NoMonitor => Stage::NoMonitor,
            Mismatch::SizeChanged => Stage::SizeChanged,
        })?;
    if !size_ok(monitor.width, monitor.height) {
        return Err(Stage::BadSize);
    }
    // The place every copy is checked against. A monitor that changed since
    // the list was read is not taken.
    let place = place_of(monitor.handle).ok_or(Stage::NoMonitor)?;
    let (left, top, width, height) = copy_area(&monitor, &place)?;
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
            left,
            top,
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
        self.poll_checked(admit, deliver, &mut |capture| !capture.source_closed())
    }

    /// [`Capture::poll`] with the place check `in_place`, which runs before
    /// and after the copy. A test can then change the place during a copy.
    fn poll_checked(
        &mut self,
        admit: &mut dyn FnMut() -> bool,
        deliver: &mut wgc::Deliver<'_>,
        in_place: &mut dyn FnMut(&Capture) -> bool,
    ) -> Result<bool, Stage> {
        if !admit() {
            return Ok(false);
        }
        if !in_place(self) {
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
        if !in_place(self) {
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
            rect: (left, 0, left + width as i32, height as i32),
            width,
            height,
        }
    }

    fn place(monitor: &wgc::Monitor) -> Place {
        Place {
            device: monitor.device,
            rect: monitor.rect,
        }
    }

    /// GIVEN a listed monitor and its place now WHEN the copy area is taken
    /// THEN it is the rectangle, also when the display mode is larger (a
    /// scaled mode); another name or a moved rectangle is not found, a
    /// reversed rectangle too, and a rectangle the buffer cannot hold is a
    /// bad size.
    #[test]
    fn given_a_monitor_place_when_the_copy_area_is_taken_then_it_is_the_rectangle() {
        let plain = listed(r"\\.\DISPLAY1", 1920, 1920, 1080);
        assert_eq!(copy_area(&plain, &place(&plain)), Ok((1920, 0, 1920, 1080)));
        // The mode says 1920x1080; the desktop rectangle is 1280x720.
        let mut scaled = listed(r"\\.\DISPLAY1", 0, 1920, 1080);
        scaled.rect = (0, 0, 1280, 720);
        assert_eq!(copy_area(&scaled, &place(&scaled)), Ok((0, 0, 1280, 720)));
        let mut renamed = place(&plain);
        renamed.device = listed(r"\\.\DISPLAY2", 1920, 1920, 1080).device;
        assert_eq!(copy_area(&plain, &renamed), Err(Stage::NoMonitor));
        let mut moved = place(&plain);
        moved.rect = (0, 0, 1920, 1080);
        assert_eq!(copy_area(&plain, &moved), Err(Stage::NoMonitor));
        let mut reversed = plain;
        reversed.rect = (1920, 0, 0, 1080);
        assert_eq!(
            copy_area(&reversed, &place(&reversed)),
            Err(Stage::NoMonitor)
        );
        let mut huge = plain;
        huge.rect = (0, 0, 7680, 4320);
        assert_eq!(copy_area(&huge, &place(&huge)), Err(Stage::BadSize));
    }

    /// A device name as the place records it: the name, then zeros.
    fn recorded_name(raw: &[u16]) -> [u16; wgc::DEVICE_NAME_UNITS] {
        let mut name = [0; wgc::DEVICE_NAME_UNITS];
        let units = device_name(raw);
        name[..units.len()].copy_from_slice(units);
        name
    }

    /// GIVEN a capture of a real monitor, opened by its device name WHEN it is
    /// polled while the monitor stays in its place THEN each poll delivers
    /// one frame of the monitor's size. WHEN the monitor is not where the
    /// capture recorded it (moved, or another monitor's name) THEN the poll
    /// ends with `NoMonitor` before the copy; WHEN it changes during the copy
    /// THEN the copy is dropped. Neither delivers a frame, and a monitor back
    /// in its place delivers again.
    ///
    /// The changes are made to the recorded place, not to the real display
    /// layout. Ignored: it copies the real screen (into memory only). Run it
    /// on Windows after a change to `poll` or the place checks:
    /// `cargo test -p persephone-test-windows -- --ignored real_monitor`.
    #[test]
    #[ignore = "copies the real screen"]
    fn given_a_real_monitor_when_its_place_changes_then_no_frame_is_delivered() {
        let list = physical_monitors();
        let monitor = list
            .iter()
            .find(|monitor| size_ok(monitor.width, monitor.height))
            .expect("a monitor the capture takes");
        let device = device_name(&monitor.device).to_vec();
        let mut capture = open(Choice::Named {
            device: &device,
            width: monitor.width,
            height: monitor.height,
            rect: monitor.rect,
        })
        .expect("the named monitor opens");
        let frames = std::cell::RefCell::new(Vec::new());
        let mut deliver = |width: u32, height: u32, stride: u32, bytes: &[u8]| {
            frames
                .borrow_mut()
                .push((width, height, stride, bytes.len()));
        };
        let mut admit = || true;
        for _ in 0..3 {
            assert_eq!(capture.poll(&mut admit, &mut deliver), Ok(true));
        }
        // The copy is the monitor's rectangle.
        let (left, top, right, bottom) = monitor.rect;
        let (width, height) = ((right - left) as u32, (bottom - top) as u32);
        let frame = (width, height, width * 4, (width * 4 * height) as usize);
        assert_eq!(*frames.borrow(), vec![frame; 3]);

        let recorded = capture.place;
        // The monitor moved: its rectangle is not the recorded one.
        capture.place.rect.0 += 1;
        assert_eq!(
            capture.poll(&mut admit, &mut deliver),
            Err(Stage::NoMonitor)
        );
        // Another monitor's name at this handle (or, with one monitor, a
        // name that no monitor has).
        capture.place = recorded;
        capture.place.device = match list
            .iter()
            .find(|other| device_name(&other.device) != device.as_slice())
        {
            Some(other) => recorded_name(&other.device),
            None => recorded_name(&wide(r"\\.\PERSEPHONE-NO-SUCH-DISPLAY")),
        };
        assert_eq!(
            capture.poll(&mut admit, &mut deliver),
            Err(Stage::NoMonitor)
        );
        // In place before the copy, moved after it: the copy is dropped.
        capture.place = recorded;
        let mut checks = 0;
        let mut moved_during_copy = |capture: &Capture| {
            checks += 1;
            checks == 1 && !capture.source_closed()
        };
        assert_eq!(
            capture.poll_checked(&mut admit, &mut deliver, &mut moved_during_copy),
            Err(Stage::NoMonitor)
        );
        assert_eq!(checks, 2, "both checks ran, so the copy ran between them");
        assert_eq!(frames.borrow().len(), 3, "no frame from a changed place");
        // Back in its place: frames come again.
        assert_eq!(capture.poll(&mut admit, &mut deliver), Ok(true));
        assert_eq!(frames.borrow().len(), 4);
    }
}

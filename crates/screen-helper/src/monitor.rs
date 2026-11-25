//! Which monitor a share takes (ADR-0053, ADR-0067).
//!
//! The core names the monitor by the GDI device name (`\\.\DISPLAY1`), the
//! size and the rectangle the user saw. [`choose`] takes only the listed
//! monitor with that name, size and rectangle. Windows can give a name to
//! another output when the monitors change, so the rectangle is the part of
//! the desktop that the user approved: the same name with another size is
//! [`Mismatch::SizeChanged`], and the same name with another rectangle is
//! [`Mismatch::NoMonitor`]. The share never takes another monitor.
//!
//! This file uses only `std`, the `windows` crate and `wgc.rs`, so the
//! screen helper and the native containment gate (its GDI measurement)
//! compile the same source.

use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};

use super::wgc;

/// The most pixels one capture takes (a 4K screen), as in the call
/// protocol's `MAX_CAPTURE_PIXELS`.
pub const MAX_PIXELS: u64 = 3840 * 2160;
/// The longest side one capture takes, as in the call protocol's
/// `MAX_CAPTURE_SIDE`. A larger monitor is refused at the start, because
/// the worker's frame check would drop each of its frames.
pub const MAX_SIDE: u32 = 7680;

/// Whether a monitor of this size can be captured. Checked before any
/// buffer exists.
pub const fn size_ok(width: u32, height: u32) -> bool {
    width >= 2
        && height >= 2
        && width <= MAX_SIDE
        && height <= MAX_SIDE
        && (width as u64) * (height as u64) <= MAX_PIXELS
}

/// Why no listed monitor is the chosen one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mismatch {
    /// No listed monitor has the chosen name, or the one that has it is in
    /// another place.
    NoMonitor,
    /// The monitor with the chosen name has another size.
    SizeChanged,
}

/// Which monitor [`choose`] takes.
#[derive(Clone, Copy, Debug)]
pub enum Choice<'a> {
    /// The first listed monitor, whatever its name and size: the gate's
    /// probe only. A share always names its monitor.
    #[allow(dead_code)]
    First,
    /// The monitor with this GDI device name (UTF-16 code units, no NUL),
    /// this size in pixels and this rectangle (left, top, right, bottom) on
    /// the virtual screen in physical pixels.
    #[allow(dead_code)]
    Named {
        device: &'a [u16],
        width: u32,
        height: u32,
        rect: (i32, i32, i32, i32),
    },
}

/// A GDI device name up to its NUL.
pub fn device_name(raw: &[u16]) -> &[u16] {
    let end = raw.iter().position(|&unit| unit == 0).unwrap_or(raw.len());
    &raw[..end]
}

/// The monitor of `list` that `choice` names.
pub fn choose(list: &[wgc::Monitor], choice: Choice<'_>) -> Result<wgc::Monitor, Mismatch> {
    match choice {
        Choice::First => list.first().copied().ok_or(Mismatch::NoMonitor),
        Choice::Named {
            device,
            width,
            height,
            rect,
        } => {
            let monitor = list
                .iter()
                .find(|monitor| device_name(&monitor.device) == device)
                .copied()
                .ok_or(Mismatch::NoMonitor)?;
            if (monitor.width, monitor.height) != (width, height) {
                Err(Mismatch::SizeChanged)
            } else if monitor.rect != rect {
                Err(Mismatch::NoMonitor)
            } else {
                Ok(monitor)
            }
        }
    }
}

/// The monitor list in physical pixels: the calling thread is set to
/// per-monitor DPI awareness first (for this thread only), as the core is
/// when it reads the rectangle it sends.
pub fn physical_monitors() -> Vec<wgc::Monitor> {
    // SAFETY: changes only the DPI awareness of the calling thread.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    wgc::monitors()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::HMONITOR;

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

    /// GIVEN two monitors of the same size that changed places WHEN the
    /// share names one by its device name and rectangle THEN that monitor is
    /// taken, not the one that is now first; a name nobody has is not found,
    /// another size is a changed size, and the right name with another
    /// rectangle (the name now on another output) is not found.
    #[test]
    fn given_monitors_when_one_is_named_then_only_that_one_is_taken() {
        let list = [
            listed(r"\\.\DISPLAY2", 0, 1920, 1080),
            listed(r"\\.\DISPLAY1", 1920, 1920, 1080),
        ];
        let device = wide(r"\\.\DISPLAY1");
        let right = (1920, 0, 3840, 1080);
        let named = |width, height, rect| Choice::Named {
            device: &device,
            width,
            height,
            rect,
        };
        let taken = choose(&list, named(1920, 1080, right)).expect("the named monitor");
        assert_eq!(taken.rect, right);
        assert_eq!(device_name(&taken.device), device.as_slice());
        assert_eq!(
            choose(&list, named(2560, 1440, right)).err(),
            Some(Mismatch::SizeChanged)
        );
        // DISPLAY1 was approved on the left; the name is now on the right.
        assert_eq!(
            choose(&list, named(1920, 1080, (0, 0, 1920, 1080))).err(),
            Some(Mismatch::NoMonitor)
        );
        let other = wide(r"\\.\DISPLAY9");
        let missing = Choice::Named {
            device: &other,
            width: 1920,
            height: 1080,
            rect: right,
        };
        assert_eq!(choose(&list, missing).err(), Some(Mismatch::NoMonitor));
        // A prefix of a listed name is another name.
        let prefix = wide(r"\\.\DISPLAY");
        let short = Choice::Named {
            device: &prefix,
            width: 1920,
            height: 1080,
            rect: right,
        };
        assert_eq!(choose(&list, short).err(), Some(Mismatch::NoMonitor));
        assert_eq!(choose(&[], Choice::First).err(), Some(Mismatch::NoMonitor));
        assert_eq!(choose(&list, Choice::First).expect("first").rect.0, 0);
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
        // Under the pixel limit, but wider than any frame the worker takes.
        assert!(!size_ok(8000, 1000));
        assert!(!size_ok(MAX_SIDE + 1, 2));
    }
}

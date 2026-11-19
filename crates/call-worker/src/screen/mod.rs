//! Screen capture (ADR-0053). The platform-neutral API: no platform and no
//! LiveKit types.
//!
//! One backend per OS: `windows.rs` (Windows.Graphics.Capture, through
//! `wgc.rs`), `linux.rs` (PipeWire over the ScreenCast portal remote) and
//! `unsupported.rs` for the rest (macOS reports screen sharing as
//! unavailable until its own decision). A backend offers `capture()`; this
//! module wraps it in [`open`]. The core names the screen: the worker never
//! lists screens to the user and never picks another one. A capture runs on
//! its own thread; [`Capture::stop`] (and `Drop`) ends it and waits for it,
//! so the screen is released when they return.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::mpsc;
use std::time::Duration;

use persephone_call_protocol::{ScreenState, ScreenTarget};

use crate::camera::{Capture, DoneSignal};

#[cfg(windows)]
#[path = "windows.rs"]
mod backend;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod backend;
#[cfg(not(any(windows, target_os = "linux")))]
#[path = "unsupported.rs"]
mod backend;

#[cfg(windows)]
mod wgc;

/// The longest wait for the screen to open: the Windows capture setup, or
/// on Linux the PipeWire connection and the wait for the stream (up to 6 s).
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// No screen picture is larger than this on either side (a 4K screen, a wide
/// screen).
const MAX_CAPTURE_SIDE: u32 = 7680;
/// Nor has it more pixels than 3840x2160.
const MAX_CAPTURE_PIXELS: u64 = 3840 * 2160;

/// A screen size the worker can take. Checked before any buffer for the
/// frame is made.
pub fn capture_size_ok(width: u32, height: u32) -> bool {
    width >= 2
        && height >= 2
        && width <= MAX_CAPTURE_SIDE
        && height <= MAX_CAPTURE_SIDE
        && u64::from(width) * u64::from(height) <= MAX_CAPTURE_PIXELS
}

/// The byte order of the 32-bit pixels in a [`CapturedFrame`], as in memory.
/// The fourth byte is alpha or padding; it is never used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    /// B, G, R, A (Windows BGRA; PipeWire BGRx and BGRA).
    Bgra,
    /// R, G, B, A (PipeWire RGBx and RGBA).
    #[cfg_attr(windows, allow(dead_code))]
    Rgba,
}

/// One screen picture. The bytes live until the callback returns.
#[derive(Debug)]
pub struct CapturedFrame<'a> {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub stride: u32,
    pub data: &'a [u8],
}

impl<'a> CapturedFrame<'a> {
    /// The bytes a whole picture needs, or `None` for a size outside the
    /// capture limits or a stride shorter than a row.
    pub fn expected_len(width: u32, height: u32, stride: u32) -> Option<usize> {
        if !capture_size_ok(width, height) || (stride as u64) < u64::from(width) * 4 {
            return None;
        }
        (stride as usize).checked_mul(height as usize)
    }

    /// A frame over `data`, or `None` when `data` is shorter than a whole
    /// picture or the size is outside the capture limits.
    pub fn checked(
        format: Format,
        width: u32,
        height: u32,
        stride: u32,
        data: &'a [u8],
    ) -> Option<Self> {
        let needed = Self::expected_len(width, height, stride)?;
        (data.len() >= needed).then(|| Self {
            format,
            width,
            height,
            stride,
            data: &data[..needed],
        })
    }
}

/// Why a capture ended by itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ended {
    /// The source went: the window or screen closed, the portal session
    /// closed. The share is off.
    Source,
    /// Capture failed. The share is unavailable.
    Failed,
}

/// What one screen start brought for its open: on Linux, the PipeWire remote
/// the core sent over the camera channel with kind `s`. Empty on Windows.
/// Dropping it closes the remote.
#[derive(Default)]
pub struct Grant {
    #[cfg(target_os = "linux")]
    remote: Option<std::os::fd::OwnedFd>,
}

impl Grant {
    /// Takes the oldest screen remote the channel holds. The core sends each
    /// remote directly before its `SetScreen`, so call this once for each
    /// such command, in order.
    pub fn take() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            remote: crate::camera_channel::take_handle(crate::camera_channel::Kind::Screen),
        }
    }
}

/// Capture threads that have not ended, as for the camera.
static CAPTURE_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Whether any screen capture thread still runs, including one whose
/// capture was dropped while a call blocked it.
pub fn captures_running() -> bool {
    CAPTURE_THREADS.load(std::sync::atomic::Ordering::Acquire) > 0
}

/// Starts capturing what `target` names and waits until it is open.
/// `on_frame` runs on the capture thread for every picture; `on_end` runs
/// there when the capture ends by itself (not after `stop`).
/// `Err(NotFound)`: the chosen screen is gone or has another size.
/// `Err(Refused)`: the OS refused. `Err(Unavailable)`: any other failure,
/// and a target of the other OS's kind.
pub fn open<F, E>(
    target: ScreenTarget,
    grant: Grant,
    on_frame: F,
    on_end: E,
) -> Result<Capture, ScreenState>
where
    F: FnMut(CapturedFrame<'_>) + Send + 'static,
    E: FnOnce(Ended) + Send + 'static,
{
    if !backend::accepts(&target) {
        return Err(ScreenState::Unavailable);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (done_tx, done) = mpsc::channel();
    let (open_tx, open_rx) = mpsc::channel();
    let thread_stop = stop.clone();
    // Counted from before the thread starts; a failed spawn drops it at once.
    let signal = DoneSignal::new(done_tx, &CAPTURE_THREADS);
    let thread = std::thread::Builder::new()
        .name("screen".to_owned())
        .spawn(move || {
            let _done = signal;
            backend::capture(target, grant, &thread_stop, open_tx, on_frame, on_end);
        })
        .map_err(|_| ScreenState::Unavailable)?;
    let capture = Capture::from_parts(stop, done, thread);
    match open_rx.recv_timeout(OPEN_TIMEOUT) {
        Ok(Ok(())) => Ok(capture),
        Ok(Err(state)) => {
            drop(capture);
            Err(state)
        }
        Err(_) => {
            drop(capture);
            Err(ScreenState::Unavailable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN screen sizes WHEN checked THEN 4K and wide screens pass and a
    /// size over the limits or under 2x2 does not.
    #[test]
    fn given_screen_sizes_when_checked_then_the_limits_hold() {
        for (width, height) in [
            (1920, 1080),
            (3840, 2160),
            (5120, 1440),
            (2, 2),
            (1080, 1920),
        ] {
            assert!(capture_size_ok(width, height), "{width}x{height}");
        }
        for (width, height) in [
            (0, 1080),
            (1, 1),
            (7681, 1080),
            (5120, 2880),
            (u32::MAX, 2),
            (4000, 4000),
        ] {
            assert!(!capture_size_ok(width, height), "{width}x{height}");
        }
    }

    /// GIVEN a short buffer, a short stride or a size over the limits WHEN a
    /// frame is made THEN there is none; a whole picture is cut to size.
    #[test]
    fn given_bad_buffers_when_a_frame_is_made_then_it_is_refused() {
        let data = vec![0_u8; 8 * 4 * 4];
        assert!(CapturedFrame::checked(Format::Bgra, 8, 4, 32, &data).is_some());
        assert!(CapturedFrame::checked(Format::Rgba, 8, 4, 36, &data).is_none());
        assert!(CapturedFrame::checked(Format::Bgra, 8, 4, 28, &data).is_none());
        assert!(CapturedFrame::checked(Format::Bgra, 8, 5, 32, &data).is_none());
        assert!(CapturedFrame::checked(Format::Bgra, 1, 1, 4, &data).is_none());
        assert!(CapturedFrame::checked(Format::Bgra, 7681, 2, 7681 * 4, &data).is_none());
        let frame = CapturedFrame::checked(Format::Bgra, 8, 2, 32, &data).expect("frame");
        assert_eq!(frame.data.len(), 64);
    }

    /// GIVEN a target of the other OS's kind WHEN it is opened THEN the
    /// result is `Unavailable` and no thread starts.
    #[test]
    fn given_a_target_of_another_os_when_opened_then_it_is_unavailable() {
        let wrong = if cfg!(windows) {
            ScreenTarget::Portal { node: 1 }
        } else {
            ScreenTarget::Monitor {
                index: 0,
                width: 1920,
                height: 1080,
            }
        };
        let result = open(wrong, Grant::default(), |_| {}, |_| {});
        assert_eq!(result.err(), Some(ScreenState::Unavailable));
    }
}

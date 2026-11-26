//! Screen capture (ADR-0053). The platform-neutral API: no platform and no
//! LiveKit types.
//!
//! One backend per OS: `windows.rs` (no capture of its own: it reads the
//! frames that the screen helper writes to a pipe, ADR-0067; Windows.Graphics.
//! Capture is denied in the call AppContainer, so the helper runs outside
//! it), `linux.rs` (PipeWire over the ScreenCast portal remote), `macos.rs`
//! (ScreenCaptureKit, ADR-0064, with its pure rules in `display.rs`) and
//! `unsupported.rs` for the rest. A backend offers `capture()`; this module
//! wraps it in [`open`]. The core names the screen: the worker never
//! lists screens to the user and never picks another one. A capture runs on
//! its own thread. [`Capture::stop`] and `Drop` ask it to end and wait for
//! it, but only for a bounded time: `stop` can return `Some(capture)` with
//! the thread still running, and `Drop` can return before the thread exits.
//! The thread may then still hold the screen source, so a return is no proof
//! that the screen was released. [`captures_running`] tells whether any
//! capture thread still runs.

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
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod backend;
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
#[path = "unsupported.rs"]
mod backend;
#[cfg(windows)]
mod frame_pipe;

/// The rules of a display share that need no OS type (ADR-0064); `macos.rs`
/// uses them, and the tests run them everywhere.
#[cfg(any(target_os = "macos", test))]
mod display;

/// The longest wait for the screen to open: on Windows the first item of
/// the helper's frame pipe (at most 6 s to its first byte and 3 s for the
/// item), or on Linux the PipeWire connection and the wait for the stream
/// (up to 6 s), or on macOS the display list and the stream start (up to
/// 4 s each).
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "linux")]
use persephone_call_protocol::MAX_CAPTURE_SIDE;
/// A screen size the worker can take: at most 7680 on a side and 3840x2160
/// pixels. Checked before any buffer for the frame is made. The core offers
/// only screens within the same rule, from the call protocol.
pub use persephone_call_protocol::screen_capture_size_ok as capture_size_ok;

/// The byte order of the 32-bit pixels in a [`CapturedFrame`], as in memory.
/// The fourth byte is alpha or padding; it is never used.
/// `unsupported.rs` makes no frame, so only tests make one there.
#[cfg_attr(
    not(any(windows, target_os = "linux", target_os = "macos")),
    allow(dead_code)
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    /// B, G, R, A (Windows and macOS BGRA; PipeWire BGRx and BGRA).
    Bgra,
    /// R, G, B, A (PipeWire RGBx and RGBA).
    #[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
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
    #[cfg_attr(
        not(any(windows, target_os = "linux", target_os = "macos")),
        allow(dead_code)
    )]
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

/// Why a capture ended by itself. `unsupported.rs` starts no capture, so
/// only tests make one there.
#[cfg_attr(
    not(any(windows, target_os = "linux", target_os = "macos")),
    allow(dead_code)
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ended {
    /// The source went: the window or screen closed, the portal session
    /// closed. The share is off.
    Source,
    /// Capture failed. The share is unavailable.
    Failed,
    /// The chosen monitor is gone or changed (Windows: the helper said so).
    /// The share is `NotFound`.
    #[cfg_attr(not(windows), allow(dead_code))]
    NotFound,
}

impl Ended {
    /// The share state that this end means.
    pub fn state(self) -> ScreenState {
        match self {
            Self::Source => ScreenState::Off,
            Self::Failed => ScreenState::Unavailable,
            Self::NotFound => ScreenState::NotFound,
        }
    }
}

/// What one screen start brought for its open: on Linux, the PipeWire remote
/// the core sent over the camera channel with kind `s`; on Windows, the read
/// end of the screen helper's frame pipe that came with `ScreenFrames`
/// (ADR-0067), and the number of its share. Empty on macOS.
/// Dropping it closes the remote or the pipe.
#[derive(Default)]
pub struct Grant {
    #[cfg(target_os = "linux")]
    remote: Option<std::os::fd::OwnedFd>,
    #[cfg(windows)]
    share: u32,
    #[cfg(windows)]
    frames: Option<std::os::windows::io::OwnedHandle>,
}

impl Grant {
    /// Takes the oldest screen remote the channel holds. The core sends each
    /// remote directly before its `SetScreen`, so call this once for each
    /// such command, in order.
    #[cfg(not(windows))]
    pub fn take() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            remote: crate::camera_channel::take_handle(crate::camera_channel::Kind::Screen),
        }
    }

    /// The frame pipe of share `share`, or none when no pipe came for it.
    #[cfg(windows)]
    pub fn from_frames(share: u32, frames: Option<std::os::windows::io::OwnedHandle>) -> Self {
        Self { share, frames }
    }
}

/// The frame pipe at handle value `value` of this process, owned, when it is
/// a pipe (Windows, ADR-0067). Any other value gives `None` and is left
/// untouched.
#[cfg(windows)]
pub fn adopt_frames(value: u32) -> Option<std::os::windows::io::OwnedHandle> {
    frame_pipe::adopt(value)
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

    /// GIVEN a target of another OS's kind WHEN it is opened THEN the result
    /// is `Unavailable` and no thread starts.
    #[test]
    fn given_a_target_of_another_os_when_opened_then_it_is_unavailable() {
        let portal = ScreenTarget::Portal { node: 1 };
        let monitor = ScreenTarget::Monitor {
            name: persephone_call_protocol::MonitorName::new(br"\\.\DISPLAY1").expect("a name"),
            width: 1920,
            height: 1080,
            rect: persephone_call_protocol::DesktopRect::new(0, 0, 1920, 1080)
                .expect("a rectangle"),
        };
        let display = ScreenTarget::Display {
            id: 1,
            width: 1920,
            height: 1080,
        };
        let wrong = if cfg!(windows) {
            [portal, display]
        } else if cfg!(target_os = "macos") {
            [portal, monitor]
        } else {
            [monitor, display]
        };
        for target in wrong {
            let result = open(target, Grant::default(), |_| {}, |_| {});
            assert_eq!(result.err(), Some(ScreenState::Unavailable), "{target:?}");
        }
    }
}

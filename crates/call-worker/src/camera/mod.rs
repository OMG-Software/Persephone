//! Camera listing and capture (ADR-0048). The platform-neutral API: no
//! platform and no LiveKit types.
//!
//! One backend per OS: `windows.rs` (Media Foundation), `macos.rs`
//! (AVFoundation), `linux.rs` (PipeWire portal or V4L2) and `unsupported.rs`
//! for the rest. A backend offers `list()`
//! and `capture()`; this module wraps them in [`list`] and [`open`]. Listing opens no camera. A capture runs on
//! its own thread; [`Capture::stop`] (and `Drop`) ends it and waits for it, so
//! the camera is released when they return.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use persephone_call_protocol::{CameraState, MAX_DEVICES, MediaDevice};

use crate::video::pick_camera;

pub use crate::args::CameraMode;

/// The camera path the launcher allowed (`--camera=...`, Linux only). Every
/// other OS has `None`. Backends read it; it never changes during a run.
#[allow(dead_code)] // read by the Linux backends (ADR-0049)
pub fn mode() -> CameraMode {
    crate::args::camera_mode()
}

#[cfg(windows)]
#[path = "windows.rs"]
mod backend;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod backend;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod backend;
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
#[path = "unsupported.rs"]
mod backend;

// Linux camera paths (ADR-0049). `yuyv` holds the format policy and the YUYV
// conversion; it has no OS code, so its tests run on every OS.
#[cfg(target_os = "linux")]
mod linux_portal;
#[cfg(target_os = "linux")]
mod v4l2;
#[cfg(any(target_os = "linux", test))]
mod yuyv;

/// Whether the backend lists and picks the camera inside its own capture (the
/// portal's descriptor serves one connection only).
#[cfg(target_os = "linux")]
fn picks_in_capture() -> bool {
    backend::picks_in_capture()
}
#[cfg(not(target_os = "linux"))]
fn picks_in_capture() -> bool {
    false
}

/// The longest wait for the camera to open.
/// macOS waits for the user's answer to the camera permission prompt, which
/// the backend bounds at 60 seconds (ADR-0049). The Linux portal backend
/// waits up to 5 seconds to list cameras (`ROUNDTRIP_TIMEOUT`) and then up to
/// 6 seconds for the stream to start (`STREAM_START_TIMEOUT`), so Linux
/// allows 5 + 6 + 2 seconds of margin. Keep this above the sum of those
/// constants in `linux_portal.rs`.
const OPEN_TIMEOUT: Duration = Duration::from_secs(if cfg!(target_os = "macos") {
    65
} else if cfg!(target_os = "linux") {
    13
} else {
    8
});
/// The longest wait for a capture thread to end.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// No camera frame is larger than this on either side, or this many pixels.
const MAX_CAPTURE_SIDE: u32 = 4096;
const MAX_CAPTURE_PIXELS: u64 = 3840 * 2160;

/// A frame size the worker can take from a camera. Checked before any buffer
/// for the frame is made.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn capture_size_ok(width: u32, height: u32) -> bool {
    width >= 2
        && height >= 2
        && width <= MAX_CAPTURE_SIDE
        && height <= MAX_CAPTURE_SIDE
        && u64::from(width) * u64::from(height) <= MAX_CAPTURE_PIXELS
}

/// The layout of the bytes in a [`CapturedFrame`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Format {
    /// `stride * height` bytes of Y, then `stride * ceil(height / 2)` bytes of
    /// interleaved U and V.
    Nv12,
    /// `stride * height` bytes of Y, then U and V planes of
    /// `ceil(stride / 2) * ceil(height / 2)` bytes each.
    #[allow(dead_code)] // for backends that deliver I420
    I420,
}

/// One camera picture. The bytes live until the callback returns.
#[derive(Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct CapturedFrame<'a> {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    /// Bytes per row of the Y plane.
    pub stride: u32,
    pub data: &'a [u8],
}

#[cfg_attr(not(windows), allow(dead_code))]
impl<'a> CapturedFrame<'a> {
    /// The bytes a whole picture needs, or `None` for a size outside the
    /// capture limits or a stride shorter than the width.
    pub fn expected_len(format: Format, width: u32, height: u32, stride: u32) -> Option<usize> {
        if !capture_size_ok(width, height) || stride < width {
            return None;
        }
        let (stride, height) = (stride as usize, height as usize);
        let y = stride.checked_mul(height)?;
        let chroma_rows = height.div_ceil(2);
        let chroma = match format {
            Format::Nv12 => stride.checked_mul(chroma_rows)?,
            Format::I420 => stride
                .div_ceil(2)
                .checked_mul(chroma_rows)?
                .checked_mul(2)?,
        };
        y.checked_add(chroma)
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
        let needed = Self::expected_len(format, width, height, stride)?;
        (data.len() >= needed).then(|| Self {
            format,
            width,
            height,
            stride,
            data: &data[..needed],
        })
    }
}

/// A running capture.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Capture {
    stop: Arc<AtomicBool>,
    done: mpsc::Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Capture {
    /// Asks the capture to end and waits up to `STOP_TIMEOUT` for the thread,
    /// so the camera is released. Returns `None` when the thread ended. When it
    /// did not end in time (a blocked read on a stalled camera), the capture
    /// comes back still running and the camera may stay busy. The caller then
    /// keeps it and polls it with [`Capture::try_finish`]. The thread is never
    /// detached silently. Dropping the capture waits once more.
    pub fn stop(mut self) -> Option<Capture> {
        if self.end() { None } else { Some(self) }
    }

    /// Whether the thread has ended, without waiting. Joins it when it has.
    /// Asks it to end first, so a capture that `stop` returned ends as soon as
    /// its blocked call returns.
    pub fn try_finish(&mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return true;
        };
        match self.done.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {
                self.thread = Some(thread);
                false
            }
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                let _ = thread.join();
                true
            }
        }
    }

    fn end(&mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return true;
        };
        let ended = self.done.recv_timeout(STOP_TIMEOUT) != Err(mpsc::RecvTimeoutError::Timeout);
        if ended {
            let _ = thread.join();
        } else {
            self.thread = Some(thread);
        }
        ended
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.end();
    }
}

/// Sends on drop, so the waiter in [`Capture::stop`] hears of every exit,
/// including a panic.
#[cfg_attr(not(windows), allow(dead_code))]
struct DoneSignal(mpsc::Sender<()>);

impl Drop for DoneSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Lists the cameras. Opens none. IDs and names fit the wire.
pub fn list() -> Vec<MediaDevice> {
    let mut list: Vec<MediaDevice> = Vec::new();
    for (id, name) in backend::list() {
        if list.len() >= MAX_DEVICES {
            break;
        }
        if list.iter().any(|device| device.id == id) {
            continue;
        }
        if let Some(device) = MediaDevice::bounded(&id, &name) {
            list.push(device);
        }
    }
    list
}

/// Opens the camera with this ID (`None`: the first listed) and starts
/// capturing. It waits until the camera is open. `on_frame` runs on the
/// capture thread for every picture; `on_end` runs there when the capture ends
/// by itself (not after `stop`). `Err(NotFound)`: the chosen ID is not listed,
/// and no other camera is used. `Err(Unavailable)`: any other failure.
pub fn open<F, E>(id: Option<&str>, on_frame: F, on_end: E) -> Result<Capture, CameraState>
where
    F: FnMut(CapturedFrame<'_>) + Send + 'static,
    E: FnOnce() + Send + 'static,
{
    if picks_in_capture() {
        // The empty ID means the first camera inside the backend. A chosen
        // empty ID is no camera, and no other is taken.
        if id == Some("") {
            return Err(CameraState::NotFound);
        }
        return start(id.unwrap_or(""), on_frame, on_end);
    }
    let devices = list();
    let chosen = pick_camera(id, devices.iter().map(|device| device.id.as_str()))?;
    start(&chosen, on_frame, on_end)
}

fn start<F, E>(device_id: &str, on_frame: F, on_end: E) -> Result<Capture, CameraState>
where
    F: FnMut(CapturedFrame<'_>) + Send + 'static,
    E: FnOnce() + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let (done_tx, done) = mpsc::channel();
    let (open_tx, open_rx) = mpsc::channel();
    let thread_stop = stop.clone();
    let id = device_id.to_owned();
    let thread = std::thread::Builder::new()
        .name("camera".to_owned())
        .spawn(move || {
            let _done = DoneSignal(done_tx);
            backend::capture(&id, &thread_stop, open_tx, on_frame, on_end);
        })
        .map_err(|_| CameraState::Unavailable)?;
    let capture = Capture {
        stop,
        done,
        thread: Some(thread),
    };
    match open_rx.recv_timeout(OPEN_TIMEOUT) {
        Ok(Ok(())) => Ok(capture),
        Ok(Err(error)) => {
            drop(capture);
            Err(error)
        }
        Err(_) => {
            drop(capture);
            Err(CameraState::Unavailable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capture whose thread ignores `stop` until `release` is sent.
    fn stuck_capture() -> (Capture, mpsc::Sender<()>) {
        let (release, wait) = mpsc::channel::<()>();
        let (done_tx, done) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _done = DoneSignal(done_tx);
            let _ = wait.recv();
        });
        let capture = Capture {
            stop: Arc::new(AtomicBool::new(false)),
            done,
            thread: Some(thread),
        };
        (capture, release)
    }

    /// GIVEN a capture whose thread ignores stop WHEN stopped THEN it comes
    /// back still running, and it finishes once the thread returns.
    #[test]
    fn given_a_stuck_capture_when_stopped_then_it_comes_back_and_can_finish() {
        let (capture, release) = stuck_capture();
        let started = std::time::Instant::now();
        let mut stalled = capture.stop().expect("the capture is still running");
        assert!(started.elapsed() >= STOP_TIMEOUT);
        assert!(!stalled.try_finish());
        release.send(()).expect("the thread waits");
        let limit = std::time::Instant::now() + Duration::from_secs(2);
        while !stalled.try_finish() {
            assert!(std::time::Instant::now() < limit, "the thread ends");
            std::thread::sleep(Duration::from_millis(10));
        }
        // A finished capture stays finished and drops at once.
        assert!(stalled.try_finish());
        assert!(stalled.stop().is_none());
    }

    /// GIVEN a capture whose thread ends at once WHEN stopped THEN nothing
    /// comes back.
    #[test]
    fn given_a_capture_that_ends_when_stopped_then_nothing_comes_back() {
        let (capture, release) = stuck_capture();
        release.send(()).expect("the thread waits");
        assert!(capture.stop().is_none());
    }

    /// GIVEN frame sizes WHEN checked THEN only sizes inside the capture
    /// limits pass.
    #[test]
    fn given_frame_sizes_when_checked_then_only_sane_ones_pass() {
        for (w, h) in [(2, 2), (640, 360), (1920, 1080), (3840, 2160), (4096, 2)] {
            assert!(capture_size_ok(w, h), "{w}x{h}");
        }
        for (w, h) in [
            (0, 0),
            (1, 1),
            (640, 0),
            (4096, 1),
            (4097, 2),
            (4096, 4096),
            (u32::MAX, 2),
        ] {
            assert!(!capture_size_ok(w, h), "{w}x{h}");
        }
    }

    /// GIVEN a frame size WHEN the length is asked THEN each layout counts
    /// its planes, and a bad size or stride gives none.
    #[test]
    fn given_frame_layout_when_length_asked_then_planes_are_counted() {
        assert_eq!(
            CapturedFrame::expected_len(Format::Nv12, 640, 360, 640),
            Some(640 * 360 * 3 / 2)
        );
        assert_eq!(
            CapturedFrame::expected_len(Format::I420, 640, 360, 640),
            Some(640 * 360 * 3 / 2)
        );
        // Odd sizes round the chroma up.
        assert_eq!(
            CapturedFrame::expected_len(Format::Nv12, 5, 3, 6),
            Some(6 * 3 + 6 * 2)
        );
        assert_eq!(
            CapturedFrame::expected_len(Format::I420, 5, 3, 5),
            Some(5 * 3 + 3 * 2 * 2)
        );
        assert_eq!(
            CapturedFrame::expected_len(Format::Nv12, 640, 360, 639),
            None
        );
        assert_eq!(CapturedFrame::expected_len(Format::Nv12, 1, 1, 1), None);
        assert_eq!(
            CapturedFrame::expected_len(Format::I420, 9000, 9000, 9000),
            None
        );
    }

    /// GIVEN a buffer WHEN a frame is made over it THEN a short buffer is
    /// refused and a long one is cut to the picture.
    #[test]
    fn given_buffer_when_frame_checked_then_short_buffers_are_refused() {
        let data = vec![0_u8; 4 * 2 * 3 / 2 + 7];
        let frame = CapturedFrame::checked(Format::Nv12, 4, 2, 4, &data).expect("fits");
        assert_eq!(frame.data.len(), 12);
        assert!(CapturedFrame::checked(Format::Nv12, 4, 2, 4, &data[..11]).is_none());
        assert!(CapturedFrame::checked(Format::Nv12, 4, 2, 3, &data).is_none());
    }

    /// GIVEN a chosen ID that no camera has WHEN a capture opens THEN it fails
    /// with NotFound (or Unavailable when there is no backend and no list) and
    /// no thread is started.
    #[test]
    fn given_unknown_camera_when_opened_then_it_fails_without_fallback() {
        let result = open(Some("no-such-camera"), |_| {}, || {});
        assert_eq!(result.err(), Some(CameraState::NotFound));
    }

    /// GIVEN the camera list WHEN it is read THEN it is within the wire limits
    /// and, on a target without a backend, empty.
    #[test]
    fn given_camera_list_when_read_then_it_fits_the_wire() {
        let list = list();
        assert!(list.len() <= MAX_DEVICES);
        if cfg!(not(any(windows, target_os = "macos"))) {
            assert!(list.is_empty());
        }
        for (index, device) in list.iter().enumerate() {
            assert!(!device.id.is_empty());
            assert!(list[..index].iter().all(|other| other.id != device.id));
        }
    }

    /// GIVEN no choice and no camera WHEN a capture opens THEN it is
    /// Unavailable. (Skipped when the machine has a camera.)
    #[test]
    fn given_no_camera_when_opened_without_choice_then_unavailable() {
        if list().is_empty() {
            assert_eq!(
                open(None, |_| {}, || {}).err(),
                Some(CameraState::Unavailable)
            );
        }
    }
}

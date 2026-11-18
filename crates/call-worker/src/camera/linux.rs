//! The Linux camera backend: it hands each call to the path the launcher
//! allowed (`--camera=...`, ADR-0049). Portal: PipeWire over the descriptor
//! the core sends (`linux_portal.rs`). V4L2: the worker opens `/dev/video*`
//! (`v4l2.rs`). None: no camera. The worker never falls from one path to the
//! other.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;

use persephone_call_protocol::CameraState;

use super::{CameraMode, CapturedFrame, linux_portal, mode, v4l2};

/// The portal's descriptor serves one connection, so the portal backend lists
/// and picks the camera inside its capture, on that connection. The other
/// paths list first.
pub fn picks_in_capture() -> bool {
    mode() == CameraMode::Portal
}

pub fn list() -> Vec<(String, String)> {
    match mode() {
        CameraMode::Portal => linux_portal::list(),
        CameraMode::V4l2 => v4l2::list(),
        CameraMode::None => Vec::new(),
    }
}

/// `device_id` is the camera ID; on the portal path `""` means the first camera.
pub fn capture<F, E>(
    device_id: &str,
    stop: &Arc<AtomicBool>,
    open_result: Sender<Result<(), CameraState>>,
    on_frame: F,
    on_end: E,
) where
    F: FnMut(CapturedFrame<'_>) + Send + 'static,
    E: FnOnce() + Send,
{
    match mode() {
        CameraMode::Portal => linux_portal::capture(device_id, stop, open_result, on_frame, on_end),
        CameraMode::V4l2 => v4l2::capture(device_id, stop.as_ref(), open_result, on_frame, on_end),
        CameraMode::None => {
            let _ = open_result.send(Err(CameraState::Unavailable));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN no camera mode (the test process has no launcher) WHEN a camera
    /// is listed or opened THEN there is none and nothing is opened.
    #[test]
    fn given_no_camera_mode_then_nothing_is_listed_or_opened() {
        assert!(!picks_in_capture());
        assert!(list().is_empty());
        let (tx, rx) = std::sync::mpsc::channel();
        capture("x", &Arc::new(AtomicBool::new(false)), tx, |_| {}, || {});
        assert_eq!(rx.recv().expect("result"), Err(CameraState::Unavailable));
    }
}

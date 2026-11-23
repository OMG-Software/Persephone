//! The Linux camera backend: it hands each call to the path the launcher
//! allowed (`--camera=...`, ADR-0049). Portal: PipeWire over the descriptor
//! the core sends (`linux_portal.rs`). V4L2: the worker opens `/dev/video*`
//! (`v4l2.rs`). None: no camera. The worker never falls from one path to the
//! other.
//!
//! Each camera ID starts with its path ([`PORTAL_PREFIX`] or
//! [`V4L2_PREFIX`]). A saved choice from the other path, or one without a
//! prefix (saved before the prefixes), matches no camera and is `NotFound`:
//! the worker never reads a portal `node.name` as a V4L2 ID or the reverse.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;

use persephone_call_protocol::CameraState;

use super::{CameraMode, CapturedFrame, linux_portal, mode, v4l2};

/// The start of each camera ID on the portal path (then the `node.name`).
const PORTAL_PREFIX: &str = "pipewire:";
/// The start of each camera ID on the V4L2 path (then `v4l2::camera_id`).
const V4L2_PREFIX: &str = "v4l2:";

/// The listed ID of a camera that the path `prefix` calls `id`.
fn tagged(prefix: &str, id: &str) -> String {
    format!("{prefix}{id}")
}

/// The path's own ID inside a listed ID, or `None` when `id` belongs to
/// another path, has no prefix, or names nothing after it.
fn untagged<'a>(prefix: &str, id: &'a str) -> Option<&'a str> {
    id.strip_prefix(prefix).filter(|rest| !rest.is_empty())
}

/// The portal's descriptor serves one connection, so the portal backend lists
/// and picks the camera inside its capture, on that connection. The other
/// paths list first.
pub fn picks_in_capture() -> bool {
    mode() == CameraMode::Portal
}

/// The cameras of the allowed path, with tagged IDs. An ID that the prefix
/// makes longer than the protocol takes is dropped later, by
/// `MediaDevice::bounded`.
pub fn list() -> Vec<(String, String)> {
    let (prefix, cameras) = match mode() {
        CameraMode::Portal => (PORTAL_PREFIX, linux_portal::list()),
        CameraMode::V4l2 => (V4L2_PREFIX, v4l2::list()),
        CameraMode::None => return Vec::new(),
    };
    cameras
        .into_iter()
        .map(|(id, name)| (tagged(prefix, &id), name))
        .collect()
}

/// `device_id` is a listed (tagged) camera ID; on the portal path `""` means
/// the first camera. An ID of another path is `NotFound`.
/// `remote` is the portal remote that came with the camera-on; only the
/// portal path uses it, and it is closed when the capture ends.
pub fn capture<F, E>(
    device_id: &str,
    remote: Option<OwnedFd>,
    stop: &Arc<AtomicBool>,
    open_result: Sender<Result<(), CameraState>>,
    on_frame: F,
    on_end: E,
) where
    F: FnMut(CapturedFrame<'_>) + Send + 'static,
    E: FnOnce() + Send,
{
    match mode() {
        CameraMode::Portal => {
            let own = if device_id.is_empty() {
                Some("")
            } else {
                untagged(PORTAL_PREFIX, device_id)
            };
            match own {
                Some(own) => {
                    linux_portal::capture(own, remote, stop, open_result, on_frame, on_end);
                }
                None => {
                    let _ = open_result.send(Err(CameraState::NotFound));
                }
            }
        }
        CameraMode::V4l2 => match untagged(V4L2_PREFIX, device_id) {
            Some(own) => v4l2::capture(own, stop.as_ref(), open_result, on_frame, on_end),
            None => {
                let _ = open_result.send(Err(CameraState::NotFound));
            }
        },
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
        capture(
            "x",
            None,
            &Arc::new(AtomicBool::new(false)),
            tx,
            |_| {},
            || {},
        );
        assert_eq!(rx.recv().expect("result"), Err(CameraState::Unavailable));
    }

    /// GIVEN a camera listed on one path WHEN its ID is read back on that
    /// path THEN the path's own ID comes out; on the other path, without a
    /// prefix, or with nothing after the prefix THEN there is none, so the
    /// choice is `NotFound` and no other camera is taken.
    #[test]
    fn given_a_tagged_id_then_only_its_own_path_reads_it() {
        let portal = tagged(PORTAL_PREFIX, "v4l2_device.usb-cam");
        assert_eq!(portal, "pipewire:v4l2_device.usb-cam");
        assert_eq!(
            untagged(PORTAL_PREFIX, &portal),
            Some("v4l2_device.usb-cam")
        );
        assert_eq!(untagged(V4L2_PREFIX, &portal), None, "other path");
        let v4l2 = tagged(V4L2_PREFIX, "uvcvideo|HD Webcam C270|usb-1");
        assert_eq!(
            untagged(V4L2_PREFIX, &v4l2),
            Some("uvcvideo|HD Webcam C270|usb-1")
        );
        assert_eq!(untagged(PORTAL_PREFIX, &v4l2), None, "other path");
        assert_eq!(
            untagged(V4L2_PREFIX, "uvcvideo|HD Webcam C270|usb-1"),
            None,
            "saved before the prefixes"
        );
        assert_eq!(untagged(PORTAL_PREFIX, "pipewire:"), None, "no name");
        assert_eq!(untagged(V4L2_PREFIX, ""), None);
    }
}

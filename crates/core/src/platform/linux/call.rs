//! The Linux call host (ADR-0049): the broker starts the call worker with the
//! call limits, a small environment, the `--camera` argument and the camera
//! channel; the worker contains itself (seccomp, Landlock) and reports
//! `Ready`; the host checks `/proc/<pid>/status` before any input. The read
//! and write loops and the `Ready` gate are shared
//! (`platform/call_host.rs`).

use super::credentials::{prompt, question_alert, yes};
use crate::calls::worker::{CameraPath, StartError};
use crate::platform::call_host::{
    ContainedCallHost as Host, DIRECT_CONNECTION_TEXT, Launched, Launcher,
};
use crate::platform::unix_call::launched;
use persephone_media_broker::{CallCamera, SeccompContainment, start_call_worker};
use std::path::Path;

/// Starts the call worker through the Linux broker.
pub struct LinuxLauncher {
    camera_path: fn() -> CameraPath,
}

/// The Linux call host.
pub type ContainedCallHost = Host<LinuxLauncher>;

impl ContainedCallHost {
    /// The call worker executable next to the running core.
    pub fn next_to_core() -> Self {
        Self::next_to_core_with(LinuxLauncher {
            camera_path: probe_camera_path,
        })
    }
}

/// Decides the camera path before the worker starts (ADR-0049): `Portal`
/// when the session bus has the camera portal, `V4l2` when it has no portal,
/// `None` when the bus fails in another way
/// (`platform/linux/camera_portal.rs`).
pub(crate) fn probe_camera_path() -> CameraPath {
    super::camera_portal::probe()
}

fn camera_argument(path: CameraPath) -> CallCamera {
    match path {
        CameraPath::Portal => CallCamera::Portal,
        CameraPath::V4l2 => CallCamera::V4l2,
        CameraPath::None => CallCamera::None,
    }
}

impl Launcher for LinuxLauncher {
    fn launch(&self, exe: &Path, camera: CameraPath) -> Result<Launched, StartError> {
        let profile = SeccompContainment::create_or_open("").map_err(|_| StartError::Launch)?;
        start_call_worker(&profile, exe, camera_argument(camera))
            .map(launched)
            .map_err(|_| StartError::Launch)
    }

    fn camera_path(&self) -> CameraPath {
        (self.camera_path)()
    }

    /// The same question as on Windows, in the core's own GTK window; "No"
    /// by default. No display, or a window that did not run, is "no".
    fn confirm_direct_connection(&self) -> bool {
        prompt(question_alert("Persephone", DIRECT_CONNECTION_TEXT))
            .and_then(|answer| yes(answer.response))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN each camera path WHEN mapped for the broker THEN the worker
    /// gets the matching argument.
    #[test]
    fn given_camera_paths_then_each_maps_to_its_broker_mode() {
        assert_eq!(camera_argument(CameraPath::Portal), CallCamera::Portal);
        assert_eq!(camera_argument(CameraPath::V4l2), CallCamera::V4l2);
        assert_eq!(camera_argument(CameraPath::None), CallCamera::None);
    }
}

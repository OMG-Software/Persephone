//! The macOS call host (ADR-0049): the broker starts the call worker with the
//! call limits and an empty environment; the worker applies its call Seatbelt
//! profile and reports `Ready`; the host asks `sandbox_check` before any
//! input. The read and write loops and the `Ready` gate are shared
//! (`platform/call_host.rs`). This module is not compiled on the development
//! machine; CI builds it.

use super::credentials::NativePrompt;
use crate::calls::worker::{CameraPath, StartError};
use crate::credentials::CredentialSource;
use crate::platform::call_host::{
    ContainedCallHost as Host, DIRECT_CONNECTION_TEXT, Launched, Launcher, other_sfu_text,
};
use crate::platform::unix_call::launched;
use persephone_media_broker::{SeatbeltContainment, start_call_worker};
use std::path::Path;

/// Starts the call worker through the macOS broker.
pub struct MacLauncher;

/// The macOS call host.
pub type ContainedCallHost = Host<MacLauncher>;

impl ContainedCallHost {
    /// The call worker executable next to the running core.
    pub fn next_to_core() -> Self {
        Self::next_to_core_with(MacLauncher)
    }
}

impl Launcher for MacLauncher {
    fn launch(&self, exe: &Path, _camera: CameraPath) -> Result<Launched, StartError> {
        let profile = SeatbeltContainment::create_or_open("").map_err(|_| StartError::Launch)?;
        start_call_worker(&profile, exe)
            .map(launched)
            .map_err(|_| StartError::Launch)
    }

    /// The same question as on Windows, in the core's own alert, "No" by
    /// default. No main loop, or an aborted alert, is "no".
    fn confirm_direct_connection(&self) -> bool {
        NativePrompt
            .confirm_open_link("Persephone", DIRECT_CONNECTION_TEXT)
            .unwrap_or(false)
    }

    /// The question about another SFU (ADR-0054) in the same Yes/No alert.
    /// Not compiled or run on the development machine.
    fn confirm_other_sfu(&self, host: &str, proxy_configured: bool) -> bool {
        NativePrompt
            .confirm_open_link("Persephone", &other_sfu_text(host, proxy_configured, false))
            .unwrap_or(false)
    }
}

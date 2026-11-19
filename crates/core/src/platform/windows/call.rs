//! The Windows call host: the call worker runs in the call AppContainer
//! (`internetClient` and `microphone` only) under a Job, and the core checks
//! its `Ready` message before it hands out the link (ADR-0043). The read and
//! write loops and the `Ready` gate are shared (`platform/call_host.rs`).

use crate::calls::worker::{CALL_WORKER_PROFILE, CameraPath, StartError, WorkerProcess};
use crate::platform::call_host::{
    ContainedCallHost as Host, DIRECT_CONNECTION_TEXT, Launched, Launcher, other_sfu_text,
};
use persephone_media_broker::{
    ContainmentProfile, LongLivedWorker, StartedCallWorker, start_call_worker,
};
use std::{
    path::{Path, PathBuf},
    ptr::null_mut,
    sync::{Arc, Mutex},
    time::Duration,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW,
};

/// Starts the call worker in its AppContainer and Job.
pub struct WindowsLauncher {
    profile: Mutex<Option<Arc<ContainmentProfile>>>,
}

/// The Windows call host.
pub type ContainedCallHost = Host<WindowsLauncher>;

impl ContainedCallHost {
    /// The call worker executable next to the running core.
    pub fn next_to_core() -> Self {
        Self::next_to_core_with(WindowsLauncher::new())
    }

    /// The call worker at `exe`; the profile is created on first use and
    /// kept.
    pub fn at(exe: PathBuf) -> Self {
        Self::with_launcher(exe, WindowsLauncher::new())
    }
}

impl WindowsLauncher {
    fn new() -> Self {
        Self {
            profile: Mutex::new(None),
        }
    }

    fn profile(&self) -> Result<Arc<ContainmentProfile>, StartError> {
        let mut profile = self
            .profile
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(profile) = profile.as_ref() {
            return Ok(profile.clone());
        }
        let created = Arc::new(
            ContainmentProfile::create_or_open(CALL_WORKER_PROFILE)
                .map_err(|_| StartError::Launch)?,
        );
        *profile = Some(created.clone());
        Ok(created)
    }
}

/// The Job and process of the worker, as the link needs them.
struct JobProcess(LongLivedWorker);

impl WorkerProcess for JobProcess {
    fn kill(&self) {
        self.0.kill();
    }

    fn wait_exit(&self, timeout: Duration) -> bool {
        self.0.wait_exit(timeout)
    }
}

/// A Yes/No box of the core with "No" as the default answer.
fn ask_yes_no(message: &str) -> bool {
    let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let caption: Vec<u16> = "Persephone".encode_utf16().chain(Some(0)).collect();
    // SAFETY: both strings are NUL-terminated and live for the call; the
    // box has no owner window.
    let answer = unsafe {
        MessageBoxW(
            null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_YESNO | MB_DEFBUTTON2 | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
        )
    };
    answer == IDYES
}

impl Launcher for WindowsLauncher {
    fn launch(&self, exe: &Path, _camera: CameraPath) -> Result<Launched, StartError> {
        let profile = self.profile()?;
        let StartedCallWorker {
            worker,
            stdin,
            stdout,
        } = start_call_worker(&profile, exe).map_err(|_| StartError::Launch)?;
        Ok(Launched {
            process: Box::new(JobProcess(worker)),
            stdin,
            stdout,
        })
    }

    fn confirm_direct_connection(&self) -> bool {
        ask_yes_no(DIRECT_CONNECTION_TEXT)
    }

    /// The same box with Yes and No: a message box has no custom button
    /// names, so the text says Yes means connect. "No" is the default.
    fn confirm_other_sfu(&self, host: &str, proxy_configured: bool) -> bool {
        ask_yes_no(&other_sfu_text(host, proxy_configured, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::worker::{CALL_WORKER_EXE, CallHost};

    /// GIVEN the built call worker WHEN the host starts it THEN it reports
    /// Ready with no missing control and the link ends it. Needs the
    /// worker built under `.local/build` (see `docs/development.md` and
    /// `crates/call-worker/AGENTS.md`). Run with
    /// `cargo test --locked -p persephone-core --lib platform::windows::call -- --ignored`.
    #[test]
    #[ignore = "needs the built persephone-call-worker.exe under .local/build"]
    fn given_the_built_worker_when_started_then_it_is_ready_and_contained() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/build");
        let exe = ["debug", "release"]
            .iter()
            .flat_map(|profile| {
                ["rust", "call-worker", "rust-call"]
                    .iter()
                    .map(|dir| root.join(dir).join(profile).join(CALL_WORKER_EXE))
                    .collect::<Vec<_>>()
            })
            .find(|path| path.is_file())
            .expect("persephone-call-worker.exe not found under .local/build");
        let link = ContainedCallHost::at(exe)
            .start()
            .expect("the contained worker reports Ready with bits 0");
        link.finish(Duration::from_secs(5));
    }
}

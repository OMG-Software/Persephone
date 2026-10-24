//! The media broker's contained worker on Windows: the worker runs in the
//! product's AppContainer profile under the broker's Job and mitigations.

use crate::media::{
    DECODE_TIME_LIMIT, DecodedPixels, ImageWorker, STRIP_TIME_LIMIT, WORKER_EXE,
    WORKER_MEMORY_BYTES, WORKER_PROFILE,
};
use persephone_core_protocol::ErrorCode;
use persephone_media_broker::{
    AppContainerProfile, LaunchError, Mitigations, RunError, WorkerPolicy, run_decode, run_strip,
};
use persephone_media_protocol::{ReceivedFrame, ResourceBudgets, WorkerFailure};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

pub struct ContainedWorker {
    exe: PathBuf,
    profile: Mutex<Option<Arc<AppContainerProfile>>>,
}

fn policy(limit: Duration) -> WorkerPolicy {
    WorkerPolicy {
        memory_limit_bytes: WORKER_MEMORY_BYTES,
        wall_deadline: limit,
        cpu_time_limit: limit,
        mitigations: Mitigations::default(),
    }
}

fn launch_error(error: LaunchError) -> ErrorCode {
    match error {
        LaunchError::InputTooLarge => ErrorCode::Rejected,
        LaunchError::Api { .. } => ErrorCode::Unavailable,
    }
}

/// Failures of the environment are `unavailable`; anything the input can
/// cause is `rejected`.
fn run_error(error: RunError) -> ErrorCode {
    match error {
        RunError::Worker(
            WorkerFailure::ContainmentCheck(_)
            | WorkerFailure::ModuleRejected
            | WorkerFailure::Instantiation,
        ) => ErrorCode::Unavailable,
        _ => ErrorCode::Rejected,
    }
}

impl ContainedWorker {
    /// The worker executable next to the running core.
    pub fn next_to_core() -> Self {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|core| core.parent().map(|dir| dir.join(WORKER_EXE)))
            .unwrap_or_else(|| PathBuf::from(WORKER_EXE));
        Self::at(exe)
    }

    /// The worker at `exe`, in the product's AppContainer profile, which
    /// is created on first use and kept.
    pub fn at(exe: PathBuf) -> Self {
        Self {
            exe,
            profile: Mutex::new(None),
        }
    }

    /// The worker at `exe` in a given profile (tests use a temporary
    /// one).
    pub fn with_profile(exe: PathBuf, profile: AppContainerProfile) -> Self {
        Self {
            exe,
            profile: Mutex::new(Some(Arc::new(profile))),
        }
    }

    fn profile(&self) -> Result<Arc<AppContainerProfile>, ErrorCode> {
        let mut profile = self
            .profile
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(profile) = profile.as_ref() {
            return Ok(profile.clone());
        }
        let created = Arc::new(
            AppContainerProfile::create_or_open(WORKER_PROFILE)
                .map_err(|_| ErrorCode::Unavailable)?,
        );
        *profile = Some(created.clone());
        Ok(created)
    }
}

impl ImageWorker for ContainedWorker {
    fn decode(
        &self,
        encoded: &[u8],
        budgets: &ResourceBudgets,
    ) -> Result<DecodedPixels, ErrorCode> {
        let profile = self.profile()?;
        let run = run_decode(
            &profile,
            &self.exe,
            encoded,
            &policy(DECODE_TIME_LIMIT),
            budgets,
        )
        .map_err(launch_error)?;
        let image = run.outcome.map_err(run_error)?;
        Ok(DecodedPixels {
            frame: image.pixels,
            orientation: image.orientation,
        })
    }

    fn strip(&self, encoded: &[u8], budgets: &ResourceBudgets) -> Result<ReceivedFrame, ErrorCode> {
        let profile = self.profile()?;
        let run = run_strip(
            &profile,
            &self.exe,
            encoded,
            &policy(STRIP_TIME_LIMIT),
            budgets,
        )
        .map_err(launch_error)?;
        run.outcome.map_err(run_error)
    }
}

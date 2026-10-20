//! Media broker: runs one decode in a freshly launched, contained media worker
//! (ADR-0003).
//!
//! The broker is trusted code in the key-holding process. It sends one encoded
//! image to the worker and accepts back exactly one bounded frame. On Windows
//! the worker starts suspended in a zero-capability AppContainer with process
//! mitigations set at creation (dynamic code prohibited, Win32k system calls
//! disabled, image-load hardening), inherits only its two pipe handles, and is
//! placed in a Job before it runs. The broker enforces the wall deadline by
//! terminating the Job. Containment for Linux and macOS is not implemented.

use persephone_media_protocol::{FrameError, ReceivedFrame, WorkerFailure};
use std::time::Duration;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{AppContainerProfile, run_worker};

/// Exit code the worker uses after reporting a failed containment check.
pub const WORKER_EXIT_CONTAINMENT: u32 = 3;

#[derive(Clone, Copy, Debug)]
pub struct WorkerPolicy {
    /// Job limit on the worker's committed memory.
    pub memory_limit_bytes: usize,
    /// Wall-clock deadline from resume; the Job is terminated when it passes.
    pub wall_deadline: Duration,
    /// Job limit on the worker's user-mode CPU time.
    pub cpu_time_limit: Duration,
    pub mitigations: Mitigations,
}

impl Default for WorkerPolicy {
    /// The media plan's starting budget: 256 MiB and 10 seconds.
    fn default() -> Self {
        Self {
            memory_limit_bytes: 256 * 1024 * 1024,
            wall_deadline: Duration::from_secs(10),
            cpu_time_limit: Duration::from_secs(10),
            mitigations: Mitigations::default(),
        }
    }
}

/// Process mitigations applied at worker creation. Both default on; the
/// worker refuses to decode if either is absent. Turning one off exists only
/// so tests can confirm the worker's self-check notices.
#[derive(Clone, Copy, Debug)]
pub struct Mitigations {
    pub prohibit_dynamic_code: bool,
    pub disable_win32k: bool,
}

impl Default for Mitigations {
    fn default() -> Self {
        Self {
            prohibit_dynamic_code: true,
            disable_win32k: true,
        }
    }
}

#[derive(Debug)]
pub enum LaunchError {
    /// A Windows API needed to build the containment failed.
    Api {
        call: &'static str,
        code: u32,
    },
    InputTooLarge,
}

#[derive(Debug)]
pub enum RunError {
    /// The wall deadline passed and the Job was terminated.
    DeadlineExceeded,
    /// The worker reported why it returned no pixels.
    Worker(WorkerFailure),
    /// The worker's reply was not a valid, in-budget frame.
    Protocol(FrameError),
    /// A control frame that is not a recognised failure message, or a frame
    /// kind other than pixels or control.
    UnexpectedReply,
    /// The worker exited without a usable reply.
    AbnormalExit(u32),
    /// The request could not be written to the worker.
    RequestWrite,
}

/// Measurements read from the worker's Job after it exits.
#[derive(Clone, Copy, Debug)]
pub struct WorkerStats {
    pub elapsed: Duration,
    pub peak_process_memory: usize,
    pub user_cpu: Duration,
    pub exit_code: u32,
}

pub struct WorkerRun {
    /// The validated pixel frame, which keeps its budget reservation.
    pub outcome: Result<ReceivedFrame, RunError>,
    pub stats: WorkerStats,
}

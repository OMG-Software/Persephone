//! Media broker: runs one decode or one metadata strip in a freshly launched,
//! contained media worker (ADR-0003, ADR-0009).
//!
//! The broker is trusted code in the key-holding process. It sends one
//! operation and one encoded image to the worker and accepts back one bounded
//! reply: pixels with their orientation ([`run_decode`]), a stripped file
//! ([`run_strip`]), or a failure. Both operations use the same containment,
//! deadline, exit-code and reply checks (`launch.rs`); each OS supplies only
//! the launcher that starts the worker under its containment. On Windows the
//! worker starts suspended in a zero-capability AppContainer with process
//! mitigations set at creation (dynamic code prohibited, Win32k system calls
//! disabled, image-load hardening), inherits only its two pipe handles, and
//! is placed in a Job before it runs. The broker enforces the wall deadline
//! by terminating the Job. Containment for Linux and macOS is not
//! implemented, so other platforms export no launcher, no [`run_decode`] and
//! no [`run_strip`].

use persephone_media_protocol::{FrameError, ReceivedFrame, WorkerFailure};
use std::time::Duration;

#[cfg_attr(not(windows), allow(dead_code))]
mod launch;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::AppContainerProfile;

/// The OS containment context a worker runs in: on Windows the AppContainer
/// profile whose SID the worker runs under.
#[cfg(windows)]
pub type ContainmentProfile = AppContainerProfile;

/// Launches a fresh contained worker, decodes `encoded`, and returns the
/// validated RGBA pixel frame with its EXIF orientation. The pixel frame keeps
/// its `WorkerOutput` budget reservation.
#[cfg(windows)]
pub fn run_decode(
    profile: &ContainmentProfile,
    worker: &std::path::Path,
    encoded: &[u8],
    policy: &WorkerPolicy,
    budgets: &persephone_media_protocol::ResourceBudgets,
) -> Result<WorkerRun<DecodedImage>, LaunchError> {
    launch::decode(profile, worker, encoded, policy, budgets)
}

/// Launches a fresh contained worker, strips the metadata from `encoded`, and
/// returns the stripped file as an `EncodedImage` frame. The frame keeps its
/// `EncodedInFlight` budget reservation and is never longer than `encoded`.
#[cfg(windows)]
pub fn run_strip(
    profile: &ContainmentProfile,
    worker: &std::path::Path,
    encoded: &[u8],
    policy: &WorkerPolicy,
    budgets: &persephone_media_protocol::ResourceBudgets,
) -> Result<WorkerRun<ReceivedFrame>, LaunchError> {
    launch::strip(profile, worker, encoded, policy, budgets)
}

/// Exit code the worker uses after reporting a failed containment check.
pub const WORKER_EXIT_CONTAINMENT: u32 = 3;

/// Limits for one worker run. Each launcher applies them with its OS's
/// controls (a Job on Windows).
#[derive(Clone, Copy, Debug)]
pub struct WorkerPolicy {
    /// Limit on the worker's committed memory.
    pub memory_limit_bytes: usize,
    /// Wall-clock deadline from the worker's start; the worker is killed
    /// when it passes.
    pub wall_deadline: Duration,
    /// Limit on the worker's user-mode CPU time.
    pub cpu_time_limit: Duration,
    /// Windows process mitigations; other launchers ignore them.
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

/// Windows process mitigations applied at worker creation. Both default on;
/// the worker refuses to decode if either is absent. Turning one off exists
/// only so tests can confirm the worker's self-check notices.
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
    /// An OS API needed to build the containment failed.
    Api {
        /// The API's name.
        call: &'static str,
        /// The OS error: a Win32 error code or HRESULT on Windows, an `errno`
        /// value on Unix.
        code: u32,
    },
    InputTooLarge,
}

#[derive(Debug)]
pub enum RunError {
    /// The wall deadline passed and the worker was killed.
    DeadlineExceeded,
    /// The worker reported why it returned no pixels.
    Worker(WorkerFailure),
    /// The worker's reply was not a valid, in-budget frame.
    Protocol(FrameError),
    /// A control frame that is not a recognised failure message, a frame kind
    /// the operation does not reply with, pixels without valid pixel
    /// information, or a stripped file longer than the input.
    UnexpectedReply,
    /// The worker exited without a usable reply.
    AbnormalExit(u32),
    /// The request could not be written to the worker.
    RequestWrite,
}

/// Measurements the launcher reads after the worker exits (from its Job on
/// Windows).
#[derive(Clone, Copy, Debug)]
pub struct WorkerStats {
    /// Wall time from the worker's start to its exit.
    pub elapsed: Duration,
    /// Peak committed memory of the worker process, in bytes; 0 if the OS
    /// does not report it.
    pub peak_process_memory: usize,
    /// User-mode CPU time the worker used.
    pub user_cpu: Duration,
    /// The worker's exit code as the OS reports it; `u32::MAX` if it could
    /// not be read.
    pub exit_code: u32,
}

/// The result of one worker run: [`DecodedImage`] for a decode, or the
/// stripped file's `EncodedImage` frame for a strip.
pub struct WorkerRun<T> {
    pub outcome: Result<T, RunError>,
    pub stats: WorkerStats,
}

/// A validated decode reply.
pub struct DecodedImage {
    /// The `RgbaPixels` frame, which keeps its `WorkerOutput` reservation.
    pub pixels: ReceivedFrame,
    /// EXIF orientation, 1 to 8 (1 for PNG). The pixels are not rotated; the
    /// caller applies the rotation or flip.
    pub orientation: u8,
}

//! The core's side of one call worker process (ADR-0043).
//!
//! A [`CallHost`] starts the worker under its platform's containment, reads
//! the worker's `Ready` message within [`READY_TIMEOUT`], and returns a
//! [`CallWorkerLink`] only when every containment bit is clear. A platform
//! without reviewed call containment refuses ([`Unsupported`]): the core
//! never starts an uncontained call worker.
//!
//! The link is two bounded queues and the process. The controller sends
//! [`ToWorker`] messages without blocking and receives [`FromWorker`]
//! messages. Dropping the link ends the process.

use persephone_call_protocol::{FromWorker, ToWorker};
use std::{fmt, sync::Arc, time::Duration};
use tokio::sync::mpsc;

/// How long the worker has to report `Ready` after it starts.
pub const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Messages waiting to be written to the worker.
pub const COMMAND_QUEUE: usize = 32;
/// Messages from the worker waiting for the controller.
pub const EVENT_QUEUE: usize = 64;

/// The call worker executable's file name, next to the core's own.
pub const CALL_WORKER_EXE: &str = if cfg!(windows) {
    "persephone-call-worker.exe"
} else {
    "persephone-call-worker"
};

/// The Windows AppContainer profile of the call worker. It is not the media
/// worker's profile, so the two never share a container identity.
pub const CALL_WORKER_PROFILE: &str = "Persephone.CallWorker";

/// Why a call worker did not start. Each is reported as a code only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// No call containment is implemented for this platform.
    Unsupported,
    /// The worker executable could not be located.
    NoExe,
    /// The OS refused to build the containment or start the process.
    Launch,
    /// The worker reported missing containment controls (the bits).
    Containment(u32),
    /// No valid `Ready` arrived within [`READY_TIMEOUT`].
    NotReady,
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => {
                f.write_str("call containment is not implemented for this platform")
            }
            Self::NoExe => f.write_str("cannot locate the call worker"),
            Self::Launch => f.write_str("cannot start the contained call worker"),
            Self::Containment(bits) => write!(f, "the call worker is not contained ({bits:#x})"),
            Self::NotReady => f.write_str("the call worker did not report ready"),
        }
    }
}

/// The worker could not take the message: its queue is full or it has gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerGone;

/// The running worker process, as the link needs it.
pub trait WorkerProcess: Send + Sync {
    /// Ends the worker and everything in its containment now.
    fn kill(&self);
    /// Waits up to `timeout` for the worker to exit. Returns whether it did.
    fn wait_exit(&self, timeout: Duration) -> bool;
}

/// One running, contained call worker.
pub struct CallWorkerLink {
    commands: mpsc::Sender<ToWorker>,
    /// Messages from the worker. `None` after the worker exited or its
    /// channel failed.
    pub events: mpsc::Receiver<FromWorker>,
    process: Arc<dyn WorkerProcess>,
}

impl CallWorkerLink {
    /// A link from its parts. Platform hosts use it; tests use it with a
    /// fake process.
    pub fn from_parts(
        commands: mpsc::Sender<ToWorker>,
        events: mpsc::Receiver<FromWorker>,
        process: Box<dyn WorkerProcess>,
    ) -> Self {
        Self {
            commands,
            events,
            process: Arc::from(process),
        }
    }

    /// A handle that ends the worker from outside the task that owns the
    /// link, for example when the session locks.
    pub fn kill_handle(&self) -> Arc<dyn WorkerProcess> {
        Arc::clone(&self.process)
    }

    /// Queues `message` for the worker without blocking.
    pub fn send(&self, message: ToWorker) -> Result<(), WorkerGone> {
        self.commands.try_send(message).map_err(|_| WorkerGone)
    }

    /// Asks the worker to leave, waits up to `timeout` for it to exit, then
    /// ends it in any case. Blocks; call it from a blocking task.
    pub fn finish(self, timeout: Duration) {
        let _ = self.commands.try_send(ToWorker::Leave);
        if !self.process.wait_exit(timeout) {
            self.process.kill();
        }
    }
}

impl Drop for CallWorkerLink {
    fn drop(&mut self) {
        self.process.kill();
    }
}

/// One platform's way to start and contain the call worker, and to ask the
/// user in the core's own window before a call bypasses the proxy.
pub trait CallHost: Send + Sync {
    /// Starts the worker, contains it before it runs, and waits for its
    /// `Ready`. Returns a link only when every containment bit is clear.
    /// Blocks for up to [`READY_TIMEOUT`]; call it from a blocking task.
    fn start(&self) -> Result<CallWorkerLink, StartError>;

    /// Asks, in a native dialog owned by the core, whether this call may
    /// connect directly: the profile has a SOCKS5 proxy, and the SFU and
    /// TURN server will see this computer's IP address (ADR-0028 addition).
    /// Blocks until answered; `true` only for an explicit yes.
    fn confirm_direct_connection(&self) -> bool;

    /// Whether this platform has reviewed call containment. `false` means
    /// [`CallHost::start`] always refuses.
    fn supported(&self) -> bool {
        true
    }
}

/// The platform's call host; [`Unsupported`] where call containment is not
/// implemented.
pub fn platform() -> Arc<dyn CallHost> {
    crate::platform::os::call_host()
}

/// Refuses every call, so an unsupported platform never runs an uncontained
/// call worker or connects a call directly.
pub struct Unsupported;

impl CallHost for Unsupported {
    fn start(&self) -> Result<CallWorkerLink, StartError> {
        Err(StartError::Unsupported)
    }

    fn confirm_direct_connection(&self) -> bool {
        false
    }

    fn supported(&self) -> bool {
        false
    }
}

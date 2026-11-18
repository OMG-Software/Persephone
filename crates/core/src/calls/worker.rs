//! The core's side of one call worker process (ADR-0043).
//!
//! A [`CallHost`] starts the worker under its platform's containment, reads
//! the worker's `Ready` message within [`READY_TIMEOUT`], and returns a
//! [`CallWorkerLink`] only when every containment bit is clear. A platform
//! without reviewed call containment refuses ([`Unsupported`]): the core
//! never starts an uncontained call worker.
//!
//! The link is two bounded queues, the picture store and the process. The
//! controller sends [`ToWorker`] messages without blocking and receives
//! [`FromWorker`] messages. Video pictures do not use the queue: the read
//! thread keeps the newest one per tile in the [`VideoStore`] (ADR-0048).
//! Dropping the link ends the process.

use super::video::VideoStore;
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

/// Which camera path the call worker may use (ADR-0049). The host decides it
/// when it starts the worker; the worker never changes it. Only Linux has more
/// than `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraPath {
    /// A PipeWire remote that the core gets from the camera portal and sends
    /// over the camera channel.
    Portal,
    /// The worker opens `/dev/video*` itself (no portal runs).
    V4l2,
    /// No camera path on this OS or desktop.
    None,
}

/// The running worker process, as the link needs it.
pub trait WorkerProcess: Send + Sync {
    /// Ends the worker and everything in its containment now.
    fn kill(&self);
    /// Waits up to `timeout` for the worker to exit. Returns whether it did.
    fn wait_exit(&self, timeout: Duration) -> bool;
    /// Whether the OS shows the worker as contained, checked after its `Ready`
    /// message and before any input goes to it (Linux: `/proc/<pid>/status`;
    /// macOS: `sandbox_check`). On Windows the containment is complete before
    /// the worker runs, so the default is `true`.
    fn confirm_contained(&self) -> bool {
        true
    }
}

/// One running, contained call worker.
pub struct CallWorkerLink {
    commands: mpsc::Sender<ToWorker>,
    /// Messages from the worker. `None` after the worker exited or its
    /// channel failed.
    pub events: mpsc::Receiver<FromWorker>,
    video: VideoStore,
    process: Arc<dyn WorkerProcess>,
    camera_path: CameraPath,
    /// The core's end of the camera channel (Linux).
    #[cfg(unix)]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    camera: Option<std::os::unix::net::UnixStream>,
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
            video: VideoStore::new(),
            process: Arc::from(process),
            camera_path: CameraPath::None,
            #[cfg(unix)]
            camera: None,
        }
    }

    /// Records the camera path the worker was started with.
    pub fn with_camera_path(mut self, path: CameraPath) -> Self {
        self.camera_path = path;
        self
    }

    /// Keeps the core's end of the worker's camera channel (Linux).
    #[cfg(unix)]
    pub fn with_camera_channel(mut self, channel: std::os::unix::net::UnixStream) -> Self {
        self.camera = Some(channel);
        self
    }

    /// The camera path this worker may use. It was fixed at start.
    pub fn camera_path(&self) -> CameraPath {
        self.camera_path
    }

    /// Sends the PipeWire remote from the camera portal to the worker over
    /// the camera channel: one descriptor and one byte, without blocking.
    /// Call it before `SetCamera` (and before `ListDevices` on the portal
    /// path). `Err(WorkerGone)`: there is no channel, it is full, or the
    /// worker has gone. The caller's descriptor stays open; close it after
    /// the call.
    #[cfg(target_os = "linux")]
    pub fn send_camera_handle(&self, fd: std::os::fd::BorrowedFd<'_>) -> Result<(), WorkerGone> {
        let channel = self.camera.as_ref().ok_or(WorkerGone)?;
        persephone_media_broker::send_camera_fd(channel, fd).map_err(|_| WorkerGone)
    }

    /// Uses `video` as the link's picture store: the one the host's read
    /// thread writes to.
    pub fn with_video(mut self, video: VideoStore) -> Self {
        self.video = video;
        self
    }

    /// The store of the newest picture on each tile. It is shared with the
    /// thread that reads the worker.
    pub fn video(&self) -> VideoStore {
        self.video.clone()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    /// A worker process that records kills and answers `wait_exit` with
    /// `exits`.
    #[derive(Default)]
    struct FakeProcess {
        exits: bool,
        kills: AtomicUsize,
        waits: Mutex<Vec<Duration>>,
    }

    impl WorkerProcess for Arc<FakeProcess> {
        fn kill(&self) {
            self.kills.fetch_add(1, Ordering::SeqCst);
        }

        fn wait_exit(&self, timeout: Duration) -> bool {
            self.waits.lock().unwrap().push(timeout);
            self.exits
        }
    }

    fn link(
        exits: bool,
        queue: usize,
    ) -> (CallWorkerLink, mpsc::Receiver<ToWorker>, Arc<FakeProcess>) {
        let (commands, received) = mpsc::channel(queue);
        let (_events_tx, events) = mpsc::channel(1);
        let process = Arc::new(FakeProcess {
            exits,
            ..FakeProcess::default()
        });
        let link = CallWorkerLink::from_parts(commands, events, Box::new(Arc::clone(&process)));
        (link, received, process)
    }

    /// GIVEN a platform without reviewed call containment WHEN a call starts
    /// or asks to bypass the proxy THEN it is refused.
    #[test]
    fn given_an_unsupported_platform_then_calls_are_refused() {
        let host = Unsupported;
        assert!(!host.supported());
        assert!(!host.confirm_direct_connection());
        assert_eq!(host.start().err(), Some(StartError::Unsupported));
    }

    /// GIVEN each start error WHEN shown THEN it is a distinct fixed text,
    /// and the containment bits are in hexadecimal.
    #[test]
    fn given_start_errors_then_each_text_is_distinct() {
        let texts: Vec<String> = [
            StartError::Unsupported,
            StartError::NoExe,
            StartError::Launch,
            StartError::Containment(0x12),
            StartError::NotReady,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let unique: std::collections::HashSet<&String> = texts.iter().collect();
        assert_eq!(unique.len(), texts.len());
        assert_eq!(texts[3], "the call worker is not contained (0x12)");
    }

    /// GIVEN a full command queue WHEN another message is sent THEN it is
    /// refused at once; GIVEN a worker that has gone THEN sending is refused.
    #[test]
    fn given_a_full_or_closed_queue_then_send_is_refused() {
        let (link, received, _) = link(true, 1);
        assert_eq!(link.send(ToWorker::Leave), Ok(()));
        assert_eq!(link.send(ToWorker::Leave), Err(WorkerGone));
        drop(received);
        assert_eq!(link.send(ToWorker::Leave), Err(WorkerGone));
    }

    /// GIVEN a worker that exits after Leave WHEN the link finishes THEN Leave
    /// is sent, the wait uses the timeout, and only the drop kills it.
    #[test]
    fn given_a_worker_that_exits_when_finished_then_it_is_asked_to_leave() {
        let (link, mut received, process) = link(true, 4);
        link.finish(Duration::from_secs(3));
        assert!(matches!(received.try_recv(), Ok(ToWorker::Leave)));
        assert_eq!(*process.waits.lock().unwrap(), [Duration::from_secs(3)]);
        assert_eq!(process.kills.load(Ordering::SeqCst), 1);
    }

    /// GIVEN a worker that does not exit WHEN the link finishes THEN it is
    /// killed, and the drop kills it again.
    #[test]
    fn given_a_worker_that_stays_when_finished_then_it_is_killed() {
        let (link, _received, process) = link(false, 4);
        link.finish(Duration::from_millis(1));
        assert_eq!(process.kills.load(Ordering::SeqCst), 2);
    }

    /// GIVEN a kill handle WHEN it is used while the link is alive THEN it
    /// ends the same process the link owns.
    #[test]
    fn given_a_kill_handle_then_it_ends_the_same_process() {
        let (link, _received, process) = link(true, 4);
        link.kill_handle().kill();
        assert_eq!(process.kills.load(Ordering::SeqCst), 1);
        drop(link);
        assert_eq!(process.kills.load(Ordering::SeqCst), 2);
    }
}

//! The call worker process as Linux and macOS start it: the broker's
//! long-lived worker, ended and checked through the link's
//! [`WorkerProcess`] (ADR-0043, ADR-0049).

use crate::calls::worker::{WorkerExit, WorkerProcess};
use crate::platform::call_host::Launched;
use persephone_media_broker::{LongLivedWorker, StartedCallWorker};
use std::time::Duration;

/// The worker and its process group, as the link needs them.
struct UnixProcess(LongLivedWorker);

impl WorkerProcess for UnixProcess {
    fn kill(&self) {
        self.0.kill();
    }

    fn wait_exit(&self, timeout: Duration) -> bool {
        self.0.wait_exit(timeout)
    }

    fn exit_status(&self) -> Option<WorkerExit> {
        self.0.exit_status().map(|exit| match exit {
            persephone_media_broker::WorkerExit::Code(code) => WorkerExit::Code(code),
            persephone_media_broker::WorkerExit::Signal(signal) => WorkerExit::Signal(signal),
        })
    }

    /// `/proc/<pid>/status` on Linux, `sandbox_check` on macOS.
    fn confirm_contained(&self) -> bool {
        self.0.confirm_contained()
    }
}

/// The host's view of a started worker.
pub fn launched(started: StartedCallWorker) -> Launched {
    Launched {
        process: Box::new(UnixProcess(started.worker)),
        stdin: started.stdin,
        stdout: started.stdout,
        camera_channel: started.camera,
    }
}

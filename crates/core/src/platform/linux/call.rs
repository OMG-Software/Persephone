//! The Linux call host (ADR-0049): the broker starts the call worker with the
//! call limits, a small environment, the `--camera` argument and the camera
//! channel; the worker contains itself (seccomp, Landlock) and reports
//! `Ready`; the host checks `/proc/<pid>/status` before any input. The read
//! and write loops and the `Ready` gate are shared
//! (`platform/call_host.rs`).
//!
//! The worker's parent-death signal (`PR_SET_PDEATHSIG`) follows the thread
//! that started it, not the core process. The host starts workers on a
//! runtime blocking thread, which the runtime ends when it is idle. So the
//! worker is started from its own thread, which lives until the core drops
//! the worker ([`spawn_kept`]).

use super::credentials::{other_sfu_alert, prompt, question_alert, yes};
use crate::calls::worker::{CameraPath, StartError, WorkerAudio, WorkerExit, WorkerProcess};
use crate::platform::call_host::{
    ContainedCallHost as Host, DIRECT_CONNECTION_TEXT, Launched, Launcher, other_sfu_text,
};
use crate::platform::unix_call::launched;
use persephone_media_broker::{CallCamera, CallSound, SeccompContainment, start_call_worker};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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

/// Runs `start` on a new thread and keeps that thread alive until the
/// returned sender is dropped. A failed start ends the thread at once. The
/// thread's handle is returned for tests; the launcher does not join it.
fn spawn_kept<T: Send + 'static>(
    start: impl FnOnce() -> Result<T, StartError> + Send + 'static,
) -> Result<(T, mpsc::Sender<()>, thread::JoinHandle<()>), StartError> {
    let (started_tx, started_rx) = mpsc::channel();
    let (keep_tx, keep_rx) = mpsc::channel::<()>();
    let thread = thread::Builder::new()
        .name("call-worker-parent".to_owned())
        .spawn(move || {
            let started = start();
            let ok = started.is_ok();
            if started_tx.send(started).is_err() || !ok {
                return;
            }
            // Nothing is ever sent: this ends when the sender is dropped.
            let _ = keep_rx.recv();
        })
        .map_err(|_| StartError::Launch)?;
    let started = started_rx.recv().map_err(|_| StartError::Launch)??;
    Ok((started, keep_tx, thread))
}

/// A worker process and the handle that keeps its parent thread alive. The
/// process is dropped first, then the parent thread ends.
struct KeptParent {
    process: Box<dyn WorkerProcess>,
    _parent: mpsc::Sender<()>,
}

impl WorkerProcess for KeptParent {
    fn kill(&self) {
        self.process.kill();
    }

    fn wait_exit(&self, timeout: Duration) -> bool {
        self.process.wait_exit(timeout)
    }

    fn exit_status(&self) -> Option<WorkerExit> {
        self.process.exit_status()
    }

    fn confirm_contained(&self) -> bool {
        self.process.confirm_contained()
    }
}

impl Launcher for LinuxLauncher {
    fn launch(
        &self,
        exe: &Path,
        camera: CameraPath,
        audio: &WorkerAudio,
    ) -> Result<Launched, StartError> {
        let exe = exe.to_owned();
        let argument = camera_argument(camera);
        // The broker checks the names again before they go into the
        // worker's environment (ADR-0049).
        let sound = CallSound {
            source: audio.source.clone(),
            sink: audio.sink.clone(),
        };
        let (started, parent, _thread) = spawn_kept(move || {
            let profile = SeccompContainment::create_or_open("").map_err(|_| StartError::Launch)?;
            start_call_worker(&profile, &exe, argument, &sound).map_err(|_| StartError::Launch)
        })?;
        let mut worker = launched(started);
        worker.process = Box::new(KeptParent {
            process: worker.process,
            _parent: parent,
        });
        Ok(worker)
    }

    fn camera_path(&self) -> CameraPath {
        (self.camera_path)()
    }

    /// The ScreenCast portal, behind the core's own warning window.
    fn screen_source(&self) -> std::sync::Arc<dyn crate::calls::screen::ScreenSource> {
        std::sync::Arc::new(super::screen_portal::PortalScreen)
    }

    /// The same question as on Windows, in the core's own GTK window; "No"
    /// by default. No display, or a window that did not run, is "no".
    fn confirm_direct_connection(&self) -> bool {
        prompt(question_alert("Persephone", DIRECT_CONNECTION_TEXT))
            .and_then(|answer| yes(answer.response))
            .unwrap_or(false)
    }

    /// The question about another SFU (ADR-0054), with the buttons "Not now"
    /// (the default) and "Connect". No display, or a window that did not run,
    /// is "not now".
    fn confirm_other_sfu(&self, host: &str, proxy_configured: bool) -> bool {
        prompt(other_sfu_alert(&other_sfu_text(
            host,
            proxy_configured,
            true,
        )))
        .and_then(|answer| yes(answer.response))
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN a start through `spawn_kept` WHEN its handle is kept THEN the
    /// starting thread stays alive; WHEN the handle is dropped THEN the
    /// thread ends.
    #[test]
    fn given_a_kept_start_then_its_thread_lives_until_the_handle_drops() {
        let (name, parent, thread) =
            spawn_kept(|| Ok(thread::current().name().map(str::to_owned))).expect("started");
        assert_eq!(name.as_deref(), Some("call-worker-parent"));
        thread::sleep(Duration::from_millis(50));
        assert!(!thread.is_finished(), "kept while the handle lives");
        drop(parent);
        thread.join().expect("the thread ends");
    }

    /// GIVEN a start that fails WHEN started through `spawn_kept` THEN the
    /// error comes back.
    #[test]
    fn given_a_failed_start_then_the_error_comes_back() {
        let started = spawn_kept(|| Err::<(), _>(StartError::Launch));
        assert!(matches!(started, Err(StartError::Launch)));
    }

    /// GIVEN each camera path WHEN mapped for the broker THEN the worker
    /// gets the matching argument.
    #[test]
    fn given_camera_paths_then_each_maps_to_its_broker_mode() {
        assert_eq!(camera_argument(CameraPath::Portal), CallCamera::Portal);
        assert_eq!(camera_argument(CameraPath::V4l2), CallCamera::V4l2);
        assert_eq!(camera_argument(CameraPath::None), CallCamera::None);
    }

    /// The call worker that `cargo build -p persephone-call-worker` put in
    /// this test binary's profile directory.
    fn built_worker() -> std::path::PathBuf {
        let exe = std::env::current_exe().expect("this test binary exists");
        let profile = exe
            .parent()
            .and_then(Path::parent)
            .expect("target/<profile>/deps");
        let worker = profile.join("persephone-call-worker");
        assert!(
            worker.is_file(),
            "build the worker first: cargo build -p persephone-call-worker"
        );
        worker
    }

    /// Runs one sound test in the contained worker, sends `Leave` once the
    /// device runs, and returns the worker's messages up to `Left` or
    /// `Failed` (at most 10 seconds), without diagnostic records.
    fn run_sound_test(
        audio: &WorkerAudio,
        test: persephone_call_protocol::DeviceTest,
        device: Option<&str>,
    ) -> Vec<persephone_call_protocol::FromWorker> {
        use crate::calls::worker::CallHost;
        use persephone_call_protocol::{FromWorker, ToWorker};
        let host = ContainedCallHost::with_launcher(
            built_worker(),
            LinuxLauncher {
                camera_path: || CameraPath::None,
            },
        );
        let mut link = host
            .start(audio)
            .expect("the contained worker reports Ready with bits 0");
        link.send(ToWorker::TestDevice {
            test,
            device: device.map(str::to_owned),
        })
        .expect("the worker reads the test");
        let mut messages = Vec::new();
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < until {
            match link.events.try_recv() {
                Ok(FromWorker::Diagnostic { .. }) => {}
                Ok(message) => {
                    let end = matches!(message, FromWorker::Left | FromWorker::Failed { .. });
                    if message == FromWorker::Connected {
                        let _ = link.send(ToWorker::Leave);
                    }
                    messages.push(message);
                    if end {
                        break;
                    }
                }
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
        link.finish(Duration::from_secs(5));
        messages
    }

    /// GIVEN the built worker and a PulseAudio server (also `pipewire-pulse`)
    /// with its default sink and source WHEN the microphone and speaker are
    /// tested in the contained worker (seccomp, Landlock, both stages) THEN
    /// each device runs and the test ends with `Left`. GIVEN the default
    /// sink chosen by its `pulse:` name THEN it runs too; GIVEN a sink name
    /// the server does not have THEN the test fails with `DeviceNotFound`,
    /// and nothing plays on another sink. Plays a short tone and records
    /// from the default source for a moment. Run with
    /// `cargo build -p persephone-call-worker` and then
    /// `cargo test --locked -p persephone-core --lib platform::linux::call -- --ignored sound`.
    #[test]
    #[ignore = "needs the built call worker and a PulseAudio or pipewire-pulse server"]
    fn given_the_sound_server_when_devices_are_tested_in_the_container_then_they_run() {
        use persephone_call_protocol::{DeviceTest, FailureCode, FromWorker};
        let ran = |messages: &[FromWorker]| {
            messages.first() == Some(&FromWorker::Connected)
                && messages.last() == Some(&FromWorker::Left)
        };
        let microphone = run_sound_test(&WorkerAudio::default(), DeviceTest::Microphone, None);
        assert!(ran(&microphone), "{microphone:?}");
        let speaker = run_sound_test(&WorkerAudio::default(), DeviceTest::Speaker, None);
        assert!(ran(&speaker), "{speaker:?}");

        let sinks = crate::calls::devices::list(
            &ContainedCallHost::with_launcher(
                built_worker(),
                LinuxLauncher {
                    camera_path: || CameraPath::None,
                },
            ),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .expect("the worker lists the devices")
        .playout;
        let named = sinks
            .first()
            .expect("the sound server lists a sink")
            .id
            .clone();
        let chosen = WorkerAudio::for_choice(None, Some(&named));
        let speaker = run_sound_test(&chosen, DeviceTest::Speaker, Some(&named));
        assert!(ran(&speaker), "{named}: {speaker:?}");

        let missing = "pulse:persephone_no_such_sink";
        let absent = WorkerAudio::for_choice(None, Some(missing));
        assert_eq!(
            run_sound_test(&absent, DeviceTest::Speaker, Some(missing)),
            vec![FromWorker::Failed {
                code: FailureCode::DeviceNotFound
            }]
        );
    }
}

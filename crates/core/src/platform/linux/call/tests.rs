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

use super::*;
use crate::calls::worker::{CallHost, CallWorkerLink};
use persephone_call_protocol::{DeviceTest, FailureCode, FromWorker, ToWorker};
use std::time::Instant;

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

/// A broker for the built worker, as the core starts it, with no camera.
fn contained_host() -> ContainedCallHost {
    ContainedCallHost::with_launcher(
        built_worker(),
        LinuxLauncher {
            camera_path: || CameraPath::None,
        },
    )
}

/// Starts one device test in a contained worker.
fn start_sound_test(audio: &WorkerAudio, test: DeviceTest, device: Option<&str>) -> CallWorkerLink {
    let link = contained_host()
        .start(audio)
        .expect("the contained worker reports Ready with bits 0");
    link.send(ToWorker::TestDevice {
        test,
        device: device.map(str::to_owned),
    })
    .expect("the worker reads the test");
    link
}

/// The worker's next message other than a diagnostic record, or `None`
/// when none comes before `until`.
fn next_message(link: &mut CallWorkerLink, until: Instant) -> Option<FromWorker> {
    while Instant::now() < until {
        match link.events.try_recv() {
            Ok(FromWorker::Diagnostic { .. }) => {}
            Ok(message) => return Some(message),
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
    None
}

fn is_end(message: &FromWorker) -> bool {
    matches!(message, FromWorker::Left | FromWorker::Failed { .. })
}

/// Reads messages into `messages` up to `Left` or `Failed`, or until
/// `until`. Sends `Leave` after each message for which `leave` is true.
fn read_to_end(
    link: &mut CallWorkerLink,
    messages: &mut Vec<FromWorker>,
    until: Instant,
    mut leave: impl FnMut(&FromWorker) -> bool,
) {
    while let Some(message) = next_message(link, until) {
        if leave(&message) {
            let _ = link.send(ToWorker::Leave);
        }
        let end = is_end(&message);
        messages.push(message);
        if end {
            return;
        }
    }
}

/// Runs one device test to its end (at most 10 seconds), with `Leave`
/// after each message for which `leave` is true.
fn run_sound_test(
    audio: &WorkerAudio,
    test: DeviceTest,
    device: Option<&str>,
    leave: impl FnMut(&FromWorker) -> bool,
) -> Vec<FromWorker> {
    let mut link = start_sound_test(audio, test, device);
    let mut messages = Vec::new();
    let until = Instant::now() + Duration::from_secs(10);
    read_to_end(&mut link, &mut messages, until, leave);
    link.finish(Duration::from_secs(5));
    messages
}

/// Tests the default microphone while a second worker runs the speaker
/// test on `sink` (the default sink when `None`) to its own end. Returns
/// the microphone's messages and the speaker's messages.
fn microphone_during_tone(sink: Option<&str>) -> (Vec<FromWorker>, Vec<FromWorker>) {
    let until = Instant::now() + Duration::from_secs(15);
    let mut microphone = start_sound_test(&WorkerAudio::default(), DeviceTest::Microphone, None);
    let mut heard = Vec::new();
    while let Some(message) = next_message(&mut microphone, until) {
        let running = message == FromWorker::Connected;
        let end = is_end(&message);
        heard.push(message);
        if running || end {
            break;
        }
    }
    let speaker = thread::scope(|scope| {
        let tone = scope.spawn(|| {
            run_sound_test(
                &WorkerAudio::for_choice(None, sink),
                DeviceTest::Speaker,
                sink,
                |_| false,
            )
        });
        while !tone.is_finished() {
            let soon = Instant::now() + Duration::from_millis(50);
            if let Some(message) = next_message(&mut microphone, soon) {
                heard.push(message);
            }
        }
        tone.join().expect("the speaker test thread ends")
    });
    // The last levels of the tone, then the stop.
    let after = Instant::now() + Duration::from_millis(300);
    while let Some(message) = next_message(&mut microphone, after) {
        heard.push(message);
    }
    let _ = microphone.send(ToWorker::Leave);
    read_to_end(&mut microphone, &mut heard, until, |_| false);
    microphone.finish(Duration::from_secs(5));
    (heard, speaker)
}

/// GIVEN the built worker and a PulseAudio server (also `pipewire-pulse`)
/// whose default source is the monitor of its default sink, and a second
/// sink named in `PERSEPHONE_TEST_OTHER_SINK` as a `pulse:` ID, WHEN the
/// devices are tested in the contained worker (seccomp, Landlock, both
/// stages) THEN:
///
/// * the speaker test stopped at once ends with `Left` (the stop path);
/// * the speaker test on the default sink runs to its own `Left`, and the
///   microphone test that runs meanwhile reports a level above 0, so the
///   tone played and the microphone read it;
/// * the speaker test on the other sink, chosen by its `pulse:` ID, runs
///   to its own `Left`, and the microphone reports no level above 0, so
///   the tone went to the chosen sink and not to the default sink;
/// * a sink name the server does not have fails with `DeviceNotFound`.
///
/// For example, on a headless `pipewire-pulse` (its `auto_null` goes away
/// when a real sink loads):
/// `pactl load-module module-null-sink sink_name=persephone_main`,
/// `pactl load-module module-null-sink sink_name=persephone_other`,
/// `pactl set-default-sink persephone_main`,
/// `pactl set-default-source persephone_main.monitor`,
/// `cargo build -p persephone-call-worker`, then
/// `PERSEPHONE_TEST_OTHER_SINK=pulse:persephone_other cargo test --locked -p persephone-core --lib platform::linux::call -- --ignored sound`.
#[test]
#[ignore = "needs the built call worker, a PulseAudio or pipewire-pulse server and a second sink"]
fn given_the_sound_server_when_devices_are_tested_in_the_container_then_they_run() {
    let ran = |messages: &[FromWorker]| {
        messages.first() == Some(&FromWorker::Connected)
            && messages.last() == Some(&FromWorker::Left)
    };
    let loud = |messages: &[FromWorker]| {
        messages
            .iter()
            .any(|message| matches!(message, FromWorker::SoundLevel { level } if *level > 0))
    };

    let stopped = run_sound_test(
        &WorkerAudio::default(),
        DeviceTest::Speaker,
        None,
        |message| *message == FromWorker::Connected,
    );
    assert!(ran(&stopped), "{stopped:?}");

    let (heard, speaker) = microphone_during_tone(None);
    assert!(ran(&speaker), "default sink: {speaker:?}");
    assert!(ran(&heard), "microphone: {heard:?}");
    assert!(
        loud(&heard),
        "the microphone heard the default sink: {heard:?}"
    );

    let other = std::env::var("PERSEPHONE_TEST_OTHER_SINK")
        .expect("PERSEPHONE_TEST_OTHER_SINK names a second sink as pulse:<name>");
    let sinks = crate::calls::devices::list(
        &contained_host(),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .expect("the worker lists the devices")
    .playout;
    assert!(
        sinks.iter().any(|sink| sink.id == other),
        "{other} is listed: {sinks:?}"
    );
    let (heard, speaker) = microphone_during_tone(Some(&other));
    assert!(ran(&speaker), "{other}: {speaker:?}");
    assert!(ran(&heard), "microphone: {heard:?}");
    assert!(
        !loud(&heard),
        "the tone on {other} did not reach the default sink: {heard:?}"
    );

    let missing = "pulse:persephone_no_such_sink";
    assert_eq!(
        run_sound_test(
            &WorkerAudio::for_choice(None, Some(missing)),
            DeviceTest::Speaker,
            Some(missing),
            |_| false,
        ),
        vec![FromWorker::Failed {
            code: FailureCode::DeviceNotFound
        }]
    );
}

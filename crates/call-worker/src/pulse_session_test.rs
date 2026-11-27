//! A live test of the call's sound server connection (ADR-0049, amendment of
//! 27 November 2026). Linux only, ignored: it needs a PulseAudio or
//! `pipewire-pulse` server with two speakers and two microphones that are
//! not monitors, and the tools `pactl`, `pacat` and `parec`.
//!
//! Use a disposable sound server, never the desktop one. The setup changes
//! the server's default sink and source and loads modules, and the test
//! moves streams. Start a separate server under a temporary runtime
//! directory (for example `pulseaudio -n --daemonize=yes --exit-idle-time=-1
//! -L module-native-protocol-unix`, or `pipewire` with `pipewire-pulse`),
//! and point the test and `pactl` at it with `XDG_RUNTIME_DIR` and
//! `PULSE_SERVER`. The teardown must run also when the test fails, so set a
//! `trap` before the first change.
//!
//! The test binary starts itself as a child. The child plays the call
//! worker: it opens its own record and playback streams, opens the
//! [`Session`], applies the real containment (stage 1 and stage 2, so no
//! new socket after the connection) and then moves its streams when the
//! parent asks. The parent checks the result from outside with `pactl` and
//! also keeps streams of other programs to show that they never move.
//!
//! Run the setup in a subshell, so the parent shell keeps its environment.
//! `PULSE_RUNTIME_PATH` points the daemon, `pactl` and `pulseaudio --kill`
//! at the private `pulse` directory of the temporary runtime directory, so
//! they never reach the desktop's server. `PULSE_SINK` and `PULSE_SOURCE`
//! are unset, as a desktop device name there would stop the test's streams
//! from opening on the private server; the test also removes them from
//! every program it starts.
//!
//! ```text
//! (
//!   export XDG_RUNTIME_DIR="$(mktemp -d)" && chmod 700 "$XDG_RUNTIME_DIR"
//!   export PULSE_RUNTIME_PATH="$XDG_RUNTIME_DIR/pulse"
//!   pulseaudio -n --daemonize=yes --exit-idle-time=-1 -L module-native-protocol-unix
//!   export PULSE_SERVER="unix:$PULSE_RUNTIME_PATH/native"
//!   unset PULSE_SINK PULSE_SOURCE
//!   ORIG_SINK="$(pactl get-default-sink)"; ORIG_SOURCE="$(pactl get-default-source)"
//!   MODULES=""
//!   cleanup() {
//!     pactl set-default-sink "$ORIG_SINK"; pactl set-default-source "$ORIG_SOURCE"
//!     for id in $MODULES; do pactl unload-module "$id"; done
//!     pulseaudio --kill; rm -rf "$XDG_RUNTIME_DIR"
//!   }
//!   trap cleanup EXIT
//!   MODULES="$MODULES $(pactl load-module module-null-sink sink_name=persephone_main)"
//!   MODULES="$MODULES $(pactl load-module module-null-sink sink_name=persephone_other)"
//!   MODULES="$MODULES $(pactl load-module module-remap-source source_name=persephone_mic_a master=persephone_main.monitor)"
//!   MODULES="$MODULES $(pactl load-module module-remap-source source_name=persephone_mic_b master=persephone_other.monitor)"
//!   pactl set-default-sink persephone_main
//!   pactl set-default-source persephone_mic_a
//!   PERSEPHONE_TEST_OTHER_SINK=pulse:persephone_other PERSEPHONE_TEST_OTHER_SOURCE=pulse:persephone_mic_b \
//!     cargo test --locked -p persephone-call-worker -- --ignored --exact pulse_session_test::given_a_call_session
//! )
//! ```
//!
//! The `trap` restores the defaults, unloads the null and remap modules and
//! stops the temporary server when the subshell ends, whether the test
//! passed or failed.

use std::ffi::{c_char, c_int, c_void};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use crate::pulse::{Session, library, symbol};

const CHILD_FLAG: &str = "PERSEPHONE_SESSION_CHILD";
const CHILD_TEST: &str = "pulse_session_test::the_call_worker_side";

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

type SimpleNew = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    c_int,
    *const c_char,
    *const c_char,
    *const SampleSpec,
    *const c_void,
    *const c_void,
    *mut c_int,
) -> *mut c_void;

/// Opens one `pa_simple` stream (1 playback, 2 record) in this process.
fn open_stream(direction: c_int) -> *mut c_void {
    // SAFETY: a fixed library name; the handle is never closed.
    let handle = unsafe {
        libc::dlopen(
            c"libpulse-simple.so.0".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        )
    };
    assert!(!handle.is_null(), "libpulse-simple.so.0");
    // SAFETY: the C signature of pa_simple_new.
    let new: SimpleNew = unsafe { symbol(handle, c"pa_simple_new").expect("pa_simple_new") };
    let spec = SampleSpec {
        format: 3, // PA_SAMPLE_S16LE
        rate: 48_000,
        channels: 1,
    };
    let mut error = 0;
    // SAFETY: valid C strings and a valid spec; null for the defaults.
    let stream = unsafe {
        new(
            std::ptr::null(),
            c"Persephone session test".as_ptr(),
            direction,
            std::ptr::null(),
            c"test".as_ptr(),
            &spec,
            std::ptr::null(),
            std::ptr::null(),
            &mut error,
        )
    };
    assert!(!stream.is_null(), "pa_simple_new failed: {error}");
    stream
}

/// The child: one `SWITCH <microphone|-> <speaker|->` line in, one `OK` or
/// `ERR <code>` line out.
#[test]
#[ignore = "started by the parent test"]
fn the_call_worker_side() {
    let Some(mode) = std::env::var_os(CHILD_FLAG) else {
        return;
    };
    // "late": no playback stream yet (nobody else is in the call), and only
    // stage 1, because a stream opened later needs a new socket.
    let late = mode == "late";
    // The same order as the worker: stage 1 first (before any input), then
    // the sound server connections, then stage 2.
    // SAFETY: plain prctl, as the launcher does.
    unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }
    assert_eq!(crate::containment::missing_controls(), 0, "stage 1");
    let _record = open_stream(2);
    let mut _playback = (!late).then(|| open_stream(1));
    assert!(library().is_some());
    let session = Session::open().expect("the session opens");
    if !late {
        assert!(crate::containment::apply_stage2(), "stage 2");
    }
    let mut out = std::io::stdout();
    writeln!(out, "READY").unwrap();
    out.flush().unwrap();
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let mut words = line.split_whitespace();
        match words.next() {
            Some("OPEN_PLAYBACK") => {
                _playback = Some(open_stream(1));
                writeln!(out, "OK").unwrap();
                out.flush().unwrap();
                continue;
            }
            Some("APPLY") => {
                let run =
                    session.apply_pending(std::time::Instant::now(), false, session.generation());
                writeln!(
                    out,
                    "OK waiting={} failed={:?}",
                    run.waiting, run.moves_failed
                )
                .unwrap();
                out.flush().unwrap();
                continue;
            }
            Some("SWITCH") => {}
            other => panic!("unknown command {other:?}"),
        }
        let choice = |word: Option<&str>| word.filter(|word| *word != "-").map(str::to_owned);
        let (recording, playout) = (choice(words.next()), choice(words.next()));
        match session.switch(
            std::time::Instant::now(),
            recording.as_deref(),
            playout.as_deref(),
        ) {
            Ok(()) => writeln!(out, "OK").unwrap(),
            Err(code) => writeln!(out, "ERR {code:?}").unwrap(),
        }
        out.flush().unwrap();
    }
}

/// A program that the test started. It is stopped and waited for when it
/// goes out of scope, also after a panic.
struct Reaper(Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts the child in `mode` (`"1"` or `"late"`) and waits until it is
/// ready.
fn start_child(mode: &str) -> (Child, BufReader<std::process::ChildStdout>) {
    let mut child = command(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            CHILD_TEST,
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_FLAG, mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the child starts");
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    // The harness prints its own lines before ours.
    let mut line = String::new();
    while !line.trim().ends_with("READY") {
        line.clear();
        assert!(reader.read_line(&mut line).unwrap() > 0, "the child ended");
    }
    (child, reader)
}

/// `(process ID, device name)` of each stream that `pactl list <kind>`
/// shows; kind is `sink-inputs` or `source-outputs`.
fn streams(kind: &str) -> Vec<(u32, String)> {
    let (devices, key) = if kind == "sink-inputs" {
        ("sinks", "Sink:")
    } else {
        ("sources", "Source:")
    };
    let names: Vec<(String, String)> = pactl(&["list", "short", devices])
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            Some((fields.next()?.to_owned(), fields.next()?.to_owned()))
        })
        .collect();
    let mut found = Vec::new();
    let mut device = String::new();
    for line in pactl(&["list", kind]).lines() {
        let line = line.trim();
        if let Some(index) = line.strip_prefix(key) {
            let index = index.trim();
            device = names
                .iter()
                .find(|(i, _)| i == index)
                .map(|(_, name)| name.clone())
                .unwrap_or_default();
        } else if let Some(pid) = line.strip_prefix("application.process.id = ") {
            found.push((pid.trim_matches('"').parse().unwrap(), device.clone()));
        }
    }
    found
}

/// A command for `program` with no inherited device override, so its
/// streams open on the private server's defaults.
fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env_remove("PULSE_SINK").env_remove("PULSE_SOURCE");
    command
}

fn pactl(args: &[&str]) -> String {
    let output = command("pactl").args(args).output().expect("pactl");
    assert!(output.status.success(), "pactl {args:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn on(kind: &str, pid: u32) -> Vec<String> {
    streams(kind)
        .into_iter()
        .filter(|(owner, _)| *owner == pid)
        .map(|(_, device)| device)
        .collect()
}

fn ask(child: &mut Child, reader: &mut impl BufRead, line: &str) -> String {
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{line}").unwrap();
    stdin.flush().unwrap();
    let mut answer = String::new();
    reader.read_line(&mut answer).unwrap();
    answer.trim().to_owned()
}

/// GIVEN a sound server with two speakers and two microphones, a child that
/// plays the contained call worker with one record and one playback stream,
/// and a program of another process on the defaults, WHEN the child is told
/// to change devices after both containment stages, THEN only the child's
/// own streams move: to the chosen speaker and microphone, back to the
/// defaults at call start for no choice, and not at all for a missing
/// device or a name that is not a `pulse:` ID; the other program's streams
/// never move.
#[test]
#[ignore = "needs a sound server with two speakers and two microphones, pactl, pacat and parec"]
fn given_a_call_session() {
    if std::env::var_os(CHILD_FLAG).is_some() {
        return;
    }
    let other_sink =
        std::env::var("PERSEPHONE_TEST_OTHER_SINK").expect("PERSEPHONE_TEST_OTHER_SINK");
    let other_source =
        std::env::var("PERSEPHONE_TEST_OTHER_SOURCE").expect("PERSEPHONE_TEST_OTHER_SOURCE");
    let sink = other_sink.strip_prefix("pulse:").unwrap();
    let source = other_source.strip_prefix("pulse:").unwrap();
    let default_sink = pactl(&["get-default-sink"]).trim().to_owned();
    let default_source = pactl(&["get-default-source"]).trim().to_owned();

    // Each program goes under its guard at once, so a panic or a failed
    // start of the next one still stops it.
    let playing = Reaper(
        command("pacat")
            .args(["--playback", "--raw"])
            .stdin(Stdio::from(std::fs::File::open("/dev/zero").unwrap()))
            .spawn()
            .expect("pacat"),
    );
    let recording = Reaper(
        command("parec")
            .arg("--raw")
            .stdout(Stdio::null())
            .spawn()
            .expect("parec"),
    );
    let (mut child, mut reader) = start_child("1");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let pid = child.id();
    let (playing_pid, recording_pid) = (playing.0.id(), recording.0.id());

    let check = |sinks: &str, sources: &str, note: &str| {
        assert_eq!(on("sink-inputs", pid), [sinks], "own playback {note}");
        assert_eq!(on("source-outputs", pid), [sources], "own record {note}");
        assert_eq!(
            on("sink-inputs", playing_pid),
            [default_sink.as_str()],
            "other playback {note}"
        );
        assert_eq!(
            on("source-outputs", recording_pid),
            [default_source.as_str()],
            "other record {note}"
        );
    };
    check(&default_sink, &default_source, "at the start");

    let reply = ask(
        &mut child,
        &mut reader,
        &format!("SWITCH {other_source} {other_sink}"),
    );
    assert_eq!(reply, "OK");
    check(sink, source, "after the change");

    // A name that is not a pulse: ID, and a missing device: nothing moves.
    for bad in ["SWITCH - index:0", "SWITCH pulse:persephone_missing -"] {
        let reply = ask(&mut child, &mut reader, bad);
        assert_eq!(reply, "ERR DeviceNotFound", "{bad}");
        check(sink, source, bad);
    }
    // A good microphone is not moved when the speaker choice is bad. The
    // microphone asked for is the start default, not the one in use, so an
    // early microphone move shows.
    let reply = ask(
        &mut child,
        &mut reader,
        &format!("SWITCH pulse:{default_source} pulse:persephone_missing"),
    );
    assert_eq!(reply, "ERR DeviceNotFound");
    check(sink, source, "a bad speaker beside a good microphone");

    // No choice: the defaults when the call started.
    let reply = ask(&mut child, &mut reader, "SWITCH - -");
    assert_eq!(reply, "OK");
    check(&default_sink, &default_source, "back to the defaults");

    drop(child.stdin.take());
    let status = child.wait().unwrap();
    assert!(status.success(), "the child ended well: {status:?}");
}

/// GIVEN a call with no playback stream yet and a new speaker, WHEN the
/// change is made, THEN it answers done and the stream does not exist; WHEN
/// the stream opens (on the default speaker) and the waiting target is
/// applied, THEN the stream moves to the chosen speaker. The microphone
/// moves at once. Stage 1 only: a stream that opens later needs a socket.
#[test]
#[ignore = "needs a sound server with two speakers and two microphones, pactl"]
fn given_a_speaker_that_waits_for_its_stream() {
    if std::env::var_os(CHILD_FLAG).is_some() {
        return;
    }
    let other_sink =
        std::env::var("PERSEPHONE_TEST_OTHER_SINK").expect("PERSEPHONE_TEST_OTHER_SINK");
    let other_source =
        std::env::var("PERSEPHONE_TEST_OTHER_SOURCE").expect("PERSEPHONE_TEST_OTHER_SOURCE");
    let sink = other_sink.strip_prefix("pulse:").unwrap();
    let source = other_source.strip_prefix("pulse:").unwrap();
    let default_sink = pactl(&["get-default-sink"]).trim().to_owned();
    let (mut child, mut reader) = start_child("late");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let pid = child.id();
    assert!(on("sink-inputs", pid).is_empty(), "no playback stream yet");

    let reply = ask(
        &mut child,
        &mut reader,
        &format!("SWITCH {other_source} {other_sink}"),
    );
    assert_eq!(reply, "OK", "a missing stream is not a failure");
    assert_eq!(on("source-outputs", pid), [source], "the microphone moved");
    let reply = ask(&mut child, &mut reader, "APPLY");
    assert_eq!(reply, "OK waiting=true failed=[]", "still no stream");

    assert_eq!(ask(&mut child, &mut reader, "OPEN_PLAYBACK"), "OK");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(on("sink-inputs", pid), [default_sink.as_str()]);
    let reply = ask(&mut child, &mut reader, "APPLY");
    assert_eq!(reply, "OK waiting=false failed=[]");
    assert_eq!(on("sink-inputs", pid), [sink], "the stream moved");
    // Nothing waits any more.
    let reply = ask(&mut child, &mut reader, "APPLY");
    assert_eq!(reply, "OK waiting=false failed=[]");

    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

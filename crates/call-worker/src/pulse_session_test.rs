//! A live test of the call's sound server connection (ADR-0049, amendment of
//! 27 November 2026). Linux only, ignored: it needs a PulseAudio or
//! `pipewire-pulse` server with two speakers and two microphones that are
//! not monitors, and the tools `pactl`, `pacat` and `parec`.
//!
//! The test binary starts itself as a child. The child plays the call
//! worker: it opens its own record and playback streams, opens the
//! [`Session`], applies the real containment (stage 1 and stage 2, so no
//! new socket after the connection) and then moves its streams when the
//! parent asks. The parent checks the result from outside with `pactl` and
//! also keeps streams of other programs to show that they never move.
//!
//! ```text
//! pactl load-module module-null-sink sink_name=persephone_main
//! pactl load-module module-null-sink sink_name=persephone_other
//! pactl load-module module-remap-source source_name=persephone_mic_a master=persephone_main.monitor
//! pactl load-module module-remap-source source_name=persephone_mic_b master=persephone_other.monitor
//! pactl set-default-sink persephone_main
//! pactl set-default-source persephone_mic_a
//! PERSEPHONE_TEST_OTHER_SINK=pulse:persephone_other PERSEPHONE_TEST_OTHER_SOURCE=pulse:persephone_mic_b \
//!   cargo test --locked -p persephone-call-worker -- --ignored --exact pulse_session_test::given_a_call_session
//! ```

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
    if std::env::var_os(CHILD_FLAG).is_none() {
        return;
    }
    let _record = open_stream(2);
    let _playback = open_stream(1);
    assert!(library().is_some());
    // The same order as the worker: the connection first, then containment.
    let session = Session::open(None, None).expect("the session opens");
    // SAFETY: plain prctl, as the launcher does.
    unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }
    assert_eq!(crate::containment::missing_controls(), 0, "stage 1");
    assert!(crate::containment::apply_stage2(), "stage 2");
    let mut out = std::io::stdout();
    writeln!(out, "READY").unwrap();
    out.flush().unwrap();
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let mut words = line.split_whitespace();
        assert_eq!(words.next(), Some("SWITCH"));
        let choice = |word: Option<&str>| word.filter(|word| *word != "-").map(str::to_owned);
        let (recording, playout) = (choice(words.next()), choice(words.next()));
        match session.switch(recording.as_deref(), playout.as_deref()) {
            Ok(()) => writeln!(out, "OK").unwrap(),
            Err(code) => writeln!(out, "ERR {code:?}").unwrap(),
        }
        out.flush().unwrap();
    }
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

fn pactl(args: &[&str]) -> String {
    let output = Command::new("pactl").args(args).output().expect("pactl");
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

    let playing = Command::new("pacat")
        .args(["--playback", "--raw"])
        .stdin(Stdio::from(std::fs::File::open("/dev/zero").unwrap()))
        .spawn()
        .expect("pacat");
    let recording = Command::new("parec")
        .arg("--raw")
        .stdout(Stdio::null())
        .spawn()
        .expect("parec");
    let mut foreign = [playing, recording];
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            CHILD_TEST,
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_FLAG, "1")
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
    std::thread::sleep(std::time::Duration::from_millis(300));
    let pid = child.id();
    let (playing_pid, recording_pid) = (foreign[0].id(), foreign[1].id());

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
    // A good microphone is not moved when the speaker choice is bad.
    let reply = ask(
        &mut child,
        &mut reader,
        &format!("SWITCH {other_source} pulse:persephone_missing"),
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
    for program in &mut foreign {
        let _ = program.kill();
        let _ = program.wait();
    }
}

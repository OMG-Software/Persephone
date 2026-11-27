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
//! worker: it opens its own record and playback streams on one connection,
//! as libwebrtc does (one client, so the claims of its process ID are one
//! client's), opens the [`Session`], applies the real containment (stage 1
//! and stage 2, so no new socket after the connection) and then moves its
//! streams when the parent asks. The parent checks the result from outside
//! with `pactl` and also keeps streams of other programs to show that they
//! never move, and one program that copies the child's process ID to show
//! that such a false claim stops every move.
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
//!     cargo test --locked -p persephone-call-worker -- --ignored --test-threads=1 pulse_session_test::given_a
//! )
//! ```
//!
//! The `trap` restores the defaults, unloads the null and remap modules and
//! stops the temporary server when the subshell ends, whether the test
//! passed or failed.

use std::ffi::{c_char, c_int, c_void};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::pulse::{Session, library, symbol};

const CHILD_FLAG: &str = "PERSEPHONE_SESSION_CHILD";
const CHILD_TEST: &str = "pulse_session_test::the_call_worker_side";

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

/// Which stream [`Connection::open`] opens.
#[derive(Clone, Copy)]
enum Direction {
    Playback,
    Record,
}

// pa_context_state_t and pa_stream_state_t.
const CONTEXT_READY: c_int = 4;
const CONTEXT_FAILED: c_int = 5;
const STREAM_READY: c_int = 2;
const STREAM_FAILED: c_int = 3;
// pa_context_flags_t: PA_CONTEXT_NOAUTOSPAWN.
const NO_AUTOSPAWN: c_int = 1;

/// The libpulse functions that the child uses for its streams.
struct Functions {
    lock: unsafe extern "C" fn(*mut c_void),
    unlock: unsafe extern "C" fn(*mut c_void),
    context_state: unsafe extern "C" fn(*const c_void) -> c_int,
    stream_new: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const SampleSpec,
        *const c_void,
    ) -> *mut c_void,
    connect_playback: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_void,
        c_int,
        *const c_void,
        *mut c_void,
    ) -> c_int,
    connect_record: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_void, c_int) -> c_int,
    stream_state: unsafe extern "C" fn(*const c_void) -> c_int,
}

/// One sound server connection of this process for its streams, as
/// libwebrtc keeps one (`pa_threaded_mainloop`, one `pa_context`): every
/// stream on it has the same client index. It stays open until the child
/// ends.
struct Connection {
    functions: Functions,
    mainloop: *mut c_void,
    context: *mut c_void,
}

impl Connection {
    /// Connects to the sound server and waits until the connection is ready.
    fn connect() -> Self {
        let handle = library().expect("libpulse.so.0");
        // SAFETY: each type is the C signature from pulse/*.h.
        let (new, get_api, start, context_new, context_connect) = unsafe {
            (
                symbol::<unsafe extern "C" fn() -> *mut c_void>(
                    handle,
                    c"pa_threaded_mainloop_new",
                )
                .unwrap(),
                symbol::<unsafe extern "C" fn(*mut c_void) -> *mut c_void>(
                    handle,
                    c"pa_threaded_mainloop_get_api",
                )
                .unwrap(),
                symbol::<unsafe extern "C" fn(*mut c_void) -> c_int>(
                    handle,
                    c"pa_threaded_mainloop_start",
                )
                .unwrap(),
                symbol::<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>(
                    handle,
                    c"pa_context_new",
                )
                .unwrap(),
                symbol::<
                    unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *const c_void) -> c_int,
                >(handle, c"pa_context_connect")
                .unwrap(),
            )
        };
        // SAFETY: as above.
        let functions = unsafe {
            Functions {
                lock: symbol(handle, c"pa_threaded_mainloop_lock").unwrap(),
                unlock: symbol(handle, c"pa_threaded_mainloop_unlock").unwrap(),
                context_state: symbol(handle, c"pa_context_get_state").unwrap(),
                stream_new: symbol(handle, c"pa_stream_new").unwrap(),
                connect_playback: symbol(handle, c"pa_stream_connect_playback").unwrap(),
                connect_record: symbol(handle, c"pa_stream_connect_record").unwrap(),
                stream_state: symbol(handle, c"pa_stream_get_state").unwrap(),
            }
        };
        // SAFETY: plain constructor.
        let mainloop = unsafe { new() };
        assert!(!mainloop.is_null(), "pa_threaded_mainloop_new");
        // SAFETY: the main loop is live and not started yet, so this thread
        // may use it without the lock; the name is a C string.
        let context =
            unsafe { context_new(get_api(mainloop), c"Persephone session test".as_ptr()) };
        assert!(!context.is_null(), "pa_context_new");
        // SAFETY: as above; no server name, so the environment chooses it.
        let connected =
            unsafe { context_connect(context, std::ptr::null(), NO_AUTOSPAWN, std::ptr::null()) };
        assert!(connected >= 0, "pa_context_connect");
        // SAFETY: the main loop is live.
        assert!(
            unsafe { start(mainloop) } >= 0,
            "pa_threaded_mainloop_start"
        );
        let connection = Self {
            functions,
            mainloop,
            context,
        };
        connection.wait_for("the connection", || {
            // SAFETY: the context is live; the caller holds the lock.
            match unsafe { (connection.functions.context_state)(connection.context) } {
                CONTEXT_READY => Some(true),
                state if state >= CONTEXT_FAILED => Some(false),
                _ => None,
            }
        });
        connection
    }

    /// Runs `state` under the main loop's lock until it gives an answer, at
    /// most 3 s. It must give `Some(true)` (ready).
    fn wait_for(&self, what: &str, state: impl Fn() -> Option<bool>) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            // SAFETY: the main loop is live and started.
            unsafe { (self.functions.lock)(self.mainloop) };
            let answer = state();
            // SAFETY: as above; this thread holds the lock.
            unsafe { (self.functions.unlock)(self.mainloop) };
            match answer {
                Some(ready) => {
                    assert!(ready, "{what} failed");
                    return;
                }
                None => assert!(Instant::now() < deadline, "{what} timed out"),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Opens one stream on the server's default device and waits until it
    /// is ready. It plays or records nothing, and stays until the child
    /// ends.
    fn open(&self, direction: Direction) {
        let spec = SampleSpec {
            format: 3, // PA_SAMPLE_S16LE
            rate: 48_000,
            channels: 1,
        };
        let f = &self.functions;
        // SAFETY: the main loop is live and started.
        unsafe { (f.lock)(self.mainloop) };
        // SAFETY: the context is ready and this thread holds the lock; the
        // name and spec are valid, and null asks for the defaults.
        let stream =
            unsafe { (f.stream_new)(self.context, c"test".as_ptr(), &spec, std::ptr::null()) };
        let connected = !stream.is_null() && {
            // SAFETY: as above; null device, attributes and volume are the
            // defaults, and no flags.
            let result = unsafe {
                match direction {
                    Direction::Playback => (f.connect_playback)(
                        stream,
                        std::ptr::null(),
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        std::ptr::null_mut(),
                    ),
                    Direction::Record => {
                        (f.connect_record)(stream, std::ptr::null(), std::ptr::null(), 0)
                    }
                }
            };
            result >= 0
        };
        // SAFETY: this thread holds the lock.
        unsafe { (f.unlock)(self.mainloop) };
        assert!(connected, "the stream does not connect");
        self.wait_for("the stream", || {
            // SAFETY: the stream is live; the caller holds the lock.
            match unsafe { (f.stream_state)(stream) } {
                STREAM_READY => Some(true),
                state if state >= STREAM_FAILED => Some(false),
                _ => None,
            }
        });
    }
}

/// The child: one `SWITCH <microphone|-> <speaker|->` line in, one `OK` or
/// `ERR <code>` line out.
#[test]
#[ignore = "started by the parent test"]
fn the_call_worker_side() {
    let Some(mode) = std::env::var_os(CHILD_FLAG) else {
        return;
    };
    // "late": no playback stream yet (nobody else is in the call). It opens
    // later on the same connection, under stage 2, as libwebrtc's does when
    // someone joins: a new stream needs no new socket.
    let late = mode == "late";
    // The same order as the worker: stage 1 first (before any input), then
    // the sound server connections, then stage 2.
    // SAFETY: plain prctl, as the launcher does.
    unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }
    assert_eq!(crate::containment::missing_controls(), 0, "stage 1");
    // Both streams on one connection, as libwebrtc opens them.
    let streams = Connection::connect();
    streams.open(Direction::Record);
    if !late {
        streams.open(Direction::Playback);
    }
    assert!(library().is_some());
    let session = Session::open().expect("the session opens");
    assert!(crate::containment::apply_stage2(), "stage 2");
    let mut out = std::io::stdout();
    writeln!(out, "READY").unwrap();
    out.flush().unwrap();
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let mut words = line.split_whitespace();
        match words.next() {
            Some("OPEN_PLAYBACK") => {
                streams.open(Direction::Playback);
                writeln!(out, "OK").unwrap();
                out.flush().unwrap();
                continue;
            }
            Some("APPLY") => {
                let run = session.apply_pending(Instant::now(), false, session.generation());
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
        match session.switch(Instant::now(), recording.as_deref(), playout.as_deref()) {
            Ok(_) => writeln!(out, "OK").unwrap(),
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
/// streams open on the private server's defaults, and the C locale, so
/// `pactl` prints the English labels (`Sink:`, `Source:`) that `streams`
/// reads.
fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command
        .env_remove("PULSE_SINK")
        .env_remove("PULSE_SOURCE")
        .env("LC_ALL", "C");
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
    std::thread::sleep(Duration::from_millis(300));
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
/// moves at once. The child runs under both stages, so the late stream
/// opens under full containment, on the connection it already has.
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
    std::thread::sleep(Duration::from_millis(300));
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
    std::thread::sleep(Duration::from_millis(300));
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

/// GIVEN a child that plays the contained call worker and another program
/// whose playback stream copies the child's process ID, WHEN the child is
/// told to change devices, THEN nothing moves, neither the child's streams
/// nor the other program's, and the answer is `DeviceKept`; WHEN the other
/// program ends and the change is asked again, THEN the child's streams move.
#[test]
#[ignore = "needs a sound server with two speakers and two microphones, pactl and pacat"]
fn given_a_false_claim_of_the_process_id() {
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
    let (mut child, mut reader) = start_child("1");
    let pid = child.id();
    // `--property` sets the client's and the stream's properties, so the
    // stream names the child's process ID from another client.
    let false_claim = Reaper(
        command("pacat")
            .args(["--playback", "--raw"])
            .arg(format!("--property=application.process.id={pid}"))
            .stdin(Stdio::from(std::fs::File::open("/dev/zero").unwrap()))
            .spawn()
            .expect("pacat"),
    );
    std::thread::sleep(Duration::from_millis(300));
    // Both playback streams name the child: its own and the false one.
    let on_defaults = [default_sink.as_str(), default_sink.as_str()];
    assert_eq!(on("sink-inputs", pid), on_defaults, "two playback claims");
    assert_eq!(on("source-outputs", pid), [default_source.as_str()]);

    let change = format!("SWITCH {other_source} {other_sink}");
    let reply = ask(&mut child, &mut reader, &change);
    assert_eq!(reply, "ERR DeviceKept", "a false claim stops the change");
    assert_eq!(on("sink-inputs", pid), on_defaults, "no playback moved");
    assert_eq!(on("source-outputs", pid), [default_source.as_str()]);

    drop(false_claim);
    std::thread::sleep(Duration::from_millis(300));
    let reply = ask(&mut child, &mut reader, &change);
    assert_eq!(reply, "OK", "one client's claims move");
    assert_eq!(on("sink-inputs", pid), [sink]);
    assert_eq!(on("source-outputs", pid), [source]);

    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

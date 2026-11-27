# Device tests on Linux and macOS

Checked 24 November 2026. Decision:
[ADR-0059](../adr/0059-test-call-devices-show-each-participants-level-and-switch-devices-during-a-call.md),
amendment of 24 November 2026. Source: branch `claude/device-tests-linux-macos`
(pull request 98) at commit `812ba0dee0693898ed01af1decd6266cd39e8a84`,
`--locked`. The run covers that snapshot only. `Cargo.lock` SHA-256:
`cd6f78ba4e2b5e67d47009c6c1fef8e24e444e008ed808126d26ba9f13eec525`
(unchanged by the change). Debug builds, rustc 1.99.1.

**Result: on Linux the microphone and speaker tests run in the contained call
worker against a real sound server. The tone plays to its end, the microphone
reads it, a tone on a chosen `pulse:` sink does not reach the default sink,
and a sink name the server does not have fails with `DeviceNotFound`. The
macOS code was
compiled and linted for an Apple target only; it was not linked or run on a
Mac.**

## Linux

Linux Mint 22.3, kernel 7.0.0-44-generic, PulseAudio protocol served by
PipeWire 1.0.5 (`pipewire-pulse`). No microphone hardware and no camera.
clang 21.1.8, libwebrtc `webrtc-a545de4` fetched by
`python3 tools/dev.py webrtc` (exit 0). The run script first loaded two null
sinks and set the defaults:

```sh
pactl load-module module-null-sink sink_name=persephone_main
pactl load-module module-null-sink sink_name=persephone_other
pactl set-default-sink persephone_main
pactl set-default-source persephone_main.monitor
```

`pactl info` then showed `Default Sink: persephone_main` and
`Default Source: persephone_main.monitor`, and both sinks were still listed
after the last command. `PERSEPHONE_TEST_OTHER_SINK` is shortened to `OTHER`
in the table.

| Command | Exit |
| --- | --- |
| `cargo build --locked -p persephone-call-worker` | 0 |
| `cargo test --locked -p persephone-call-worker --all-targets` (231 passed, 1 ignored) | 0 |
| `cargo clippy --locked -p persephone-call-worker --all-targets -- -D warnings` | 0 |
| `cargo test --locked -p persephone-call-worker -- --ignored default_devices` (1 passed) | 0 |
| `cargo test --locked -p persephone-core --lib device_test` (9 passed) | 0 |
| `cargo test --locked -p persephone-core --lib platform::linux::call` (3 passed, 1 ignored) | 0 |
| `OTHER=pulse:persephone_other cargo test --locked -p persephone-core --lib platform::linux::call -- --ignored sound` (1 passed) | 0 |
| Negative control: the same with `OTHER=pulse:persephone_main` (the default sink) | 101, as expected |
| `cargo clippy --locked -p persephone-core --all-targets -- -D warnings` | 0 |

The commands ran in a scratch clone checked out at the commit above; `git
status` showed no changed tracked file. `cargo fmt --all -- --check` did not
run on this machine: rustfmt is not installed for the 1.99.1 toolchain there.
It passed on Windows.

The `default_devices` test runs the test code in the test process, without
containment. The `sound` test starts each built worker through the Linux
broker, as the core does: stage 1 before any input, the sound server stream
opened, then stage 2. It checks:

* the default speaker, stopped when it runs: `Connected`, then `Left`;
* the default microphone while a second worker plays the tone on the default
  sink to its own end: the speaker gives `Connected`, then `Left` with no
  `Leave`, and the microphone reports a level above 0;
* the same with the tone on the sink in `OTHER`, chosen by its `pulse:` ID and
  passed as `PULSE_SINK`: the speaker runs to its own `Left`, and the
  microphone reports no level above 0;
* `pulse:persephone_no_such_sink`: only `Failed(DeviceNotFound)`.

The negative control names the default sink as `OTHER`. It failed at the
silence check, with microphone levels of 79 and 80 while the tone played.
This shows that the check finds a tone on the default sink, so a worker that
ignored `PULSE_SINK` would fail the test. The microphone was a monitor, not
real capture hardware. No camera test ran: the portal camera grant for a
camera test was not exercised.

The table and the list above are the run at `812ba0d` only.

### In-call sound move, 27 November 2026

A later run, for the
[ADR-0049](../adr/0049-contain-the-call-worker-on-linux-and-macos.md)
amendment of 27 November 2026 (pull request 111). Source: branch
`claude/call-av-settings` at commit
`8965b120a556e1d36b0edfc855b1d80c4827141b`, a clean clone, `--locked`.
`Cargo.lock` SHA-256:
`b3f228bb9e9570a2a51e492c7e12553cb35cf21fd52e1ba9d9a97b703be6fbae`. Linux
Mint 22.3, kernel 7.0.0-44-generic, rustc 1.99.1, clang 21.1.8, libwebrtc
`webrtc-a545de4`, debug build.

The sound server was a disposable PipeWire 1.0.5 with WirePlumber 0.4.17 and
`pipewire-pulse` (`pactl` 16.1), not the desktop's server: its own runtime,
configuration and state folders under a temporary folder, a private D-Bus
session (`dbus-run-session`), and the ALSA, V4L2, libcamera and BlueZ monitors
turned off, so it opened no device. It had two null sinks and two remapped
sources:

```sh
pactl load-module module-null-sink sink_name=persephone_main
pactl load-module module-null-sink sink_name=persephone_other
pactl load-module module-remap-source source_name=persephone_mic_a master=persephone_main.monitor
pactl load-module module-remap-source source_name=persephone_mic_b master=persephone_other.monitor
pactl set-default-sink persephone_main
pactl set-default-source persephone_mic_a
```

| Command | Exit |
| --- | --- |
| `PERSEPHONE_TEST_OTHER_SINK=pulse:persephone_other PERSEPHONE_TEST_OTHER_SOURCE=pulse:persephone_mic_b cargo test --locked -p persephone-call-worker -- --ignored --exact --test-threads=1 pulse_session_test::given_a_call_session pulse_session_test::given_a_speaker_that_waits_for_its_stream` (2 passed) | 0 |

Rerun, same day: after the second review of pull request 111 changed the
sound moves (a kind with no stream always waits, caller-started deadlines,
last-try reports, monitors known for routes), the same command passed again
(2 passed, exit 0) at commit `83afdcf652719c57ee8c752e6e4447a18ff4fc8d`, with
the same `Cargo.lock`, machine, toolchain and disposable server setup. No
server process was left.

Both tests are synthetic session tests, not calls: they run no call loop and
connect to no SFU. `given_a_call_session` starts the test binary as a child
process that plays the worker side. The child applies stage 1, opens its own
playback and record streams and the `Session`, then applies stage 2, and
calls `pulse::Session` directly on request. A `pacat` and a `parec` of
another program run on the defaults. The test checks with `pactl`:

* a change to `pulse:persephone_mic_b` and `pulse:persephone_other` moves both
  own streams, and the other program's streams stay on the defaults;
* a name that is not a `pulse:` ID, and a missing device, answer
  `DeviceNotFound`, and nothing moves;
* a missing speaker with the start default as the microphone answers
  `DeviceNotFound`, and the microphone does not move;
* no choice moves the own streams back to the defaults of the call's start.

`given_a_speaker_that_waits_for_its_stream` applies stage 1 only, because a
stream that opens later needs a new socket. A new speaker before any playback
stream answers done and moves the microphone at once. The playback stream
then opens on the default sink, and the waiting target moves it to the chosen
sink. After that, nothing waits.

Observed: the other program's `pacat` and `parec` streams stayed on their
original devices through every move. The assertions fail if the worker moves
either of them. They do not show which rule picked the own streams: a
different selector that also left out `pacat` and `parec` would pass too. No
run with a broken selector was made. The child printed "Failed to load cookie
file from cookie: Permission denied": the contained child cannot read the
cookie, and `pipewire-pulse` does not need it. No server process was left
after the run. Not covered: a change during a running call, a PulseAudio
daemon, sound hardware, a device that goes away during a call, the report
after the last retry of a waiting target, and the native containment gate.

### Earlier runs

Each earlier run was its own run on the same VM, on 24 November 2026, of the
source at its own commit. None of them certifies `812ba0d`; they show how
the `sound` test changed.

* `7586758` and `894ef32`: the commands of the table without the
  `platform::linux::call` run and the negative control, all exit 0. Their
  `sound` test was older: it stopped each run when the device started and
  did not check levels or a chosen sink.
* `586eb97`, first run: both `sound` runs exited 101 at the check that
  `OTHER` is listed. The worker listed only `pulse:auto_null`: the two
  sinks, loaded from a separate SSH session, were gone. Likely the sound
  server stopped with that session, because the user has no linger. The
  later runs load the sinks in the run script.
* `586eb97`, second run, and `907386b`: the commands of the table, with
  the same exits and counts as above. The `sound` test then waited to its
  deadline on a closed worker channel (`586eb97`), or could read a worker
  again after its last message (`907386b`).

### PulseAudio header values

The worker declares the PulseAudio constants and structs itself, because it
loads `libpulse.so.0` with `dlopen`. Each value was checked against the
headers of `libpulse-dev` 1:16.1+dfsg1-2ubuntu10.1 (Ubuntu noble), unpacked
with `apt-get download libpulse-dev` and `dpkg-deb -x`. This C file (SHA-256
`9a38ee0c92deb33b09a9d9d57c980ab608a3ef6d6e9451e0c0460f8d5191060c`) compiled
with `cc -I <unpacked>/usr/include -o check check.c` (exit 0, on the
same VM), so every assert held:

```c
#include <pulse/def.h>
#include <pulse/sample.h>
#include <pulse/context.h>
#include <pulse/stream.h>
_Static_assert(PA_CONTEXT_NOAUTOSPAWN == 1, "noautospawn");
_Static_assert(PA_CONTEXT_READY == 4 && PA_CONTEXT_FAILED == 5 && PA_CONTEXT_TERMINATED == 6, "context states");
_Static_assert(PA_STREAM_READY == 2 && PA_STREAM_FAILED == 3 && PA_STREAM_TERMINATED == 4, "stream states");
_Static_assert(PA_SAMPLE_FLOAT32LE == 5, "float32le");
_Static_assert(PA_STREAM_DONT_MOVE == 0x0200, "dont move");
_Static_assert(PA_STREAM_ADJUST_LATENCY == 0x2000, "adjust latency");
_Static_assert(PA_SEEK_RELATIVE == 0, "seek");
_Static_assert(PA_ERR_ACCESS == 1 && PA_ERR_NOENTITY == 5, "errors");
_Static_assert(sizeof(pa_sample_spec) == 12 && sizeof(pa_buffer_attr) == 20, "sizes");
int main(void) { return 0; }
```

The Rust unit test checks the struct sizes and the error mapping with
written-out numbers. It cannot read the headers, so this C check is the
source for the values.

## macOS

Not run. On the Windows development machine the macOS backend of
`crates/call-worker/src/sound_test.rs` was type-checked and linted for
`aarch64-apple-darwin` in a scratch crate with the same pinned `objc2`,
`block2` and `objc2-av-foundation` versions:
`cargo +1.99.1 clippy --offline --target aarch64-apple-darwin --all-targets -- -D warnings`
(exit 0). A deliberate error behind `cfg(target_os = "macos")` was reported,
which shows that the macOS code was compiled. Linking against Core Audio, the
microphone permission prompt, the Seatbelt profile and real devices are not
checked. The `macos-latest` CI job builds and tests the worker in the merge
queue.

## Windows

The Windows backend is unchanged apart from its entry points.
`cargo test --locked -p persephone-call-worker --all-targets` (179 passed,
1 ignored) and its Clippy passed on the first commit of the pull request
(`5538cda`). The ignored real-device test was not run, because it records
from the owner's microphone.

At `7586758` (the limit change), on Windows:
`cargo test --locked -p persephone-core --lib device_test` (9 passed),
`cargo clippy --locked -p persephone-core --all-targets -- -D warnings` and
`cargo fmt --all -- --check` exited 0. `cargo test --locked -p
persephone-core --lib calls::controller` had 121 passed and 1 failed: the
wiremock test `given_membership_failures_then_only_a_client_error_is_a_refusal`
failed in the same way without the change. Loopback HTTP tests on this
machine failed with `WSAEADDRINUSE` (10048) on connect throughout the work,
and SSH from the same machine failed in the same way. This is a local socket
fault, not a fault in the changed code.

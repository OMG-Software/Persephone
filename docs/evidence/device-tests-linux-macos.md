# Device tests on Linux and macOS

Checked 24 November 2026. Decision:
[ADR-0059](../adr/0059-test-call-devices-show-each-participants-level-and-switch-devices-during-a-call.md),
amendment of 24 November 2026. Source: branch `claude/device-tests-linux-macos`
(pull request 98) at commit `907386bc4dd97bff3eb6a27e7de7360e1d32704f`,
`--locked`. Later commits of the pull request change only comments and
documentation. `Cargo.lock` SHA-256:
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
silence check, with microphone levels of 80 while the tone played.
This shows that the check finds a tone on the default sink, so a worker that
ignored `PULSE_SINK` would fail the test. The microphone was a monitor, not
real capture hardware. No camera test ran: the portal camera grant for a
camera test was not exercised.

The same commands passed at `586eb97`, before the test failed fast on a
closed worker channel; its negative control showed levels of 79 and 80.
Earlier runs of this pull request, at `7586758` and `894ef32`, had the same
counts for the commands that they shared, with an older `sound` test. That
test stopped each run when it started and did not check levels or the
chosen sink. An earlier run of the same script found that a sink loaded
from a separate SSH session was gone when the test ran; likely the sound
server stopped with that session, because the user has no linger. The run above loaded
the sinks in the script itself.

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

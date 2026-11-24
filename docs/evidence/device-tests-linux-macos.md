# Device tests on Linux and macOS

Checked 24 November 2026. Decision:
[ADR-0059](../adr/0059-test-call-devices-show-each-participants-level-and-switch-devices-during-a-call.md),
amendment of 24 November 2026. Source: branch `claude/call-camera-diagnostics`
at commit `f5a95c3` with the uncommitted code change of this amendment
(`git diff` of the Rust sources, SHA-256 `bc49e6945b5748b3bc019062ea88b8f1cd849b29d97e73fb3fb2ba8c8a6b0f4d`),
`--locked`. `Cargo.lock` SHA-256:
`cd6f78ba4e2b5e67d47009c6c1fef8e24e444e008ed808126d26ba9f13eec525`
(unchanged by the change). Debug builds, rustc 1.99.1.

**Result: on Linux the microphone and speaker tests run in the contained call
worker against a real sound server, a chosen `pulse:` sink runs, and a sink
name the server does not have fails with `DeviceNotFound`. The macOS code was
compiled and linted for an Apple target only; it was not linked or run on a
Mac.**

## Linux

Linux Mint 22.3, kernel 7.0.0-44-generic, PulseAudio protocol served by
PipeWire 1.0.5 (`pipewire-pulse`) with its null sink `auto_null` as the only
sink and its monitor as the default source. No microphone hardware and no
camera. clang 21.1.8, libwebrtc `webrtc-a545de4` fetched by
`python3 tools/dev.py webrtc` (exit 0).

| Command | Exit |
| --- | --- |
| `cargo build --locked -p persephone-call-worker` | 0 |
| `cargo test --locked -p persephone-call-worker --all-targets` (231 passed, 1 ignored) | 0 |
| `cargo clippy --locked -p persephone-call-worker --all-targets -- -D warnings` | 0 |
| `cargo test --locked -p persephone-call-worker -- --ignored default_devices` (1 passed) | 0 |
| `cargo test --locked -p persephone-core --lib calls::device_test` (7 passed) | 0 |
| `cargo test --locked -p persephone-core --lib platform::linux::call -- --ignored sound` (1 passed) | 0 |
| `cargo clippy --locked -p persephone-core --all-targets -- -D warnings` | 0 |

`cargo fmt --all -- --check` did not run on this machine: rustfmt is not
installed for the 1.99.1 toolchain there. It passed on Windows.

The `default_devices` test runs the test code in the test process, without
containment. The `sound` test starts the built worker through the Linux
broker, as the core does: stage 1 before any input, the sound server stream
opened, then stage 2. It checks four runs:

* the default microphone: `Connected`, then `Left` after `Leave`;
* the default speaker: `Connected`, then `Left`;
* the first sink that the worker lists, chosen by its `pulse:` ID and passed
  as `PULSE_SINK`: `Connected`, then `Left`;
* `pulse:persephone_no_such_sink`: only `Failed(DeviceNotFound)`.

Because the only source was a monitor, the microphone level was not checked
for sound. No camera test ran: the portal camera grant for a camera test was
not exercised.

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
1 ignored) and its Clippy passed. The ignored real-device test was not run,
because it records from the owner's microphone. In
`cargo test -p persephone-core --lib calls::` the device test rules passed.
Between 11 and 21 loopback HTTP tests in other modules failed with
`WSAEADDRINUSE` (10048) on connect. The set of failed tests changed from run
to run, and SSH from the same machine failed in the same way. This was a
local socket fault, not a fault in the changed code.

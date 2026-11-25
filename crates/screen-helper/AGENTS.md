# Screen helper

The Windows process that captures the one monitor the user approved and writes bounded frames to the call worker (ADR-0067, proposed). The core starts one for each share through `crates/media-broker`. It runs outside the call worker's AppContainer, because Windows.Graphics.Capture is refused there.

- Keep it small. It holds no key, opens no network connection and reads no homeserver or peer data. Its only input is its command line (`persephone_call_protocol::screen_helper::HelperStart`) and the end of its control pipe. Add no dependency beyond `persephone-call-protocol` and the locked `windows` crate, and add no feature to `windows` without naming it in the manifest comment.
- It has no Win32k lockdown and no dynamic-code prohibition, because WGC and Direct3D need both. It is not contained. Do not add code that reads files, the registry, the clipboard, the network or other processes; do not add a generic command or a way to name another monitor.
- Validate the command line before use (`HelperStart::parse`): exactly seven arguments in order. Check that both inherited handles are pipes (`GetFileType`) before use; with strict handle checks, a bad value ends the process, which the core maps to "unavailable".
- Take only the monitor with the approved device name, size and rectangle (`monitor::choose`); none, another size or another rectangle is "not found". Check the monitor's place again on each loop (`wgc::monitor_place`), and end the share as "not found" when it changes. Never take another monitor and never fall back to another capture method.
- Pictures are BGRA, at most 3840x2160 pixels and 7680 on a side, at most 5 a second (`RateGate`; frames in between are dropped before the copy). A later picture of another size than the first ends the share as "not found". Rows go to the pipe without padding (`repack`) in the format of `persephone_call_protocol::screen_frame`. Never write a picture that `write_picture` refuses.
- Keep the WGC rules of `wgc.rs`: the yellow border stays on (`IsBorderRequired` is never set) and the cursor is captured. `wgc.rs` and `monitor.rs` use only `std` and the `windows` crate, because the containment gate compiles them too (`tests/platform/windows`); keep them free of other crates. The limits in `monitor.rs` and `wgc.rs` must equal the call protocol's; `run.rs` asserts it at compile time.
- Every end writes an end item (`ScreenEnd`) and exits with a code of `persephone_call_protocol::screen_helper` (0 stop, 10 not found, 11 unavailable, 12 bad start, 13 frame pipe closed). Keep the codes and the core's mapping (`state_for_exit`) in step.
- The control pipe is closed by the core to ask for the end: poll it (`PeekNamedPipe`) about every 20 ms and again before each picture is written (`send_picture`), and exit within the core's 2 s wait. Never cut a picture short: the worker reads whole items, and the end item follows the last one. A blocked write to a worker that stopped reading can only end by the core's Job terminate; do not add a thread to work around it.
- The binary has the Windows subsystem outside tests (no console: a console would need `conhost.exe`, a child process that the launch forbids). On other OSes it exits with the bad-start code and does nothing.

## Checks

- `cargo test --locked -p persephone-screen-helper`
- `cargo clippy --locked -p persephone-screen-helper --all-targets -- -D warnings`
- The real capture runs in the core's ignored test: `cargo build --locked -p persephone-screen-helper`, then `cargo test --locked -p persephone-core --lib platform::windows::screen_helper -- --ignored`. It shows the capture border for a moment and needs an unlocked desktop.

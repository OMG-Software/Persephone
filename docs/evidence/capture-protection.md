# Capture protection

Checked 16 November 2026 on Windows 11 Pro x64 (build 26300), debug builds,
rustc 1.99.1, Node from the locked frontend dependencies. Source: branch
`claude/image-load-and-capture`, stacked on `e0d99b7` (the
[third-party module report](third-party-module-report.md) branch), worktree
`.local/wt-capture`, `CARGO_TARGET_DIR=.local/build/rust-lead`,
`--locked --offline`. The change was uncommitted during the checks and was
then committed unchanged. `Cargo.lock` is unchanged. **Result: every check that
was run passed. In the running demo app, Windows reports the main window as
excluded from capture (`WDA_EXCLUDEFROMCAPTURE`); the same read on a build
without this change reports no exclusion. No screen image was taken. The
setting was not switched in the running app.**

Decision: [ADR-0045](../adr/0045-exclude-the-main-window-from-screen-capture-by-default.md).

## What was built

* **Setting:** `AccountSettings.capture_protection`, `true` by default. The
  core stores it with the other account settings; a stored record without it
  reads as `true`.
* **UI host** (`src-tauri/src/capture.rs`, `src-tauri/src/lib.rs`):
  * `CaptureState` holds the wanted state, `true` at start, and a session
    number.
  * At window creation, `setup` installs an applier for the main window,
    which applies "on" at once.
  * A successful `account_settings` reply or `set_account_settings` save sets
    the wanted state, unless a session change happened while the call was in
    flight. A failed or refused call changes nothing.
  * `SessionChanged` turns protection on and makes every call in flight stale.
  * The applier runs `SetWindowDisplayAffinity` on the window's thread:
    `WDA_EXCLUDEFROMCAPTURE`, then `WDA_MONITOR` if Windows refuses it, and
    `WDA_NONE` for off. A failure prints one line without detail.
  * Manifest: the `Win32_UI_WindowsAndMessaging` feature of the existing
    `windows-sys` 0.61.2 dependency. The lockfile does not change.
* **Page:** `CaptureProtection.svelte` in Settings > Security when signed in.
  It saves through the same settings path as Preferences. All text is inert.

## Tests

New:

* `src-tauri/src/capture.rs`: protection is on and applied when an applier
  is set; a session change turns it on again; a reply from an older session
  is ignored; a state set before the applier is applied when it comes.
* `src-tauri/tests/ipc.rs`:
  `given_account_settings_then_capture_protection_follows_only_successful_calls`
  (default on; a reply with `false` turns it off; a session change turns it
  on; a successful save turns it off; refused saves and a malformed call
  change nothing). The mock runtime installs no applier, so this test does
  not call Windows.
* `crates/core/src/settings.rs`: an old record without the field reads as on;
  a stored off stays off; the round trip carries the field.
* Frontend: the bridge sends the field; the defaults hold it; a render test
  checks the label, the checked state and inert text.

## Checks run

On `1e03d88` plus this change (the first run on the full branch), all exited
with status 0:

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | passed |
| `cargo run --locked --offline -p persephone-contracts --bin export-contracts -- --check` | bindings current |
| `python tools/dev.py frontend --offline` | svelte-check 177 files, 0 errors, 0 warnings; 314 Node tests passed |
| `cargo build --locked --offline -p persephone-media-worker` | passed |
| `cargo test --locked --offline -p persephone-contracts -p persephone-core-protocol` | 7, 16 and 25 passed |
| `cargo test --locked --offline -p persephone-core --no-fail-fast` | lib 285 passed, 1 ignored; integration binaries 13, 5, 5, 2, 12, 15, 20 and 4 passed |
| `cargo test --locked --offline -p persephone-desktop` | 19 passed, 1 ignored; IPC 36 passed |
| `cargo clippy --locked --offline -p persephone-contracts -p persephone-core-protocol -p persephone-core -p persephone-desktop --all-targets -- -D warnings` | passed |
| `cargo clippy --locked --offline -p persephone-core --features dev-disposable-servers --all-targets -- -D warnings` | passed |
| `git diff origin/claude/injected-dll-report -- Cargo.lock` | empty |

`PERSEPHONE_MEDIA_GUEST` pointed at the release Wasm guest built for the base
branch, because this target folder had none. A first run of the same list
failed the bindings check (stale after a doc comment change; regenerated)
and the worker build (no guest); it is not counted.

The [rerun after the rebase](#rerun-after-the-rebase) is recorded below.

## Rerun after the rebase

The branch was rebased onto `9e9c645` and the same list was run again, all
exiting with status 0: fmt; bindings current; frontend (svelte-check 177
files, 0 errors, 0 warnings; 314 Node tests passed); worker build; contracts
and protocol 7, 16 and 25 passed; core lib 286 passed, 1 ignored, and
integration binaries 13, 5, 5, 2, 12, 15, 20 and 4 passed; desktop 19 passed,
1 ignored, IPC 36 passed; both Clippy runs; `Cargo.lock` unchanged.

It was then rebased onto `e0d99b7`, which adds only a scanner test and
evidence text. After that: `cargo fmt --all -- --check` passed; `cargo test
--locked --offline -p persephone-core --lib` 287 passed, 1 ignored; `cargo
test --locked --offline -p persephone-desktop` 19 passed, 1 ignored, IPC 36
passed; `Cargo.lock` unchanged.

The live run below used the build rebased onto `9e9c645`.

## Live run

`persephone.exe --demo` from this build, with `WEBVIEW2_USER_DATA_FOLDER` set
to a disposable folder under `.local`. After 15 s, a script listed the
visible top-level windows of the `persephone-ui.exe` child of that core
process and read each one's affinity with `GetWindowDisplayAffinity`. It read
no pixels and no window text.

| Build | Window class | Size | Affinity |
| --- | --- | --- | --- |
| this change | `Tauri Window` (the main window) | 1040 × 759 | `0x11` (`WDA_EXCLUDEFROMCAPTURE`) |
| this change | `Tao Thread Event Target` | 16 × 16 | `0x0` |
| control: `1e03d88`, without this change | `Tauri Window` | 1040 × 759 | `0x0` |
| control | `Tao Thread Event Target` | 16 × 16 | `0x0` |

* The 16 × 16 window is the windowing library's transparent, non-activating
  event-target tool window. It shows no content.
* The demo has no account, so the protection stayed at its start value. The
  settings path was not exercised live.
* Both cores' error output stayed empty. Stopping each core left no process
  of either run.

## Limits

* No screen capture was taken to show a blank window. A capture of the
  screen at the window's place would show whatever is behind the window,
  which can be private content of the user. The live result is the affinity
  that Windows reports.
* `PrintWindow` and other ways for a program to read the window's content
  were not tested. Display affinity covers screen capture APIs only. A
  same-user program can still read the process memory or inject code.
* The `WDA_MONITOR` fallback, the off state and a change from the setting
  were not observed live. They are covered by unit and IPC tests with a
  replaced applier only.
* The effect on screen readers, magnifiers and remote-support tools was not
  tested.
* Native file and credential dialogs are separate windows and are not
  protected.
* Windows only. On other platforms the setting has no effect.
* Debug builds on one Windows host only.

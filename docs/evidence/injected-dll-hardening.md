# Injected DLL hardening

Checked 16 November 2026 on Windows 11 Pro x64 (build 26300), debug builds,
rustc 1.99.1, WebView2 runtime 157.0.4341.46. Source: commit `f6b84b8` plus
the hardening change only, built in a separate worktree. **Result: the core
and the UI now refuse images not signed through Microsoft and all
extension-point DLLs, including global window hooks, from the first statement
of `main`. The demo app started and ran with both policies on. The live block
of RivaTuner Statistics Server's hook was not observed, because the hook loader
injected no new process during the session.**

## Why

The [failed window retry](window-retry.md) crash dumps all fail inside
`RTSSHooks64.dll` (RivaTuner Statistics Server) in the WebView2 browser
process. On 16 November 2026 the same DLL was loaded in the running core
(`persephone.exe`), the process that holds the keys, tokens and decrypted
messages. Code in a DLL loaded into a process can read all of its memory.

Owner decisions (16 November 2026):

* Block hook-based injection for every signer.
* Keep refusing DLLs not signed through Microsoft.
* Do not stop Persephone from running beside anti-malware software. Report
  every other DLL that loads, in an in-app panel with a notice for new ones.
  The report is not built yet.

## Design

* `crates/core/src/platform/windows/harden.rs` and `src-tauri/src/harden.rs`
  each define `block_injected_dlls`. It sets two process mitigation policies
  with `SetProcessMitigationPolicy`, then reads both back with
  `GetProcessMitigationPolicy`:
  * `ProcessExtensionPointDisablePolicy`: no global window hook DLLs
    (`SetWindowsHookEx`), AppInit DLLs, legacy IMEs or Winsock LSPs, whoever
    signed them.
  * `ProcessSignaturePolicy` with `MicrosoftSignedOnly`: later image loads must
    be signed through Microsoft.
* The core calls it first in `main`; the UI calls it first in `run`, before
  launcher mode, Tauri or any window. If either policy is not in effect, the
  process exits with status 2 (fail closed).
* Windows never lets a process undo these policies. Modules loaded before the
  call stay loaded.
* The UI crate does not depend on the core, so the 30-line function exists
  twice. No manifest or lockfile changed: `Win32_System_Threading` was already
  enabled in both crates.

## What the signature policy admits on this host

A probe set `MicrosoftSignedOnly` and then mapped each DLL without running it
(`LoadLibraryExW` with `DONT_RESOLVE_DLL_REFERENCES`). A control run without the
policy loaded all five.

| DLL | Signer (issuing CA) | With the policy |
| --- | --- | --- |
| `SophosAmsiProvider.dll` (the only AMSI provider) | Sophos Limited (Microsoft ID Verified CS AOC CA 01) | loaded |
| `hmpalert.dll` (Sophos HitmanPro.Alert) | Sophos Limited (Microsoft ID Verified CS EOC CA 02) | loaded |
| `MpOAV.dll` (Defender AMSI provider) | Microsoft Windows | loaded |
| `RTSSHooks64.dll` | Micro-Star International (GlobalSign EV) | refused, error 577 |
| `msimg32.dll` | Microsoft Windows | loaded |

So the signature policy admits third-party DLLs signed through Microsoft ID
Verified certificates. It is not an anti-malware allowlist. Whether the
admission comes from that CA or from a further Microsoft signature in these
files was not checked. Persephone does not call AMSI, so no AMSI provider is
loaded into it either way.

`RTSSHooks64.dll` contains `SetWindowsHookExA`, `CreateRemoteThread` and
`WriteProcessMemory`. The extension-point policy covers the first route; the
signature policy covers the second for this DLL.

## Tests

New, both passing on the base commit plus this change:

* `persephone-core`: `hardening_blocks_unsigned_images`;
* `persephone-desktop`: `given_hardened_child_then_unsigned_dll_is_refused`.

Each runs its own test binary as a child process, because the policies are
one-way. The child loads a copy of `version.dll` with one byte of its DOS stub
changed (negative control: it loads), applies the policies, then expects a
second such copy to fail with `ERROR_INVALID_IMAGE_HASH` (577) and
`msimg32.dll` to load. The parent checks that the child ran exactly one test.

Commands (worktree `.local/wt-harden`, `CARGO_TARGET_DIR=.local/build/rust-wt`):

* `cargo test --locked --offline -p persephone-core harden`: 1 passed, 1 ignored (the child half).
* `cargo test --locked --offline -p persephone-desktop harden`: 1 passed, 1 ignored.
* `cargo clippy --locked --offline -p persephone-core --features dev-disposable-servers --all-targets -- -D warnings`: passed.

Run by the workers in the main checkout before other uncommitted call work
broke its build: `cargo fmt` checks for both packages, `cargo test -p
persephone-core --bin persephone` (13 passed), `cargo test -p
persephone-desktop` (15 lib, 34 IPC) and default-feature Clippy for both
packages, all passing.

## Live run

`persephone.exe --demo` from the worktree build, with
`WEBVIEW2_USER_DATA_FOLDER` set to a disposable folder under `.local`:

* both the core and the UI reported `MicrosoftSignedOnly` and
  `DisableExtensionPoints` on (`Get-ProcessMitigation`);
* the web view started six processes, and no window failure was reported after
  37 s, past the 30 s readiness limit;
* modules outside Windows folders: the executables, `hmpalert.dll` in both, and
  the Microsoft-signed `EmbeddedBrowserWebView.dll` in the UI;
* stopping the core left no process of the run.

## Limits

* The WebView2 processes are started by the WebView2 runtime and are not
  covered. Its renderer, GPU and storage processes already block non-Microsoft
  images; the browser and network processes do not. No supported WebView2
  setting changes that. Microsoft's Edge-only `BrowserCodeIntegritySetting`
  is not a WebView2 policy.
* A DLL already present when `main` starts stays. Sophos's kernel driver
  inserts `hmpalert.dll` before then.
* A program running as the same user can still read the process's memory or
  inject code without loading an image. This hardening removes accidental
  third-party code; it is not a boundary against same-user malware.
* Not checked under the policies: the native file and credential dialogs, the
  passkey prompt, a real sign-in, and an RTSS injection attempt.
* Debug builds on one Windows host only.

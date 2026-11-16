# Injected DLL hardening

Checked 16 November 2026 on Windows 11 Pro x64 (build 26300), debug builds,
rustc 1.99.1, WebView2 runtime 157.0.4341.46. Source: commit `f6b84b8` plus
the hardening change only, built in a separate worktree. That tree was then
committed unchanged as `e3cb522`; its `Cargo.lock` is blob `f1d06a4`, the same
as in `f6b84b8`. Both commits are local and unpublished: `f6b84b8` belongs to
other, unmerged work. The published equivalent is `7b3e00d` on the pull
request branch, on a later `main`. Its `harden.rs` files, `crates/core/src/main.rs`
and `src-tauri/src/lib.rs` are byte-identical to `e3cb522`. A labelled rerun on
the published branch is under [Tests](#tests). **Result: the core
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

Provenance (recorded on 16 November 2026 from the session record; not rerun):

* The probe is the standalone script in [the appendix](#appendix-signer-probe).
  It uses no Persephone source or build, only Python 3.14.4 `ctypes` and
  `kernel32`.
* It ran twice, as `python -I signed_probe.py control <dll>...` and
  `python -I signed_probe.py block <dll>...`, with these five paths:
  * `C:\Program Files\Sophos\Sophos AMSI Protection\SophosAmsiProvider.dll`
  * `C:\Windows\System32\hmpalert.dll`
  * `C:\Program Files (x86)\RivaTuner Statistics Server\RTSSHooks64.dll`
  * `C:\ProgramData\Microsoft\Windows Defender\Platform\4.18.25110.6-0\MpOAV.dll`
  * `C:\Windows\System32\msimg32.dll`
* Both runs completed and printed one line for each DLL. The `block` run did
  not stop at its `policy not set` exit, so the policy was in effect. The
  numeric exit status was not captured separately.
* The signers came from each file's Authenticode signer chain, read on the same
  host after the probe.
* The raw output was not kept. The table below is the sanitized result; it
  contains no user paths or identifiers.

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

Rerun on 16 November 2026 on the published pull request branch at `bfd71ef`
(`7b3e00d` plus the test-path fix below; `Cargo.lock` blob `adf1bca`), same
host, debug, `--locked --offline`, worktree `.local/wt-pr`,
`CARGO_TARGET_DIR=.local/build/rust-pr`. The whole run exited with status 0:

* `cargo fmt -p persephone-core -p persephone-desktop -- --check`: passed.
* `cargo test --locked --offline -p persephone-core harden`: 1 passed, 1 ignored.
* `cargo test --locked --offline -p persephone-desktop --lib harden`: 1 passed, 1 ignored.
* `cargo clippy --locked --offline -p persephone-core -p persephone-desktop --all-targets -- -D warnings`: passed.

The fix in `bfd71ef` builds the test DLL paths from `GetSystemDirectoryW`
instead of a fixed `C:\Windows\System32`. The live run below was not repeated.

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

## Appendix: signer probe

The script as run (`signed_probe.py`):

```python
"""Tries to map each DLL given as an image, with or without the
MicrosoftSignedOnly binary signature policy, without running its code
(DONT_RESOLVE_DLL_REFERENCES: no DllMain, no imports resolved).

Usage: python -I signed_probe.py control|block <dll> [<dll> ...]
Prints one line per DLL: loaded, or the Windows error code.
"""
import ctypes
import sys
from ctypes import wintypes

mode, paths = sys.argv[1], sys.argv[2:]
k32 = ctypes.WinDLL("kernel32", use_last_error=True)
k32.LoadLibraryExW.restype = wintypes.HMODULE
k32.LoadLibraryExW.argtypes = [wintypes.LPCWSTR, wintypes.HANDLE, wintypes.DWORD]
k32.FreeLibrary.argtypes = [wintypes.HMODULE]
DONT_RESOLVE_DLL_REFERENCES = 0x1

if mode == "block":
    flags = wintypes.DWORD(1)  # MicrosoftSignedOnly
    if not k32.SetProcessMitigationPolicy(8, ctypes.byref(flags), 4):
        sys.exit(f"policy not set: {ctypes.get_last_error()}")

for path in paths:
    handle = k32.LoadLibraryExW(path, None, DONT_RESOLVE_DLL_REFERENCES)
    if handle:
        print(f"{mode:7} loaded   {path}")
        k32.FreeLibrary(handle)
    else:
        print(f"{mode:7} refused  {path}  (error {ctypes.get_last_error()})")
```

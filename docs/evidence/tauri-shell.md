# Tauri shell and IPC permissions

Checked 20 October 2026 on Windows 11 Pro x64 (build 26200), rustc 1.99.1,
WebView2. First P1 slice: a thin desktop host with one allowlisted command.
**Result: the shell builds and runs; the web view can call only `app_status`,
from the bundled origin only, under a strict CSP, and cannot navigate away or
open windows.** No Matrix, storage or media functionality is wired in yet, and
IPC body size is not yet bounded before Tauri allocates it.

## Shape

* [`src-tauri/`](../../src-tauri) (`persephone-desktop`): Tauri 2.12.4 and
  tauri-build 2.7.3, the versions the [P0 IPC review](tauri-ipc.md) inspected.
  Both were released 14 October 2026, six days before this run; they are kept
  so the review stays valid, and the short release age is a supply-chain note.
* Features: `wry`, `common-controls-v6` and `custom-protocol` only. Every
  build, debug included, serves the embedded frontend; there is no development
  server. Off: `dynamic-acl`, `devtools` (release), `tracing` (records IPC
  payloads), `compression`, `isolation`, and Linux `x11`/`dbus` until Linux is
  built. No plugins.
* One command, `app_status`, returning the generated
  [`AppStatus`](../../crates/contracts/src/lib.rs) contract. `build.rs` registers
  only that command; [`capabilities/main.json`](../../src-tauri/capabilities/main.json)
  grants only `allow-app-status` to the `main` window, local origin only. No
  `core:default` or other core permissions.
* The window is created in code with `on_navigation` limited to the bundled
  origin and `on_new_window` denying every request. `withGlobalTauri` is off
  and `freezePrototype` on.
* Frontend calls go through [`src/bridge`](../../frontend/src/bridge/index.ts);
  a test fails if any other source imports `@tauri-apps/api`, uses
  `__TAURI_INTERNALS__` or calls `invoke`, or uses raw HTML insertion.

## Windows build note

`common-controls-v6` imports functions that exist only in Common Controls 6,
which an executable gets only from its application manifest. tauri-build
embeds the manifest in the app binary alone, so the test executables failed to
load (`STATUS_ENTRYPOINT_NOT_FOUND`). `build.rs` now embeds the same manifest in
every artifact the crate links with MSVC `/MANIFEST:EMBED`.

Adding the Tauri graph made `aws-lc-sys` rebuild, which hit the known MSVC
`lib.exe` archive failure (`LNK1114`). The [development guide](../development.md)
workaround (`AR_x86_64_pc_windows_msvc` set to `llvm-lib.exe` in the shell)
resolved it; no repository configuration was changed.

## IPC permission tests (mock runtime)

Seven tests run against the real configuration and compiled capabilities with
Tauri's mock runtime, plus one navigation unit test:

* `app_status` from the bundled origin returns the typed status.
* Unregistered commands (including case variants and the empty name) fail.
* 14 core plugin commands (`app`, `event`, `path`, `window`, `webview`,
  `image`, `resources`, `menu`, `tray`) are all denied.
* `app_status` from three remote or look-alike origins is refused.
* A request with a forged invoke key is dropped without any reply; the same
  request with the real key is answered.
* Configuration: `default-src 'none'`; no `unsafe-inline`, `unsafe-eval`,
  wildcard, `data:`, `blob:` or scheme sources; no dev server; capability is
  exactly `["allow-app-status"]` for `main` with no remote origins.
* Navigation predicate: only `http(s)://tauri.localhost` and
  `tauri://localhost` without a port; nine look-alike, file, data,
  `javascript:` and IPC URLs refused.

## Real web view check

The debug build was launched with WebView2's remote-debugging port
(environment variable only) and inspected over the DevTools protocol:

| Check | Result |
| --- | --- |
| Status line rendered from a real IPC reply | "Rust host 0.0.0 · P1" |
| `window.__TAURI__` | absent |
| `plugin:window\|close` from the page | denied ("window.close not allowed") |
| `fetch('https://example.org/')` | blocked by `connect-src` |
| Injected inline `<script>` | blocked |
| `eval` and `new Function` from page code | `EvalError` |
| `setTimeout` with a string | blocked |
| `location.href = 'https://example.org/'` | page stays on `http://tauri.localhost/` |
| `window.open(...)` | returns `null` |

The served CSP matches the configuration, with one `sha256` hash Tauri adds
for its initialization script. Code evaluated directly by the debugger is
exempt from CSP; `eval` was therefore tested from page-scheduled code. Tauri
prints the expected invoke key to stderr when it drops a forged request; a
release build has no console.

An earlier attempt appeared to show the web view exiting a few seconds after
launch. That was a test artifact: stopping a background task left an earlier
app instance running, and instances shared one WebView2 user-data folder. With
one instance launched by PID, the web view stayed up for the full 40-second
check.

## Limits

Not done: a pre-allocation bound on IPC bodies (the plan's open transport
item), Isolation pattern evaluation, accessibility and keyboard checks,
release-build and packaged runs, signing, three-OS CI, and Linux/macOS builds.
The real web view check is a manual DevTools probe, not an automated test.

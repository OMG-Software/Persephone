# Web view to host IPC: size and allocation

Checked 20 October 2026 on Windows 11 Pro x64 (build 26200) with the
[Tauri shell](tauri-shell.md) debug build, Tauri 2.12.4, wry 0.57.0 and
WebView2. Continues the [P0 Tauri IPC source review](tauri-ipc.md).
**Result: a page can make the host process allocate about nine times any
payload up to at least 256 MiB, on two paths, before any application code runs
and without an invoke key. The host cannot bound one of the paths at all.**

## Where allocation happens (wry 0.57.0, `src/webview2/mod.rs`)

* **Custom-protocol IPC** (`http://ipc.localhost/<command>`, Tauri's default):
  wry reads the WebView2 request body stream in 1 KiB chunks into a `Vec` with
  no limit. The `Content-Length` header is available before the read but is not
  checked. Tauri then receives an owned `Request<Vec<u8>>`, as the earlier
  review found.
* **`postMessage`** (`window.chrome.webview.postMessage`, a WebView2 built-in
  available to any page): wry's `WebMessageReceived` handler calls
  `TryGetWebMessageAsString`, which returns the whole message already allocated
  by WebView2 in the host process, then copies it into a Rust `String`. No host
  code sees a size before that allocation exists.

## Measurement

One fresh app instance per case, launched and stopped by PID. The page sent
each payload once; the host's peak committed memory (`peak_pagefile`) was read
before sending and three seconds after. Fetch requests carried no invoke key,
so Tauri answered HTTP 500 after wry had read the body.

| Payload | `postMessage` peak growth | IPC fetch peak growth |
| --- | --- | --- |
| 1 MiB | 8.8 MiB | 9.7 MiB |
| 16 MiB | 144.1 MiB | 152.1 MiB |
| 64 MiB | 577.0 MiB | 608.9 MiB |
| 128 MiB | 1154.1 MiB | 1218.1 MiB |
| 256 MiB | 2308.4 MiB | 2436.6 MiB |
| 512 MiB | none: the page could not build the string (`RangeError`) | none: the request did not complete |

Growth is about 9× the payload on both paths. The host stayed alive on this
machine in every case. The limit observed is the renderer's own (JavaScript
string length, request construction), not anything in WebView2, wry, Tauri or
the application. Repeated or concurrent requests were not measured.

## Consequences

* A compromised renderer can drive the key-holding host process to multi-GB
  commit per request, unauthenticated. This is an availability and
  memory-pressure attack on the process that will hold sessions and keys.
* Command validation, raw-body commands and Tauri's Isolation pattern all run
  after these allocations; they can stop further deserialization but cannot
  bound memory.
* A host-side pre-allocation limit would need a patched wry for the
  custom-protocol path, and is not possible for `postMessage`, whose message
  arrives already allocated.

## Options

**Decision (owner, 20 October 2026): option 1**, recorded as
[ADR-0004](../adr/0004-separate-the-key-holding-core-from-the-web-view-process.md).

1. **Separate the key-holding core from the web view host.** The Tauri process
   becomes a UI relay with no keys, forwarding bounded frames to a core process
   that enforces limits before allocation on its own pipe, as the media worker
   protocol does. Exhausting the UI process then cannot touch keys or stores,
   and the UI process can be given its own memory limit.
2. **Patch wry** to check `Content-Length` and the stream size before reading,
   and remove the `postMessage` route. This leaves a maintained fork and does
   not address WebView2's own allocation for `postMessage`.
3. **Accept the residual risk** as denial of service that requires a renderer
   compromise, optionally capping the host's memory with a Job so exhaustion
   ends in a clean exit rather than system-wide pressure.

## After the core/UI split

Implemented per ADR-0004 and measured the same day with debug builds:

* `persephone` (the core, [`crates/core`](../../crates/core)) is the entry
  process. It starts `persephone-ui` (the Tauri shell) with a pipe on the UI's
  stdin and stdout and places the UI in a Job: 2 GiB for the UI tree, 1 GiB per
  process, kill on close. The UI's WebView2 processes are its descendants and
  join the same Job.
* The UI moves the pipe to non-inheritable handles at startup and points its
  standard handles at `NUL`. `app_status` is relayed to the core over
  [`persephone-core-protocol`](../../crates/core-protocol): one control frame
  (at most 128 KiB, header checked before allocation) each way. Any bad frame,
  unknown command or unknown field ends the session.
* The UI refuses to start without the core (exit 2).

| Check | Result |
| --- | --- |
| Status line and `invoke('app_status')` | Answered by the core through the UI |
| UI process tree | 7 processes, the UI a child of the core |
| Core killed | All 7 UI-tree processes ended within 3 s |
| 64 MiB `postMessage` | Core peak commit +0.0 MiB; UI +577 MiB, alive |
| 128 MiB `postMessage` | Core +0.0 MiB; UI +898 MiB, capped below its 1 GiB limit, alive |
| 256 MiB `postMessage` | Core +0.0 MiB; UI +0; the renderer, also in the Job, failed to build the message |

The key-holding process did not grow in any case. A memory flood now stops at
the UI's Job limit instead of reaching 1.2–2.4 GB.

A test also caught a serde gap: `deny_unknown_fields` is not applied to unit
variants of an internally tagged enum, so `{"command":"app_status","arg":1}` was
accepted. Commands are now empty struct variants, which serde checks.

## Limits

Windows only. Peak commit was read three seconds after sending; transient peaks
inside that window are included, later ones are not. Release builds, repeated
or concurrent requests, other WebView2 versions and Linux/macOS web views were
not measured.

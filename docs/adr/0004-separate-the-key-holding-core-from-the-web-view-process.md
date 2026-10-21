# Separate the key-holding core from the web view process

* Status: accepted
* Deciders: project owner
* Date: 2026-10-20

## Context and Problem Statement

ADR-0000 places a Rust core behind Tauri's IPC. The plan assumed that core
would run inside the Tauri process. Measurement showed that a page can make
that process allocate about nine times any payload up to at least 256 MiB
(about 2.4 GB from one request), on Tauri's custom-protocol IPC and on WebView2's
built-in `postMessage`, before application code runs and without an invoke key
([evidence](../evidence/ipc-transport.md)). `postMessage` arrives already
allocated, so no code in that process can bound it. A compromised renderer
could therefore exhaust the process that holds sessions, keys and stores.

## Decision Drivers

* Keys, sessions and stores must not share a process with web view IPC whose
  allocation the application cannot bound.
* Limits must apply before allocation, as they do for the media worker.
* Keep Tauri (ADR-0000) and SDK-owned cryptography (ADR-0002).
* Losing the interface must not corrupt or expose protected state.

## Considered Options

* Run the core in a separate process; the Tauri process becomes a keyless UI
  relay.
* Maintain a patched wry that checks request sizes and drops `postMessage`.
* Accept the risk as renderer-dependent denial of service.

## Decision Outcome

Chosen option: **a separate core process, with the Tauri process as a keyless
UI relay.**

* The core is the application's entry process. It owns sessions, the Matrix
  SDK, stores, keys and the media broker, and starts the UI process.
* The UI process runs Tauri and the web view. It holds no keys, tokens or store
  handles. It forwards each allowed command to the core as one bounded frame
  using the existing worker frame protocol, and returns the core's reply.
* The core reads every UI frame with the budgeted frame reader, so oversized
  or malformed requests are rejected before their bodies are allocated. It
  validates each request strictly and treats the UI as untrusted.
* On Windows the core places the UI process and its web view processes in a Job
  with a memory limit and kill-on-close. A memory flood ends the UI, which the
  core observes as a closed pipe; protected state is unaffected.
* Tauri capabilities and the bridge remain in place as a first filter; they are
  no longer the only boundary.

### Positive Consequences

* Unbounded web view IPC allocation can exhaust only the keyless UI process.
* The core enforces pre-allocation limits on every request, closing the plan's
  open transport gate for the core boundary.
* UI crashes become recoverable events rather than failures of the
  key-holding process.

### Negative Consequences

* Two processes to launch, supervise and package, plus an IPC hop per command.
* Events from the core to the UI need their own bounded, back-pressured stream.
* The UI process can still be denied service; this decision protects the core,
  not the interface.
* The Job limit is Windows-specific; Linux and macOS need equivalent controls.

## Links

* [Web view to host IPC measurement](../evidence/ipc-transport.md)
* [P0 Tauri IPC source review](../evidence/tauri-ipc.md)
* [Contained media worker](../evidence/media-worker.md) (same frame protocol)
* [ADR-0000](0000-use-tauri-for-the-desktop-client.md)

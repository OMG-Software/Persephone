# P0 Tauri IPC source review

> Historical run record: commands and source paths below describe the pre-restructure checkout. See [Development](../development.md) for current paths and commands.

19 October 2026. This is inspection of the published **Tauri 2.12.4** source,
downloaded by `cargo info tauri@2.12.4`; no Tauri executable was built or run.
It does not establish actual webview allocation limits on any operating system.

## Finding: command validation happens after transport allocation

The inspected `src/ipc/protocol.rs` custom-protocol path accepts an owned
`http::Request<Vec<u8>>` in `parse_invoke_request` (lines 434–550). For JSON,
line 526 parses that buffer into `serde_json::Value` before calling the webview
message handler. The alternative post-message path accepts an owned
`Request<String>` (line 185), then deserializes the message and its JSON payload
before dispatch (lines 235–311). Neither inspected path applies an application
body-size ceiling before those parses.

Consequently, a generated typed command or an `invoke_handler` check cannot
establish a pre-allocation transport bound. A command taking `ipc::Request`
can examine `InvokeBody::Raw(Vec<u8>)` and reject its length before application
DTO deserialization, but that vector already exists. This is a source-derived
limit of that proposed control, not an executed memory-exhaustion exploit.

The optional Isolation pattern also parses/decrypts an already received body;
it does not establish a size ceiling in these inspected functions. Frontend
checks can improve normal behavior but cannot constrain a compromised renderer.
The worker frame probe's early rejection guarantee does not transfer to Tauri
IPC merely because both transports carry bytes.

Primary pinned sources: [IPC protocol](https://docs.rs/crate/tauri/2.12.4/source/src/ipc/protocol.rs),
[request/body types](https://docs.rs/crate/tauri/2.12.4/source/src/ipc/mod.rs),
[builder hooks](https://docs.rs/crate/tauri/2.12.4/source/src/app.rs).
The downloaded `src/ipc/protocol.rs` SHA-256 is
`bf93dc095490db290d5d3fc2a3c81fa0778ca8142911a7c2f97dcbcc98b3be97`.

## Finding: optional tracing includes IPC payloads

With Tauri's `tracing` feature enabled and the relevant trace spans recorded,
these handlers include request and response bodies in tracing fields (for
example protocol lines 63–104 and 190–195). Such bodies can contain passwords
or decrypted chat content. Production must leave that feature disabled unless
the exact sink/feature combination is separately reviewed and tested for
content exclusion. Check the unified application feature graph, including
plugins, rather than only the direct Tauri dependency declaration. Do not infer
an existing disclosure: there is no product executable or configured sink here.

## Required next proof

Keep the 128 KiB control limit and bounded admission design, but do not claim
them as a bound on Tauri/webview transport memory. Before accepting this gate:

1. Trace the exact pinned runtime/webview backends for every supported OS,
   including the alternative post-message path and concurrent requests.
2. Exercise malformed, oversized and concurrent payloads in a real packaged
   webview; measure broker and renderer allocation before rejection.
3. Establish an enforceable earlier limit or containment strategy. Custom IPC
   or an upstream change requires a separate review of invoke-key integrity,
   origin checks, ACL/capability dispatch and platform behavior. A JS-only
   replacement is insufficient evidence.
4. Verify payload-free diagnostics with synthetic secret canaries in both
   development and packaged builds.

The production transport choice remains open. The accepted Tauri decision is
unchanged; the feasibility gate records this concrete limitation rather than
weakening its requirement.
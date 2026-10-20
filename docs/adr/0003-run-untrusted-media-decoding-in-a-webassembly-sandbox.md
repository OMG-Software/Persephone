# Run untrusted media decoding in a WebAssembly sandbox

* Status: accepted
* Deciders: project owner
* Date: 2026-10-20

## Context and Problem Statement

Persephone decodes images from hostile senders. A decoder bug must not give an
attacker network access, files, processes, keys or the Matrix session. The plan
relied on OS process containment for that. On Windows, P0 tested three worker
configurations ([evidence](../evidence/windows-sandbox.md#network-denial-differential)):

* A zero-capability AppContainer can still send packets. Listener-side filtering
  blocked every observed connection, but closed ports answer it, so it can map
  local port state.
* A Less Privileged AppContainer breaks Winsock initialization but can still
  open the kernel socket device.
* An AppContainer with a restricted token cannot start.

None of them denies networking at the sender. Linux and macOS containment is
not yet tested either. The decoder therefore needs a boundary that does not
depend on each OS's network policy.

## Decision Drivers

* A decoder compromise must not reach the network, even where OS policy cannot
  deny sockets.
* Prefer a boundary whose capabilities are explicit and the same on every OS.
* Keep defense in depth: no single bug should grant both code execution and
  network access.
* Keep CPU, memory and output bounded, within the existing media budgets.

## Considered Options

* Accept the measured Windows network posture as residual risk.
* Keep searching for an OS-only configuration that denies sockets.
* Compile the decoder to WebAssembly and run it in an embedded runtime that
  grants no network, file, clock or process capability, inside the existing
  OS-contained worker process.

## Decision Outcome

Chosen option: **run the decoder as WebAssembly with no networking
capability, inside the OS-contained worker.**

* The decoder is a `wasm32-unknown-unknown` module (no WASI). Its only imports
  are a fixed, reviewed set of host functions: read the input bytes, write a
  bounded output, report an error. It receives no socket, file, clock,
  environment, randomness or process functions, so the module cannot express a
  network request, whatever the OS would permit.
* The host instantiates the module only if its import list exactly matches that
  set; any other import rejects the module before execution.
* The runtime enforces a linear-memory cap, an instruction (fuel) or epoch
  budget, and the existing decoder wall deadline. Output dimensions and byte
  counts are validated by the host before pixels leave the worker.
* The worker process keeps its OS containment (AppContainer and Job limits on
  Windows; the planned Linux/macOS mechanisms). That outer layer now covers
  runtime escapes rather than being the only network control.

The owner selected **`wasmi`**, an interpreter with no code generation, over
`wasmtime` (Cranelift compilation, or its Pulley interpreter). An interpreter
has a smaller attack surface and is compatible with prohibiting dynamic code in
the worker, but decodes more slowly. The prototype pins `wasmi 2.0.0` with
`std`, `validate` and `portable-dispatch`. The owner chose portable (loop)
dispatch over the roughly 3× faster tail-call dispatch on 20 October 2026, so
tested and shipped builds run the same dispatcher and native stack use stays
bounded without relying on compiler tail-call optimization; the
[evidence report](../evidence/wasm-decoder.md) records the controls, tests and
16-megapixel benchmark against the 10-second deadline.

This ADR revises the plan's statement that a crate with no networking imports
is insufficient: that still holds for native code, because native code can make
system calls directly. A WebAssembly guest cannot; the runtime only executes
imports the host chose to provide.

### Positive Consequences

* Network denial for decoded content no longer depends on Windows, Linux or
  macOS socket policy; the capability does not exist inside the sandbox.
* Exploiting the decoder no longer yields native code execution. An attacker
  needs a second, independent runtime escape, which then meets OS containment.
* The same module and host checks apply on every platform, so one test suite
  covers the capability boundary.
* Memory and CPU limits become deterministic runtime limits, in addition to OS
  Job limits.

### Negative Consequences

* Decoding is slower than native code; an interpreter may be far slower. The
  budgets need benchmarking, and some maximum-size images may need lower limits.
* The runtime is new security-critical code and a new dependency with its own
  advisories, maintenance and review.
* Decoders must compile to `wasm32-unknown-unknown` without WASI, which limits
  library choice and rules out native SIMD paths unless explicitly supported.
* A runtime escape still lands in an OS sandbox whose Windows network posture
  was measured as not denied. That residual requires two independent bugs and
  is recorded rather than eliminated.

## Links

* [WebAssembly decoder sandbox evidence](../evidence/wasm-decoder.md)
* [Contained media worker evidence](../evidence/media-worker.md)
* [Windows network denial differential](../evidence/windows-sandbox.md#network-denial-differential)
* [Media containment and starting budgets](../architecture/development-plan.md#media-containment-and-starting-budgets)
* [P0 feasibility workbench](../planning/feasibility.md)

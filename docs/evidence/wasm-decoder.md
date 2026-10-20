# WebAssembly decoder sandbox

Checked 20 October 2026 on Windows 11 Pro x64 (build 26200), rustc 1.99.1.
Implements [ADR-0003](../adr/0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md).
**Result: the decoder runs as a WebAssembly guest whose only capabilities are
three host I/O functions; every other import is refused before execution.** It is
not yet wired into the OS-contained worker process, Tauri or the Matrix media flow.

## Components

| Crate | Role |
| --- | --- |
| [`persephone-media-decoder`](../../crates/media-decoder) | PNG (`png 0.18.1`) and JPEG (`zune-jpeg 0.5.15`, unsafe fast paths off) to 8-bit RGBA. Built for `wasm32-unknown-unknown`, no WASI. The same logic compiles natively for tests. |
| [`persephone-media-sandbox`](../../crates/media-sandbox) | `wasmi 2.0.0` host: import/export allowlist, fresh store per decode, memory cap, fuel budget, output validation. |

The guest ABI is three imports in module `persephone`: `input_len() -> i32`,
`read_input(ptr, len) -> i32` and `write_output(width, height, ptr, len) -> i32`,
plus exports `memory` and `decode() -> i32`. The release guest is 315 KB; parsing
its binary shows exactly those three imports and two exports. Rust std on this
target compiles unsupported I/O to stubs, so it adds no imports.

## Host controls

* **Import allowlist.** Every import must be one of the three functions, with its
  exact signature. Any other module or name (including all WASI functions), a
  wrong signature, or an imported memory, table or global rejects the module
  before instantiation. Only the three functions are linked.
* **Validation.** wasmi's `validate` feature is enabled explicitly. Without it,
  wasmi skips wasmparser validation; a type-invalid test module confirms
  rejection.
* **Memory.** A store limiter caps linear memory (192 MiB by default) and allows
  one memory, one table and one instance. Growth past the cap fails inside the
  guest; initial memory above it fails instantiation.
* **CPU.** Fuel metering bounds instructions per decode (64 × 10⁹ by default,
  calibrated below). Fuel is deterministic; the worker's wall deadline remains a
  separate, OS-level control.
* **Output.** The host accepts one write, checks dimensions (≤ 8192 per side,
  ≤ 16 megapixels) and an exact `width × height × 4` length, and copies through
  bounds-checked memory reads. A violation fails the decode even if the guest
  then returns success.
* **Dispatch.** `portable-dispatch` is selected. wasmi's default tail-call
  dispatch keeps native stack bounded only when LLVM emits tail calls. With
  default features off, an unoptimized build overflowed the **host** stack
  (`STATUS_STACK_OVERFLOW`) on a guest `loop`/`br` before fuel ran out; it also
  overflowed with an 8 MiB stack. The portable loop dispatcher runs the same
  code in tested and shipped builds.

## Tests

22 new tests pass. They run in both debug and release; `tools/dev.py check` now
builds and lints the wasm guest before the workspace tests.

* Decoder, native (4): RGB/grey-alpha to RGBA, unknown/truncated/oversized input,
  dimension checks before allocation, a PNG header claiming 100000 × 100000.
* Sandbox, real guest (6): exact allowlist acceptance, PNG round trip, baseline
  and progressive JPEG, malformed/unsupported/huge-header refusal with guest
  status codes, fuel exhaustion on a real decode, host input limit.
* Sandbox, hostile WAT modules (12): WASI `sock_open`/`fd_write`, unknown
  `persephone` and foreign-module imports; wrong signatures and memory, global or
  table imports; missing or mistyped exports; invalid Wasm; an infinite loop;
  memory growth and initial memory past the cap; `unreachable` and unbounded
  recursion; five output violations (length, dimension, zero, out-of-bounds,
  double write); success without output; an out-of-bounds input copy.

JPEG fixtures are synthetic Pillow images (16 × 8 solid baseline, 33 × 17
progressive gradient).

## Benchmark

Release build on this host, 4096 × 4096 (16-megapixel) photo-like synthetic images
generated outside the repository. Sandboxed output is byte-identical to the
native decoder. Run with the ignored test in
[`benchmark.rs`](../../crates/media-sandbox/tests/benchmark.rs).

| Input | Native | Sandbox, portable dispatch | Sandbox, tail-call dispatch | Fuel |
| --- | --- | --- | --- | --- |
| Baseline JPEG, 2.2 MiB | 70 ms | 4474 ms (64×) | 1488 ms (21×) | 9.23 × 10⁹ |
| Progressive JPEG, 1.9 MiB | 181 ms | 7156 ms (40×) | 2117 ms (12×) | 40.24 × 10⁹ |
| PNG, 15.8 MiB | 135 ms | 6075 ms (45×) | 1896 ms (14×) | 13.77 × 10⁹ |

Module validation took 2 ms. With the selected portable dispatch, every maximum
input fits the 10-second deadline on this machine, but the progressive JPEG uses
72% of it; slower CPUs may not fit. Tail-call dispatch is about 3× faster with
identical fuel. One measurement run per configuration; no variance was measured.

**Decision (owner, 20 October 2026): keep portable dispatch.** Speed is
revisited only if measurements on slower supported hardware exceed the deadline;
switching would require release-mode test coverage of the tail-call dispatcher.

## Limits

Running inside the contained worker, the wall deadline, the dynamic-code
prohibition and the memory budget are now covered by the
[media worker evidence](media-worker.md), which also lowers the pixel limit to
the protocol's 16,000,000 and fixes guest memory at 176 MiB. Not yet done: Linux/macOS runs, fuzzing the
decoders and the host, and wiring decode into Tauri and the Matrix media flow.
Peak process memory was not measured. The guest's memory cap and the 64 MiB
output copy together exceed the plan's 256 MiB worker budget at the limits, so
those budgets need reconciling. The benchmark covers one host and three images.

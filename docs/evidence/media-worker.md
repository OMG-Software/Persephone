# Contained media worker

Checked 20 October 2026 on Windows 11 Pro x64 (build 26200), rustc 1.99.1,
release builds. Joins the [WebAssembly decoder sandbox](wasm-decoder.md) to the
Windows containment from [the platform evidence](windows-sandbox.md).
**Result: real decodes run in a contained worker process that checks its own
containment first; memory stays within budget for every tested image, and the
broker's deadline terminates an over-long decode.** Not yet wired into Tauri or
the Matrix media flow; Linux and macOS are not implemented.

## Components

| Crate | Role |
| --- | --- |
| [`persephone-media-worker`](../../crates/media-worker) | Worker binary, one decode per process. The decoder guest is embedded at build time. |
| [`persephone-media-broker`](../../crates/media-broker) | Trusted launcher: containment, deadline, Job accounting, reply validation. |
| [`persephone-media-protocol`](../../crates/media-protocol) | Existing frame protocol, plus `write_frame` and the 6-byte `WorkerFailure` message. |

## Launch and containment (Windows)

The broker creates the worker suspended, assigns it to a Job, and only then
resumes it:

* **AppContainer** with no capabilities.
* **Handle list:** `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` limits inheritance to the
  worker's stdin and stdout pipe ends; the broker's ends are not inheritable.
  There is no stderr handle. The environment carries only `LOCALAPPDATA`,
  `SystemDrive`, `SystemRoot` and `windir`.
* **Mitigations at creation:** dynamic code prohibited, Win32k system calls
  disabled, extension points and non-system fonts disabled, remote and
  low-label image loads blocked, System32 preferred for image loads, strict
  handle checks, heap terminate on corruption, bottom-up and high-entropy ASLR.
  The interpreter-based sandbox needs no executable memory, which makes the
  dynamic-code policy possible.
* **Job:** one active process, 256 MiB process memory, 10 s user CPU, kill on
  close, die on unhandled exception.
* **Broker deadline:** 10 s wall time from resume, then `TerminateJobObject`.

## Worker self-check (fail closed)

Before reading input, the worker verifies its own token is an AppContainer
token, that it is in a Job, that a read-write-execute allocation fails with
`ERROR_DYNAMIC_CODE_BLOCKED`, and that its Win32k system-call policy is set.
Any missing control makes it send only a `ContainmentCheck` failure with a
bitmask and exit 3. Run without the broker, it reported `0b1111`. With the
broker but one mitigation removed, it reported exactly that mitigation's bit.

## Protocol

The broker sends one encoded-image frame. The worker returns exactly one frame:
validated RGBA pixels, or a 6-byte failure message (version, tag, 32-bit
detail) with no free text. The broker reads it with the existing budgeted
reader, which rejects bad headers before allocating, and accepts pixels only
with exit code 0.

## Memory: changes and result

The first contained run trapped on a 4000 × 4000 RGBA PNG that decoded fine in
the sandbox alone. wasmi stores linear memory in a `Vec` and grows it by
reallocating, so old and new buffers briefly coexist; together with host copies
of the input and output, one allocation exceeded the 256 MiB Job limit. Fixes:

* The sandbox borrows the input and passes output pixels as a slice of guest
  memory (`decode_with`); the worker writes them to stdout directly. The guest
  leaks its output buffer after handing it over, because freeing it would let
  the allocator write into the region before the host reads it.
* The guest widens RGB, grey and grey-alpha to RGBA in place, with the full
  RGBA capacity reserved up front.
* The guest's linear memory is fixed at 176 MiB (initial and maximum equal),
  so `memory.grow` never reallocates. It covers the heaviest measured input
  (158 MiB) with about 11% headroom; a test checks the built guest's limits.
* Decoder and sandbox now use the protocol's pixel limit (16,000,000; 4000 ×
  4000). They previously allowed 16 × 1024², which the protocol's pixel frame
  rejects. Earlier [sandbox benchmark](wasm-decoder.md#benchmark) figures used
  4096 × 4096 images.

## Benchmark

Synthetic photo-like images generated outside the repository, default policy,
one run each. `cargo test --release -p persephone-media-broker --test benchmark
-- --ignored --nocapture` with `PERSEPHONE_BENCH_DIR` set, after building the
release worker.

| Input (4000 × 4000 unless noted) | Result | Wall | User CPU | Peak process memory |
| --- | --- | --- | --- | --- |
| PNG, RGB, 15.1 MiB | Decoded | 6468 ms | 5328 ms | 193.0 MiB |
| PNG, RGBA, 15.7 MiB | Decoded | 7093 ms | 5812 ms | 193.7 MiB |
| JPEG baseline 4:2:0, 2.1 MiB | Decoded | 5315 ms | 3734 ms | 179.9 MiB |
| JPEG baseline 4:4:4, 4.8 MiB | Decoded | 6230 ms | 4625 ms | 182.6 MiB |
| JPEG progressive 4:2:0, 1.8 MiB | Decoded | 7820 ms | 6187 ms | 179.7 MiB |
| JPEG progressive 4:4:4, 4.2 MiB | **Deadline exceeded**, terminated | 10010 ms | 7562 ms | 182.1 MiB |
| PNG, 48 × 48 | Decoded | 42 ms | 15 ms | 177.9 MiB |

Peak memory is now 178–194 MiB for every input, under the 256 MiB limit. The
fixed guest memory sets a floor of about 178 MiB even for a tiny image, and
wasmi zero-fills it, which is included in the 42 ms. The maximum-size
progressive 4:4:4 JPEG, a valid file, cannot be previewed within the 10 s
deadline on this host. In the sandbox alone it took 11.6 s and 65 × 10⁹ fuel,
above the 64 × 10⁹ default.

## Tests

`python tools/dev.py check --offline` passed 144 tests (126 Rust, 3 frontend, 2
server-registration and 13 controller tests), with formatting and Clippy. New:

* Broker end-to-end (6): contained PNG decode with matching pixels within the
  memory limit; uncontained worker refusal; dropping either mitigation yields
  exactly its bit; malformed input returns the guest status; a 50 ms deadline
  terminates the worker; a 12 MiB memory limit prevents output.
* Broker unit (2): mitigation bits, including no ALWAYS_OFF bit set; the
  environment block holds only the four runtime variables.
* Protocol (2), decoder (1), sandbox (1): failure-message encoding, frame
  writer validation, in-place widening, fixed guest memory.

## Limits

Not verified: the AppContainer network posture still applies if the runtime is
escaped ([measured as not denied](windows-sandbox.md#network-denial-differential)),
and the worker's own syscalls, file access and handle table were not probed in
this configuration. The CPU-time Job limit was set but not separately
demonstrated. A signed, packaged worker; Linux and macOS; fuzzing; and
production integration remain open. The benchmark covers one host and one run
per image.

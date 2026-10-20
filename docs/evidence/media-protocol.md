# P0 resource admission and worker IPC probe

> Historical run record: commands and source paths below describe the pre-restructure checkout. See [Development](../development.md) for current paths and commands.

**Date:** 2026-10-19  
**Status:** Dependency-free Rust investigation; thresholds are candidates for the P0 aggregate budget, not production enforcement or an OS sandbox proof.

## Scope

This probe implements byte-accounted RAII admission and a framed worker stream parser under `probes/p0/resources`. It concerns app payload budgets and media-worker IPC only. It does not implement Tauri command IPC, application command authorization, an OS worker launcher, or production integration.

The aggregate policy candidate is 256 MiB across app-admitted payloads. Per-class ceilings are: 40 MiB encoded in-flight, 64 MiB worker output, 64 MiB retained pixels, 16 MiB bounded views, and 16 MiB queued controls. Their ceilings sum to 200 MiB, leaving 56 MiB reserved headroom that these classes cannot admit. This is payload accounting; allocator metadata, runtime state, decoder scratch space, host/webview copies, and other process memory are not thereby bounded. The worker's separate 256 MiB OS memory limit remains a platform enforcement candidate and has not been applied or verified.

Every nonempty reservation first checks its per-class limit and the shared aggregate counter with checked arithmetic; zero-byte permits are rejected so they cannot create unaccounted queue entries. Atomic compare-and-swap prevents concurrent admission from exceeding either counter. A successful `BudgetPermit` owns both reservations and returns them on drop, including when body reading fails or a caller cancels by dropping the frame. The counter is reserved before the frame body's vector is allocated.

## Worker frame protocol

The probe uses a fixed 17-byte header: one kind byte, an unsigned 64-bit big-endian body length, then two unsigned 32-bit big-endian fields. The final two fields are zero for control and encoded-image frames, and width/height for RGBA pixel frames. Kind-specific body limits are 128 KiB for control, 20 MiB for encoded media, and 64 MiB for RGBA pixels. Unknown kinds, empty frames, oversized lengths, and inappropriate dimensions are rejected after reading only the fixed header and before reserving or allocating the body. `ReceivedFrame` keeps both its vector and reservation private and exposes only a borrowed body slice plus a copied header, so callers cannot move the allocation out and release its permit early. The test reads through this API, verifies the aggregate reservation remains held, then verifies dropping the frame releases it.

RGBA metadata requires nonzero dimensions, each dimension at most 8192, at most 16,000,000 pixels, and an exact `width * height * 4` byte body length, with checked multiplication. The accepted upper pixel case is 4000 × 4000 = 16,000,000 pixels and exactly 64,000,000 bytes (slightly under 64 MiB); decoded dimensions larger than that pixel count are rejected.

This protocol is only a worker byte stream design. It does not prove that a Tauri request can be limited before Tauri or its plugin/command layer has buffered or deserialized its body. The earliest effective Tauri control-IPC body limit remains unresolved and must be measured or enforced at the receiving transport before production commands accept large payloads.

## Tests and results

Ten unit tests cover: category ceilings plus reserved headroom; aggregate and class limit rejection; integer overflow; permit release after normal drop and truncated/cancelled frame reads; concurrent permit contenders; exact per-kind frame limits and one-unit-over rejection; maximum pixel dimensions/count/exact RGBA length; zero and inconsistent pixel metadata; and oversized frame rejection before any body byte is read. The oversized-header test supplies a trailing body and confirms the stream cursor stops at byte 17 with no reservation. The concurrent test launches 32 contenders for 3 bytes against a shared 64-byte aggregate limit and observes 21 admitted permits (63 bytes), never exceeding the cap.

Environment: Windows, `rustc 1.99.1`, `cargo 1.99.1`. Commands used an isolated ignored target directory at `.local/p0/resources-target` and the crate has no third-party dependencies.

| Command | Exit | Result |
| --- | ---: | --- |
| `cargo fmt --manifest-path probes/p0/resources/Cargo.toml -- --check` | 0 | Formatting clean. |
| `cargo test --manifest-path probes/p0/resources/Cargo.toml --target-dir .local/p0/resources-target` | 0 | 10 passed, 0 failed; 0 doc tests. |
| `cargo clippy --manifest-path probes/p0/resources/Cargo.toml --all-targets --target-dir .local/p0/resources-target -- -D warnings` | 0 | Clean under Clippy with warnings denied. |

Cargo emitted an environment warning that hard links were unavailable in the isolated incremental cache and it copied files instead. The commands completed successfully; this affects build efficiency, not test results.

## Limits and next checks

The probe is a synchronous parser with a small protocol surface. It does not test process launch, timeouts, peer authentication, cancellation while blocked waiting for a header, decoder peak memory, filesystem/network denial, or behavior on macOS/Linux. `try_reserve_exact` reports allocation failure, but an allocator may reserve capacity above the requested logical length; permitted buffer capacity and parser/decoder allocations still need process-level measurement. The aggregate counters only account for buffers whose owners retain permits; later code must transfer or retain permits across every queue, copy, pixel buffer, and webview handoff. Dropping a Rust permit does not itself erase the bytes.

Before adopting these budgets, integrate shared permits into broker queues and buffer lifetimes, enforce a pre-body transport limit for Tauri control IPC, and verify memory/resource ceilings with a real worker under load on each target OS. Keep host/webview overhead explicitly measurement-required until those runs establish it.

**Assessment:** This implementation demonstrates checked, shared byte admission and early worker-frame rejection for the tested cases. It does not establish a total application memory bound or a secure worker boundary.  
**Confidence:** 92% for the probe behavior covered by the passing tests; low for production aggregate memory, native IPC, and OS containment because those remain unimplemented.
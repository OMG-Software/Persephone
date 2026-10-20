# P0 encrypted-message smoke

> Historical run record: commands and source paths below describe the pre-restructure checkout. See [Development](../development.md) for current paths and commands.

The subsequent [independent-recipient test](verified-recipients.md) exercises
mutual SAS device verification and bidirectional messages between two accounts.
The same-account run below remains historical evidence with its original scope.

**Run date:** 2026-10-19  
**SDK:** `matrix-sdk` 0.19.1, with default features disabled and only `e2e-encryption` plus `rustls-aws-lc-rs` enabled.  
**Scope:** Disposable synthetic fixture at `.local/p0/live-run-06/profile.json`; no production homeserver, account, or credentials.

## Result

The live same-account, two-device smoke passed against both fixture servers:

| Homeserver | Version from disposable profile | Result |
| --- | --- | --- |
| Merovingian | `merovingian-server 0.12.29` | PASS |
| Synapse | `1.163.0` | PASS |

For each server, the probe logged in as `p0_alice` on two distinct synthetic device IDs, performed bounded SDK classic `/sync` requests, created a private room with the SDK-recommended encryption state, and sent a fixed synthetic message through the SDK. A raw SDK HTTP request to room history returned that exact event ID with wire type `m.room.encrypted`. The second SDK client then read room history, decrypted the same event ID as `m.room.message`, and matched the fixed synthetic body. Both SDK logout requests succeeded.

The probe did not set local device trust, verify either device, or change key-sharing policy. Passing under the pinned SDK’s defaults shows this same-account device flow worked on these fixtures; it does not establish that an independently controlled recipient is verified or that a production key-sharing policy is safe. It is not production E2EE approval.

## Reproduction

From the repository root, with the two live disposable homeservers represented by the profile file:

```powershell
$env:CARGO_HOME = (Resolve-Path .local/p0/cargo-home).Path
$env:CARGO_TARGET_DIR = (Join-Path (Get-Location) '.local/p0/target-storage')
$env:AR_x86_64_pc_windows_msvc = (Get-Command llvm-lib.exe).Source
cargo fmt --manifest-path probes/p0/matrix/Cargo.toml --check
cargo clippy --locked --manifest-path probes/p0/matrix/Cargo.toml --bin encrypted_smoke -- -D warnings
cargo test --locked --manifest-path probes/p0/matrix/Cargo.toml
cargo build --locked --manifest-path probes/p0/matrix/Cargo.toml --bin encrypted_smoke
& .local/p0/target-storage/debug/encrypted_smoke.exe
```

The executable reads the profile locally and prints only the fixture name/version, stage outcomes, and pass/fail status. It accepts only `http://` URLs with a literal loopback IP, explicit port, and bare origin; it disables proxy use and redirects for its fixture-only HTTP client. The account name must be exactly `p0_alice`. Profile credentials, access tokens, and encryption keys are not printed. The executable continues to the next profile if one server fails so both outcomes are available in one run.

On 2026-10-19, `cargo fmt --check`, `cargo clippy --bin encrypted_smoke -- -D warnings`, and `cargo build --bin encrypted_smoke` completed successfully with the locked graph. `cargo test` completed with 9 library tests and 1 integration test passing; the binary and doc-test targets contained no tests. The live executable exited 0 with both server results shown above. The first ordinary Windows `aws-lc-sys` build attempt failed while MSVC `lib.exe` tried to overwrite its own archive (error 5). Setting the target archiver to the installed `llvm-lib.exe` allowed the build to complete without changing the selected crypto backend. Rustc also emitted a non-fatal incremental-compilation finalization access-denied warning during the test build.

## Limits

This probe uses one account with two devices, not two independently controlled accounts. It does not test user-driven verification, cross-signing, device replacement, key mismatch, restart/recovery, room-key persistence, or behavior when key sharing is refused. It only demonstrates this versioned fixture path. Each room and device was created on the disposable fixture and the device logout requests succeeded.

Each SDK operation is awaited directly; the probe creates no long-running sync loop or detached task. A timeout around an SDK future bounds the caller’s wait but does not by itself prove that SDK-internal work spawned before cancellation has drained. The successful run did not hit those timeouts, and this probe is not a general SDK shutdown/drain verification.

Root independently rebuilt this binary into `.local/p0/matrix-target`, repeated both live checks with exit 0, and then stopped the disposable runner with exit 0. Its servers and temporary databases were torn down. This encrypted check does not exercise native Sliding Sync.

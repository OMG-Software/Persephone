# Encrypted-store restart and session restoration

Checked 19 October 2026 using Matrix Rust SDK/SQLite 0.19.1, a Windows Rust
executable, and disposable Merovingian 0.12.29 and Synapse 1.163.0 in WSL
Ubuntu 24.04. No production instance was contacted. The
[sanitized machine-readable result](encrypted-restart.json) records each phase,
its distinct process ID, source/lock hashes, historical failures and limits.
This is feasibility evidence; no production session service or Tauri host exists.

## Result

All five worker processes passed on **both servers**:

| Process | Assertion |
| --- | --- |
| Seed | Two independent accounts, initially unverified devices, cancelled negative SAS control, mutual SAS verification, native sync delivery of encrypted messages in both directions, encrypted session checkpoints |
| Wrong key | Both encrypted account stores reject different keys |
| Missing key | Both stores reject missing keys; a fresh path is not created and plaintext storage is not selected |
| Offline restoration | A fresh process restores the original sessions, users, devices, own/peer public keys and mutual verification; local room keys decrypt the saved exact old event IDs, senders and bodies before sync |
| Online resumption | Another fresh process restores the same sessions without password login or SAS, then delivers new exact decrypted messages through native sync in both directions and logs out both accounts |

Each seed completed 18 native pair rounds, and each online resumption completed
3; all had zero classic rounds. Distinct worker PIDs and terminated child
processes prevent the prior process's in-memory Olm machine from satisfying
the restart check. Correct-key offline and online recovery after the two key
rejection processes also demonstrates that those refusals did not destroy the
tested identities, trust or room keys.

The offline worker owns a bound, non-listening ephemeral loopback TCP socket
for its entire recovery check. Its client points only there, with proxies,
redirects and well-known lookup disabled. A connection control must fail.
The worker creates no sync engine and performs no history fetch. SDK background
initialization may attempt unsuccessful requests; this does not assert zero
connection attempts or establish an OS network sandbox. Successful key retrieval
from a homeserver cannot satisfy this local-decryption check.

## Persistence and key handling

The Rust fixture opens encrypted state, event-cache, media and crypto SQLite
stores with the same per-account random key. It rejects a missing key **before
opening any store**. The fixture controller keeps two separate random 32-byte
keys in memory and supplies them through child stdin only. Resume profiles
contain only the server name, version and loopback origin, so restored workers
do not receive account passwords. Python only starts workers and validates their
sanitized reports; Matrix networking, sessions, verification, encryption,
storage and decryption remain Rust SDK operations.

Session tokens and checkpoint data use the SDK **CryptoStore custom-value API**,
which encrypts bytes with the configured store cipher. The SDK StateStore
custom-value API stores raw values even when other structured state is encrypted;
it must not receive raw session secrets. This distinction was checked in the
pinned SQLite implementation (`crypto_store.rs` get/set_custom_value and
`state_store.rs` get/set_custom_value). No custom application cryptography was
introduced.

At setup and online resumption, scans of 24 store files per server found none
of the tested access-token bytes, store-key bytes or old/new message markers.
The focused checkpoint regression also checks an unencrypted store as a positive
control for marker detection. These checks do not establish encryption of every
metadata field, filename, record type or possible plaintext copy. The fixture
key provider is not an OS credential vault, and Python/Rust/SDK memory copies,
pagefiles and crash dumps are outside these checks; no secure erasure is claimed.

## Reproduction and verification

Start fresh disposable homeservers using the
[runner instructions](verified-recipients.md#reproduction), wait for the profile,
then use a new output directory:

```powershell
$env:CARGO_HOME = (Resolve-Path .local/cache/cargo).Path
cargo build --locked -p persephone-test-matrix --bin verified_recipients
python tools/matrix_restart.py .local/runs/your-servers/profile.json .local/runs/your-restart
```

The recorded final run used `.local/runs/encrypted-restart-01/processes-03`,
calling the same `tools.matrix_restart.run_harness` through an ignored diagnostic
wrapper that only prints fixed stage labels on failure. Every child exited 0;
the controller exited 0. Keys were not saved. The controller limits the entire
child invocation, including stdin, to 180 seconds; the worker's asynchronous
phase has a separate 120-second timeout. Standalone worker stdin reads rely on
the controller closing the pipe and enforcing that outer process deadline.

`python tools/dev.py check --offline` completed with exit 0: **62 tests passed
(52 Rust, 3 frontend, 2 server-registration and 5 restart-controller tests)**,
plus formatting, strict Clippy, frontend check/build and contract/TypeScript
checks. Four new Rust checks cover key/checkpoint handling and the reserved
offline origin; five Python checks cover the process protocol, credential
removal, key pipes, failed stages, invalid reports and reused PIDs. The existing
nonfatal ts-rs attribute warning remains; Rust checks enforce deserialization.

Both prior messaging scenarios were rerun successfully on both servers:
native encrypted sync with a completed-operation classic switch, and
classic-only verified-recipient messaging. The disposable runner exited 0 and
both WSL listeners independently returned connection refused after cleanup.

Adding direct SDK-store, random, zeroizing-buffer, socket and test-tempfile
dependencies changed local workspace dependency edges intentionally. The 418
registry package name/version/source/checksum tuples match the pre-change lock.
No new advisory query or production dependency approval is claimed.

## Failures retained and limits

The first two live attempts rejected the offline phase because the fixture
assumed port 9 was unused, but it accepted connections on this machine. Neither
attempt reached restoration. The final implementation reserves its own port
without listening; the full final sequence passed on both servers.

The first aggregate check exited 101 because Cargo tried to replace the Windows
executable while the independent regression run was using it. After that run
finished, the sequential aggregate check exited 0. These failures are retained
in the local run logs and machine-readable record rather than counted as passes.

This tests normal process exit after completed operations. Crash consistency,
corruption, device replacement, forgotten-key recovery, cross-signing, real user
verification and the production key-sharing policy remain open. Native sync uses
a fresh position after restart; persistence of Sliding Sync positions is not
claimed. Full SDK task drain, lock/logout ownership and capability-based fallback
need separate evidence. Windows media network containment remains unresolved;
packaged native Linux/macOS desktop controls were not tested. P0 remains open.

# Independent recipients and SAS-verified devices

> Historical classic-only run record. Source hashes below describe the program
> before the Sliding Sync extension. The current program also supports
> [native encrypted Sliding Sync and a classic transition](encrypted-sliding-sync.md),
> with new source hashes and a fresh classic-only regression in that record.

**Run date:** 19 October 2026. **SDK:** `matrix-sdk` 0.19.1, with defaults
disabled and the existing `e2e-encryption` / `rustls-aws-lc-rs` feature selection.
The live program ran outside the tool sandbox on Windows, against fresh
disposable Merovingian 0.12.29 and Synapse 1.163.0 instances in WSL. No
production accounts or server state were used.

## Result and assertions

The [integration program](../../tests/matrix/src/bin/verified_recipients.rs)
passed on both homeservers in two fresh runs. [Machine-readable evidence](verified-recipients.json)
records results, verification commands, input hashes and run limitations.

Each homeserver gets two separately registered accounts (`p0_alice` and
`p0_bob`) with distinct generated passwords and one SDK device per account.
The program asserts distinct authenticated user IDs, creates a private encrypted
room, invites Bob and joins through Bob's own authenticated client.

For each pair it establishes all of the following:

1. Each device initially considers its peer unverified. A cancelled verification
   request is delivered to the initiator and leaves both peers unverified.
2. A fresh SDK `m.sas.v1` request/ready/start/accept flow identifies the expected
   independent peer user and device from both sides.
3. Both sides can present SAS, but neither peer is trusted yet. The trusted test
   controller compares both decimal strings and refuses missing or unequal
   values before either confirmation.
4. Both clients confirm; both SAS flows and request objects reach done. Fresh
   SDK device lookups report mutual verification before either message is sent.
   The program never calls a local-trust setter or manually verifies a device.
5. Alice sends to Bob and Bob sends to Alice. For each send, recipient-side raw
   history contains the exact event ID and sender as `m.room.encrypted` using
   `m.megolm.v1.aes-sha2`, without a plaintext `body`. SDK-decrypted history must
   match the same event ID, sender and exact synthetic body.
6. Both authenticated logout requests succeed. After both clients authenticate,
   both logout requests are attempted on scenario failure before reporting the
   failed result. A client that never authenticated has no logout request to send.

All calls and verification progress have timeouts. Test output contains stage
outcomes rather than passwords, access tokens, keys or SAS values. SDK failures
are classified using HTTP status or a fixed error category without retaining
their request URLs or response bodies.

## Reproduction

Start the [disposable runner](../../tools/servers/run.py) in a separate terminal
with fresh output. The two local prerequisites are described in
[Development](../development.md#disposable-homeservers).

```powershell
wsl -d Ubuntu-24.04 -u root -- /usr/bin/setpriv --bounding-set=-all --inh-caps=-all --ambient-caps=-all --reuid=1000 --regid=1000 --init-groups --reset-env --no-new-privs /usr/bin/python3 /mnt/c/dev/Persephone/tools/servers/run.py --merovingian-binary /mnt/c/dev/Persephone/.local/servers/merovingian/merovingian-server --synapse-python /mnt/c/dev/Persephone/.local/servers/synapse/bin/python --server both --output /mnt/c/dev/Persephone/.local/runs/verified-recipients
```

Wait for both `READY` lines and `PROFILE`. The runner starts as a non-root UID
with empty capability sets and `no_new_privs`; the `-u root` launch only permits
`setpriv` to establish that restricted context. Then, from the repository root:

```powershell
$env:CARGO_HOME = (Resolve-Path .local/cache/cargo).Path
cargo build --offline --locked -p persephone-test-matrix --bin verified_recipients
& .local/build/rust/debug/verified_recipients.exe .local/runs/verified-recipients/profile.json .local/runs/verified-recipients/results.json
python tools/dev.py check --offline
```

Use the LLVM librarian workaround from Development if required on Windows.
Interrupt the runner normally after the test so its servers stop and temporary
databases are removed. Profiles and unredacted server logs stay under ignored
`.local/`; publish only the sanitized result artifact. Use only profiles from a
fresh trusted runner: loopback URL/account checks prevent common mistakes but
do not authenticate an arbitrary caller-supplied profile's origin.

## Driver failures and limits

Two initial Windows debug runs aborted with `STATUS_STACK_OVERFLOW`
(`0xC00000FD`, signed exit `-1073741571`) after joining the room. Boxing the outer
async phases alone did not resolve it. The second run also recorded a
Merovingian login failure whose underlying cause was not captured by the old
stage-only diagnostics. These runs are failures, not partial passes; their
disposable instances were stopped.

The final test driver uses one explicitly sized 8 MiB thread and a current-thread
Tokio runtime. This accommodates the nested debug SDK verification path; it is
not an application stack/memory measurement or a media-worker sandbox change.
The passing result uses this driver. Windows SDK stack behavior remains a
product integration consideration. The full check includes the two SAS/profile
guard tests and two server registration/session/redaction tests.

The controller's direct comparison simulates a trusted out-of-band comparison.
No human verification UI, cross-signed user identity, device replacement,
malicious-key substitution, recovery/restart, denied room-key sharing, SDK-wide
task drain or federation is tested here. The devices use separate in-memory
stores; SDK room-key sharing settings remain the existing test defaults.
Verification here means mutual SDK device trust resulting from completed SAS,
not cross-signing persistence. These historical runs used classic sync. The
subsequent [encrypted Sliding Sync check](encrypted-sliding-sync.md) exercises
native sync and the completed-operation classic transition. P0 remains open.

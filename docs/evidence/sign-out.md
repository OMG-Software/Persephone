# Sign-out: remote revocation and local deletion

Checked 21 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
Matrix Rust SDK 0.19.1, against disposable Merovingian 0.12.29 and Synapse
1.163.0 in WSL Ubuntu 24.04. **Result: on both servers the app signed in,
signed out after a confirmation, deleted the store key at once, and wiped the
store files on the next start, which opened a new, signed-out profile.** Synapse logged the logout request; Merovingian does not log request
paths, and the core reported no revocation failure.

## Design

Owner decisions (21 October 2026): sign-out deletes this device's local profile,
and a failed revocation still signs out locally, with a warning.

* **Order** ([`session.rs`](../../crates/core/src/session.rs) `sign_out`):
  1. Under a lease, write the `signed-out` marker durably as an intent
     (`revoking`), then ask the homeserver to revoke the session
     (`POST /logout`). From here an ended process cannot restore the account:
     the next start wipes the profile. The result is kept; a failure does not
     stop the sign-out. If all leases are in use, sign-out waits for one for
     up to 15 s; after that nothing has changed and the call fails with
     `limit_exceeded`, which the page reports.
  2. Record the result in the marker (`revoked` or `not-revoked`), then delete
     `store-key.v1`. From here the encrypted stores cannot be read again, even
     if the process stops.
  3. Lock as usual: close admission, join the sync loop, drop the client inside
     the runtime and close the stores. The state ends `restart_required`,
     because an SDK client ran.
  4. Leave the store files. The SDK's background work is not proven stopped
     in this process ([SDK lifecycle](../architecture/sdk-lifecycle.md)), so
     its files are not deleted under it. The next start wipes them before any
     client exists.
* **Wipe** ([`store.rs`](../../crates/core/src/store.rs)): opening a
  profile with the marker wipes the store, the key and its temporary file
  before anything else, and deletes the marker last. If the marker's presence
  cannot be read, the profile does not open. A crash at any point
  repeats the wipe; it never opens the old stores. If the marker cannot be
  written, the key is still deleted: a store without its key opens as
  `key_missing`, never as a new or signed-in profile.
* **Reporting**: the account becomes `signed_out`. `session_not_revoked` means
  the homeserver may not have been told; `failed` means the local deletion did
  not finish. A start that wipes a profile whose marker does not record a
  confirmed revocation (an interrupted sign-out, a failed request, or an
  unreadable marker) also reports `session_not_revoked`, on the sign-in page.
  The page shows each case in fixed text and never reports revocation when the
  request failed or its outcome is unknown.
* **UI**: the header's "Sign out" button asks once more ("Sign out and delete
  this device's keys and local data? …") before the call. The command takes no
  arguments; the IPC schema refuses extra fields such as `keep_data`.

## Tests

New, all passing (the full check passed 258 tests after the review fixes; see the [ledger](verification.json)):

* Store: a signed-out profile is wiped on open and starts fresh, including a
  wipe that stopped before the marker went; a marker that cannot be written
  still deletes the key, and the profile stays closed; an interrupted sign-out
  (intent only, key still present) is wiped rather than opened, and the open
  reports an unconfirmed revocation, as it does for a failed one but not for a
  confirmed one.
* Account (mock homeserver, development feature): sign-out revokes the session,
  deletes the key, leaves the store files and ends `restart_required`, and the
  next start wipes them and opens a fresh profile; a logout that fails with 500
  still signs out and reports `session_not_revoked`; with every lease in use,
  sign-out reports that it did not start, and completes once a lease frees;
  with a slow logout, the intent is on disk (and the key still present) while
  the request is in flight, which fails if the intent is written after the
  request; a sign-out interrupted after its intent is not restored on the next
  start, asks for nothing and reports `session_not_revoked`; without an
  account, sign-out changes nothing.
* IPC: the `sign_out` command is allowlisted and relayed, and its capability
  is listed. Protocol: `sign_out` with an extra field is refused.

## Live runs

The development core (`dev-disposable-servers`) signed in from the harness
profile; the probe drove the page through DevTools, then restarted the app
without the development source.

| Step | Merovingian 0.12.29 | Synapse 1.163.0 |
| --- | --- | --- |
| Signed in | `@p0_alice:…` | `@p0_alice:…` |
| Confirmation shown before sign-out | yes | yes |
| After sign-out | signed-out text, `restart_required` | same |
| Revocation warning | none | none |
| Logout request in the server log | not logged (no request paths) | 1 `POST /_matrix/client/v3/logout` |
| Key file after sign-out, before exit | deleted | deleted |
| Profile after exit | `profile.lock` only (the run deleted the store files in-process; see below) | same |
| After restart | new profile, sign-in form, `Local store protected` | same |

Each server passed on the first attempt. These runs used the first version,
which also deleted the store files after the stores closed. Review fixes since
then leave the files for the next start, wait for a lease when all are in use,
and delete the key even when the marker cannot be written; the account and
store tests cover them, and the second live run below checks the deferred
wipe. See the [ledger](verification.json).

### Second run, after the review fixes

Same probe and servers (fresh harness), with the store files left to the next
start:

| Step | Merovingian 0.12.29 | Synapse 1.163.0 |
| --- | --- | --- |
| After sign-out | "keys were deleted … the rest … is deleted as it starts", no warning, key file gone | same |
| Logout request in the server log | not logged | 1 |
| Profile after exit | `profile.lock`, `signed-out`, `store/` | same |
| After restart | marker gone, new key and store, sign-in form, `Local store protected` | same |
| Exit codes | 0 and 0 | 0 and 0 |

Each server passed on the first attempt. A later review fix records the
intent before the logout request and reports an unconfirmed revocation after an
interrupted sign-out; the account and store tests cover it, and it was not run
live.

## Limits

* The live runs do not show that the old access token is refused afterwards;
  only Synapse's log shows the request, and the mock tests show the core's
  handling of success and failure.
* Logout uses the core's 30-second request timeout. A homeserver that never
  answers holds the sign-out, and other calls to the core, for up to that long.
* Until the next start the encrypted store files stay on disk, unreadable
  without the deleted key. Deleting files does not erase their contents from
  the disk either. The encrypted
  stores are unreadable once the DPAPI-wrapped key is deleted, but DPAPI blobs
  and SQLite pages may remain in free space, and the SDK's in-memory copies
  are not shown to be zeroed.
* There is no key backup or recovery yet (P4), so encrypted history this
  device could read is lost; the confirmation says so.
* Signing in again needs a restart, as after any lock once a client has run.
  Signing in as a different user after sign-out is not tested.
* On Unix the marker's directory entry and the key's removal are also synced
  to disk; that code path has not been built or run, because only Windows has
  a vault so far.
* Debug builds on one Windows host only.

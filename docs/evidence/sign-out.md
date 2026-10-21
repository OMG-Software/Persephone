# Sign-out: remote revocation and local deletion

Checked 21 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
Matrix Rust SDK 0.19.1, against disposable Merovingian 0.12.29 and Synapse
1.163.0 in WSL Ubuntu 24.04. **Result: on both servers the app signed in,
signed out after a confirmation, deleted the store key at once and the store
files after the stores closed, and started as a new, signed-out profile after a
restart.** Synapse logged the logout request; Merovingian does not log request
paths, and the core reported no revocation failure.

## Design

Owner decisions (21 October 2026): sign-out deletes this device's local profile,
and a failed revocation still signs out locally, with a warning.

* **Order** ([`session.rs`](../../crates/core/src/session.rs) `sign_out`):
  1. Revoke the session on the homeserver (`POST /logout`) under a lease. The
     result is kept; a failure does not stop the sign-out.
  2. Write the `signed-out` marker durably, then delete `store-key.v1`. From
     here the encrypted stores cannot be read again, even if the process stops.
  3. Lock as usual: close admission, join the sync loop, drop the client inside
     the runtime and close the stores. The state ends `restart_required`,
     because an SDK client ran.
  4. Delete the store files, then the marker.
* **Crash safety** ([`store.rs`](../../crates/core/src/store.rs)): opening a
  profile with the marker wipes the store, the key and its temporary file
  before anything else, and deletes the marker last. A crash at any point
  repeats the wipe; it never opens the old stores. If the marker cannot be
  written, the key is still deleted: a store without its key opens as
  `key_missing`, never as a new or signed-in profile.
* **Reporting**: the account becomes `signed_out`. `session_not_revoked` means
  the homeserver was not told; `failed` means the local deletion did not
  finish. The page shows each case in fixed text and never reports revocation
  when the request failed.
* **UI**: the header's "Sign out" button asks once more ("Sign out and delete
  this device's keys and local data? …") before the call. The command takes no
  arguments; the IPC schema refuses extra fields such as `keep_data`.

## Tests

New, all passing (the full check passed 254 tests; see the [ledger](verification.json)):

* Store: a signed-out profile is wiped on open and starts fresh, including a
  wipe that stopped before the marker went; a marker that cannot be written
  still deletes the key, and the profile stays closed.
* Account (mock homeserver, development feature): sign-out revokes the session,
  wipes the profile and ends `restart_required`; a logout that fails with 500
  still wipes and reports `session_not_revoked`; without an account, sign-out
  changes nothing.
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
| After sign-out | "Signed out, and this device's local data was deleted", `restart_required` | same |
| Revocation warning | none | none |
| Logout request in the server log | not logged (no request paths) | 1 `POST /_matrix/client/v3/logout` |
| Key file after sign-out, before exit | deleted | deleted |
| Profile after exit | `profile.lock` only | same |
| After restart | new profile, sign-in form, `Local store protected` | same |

Each server passed on the first attempt. The runs used the working tree before
one later fix, which deletes the key even when the marker cannot be written;
that fix and its page text change only that failure path, which the store test
covers. See the [ledger](verification.json).

## Limits

* The live runs do not show that the old access token is refused afterwards;
  only Synapse's log shows the request, and the mock tests show the core's
  handling of success and failure.
* Logout uses the core's 30-second request timeout. A homeserver that never
  answers holds the sign-out, and other calls to the core, for up to that long.
* Deleting files does not erase their contents from the disk. The encrypted
  stores are unreadable once the DPAPI-wrapped key is deleted, but DPAPI blobs
  and SQLite pages may remain in free space, and the SDK's in-memory copies
  are not shown to be zeroed.
* There is no key backup or recovery yet (P4), so encrypted history this
  device could read is lost; the confirmation says so.
* Signing in again needs a restart, as after any lock once a client has run.
  Signing in as a different user after sign-out is not tested.
* Debug builds on one Windows host only.

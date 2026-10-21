# Native sign-in and real rooms in the core

Checked 21 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
Matrix Rust SDK 0.19.1, against disposable Merovingian 0.12.29 and Synapse
1.163.0 in WSL Ubuntu 24.04. P3 first slice: the core signs in through a
**native, core-owned credential prompt**, keeps the session in the encrypted
store, syncs (Sliding Sync preferred, classic fallback) and serves real rooms
and messages through the existing bounded views. **Result: on both servers, and
on classic-only Synapse, the app signed in without the page ever handling a
password, showed real rooms and history, received a new message live, locked
to `restart_required`, and restored the session after a restart without asking
again.** Encrypted messages from other devices are not decrypted yet (no key
sharing or verification in this slice); they show as notices.

## Design

* **Homeserver** ([`homeserver.rs`](../../crates/core/src/homeserver.rs)): the
  page sends only a URL. The core accepts `https://` with a host and nothing
  else (no user info, path, query or fragment, at most 2,048 bytes). Every
  connection uses TLS 1.3 or later, no proxy and no redirects; well-known
  lookup is off, so the client talks only to the named server.
* **Credentials** ([`credentials.rs`](../../crates/core/src/credentials.rs)):
  the core shows Windows' own credential dialog
  (`CredUIPromptForWindowsCredentialsW`, generic credentials, no save option)
  with the homeserver host in its text. The page and the UI process never see
  the username or password. Unpacked buffers and the dialog's buffer are zeroed;
  the SDK's copy in the login request is outside that guarantee. Other
  platforms report `unsupported`.
* **Account** ([`account.rs`](../../crates/core/src/account.rs),
  [`session.rs`](../../crates/core/src/session.rs)): sign-in runs on a worker
  thread. The prompt holds no lease (it may stay open); building the client,
  logging in and saving the session hold one. The session (tokens and device)
  is saved as an encrypted crypto-store custom value, never in the state
  store's unencrypted custom values. A restart restores it without a prompt.
* **Sync**: capability discovery chooses native Sliding Sync when advertised,
  otherwise classic sync; a discovery error is reported and never treated as
  absence. One lease per complete round; a lock is noticed only between rounds.
  Rounds start at most every 500 ms, so a server that answers at once cannot
  make the core spin. Rooms touched by a round become content-free
  `room_changed` events for subscribed rooms and one coalesced
  `room_list_changed` event.
* **Views** ([`matrix_views.rs`](../../crates/core/src/matrix_views.rs)):
  names, senders and bodies go through the existing projections. Text, notice
  and emote messages are shown; other message types become an `unsupported`
  notice and events this device cannot decrypt an `undecryptable` notice;
  state events and reactions are not shown. Pages keep the 100-item and
  112 KiB limits; a chunk over the byte budget is fetched again with half the
  limit rather than cut. Pagination tokens stay in the core; the UI gets
  `m1.<n>` cursors for at most 64 tokens.
* **Lock** (owner decision): once a Matrix client has run, lock closes
  everything and ends `restart_required`; unlock is refused in that process,
  because the SDK cannot yet prove its background tasks stopped
  ([lifecycle contract](../architecture/sdk-lifecycle.md)).
* **Development builds** (owner decision): the off-by-default
  `dev-disposable-servers` feature accepts `http://127.0.0.1:<port>` and
  `http://[::1]:<port>` only, and lets `PERSEPHONE_DEV_PROFILE` point at the
  harness profile instead of showing the prompt. Such a build prints
  `DEVELOPMENT BUILD` on start. Default, CI and release builds refuse plain
  HTTP and have no credential source but the native prompt.
* **UI**: one new command, `sign_in { homeserver }`, in the allowlist and
  capability. `app_status` gains the account (state, user ID, error code).
  The page has one input (the homeserver URL); its text says Windows asks for
  the password.

## The native prompt and automation

The credential dialog is drawn by `CredentialUIBroker`; UI Automation sees
only its window (`Credential Dialog Xaml Host`), not its fields or buttons.
Other programs therefore cannot read or fill it without injecting keystrokes,
which was rejected (owner decision): they could land in another window. The
live runs below use the development credential source; the real dialog is
checked by hand. A `credential_prompt` example
(`cargo run -p persephone-core --example credential_prompt -- <host> [--compare <harness profile> <server>]`)
shows the dialog once and prints only lengths and yes/no comparisons with the
disposable account, never the entries.

### Hand check of the real dialog

The owner signed in through the real Windows dialog against classic-only
Synapse, with a build that had no development credential source. After the
check, the page showed `Signed in as @p0_alice:synapse.p0.test` and both seeded
rooms, had no password field, and the server logged the login as `200`.

The first three attempts were refused (`403`; the server's log showed the
right username each time). Investigating them found two real faults in the
unpacking, fixed before the successful attempt:

* The dialog can return a protected (encrypted) password. Unpacking without
  `CRED_PACK_PROTECTED_CREDENTIALS` returns that encrypted form; the core now
  passes the flag, which decrypts a protected password and returns a plain one
  unchanged.
* The lengths the unpack call reports are buffer sizes and can exceed the text
  plus one terminator (10 for the 8-character `p0_alice`). Trimming one
  terminator left a NUL in the text, which the input check rejected; the core
  now cuts each value at its first NUL.

Two new Windows tests pack known credentials with
`CredPackAuthenticationBufferW` (protected and unprotected) and require the
exact username and password back. With the comparison example, the dialog then
returned an 8-character password that was not the username nor the start or
end of the account's password, so the entry, not the code, differed; the next
attempt with the account's password succeeded. Which of the two faults, if
either, affected the first attempts is not established.

## Tests

`python tools/dev.py check --offline` exited 0 and passed 227 tests (197 Rust,
15 frontend, 2 server-registration and 13 controller tests), with formatting,
Clippy for default and development features, and contract checks. It now also
runs the mock-homeserver tests with the development feature. That run used
the uncommitted working tree later committed as `a89a891` (which adds only
these counts to the docs), Rust and Cargo 1.99.1, Node.js 24.15.0, npm 11.13.0
and Python 3.14.4, `--offline` with cached dependencies; `Cargo.lock` SHA-256
`946730d6…2c35`, `frontend/package-lock.json` `a22d978b…f3e2`; its raw log
was not retained. A rerun after review fixes is recorded in the
[ledger](verification.json). New:

* Homeserver policy (4): accepted URLs and their shown host (including
  punycode); 16 rejected forms; plain HTTP refused without the feature and
  only loopback literals with it (`localhost`, `127.0.0.2`, `0.0.0.0`, LAN and
  IPv4-mapped forms always refused); the policy client builds.
* Credentials (5): decoding, empty/oversized/control/invalid text refused,
  `unsupported` without a native prompt; on Windows, protected and unprotected
  packed buffers unpack to the exact credentials.
* Matrix views (3): cursors bounded, room-bound and evicted oldest first;
  malformed cursors and room IDs refused.
* Account, against a mock homeserver (5):
  * Sign-in through the prompt; one login request; rooms, names and a page
    with an undecryptable and an unsupported notice and inert markup; cursor
    paging and `not_found` for an unknown cursor; a subscribed room's sync
    becomes one event; rounds start at most every 500 ms; lock →
    `restart_required` and unlock refused; neither the access token nor the
    password is in any profile file; a restart restores without asking.
  * A refused password → `invalid_credentials`; a cancelled prompt →
    `cancelled` with no login request.
  * Bad URLs and a missing profile are refused before any prompt.
  * Advertised Sliding Sync is used and classic is not.
  * A failed capability discovery fails sign-in with no login and no sync of
    either kind.
* Core, UI and frontend: the hub drops room-list events with the session;
  `sign_in` relays only the URL, refuses oversized input before the core and
  checks replies against the contract; the session channel carries `session`
  and `rooms`; the sign-in form has one input and no password field; notices
  render as fixed text with an isolated sender; merging deduplicates notices.

## Live runs

Disposable servers from [`tools/servers/run.py`](../../tools/servers/run.py),
started as described in the [verified-recipient evidence](verified-recipients.md#reproduction).
A script seeded each server over the Matrix API: a plain room with three text
messages and an `m.image`, and an encrypted room with a ciphertext this device
has no key for. The real core and UI were then driven over DevTools; the
second account posted a message with a unique text while the room was open.
The runs used the uncommitted working tree before `a89a891` (exact tree not
recorded), the same lockfiles as that commit and a debug core built with
`dev-disposable-servers`; server logs stay under the ignored `.local/runs`.

| Check | Merovingian 0.12.29 | Synapse 1.163.0 | Synapse classic-only |
| --- | --- | --- | --- |
| Password fields in the page | 0 | 0 | 0 |
| Signed in as | `@p0_alice:…` | `@p0_alice:…` | `@p0_alice:…` |
| Rooms, with names | both | both | both |
| Plain room history | 3 messages, image notice, markup inert | same | same |
| Live message (absent before posting) | 0.4 s | 0.2 s | 0.4 s |
| Encrypted room | undecryptable notice | same | same |
| Lock | `restart_required`, nothing shown | same | same |
| Restart without the dev source | restored, no dialog, no form | same | same |
| Sync mode | Sliding (advertised; Merovingian's log does not record request paths) | Sliding: 10 sliding, 0 classic requests | Classic: 0 sliding, 12 classic requests |

The first Merovingian run showed two problems, both fixed before the runs in
the table: the room list stayed empty after sign-in (the page loaded it once,
before the first sync; the core now sends `room_list_changed`), and rooms were
named "2 people" (the sliding list requested no room state; it now requests
name, alias, encryption, create and own membership). Several runs lost the
DevTools connection partway and were repeated with a fresh app profile. After
the live runs, one change stopped synthetic demo messages (no profile) from
raising `room_list_changed`; the Matrix path the live runs used is unchanged.

## Limits

* No end-to-end encryption beyond the SDK's own device keys: no key sharing,
  verification, recovery or backup UI (P4). Messages from other devices in
  encrypted rooms are undecryptable notices.
* No sign-out or remote revocation; no SSO; no `.well-known` discovery.
* The SDK's background tasks are not stopped and joined; lock therefore ends
  `restart_required` once a client has run.
* The real credential dialog is checked by hand only; see above.
* WebView2 intermittently failed to start in this session: its browser
  processes started and exited within about 10 s, leaving a blank window, with
  nothing on stderr and no error report (the UI's Job disables error reporting
  for its processes). It happened with and without `--disable-gpu`, from two
  launch paths, and a relaunch usually worked; the owner sees similar
  behaviour in other WebView2 applications on this host. The cause is not
  established; the UI shows a blank window rather than an error when it
  happens.
* Sending messages, media, room-list ordering by activity and read markers are
  not built.
* Debug builds on one Windows host only; Linux and macOS have no prompt.

# Protected local store in the core

Checked 21 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
Matrix SDK SQLite stores 0.19.1. P2 "vault and storage adapters", first slice:
the core owns one local profile, unwraps its store key through the OS vault
and opens the SDK's encrypted stores only while it holds that key. **Result:
every tested path that cannot produce the right key leaves the profile locked
and byte-for-byte unchanged, and no path creates a replacement key for an
existing store.** No Matrix client, account or homeserver uses these stores
yet; they hold only a synthetic test value.

## Design

* **Key** ([`vault.rs`](../../crates/core/src/vault.rs)): 32 bytes from the OS
  generator (`getrandom`), held in zeroed-on-drop memory. Never derived from a
  password and never saved unwrapped.
* **Vault (Windows)**: DPAPI for the current user (`CryptProtectData`,
  `CRYPTPROTECT_UI_FORBIDDEN`) with application-specific entropy, so a blob
  wrapped by another program does not unwrap here. DPAPI's output buffer is
  zeroed before it is freed. DPAPI does not prompt: any process running as the
  same Windows user can unwrap the key. The vault protects a copied or stolen
  profile, not a compromised account. Other platforms refuse (`unsupported`)
  until their vault is built.
* **Profile** ([`store.rs`](../../crates/core/src/store.rs)):
  `profile.lock`, `store-key.v1` (a magic line plus the DPAPI blob, at most
  4 KiB) and `store/` (the SDK's state, event-cache, media and crypto
  databases, all opened with the same key). The core holds an exclusive lock
  on `profile.lock` for as long as the stores are open.
* **Fail-closed rules:**

  | Profile | Result |
  | --- | --- |
  | New: no key, empty store directory | Key created and wrapped; the saved file must unwrap to the same key before the stores are created |
  | Key present | Unwrapped; the crypto store is opened first and must accept it |
  | Store present, key absent | `key_missing`; no key is made |
  | Key file unreadable, oversized, without the magic line, or refused by DPAPI | `key_unreadable` |
  | Stores refuse the key | `key_rejected` |
  | Profile open elsewhere | `profile_in_use` |
  | No vault on this platform | `unsupported`; no key or store is created |

* **Ownership:** `persephone --profile <dir>` opens the profile before starting
  the UI. Without `--profile` nothing is stored (`not_configured`). On exit the
  core closes each store explicitly and only then releases the profile lock;
  a failed close makes the core exit with status 1.
* **UI:** `app_status` now carries `store`, one of the codes above. The page
  maps each code to fixed text. No path, vault detail or error text crosses
  the boundary. The contract rejects unknown codes and a missing field.

## Tests

`python tools/dev.py check --offline` passed 196 tests (169 Rust, 12 frontend,
2 server-registration and 13 controller tests), with formatting, Clippy and
contract checks. New, in [`crates/core/tests/store.rs`](../../crates/core/tests/store.rs),
against real SDK SQLite stores:

* With a deterministic stand-in protector (runs on every platform):
  * A new profile saves a wrapped key; a marker written through the encrypted
    crypto store reads back after reopening.
  * Key deleted: `key_missing`, no key file created, every profile file
    unchanged.
  * Key wrapped for another context, magic line damaged, truncated, oversized
    or empty: `key_unreadable`, profile unchanged. A key that unwraps to wrong
    bytes: `key_rejected`. Restoring the original key file recovers the
    marker.
  * Another profile's key: `key_rejected`, profile unchanged, then the original
    key recovers the marker.
  * A second owner while the profile is open: `profile_in_use`; it opens once
    the first closes.
  * No vault: `unsupported`, no key or store created.
  * Interrupted first runs (a torn temporary key file, or a key with no store)
    complete normally.
* With real DPAPI:
  * No file in the profile, including the WAL and SHM sidecars, contains the
    raw key bytes or the marker.
  * A blob DPAPI cannot unwrap (different entropy, standing in for another
    program or user) leaves the profile locked and unchanged; the right
    entropy recovers the marker.
  * Wrapping the same key twice gives different blobs; a one-byte change makes
    unwrapping fail.
* Contract: `store` is required, unknown codes are refused.

## Real application walkthrough

The real core and UI were run with a scratch profile. The status line was read
over DevTools, and each run ended with a normal window close, so the core
closed its stores itself:

| Run | Status line | Exit |
| --- | --- | --- |
| First run | "Local store protected" | 0 |
| Reopen | "Local store protected" | 0 |
| Second instance while the first is open | "Local store locked: open in another window"; the first still "protected" | 0 and 0 |
| Key file deleted | "Local store locked: its key is missing"; every profile file unchanged, no new key | 0 |

After each normal close only the four main database files remained; the SQLite
WAL and SHM sidecars were gone, consistent with the SDK's explicit close
checkpointing them.

## Limits

* Same-user access: on Windows any process running as the user can unwrap the
  key. There is no unlock prompt, and the security design's account
  reauthentication and recovery remain separate work.
* The SDK SQLite stores encrypt values, not whole files: schema, table names
  and some keys stay visible (see the [P0 storage evidence](storage.md)).
* No Matrix client uses the stores yet, so SDK background tasks, the
  [lifecycle contract](../architecture/sdk-lifecycle.md) and store handoff
  between accounts are not exercised. Closing four idle stores is not
  evidence of an SDK-wide drain.
* Lock and logout while the app runs, key rotation, corruption recovery,
  backups and secure deletion are not built.
* Linux Secret Service and macOS Keychain adapters are not built; those
  platforms report `unsupported`.
* Debug builds on one Windows host only.

## Dependencies

The core now depends directly on `matrix-sdk-sqlite` (with `bundled` and
`crypto-store`), `matrix-sdk-base` and `matrix-sdk-crypto` 0.19.1, `tokio`
1.53.3 (`rt-multi-thread`), `getrandom` 0.4.3 and `zeroize` 1.9.1, plus
`tempfile` 3.27.0 for tests. All of these were already in the root lockfile
at the same versions for the storage and Matrix test programs; the lockfile
changed only in the core's dependency list. `windows-sys` gains the
`Win32_Security_Cryptography` feature. The UI crate gains none of them.

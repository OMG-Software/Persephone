# P0 storage and credential evidence

> Historical run record: commands and source paths below describe the pre-restructure checkout. See [Development](../development.md) for current paths and commands.

**Date:** 2026-10-19  
**SDK candidate:** Matrix Rust SDK 0.19.1  
**Scope:** Synthetic-only inspection of the SDK SQLite state and crypto stores, source-level review of adjacent stores, and a Windows Credential Manager feasibility check.

## Decision

Matrix Rust SDK 0.19.1 is a viable **conditional** candidate for per-value local protection. Its SQLite backend encrypts configured store values and can hash selected lookup keys, but it does not encrypt the SQLite file as a whole. The database header, schema, table names, and some caller-provided keys remain visible. Treat SQLite data, WAL/SHM/journal files, copies, backups, swap, and OS snapshots as artifacts requiring review; do not describe this feature as SQLCipher or whole-file encryption.

The SDK does not enforce a locked application lifecycle. Opening an existing encrypted store with no key succeeds. A caller can then write new custom values without encryption. The app must keep every SDK store unavailable until the account’s local key has been retrieved or explicitly entered, and must keep the session locked when the vault is unavailable or locked. Never let an SDK default/no-key path become a silent fallback.

The probe found no silent reset on wrong-key failure: a wrong key failed to open the crypto store, after which the original key reopened it and recovered the generated Olm account pickle. That is useful evidence for this release, not a general corruption-recovery guarantee. Store unreadable/corrupt conditions must still surface as a locked or recovery-required state and preserve the original directory.

## Version, features, and licensing

The latest published candidate checked on the date above was 0.19.1 for `matrix-sdk-sqlite`, `matrix-sdk-store-encryption`, `matrix-sdk-base`, and `matrix-sdk-crypto`. The SQLite and store-encryption crates declare Rust 1.96 and Apache-2.0. The isolated probe compiled with Rust 1.99.1 / Cargo 1.99.1. The Matrix SDK release notes list 0.19.1 on 2026-09-18. See [matrix-sdk-sqlite 0.19.1](https://crates.io/crates/matrix-sdk-sqlite/0.19.1), [matrix-sdk-store-encryption 0.19.1](https://crates.io/crates/matrix-sdk-store-encryption/0.19.1), [SDK release notes](https://github.com/matrix-org/matrix-rust-sdk/blob/main/crates/matrix-sdk-base/CHANGELOG.md), and the [0.19.1 SQLite API](https://docs.rs/matrix-sdk-sqlite/0.19.1/matrix_sdk_sqlite/).

The isolated crate pins three direct Matrix crates at `=0.19.1`, with `matrix-sdk-store-encryption` 0.19.1 pinned transitively in the lockfile and carries a generated `Cargo.lock`. It enables SQLite’s default `state-store`, `event-cache-store`, and `media-store` features, plus `crypto-store` and `bundled`. `bundled` was needed to link SQLite in this Windows environment. The probe’s `matrix-sdk-base`, `matrix-sdk-crypto`, and `matrix-sdk-sqlite` dependencies declare Apache-2.0; `rusqlite` declares MIT; `tempfile` declares MIT OR Apache-2.0. Probe dependency metadata is not a production dependency review. The SDK crypto crate exposes `automatic-room-key-forwarding` as a non-default feature, but the root integration must still pin reviewed features explicitly rather than inherit broad feature activation through another SDK crate.

The store-encryption documentation describes XChaCha20-Poly1305 with a fresh nonce for each value and keyed BLAKE3 for supported key obfuscation. The crate itself warns that it is a low-level, “hazmat” building block. Use the SDK SQLite integration, not direct application use of `StoreCipher`; see the [0.19.1 encryption crate documentation](https://docs.rs/matrix-sdk-store-encryption/0.19.1/matrix_sdk_store_encryption/).

## Runtime probe

Current storage test sources are in [tests/storage](../../tests/storage). The pre-restructure run used a temporary directory, synthetic marker/key bytes, and the former `probes/p0/storage` path; the commands below are preserved as historical records:

```powershell
$env:CARGO_HOME = 'C:\dev\Persephone\.local\p0\cargo-home'
$env:CARGO_TARGET_DIR = 'C:\dev\Persephone\.local\p0\target-storage'
cargo test --offline --locked
```

The observed result was **7 passed, 0 failed** on Windows. Tests cover:

- SDK-generated Olm account persistence: build `OlmMachine` with fixed synthetic user/device IDs and the encrypted SQLite `CryptoStore`, load the account through the SDK, serialize its actual account pickle in memory, and derive four scan signatures from its signing/Diffie-Hellman private pickle fields. Scan the encrypted crypto database and live WAL/SHM and again after explicit close; no private signature was found.
- Scanner positive control: write that same real account-pickle serialization through `set_custom_value` into a separate unkeyed SDK crypto store. The signature scanner detected the private pickle in `matrix-sdk-crypto.sqlite3-wal`, demonstrating it can detect this generated account material when persisted without the store key. The control directory is a disposable temp directory and is removed by the test fixture.
- Crypto-store custom value write/read with a 32-byte synthetic key and marker, then binary scan of the SQLite database and its live WAL/SHM files.
- State-store sync-token write with the same synthetic marker, then scan of the state database and sidecars.
- Event-cache `save_event` of a synthetic plaintext Matrix room message with the marker in `content.body`; read it back through `get_room_events`, then scan the event-cache database and sidecars.
- Media-store `add_media_content`/`get_media_content` with the marker as media bytes, then scan the media database and sidecars.
- Send-queue save/load of a synthetic room-message request with the marker in its serialized event content, then scan the state database and sidecars.
- Wrong-key failure followed by reopening with the original key and recovering the same SDK-generated Olm account pickle.
- Missing-key behavior: opening an existing encrypted store with `None` succeeds, and `set_custom_value` can then write the marker in plaintext.

During live writes, the crypto, state, event-cache, and media directories each contained their expected `.sqlite3`, `-shm`, and `-wal` files. The scanner read every listed file, recorded filenames and total bytes, and found no marker hits with the key supplied. After each store's explicit SDK `close()`, only its main database remained and the marker was still absent. The raw crypto DB began with the ordinary `SQLite format 3` header. A SQLite read-only query showed plaintext `sqlite_master` schema and the literal `synthetic-private-value` custom key, while the value payload stayed absent from the scanned files. This directly demonstrates logical value encryption rather than full-file encryption.

The SDK's explicit `close()` drained/checkpointed these fixtures and removed the sidecar names before the post-close scan. This probe does not claim secure erasure, or behavior under power loss, OS snapshots, or copies made by another process. SQLite schema/catalog bytes and custom-value key names are metadata, not secrets, by SDK design; avoid placing sensitive identifiers in caller-supplied keys.

The no-key test demonstrates that opening without the key is not a safe UI lock by itself. It does **not** demonstrate decryption of existing private values with no key; the SDK constructs no cipher in that path. It does demonstrate plaintext write capability, which is enough to require a hard app-level gate before any store use. The account-pickle positive control also demonstrates that the scanner detects this actual generated private-account serialization when it is persisted through an unkeyed store.

## Source review and coverage limits

The pinned SQLite source has separate files/configuration for state (`matrix-sdk-state.sqlite3`), crypto (`matrix-sdk-crypto.sqlite3`), event cache (`matrix-sdk-event-cache.sqlite3`), and media (`matrix-sdk-media.sqlite3`). Each SQLite store accepts an optional secret and only constructs/loads its `StoreCipher` when a secret is present. The state store serializes values through the cipher when configured; the crypto store encrypts custom values but leaves their key labels as ordinary SQLite keys. Event-cache code encrypts event values and hashes selected event identifiers. Media code encrypts media bytes and hashes selected cache keys. The send-queue code stores message content through the state store's encrypted serialization, but intentionally leaves transaction IDs in plaintext and does not hash them; source comments say the SDK expects them to be random and non-identifying. These are independent stores: selecting each persistence path requires passing a reviewed secret to that store. See the [0.19.1 SQLite source](https://github.com/matrix-org/matrix-rust-sdk/tree/matrix-sdk-sqlite-0.19.1/crates/matrix-sdk-sqlite/src), [state-store send queue implementation](https://docs.rs/crate/matrix-sdk-sqlite/0.19.1/source/src/state_store.rs), and [StoreCipher source docs](https://docs.rs/matrix-sdk-store-encryption/0.19.1/src/matrix_sdk_store_encryption/lib.rs.html).

Runtime marker checks cover a real serialized timeline event in the event cache, media bytes in the media store, send-queue event content, a state sync token, and a crypto custom value. The added crypto test covers an SDK-generated Olm account's private signing and Diffie-Hellman pickle fields, including correct-key persistence and wrong-key preservation. It does not cover Olm sessions, Megolm inbound/outbound group sessions, cross-signing private identities, or other crypto records. The known-private-pattern scan is scoped to four byte/string patterns extracted from that generated account's signing and Diffie-Hellman private pickle fields; it is not a general proof that every private serialization is absent. Other uncovered areas include dependent send-queue payloads, application settings, logging, crash reports, backups, temporary files outside the store directory, process memory, and OS snapshots. The root implementation should add separate synthetic artifact probes for any of those stores/caches it enables. In particular, do not persist an extra decrypted-history, draft, search, or media cache without its own evidence and policy.

SDK configuration wraps owned secret bytes in `Zeroizing` buffers, but that cannot erase caller-owned copies or prove that a JS password entry leaves no copies. The app should generate a random 32-byte store key where practical, keep it in a platform credential facility, and pass it only through a narrow Rust lifecycle boundary. A human-chosen passphrase requires the SDK’s passphrase KDF path; its `high_entropy_passphrase`/`key` paths explicitly assume random high-entropy input and should not be used for human passwords. Do not log secrets or include them in error text.

## Platform credential facilities

**Windows:** A synthetic roundtrip through `CredWriteW` → `CredReadW` → `CredDeleteW` passed when run with the required host access. The credential target contained a unique PID/timestamp probe suffix, persistence was scoped to the current logon session, and a read-after-delete check confirmed cleanup. The initial sandboxed attempt failed with Win32 error 5 (Access denied); the unsandboxed synthetic-only retry passed; administrator elevation was not established. No real credential or lasting test credential was used. Microsoft documents that Credential Manager generic credentials are associated with the current token/logon session, and that generic credentials are readable/writable by user processes. Thus Credential Manager is feasible for app-scoped key storage, but retrieval is not user reauthentication; userspace code running as the same account can read such credentials. [CredWriteW](https://learn.microsoft.com/en-us/windows/win32/api/wincred/nf-wincred-credwritew), [credential kinds](https://learn.microsoft.com/en-us/windows/win32/secauthn/kinds-of-credentials). DPAPI protects data for the current user by default, with documented same-user/same-machine qualifications and a machine-scope option; it is an alternative wrapping mechanism, not a prompt by itself. [CryptProtectData](https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptprotectdata).

**macOS:** Keychain Services is a supported encrypted store for small secrets and offers item access controls and app access groups. The required app identity, code-signing entitlements, and access-control choices need a packaged-app probe; no macOS host was available here. [Keychain Services](https://developer.apple.com/documentation/Security/keychain-services), [macOS keychain models](https://developer.apple.com/documentation/Technotes/tn3137-on-mac-keychains).

**Linux:** Secret Service exposes secrets through D-Bus collections and items that may be locked; clients must handle explicit unlock prompts, service absence, and collections relocking at any time. This supports a fail-closed adapter design, but does not imply a service is installed or unlocked in every desktop session. No desktop Secret Service runtime was tested. WSL was available for disposable server work, which does not verify a desktop vault. [Secret Service API](https://specifications.freedesktop.org/secret-service/latest/), [locking and unlocking](https://specifications.freedesktop.org/secret-service/latest/unlocking.html).

For each platform, define whether auto-retrieval is allowed and keep it distinct from account reauthentication and Matrix recovery/secret storage. A vault-unavailable, locked, or denied result must leave the local session locked and must never create an unprotected replacement store key.

## Commands and limits

The probe was built and run with the exact pinned lockfile after dependency resolution:

```text
cargo test --offline --locked --all-targets
test result: ok. 7 passed; 0 failed
```

The storage suite was rerun against the exact lockfile after adding the real Olm-account probe. Its account-specific tests passed individually, the full suite passed 7/7, and `cargo clippy --offline --locked --all-targets -- -D warnings` passed. Windows incremental compilation emitted a hard-link warning and copied files instead; it did not fail the commands.

`cargo info` confirmed Matrix package metadata for 0.19.1 (Apache-2.0, Rust 1.96). `cargo test` compiled the exact SQLite and crypto crates. The Windows vault check used only synthetic test bytes and verified cleanup. The source is now [`vault_roundtrip/windows.rs`](../../tests/storage/src/bin/vault_roundtrip/windows.rs); the historical binary and command were `probes/p0/storage/src/bin/vault_probe.rs` and `cargo run --locked --manifest-path probes/p0/storage/Cargo.toml --bin vault_probe`. No live homeserver, Matrix account, Linux Secret Service, macOS Keychain, restore/corruption recovery, backups, or production application integration was tested. The isolated probe is feasibility evidence; it does not approve storage policy or production use.

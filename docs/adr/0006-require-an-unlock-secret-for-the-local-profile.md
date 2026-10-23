# Require an unlock secret for the local profile

* Status: accepted
* Deciders: project owner
* Date: 2026-10-23

Technical Story: the owner's release-build test against a real homeserver on
23 October 2026 found that Lock gives little protection, and the owner made a
real unlock secret the next priority.

## Context and Problem Statement

The local profile's store key is 32 random bytes. Windows DPAPI wraps it for
the current user (`crates/core/src/vault.rs`), and the profile opens without a
prompt. Any process that runs as the same user can call DPAPI and unwrap the
key. Lock and Unlock therefore protect only a profile folder copied to another
account or computer. They do not protect a profile on a logged-in desktop,
where an attacker with the user's session (malware, or a person at an unlocked
computer) can start Persephone or read the files and unwrap the key.

Which secret must the user give before the core opens the stores, and how do
first start, Unlock, wrong entries, existing profiles, a forgotten secret and
the other platforms work?

## Decision Drivers

* Opening the profile needs something that a process running as the user
  cannot get from the OS alone.
* The secret is entered only in the core's own native window, never in the web
  view (ADR-0004).
* Reviewed crates and platform APIs only, with explicit features and the
  committed lockfile. No custom cryptography (ADR-0002 and the root guidance).
* Fail closed: a wrong secret, a cancelled prompt, a missing key or an
  unsupported platform leaves the profile locked and unchanged.
* Keep the SDK lifecycle rule: once a client ran, a lock ends in
  `restart_required` (`docs/architecture/sdk-lifecycle.md`).
* The same design should be possible on Linux and macOS later.
* A forgotten secret must not make the account unusable. Encrypted history
  comes back from key backup after a new sign-in.

## Considered Options

* **A. Passphrase.** A key-encryption key (KEK) derived from a passphrase by
  Argon2id wraps the store key with an AEAD. On Windows, DPAPI stays as the
  outer layer.
* **B. Windows Hello or an NCrypt key with a user-presence UI policy.**
* **C. Both:** A first, B later as an optional extra factor.
* **D. The SDK's own passphrase mode** (`SqliteStoreConfig::passphrase`),
  without a separate store key.

## Decision Outcome

Chosen option: "A. Passphrase", with DPAPI kept as the outer layer on
Windows (owner decisions, 23 October 2026), because it is the only option that works on every Windows computer
and has a path to Linux and macOS, it needs one small reviewed dependency, and
its offline-guessing cost is set by a memory-hard KDF that we choose. B can be
added later as a separate decision if hardware-backed rate limiting is needed.

The owner also accepted the `argon2 =0.6.0` dependency, asked for a "Reset
profile" action in the same change, and kept Linux and macOS unsupported.

### Key file and cryptography

* The store key stays 32 random bytes from the OS generator. The SQLite stores
  are not re-encrypted.
* `store-key.v2` holds a magic line and a DPAPI blob. Inside the DPAPI blob
  are the format version, the Argon2id parameters, a 16-byte random salt, a
  24-byte random nonce, and the store key sealed with XChaCha20-Poly1305. The
  header is the AEAD's associated data, so a changed parameter fails the
  check.
* KEK = Argon2id(passphrase as UTF-8, salt), 32 bytes, with RFC 9106's second
  recommended parameter set: m = 64 MiB, t = 3, p = 4. The parameters are
  stored, so a later release can raise them when the file is next written.
* Crates: `argon2 =0.6.0` (RustCrypto, published 27 August 2026) with
  `default-features = false` and `features = ["alloc", "zeroize"]`, which is
  new; and `chacha20poly1305 =0.11.0` with `alloc` and `zeroize`, which is
  already compiled for the SDK's store cipher.
* After writing, the core reads the file back and unwraps it with the same
  passphrase before the stores are opened, as it does for v1 today.

### When the prompt appears

* **Start, existing profile:** before the window opens, the core asks for the
  passphrase in its native credential dialog. Only the password field is
  used.
* **Start, new profile:** the core asks for a new passphrase twice. The two
  entries must match. The minimum is 10 characters. There are no composition
  rules.
* **Unlock:** the `unlock_session` command returns `unlocking` at once. A
  worker thread shows the prompt, derives the KEK and opens the stores, then
  announces the new state. The serving loop is never blocked by the dialog.
  While the state is `unlocking`, Lock, Unlock and Reset return `unlocking`
  and change nothing. A core that exits during an unlock exits with code 1.
  The rule that a profile whose client ran ends in `restart_required` is not
  changed. In practice, after sign-in, Unlock means "restart Persephone, then
  enter the passphrase".

### Wrong secret, limits and backoff

* A wrong passphrase gives an AEAD failure. Nothing is written and the stores
  are not opened. A changed sealed key fails in the same way, so it also ends
  in `secret_rejected`. A malformed one, or one with parameters outside the
  core's bounds (at most 1 GiB, 16 passes and 16 lanes), is `key_unreadable`
  before any memory is allocated for it.
* In one unlock attempt, the core allows 5 entries. Before each new prompt it
  waits 1, 2, 4 and 8 seconds. After the fifth wrong entry the state is the new
  `secret_rejected`, and the user must press Unlock again.
* There is no persistent counter and no automatic wipe. An attacker with the
  user's session can copy the file and guess offline, so a counter would not
  stop them, and an automatic wipe would let anyone destroy the profile. The
  real limit on guessing is the Argon2id cost per guess and the passphrase
  strength.
* A cancelled prompt leaves the state `locked`, and Unlock can try again.

### Migration of DPAPI-only profiles

* When only `store-key.v1` exists, the core unwraps it with DPAPI as today,
  then asks for a new passphrase twice. It writes `store-key.v2` through a
  temporary file, reads it back, opens the stores with the key, and only then
  deletes `store-key.v1`.
* A cancelled or failed migration leaves v1 in place and the profile `locked`.
  The profile does not open without a passphrase.
* If both files exist (an interrupted migration), v2 is used and v1 is
  deleted after a successful open.
* Limitation: the store key does not change. Copies of the v1 file in backups,
  shadow copies or free disk sectors still unwrap with DPAPI alone. A user who
  needs a clean break should sign out and sign in again, which makes a new key.

### Forgotten passphrase

* There is no recovery of the passphrase. The user deletes the profile and
  signs in again. Encrypted history comes back from key backup (ADR-0005),
  with the recovery key.
* The old session cannot be revoked from this device, because its access
  token is in the encrypted store. The user must remove the old device from
  another client.
* The page shows "Reset profile" when the profile is closed and no Matrix
  client ran in this process: `locked`, `secret_rejected`, `key_missing`,
  `key_unreadable`, `key_rejected` and `store_unavailable`. The new
  `reset_profile` command only asks the core to show its own Yes/No warning,
  with No as the default. The page cannot reset a profile without that
  native confirmation.
* On Yes, the core takes the profile lock and writes the existing
  `signed-out` marker as "not revoked". It then deletes the stores and every
  key file, and removes the marker last, so an interrupted reset is finished
  by the next start. It then asks for a new passphrase and creates an empty
  profile. A cancelled prompt leaves an empty, `locked` profile.
* When the deleted store held data, the account shows `session_not_revoked`.

### Zeroization

* The passphrase goes from the dialog's buffer (zeroed and freed, as for the
  sign-in password) into a `Zeroizing<String>`. The KEK, the unsealed store key
  and the Argon2 working memory are zeroed on drop. The core passes its own
  zeroizing buffer to Argon2, because the crate frees the memory it
  allocates itself without clearing it.
* Copies made by the OS dialog, by SQLCipher and by the SDK's store cipher are
  outside this guarantee, as they are today.

### Linux and macOS

* No change: they have no vault and no native prompt, so they stay
  `unsupported` and fail closed. The v2 format allows a passphrase-only layer
  without DPAPI later. That needs a native prompt and its own review.

### Positive Consequences

* A process running as the user can no longer open the profile without the
  passphrase. It must guess, at Argon2id cost per guess.
* A copied profile needs both the user's DPAPI keys and the passphrase.
* Existing profiles keep their keys and their history.

### Negative Consequences

* While the profile is open, the key is in the core's memory. Malware running
  as the user can read it there, or record the passphrase as it is typed. The
  passphrase protects a closed or locked profile only.
* A fake prompt could ask the user for the passphrase. The native dialog names
  Persephone, but the user cannot verify which process showed it.
* A forgotten passphrase loses the local profile. Recovery needs key backup
  and the recovery key.
* Each derivation uses 64 MiB of memory. It took about 85 ms in a release
  build and about 1.3 s in a debug build on the development computer
  (`docs/evidence/unlock-secret.md`). The four lanes run one after another,
  because the crate's `parallel` feature is not enabled; an attacker can run
  them in parallel. Stronger parameters need a separate decision, and the
  stored parameters allow them without a format change.
* `argon2` is a new dependency. To our knowledge it has no independent audit.

## Pros and Cons of the Options

### A. Passphrase with Argon2id, DPAPI as the outer layer

* Good, because it works on every Windows computer, including VMs and remote
  desktops without Windows Hello.
* Good, because the cost per guess is memory-hard and stored with the file.
* Good, because the same design can run on Linux and macOS without DPAPI.
* Good, because DPAPI still binds the file to the Windows user.
* Bad, because protection depends on the passphrase's strength.
* Bad, because there is no hardware rate limit on offline guessing.
* Bad, because it adds the `argon2` crate.

### B. Windows Hello or NCrypt with a user-presence policy

A TPM-backed key in the Microsoft Passport key storage provider (Windows
Hello), or an NCrypt key with a UI protection policy, is needed for each
unwrap.

* Good, because a Hello PIN is rate-limited by the TPM's anti-hammering, and
  the key cannot leave the TPM.
* Good, because biometric unlock is convenient.
* Bad, because Hello is not set up on many computers, VMs and remote sessions,
  so a fallback (A) is still needed.
* Bad, because Hello (`KeyCredentialManager`) only signs. Getting a wrapping
  key needs a signature-derived construction, which is close to custom
  cryptography. Using Hello only as a gate in front of DPAPI adds nothing,
  because DPAPI can still be called without it.
* Bad, because a key is not bound to Persephone: another process running as
  the user can ask for the same key and show the same OS prompt.
* Bad, because it needs WinRT bindings (a large new dependency) and has no
  equivalent design for Linux. macOS would need the Keychain with an access
  control policy instead.
* Bad, because a TPM reset or a new computer loses the profile.

### C. Both

* Good, because users with Hello get hardware rate limiting, and others still
  have a passphrase.
* Bad, because it has all of B's costs and two unlock paths to test and
  explain. It can be decided later on top of A.

### D. SDK passphrase mode

* Good, because it adds no dependency, and the SDK owns the cryptography.
* Bad, because the SDK uses PBKDF2-SHA256 with 200,000 rounds, which is not
  memory-hard and is cheap to guess on GPUs.
* Bad, because there is no separate store key. Each of the four stores holds
  its own passphrase-wrapped cipher, so migration and a change of passphrase
  rewrite every store.
* Bad, because a wrong passphrase is detected only when SQLite is opened.

## Links

* Refines [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md):
  the secret is entered in the core's native window.
* Relates to [ADR-0002](0002-use-sdk-owned-matrix-cryptography-and-explicit-features.md)
  and [ADR-0005](0005-share-room-keys-only-with-cross-signed-devices.md)
  (key backup restores history after a reset).
* [`docs/architecture/sdk-lifecycle.md`](../architecture/sdk-lifecycle.md)
  (the `restart_required` rule is unchanged).
* [RFC 9106](https://www.rfc-editor.org/rfc/rfc9106) (Argon2 parameters).

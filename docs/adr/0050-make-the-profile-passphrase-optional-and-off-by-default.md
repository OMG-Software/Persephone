# Make the profile passphrase optional and off by default

* Status: accepted
* Deciders: project owner
* Date: 2026-11-17

Technical Story: on 17 November 2026 the owner asked to make the encrypted
profile's passphrase an optional feature that is off by default, which users
can turn on when they want.

## Context and Problem Statement

ADR-0006 requires a passphrase before the core opens the local profile. The
SQLite stores are encrypted with a 32-byte random store key. Argon2id and
XChaCha20-Poly1305 seal that key under the passphrase, and the OS vault
(DPAPI, the macOS Keychain or the Linux Secret Service) wraps the sealed key
in `store-key.v2`. Every new profile must therefore choose a passphrase
before first use, and every start asks for it.

The owner wants that protection to be a choice. What protects the profile
when the passphrase is off, how does a user turn it on or off, and what
happens to existing profiles and to Lock?

## Decision Drivers

* Keep the stores encrypted. Never select plaintext storage (root guidance).
* Keep the secret entry in the core's own window (ADR-0004, ADR-0006).
* Do not change the on-disk key formats or the SQLite stores.
* An interrupted switch must leave a profile that opens correctly, and must
  fail toward the passphrase, not away from it.
* The UI must not offer a control that gives no protection.

## Considered Options

* **A. Vault-only key by default; passphrase optional.** The store key is
  wrapped by the OS vault alone (`store-key.v1`) until the user turns the
  passphrase on.
* **B. Plaintext stores by default; encryption optional.**
* **C. Keep ADR-0006 unchanged.**

## Decision Outcome

Chosen option: "A. Vault-only key by default", because it keeps every store
encrypted at rest and bound to the user's OS account, uses formats the core
already reads and writes, and lets a user add the passphrase later without
re-encrypting the stores (owner decisions, 17 November 2026).

The owner also decided that existing profiles keep their passphrase and can
turn it off, and that Lock and the idle lock are shown only while the
passphrase is on.

### Key files

* `store-key.v1` (magic plus the vault's blob of the bare store key) is the
  key file of a profile without a passphrase. It is no longer migrated.
* `store-key.v2` (ADR-0006, unchanged) is the key file of a profile with a
  passphrase.
* When both files exist, `store-key.v2` wins, and the core deletes
  `store-key.v1` after the stores open. An interrupted switch in either
  direction therefore ends with the passphrase on.

### Opening a profile

* **New profile:** the core makes a new store key, writes `store-key.v1`
  through a temporary file, reads it back through the vault, and only then
  creates the stores. There is no prompt.
* **`store-key.v1` only:** the vault unwraps the key and the stores open.
  There is no prompt.
* **`store-key.v2`:** as ADR-0006, including attempts, backoff and
  `secret_rejected`.
* A store without a key is still `key_missing`. Every other failure keeps its
  ADR-0006 state.
* After a reset (ADR-0006), the new empty profile has no passphrase.

### Turning the passphrase on or off

* Settings, Security shows whether the profile has a passphrase. The new
  `set_profile_passphrase` command carries only `on` (a boolean). The
  passphrase never crosses the IPC channel.
* The command needs an open profile. It replies at once, and the result
  arrives as a security status change. The prompts run on a worker thread
  without a lease, so an open prompt never blocks the serving loop or a lock.
* **On:** the core unwraps `store-key.v1`, asks for a new passphrase twice
  (at least 10 characters, as ADR-0006), writes `store-key.v2` through a
  temporary file, reads it back with the same passphrase, renames it into
  place, and then deletes `store-key.v1`.
* **Off:** the core asks for the current passphrase, with the ADR-0006
  attempt limit and backoff. It writes `store-key.v1` through a temporary
  file, reads it back through the vault, renames it into place, and then
  deletes `store-key.v2`. Five wrong entries end the operation with
  `passphrase_rejected`, and nothing is written.
* The core writes the key files only while it holds the session's
  transition lock and the profile is still open. If the profile was locked
  while a prompt was open, the result is dropped and nothing is written.
* The store key does not change. The SQLite stores are not rewritten.

### Lock and idle lock

* The page shows Lock and the idle-lock setting (ADR-0025) only while the
  passphrase is on. Unlock stays available for a closed profile in every
  case, so a failed open can be retried.
* The core does not run the idle lock while the passphrase is off, even when
  an idle time was saved earlier.

### Positive Consequences

* A new user can use Persephone without choosing a passphrase.
* The stores are still encrypted, and a profile folder copied to another
  account or computer is still unreadable without the user's vault.
* A user can add a passphrase later without signing in again or losing
  history.

### Negative Consequences

* Without a passphrase, any process that runs as the user can open the
  profile, as before ADR-0006. So can a person at the user's unlocked
  computer. The page says so next to the setting.
* Turning the passphrase on does not change the store key. Copies of
  `store-key.v1` in backups, shadow copies or free disk sectors still unwrap
  with the vault alone. A user who needs a clean break should sign out and
  sign in again with the passphrase on, which makes a new key (as ADR-0006
  says for migrated profiles).
* Turning the passphrase off is the reverse case: copies of `store-key.v2`
  still open with the old passphrase.
* An interrupted "off" leaves the passphrase on. The user must turn it off
  again.
* Turning the passphrase on reads `store-key.v1` from disk instead of using
  the key that opened the stores. A process running as the user could
  replace that file first, and the profile would then fail with
  `key_rejected` at the next start. Such a process can already read the
  profile, so this adds no access, but it can deny service.

## Pros and Cons of the Options

### A. Vault-only key by default

* Good, because the stores stay encrypted and bound to the OS account.
* Good, because it uses the existing key formats and code paths.
* Good, because the passphrase can be added or removed without touching the
  stores.
* Bad, because the default no longer protects against processes that run as
  the user.

### B. Plaintext stores by default

* Good, because it is the simplest to open.
* Bad, because it breaks the rule against plaintext storage, and leaves the
  account's encryption keys and access token readable on disk.
* Bad, because turning encryption on later would rewrite every store.

### C. Keep ADR-0006 unchanged

* Good, because every profile has the strongest local protection.
* Bad, because it does not meet the owner's requirement.

## Links

* Refines [ADR-0006](0006-require-an-unlock-secret-for-the-local-profile.md):
  the passphrase is optional, and `store-key.v1` is a supported key file
  instead of a format to migrate. The rest of ADR-0006 still applies when the
  passphrase is on.
* Refines [ADR-0025](0025-lock-after-system-idle-time-measured-in-the-core.md):
  the idle lock runs only while the passphrase is on.
* Relates to [ADR-0038](0038-unlock-the-profile-with-a-passkey-through-prf.md):
  a passkey is a way to unlock a profile whose protection is on.
* Relates to [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md):
  the secret is entered in the core's native window.

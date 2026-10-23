# Unlock secret for the local profile

Checked 23 October 2026 on Windows 11 Pro x64 (build 26300), debug and
release test builds, rustc 1.99.1. This records the implementation of
[ADR-0006](../adr/0006-require-an-unlock-secret-for-the-local-profile.md),
which the owner accepted on 23 October 2026, after their release-build test
that day found that Unlock asked for nothing.

**Result: the profile no longer opens with DPAPI alone.** Each open asks for
the profile passphrase in the core's own window. A wrong, empty, cancelled or
unavailable entry leaves the profile closed and unchanged. A DPAPI-only
profile is upgraded on its first open and keeps its data. A forgotten
passphrase can be handled only by a reset that the core confirms in its own
window. These results come from automated tests with a scripted prompt and a
stand-in vault, plus real DPAPI on Windows. **No live homeserver run and no
manual run of the native dialogs were made for this change.**

## Design as built

* **Sealing** ([`passphrase.rs`](../../crates/core/src/passphrase.rs)):
  Argon2id (64 MiB, 3 passes, 4 lanes; `argon2 0.6.0`) derives a 32-byte key
  from the passphrase and a 16-byte random salt. XChaCha20-Poly1305 seals the
  store key with a 24-byte random nonce. The format byte, the parameters, the
  salt and the nonce are the associated data. Stored parameters above 1 GiB,
  16 passes or 16 lanes are refused before any memory is allocated. The
  Argon2 working memory is the core's own zeroizing buffer.
* **Key file** ([`store.rs`](../../crates/core/src/store.rs)):
  `store-key.v2` holds a magic line and the vault's blob of the sealed key.
  On Windows the vault is DPAPI, as before
  ([`vault.rs`](../../crates/core/src/vault.rs)). A new file is written
  through a temporary file and read back with the same passphrase before the
  stores are used. A file that does not give back the key is removed.
* **States:**

  | Profile or entry | Result |
  | --- | --- |
  | New profile | New passphrase twice, at least 10 characters; reasons are shown for a short or mismatched entry; at most 5 rounds, then `locked` |
  | `store-key.v2` | Passphrase asked; 5 entries with waits of 1, 2, 4 and 8 s before the second to fifth; then `secret_rejected` |
  | Only `store-key.v1` | Stores opened with the DPAPI key first; new passphrase twice; v2 saved and read back; v1 deleted. If v1 cannot be deleted, the stores close again (`store_unavailable`) |
  | Both files (interrupted upgrade) | v2 used; v1 deleted |
  | Prompt cancelled or failed | `locked`; nothing written |
  | Changed sealed key | Same as a wrong passphrase (`secret_rejected`) |
  | Malformed sealed key or parameters out of bounds | `key_unreadable` |
  | No prompt (Linux, macOS) | `unsupported` |

* **Unlock and reset** ([`session.rs`](../../crates/core/src/session.rs)):
  `unlock_session` and the new `reset_profile` reply `unlocking` at once. A
  worker thread runs the prompt and the open without the transition lock.
  While the state is `unlocking`, lock, unlock and reset return `unlocking`
  and change nothing. The result is announced as a session change. The
  `restart_required` rule is unchanged.
* **Reset:** only from `locked`, `secret_rejected`, `key_missing`,
  `key_unreadable`, `key_rejected` or `store_unavailable`, and only when no
  Matrix client ran in the process. The core shows a Yes/No warning with No
  as the default. On Yes it writes the `signed-out` marker as "not revoked",
  deletes the stores and both key files, removes the marker, asks for a new
  passphrase and opens an empty profile. If the store held data, the account
  shows `session_not_revoked`.
* **Start:** `persephone --profile <dir>` asks before the window opens.
* **Page:** the store line names `unlocking` and `secret_rejected`. "Reset
  profile…" is shown for the resettable states. The Lock button has help
  text (`frontend/src/help.ts`), shown on hover and on keyboard focus,
  hidden by Escape, and linked with `aria-describedby`.
* **Development builds:** with `PERSEPHONE_DEV_PROFILE`,
  `PERSEPHONE_DEV_PASSPHRASE_FILE` (a path inside `.local`) answers the
  passphrase prompts. Without it, the native window is used.

## KDF cost

A temporary ignored test (not committed) sealed and opened a key with the
default parameters three times on the development computer:

| Build | Seal | Open |
| --- | --- | --- |
| Release (`cargo test --release`) | 83–93 ms | 80–93 ms |
| Debug | 1256–1330 ms | 1273–1287 ms |

The lanes run one after another, because the `parallel` feature is off. An
attacker can run them in parallel, so an offline guess costs less than this.

## Tests

`python tools/dev.py check --offline` passed (exit 0) on the final tree: 297
Rust tests (233 in the workspace run and 64 in the `dev-disposable-servers`
core run; 2 ignored), 21 frontend tests, 2 server-registration and 13
development-controller tests, with formatting, Clippy (default and development
features), contract drift and TypeScript checks. Raw output:
`.local/runs/unlock-secret/check.txt` (ignored). An earlier run of the same
command failed (exit 101) on two unused imports in `tests/account.rs` that only
the development-feature Clippy step sees; they were removed before the
passing run.

New and changed tests (GIVEN/WHEN/THEN):

* `passphrase.rs` (unit): only the passphrase opens a sealed key; every
  changed byte is refused; parameter bounds; backoff waits.
* [`tests/store.rs`](../../crates/core/tests/store.rs): upgrade of a v1
  profile with its data; cancelled and interrupted upgrades; new-passphrase
  rules (short, empty, mismatch, cancelled, unsupported); five wrong entries
  leave every file unchanged; reset deletes everything, or nothing when the
  profile is in use. Existing tests now use v2 files. A changed sealed key now
  ends in `secret_rejected` instead of `key_rejected`.
* [`tests/session.rs`](../../crates/core/tests/session.rs): wrong entries at
  Unlock and a later right one; cancelled and failed prompts; an open prompt
  does not block lock, unlock or reset; declined and failed reset warnings
  keep the data; a confirmed reset makes a new empty profile that only the
  new passphrase opens; no reset of an open or missing profile.
* `tests/serve.rs`, `tests/account.rs`: Unlock replies `unlocking`, then the
  session change follows.
* `crates/core-protocol`: unknown fields on `unlock_session` and
  `reset_profile` are refused.
* `src-tauri/tests/ipc.rs`: `reset_profile` is allowed and relayed.
* `frontend/tests/render.test.mjs`: the Lock button is described by help
  text that says what Lock closes, that it does not sign out, and what
  unlocking needs.

## Limitations

* While the profile is open, the key is in the core's memory. Malware running
  as the user can read it there, or record the passphrase.
* The user cannot verify which process showed a passphrase prompt.
* The store key does not change on upgrade. Old copies of `store-key.v1` in
  backups or free disk space still open the profile with DPAPI alone.
* The passphrase is not Unicode-normalized. The same text typed in another
  normalization form does not match.
* The native dialogs, the tooltip's appearance and keyboard behaviour in the
  web view, and the exit during `unlocking` were not checked by hand.

//! The protected profile against real SDK SQLite stores. The store key is
//! wrapped by the vault, and sealed under the profile passphrase only when
//! the user turned the passphrase on (ADR-0050); on Windows the wrapped key
//! is wrapped by real DPAPI. The fail-closed rules are also checked with a
//! stand-in protector so they run on every platform.

use matrix_sdk_crypto::store::CryptoStore;
use persephone_contracts::StoreState;
use persephone_core::{
    credentials::PromptError,
    passphrase::{self, SEALED_BYTES},
    store::{self, MAX_KEY_FILE_BYTES, SEALED_KEY_FILE, STORE_DIR, VAULT_KEY_FILE},
    vault::{KeyProtector, Unsupported},
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

mod common;
use common::{PASSPHRASE, Script, Tagged, unlock, unlock_with};

const MARKER: &[u8] = b"PERSEPHONE_SYNTHETIC_CORE_STORE_MARKER_7C1D";
const MARKER_KEY: &str = "synthetic-marker";

fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().into(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    out
}

/// Drops SQLite's shared-memory index and empty write-ahead logs. Whether
/// they remain after a close depends on when the last connection ends, and
/// they hold no data; a write-ahead log with content is kept.
fn durable(mut files: BTreeMap<PathBuf, Vec<u8>>) -> BTreeMap<PathBuf, Vec<u8>> {
    files.retain(|path, bytes| {
        let name = path.to_string_lossy();
        !(name.ends_with("-shm") || (name.ends_with("-wal") && bytes.is_empty()))
    });
    files
}

/// Creates a profile without a passphrase, stores the marker through the
/// encrypted crypto store and closes it.
fn seeded(protector: &dyn KeyProtector) -> TempDir {
    let dir = TempDir::new().unwrap();
    let stores = store::open(dir.path(), protector, &unlock()).expect("new profile opens");
    stores
        .block_on(
            stores
                .crypto()
                .set_custom_value(MARKER_KEY, MARKER.to_vec()),
        )
        .unwrap();
    assert!(stores.close(), "stores close cleanly");
    dir
}

/// Like [`seeded`], then turns the passphrase on with [`PASSPHRASE`], so the
/// profile holds `store-key.v2` only.
fn seeded_with_passphrase(protector: &dyn KeyProtector) -> TempDir {
    let dir = seeded(protector);
    let stores = store::open(dir.path(), protector, &unlock()).expect("profile reopens");
    store::turn_on_passphrase(dir.path(), protector, &unlock()).expect("passphrase turns on");
    assert!(stores.close());
    assert!(dir.path().join(SEALED_KEY_FILE).is_file());
    assert!(!dir.path().join(VAULT_KEY_FILE).exists());
    dir
}

fn read_marker(dir: &Path, protector: &dyn KeyProtector) -> Option<Vec<u8>> {
    let stores = store::open(dir, protector, &unlock()).expect("profile reopens");
    let value = stores
        .block_on(stores.crypto().get_custom_value(MARKER_KEY))
        .unwrap();
    assert!(stores.close());
    value
}

fn refused(dir: &Path, protector: &dyn KeyProtector) -> StoreState {
    match store::open(dir, protector, &unlock()) {
        Ok(stores) => {
            stores.close();
            panic!("profile opened");
        }
        Err(state) => state,
    }
}

/// GIVEN a new profile WHEN it opens THEN no prompt is shown, only the
/// vault-wrapped `store-key.v1` is written, the stores are encrypted with it,
/// and they reopen with it.
#[test]
fn given_a_new_profile_then_it_opens_without_a_prompt_and_writes_only_the_vault_key() {
    let protector = Tagged(1);
    let dir = TempDir::new().unwrap();
    let script = Script::new([]);
    let stores = store::open(dir.path(), &protector, &unlock_with(script.clone())).unwrap();
    assert!(!stores.passphrase());
    stores
        .block_on(
            stores
                .crypto()
                .set_custom_value(MARKER_KEY, MARKER.to_vec()),
        )
        .unwrap();
    assert!(stores.close());
    assert!(script.seen().is_empty(), "no prompt: {:?}", script.seen());
    assert!(dir.path().join(VAULT_KEY_FILE).is_file());
    assert!(!dir.path().join(SEALED_KEY_FILE).exists());
    assert!(!dir.path().join(format!("{VAULT_KEY_FILE}.tmp")).exists());
    assert!(
        dir.path()
            .join(STORE_DIR)
            .join("matrix-sdk-crypto.sqlite3")
            .is_file()
    );
    let bytes = files(dir.path());
    assert!(
        !bytes.iter().any(|(path, bytes)| path.starts_with(STORE_DIR)
            && bytes.windows(MARKER.len()).any(|w| w == MARKER)),
        "the stores hold no plaintext marker"
    );
    let script = Script::new([]);
    let stores = store::open(dir.path(), &protector, &unlock_with(script.clone())).unwrap();
    assert!(!stores.passphrase());
    assert!(stores.close());
    assert!(script.seen().is_empty());
    assert_eq!(read_marker(dir.path(), &protector).as_deref(), Some(MARKER));
}

/// The stores exist but the key is gone: the profile stays locked, no key is
/// made, and not one byte of the profile changes.
#[test]
fn given_a_store_without_its_key_then_the_profile_stays_locked_and_unchanged() {
    let protector = Tagged(1);
    let dir = seeded(&protector);
    fs::remove_file(dir.path().join(VAULT_KEY_FILE)).unwrap();
    let before = files(dir.path());
    assert_eq!(refused(dir.path(), &protector), StoreState::KeyMissing);
    assert!(
        !dir.path().join(VAULT_KEY_FILE).exists(),
        "no replacement key"
    );
    assert!(!dir.path().join(SEALED_KEY_FILE).exists());
    assert_eq!(files(dir.path()), before);
}

#[test]
fn given_an_unreadable_sealed_key_file_then_the_profile_stays_locked_and_recovers_when_restored() {
    let protector = Tagged(1);
    let dir = seeded_with_passphrase(&protector);
    let key_path = dir.path().join(SEALED_KEY_FILE);
    let original = fs::read(&key_path).unwrap();
    let mut flipped = original.clone();
    *flipped.last_mut().unwrap() ^= 1;
    let mut no_magic = original.clone();
    no_magic[0] ^= 1;
    for bad in [
        // Wrapped for another context.
        {
            let mut other = original.clone();
            let tag = other.len() - SEALED_BYTES - 1;
            other[tag] = 2;
            other
        },
        no_magic,
        original[..original.len() - 1].to_vec(),
        vec![b'x'; MAX_KEY_FILE_BYTES as usize + 1],
        Vec::new(),
    ] {
        fs::write(&key_path, &bad).unwrap();
        let before = files(dir.path());
        assert_eq!(refused(dir.path(), &protector), StoreState::KeyUnreadable);
        assert_eq!(files(dir.path()), before);
    }
    // A changed sealed key unwraps, but no passphrase opens it: the AEAD
    // cannot tell a tampered file from a wrong passphrase.
    fs::write(&key_path, &flipped).unwrap();
    assert_eq!(refused(dir.path(), &protector), StoreState::SecretRejected);
    fs::write(&key_path, &original).unwrap();
    assert_eq!(read_marker(dir.path(), &protector).as_deref(), Some(MARKER));
}

/// GIVEN a profile without a passphrase WHEN its key file is damaged or
/// wrapped for another context THEN it stays locked and unchanged, and opens
/// again once the file is restored.
#[test]
fn given_an_unreadable_vault_key_file_then_the_profile_stays_locked_and_recovers_when_restored() {
    let protector = Tagged(1);
    let dir = seeded(&protector);
    let key_path = dir.path().join(VAULT_KEY_FILE);
    let original = fs::read(&key_path).unwrap();
    let mut no_magic = original.clone();
    no_magic[0] ^= 1;
    for bad in [
        // Wrapped for another context: the tag byte follows the magic.
        {
            let mut other = original.clone();
            let tag = original.iter().position(|b| *b == b'\n').unwrap() + 1;
            other[tag] = 2;
            other
        },
        no_magic,
        // A blob of the wrong length is not a store key.
        original[..original.len() - 1].to_vec(),
        vec![b'x'; MAX_KEY_FILE_BYTES as usize + 1],
        Vec::new(),
    ] {
        fs::write(&key_path, &bad).unwrap();
        let before = files(dir.path());
        assert_eq!(refused(dir.path(), &protector), StoreState::KeyUnreadable);
        assert_eq!(files(dir.path()), before);
    }
    fs::write(&key_path, &original).unwrap();
    assert_eq!(read_marker(dir.path(), &protector).as_deref(), Some(MARKER));
}

#[test]
fn given_another_profiles_key_then_the_stores_refuse_it_and_keep_their_data() {
    let protector = Tagged(1);
    let first = seeded(&protector);
    let second = seeded(&protector);
    let key_path = first.path().join(VAULT_KEY_FILE);
    let original = fs::read(&key_path).unwrap();
    fs::copy(second.path().join(VAULT_KEY_FILE), &key_path).unwrap();
    let before = durable(files(first.path()));
    assert_eq!(refused(first.path(), &protector), StoreState::KeyRejected);
    assert_eq!(
        durable(files(first.path())),
        before,
        "a refused key changes nothing"
    );
    fs::write(&key_path, original).unwrap();
    assert_eq!(
        read_marker(first.path(), &protector).as_deref(),
        Some(MARKER)
    );
}

#[test]
fn given_an_open_profile_then_a_second_owner_is_refused_until_it_closes() {
    let protector = Tagged(1);
    let dir = TempDir::new().unwrap();
    let stores = store::open(dir.path(), &protector, &unlock()).unwrap();
    assert_eq!(refused(dir.path(), &protector), StoreState::ProfileInUse);
    assert!(stores.close());
    assert!(
        store::open(dir.path(), &protector, &unlock())
            .unwrap()
            .close()
    );
}

#[test]
fn given_no_vault_then_a_new_profile_stays_locked_and_creates_no_key_or_store() {
    let dir = TempDir::new().unwrap();
    assert_eq!(refused(dir.path(), &Unsupported), StoreState::Unsupported);
    assert!(!dir.path().join(VAULT_KEY_FILE).exists());
    assert!(!dir.path().join(SEALED_KEY_FILE).exists());
    assert!(!dir.path().join(STORE_DIR).exists());
}

/// A first run that stopped after saving the key, or while writing it, can be
/// completed: nothing was protected with a key that was never saved.
#[test]
fn given_an_interrupted_first_run_then_opening_completes_it() {
    let protector = Tagged(1);
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(format!("{VAULT_KEY_FILE}.tmp")), b"torn").unwrap();
    fs::create_dir(dir.path().join(STORE_DIR)).unwrap();
    assert!(
        store::open(dir.path(), &protector, &unlock())
            .unwrap()
            .close()
    );

    let key_only = seeded(&protector);
    fs::remove_dir_all(key_only.path().join(STORE_DIR)).unwrap();
    assert_eq!(read_marker(key_only.path(), &protector), None);
}

/// Dropping open stores without closing them (as while a panic unwinds) must
/// not panic: the SQLite pool's destructor needs a Tokio runtime. The profile
/// lock is released and the data kept.
#[test]
fn given_open_stores_dropped_without_close_then_they_shut_down_and_keep_their_data() {
    let protector = Tagged(1);
    let dir = TempDir::new().unwrap();
    let stores = store::open(dir.path(), &protector, &unlock()).unwrap();
    stores
        .block_on(
            stores
                .crypto()
                .set_custom_value(MARKER_KEY, MARKER.to_vec()),
        )
        .unwrap();
    drop(stores);
    assert_eq!(read_marker(dir.path(), &protector).as_deref(), Some(MARKER));
}

/// A signed-out profile is wiped the next time it opens: the key is already
/// gone, the old files are deleted, and the profile opens as a new one
/// without the old data. An interrupted wipe (marker still present) repeats.
#[test]
fn given_a_signed_out_profile_then_opening_wipes_it_and_starts_fresh() {
    let protector = Tagged(1);
    let dir = seeded(&protector);
    store::mark_signed_out(dir.path(), true).unwrap();
    assert!(
        !dir.path().join(VAULT_KEY_FILE).exists(),
        "key deleted at sign-out"
    );
    assert!(dir.path().join(store::SIGNED_OUT_FILE).exists());
    assert_eq!(read_marker(dir.path(), &protector), None, "old data gone");
    assert!(
        !dir.path().join(store::SIGNED_OUT_FILE).exists(),
        "wipe finished"
    );
    assert!(dir.path().join(VAULT_KEY_FILE).exists(), "a new key");
    let stores = store::open(dir.path(), &protector, &unlock()).unwrap();
    assert!(!stores.unconfirmed_sign_out(), "nothing wiped this time");
    assert!(stores.close());

    // A wipe that stopped after the files went but before the marker did
    // completes on the next open.
    let again = seeded(&protector);
    store::mark_signed_out(again.path(), true).unwrap();
    fs::remove_dir_all(again.path().join(STORE_DIR)).unwrap();
    assert_eq!(read_marker(again.path(), &protector), None);
    assert!(!again.path().join(store::SIGNED_OUT_FILE).exists());
}

/// GIVEN a profile whose sign-out marker cannot be written (a directory is
/// in its place) WHEN signing out THEN the failure is reported, the key is
/// still deleted, and the old stores never open again.
#[test]
fn given_the_sign_out_marker_cannot_be_written_then_the_key_still_goes_and_the_profile_stays_closed()
 {
    let protector = Tagged(1);
    let dir = seeded(&protector);
    fs::create_dir(dir.path().join(store::SIGNED_OUT_FILE)).unwrap();
    assert!(
        store::mark_signed_out(dir.path(), true).is_err(),
        "failure reported"
    );
    assert!(
        !dir.path().join(VAULT_KEY_FILE).exists(),
        "key deleted anyway"
    );
    assert_ne!(refused(dir.path(), &protector), StoreState::Protected);
    assert!(
        !dir.path().join(VAULT_KEY_FILE).exists(),
        "no new key over the old data"
    );
}

#[cfg(windows)]
mod dpapi {
    use super::*;
    use persephone_core::vault::{Dpapi, VaultError};

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn given_dpapi_then_no_profile_file_holds_the_raw_key_or_the_marker() {
        let protector = Dpapi::new();
        let dir = seeded(&protector);
        let saved = fs::read(dir.path().join(VAULT_KEY_FILE)).unwrap();
        let blob = &saved[saved.iter().position(|b| *b == b'\n').unwrap() + 1..];
        let key = protector.unprotect(blob).unwrap();
        for (path, bytes) in files(dir.path()) {
            assert!(!contains(&bytes, &key), "raw key in {path:?}");
            assert!(!contains(&bytes, MARKER), "marker in {path:?}");
        }
        assert_eq!(read_marker(dir.path(), &protector).as_deref(), Some(MARKER));

        // With the passphrase on, the vault wraps the sealed key.
        let stores = store::open(dir.path(), &protector, &unlock()).unwrap();
        store::turn_on_passphrase(dir.path(), &protector, &unlock()).unwrap();
        assert!(stores.close());
        let saved = fs::read(dir.path().join(SEALED_KEY_FILE)).unwrap();
        let blob = &saved[saved.iter().position(|b| *b == b'\n').unwrap() + 1..];
        let opened = passphrase::open(&protector.unprotect(blob).unwrap(), PASSPHRASE).unwrap();
        assert_eq!(opened.as_slice(), key.as_slice());
        for (path, bytes) in files(dir.path()) {
            assert!(!contains(&bytes, &key), "raw key in {path:?}");
        }
        assert_eq!(read_marker(dir.path(), &protector).as_deref(), Some(MARKER));
    }

    /// Other entropy stands in for a blob from another program or user: DPAPI
    /// refuses it and the profile stays locked and unchanged.
    #[test]
    fn given_a_blob_dpapi_cannot_unwrap_then_the_profile_stays_locked() {
        let dir = seeded(&Dpapi::new());
        let before = files(dir.path());
        assert_eq!(
            refused(dir.path(), &Dpapi::with_entropy(b"another program")),
            StoreState::KeyUnreadable
        );
        assert_eq!(files(dir.path()), before);
        assert_eq!(
            read_marker(dir.path(), &Dpapi::new()).as_deref(),
            Some(MARKER)
        );
    }

    #[test]
    fn given_dpapi_then_wrapping_is_randomized_and_tamper_evident() {
        let protector = Dpapi::new();
        let key = persephone_core::vault::generate_key().unwrap();
        let first = protector.protect(key.as_ref()).unwrap();
        let second = protector.protect(key.as_ref()).unwrap();
        assert_ne!(first, second, "fresh salt per wrap");
        assert!(!contains(&first, key.as_ref()));
        assert_eq!(
            protector.unprotect(&first).unwrap().as_slice(),
            key.as_ref()
        );
        let mut tampered = first.clone();
        let middle = tampered.len() / 2;
        tampered[middle] ^= 1;
        assert_eq!(protector.unprotect(&tampered), Err(VaultError::Failed));
    }
}

/// GIVEN a sign-out that stopped after recording its intent (the homeserver
/// request never finished, the key is still there) WHEN the profile opens
/// THEN it is wiped rather than restored, and the open says the revocation
/// was not confirmed; a failed revocation says the same, and a confirmed one
/// does not.
#[test]
fn given_an_interrupted_or_unrevoked_sign_out_then_opening_wipes_and_reports_it() {
    let protector = Tagged(1);
    let interrupted = seeded(&protector);
    store::begin_sign_out(interrupted.path()).unwrap();
    assert!(
        interrupted.path().join(VAULT_KEY_FILE).exists(),
        "key not yet deleted"
    );
    let stores = store::open(interrupted.path(), &protector, &unlock()).unwrap();
    assert!(stores.unconfirmed_sign_out());
    assert_eq!(
        stores
            .block_on(stores.crypto().get_custom_value(MARKER_KEY))
            .unwrap(),
        None,
        "old data gone although its key was still there"
    );
    assert!(stores.close());

    for (revoked, unconfirmed) in [(false, true), (true, false)] {
        let dir = seeded(&protector);
        store::begin_sign_out(dir.path()).unwrap();
        store::mark_signed_out(dir.path(), revoked).unwrap();
        let stores = store::open(dir.path(), &protector, &unlock()).unwrap();
        assert_eq!(
            stores.unconfirmed_sign_out(),
            unconfirmed,
            "revoked: {revoked}"
        );
        assert!(stores.close());
    }
}

/// Opens the profile, runs `switch` with its stores open (as the session
/// does), and closes it.
fn while_open<T>(dir: &Path, protector: &dyn KeyProtector, switch: impl FnOnce() -> T) -> T {
    let stores = store::open(dir, protector, &unlock()).expect("profile opens");
    let result = switch();
    assert!(stores.close());
    result
}

/// GIVEN a profile without a passphrase WHEN it is turned on THEN a new
/// passphrase is asked twice, `store-key.v2` replaces `store-key.v1`, the
/// next open asks for it, and the same data opens.
#[test]
fn given_a_profile_without_a_passphrase_then_turning_it_on_replaces_the_vault_key() {
    let protector = Tagged(1);
    let dir = seeded(&protector);
    let script = Script::new([Ok("a new passphrase"), Ok("a new passphrase")]);
    let turned = while_open(dir.path(), &protector, || {
        store::turn_on_passphrase(dir.path(), &protector, &unlock_with(script.clone()))
    });
    assert_eq!(turned, Ok(()));
    assert_eq!(
        script.seen(),
        ["NewPassphrase(TurnOn)", "NewPassphrase(Confirm)"]
    );
    assert!(dir.path().join(SEALED_KEY_FILE).is_file());
    assert!(!dir.path().join(VAULT_KEY_FILE).exists(), "vault key gone");
    assert!(!dir.path().join(format!("{SEALED_KEY_FILE}.tmp")).exists());

    let next = Script::new([Ok("a new passphrase")]);
    let stores = store::open(dir.path(), &protector, &unlock_with(next.clone())).unwrap();
    assert!(stores.passphrase());
    assert_eq!(next.seen(), ["Passphrase { retry: false }"]);
    assert_eq!(
        stores
            .block_on(stores.crypto().get_custom_value(MARKER_KEY))
            .unwrap()
            .as_deref(),
        Some(MARKER)
    );
    assert!(stores.close());
}

/// GIVEN a profile without a passphrase WHEN the new passphrase is too short,
/// empty or not confirmed THEN it is asked again with the reason and only a
/// matching passphrase of the minimum length is saved; WHEN the prompt is
/// cancelled or missing THEN `store-key.v1` stays and nothing else is written.
#[test]
fn given_turning_the_passphrase_on_then_it_must_be_long_enough_and_confirmed() {
    let protector = Tagged(1);
    let dir = seeded(&protector);
    let short = "x".repeat(passphrase::MIN_PASSPHRASE_CHARS - 1);
    let script = Script::new([
        Ok(short.as_str()),
        Err(PromptError::Invalid),
        Ok("long enough!"),
        Ok("long enough?"),
        Ok("long enough!"),
        Ok("long enough!"),
    ]);
    // Cancelled and unsupported first: nothing changes.
    for answer in [PromptError::Cancelled, PromptError::Unsupported] {
        let before = durable(files(dir.path()));
        let refused = unlock_with(Script::new([Err(answer)]));
        let state = while_open(dir.path(), &protector, || {
            store::turn_on_passphrase(dir.path(), &protector, &refused)
        });
        assert_eq!(
            state,
            Err(match answer {
                // On Linux the core's windows are refused only without a
                // display (ADR-0061).
                PromptError::Unsupported if cfg!(target_os = "linux") => StoreState::NoDisplay,
                PromptError::Unsupported => StoreState::Unsupported,
                _ => StoreState::Locked,
            })
        );
        assert!(!dir.path().join(SEALED_KEY_FILE).exists());
        assert!(!dir.path().join(format!("{SEALED_KEY_FILE}.tmp")).exists());
        assert!(dir.path().join(VAULT_KEY_FILE).exists(), "vault key kept");
        assert_eq!(durable(files(dir.path())), before);
    }
    let turned = while_open(dir.path(), &protector, || {
        store::turn_on_passphrase(dir.path(), &protector, &unlock_with(script.clone()))
    });
    assert_eq!(turned, Ok(()));
    assert_eq!(
        script.seen(),
        [
            "NewPassphrase(TurnOn)",
            "NewPassphrase(TooShort)",
            "NewPassphrase(TooShort)",
            "NewPassphrase(Confirm)",
            "NewPassphrase(Mismatch)",
            "NewPassphrase(Confirm)",
        ]
    );
    let right = unlock_with(Script::new([Ok("long enough!")]));
    assert!(store::open(dir.path(), &protector, &right).unwrap().close());
}

/// GIVEN a profile with a passphrase WHEN it is turned off with the right
/// passphrase THEN `store-key.v1` replaces `store-key.v2` and the next open
/// shows no prompt and finds the same data.
#[test]
fn given_a_profile_with_a_passphrase_then_turning_it_off_replaces_the_sealed_key() {
    let protector = Tagged(1);
    let dir = seeded_with_passphrase(&protector);
    let script = Script::new([Ok(PASSPHRASE)]);
    let turned = while_open(dir.path(), &protector, || {
        store::turn_off_passphrase(dir.path(), &protector, &unlock_with(script.clone()))
    });
    assert_eq!(turned, Ok(()));
    assert_eq!(script.seen(), ["Passphrase { retry: false }"]);
    assert!(dir.path().join(VAULT_KEY_FILE).is_file());
    assert!(
        !dir.path().join(SEALED_KEY_FILE).exists(),
        "sealed key gone"
    );
    assert!(!dir.path().join(format!("{VAULT_KEY_FILE}.tmp")).exists());

    let next = Script::new([]);
    let stores = store::open(dir.path(), &protector, &unlock_with(next.clone())).unwrap();
    assert!(!stores.passphrase());
    assert!(stores.close());
    assert!(next.seen().is_empty(), "no prompt: {:?}", next.seen());
    assert_eq!(read_marker(dir.path(), &protector).as_deref(), Some(MARKER));
}

/// GIVEN a profile with a passphrase WHEN turning it off gets a wrong
/// passphrase five times THEN `passphrase_rejected` is the result (as
/// `secret_rejected`), the files are unchanged, and the passphrase still
/// opens the profile; WHEN the prompt is cancelled THEN nothing changes.
#[test]
fn given_wrong_passphrases_when_turning_it_off_then_nothing_changes() {
    let protector = Tagged(1);
    let dir = seeded_with_passphrase(&protector);
    let before = durable(files(dir.path()));
    let wrong = Script::new([Ok("not the passphrase"); 5]);
    let state = while_open(dir.path(), &protector, || {
        store::turn_off_passphrase(dir.path(), &protector, &unlock_with(wrong.clone()))
    });
    assert_eq!(state, Err(StoreState::SecretRejected));
    assert_eq!(wrong.seen().len(), 5, "no more than five attempts");
    assert_eq!(durable(files(dir.path())), before);

    let cancelled = unlock_with(Script::new([Err(PromptError::Cancelled)]));
    let state = while_open(dir.path(), &protector, || {
        store::turn_off_passphrase(dir.path(), &protector, &cancelled)
    });
    assert_eq!(state, Err(StoreState::Locked));
    assert_eq!(durable(files(dir.path())), before);
    assert!(!dir.path().join(VAULT_KEY_FILE).exists());
    assert_eq!(read_marker(dir.path(), &protector).as_deref(), Some(MARKER));
}

/// GIVEN a profile that has both key files (a turn-off that stopped after
/// writing `store-key.v1`) WHEN it opens THEN the passphrase is still asked
/// (the sealed key wins), and `store-key.v1` is deleted.
#[test]
fn given_both_key_files_then_the_passphrase_wins_and_the_vault_key_is_deleted() {
    let protector = Tagged(1);
    let dir = seeded_with_passphrase(&protector);
    let sealed = fs::read(dir.path().join(SEALED_KEY_FILE)).unwrap();
    while_open(dir.path(), &protector, || {
        store::turn_off_passphrase(dir.path(), &protector, &unlock()).unwrap();
    });
    // The process stopped before `store-key.v2` was deleted.
    fs::write(dir.path().join(SEALED_KEY_FILE), &sealed).unwrap();
    assert!(dir.path().join(VAULT_KEY_FILE).exists());

    let wrong = unlock_with(Script::new([Ok("not the passphrase"); 5]));
    assert_eq!(
        store::open(dir.path(), &protector, &wrong).err(),
        Some(StoreState::SecretRejected),
        "the vault key does not open it without the passphrase"
    );
    let script = Script::new([]);
    let stores = store::open(dir.path(), &protector, &unlock_with(script.clone())).unwrap();
    assert!(stores.passphrase());
    assert!(stores.close());
    assert_eq!(script.seen(), ["Passphrase { retry: false }"]);
    assert!(
        !dir.path().join(VAULT_KEY_FILE).exists(),
        "vault key deleted"
    );
    assert_eq!(fs::read(dir.path().join(SEALED_KEY_FILE)).unwrap(), sealed);
}

/// GIVEN a profile that had only the vault key before passphrases became
/// optional WHEN it opens THEN no prompt is shown and nothing is upgraded.
#[test]
fn given_a_vault_key_profile_then_it_opens_without_a_prompt_and_is_not_upgraded() {
    let protector = Tagged(1);
    let dir = seeded(&protector);
    let before = durable(files(dir.path()));
    let script = Script::new([]);
    let stores = store::open(dir.path(), &protector, &unlock_with(script.clone())).unwrap();
    assert!(!stores.passphrase());
    assert!(stores.close());
    assert!(script.seen().is_empty());
    assert!(!dir.path().join(SEALED_KEY_FILE).exists());
    assert_eq!(
        durable(files(dir.path())).get(Path::new(VAULT_KEY_FILE)),
        before.get(Path::new(VAULT_KEY_FILE)),
        "the key file is untouched"
    );
}

/// GIVEN a profile with a passphrase WHEN a wrong passphrase is entered five
/// times THEN it is `secret_rejected` and not one byte changes; the right one
/// still opens it.
#[test]
fn given_wrong_passphrases_then_the_profile_is_rejected_and_unchanged() {
    let protector = Tagged(1);
    let dir = seeded_with_passphrase(&protector);
    let before = files(dir.path());
    let wrong = unlock_with(Script::new([Ok("not the passphrase"); 5]));
    assert_eq!(
        store::open(dir.path(), &protector, &wrong).err(),
        Some(StoreState::SecretRejected)
    );
    assert_eq!(files(dir.path()), before);
    assert_eq!(read_marker(dir.path(), &protector).as_deref(), Some(MARKER));
}

/// GIVEN a profile with data WHEN it is reset THEN the stores, every key file
/// and the marker are gone, and it opens as a new profile without the data
/// and without a prompt; WHEN it is in use THEN the reset is refused and
/// nothing is deleted.
#[test]
fn given_a_reset_then_the_profile_is_deleted_unless_it_is_in_use() {
    let protector = Tagged(1);
    let dir = seeded_with_passphrase(&protector);
    let stores = store::open(dir.path(), &protector, &unlock()).unwrap();
    assert_eq!(store::reset(dir.path()), Err(StoreState::ProfileInUse));
    assert!(stores.close());
    assert!(dir.path().join(SEALED_KEY_FILE).exists());

    fs::write(dir.path().join(VAULT_KEY_FILE), b"left over").unwrap();
    assert_eq!(store::reset(dir.path()), Ok(true));
    for gone in [
        SEALED_KEY_FILE,
        VAULT_KEY_FILE,
        STORE_DIR,
        store::SIGNED_OUT_FILE,
    ] {
        assert!(!dir.path().join(gone).exists(), "{gone}");
    }
    let script = Script::new([]);
    let stores = store::open(dir.path(), &protector, &unlock_with(script.clone())).unwrap();
    assert!(!stores.passphrase(), "a reset profile has no passphrase");
    assert!(
        stores
            .block_on(stores.crypto().get_custom_value(MARKER_KEY))
            .unwrap()
            .is_none()
    );
    assert!(stores.close());
    assert!(script.seen().is_empty(), "no prompt: {:?}", script.seen());
    assert_eq!(store::reset(TempDir::new().unwrap().path()), Ok(false));
}

/// Wraps like `Tagged(1)`, but cannot unwrap a sealed key: it stands in for a
/// vault whose read-back of a new sealed key file fails.
struct SealedUnreadable;

impl KeyProtector for SealedUnreadable {
    fn protect(&self, data: &[u8]) -> Result<Vec<u8>, persephone_core::vault::VaultError> {
        Tagged(1).protect(data)
    }

    fn unprotect(
        &self,
        blob: &[u8],
    ) -> Result<zeroize::Zeroizing<Vec<u8>>, persephone_core::vault::VaultError> {
        if blob.len() > 1 + persephone_core::vault::KEY_BYTES {
            return Err(persephone_core::vault::VaultError::Failed);
        }
        Tagged(1).unprotect(blob)
    }
}

/// Wraps like `Tagged(1)`, but never unwraps: it stands in for a vault whose
/// read-back of a new key file fails.
struct NeverUnwraps;

impl KeyProtector for NeverUnwraps {
    fn protect(&self, data: &[u8]) -> Result<Vec<u8>, persephone_core::vault::VaultError> {
        Tagged(1).protect(data)
    }

    fn unprotect(
        &self,
        _: &[u8],
    ) -> Result<zeroize::Zeroizing<Vec<u8>>, persephone_core::vault::VaultError> {
        Err(persephone_core::vault::VaultError::Failed)
    }
}

/// GIVEN a new key file that does not read back WHEN a profile is created
/// THEN neither key file nor store is left; WHEN the passphrase is turned on
/// THEN `store-key.v2` never appears (nor its temporary file) and the vault
/// key stays usable; a later switch with a working vault succeeds.
#[test]
fn given_a_failed_read_back_then_no_new_key_file_is_left() {
    let dir = TempDir::new().unwrap();
    assert_eq!(
        store::open(dir.path(), &NeverUnwraps, &unlock()).err(),
        Some(StoreState::KeyUnreadable)
    );
    for gone in [
        VAULT_KEY_FILE,
        "store-key.v1.tmp",
        SEALED_KEY_FILE,
        STORE_DIR,
    ] {
        assert!(!dir.path().join(gone).exists(), "{gone}");
    }

    let protector = Tagged(1);
    let profile = seeded(&protector);
    let before = durable(files(profile.path()));
    let state = while_open(profile.path(), &protector, || {
        store::turn_on_passphrase(profile.path(), &SealedUnreadable, &unlock())
    });
    assert_eq!(state, Err(StoreState::KeyUnreadable));
    assert!(!profile.path().join(SEALED_KEY_FILE).exists());
    assert!(!profile.path().join("store-key.v2.tmp").exists());
    assert!(
        profile.path().join(VAULT_KEY_FILE).exists(),
        "vault key kept"
    );
    assert_eq!(durable(files(profile.path())), before);
    assert_eq!(
        read_marker(profile.path(), &protector).as_deref(),
        Some(MARKER)
    );

    // And a turn-off through a vault that cannot unwrap keeps the sealed key.
    let sealed = seeded_with_passphrase(&protector);
    let before = durable(files(sealed.path()));
    let state = while_open(sealed.path(), &protector, || {
        store::turn_off_passphrase(sealed.path(), &NeverUnwraps, &unlock())
    });
    assert_eq!(state, Err(StoreState::KeyUnreadable));
    assert_eq!(durable(files(sealed.path())), before);
}

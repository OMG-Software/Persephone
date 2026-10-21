//! The protected profile against real SDK SQLite stores. On Windows the key is
//! wrapped by real DPAPI; the fail-closed rules are also checked with a
//! stand-in protector so they run on every platform.

use matrix_sdk_crypto::store::CryptoStore;
use persephone_contracts::StoreState;
use persephone_core::{
    store::{self, KEY_FILE, MAX_KEY_FILE_BYTES, STORE_DIR},
    vault::{KEY_BYTES, KeyProtector, Unsupported, VaultError},
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

mod common;
use common::Tagged;

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

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Creates a profile, stores the marker through the encrypted crypto store and
/// closes it.
fn seeded(protector: &dyn KeyProtector) -> TempDir {
    let dir = TempDir::new().unwrap();
    let stores = store::open(dir.path(), protector).expect("new profile opens");
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

fn read_marker(dir: &Path, protector: &dyn KeyProtector) -> Option<Vec<u8>> {
    let stores = store::open(dir, protector).expect("profile reopens");
    let value = stores
        .block_on(stores.crypto().get_custom_value(MARKER_KEY))
        .unwrap();
    assert!(stores.close());
    value
}

fn refused(dir: &Path, protector: &dyn KeyProtector) -> StoreState {
    match store::open(dir, protector) {
        Ok(stores) => {
            stores.close();
            panic!("profile opened");
        }
        Err(state) => state,
    }
}

#[test]
fn given_a_new_profile_then_the_key_is_saved_wrapped_and_the_stores_reopen_with_it() {
    let protector = Tagged(1);
    let dir = seeded(&protector);
    assert!(dir.path().join(KEY_FILE).is_file());
    assert!(
        dir.path()
            .join(STORE_DIR)
            .join("matrix-sdk-crypto.sqlite3")
            .is_file()
    );
    assert_eq!(read_marker(dir.path(), &protector).as_deref(), Some(MARKER));
}

/// The stores exist but the key is gone: the profile stays locked, no key is
/// made, and not one byte of the profile changes.
#[test]
fn given_a_store_without_its_key_then_the_profile_stays_locked_and_unchanged() {
    let protector = Tagged(1);
    let dir = seeded(&protector);
    fs::remove_file(dir.path().join(KEY_FILE)).unwrap();
    let before = files(dir.path());
    assert_eq!(refused(dir.path(), &protector), StoreState::KeyMissing);
    assert!(!dir.path().join(KEY_FILE).exists(), "no replacement key");
    assert_eq!(files(dir.path()), before);
}

#[test]
fn given_an_unreadable_key_file_then_the_profile_stays_locked_and_recovers_when_restored() {
    let protector = Tagged(1);
    let dir = seeded(&protector);
    let key_path = dir.path().join(KEY_FILE);
    let original = fs::read(&key_path).unwrap();
    let mut flipped = original.clone();
    *flipped.last_mut().unwrap() ^= 1;
    let mut no_magic = original.clone();
    no_magic[0] ^= 1;
    for bad in [
        // Wrapped for another context.
        {
            let mut other = original.clone();
            let tag = other.len() - KEY_BYTES - 1;
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
    // A flipped key byte unwraps, but the stores refuse it.
    fs::write(&key_path, &flipped).unwrap();
    assert_eq!(refused(dir.path(), &protector), StoreState::KeyRejected);
    fs::write(&key_path, &original).unwrap();
    assert_eq!(read_marker(dir.path(), &protector).as_deref(), Some(MARKER));
}

#[test]
fn given_another_profiles_key_then_the_stores_refuse_it_and_keep_their_data() {
    let protector = Tagged(1);
    let first = seeded(&protector);
    let second = seeded(&protector);
    let key_path = first.path().join(KEY_FILE);
    let original = fs::read(&key_path).unwrap();
    fs::copy(second.path().join(KEY_FILE), &key_path).unwrap();
    let before = files(first.path());
    assert_eq!(refused(first.path(), &protector), StoreState::KeyRejected);
    assert_eq!(files(first.path()), before, "a refused key changes nothing");
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
    let stores = store::open(dir.path(), &protector).unwrap();
    assert_eq!(refused(dir.path(), &protector), StoreState::ProfileInUse);
    assert!(stores.close());
    assert!(store::open(dir.path(), &protector).unwrap().close());
}

#[test]
fn given_no_vault_then_a_new_profile_stays_locked_and_creates_no_key_or_store() {
    let dir = TempDir::new().unwrap();
    assert_eq!(refused(dir.path(), &Unsupported), StoreState::Unsupported);
    assert!(!dir.path().join(KEY_FILE).exists());
    assert!(!dir.path().join(STORE_DIR).exists());
}

/// A first run that stopped after saving the key, or while writing it, can be
/// completed: nothing was protected with a key that was never saved.
#[test]
fn given_an_interrupted_first_run_then_opening_completes_it() {
    let protector = Tagged(1);
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(format!("{KEY_FILE}.tmp")), b"torn").unwrap();
    fs::create_dir(dir.path().join(STORE_DIR)).unwrap();
    assert!(store::open(dir.path(), &protector).unwrap().close());

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
    let stores = store::open(dir.path(), &protector).unwrap();
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
    store::mark_signed_out(dir.path()).unwrap();
    assert!(
        !dir.path().join(KEY_FILE).exists(),
        "key deleted at sign-out"
    );
    assert!(dir.path().join(store::SIGNED_OUT_FILE).exists());
    assert_eq!(read_marker(dir.path(), &protector), None, "old data gone");
    assert!(
        !dir.path().join(store::SIGNED_OUT_FILE).exists(),
        "wipe finished"
    );
    assert!(dir.path().join(KEY_FILE).exists(), "a new key");

    // A wipe that stopped after the files went but before the marker did
    // completes on the next open.
    let again = seeded(&protector);
    store::mark_signed_out(again.path()).unwrap();
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
        store::mark_signed_out(dir.path()).is_err(),
        "failure reported"
    );
    assert!(!dir.path().join(KEY_FILE).exists(), "key deleted anyway");
    assert_ne!(refused(dir.path(), &protector), StoreState::Protected);
    assert!(
        !dir.path().join(KEY_FILE).exists(),
        "no new key over the old data"
    );
}

#[cfg(windows)]
mod dpapi {
    use super::*;
    use persephone_core::vault::Dpapi;

    #[test]
    fn given_dpapi_then_no_profile_file_holds_the_raw_key_or_the_marker() {
        let protector = Dpapi::new();
        let dir = seeded(&protector);
        let saved = fs::read(dir.path().join(KEY_FILE)).unwrap();
        let blob = &saved[saved.iter().position(|b| *b == b'\n').unwrap() + 1..];
        let key = protector.unprotect(blob).unwrap();
        for (path, bytes) in files(dir.path()) {
            assert!(!contains(&bytes, key.as_ref()), "raw key in {path:?}");
            assert!(!contains(&bytes, MARKER), "marker in {path:?}");
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
        let first = protector.protect(&key).unwrap();
        let second = protector.protect(&key).unwrap();
        assert_ne!(first, second, "fresh salt per wrap");
        assert!(!contains(&first, key.as_ref()));
        assert_eq!(*protector.unprotect(&first).unwrap(), *key);
        let mut tampered = first.clone();
        let middle = tampered.len() / 2;
        tampered[middle] ^= 1;
        assert_eq!(protector.unprotect(&tampered), Err(VaultError::Failed));
    }
}

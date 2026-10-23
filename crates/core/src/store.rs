//! The protected local profile: one exclusive owner, one store key sealed by
//! the profile passphrase and wrapped by the vault (ADR-0006), and the SDK's
//! encrypted SQLite stores.
//!
//! The SDK opens an existing encrypted store without a key and then writes
//! plaintext, so the stores are never opened until the key is held. Every path
//! that cannot produce the right key leaves the profile locked and unchanged:
//!
//! | Profile | Result |
//! | --- | --- |
//! | New (no key, empty store directory) | Passphrase chosen; key created, sealed, wrapped and read back; then stores created |
//! | Key present | Passphrase asked; key unsealed; the stores must accept it |
//! | Only a key from before ADR-0006 | Stores opened with it; passphrase chosen; sealed key saved and read back; old key deleted |
//! | Store present, key absent | `KeyMissing`; no new key is made |
//! | Prompt cancelled or not shown | `Locked`; nothing written |
//! | [`MAX_ATTEMPTS`] wrong passphrases | `SecretRejected`; nothing written |
//! | Key unwrap fails, or the sealed key is malformed | `KeyUnreadable` |
//! | Stores refuse the key | `KeyRejected` |
//! | Another process holds the profile | `ProfileInUse` |
//! | No vault or no prompt on this platform | `Unsupported` |
//!
//! Layout: `profile.lock`, `store-key.v2` (magic plus the vault's blob of the
//! sealed key) and `store/` (state, event-cache, media and crypto databases).
//! A profile from before ADR-0006 has `store-key.v1` (magic plus the vault's
//! blob of the bare key) instead, until it is first opened.
//!
//! Signing out writes `signed-out` before it asks the homeserver to revoke
//! the session, so an interrupted sign-out never restores the account. The
//! marker then records whether revocation was confirmed, and the key is
//! deleted, so the encrypted files are unreadable from then on. The files
//! themselves are wiped the next time the profile opens, before anything else,
//! so no SDK work from the signed-out session can still use them. A crash at
//! any point repeats the wipe rather than leaving a profile half-deleted, and
//! a wipe without a confirmed revocation is reported.
//!
//! A reset (a forgotten passphrase) wipes the same way, with a marker that
//! records no revocation: the access token is in the encrypted store, so the
//! homeserver cannot be told.

use crate::credentials::{NewPassphrase, PromptError, SecretPrompt};
use crate::passphrase::{self, MAX_ATTEMPTS, MIN_PASSPHRASE_CHARS, SealError, Unlock};
use crate::vault::{KEY_BYTES, KeyProtector, StoreKey, VaultError, generate_key};
use matrix_sdk_base::store::StateStore;
use matrix_sdk_crypto::store::CryptoStore;
use matrix_sdk_sqlite::{
    OpenStoreError, SqliteCryptoStore, SqliteEventCacheStore, SqliteMediaStore, SqliteStateStore,
    SqliteStoreConfig,
};
use persephone_contracts::StoreState;
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{Read, Write},
    path::Path,
};
use tokio::runtime::Runtime;
use zeroize::Zeroizing;

pub const LOCK_FILE: &str = "profile.lock";
/// The sealed and wrapped store key.
pub const KEY_FILE: &str = "store-key.v2";
/// A store key wrapped by the vault alone, from before ADR-0006.
pub const OLD_KEY_FILE: &str = "store-key.v1";
const KEY_FILES: [&str; 2] = [KEY_FILE, OLD_KEY_FILE];
pub const STORE_DIR: &str = "store";
/// Present from the start of sign-out until the profile has been wiped.
pub const SIGNED_OUT_FILE: &str = "signed-out";
/// Marker contents. Anything but `REVOKED` (an interrupted sign-out, a failed
/// revocation, or a marker that cannot be read) means the homeserver may not
/// have been told.
const REVOKING: &[u8] = b"revoking\n";
const REVOKED: &[u8] = b"revoked\n";
const NOT_REVOKED: &[u8] = b"not-revoked\n";

fn write_marker(dir: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut marker = File::create(dir.join(SIGNED_OUT_FILE))?;
    marker.write_all(contents)?;
    marker.sync_all()?;
    sync_dir(dir)
}

/// Makes a created or removed entry in `dir` durable: on POSIX systems a
/// file's own sync does not cover its directory entry.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Windows cannot open a directory with `File::open`; there the file's own
/// flush is the only step taken.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Records durably that the profile in `dir` is being signed out, before the
/// homeserver is asked to revoke the session: from here the next start wipes
/// the profile instead of restoring its account.
pub fn begin_sign_out(dir: &Path) -> std::io::Result<()> {
    write_marker(dir, REVOKING)
}

/// Finishes signing out locally: records whether the homeserver confirmed
/// the revocation, then deletes the store key, so the encrypted stores become
/// unreadable. The open stores keep working until they close. The key is
/// deleted even if the marker cannot be written: a store without its key
/// opens as `key_missing`, never as a new or signed-in profile.
pub fn mark_signed_out(dir: &Path, revoked: bool) -> std::io::Result<()> {
    let marked = write_marker(dir, if revoked { REVOKED } else { NOT_REVOKED });
    let removed = KEY_FILES
        .iter()
        .try_for_each(|file| remove_if_present(dir, file))
        // A removal lost in a crash would bring the key back.
        .and_then(|()| sync_dir(dir));
    marked.and(removed)
}

/// Removes `name` from `dir`; a file that is already gone is not an error.
fn remove_if_present(dir: &Path, name: &str) -> std::io::Result<()> {
    match fs::remove_file(dir.join(name)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Whether the marker records a confirmed revocation; reads at most a few
/// bytes, and any failure counts as unconfirmed.
fn revocation_confirmed(dir: &Path) -> bool {
    let mut contents = Vec::new();
    File::open(dir.join(SIGNED_OUT_FILE))
        .and_then(|marker| marker.take(64).read_to_end(&mut contents))
        .is_ok()
        && contents == REVOKED
}

/// Deletes a signed-out profile's store files and key, then the marker.
/// The marker goes last, so an interrupted wipe is repeated. Returns `None`
/// when the profile was not signed out, otherwise whether the homeserver
/// confirmed the revocation.
pub fn wipe_signed_out(dir: &Path) -> std::io::Result<Option<bool>> {
    match fs::symlink_metadata(dir.join(SIGNED_OUT_FILE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // Unknown whether the profile was signed out: it must not open.
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    match fs::remove_dir_all(dir.join(STORE_DIR)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    for file in KEY_FILES {
        remove_if_present(dir, file)?;
        remove_if_present(dir, &format!("{file}.tmp"))?;
    }
    let revoked = revocation_confirmed(dir);
    fs::remove_file(dir.join(SIGNED_OUT_FILE))?;
    Ok(Some(revoked))
}

/// Deletes the profile in `dir`, for a forgotten passphrase (ADR-0006). The
/// marker is written first, as "not revoked", so an interrupted reset is
/// finished by the next open. Returns whether the store held anything.
pub fn reset(dir: &Path) -> Result<bool, StoreState> {
    let _lock = lock_profile(dir)?;
    let had_data = has_entries(&dir.join(STORE_DIR))?;
    write_marker(dir, NOT_REVOKED).map_err(|_| StoreState::StoreUnavailable)?;
    wipe_signed_out(dir).map_err(|_| StoreState::StoreUnavailable)?;
    Ok(had_data)
}

const KEY_MAGIC: &[u8] = b"PERSEPHONE-STORE-KEY-2\n";
const OLD_KEY_MAGIC: &[u8] = b"PERSEPHONE-STORE-KEY-1\n";
/// A DPAPI blob for a sealed key is a few hundred bytes; anything far larger
/// is not a key file.
pub const MAX_KEY_FILE_BYTES: u64 = 4096;

/// The SDK's four encrypted stores.
pub struct SdkStores {
    pub state: SqliteStateStore,
    pub event_cache: SqliteEventCacheStore,
    pub media: SqliteMediaStore,
    pub crypto: SqliteCryptoStore,
}

/// The open stores. The core closes them explicitly with [`Stores::close`];
/// dropping them otherwise (for example while a panic unwinds) still closes
/// them inside their runtime, because the SQLite pool's destructor spawns
/// blocking work and panics outside a Tokio runtime.
pub struct Stores {
    runtime: Runtime,
    sdk: Option<SdkStores>,
    /// Opening wiped a signed-out profile whose revocation was not confirmed.
    unconfirmed_sign_out: bool,
    // Held until the stores are closed or dropped.
    _lock: File,
}

impl Stores {
    /// Whether opening wiped a signed-out profile without a confirmed
    /// revocation, so the homeserver may still hold that session.
    pub fn unconfirmed_sign_out(&self) -> bool {
        self.unconfirmed_sign_out
    }

    /// Runs SDK store work on the stores' runtime.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// The runtime the stores (and any SDK client over them) run on.
    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub fn sdk(&self) -> &SdkStores {
        self.sdk
            .as_ref()
            .expect("stores are open until closed or dropped")
    }

    pub fn crypto(&self) -> &SqliteCryptoStore {
        &self.sdk().crypto
    }

    /// Closes every store, crypto last, and reports whether all closed
    /// cleanly. The profile lock is released only after this returns.
    pub fn close(mut self) -> bool {
        let Some(sdk) = self.sdk.take() else {
            return false;
        };
        self.runtime.block_on(async move {
            let results = [
                sdk.state.close().await.is_ok(),
                sdk.event_cache.close().await.is_ok(),
                sdk.media.close().await.is_ok(),
                sdk.crypto.close().await.is_ok(),
            ];
            // Dropped here, inside the runtime.
            drop(sdk);
            results.iter().all(|ok| *ok)
        })
    }
}

impl Drop for Stores {
    fn drop(&mut self) {
        if let Some(sdk) = self.sdk.take() {
            let _context = self.runtime.enter();
            drop(sdk);
        }
    }
}

fn read_key_file(path: &Path, magic: &[u8]) -> Result<Vec<u8>, StoreState> {
    let file = File::open(path).map_err(|_| StoreState::KeyUnreadable)?;
    let mut bytes = Vec::new();
    file.take(MAX_KEY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| StoreState::KeyUnreadable)?;
    if bytes.len() as u64 > MAX_KEY_FILE_BYTES {
        return Err(StoreState::KeyUnreadable);
    }
    match bytes.strip_prefix(magic) {
        Some(blob) if !blob.is_empty() => Ok(blob.to_vec()),
        _ => Err(StoreState::KeyUnreadable),
    }
}

fn vault_error(error: VaultError) -> StoreState {
    match error {
        VaultError::Unsupported => StoreState::Unsupported,
        VaultError::Failed => StoreState::KeyUnreadable,
    }
}

fn has_entries(dir: &Path) -> Result<bool, StoreState> {
    match fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(StoreState::StoreUnavailable),
    }
}

/// Whether `path` exists; a failure to tell keeps the profile closed.
fn exists(path: &Path) -> Result<bool, StoreState> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(StoreState::StoreUnavailable),
    }
}

/// What a prompt that gave no entry leaves: a cancelled or failed prompt
/// leaves the profile locked, so Unlock can try again.
fn prompt_state(error: PromptError) -> StoreState {
    match error {
        PromptError::Unsupported => StoreState::Unsupported,
        PromptError::Cancelled | PromptError::Invalid | PromptError::Failed => StoreState::Locked,
    }
}

/// Asks for the passphrase until it unseals the key, waiting longer before
/// each new entry. Nothing is written.
fn unseal(
    dir: &Path,
    protector: &dyn KeyProtector,
    unlock: &Unlock,
) -> Result<StoreKey, StoreState> {
    let wrapped = read_key_file(&dir.join(KEY_FILE), KEY_MAGIC)?;
    let sealed = protector.unprotect(&wrapped).map_err(vault_error)?;
    for attempt in 0..MAX_ATTEMPTS {
        std::thread::sleep(unlock.wait_before(attempt));
        let entry = unlock
            .source
            .ask_secret(SecretPrompt::Passphrase { retry: attempt > 0 });
        let passphrase = match entry {
            Ok(passphrase) => passphrase,
            // An empty entry counts as a wrong one.
            Err(PromptError::Invalid) => continue,
            Err(error) => return Err(prompt_state(error)),
        };
        match passphrase::open(&sealed, &passphrase) {
            Ok(key) => return Ok(key),
            Err(SealError::Wrong) => {}
            Err(SealError::Malformed) => return Err(StoreState::KeyUnreadable),
            Err(SealError::Failed) => return Err(StoreState::StoreUnavailable),
        }
    }
    Err(StoreState::SecretRejected)
}

/// Asks for a new passphrase twice until the entries are long enough and
/// match, for at most [`MAX_ATTEMPTS`] rounds.
fn choose_passphrase(
    unlock: &Unlock,
    first: NewPassphrase,
) -> Result<Zeroizing<String>, StoreState> {
    let ask = |step| unlock.source.ask_secret(SecretPrompt::NewPassphrase(step));
    let mut step = first;
    for _ in 0..MAX_ATTEMPTS {
        let entry = match ask(step) {
            Ok(entry) if entry.chars().count() >= MIN_PASSPHRASE_CHARS => entry,
            Ok(_) | Err(PromptError::Invalid) => {
                step = NewPassphrase::TooShort;
                continue;
            }
            Err(error) => return Err(prompt_state(error)),
        };
        match ask(NewPassphrase::Confirm) {
            Ok(again) if *again == *entry => return Ok(entry),
            Ok(_) | Err(PromptError::Invalid) => step = NewPassphrase::Mismatch,
            Err(error) => return Err(prompt_state(error)),
        }
    }
    Err(StoreState::Locked)
}

/// Seals, wraps and saves `key`, then reads the file back and opens it with
/// the same passphrase. A file that does not give back the key is removed.
fn save_key(
    dir: &Path,
    protector: &dyn KeyProtector,
    key: &StoreKey,
    passphrase: &str,
    unlock: &Unlock,
) -> Result<(), StoreState> {
    let sealed =
        passphrase::seal(key, passphrase, unlock.kdf).map_err(|_| StoreState::StoreUnavailable)?;
    let blob = protector.protect(&sealed).map_err(vault_error)?;
    let temporary = dir.join(format!("{KEY_FILE}.tmp"));
    let write = || -> std::io::Result<()> {
        let mut file = File::create(&temporary)?;
        file.write_all(KEY_MAGIC)?;
        file.write_all(&blob)?;
        file.sync_all()?;
        fs::rename(&temporary, dir.join(KEY_FILE))?;
        sync_dir(dir)
    };
    write().map_err(|_| StoreState::StoreUnavailable)?;
    let read_back = read_key_file(&dir.join(KEY_FILE), KEY_MAGIC)
        .and_then(|saved| protector.unprotect(&saved).map_err(vault_error))
        .and_then(|sealed| {
            passphrase::open(&sealed, passphrase).map_err(|_| StoreState::KeyUnreadable)
        });
    match read_back {
        Ok(read_back) if *read_back == **key => Ok(()),
        other => {
            let _ = remove_if_present(dir, KEY_FILE);
            Err(other.err().unwrap_or(StoreState::KeyUnreadable))
        }
    }
}

/// The key from before ADR-0006, wrapped by the vault alone.
fn old_key(dir: &Path, protector: &dyn KeyProtector) -> Result<StoreKey, StoreState> {
    let wrapped = read_key_file(&dir.join(OLD_KEY_FILE), OLD_KEY_MAGIC)?;
    let bytes = protector.unprotect(&wrapped).map_err(vault_error)?;
    if bytes.len() != KEY_BYTES {
        return Err(StoreState::KeyUnreadable);
    }
    let mut key = Zeroizing::new([0u8; KEY_BYTES]);
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// Deletes the key from before ADR-0006 once the sealed key is in place.
/// While it exists, DPAPI alone opens the profile, so a failure closes the
/// stores again; the next open retries.
fn remove_old_key(dir: &Path, stores: Stores) -> Result<Stores, StoreState> {
    let removed = remove_if_present(dir, OLD_KEY_FILE)
        .and_then(|()| remove_if_present(dir, &format!("{OLD_KEY_FILE}.tmp")))
        .and_then(|()| sync_dir(dir));
    match removed {
        Ok(()) => Ok(stores),
        Err(_) => {
            stores.close();
            Err(StoreState::StoreUnavailable)
        }
    }
}

fn open_error(error: OpenStoreError) -> StoreState {
    match error {
        OpenStoreError::InitCipher(_) => StoreState::KeyRejected,
        _ => StoreState::StoreUnavailable,
    }
}

/// Takes the profile's exclusive lock, creating the directory if needed.
fn lock_profile(dir: &Path) -> Result<File, StoreState> {
    fs::create_dir_all(dir).map_err(|_| StoreState::StoreUnavailable)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LOCK_FILE))
        .map_err(|_| StoreState::StoreUnavailable)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(StoreState::ProfileInUse),
        Err(TryLockError::Error(_)) => Err(StoreState::StoreUnavailable),
    }
}

/// Opens the profile in `dir`, creating it if it is new. Asks for the
/// passphrase (or a new one) through `unlock`; see the module documentation.
pub fn open(
    dir: &Path,
    protector: &dyn KeyProtector,
    unlock: &Unlock,
) -> Result<Stores, StoreState> {
    let lock = lock_profile(dir)?;
    // A profile signed out earlier is wiped before anything else; it then
    // opens as a new profile.
    let wiped = wipe_signed_out(dir).map_err(|_| StoreState::StoreUnavailable)?;
    let unconfirmed_sign_out = wiped == Some(false);

    let store_dir = dir.join(STORE_DIR);
    if exists(&dir.join(KEY_FILE))? {
        let key = unseal(dir, protector, unlock)?;
        let stores = open_stores(&store_dir, &key, lock, unconfirmed_sign_out)?;
        drop(key);
        // An interrupted upgrade may have left the old key behind.
        return remove_old_key(dir, stores);
    }
    if exists(&dir.join(OLD_KEY_FILE))? {
        // The stores must accept the old key before it is sealed again.
        let key = old_key(dir, protector)?;
        let stores = open_stores(&store_dir, &key, lock, unconfirmed_sign_out)?;
        let saved = choose_passphrase(unlock, NewPassphrase::Upgrade)
            .and_then(|passphrase| save_key(dir, protector, &key, &passphrase, unlock));
        drop(key);
        if let Err(state) = saved {
            stores.close();
            return Err(state);
        }
        return remove_old_key(dir, stores);
    }
    if has_entries(&store_dir)? {
        return Err(StoreState::KeyMissing);
    }
    let passphrase = choose_passphrase(unlock, NewPassphrase::Choose)?;
    let key = generate_key().map_err(vault_error)?;
    save_key(dir, protector, &key, &passphrase, unlock)?;
    drop(passphrase);
    open_stores(&store_dir, &key, lock, unconfirmed_sign_out)
}

/// Opens the SDK's stores with `key`. The configuration's own copy is dropped
/// as soon as the stores are open.
fn open_stores(
    store_dir: &Path,
    key: &StoreKey,
    lock: File,
    unconfirmed_sign_out: bool,
) -> Result<Stores, StoreState> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("core-store")
        .enable_all()
        .build()
        .map_err(|_| StoreState::StoreUnavailable)?;
    let config = SqliteStoreConfig::new(store_dir).key(Some(key.as_ref()));
    let opened = runtime.block_on(async {
        // Crypto first: a wrong key is refused before the other stores are
        // created in a new directory.
        let crypto = SqliteCryptoStore::open_with_config(&config)
            .await
            .map_err(open_error)?;
        let rest = async {
            Ok::<_, StoreState>((
                SqliteStateStore::open_with_config(&config)
                    .await
                    .map_err(open_error)?,
                SqliteEventCacheStore::open_with_config(&config)
                    .await
                    .map_err(open_error)?,
                SqliteMediaStore::open_with_config(&config)
                    .await
                    .map_err(open_error)?,
            ))
        }
        .await;
        match rest {
            Ok((state, event_cache, media)) => Ok((state, event_cache, media, crypto)),
            Err(error) => {
                let _ = crypto.close().await;
                Err(error)
            }
        }
    });
    drop(config);
    let (state, event_cache, media, crypto) = opened?;
    Ok(Stores {
        runtime,
        sdk: Some(SdkStores {
            state,
            event_cache,
            media,
            crypto,
        }),
        unconfirmed_sign_out,
        _lock: lock,
    })
}

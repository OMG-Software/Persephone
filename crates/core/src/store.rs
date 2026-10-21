//! The protected local profile: one exclusive owner, one vault-wrapped key,
//! and the SDK's encrypted SQLite stores.
//!
//! The SDK opens an existing encrypted store without a key and then writes
//! plaintext, so the stores are never opened until the key is held. Every path
//! that cannot produce the right key leaves the profile locked and unchanged:
//!
//! | Profile | Result |
//! | --- | --- |
//! | New (no key, empty store directory) | Key created, wrapped, read back, then stores created |
//! | Key present | Key unwrapped; the stores must accept it |
//! | Store present, key absent | `KeyMissing`; no new key is made |
//! | Key unwrap fails | `KeyUnreadable` |
//! | Stores refuse the key | `KeyRejected` |
//! | Another process holds the profile | `ProfileInUse` |
//!
//! Layout: `profile.lock`, `store-key.v1` (magic plus the vault's blob) and
//! `store/` (state, event-cache, media and crypto databases).

use crate::vault::{KeyProtector, StoreKey, VaultError, generate_key};
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

pub const LOCK_FILE: &str = "profile.lock";
pub const KEY_FILE: &str = "store-key.v1";
pub const STORE_DIR: &str = "store";
const KEY_MAGIC: &[u8] = b"PERSEPHONE-STORE-KEY-1\n";
/// A DPAPI blob for 32 bytes is a few hundred bytes; anything far larger is
/// not a key file.
pub const MAX_KEY_FILE_BYTES: u64 = 4096;

/// The open stores. Dropping this without [`Stores::close`] leaves the SDK to
/// close them implicitly; the core always closes explicitly.
pub struct Stores {
    runtime: Runtime,
    pub state: SqliteStateStore,
    pub event_cache: SqliteEventCacheStore,
    pub media: SqliteMediaStore,
    pub crypto: SqliteCryptoStore,
    // Held until the stores are closed and this value is dropped.
    _lock: File,
}

impl Stores {
    /// Runs SDK store work on the stores' runtime.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// Closes every store, crypto last, and reports whether all closed
    /// cleanly. The profile lock is released only after this returns.
    pub fn close(self) -> bool {
        let Stores {
            runtime,
            state,
            event_cache,
            media,
            crypto,
            _lock,
        } = self;
        runtime.block_on(async {
            let results = [
                state.close().await.is_ok(),
                event_cache.close().await.is_ok(),
                media.close().await.is_ok(),
                crypto.close().await.is_ok(),
            ];
            results.iter().all(|ok| *ok)
        })
    }
}

fn read_key_file(path: &Path) -> Result<Vec<u8>, StoreState> {
    let file = File::open(path).map_err(|_| StoreState::KeyUnreadable)?;
    let mut bytes = Vec::new();
    file.take(MAX_KEY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| StoreState::KeyUnreadable)?;
    if bytes.len() as u64 > MAX_KEY_FILE_BYTES {
        return Err(StoreState::KeyUnreadable);
    }
    match bytes.strip_prefix(KEY_MAGIC) {
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

/// Makes and saves a new key for a profile with no store. The key is used
/// only after its saved form unwraps to the same bytes.
fn create_key(dir: &Path, protector: &dyn KeyProtector) -> Result<StoreKey, StoreState> {
    let key = generate_key().map_err(vault_error)?;
    let blob = protector.protect(&key).map_err(vault_error)?;
    let temporary = dir.join(format!("{KEY_FILE}.tmp"));
    let write = || -> std::io::Result<()> {
        let mut file = File::create(&temporary)?;
        file.write_all(KEY_MAGIC)?;
        file.write_all(&blob)?;
        file.sync_all()?;
        fs::rename(&temporary, dir.join(KEY_FILE))
    };
    write().map_err(|_| StoreState::StoreUnavailable)?;
    let saved = read_key_file(&dir.join(KEY_FILE))?;
    match protector.unprotect(&saved) {
        Ok(read_back) if *read_back == *key => Ok(key),
        _ => Err(StoreState::KeyUnreadable),
    }
}

fn open_error(error: OpenStoreError) -> StoreState {
    match error {
        OpenStoreError::InitCipher(_) => StoreState::KeyRejected,
        _ => StoreState::StoreUnavailable,
    }
}

/// Opens the profile in `dir`, creating it if it is new.
pub fn open(dir: &Path, protector: &dyn KeyProtector) -> Result<Stores, StoreState> {
    fs::create_dir_all(dir).map_err(|_| StoreState::StoreUnavailable)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LOCK_FILE))
        .map_err(|_| StoreState::StoreUnavailable)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(StoreState::ProfileInUse),
        Err(TryLockError::Error(_)) => return Err(StoreState::StoreUnavailable),
    }

    let store_dir = dir.join(STORE_DIR);
    let key = match fs::symlink_metadata(dir.join(KEY_FILE)) {
        Ok(_) => {
            let blob = read_key_file(&dir.join(KEY_FILE))?;
            protector.unprotect(&blob).map_err(vault_error)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if has_entries(&store_dir)? {
                return Err(StoreState::KeyMissing);
            }
            create_key(dir, protector)?
        }
        Err(_) => return Err(StoreState::StoreUnavailable),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("core-store")
        .enable_all()
        .build()
        .map_err(|_| StoreState::StoreUnavailable)?;
    let config = SqliteStoreConfig::new(&store_dir).key(Some(key.as_ref()));
    drop(key);
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
        state,
        event_cache,
        media,
        crypto,
        _lock: lock,
    })
}

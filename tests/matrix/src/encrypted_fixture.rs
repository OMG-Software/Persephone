//! Encrypted SDK-store helpers for the disposable Matrix restart fixture.
//!
//! Checkpoints are kept in the SDK crypto store because the SDK state store's
//! custom-value API does not encrypt its values.

use std::path::Path;

use anyhow::{Result, ensure};
use matrix_sdk::{Client, config::StoreConfig, cross_process_lock::CrossProcessLockConfig};
use matrix_sdk_crypto::store::CryptoStore;
use matrix_sdk_sqlite::{
    SqliteCryptoStore, SqliteEventCacheStore, SqliteMediaStore, SqliteStateStore, SqliteStoreConfig,
};
use zeroize::Zeroizing;

const CHECKPOINT_KEY: &str = "persephone.fixture.restart.v1";
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;

/// An in-memory test key that is cleared when dropped.
///
/// This type deliberately does not implement `Debug` or serialization traits.
pub struct FixtureKey(Zeroizing<[u8; 32]>);

impl FixtureKey {
    /// Generate a fresh key using the system random source.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes)?;
        Ok(Self::from_bytes(bytes))
    }

    /// Wrap caller-provided key bytes in zeroizing storage.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Borrow the key bytes for the SDK store configuration.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// A client backed by four same-key encrypted SQLite stores.
///
/// The crypto-store handle is retained so fixture checkpoints can use its
/// encrypted custom-value API rather than the plaintext state-store API.
pub struct EncryptedClient {
    pub client: Client,
    crypto: SqliteCryptoStore,
}

/// Open an SDK client over encrypted state, event-cache, media, and crypto stores.
///
/// A missing key is rejected before any store is opened or any directory is
/// created. The helper only configures the client; building it does not perform
/// homeserver requests.
pub async fn open_encrypted_client(
    base_url: &str,
    path: &Path,
    key: Option<&FixtureKey>,
) -> Result<EncryptedClient> {
    let key = key.ok_or_else(|| anyhow::anyhow!("encrypted fixture requires a key"))?;
    let config = SqliteStoreConfig::new(path).key(Some(key.as_bytes()));

    let state_store = SqliteStateStore::open_with_config(&config).await?;
    let event_cache_store = SqliteEventCacheStore::open_with_config(&config).await?;
    let media_store = SqliteMediaStore::open_with_config(&config).await?;
    let crypto_store = SqliteCryptoStore::open_with_config(&config).await?;

    let store_config = StoreConfig::new(CrossProcessLockConfig::multi_process("restart-fixture"))
        .state_store(state_store)
        .event_cache_store(event_cache_store)
        .media_store(media_store)
        .crypto_store(crypto_store.clone());

    let client = crate::fixture_client_builder(base_url)?
        .store_config(store_config)
        .build()
        .await?;

    Ok(EncryptedClient {
        client,
        crypto: crypto_store,
    })
}

/// Save a non-empty bounded checkpoint through the encrypted crypto store.
pub async fn save_checkpoint(client: &EncryptedClient, bytes: &[u8]) -> Result<()> {
    ensure!(!bytes.is_empty(), "checkpoint must not be empty");
    ensure!(
        bytes.len() <= MAX_CHECKPOINT_BYTES,
        "checkpoint exceeds {MAX_CHECKPOINT_BYTES} bytes"
    );
    client
        .crypto
        .set_custom_value(CHECKPOINT_KEY, bytes.to_vec())
        .await?;
    Ok(())
}

/// Read a present, non-empty, bounded checkpoint from the encrypted crypto store.
pub async fn read_checkpoint(client: &EncryptedClient) -> Result<Vec<u8>> {
    let bytes = client
        .crypto
        .get_custom_value(CHECKPOINT_KEY)
        .await?
        .ok_or_else(|| anyhow::anyhow!("encrypted fixture checkpoint is missing"))?;
    ensure!(!bytes.is_empty(), "checkpoint must not be empty");
    ensure!(
        bytes.len() <= MAX_CHECKPOINT_BYTES,
        "checkpoint exceeds {MAX_CHECKPOINT_BYTES} bytes"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const CHECKPOINT_MARKER: &[u8] = b"PERSEPHONE_ENCRYPTED_RESTART_CHECKPOINT_MARKER_79C2A4";

    fn find_marker_files(root: &Path, marker: &[u8], found: &mut Vec<String>) -> Result<()> {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                find_marker_files(&path, marker, found)?;
                continue;
            }

            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                // SQLite WAL/SHM files can disappear while a store closes.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if bytes.windows(marker.len()).any(|window| window == marker) {
                found.push(path.display().to_string());
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn given_no_key_when_opening_then_path_is_not_created() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("not-created");

        let result = open_encrypted_client("http://127.0.0.1:9", &path, None).await;

        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn given_encrypted_checkpoint_when_reopened_then_wrong_key_fails_and_right_key_recovers_it()
     {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("encrypted");
        let key = FixtureKey::generate().unwrap();
        let wrong_key = FixtureKey::generate().unwrap();

        let first = open_encrypted_client("http://127.0.0.1:9", &path, Some(&key))
            .await
            .unwrap();
        save_checkpoint(&first, CHECKPOINT_MARKER).await.unwrap();
        assert_eq!(read_checkpoint(&first).await.unwrap(), CHECKPOINT_MARKER);
        drop(first);

        assert!(
            open_encrypted_client("http://127.0.0.1:9", &path, Some(&wrong_key))
                .await
                .is_err(),
            "opening encrypted SDK stores with the wrong key must fail"
        );

        let reopened = open_encrypted_client("http://127.0.0.1:9", &path, Some(&key))
            .await
            .unwrap();
        assert_eq!(read_checkpoint(&reopened).await.unwrap(), CHECKPOINT_MARKER);
        drop(reopened);

        let mut marker_files = Vec::new();
        find_marker_files(&path, CHECKPOINT_MARKER, &mut marker_files).unwrap();
        assert!(
            marker_files.is_empty(),
            "checkpoint plaintext appeared in SDK store artifacts: {marker_files:?}"
        );

        let plaintext_path = temp.path().join("plaintext-control");
        let plaintext_store = SqliteCryptoStore::open_with_key(&plaintext_path, None)
            .await
            .unwrap();
        plaintext_store
            .set_custom_value(CHECKPOINT_KEY, CHECKPOINT_MARKER.to_vec())
            .await
            .unwrap();
        drop(plaintext_store);

        let mut plaintext_marker_files = Vec::new();
        find_marker_files(
            &plaintext_path,
            CHECKPOINT_MARKER,
            &mut plaintext_marker_files,
        )
        .unwrap();
        assert!(
            !plaintext_marker_files.is_empty(),
            "artifact scanner failed to detect the plaintext crypto-store control"
        );
    }

    #[tokio::test]
    async fn given_missing_or_invalid_checkpoint_then_read_and_write_fail_closed() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("encrypted");
        let key = FixtureKey::generate().unwrap();
        let client = open_encrypted_client("http://127.0.0.1:9", &path, Some(&key))
            .await
            .unwrap();

        assert!(read_checkpoint(&client).await.is_err());
        assert!(save_checkpoint(&client, b"").await.is_err());
        assert!(
            save_checkpoint(&client, &vec![0; MAX_CHECKPOINT_BYTES + 1])
                .await
                .is_err()
        );
        assert!(read_checkpoint(&client).await.is_err());
    }
}

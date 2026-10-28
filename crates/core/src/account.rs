//! The Matrix account: one SDK client over the protected stores.
//!
//! The client is built from the four open stores, a validated homeserver and
//! the single HTTP policy (TLS 1.3, no proxy, no redirects). Well-known lookup
//! is off, so the client talks only to the homeserver the user named. The
//! session (tokens and device) is saved as a crypto-store custom value, which
//! the store encrypts; the state store's custom values are not encrypted and
//! are never used for it (`docs/evidence/encrypted-restart.md`).
//!
//! Encryption policy (ADR-0005): room keys go only to devices signed by their
//! owner's cross-signing identity, so this device must be verified before it
//! can send in an encrypted room; incoming messages are shown only from
//! cross-signed devices or legacy sessions. Cross-signing and backups are never
//! set up automatically: the user starts them (`security.rs`).
//!
//! Room keys can arrive after the message they open: from another device, or
//! from the key backup, which the SDK asks only once a message fails to
//! decrypt. [`take_key_rooms`] names the rooms that got keys since it was last
//! called, so the page can fetch them again (`session.rs`).

use crate::credentials::Credentials;
use crate::homeserver::{self, Homeserver};
use crate::store::Stores;
use crate::views::MAX_ROOMS;
use futures_core::Stream;
use matrix_sdk::{
    Client,
    authentication::matrix::MatrixSession,
    config::{RequestConfig, StoreConfig, SyncSettings},
    cross_process_lock::CrossProcessLockConfig,
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    ruma::{api::error::ErrorKind, events::StateEventType},
    sliding_sync::{SlidingSync, SlidingSyncList, SlidingSyncMode, Version, VersionBuilder},
};
use matrix_sdk_crypto::{
    CollectStrategy, DecryptionSettings, TrustRequirement,
    store::{CryptoStore, types::RoomKeyInfo},
};
use persephone_contracts::SignInError;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, pin::Pin, task::Poll, time::Duration};
use zeroize::Zeroizing;

/// Crypto-store custom value holding the saved account.
pub const ACCOUNT_KEY: &str = "persephone.account.v1";
const MAX_SAVED_ACCOUNT_BYTES: usize = 64 * 1024;
pub const DEVICE_NAME: &str = "Persephone";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Long-poll wait for one sync round. A lock waits for at most one round.
pub const SYNC_POLL: Duration = Duration::from_secs(5);
/// Most batches [`take_key_rooms`] takes in one call; later ones wait.
const MAX_KEY_BATCHES: usize = 64;

#[derive(Serialize, Deserialize)]
struct SavedAccount {
    homeserver: String,
    session: MatrixSession,
}

/// Builds a client over the open stores. Must run inside the stores' runtime.
pub async fn build_client(homeserver: &Homeserver, stores: &Stores) -> Result<Client, SignInError> {
    let sdk = stores.sdk();
    let store_config = StoreConfig::new(CrossProcessLockConfig::multi_process("persephone"))
        .state_store(sdk.state.clone())
        .event_cache_store(sdk.event_cache.clone())
        .media_store(sdk.media.clone())
        .crypto_store(sdk.crypto.clone());
    let http = homeserver::http_client().map_err(|_| SignInError::Failed)?;
    Client::builder()
        .homeserver_url(homeserver.url().as_str())
        .respect_login_well_known(false)
        .http_client(http)
        .request_config(
            RequestConfig::new()
                .disable_retry()
                .timeout(REQUEST_TIMEOUT),
        )
        .store_config(store_config)
        .with_room_key_recipient_strategy(CollectStrategy::IdentityBasedStrategy)
        .with_decryption_settings(DecryptionSettings {
            sender_device_trust_requirement: TrustRequirement::CrossSignedOrLegacy,
        })
        .with_encryption_settings(EncryptionSettings {
            auto_enable_cross_signing: false,
            auto_enable_backups: false,
            // Once recovery is set up, a key missing for a message is fetched
            // from the backup when that message fails to decrypt.
            backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
        })
        .build()
        .await
        .map_err(|_| SignInError::Failed)
}

fn sign_in_error(error: &matrix_sdk::Error) -> SignInError {
    match error.client_api_error_kind() {
        Some(ErrorKind::Forbidden | ErrorKind::UserDeactivated) => SignInError::InvalidCredentials,
        Some(_) => SignInError::Failed,
        None => match error {
            matrix_sdk::Error::Http(http)
                if matches!(**http, matrix_sdk::HttpError::Reqwest(_)) =>
            {
                SignInError::Unreachable
            }
            _ => SignInError::Failed,
        },
    }
}

/// Signs in with a password and saves the session encrypted. Returns the
/// user ID.
pub async fn sign_in(
    client: &Client,
    homeserver: &Homeserver,
    credentials: &Credentials,
    stores: &Stores,
) -> Result<String, SignInError> {
    client
        .matrix_auth()
        .login_username(credentials.username.as_str(), credentials.password.as_str())
        .initial_device_display_name(DEVICE_NAME)
        .await
        .map_err(|error| sign_in_error(&error))?;
    save_session(client, homeserver, stores).await
}

/// Saves the client's new session encrypted, after a sign-in or a
/// registration. Returns the user ID.
pub async fn save_session(
    client: &Client,
    homeserver: &Homeserver,
    stores: &Stores,
) -> Result<String, SignInError> {
    let session = client.matrix_auth().session().ok_or(SignInError::Failed)?;
    let user_id = session.meta.user_id.to_string();
    let saved = Zeroizing::new(
        serde_json::to_vec(&SavedAccount {
            homeserver: homeserver.url().to_string(),
            session,
        })
        .map_err(|_| SignInError::Failed)?,
    );
    stores
        .crypto()
        .set_custom_value(ACCOUNT_KEY, saved.to_vec())
        .await
        .map_err(|_| SignInError::Failed)?;
    Ok(user_id)
}

/// Restores a saved account without a password, if there is one. `Ok(None)`
/// means no account was saved.
pub async fn restore(stores: &Stores) -> Result<Option<(Client, String)>, SignInError> {
    let Some(bytes) = stores
        .crypto()
        .get_custom_value(ACCOUNT_KEY)
        .await
        .map_err(|_| SignInError::Failed)?
    else {
        return Ok(None);
    };
    let bytes = Zeroizing::new(bytes);
    if bytes.len() > MAX_SAVED_ACCOUNT_BYTES {
        return Err(SignInError::Failed);
    }
    let saved: SavedAccount = serde_json::from_slice(&bytes).map_err(|_| SignInError::Failed)?;
    // The saved URL is checked again: a build without the development feature
    // refuses a plain-HTTP account saved by one with it.
    let homeserver =
        Homeserver::parse(&saved.homeserver).map_err(|_| SignInError::HomeserverRefused)?;
    let user_id = saved.session.meta.user_id.to_string();
    let client = build_client(&homeserver, stores).await?;
    client
        .restore_session(saved.session)
        .await
        .map_err(|_| SignInError::Failed)?;
    Ok(Some((client, user_id)))
}

/// The selected sync engine. Selection follows successful capability
/// discovery only: a discovery error is reported, never treated as absence.
pub enum Engine {
    Sliding(SlidingSync),
    Classic,
}

pub async fn select_engine(client: &Client) -> Result<Engine, SignInError> {
    let versions = client
        .supported_versions()
        .await
        .map_err(|_| SignInError::SyncFailed)?;
    if VersionBuilder::DiscoverNative
        .build(Some(&versions))
        .is_err()
    {
        return Ok(Engine::Classic);
    }
    let sliding = client
        .sliding_sync("persephone")
        .map_err(|_| SignInError::SyncFailed)?
        .version(Version::Native)
        .add_list(
            SlidingSyncList::builder("rooms")
                .sync_mode(SlidingSyncMode::new_growing(50))
                .required_state(vec![
                    (StateEventType::RoomName, String::new()),
                    (StateEventType::RoomCanonicalAlias, String::new()),
                    (StateEventType::RoomEncryption, String::new()),
                    (StateEventType::RoomCreate, String::new()),
                    (StateEventType::RoomMember, "$ME".into()),
                ])
                .timeline_limit(20),
        )
        // Typing notices and receipts are received to be shown; whether this
        // account sends its own is a setting (ADR-0023).
        .with_all_extensions()
        .poll_timeout(SYNC_POLL)
        .network_timeout(REQUEST_TIMEOUT)
        .build()
        .await
        .map_err(|_| SignInError::SyncFailed)?;
    Ok(Engine::Sliding(sliding))
}

/// Runs one complete sync round and returns the rooms it touched. Callers
/// must not cancel it midway: the SDK processes the response after the
/// request returns.
pub async fn sync_round(client: &Client, engine: &Engine) -> Result<Vec<String>, SignInError> {
    match engine {
        Engine::Sliding(sliding) => {
            let summary = sliding
                .sync_once()
                .await
                .map_err(|_| SignInError::SyncFailed)?;
            Ok(summary.rooms.iter().map(ToString::to_string).collect())
        }
        Engine::Classic => {
            let response = client
                .sync_once(SyncSettings::default().timeout(SYNC_POLL))
                .await
                .map_err(|_| SignInError::SyncFailed)?;
            Ok(response
                .rooms
                .joined
                .keys()
                .chain(response.rooms.left.keys())
                // A new invitation changes the invitation list.
                .chain(response.rooms.invited.keys())
                .map(ToString::to_string)
                .collect())
        }
    }
}

/// Rooms for which this device stored new room keys.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyRooms {
    /// These rooms, at most [`MAX_ROOMS`].
    Listed(BTreeSet<String>),
    /// More rooms than may be listed, or the stream dropped batches it could
    /// not keep (it lagged): any joined room may have new keys. Never expanded
    /// to room IDs; the receiver applies it to the rooms it watches.
    All,
}

impl KeyRooms {
    fn note<E>(&mut self, batch: Result<Vec<RoomKeyInfo>, E>) {
        let Self::Listed(rooms) = self else {
            return;
        };
        let overflow = match batch {
            Ok(keys) => keys.iter().any(|key| {
                if !rooms.contains(key.room_id.as_str()) {
                    rooms.insert(key.room_id.to_string());
                }
                rooms.len() > MAX_ROOMS
            }),
            Err(_) => true,
        };
        if overflow {
            *self = Self::All;
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Listed(rooms) if rooms.is_empty())
    }

    /// Whether `room` may have new keys.
    pub fn includes(&self, room: &str) -> bool {
        match self {
            Self::Listed(rooms) => rooms.contains(room),
            Self::All => true,
        }
    }
}

/// Takes the batches the SDK's room-key stream holds now, without waiting.
/// The stream is `Encryption::room_keys_received_stream`: keys from other
/// devices and from the backup alike. Later batches stay for the next call.
pub async fn take_key_rooms<S, E>(mut stream: Pin<&mut S>) -> KeyRooms
where
    S: Stream<Item = Result<Vec<RoomKeyInfo>, E>> + ?Sized,
{
    std::future::poll_fn(|context| {
        let mut rooms = KeyRooms::Listed(BTreeSet::new());
        for _ in 0..MAX_KEY_BATCHES {
            match stream.as_mut().poll_next(context) {
                Poll::Ready(Some(batch)) => rooms.note(batch),
                // Nothing more now, or the stream ended.
                Poll::Ready(None) | Poll::Pending => break,
            }
        }
        Poll::Ready(rooms)
    })
    .await
}

/// Whether the stream has any item ready now; takes at most
/// [`MAX_KEY_BATCHES`] of them without waiting.
pub async fn take_any<S>(mut stream: Pin<&mut S>) -> bool
where
    S: Stream + ?Sized,
{
    std::future::poll_fn(|context| {
        let mut any = false;
        for _ in 0..MAX_KEY_BATCHES {
            match stream.as_mut().poll_next(context) {
                Poll::Ready(Some(_)) => any = true,
                Poll::Ready(None) | Poll::Pending => break,
            }
        }
        Poll::Ready(any)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::{
        SessionMeta, SessionTokens,
        ruma::{device_id, room_id, user_id},
    };
    use matrix_sdk_crypto::{
        EncryptionSettings as KeySettings, OlmMachine, encrypt_room_key_export,
        types::EventEncryptionAlgorithm, vodozemac::Curve25519PublicKey,
    };

    /// GIVEN a signed-in client watching its room keys WHEN three keys for two
    /// rooms are imported (the way a backup download stores them) THEN one
    /// take names both rooms once, and the next take, with nothing new, none.
    #[test]
    fn given_imported_room_keys_then_their_rooms_are_named_once() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let client = Client::builder()
                .homeserver_url("https://example.org")
                .build()
                .await
                .unwrap();
            client
                .restore_session(MatrixSession {
                    meta: SessionMeta {
                        user_id: user_id!("@me:example.org").to_owned(),
                        device_id: device_id!("ME").to_owned(),
                    },
                    tokens: SessionTokens {
                        access_token: "synthetic".into(),
                        refresh_token: None,
                    },
                })
                .await
                .unwrap();
            let mut keys = Box::pin(
                client
                    .encryption()
                    .room_keys_received_stream()
                    .await
                    .expect("signed in"),
            );
            assert_eq!(
                take_key_rooms(keys.as_mut()).await,
                KeyRooms::Listed(BTreeSet::new())
            );

            // A peer makes the keys: two sessions in one room, one in another.
            let peer = OlmMachine::new(user_id!("@peer:example.org"), device_id!("PEER")).await;
            let (a, b) = (room_id!("!a:example.org"), room_id!("!b:example.org"));
            for room in [a, b, a] {
                peer.discard_room_key(room).await.unwrap();
                peer.share_room_key(room, std::iter::empty(), KeySettings::default())
                    .await
                    .unwrap();
            }
            let exported = peer.store().export_room_keys(|_| true).await.unwrap();
            assert_eq!(exported.len(), 3);
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(
                file.path(),
                encrypt_room_key_export(&exported, "synthetic", 1).unwrap(),
            )
            .unwrap();
            client
                .encryption()
                .import_room_keys(file.path().to_owned(), "synthetic")
                .await
                .unwrap();

            assert_eq!(
                take_key_rooms(keys.as_mut()).await,
                KeyRooms::Listed([a.to_string(), b.to_string()].into())
            );
            assert_eq!(
                take_key_rooms(keys.as_mut()).await,
                KeyRooms::Listed(BTreeSet::new())
            );
        });
    }

    /// GIVEN keys for more rooms than may be listed, or a stream that lagged
    /// WHEN noted THEN any joined room counts, and stays counted.
    #[test]
    fn given_too_many_rooms_or_a_lag_then_all_rooms_count() {
        let key = |room: usize| RoomKeyInfo {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            room_id: format!("!r{room}:example.org").try_into().unwrap(),
            sender_key: Curve25519PublicKey::from_bytes([0; 32]),
            session_id: room.to_string(),
        };
        let listed = |rooms: &KeyRooms| match rooms {
            KeyRooms::Listed(rooms) => rooms.len(),
            KeyRooms::All => usize::MAX,
        };
        let mut rooms = KeyRooms::Listed(BTreeSet::new());
        rooms.note::<()>(Ok((0..MAX_ROOMS).map(key).collect()));
        assert_eq!(listed(&rooms), MAX_ROOMS);
        rooms.note::<()>(Ok(vec![key(0)]));
        assert_eq!(listed(&rooms), MAX_ROOMS, "a room already listed");
        rooms.note::<()>(Ok(vec![key(MAX_ROOMS)]));
        assert_eq!(rooms, KeyRooms::All);
        rooms.note::<()>(Ok(vec![key(0)]));
        assert_eq!(rooms, KeyRooms::All);

        let mut lagged = KeyRooms::Listed(BTreeSet::new());
        lagged.note(Err(()));
        assert_eq!(lagged, KeyRooms::All);
    }
}

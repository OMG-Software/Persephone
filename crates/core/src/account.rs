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
//!
//! The homeserver can end the session itself: the device was signed out
//! elsewhere, or the account was reset or removed. [`session_ended`] tells
//! the sync loop when that happened, and the session then deletes this
//! device's profile as a sign-out does.

use crate::credentials::Credentials;
use crate::homeserver::{self, Homeserver};
use crate::store::Stores;
use crate::views::MAX_ROOMS;
use futures_core::Stream;
use matrix_sdk::{
    Client, SessionChange,
    authentication::{
        matrix::MatrixSession,
        oauth::{ClientId, OAuthSession, UserSession},
    },
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
use tokio::sync::broadcast::{Receiver, error::TryRecvError};
use zeroize::Zeroizing;

/// Crypto-store custom value holding the saved account.
pub const ACCOUNT_KEY: &str = "persephone.account.v1";
const MAX_SAVED_ACCOUNT_BYTES: usize = 64 * 1024;
pub const DEVICE_NAME: &str = "Persephone";
/// The page that names Persephone to an OAuth provider (ADR-0030).
pub const PROJECT_URI: &str = "https://omg-software.github.io/Persephone/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Long-poll wait for one sync round. A lock waits for at most one round.
pub const SYNC_POLL: Duration = Duration::from_secs(5);
/// Most batches [`take_key_rooms`] takes in one call; later ones wait.
const MAX_KEY_BATCHES: usize = 64;

/// The saved account: a password session (`session`), or an OAuth one
/// (`oauth`, ADR-0030), never both. Records from before OAuth have only
/// `session`, and still read.
#[derive(Serialize, Deserialize)]
struct SavedAccount {
    homeserver: String,
    #[serde(default)]
    session: Option<MatrixSession>,
    #[serde(default)]
    oauth: Option<SavedOAuth>,
}

/// An OAuth session, in parts the SDK can serialize.
#[derive(Serialize, Deserialize)]
struct SavedOAuth {
    client_id: String,
    user: UserSession,
}

/// Builds a client over the open stores. Must run inside the stores' runtime.
pub async fn build_client(homeserver: &Homeserver, stores: &Stores) -> Result<Client, SignInError> {
    let sdk = stores.sdk();
    let store_config = StoreConfig::new(CrossProcessLockConfig::multi_process("persephone"))
        .state_store(sdk.state.clone())
        .event_cache_store(sdk.event_cache.clone())
        .media_store(sdk.media.clone())
        .crypto_store(sdk.crypto.clone());
    // An unreadable proxy record refuses the connection: never direct.
    let proxy = crate::proxy::load(stores)
        .await
        .map_err(|_| SignInError::Failed)?;
    let http = homeserver::http_client(proxy.as_ref()).map_err(|_| SignInError::Failed)?;
    Client::builder()
        .homeserver_url(homeserver.url().as_str())
        .respect_login_well_known(false)
        // An access token with a refresh token expires; the SDK refreshes
        // it, and the sync loop saves the new tokens (`save_if_refreshed`).
        .handle_refresh_tokens()
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
    let (saved, user_id) = match client.oauth().full_session() {
        Some(oauth) => {
            let user_id = oauth.user.meta.user_id.to_string();
            let saved = SavedAccount {
                homeserver: homeserver.url().to_string(),
                session: None,
                oauth: Some(SavedOAuth {
                    client_id: oauth.client_id.as_str().to_owned(),
                    user: oauth.user,
                }),
            };
            (saved, user_id)
        }
        None => {
            let session = client.matrix_auth().session().ok_or(SignInError::Failed)?;
            let user_id = session.meta.user_id.to_string();
            let saved = SavedAccount {
                homeserver: homeserver.url().to_string(),
                session: Some(session),
                oauth: None,
            };
            (saved, user_id)
        }
    };
    let saved = Zeroizing::new(serde_json::to_vec(&saved).map_err(|_| SignInError::Failed)?);
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
    let client = build_client(&homeserver, stores).await?;
    let user_id = match (saved.session, saved.oauth) {
        (Some(session), None) => {
            let user_id = session.meta.user_id.to_string();
            client
                .restore_session(session)
                .await
                .map_err(|_| SignInError::Failed)?;
            user_id
        }
        (None, Some(oauth)) => {
            let user_id = oauth.user.meta.user_id.to_string();
            client
                .restore_session(OAuthSession {
                    client_id: ClientId::new(oauth.client_id),
                    user: oauth.user,
                })
                .await
                .map_err(|_| SignInError::Failed)?;
            user_id
        }
        // Neither or both: not a record this build wrote.
        _ => return Err(SignInError::Failed),
    };
    Ok(Some((client, user_id)))
}

/// Saves the session again when the SDK refreshed its access token since
/// `last` (the token last saved), so a restart restores the new one. An
/// OAuth session refreshes; a password session does only if the server gave
/// it a refresh token, which sign-in does not ask for, but it is saved the
/// same way if it does. Returns the token now saved.
pub async fn save_if_refreshed(
    client: &Client,
    stores: &Stores,
    last: Option<Zeroizing<String>>,
) -> Option<Zeroizing<String>> {
    let now = Zeroizing::new(client.access_token()?);
    if last.as_ref().is_some_and(|last| **last == *now) {
        return last;
    }
    let homeserver = Homeserver::parse(client.homeserver().as_str()).ok()?;
    match save_session(client, &homeserver, stores).await {
        Ok(_) => Some(now),
        Err(_) => last,
    }
}

/// An OAuth sign-in that is started (ADR-0030): the loopback redirect and the
/// provider's page, which the user has yet to be asked about.
pub struct OAuthStart {
    redirect: crate::oauth_login::Redirect,
    authorization: matrix_sdk::authentication::oauth::OAuthAuthorizationData,
}

impl OAuthStart {
    /// The provider's page to open in the browser.
    pub fn url(&self) -> &url::Url {
        &self.authorization.url
    }
}

/// Starts signing in through the homeserver's OAuth 2.0 provider (ADR-0030):
/// reads its metadata, binds the loopback redirect, and registers as a
/// native client with it. Nothing is asked here: the caller asks the user,
/// with no lease held, then finishes with [`finish_oauth`] or
/// [`abort_oauth`].
pub async fn start_oauth(client: &Client) -> Result<OAuthStart, SignInError> {
    use crate::oauth_login::Redirect;
    use matrix_sdk::authentication::oauth::registration::{
        ApplicationType, ClientMetadata, Localized, OAuthGrantType,
    };
    // A server without an OAuth provider has none to offer.
    client
        .oauth()
        .server_metadata()
        .await
        .map_err(|_| SignInError::OidcUnavailable)?;
    let redirect = Redirect::bind().await.map_err(|_| SignInError::Failed)?;
    let redirect_uri = redirect.uri();
    let project = url::Url::parse(PROJECT_URI).map_err(|_| SignInError::Failed)?;
    let mut metadata = ClientMetadata::new(
        ApplicationType::Native,
        vec![OAuthGrantType::AuthorizationCode {
            redirect_uris: vec![redirect_uri.clone()],
        }],
        Localized::new(project, []),
    );
    metadata.client_name = Some(Localized::new(DEVICE_NAME.to_owned(), []));
    let metadata = matrix_sdk::ruma::serde::Raw::new(&metadata).map_err(|_| SignInError::Failed)?;
    let authorization = client
        .oauth()
        .login(redirect_uri, None, Some(metadata.into()), None)
        .build()
        .await
        .map_err(|_| SignInError::OidcUnavailable)?;
    Ok(OAuthStart {
        redirect,
        authorization,
    })
}

/// Forgets a started sign-in the user declined.
pub async fn abort_oauth(client: &Client, start: OAuthStart) {
    client.oauth().abort_login(&start.authorization.state).await;
}

/// Finishes a started sign-in once the provider's page is open: waits for
/// the redirect (until `abandoned`), finishes the login and saves the
/// session encrypted. Returns the user ID.
pub async fn finish_oauth(
    client: &Client,
    homeserver: &Homeserver,
    stores: &Stores,
    start: OAuthStart,
    abandoned: impl Fn() -> bool,
) -> Result<String, SignInError> {
    use crate::oauth_login::RedirectError;
    use matrix_sdk::utils::UrlOrQuery;
    let OAuthStart {
        redirect,
        authorization,
    } = start;
    let query = match redirect.wait(abandoned).await {
        Ok(query) => query,
        Err(error) => {
            client.oauth().abort_login(&authorization.state).await;
            return Err(match error {
                RedirectError::TimedOut | RedirectError::Abandoned => SignInError::Cancelled,
                RedirectError::Failed => SignInError::Failed,
            });
        }
    };
    client
        .oauth()
        .finish_login(UrlOrQuery::Query(query))
        .await
        .map_err(|error| sign_in_error(&error))?;
    save_session(client, homeserver, stores).await
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

/// Whether the homeserver ended this session for good, as the SDK reports it
/// in `changes` (from `Client::subscribe_to_session_changes`): an unknown
/// access token without soft logout, once any refresh was refused. Asked
/// after a failed sync round. A second request must be refused the same way,
/// so one stray answer never deletes the profile. A soft logout does not end
/// the session: the account still exists and the device may sign in again.
pub async fn session_ended(client: &Client, changes: &mut Receiver<SessionChange>) -> bool {
    if !hard_logout(changes) {
        return false;
    }
    // Its result is not needed: the SDK reports a refused token in `changes`.
    let _ = client.whoami().await;
    hard_logout(changes)
}

/// Whether the newest change waiting in `changes` ends the session. The SDK
/// keeps only the newest change for a receiver that falls behind.
fn hard_logout(changes: &mut Receiver<SessionChange>) -> bool {
    let mut ended = false;
    loop {
        match changes.try_recv() {
            Ok(SessionChange::UnknownToken(token)) => ended = !token.soft_logout,
            Ok(SessionChange::TokensRefreshed) => ended = false,
            Err(TryRecvError::Lagged(_)) => {}
            Err(TryRecvError::Empty | TryRecvError::Closed) => return ended,
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

    /// GIVEN a saved account from before OAuth and an OAuth one WHEN read
    /// THEN the first is a password session and the second an OAuth one,
    /// tokens and client ID kept, and an OAuth record writes back the same.
    #[test]
    fn given_saved_accounts_of_both_kinds_then_each_reads_back() {
        let old: SavedAccount = serde_json::from_str(
            r#"{"homeserver":"https://example.org/","session":{"user_id":"@me:example.org","device_id":"ME","access_token":"a"}}"#,
        )
        .unwrap();
        assert!(old.session.is_some() && old.oauth.is_none());

        let oauth: SavedAccount = serde_json::from_str(
            r#"{"homeserver":"https://example.org/","oauth":{"client_id":"client-1","user":{"user_id":"@me:example.org","device_id":"ME","access_token":"a","refresh_token":"r"}}}"#,
        )
        .unwrap();
        let saved = oauth.oauth.as_ref().unwrap();
        assert!(oauth.session.is_none());
        assert_eq!(saved.client_id, "client-1");
        assert_eq!(saved.user.tokens.refresh_token.as_deref(), Some("r"));
        let again: SavedAccount =
            serde_json::from_slice(&serde_json::to_vec(&oauth).unwrap()).unwrap();
        assert_eq!(again.oauth.unwrap().user.meta.user_id, "@me:example.org");
    }

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

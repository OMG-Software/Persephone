//! The Matrix account: one SDK client over the protected stores.
//!
//! The client is built from the four open stores, a validated homeserver and
//! the single HTTP policy (TLS 1.3, no proxy, no redirects). Well-known lookup
//! is off, so the client talks only to the homeserver the user named. The
//! session (tokens and device) is saved as a crypto-store custom value, which
//! the store encrypts; the state store's custom values are not encrypted and
//! are never used for it (`docs/evidence/encrypted-restart.md`).

use crate::credentials::Credentials;
use crate::homeserver::{self, Homeserver};
use crate::store::Stores;
use matrix_sdk::{
    Client,
    authentication::matrix::MatrixSession,
    config::{RequestConfig, StoreConfig, SyncSettings},
    cross_process_lock::CrossProcessLockConfig,
    ruma::{api::error::ErrorKind, events::StateEventType},
    sliding_sync::{SlidingSync, SlidingSyncList, SlidingSyncMode, Version, VersionBuilder},
};
use matrix_sdk_crypto::store::CryptoStore;
use persephone_contracts::SignInError;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use zeroize::Zeroizing;

/// Crypto-store custom value holding the saved account.
pub const ACCOUNT_KEY: &str = "persephone.account.v1";
const MAX_SAVED_ACCOUNT_BYTES: usize = 64 * 1024;
const DEVICE_NAME: &str = "Persephone";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Long-poll wait for one sync round. A lock waits for at most one round.
pub const SYNC_POLL: Duration = Duration::from_secs(5);

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
        .with_all_extensions()
        .without_typing_extension()
        .without_receipt_extension()
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
                .map(ToString::to_string)
                .collect())
        }
    }
}

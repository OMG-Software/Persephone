//! The signed-in account's encryption identity (ADR-0005): reading its state,
//! setting it up, and verifying this device with the recovery key.
//!
//! * **Set up** is offered only for an account the homeserver says has no
//!   cross-signing identity. Bootstrapping over an existing identity would
//!   replace it, so the homeserver is asked again first. The first upload asks
//!   the homeserver how to authenticate; the second sends the password asked
//!   in the core's own window. Recovery is then enabled, which stores the
//!   cross-signing keys and a new key backup in secret storage, and returns
//!   the recovery key for the core to show once.
//! * **Recover** opens secret storage with the recovery key: the SDK imports
//!   the cross-signing keys, marks the identity verified and signs this device.
//! * The state shown is read from the local crypto store, together with what
//!   the homeserver last said about the account's identity. The first read
//!   asks it, so a new device never offers to set one up over an existing one.
//!   An identity only this device holds (a set-up whose upload failed) counts
//!   as none, so set-up can be retried; the retry uploads the same keys.
//! * **Set up recovery** alone is offered when the identity is set up but
//!   recovery is not (its first attempt failed); it needs no password.
//!
//! These functions run under a session lease inside the stores' runtime; the
//! session owns the thread and the prompts (`session.rs`). Errors are codes
//! only. The SDK copies the password into its request; that copy is outside
//! the zeroing guarantee, as at sign-in.

use matrix_sdk::{
    Client, HttpError,
    encryption::{
        recovery::{RecoveryError, RecoveryState as SdkRecoveryState},
        secret_storage::SecretStorageError,
    },
    ruma::{
        UserId,
        api::client::{
            keys::get_keys,
            uiaa::{AuthData, MatrixUserIdentifier, Password, UserIdentifier},
        },
    },
};
use persephone_contracts::{IdentityState, RecoveryState, SecurityError};
use zeroize::Zeroizing;

/// The identity state from the local store and `published`, what the
/// homeserver last said (`None` before it was asked).
pub async fn identity_state(
    client: &Client,
    user_id: &UserId,
    published: Option<bool>,
) -> IdentityState {
    let encryption = client.encryption();
    let Ok(identity) = encryption.get_user_identity(user_id).await else {
        return IdentityState::Unknown;
    };
    let identity = match (identity, published) {
        (_, None) => return IdentityState::Unknown,
        // Not on the homeserver: nobody else can trust it yet.
        (None, Some(_)) | (Some(_), Some(false)) => return IdentityState::None,
        (Some(identity), Some(true)) => identity,
    };
    let signed = encryption
        .get_own_device()
        .await
        .ok()
        .flatten()
        .is_some_and(|device| device.is_cross_signed_by_owner());
    if identity.is_verified() && signed {
        IdentityState::Verified
    } else {
        IdentityState::Unverified
    }
}

/// Whether this device holds all of the account's private cross-signing keys,
/// which recovery stores.
pub async fn holds_private_keys(client: &Client) -> bool {
    client
        .encryption()
        .cross_signing_status()
        .await
        .is_some_and(|status| status.is_complete())
}

pub fn recovery_state(client: &Client) -> RecoveryState {
    match client.encryption().recovery().state() {
        SdkRecoveryState::Enabled => RecoveryState::Enabled,
        SdkRecoveryState::Disabled => RecoveryState::Disabled,
        SdkRecoveryState::Incomplete => RecoveryState::Incomplete,
        SdkRecoveryState::Unknown => RecoveryState::Unknown,
    }
}

/// Asks the homeserver whether the account has a published identity. The
/// local store is no answer: a set-up whose upload failed leaves an identity
/// there. A published identity is then also stored, for recovery.
pub async fn query_identity(client: &Client, user_id: &UserId) -> Result<bool, SecurityError> {
    let mut request = get_keys::v3::Request::new();
    request.device_keys.insert(user_id.to_owned(), Vec::new());
    let response = client
        .send(request)
        .await
        .map_err(|error| http_error(&error))?;
    let published = response.master_keys.contains_key(user_id);
    if published {
        let _ = client.encryption().request_user_identity(user_id).await;
    }
    Ok(published)
}

/// Why set-up failed, and whether the identity reached the homeserver
/// (`None` when that is not known).
pub struct SetUpFailure {
    pub error: SecurityError,
    pub published: Option<bool>,
}

/// Sets up cross-signing and recovery for an account without an identity.
/// Returns the new recovery key.
pub async fn set_up(
    client: &Client,
    user_id: &UserId,
    password: &Zeroizing<String>,
) -> Result<Zeroizing<String>, SetUpFailure> {
    let failed = |error, published| SetUpFailure { error, published };
    match query_identity(client, user_id).await {
        Ok(true) => return Err(failed(SecurityError::IdentityExists, Some(true))),
        Ok(false) => {}
        Err(error) => return Err(failed(error, None)),
    }
    let encryption = client.encryption();
    if let Err(error) = encryption.bootstrap_cross_signing(None).await {
        let Some(info) = error.as_uiaa_response() else {
            return Err(failed(sdk_error(&error), None));
        };
        let mut auth = Password::new(
            UserIdentifier::Matrix(MatrixUserIdentifier::new(user_id.to_string())),
            password.to_string(),
        );
        auth.session = info.session.clone();
        // The first attempt created the keys locally; this one uploads them
        // (not the `_if_needed` form, which would now see an identity).
        encryption
            .bootstrap_cross_signing(Some(AuthData::Password(auth)))
            .await
            .map_err(|error| {
                if error.as_uiaa_response().is_some() {
                    // Refused: nothing was uploaded.
                    failed(SecurityError::InvalidPassword, Some(false))
                } else {
                    failed(sdk_error(&error), None)
                }
            })?;
    }
    enable_recovery(client)
        .await
        .map_err(|error| failed(error, Some(true)))
}

/// Sets up recovery for an identity this device holds: secret storage and a
/// new key backup. Returns the new recovery key. Needs no password.
pub async fn enable_recovery(client: &Client) -> Result<Zeroizing<String>, SecurityError> {
    client
        .encryption()
        .recovery()
        .enable()
        .await
        .map(Zeroizing::new)
        .map_err(|error| recovery_error(&error))
}

/// Verifies this device with the recovery key (or its passphrase).
pub async fn recover(client: &Client, key: &Zeroizing<String>) -> Result<(), SecurityError> {
    client
        .encryption()
        .recovery()
        .recover(key)
        .await
        .map_err(|error| recovery_error(&error))
}

fn recovery_error(error: &RecoveryError) -> SecurityError {
    match error {
        RecoveryError::BackupExistsOnServer => SecurityError::BackupExistsOnServer,
        RecoveryError::Sdk(error) => sdk_error(error),
        RecoveryError::SecretStorage(
            SecretStorageError::SecretStorageKey(_) | SecretStorageError::Decryption(_),
        ) => SecurityError::WrongRecoveryKey,
        RecoveryError::SecretStorage(SecretStorageError::Sdk(error)) => sdk_error(error),
        RecoveryError::SecretStorage(_) => SecurityError::Failed,
    }
}

fn sdk_error(error: &matrix_sdk::Error) -> SecurityError {
    match error {
        matrix_sdk::Error::Http(http) => http_error(http),
        _ => SecurityError::Failed,
    }
}

fn http_error(error: &HttpError) -> SecurityError {
    match error {
        HttpError::Reqwest(_) => SecurityError::Unreachable,
        _ => SecurityError::Failed,
    }
}

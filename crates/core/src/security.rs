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
//! * The state shown is read from the local crypto store, except that the
//!   first read asks the homeserver whether the account has an identity, so a
//!   new device never offers to set one up over an existing one.
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
        api::client::uiaa::{AuthData, MatrixUserIdentifier, Password, UserIdentifier},
    },
};
use persephone_contracts::{IdentityState, RecoveryState, SecurityError};
use zeroize::Zeroizing;

/// The identity state from the local store. `queried` says whether the
/// homeserver was asked once; before that, no identity means "not known".
pub async fn identity_state(client: &Client, user_id: &UserId, queried: bool) -> IdentityState {
    let encryption = client.encryption();
    let Ok(identity) = encryption.get_user_identity(user_id).await else {
        return IdentityState::Unknown;
    };
    let Some(identity) = identity else {
        return if queried {
            IdentityState::None
        } else {
            IdentityState::Unknown
        };
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

pub fn recovery_state(client: &Client) -> RecoveryState {
    match client.encryption().recovery().state() {
        SdkRecoveryState::Enabled => RecoveryState::Enabled,
        SdkRecoveryState::Disabled => RecoveryState::Disabled,
        SdkRecoveryState::Incomplete => RecoveryState::Incomplete,
        SdkRecoveryState::Unknown => RecoveryState::Unknown,
    }
}

/// Asks the homeserver whether the account has an identity (and stores what
/// it says). Returns whether one exists.
pub async fn query_identity(client: &Client, user_id: &UserId) -> Result<bool, SecurityError> {
    client
        .encryption()
        .request_user_identity(user_id)
        .await
        .map(|identity| identity.is_some())
        .map_err(|error| sdk_error(&error))
}

/// Sets up cross-signing and recovery for an account without an identity.
/// Returns the new recovery key.
pub async fn set_up(
    client: &Client,
    user_id: &UserId,
    password: &Zeroizing<String>,
) -> Result<Zeroizing<String>, SecurityError> {
    if query_identity(client, user_id).await? {
        return Err(SecurityError::IdentityExists);
    }
    let encryption = client.encryption();
    if let Err(error) = encryption.bootstrap_cross_signing(None).await {
        let Some(info) = error.as_uiaa_response() else {
            return Err(sdk_error(&error));
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
                    SecurityError::InvalidPassword
                } else {
                    sdk_error(&error)
                }
            })?;
    }
    encryption
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
        matrix_sdk::Error::Http(http) if matches!(**http, HttpError::Reqwest(_)) => {
            SecurityError::Unreachable
        }
        _ => SecurityError::Failed,
    }
}

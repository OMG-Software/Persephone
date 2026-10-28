//! Verifying by comparing emoji (SAS): this device with another device of the
//! same account, or this account with a contact. The security thread
//! (`session.rs`) owns at most one flow and advances it between other steps,
//! one lease per step.
//!
//! * This device: it asks the account's own identity, and accepts a request
//!   sent to the device by the account's own user.
//! * A contact: the request is a message in an existing direct-message room
//!   with them (`contacts.rs`), and an incoming one is taken only from the
//!   contact of the direct-message room it arrived in. A match signs the
//!   contact's identity with the account's user-signing key, so both starting
//!   and accepting need this device to hold that key; without it the SDK
//!   would finish the flow without verifying anyone.
//! * The flow advances on its own where the protocol needs no decision: it
//!   starts the comparison once the other device is ready (when this device
//!   asked) and accepts a comparison the other device starts. Accepting an
//!   incoming request, confirming a match and cancelling are the user's.
//! * The page sees a phase, the seven emoji to compare, the other device's
//!   name and, for a contact, their user ID (untrusted text, projected like
//!   any name). It acts by flow ID, and the core refuses an action for any
//!   other flow or phase.

use crate::{contacts, views::project_name};
use matrix_sdk::{
    Client,
    encryption::verification::{
        SasState, SasVerification, VerificationRequest, VerificationRequestState,
    },
    ruma::UserId,
};
use persephone_contracts::{SasEmoji, SecurityError, VerificationPhase, VerificationView};

pub use persephone_core_protocol::MAX_FLOW_ID_BYTES;

/// What the page asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Verify this device with another of the account's devices.
    Start,
    /// Verify the contact of this direct-message room.
    StartContact(String),
    /// Verify another of the account's devices, by its ID, from this
    /// verified device.
    StartDevice(String),
    Accept(String),
    Confirm(String),
    Cancel(String),
}

pub struct Flow {
    request: VerificationRequest,
    sas: Option<SasVerification>,
    /// The contact's identity was asked for again after the match.
    refreshed: bool,
}

impl Flow {
    pub fn flow_id(&self) -> &str {
        self.request.flow_id()
    }

    /// Whether this flow is over, so a new one may replace it.
    pub fn is_over(&self) -> bool {
        matches!(
            self.view().phase,
            VerificationPhase::Done | VerificationPhase::Cancelled
        )
    }

    /// Advances the steps that need no decision.
    pub async fn drive(&mut self) {
        match self.request.state() {
            VerificationRequestState::Ready { .. }
                if self.sas.is_none() && self.request.we_started() =>
            {
                if let Ok(Some(sas)) = self.request.start_sas().await {
                    self.sas = Some(sas);
                }
            }
            // Follow the comparison the request runs now, not the first one
            // seen: when both devices start at once, the SDK keeps the start
            // of the lexicographically smaller user or device and replaces
            // ours with it.
            VerificationRequestState::Transitioned { verification } => {
                if let Some(sas) = verification.sas() {
                    self.sas = Some(sas);
                }
            }
            _ => {}
        }
        // `Started` is a comparison the other device proposed that nothing
        // has accepted yet; a start of ours is `Created`. The SDK's
        // `we_started` names who sent the request, not who started the
        // comparison, so it cannot tell these apart.
        if let Some(sas) = &self.sas
            && matches!(sas.state(), SasState::Started { .. })
        {
            let _ = sas.accept().await;
        }
        // The other device chose a comparison without emoji (numbers only),
        // which this page cannot show: end it rather than leave both waiting.
        if let Some(sas) = &self.sas
            && matches!(sas.state(), SasState::KeysExchanged { .. })
            && emoji_of(sas).len() != SAS_EMOJI
        {
            let _ = sas.cancel().await;
        }
    }

    /// Once a contact verification is done, asks the homeserver once for
    /// the contact's identity. The match signed it with the account's
    /// user-signing key. In live runs on Merovingian 0.12.29, a contact
    /// verified right after their identity changed still read as changed
    /// until the next start, which queries keys again; most likely nothing
    /// started an earlier keys query (`docs/evidence/identity-changes.md`).
    pub async fn refresh_contact(&mut self, client: &Client) {
        if self.refreshed || !self.is_contact() || self.view().phase != VerificationPhase::Done {
            return;
        }
        self.refreshed = true;
        let _ = client
            .encryption()
            .request_user_identity(self.request.other_user_id())
            .await;
    }

    /// Whether this flow verifies a contact rather than this device.
    pub fn is_contact(&self) -> bool {
        !self.request.is_self_verification()
    }

    /// Carries out an action meant for this flow. Fails with `Failed` when it
    /// does not apply here.
    pub async fn act(&mut self, client: &Client, action: &Action) -> Result<(), SecurityError> {
        let phase = self.view().phase;
        let done = match action {
            Action::Accept(id) if id == self.flow_id() && phase == VerificationPhase::Incoming => {
                if self.is_contact() && !can_sign_contacts(client).await {
                    return Err(SecurityError::CannotSignContacts);
                }
                self.request.accept().await.is_ok()
            }
            Action::Confirm(id) if id == self.flow_id() && phase == VerificationPhase::Compare => {
                match &self.sas {
                    Some(sas) => sas.confirm().await.is_ok(),
                    None => false,
                }
            }
            Action::Cancel(id) if id == self.flow_id() && !self.is_over() => {
                match &self.sas {
                    // A mismatch tells the other device the emoji differed.
                    Some(sas) if phase == VerificationPhase::Compare => {
                        sas.mismatch().await.is_ok()
                    }
                    Some(sas) => sas.cancel().await.is_ok(),
                    None => self.request.cancel().await.is_ok(),
                }
            }
            _ => false,
        };
        if done {
            Ok(())
        } else {
            Err(SecurityError::Failed)
        }
    }

    pub fn view(&self) -> VerificationView {
        let (phase, emoji, other) = match &self.sas {
            Some(sas) => {
                let mut emoji = Vec::new();
                let phase = match sas.state() {
                    // Only seven emoji can be compared; without them nothing
                    // may be confirmed (`drive` cancels such a flow).
                    SasState::KeysExchanged { .. } => {
                        emoji = emoji_of(sas);
                        if emoji.len() == SAS_EMOJI {
                            VerificationPhase::Compare
                        } else {
                            emoji.clear();
                            VerificationPhase::Waiting
                        }
                    }
                    SasState::Confirmed => VerificationPhase::Confirmed,
                    SasState::Done { .. } => VerificationPhase::Done,
                    SasState::Cancelled(_) => VerificationPhase::Cancelled,
                    _ => VerificationPhase::Waiting,
                };
                (
                    phase,
                    emoji,
                    sas.other_device().display_name().map(str::to_owned),
                )
            }
            None => match self.request.state() {
                VerificationRequestState::Requested {
                    other_device_data, ..
                } => (
                    VerificationPhase::Incoming,
                    Vec::new(),
                    other_device_data.display_name().map(str::to_owned),
                ),
                VerificationRequestState::Done => (VerificationPhase::Done, Vec::new(), None),
                VerificationRequestState::Cancelled(_) => {
                    (VerificationPhase::Cancelled, Vec::new(), None)
                }
                _ => (VerificationPhase::Waiting, Vec::new(), None),
            },
        };
        VerificationView {
            flow_id: self.flow_id().to_owned(),
            phase,
            emoji,
            other_device: other.as_deref().map(project_name),
            contact: self
                .is_contact()
                .then(|| contacts::shown(self.request.other_user_id())),
        }
    }
}

/// Whether this device holds the account's user-signing key, which a match
/// with a contact signs their identity with.
pub async fn can_sign_contacts(client: &Client) -> bool {
    client
        .encryption()
        .cross_signing_status()
        .await
        .is_some_and(|status| status.has_user_signing)
}

/// How many emoji a comparison shows.
pub const SAS_EMOJI: usize = 7;

/// The emoji the SDK gives for comparing, or none when the other device
/// chose numbers only.
fn emoji_of(sas: &SasVerification) -> Vec<SasEmoji> {
    sas.emoji()
        .map(|emoji| {
            emoji
                .iter()
                .map(|item| SasEmoji {
                    symbol: item.symbol.to_owned(),
                    description: item.description.to_owned(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Asks the account's other devices to verify this one.
pub async fn start(client: &Client, user_id: &UserId) -> Option<Flow> {
    let identity = client
        .encryption()
        .get_user_identity(user_id)
        .await
        .ok()??;
    let request = identity.request_verification().await.ok()?;
    Some(Flow {
        request,
        sas: None,
        refreshed: false,
    })
}

/// Whether this device holds the account's self-signing key, which a match
/// with another of the account's devices signs that device with.
pub async fn can_sign_devices(client: &Client) -> bool {
    client
        .encryption()
        .cross_signing_status()
        .await
        .is_some_and(|status| status.has_self_signing)
}

/// A device ID the page sent: within the limit, printable ASCII. Device IDs
/// are opaque, so nothing more is checked; the homeserver's list decides.
pub fn valid_device_id(device_id: &str) -> bool {
    !device_id.is_empty()
        && device_id.len() <= persephone_core_protocol::MAX_DEVICE_ID_BYTES
        && device_id.bytes().all(|byte| byte.is_ascii_graphic())
}

/// Asks one other device of this account to verify each other. The request
/// goes to that device only. Refused for this device itself, for a device
/// the store does not know, and when this device cannot sign it.
pub async fn start_device(
    client: &Client,
    user_id: &UserId,
    device_id: &str,
) -> Result<Flow, SecurityError> {
    if client
        .device_id()
        .is_some_and(|own| own.as_str() == device_id)
    {
        return Err(SecurityError::DeviceNotFound);
    }
    if !can_sign_devices(client).await {
        return Err(SecurityError::CannotSignDevices);
    }
    let encryption = client.encryption();
    let device_id: matrix_sdk::ruma::OwnedDeviceId = device_id.into();
    let device = encryption
        .get_device(user_id, &device_id)
        .await
        .map_err(|_| SecurityError::Failed)?
        .ok_or(SecurityError::DeviceNotFound)?;
    let request = device
        .request_verification()
        .await
        .map_err(|_| SecurityError::Failed)?;
    Ok(Flow {
        request,
        sas: None,
        refreshed: false,
    })
}

/// Asks the contact of this direct-message room to verify each other, with a
/// message in an existing direct-message room with them.
pub async fn start_contact(client: &Client, room_id: &str) -> Result<Flow, SecurityError> {
    if !can_sign_contacts(client).await {
        return Err(SecurityError::CannotSignContacts);
    }
    let contact = contacts::verifiable_contact(client, room_id).await?;
    let encryption = client.encryption();
    // The store may not hold the contact's keys yet (`contacts.rs`): ask the
    // homeserver before saying they have no identity.
    let identity = match encryption
        .get_user_identity(&contact)
        .await
        .map_err(|_| SecurityError::Failed)?
    {
        Some(identity) => identity,
        None => encryption
            .request_user_identity(&contact)
            .await
            .map_err(|_| SecurityError::Failed)?
            .ok_or(SecurityError::ContactHasNoIdentity)?,
    };
    let request = identity
        .request_verification()
        .await
        .map_err(|_| SecurityError::Failed)?;
    Ok(Flow {
        request,
        sas: None,
        refreshed: false,
    })
}

/// An incoming request: from another of the account's devices, or from a
/// contact (`sender`).
pub async fn incoming(client: &Client, sender: &UserId, flow_id: &str) -> Option<Flow> {
    let request = client
        .encryption()
        .get_verification_request(sender, flow_id)
        .await?;
    Some(Flow {
        request,
        sas: None,
        refreshed: false,
    })
}

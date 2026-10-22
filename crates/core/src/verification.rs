//! Verifying this device with another device of the same account, by
//! comparing emoji (SAS). The security thread (`session.rs`) owns at most one
//! flow and advances it between other steps, one lease per step.
//!
//! * Only the account's own devices: an incoming request from anyone else is
//!   ignored, and this device only ever asks its own identity.
//! * The flow advances on its own where the protocol needs no decision: it
//!   starts the comparison once the other device is ready (when this device
//!   asked) and accepts a comparison the other device starts. Accepting an
//!   incoming request, confirming a match and cancelling are the user's.
//! * The page sees a phase, the seven emoji to compare and the other device's
//!   name (untrusted text, projected like any name). It acts by flow ID, and
//!   the core refuses an action for any other flow or phase.

use crate::views::project_name;
use matrix_sdk::{
    Client,
    encryption::verification::{
        SasState, SasVerification, VerificationRequest, VerificationRequestState,
    },
    ruma::UserId,
};
use persephone_contracts::{SasEmoji, VerificationPhase, VerificationView};

pub use persephone_core_protocol::MAX_FLOW_ID_BYTES;

/// What the page asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Start,
    Accept(String),
    Confirm(String),
    Cancel(String),
}

pub struct Flow {
    request: VerificationRequest,
    sas: Option<SasVerification>,
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
        if self.sas.is_none() {
            match self.request.state() {
                VerificationRequestState::Ready { .. } if self.request.we_started() => {
                    if let Ok(Some(sas)) = self.request.start_sas().await {
                        self.sas = Some(sas);
                    }
                }
                VerificationRequestState::Transitioned { verification } => {
                    self.sas = verification.sas();
                }
                _ => {}
            }
        }
        if let Some(sas) = &self.sas
            && !sas.we_started()
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

    /// Carries out an action meant for this flow. Returns `false` when it
    /// does not apply here.
    pub async fn act(&mut self, action: &Action) -> bool {
        let phase = self.view().phase;
        match action {
            Action::Accept(id) if id == self.flow_id() && phase == VerificationPhase::Incoming => {
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
        }
    }
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
    Some(Flow { request, sas: None })
}

/// An incoming request from another of the account's devices.
pub async fn incoming(client: &Client, user_id: &UserId, flow_id: &str) -> Option<Flow> {
    let request = client
        .encryption()
        .get_verification_request(user_id, flow_id)
        .await?;
    Some(Flow { request, sas: None })
}

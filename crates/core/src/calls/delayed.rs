//! The delayed leave of a call membership (MSC4140, ADR-0043).
//!
//! The core schedules a delayed state event that writes the leave content
//! after [`DELAYED_LEAVE`], and restarts it every [`DELAYED_LEAVE_RESTART`]
//! while the call runs. If the core crashes or loses the network, the
//! homeserver ends the membership by itself. A homeserver without delayed
//! events cannot run a call: the leave would be missing.

use super::membership::{DELAYED_LEAVE, EVENT_TYPE, leave_content};
use matrix_sdk::ruma::{
    api::client::delayed_events::{
        DelayParameters, delayed_state_event,
        update_delayed_event::{self, UpdateAction},
    },
    events::{AnyStateEventContent, StateEventType},
    serde::Raw,
};

/// Restart failures in a row after which the call ends. The membership may
/// have been removed by then.
pub const MAX_RESTART_FAILURES: u8 = 2;

/// Longest delay ID accepted from a homeserver, in bytes.
const MAX_DELAY_ID_BYTES: usize = 256;

/// Why a delayed-event request failed. There is no server text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelayedError {
    /// The homeserver refused or does not know the request.
    Refused,
    /// The request could not be made or finished.
    Network,
    /// The homeserver answered with an unusable delay ID.
    BadId,
}

fn classify(error: &matrix_sdk::HttpError) -> DelayedError {
    if error.as_client_api_error().is_some() {
        DelayedError::Refused
    } else {
        DelayedError::Network
    }
}

/// A scheduled leave at the homeserver.
#[derive(Clone, PartialEq, Eq)]
pub struct DelayedLeave {
    delay_id: String,
}

impl std::fmt::Debug for DelayedLeave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelayedLeave").finish_non_exhaustive()
    }
}

impl DelayedLeave {
    /// Schedules the leave of `state_key` in `room_id`.
    pub async fn schedule(
        client: &matrix_sdk::Client,
        room_id: &matrix_sdk::ruma::RoomId,
        state_key: &str,
    ) -> Result<Self, DelayedError> {
        let body = Raw::<AnyStateEventContent>::from_json(
            serde_json::value::to_raw_value(&leave_content()).map_err(|_| DelayedError::Refused)?,
        );
        let request = delayed_state_event::unstable::Request::new_raw(
            room_id.to_owned(),
            state_key.to_owned(),
            StateEventType::from(EVENT_TYPE),
            DelayParameters::Timeout {
                timeout: DELAYED_LEAVE,
            },
            body,
        );
        let response = client.send(request).await.map_err(|e| classify(&e))?;
        let id = response.delay_id;
        if id.is_empty() || id.len() > MAX_DELAY_ID_BYTES {
            return Err(DelayedError::BadId);
        }
        Ok(Self { delay_id: id })
    }

    async fn act(
        &self,
        client: &matrix_sdk::Client,
        action: UpdateAction,
    ) -> Result<(), DelayedError> {
        let request =
            update_delayed_event::unstable_v1::Request::new(self.delay_id.clone(), action);
        client
            .send(request)
            .await
            .map(|_| ())
            .map_err(|e| classify(&e))
    }

    /// Pushes the leave back by [`DELAYED_LEAVE`].
    pub async fn restart(&self, client: &matrix_sdk::Client) -> Result<(), DelayedError> {
        self.act(client, UpdateAction::Restart).await
    }

    /// Writes the leave now.
    pub async fn send_now(&self, client: &matrix_sdk::Client) -> Result<(), DelayedError> {
        self.act(client, UpdateAction::Send).await
    }

    /// Deletes the scheduled leave; it never fires.
    pub async fn cancel(&self, client: &matrix_sdk::Client) -> Result<(), DelayedError> {
        self.act(client, UpdateAction::Cancel).await
    }
}

/// Counts restart results. Two failures in a row ends the call.
#[derive(Debug, Default)]
pub struct RestartHealth {
    failures: u8,
}

impl RestartHealth {
    /// Records one restart. Returns whether the call must end now.
    pub fn record(&mut self, ok: bool) -> bool {
        if ok {
            self.failures = 0;
            false
        } else {
            self.failures = self.failures.saturating_add(1);
            self.failures >= MAX_RESTART_FAILURES
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN a restart that failed once WHEN the next one works THEN the
    /// count starts again and the call goes on.
    #[test]
    fn given_one_failure_when_success_follows_then_the_call_goes_on() {
        let mut health = RestartHealth::default();
        assert!(!health.record(false));
        assert!(!health.record(true));
        assert!(!health.record(false));
    }

    /// GIVEN two failures in a row WHEN the second is recorded THEN the call
    /// must end, and it stays ended.
    #[test]
    fn given_two_failures_in_a_row_then_the_call_ends() {
        let mut health = RestartHealth::default();
        assert!(!health.record(false));
        assert!(health.record(false));
        assert!(health.record(false));
    }
}

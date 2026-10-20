//! Explicit, sequential sync engines for disposable integration tests only.
//! Successful calls are awaited; failure/cancellation forbids a mode change.
//! This is not an application coordinator or SDK-wide shutdown/drain proof.
use std::{cell::Cell, time::Duration};

use anyhow::{Result, ensure};
use matrix_sdk::{
    Client,
    config::SyncSettings,
    ruma::{
        RoomId, api::client::sync::sync_events::v5::request::RoomSubscription,
        events::StateEventType,
    },
    sliding_sync::{SlidingSync, SlidingSyncList, SlidingSyncMode, Version},
};
use serde::Serialize;

use crate::{SyncMode, discover};

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioMode {
    ClassicOnly,
    SlidingThenClassic,
}

#[derive(Default)]
struct Progress {
    in_flight: Cell<bool>,
    failed: Cell<bool>,
    native_rounds: Cell<u32>,
    classic_rounds: Cell<u32>,
}

impl Progress {
    fn begin(&self) -> Result<()> {
        ensure!(
            !self.failed.get(),
            "previous sync failed; fixture cannot continue"
        );
        ensure!(!self.in_flight.replace(true), "sync is already in flight");
        Ok(())
    }

    fn finish(&self, native: bool, success: bool) {
        if success {
            let count = if native {
                &self.native_rounds
            } else {
                &self.classic_rounds
            };
            count.set(count.get() + 1);
        } else {
            self.failed.set(true);
        }
        self.in_flight.set(false);
    }

    fn require_completed_native(&self) -> Result<()> {
        ensure!(
            !self.in_flight.get(),
            "sync is in flight or was cancelled; no transition"
        );
        ensure!(!self.failed.get(), "sync failed; no transition");
        ensure!(
            self.native_rounds.get() > 0,
            "native sync has not completed"
        );
        Ok(())
    }
}

fn required_state() -> Vec<(StateEventType, String)> {
    vec![
        (StateEventType::RoomEncryption, String::new()),
        (StateEventType::RoomMember, "*".into()),
        (StateEventType::RoomCreate, String::new()),
        (StateEventType::RoomHistoryVisibility, String::new()),
        (StateEventType::RoomPowerLevels, String::new()),
    ]
}

async fn native(client: &Client, id: &str) -> Result<SlidingSync> {
    ensure!(
        discover(client).await.map_err(matrix_sdk::Error::from)? == SyncMode::Sliding,
        "native Sliding Sync must be advertised; this test never falls back on error or absence"
    );
    Ok(client
        .sliding_sync(id)?
        .version(Version::Native)
        .add_list(
            SlidingSyncList::builder("rooms")
                .sync_mode(SlidingSyncMode::new_selective().add_range(0..=9))
                .required_state(required_state())
                .timeline_limit(20),
        )
        .with_all_extensions()
        .without_typing_extension()
        .without_receipt_extension()
        .poll_timeout(Duration::from_millis(100))
        .network_timeout(Duration::from_secs(3))
        .build()
        .await?)
}

pub struct SyncPair<'a> {
    alice: &'a Client,
    bob: &'a Client,
    native: Option<(SlidingSync, SlidingSync)>,
    progress: Progress,
}

impl<'a> SyncPair<'a> {
    /// Select only after successful discovery. A discovery error returns before
    /// either engine is constructed; sync failures never renegotiate this choice.
    pub async fn preferred(alice: &'a Client, bob: &'a Client) -> Result<Self> {
        let alice_mode = discover(alice).await.map_err(matrix_sdk::Error::from)?;
        let bob_mode = discover(bob).await.map_err(matrix_sdk::Error::from)?;
        ensure!(
            alice_mode == bob_mode,
            "fixture clients disagree on capability"
        );
        let scenario = match alice_mode {
            SyncMode::Sliding => ScenarioMode::SlidingThenClassic,
            SyncMode::Classic => ScenarioMode::ClassicOnly,
        };
        Self::new(alice, bob, scenario).await
    }

    pub fn selected_mode(&self) -> SyncMode {
        if self.native.is_some() {
            SyncMode::Sliding
        } else {
            SyncMode::Classic
        }
    }

    pub async fn new(alice: &'a Client, bob: &'a Client, mode: ScenarioMode) -> Result<Self> {
        let native = if mode == ScenarioMode::SlidingThenClassic {
            Some((
                native(alice, "verified-alice").await?,
                native(bob, "verified-bob").await?,
            ))
        } else {
            None
        };
        Ok(Self {
            alice,
            bob,
            native,
            progress: Progress::default(),
        })
    }

    pub fn subscribe(&self, room_id: &RoomId) {
        if let Some((alice, bob)) = &self.native {
            let mut settings = RoomSubscription::default();
            settings.required_state = required_state();
            settings.timeline_limit = 20u32.into();
            for engine in [alice, bob] {
                engine.add_room_subscriptions(&[room_id], Some(settings.clone()), false);
            }
        }
    }

    pub async fn cycle(&self) -> Result<()> {
        self.progress.begin()?;
        // If this future is cancelled, in_flight stays true. The caller must
        // fail the fixture rather than treating cancellation as a drained SDK.
        let result: Result<()> = async {
            if let Some((alice, bob)) = &self.native {
                alice.sync_once().await?;
                bob.sync_once().await?;
            } else {
                for client in [self.alice, self.bob] {
                    client
                        .sync_once(SyncSettings::default().timeout(Duration::from_millis(100)))
                        .await?;
                }
            }
            Ok(())
        }
        .await;
        self.progress.finish(self.native.is_some(), result.is_ok());
        result
    }

    /// Only a successful, sequential completed-operation transition. Not fallback
    /// negotiation, an in-flight cancellation test, or general SDK drain.
    pub fn transition_to_classic(&mut self) -> Result<()> {
        self.progress.require_completed_native()?;
        ensure!(self.native.is_some(), "native engine is not active");
        drop(self.native.take());
        Ok(())
    }

    pub fn completed_rounds(&self) -> (u32, u32) {
        (
            self.progress.native_rounds.get(),
            self.progress.classic_rounds.get(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_in_flight_or_cancelled_sync_then_transition_is_refused() {
        let progress = Progress::default();
        progress.begin().unwrap();
        // Also models an outer caller dropping a cycle before finish.
        assert!(progress.require_completed_native().is_err());
        assert!(progress.begin().is_err());
    }

    #[test]
    fn given_failed_native_sync_then_transition_and_replacement_are_refused() {
        let progress = Progress::default();
        progress.begin().unwrap();
        progress.finish(true, false);
        assert!(progress.require_completed_native().is_err());
        assert!(progress.begin().is_err());
    }

    #[test]
    fn given_no_completed_native_call_then_only_successful_native_completion_allows_transition() {
        let progress = Progress::default();
        assert!(progress.require_completed_native().is_err());
        progress.begin().unwrap();
        progress.finish(false, true);
        assert!(progress.require_completed_native().is_err());
        progress.begin().unwrap();
        progress.finish(true, true);
        assert!(progress.require_completed_native().is_ok());
    }
}

//! The call as the page sees it (ADR-0043).
//!
//! Every answer here is a code, a flag, a count or a validated Matrix ID.
//! No key, token, URL or peer free text crosses to the page. The mappings
//! are closed: a new core-side cause needs a new page-side code.

use super::MAX_CALL_MEMBERS;
use super::controller::{CallError, CallState, EndReason, now_ms};
use super::membership::{EVENT_TYPE, participant_identity, read_members};
use super::readiness::sfu_transport;
use super::worker::StartError;
use matrix_sdk::{
    Client, RoomState,
    ruma::{RoomId, UserId, events::StateEventType, events::room::power_levels::RoomPowerLevels},
};
use persephone_call_protocol::FailureCode;
use persephone_contracts::{
    CALL_PARTICIPANTS_MAX, CALL_VIDEO_TILES_MAX, CallEnd, CallJoinError, CallParticipant,
    CallPhase, CallReadiness, CallStatus, CallUnavailable,
};

/// The page's code for the end of a call.
pub fn end_code(reason: EndReason) -> CallEnd {
    match reason {
        EndReason::Left => CallEnd::Left,
        EndReason::WorkerFailed(code) => match code {
            FailureCode::Containment => CallEnd::Containment,
            FailureCode::SignallingRefused | FailureCode::Tls | FailureCode::Network => {
                CallEnd::ConnectionFailed
            }
            FailureCode::AudioDevice => CallEnd::AudioDevice,
            FailureCode::DeviceNotFound => CallEnd::DeviceNotFound,
            FailureCode::Decryption | FailureCode::Protocol | FailureCode::Internal => {
                CallEnd::Internal
            }
        },
        EndReason::WorkerStopped => CallEnd::WorkerStopped,
        EndReason::WorkerCrashed => CallEnd::WorkerCrashed,
        EndReason::MembershipLost => CallEnd::MembershipLost,
        EndReason::KeysUnavailable => CallEnd::KeysUnavailable,
        EndReason::SessionEnded => CallEnd::SessionEnded,
        EndReason::Internal => CallEnd::Internal,
    }
}

/// The page's code for a refused or failed join. `None` for an error that is
/// not a refusal: a cancelled join, an ended session, or no call to leave.
pub fn join_error(error: CallError) -> Option<CallJoinError> {
    Some(match error {
        CallError::AlreadyInCall => CallJoinError::AlreadyInCall,
        CallError::NotJoined => CallJoinError::NotJoined,
        CallError::NotEncrypted => CallJoinError::NotEncrypted,
        CallError::NoSfu => CallJoinError::NoCallServer,
        CallError::Declined => CallJoinError::Declined,
        CallError::ProxyUnreadable => CallJoinError::ProxyUnreadable,
        CallError::DeviceSettingUnreadable => CallJoinError::DeviceSettingUnreadable,
        CallError::Worker(StartError::Unsupported) => CallJoinError::UnsupportedPlatform,
        CallError::Worker(StartError::Containment(_) | StartError::NotReady) => {
            CallJoinError::Containment
        }
        CallError::Worker(StartError::NoExe | StartError::Launch) => CallJoinError::NoWorker,
        CallError::NoDelayedEvents => CallJoinError::NoDelayedEvents,
        CallError::Membership => CallJoinError::Membership,
        CallError::OpenId | CallError::Sfu(_) => CallJoinError::CallServer,
        CallError::Keys => CallJoinError::Keys,
        CallError::NotInCall | CallError::Busy | CallError::Cancelled | CallError::SessionEnded => {
            return None;
        }
    })
}

/// The call state as the page's reply, with no join error.
pub fn status(state: &CallState) -> CallStatus {
    match state {
        CallState::Idle => CallStatus::idle(),
        CallState::Joining { room_id } => CallStatus {
            phase: CallPhase::Joining,
            room_id: Some(room_id.to_string()),
            ..CallStatus::idle()
        },
        CallState::Active {
            room_id,
            muted,
            deafened,
            volume,
            participants,
            camera,
            screen,
            video,
            ring_declined,
        } => CallStatus {
            phase: CallPhase::Active,
            room_id: Some(room_id.to_string()),
            muted: *muted,
            deafened: *deafened,
            volume: *volume,
            participants: participants
                .iter()
                .take(CALL_PARTICIPANTS_MAX)
                .map(|p| CallParticipant {
                    user_id: p.user_id.to_string(),
                    device_id: p.device_id.to_string(),
                    speaking: p.speaking,
                    reachable: p.reachable,
                    keyed: p.keyed,
                    sharing: p.sharing,
                })
                .collect(),
            ended: None,
            join_error: None,
            camera: *camera,
            screen: *screen,
            video: video.iter().take(CALL_VIDEO_TILES_MAX).cloned().collect(),
            ring_declined: *ring_declined,
        },
        CallState::Ended { room_id, reason } => CallStatus {
            phase: CallPhase::Ended,
            room_id: Some(room_id.to_string()),
            ended: Some(end_code(*reason)),
            ..CallStatus::idle()
        },
    }
}

/// The reply to a join that did not start a call. A refusal replies idle,
/// or the existing call's state for `already_in_call`, with the code set. A
/// cancelled join or an ended session replies the state, which says so.
pub fn join_reply(error: CallError, state: &CallState) -> CallStatus {
    match (join_error(error), error) {
        (Some(code), CallError::AlreadyInCall) => CallStatus {
            join_error: Some(code),
            ..status(state)
        },
        (Some(code), _) => CallStatus {
            join_error: Some(code),
            ..CallStatus::idle()
        },
        (None, _) => status(state),
    }
}

/// What the room says about a call, from the local store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomFacts {
    NotJoined,
    NotEncrypted,
    /// The room is encrypted, but this user may not send the call member
    /// state event.
    NotPermitted,
    Encrypted,
}

/// What the homeserver said about a call server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerFacts {
    Offered,
    None,
    Unreachable,
}

/// Whether `user` may send the call member state event under `levels`.
///
/// A room without a power levels event uses the Matrix defaults, which the
/// SDK builds: state events need 50, and only the creator has more than 0.
pub fn can_send_member_state(levels: &RoomPowerLevels, user: &UserId) -> bool {
    levels.user_can_send_state(user, StateEventType::from(EVENT_TYPE))
}

/// The reason a call cannot start from what is known without the network.
/// `None` means the homeserver must be asked.
pub fn local_reason(supported: bool, room: RoomFacts) -> Option<CallUnavailable> {
    if !supported {
        return Some(CallUnavailable::UnsupportedPlatform);
    }
    match room {
        RoomFacts::NotJoined => Some(CallUnavailable::NotJoined),
        RoomFacts::NotEncrypted => Some(CallUnavailable::NotEncrypted),
        RoomFacts::NotPermitted => Some(CallUnavailable::NotPermitted),
        RoomFacts::Encrypted => None,
    }
}

/// The reason a call cannot start from the homeserver's answer.
pub fn server_reason(server: ServerFacts) -> Option<CallUnavailable> {
    match server {
        ServerFacts::Offered => None,
        ServerFacts::None => Some(CallUnavailable::NoCallServer),
        ServerFacts::Unreachable => Some(CallUnavailable::ServerUnreachable),
    }
}

/// The reply of `call_readiness` for `room_id`. It asks the homeserver for
/// its call server only when nothing local rules the call out. The member
/// count comes from the local store and is reported either way.
pub async fn readiness(client: &Client, room_id: &RoomId, supported: bool) -> CallReadiness {
    let room = client
        .get_room(room_id)
        .filter(|room| room.state() == RoomState::Joined);
    let (facts, active) = match &room {
        None => (RoomFacts::NotJoined, 0),
        Some(room) => {
            let facts = match room.latest_encryption_state().await {
                Ok(state) if state.is_encrypted() => {
                    let levels = room.power_levels_or_default().await;
                    match client.user_id() {
                        Some(user) if !can_send_member_state(&levels, user) => {
                            RoomFacts::NotPermitted
                        }
                        _ => RoomFacts::Encrypted,
                    }
                }
                _ => RoomFacts::NotEncrypted,
            };
            let own = match (client.user_id(), client.device_id()) {
                (Some(user), Some(device)) => Some(participant_identity(user, device)),
                _ => None,
            };
            let active = read_members(room, now_ms())
                .await
                .map(|members| {
                    members
                        .iter()
                        .filter(|member| Some(&member.identity) != own.as_ref())
                        .count()
                })
                .unwrap_or(0);
            (facts, active)
        }
    };
    let reason = match local_reason(supported, facts) {
        Some(reason) => Some(reason),
        None => server_reason(match sfu_transport(client).await {
            Ok(Some(_)) => ServerFacts::Offered,
            Ok(None) => ServerFacts::None,
            Err(_) => ServerFacts::Unreachable,
        }),
    };
    CallReadiness {
        available: reason.is_none(),
        reason,
        active_members: u8::try_from(active.min(MAX_CALL_MEMBERS)).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::controller::Participant;
    use crate::calls::sfu::{Refusal, SfuError};
    use matrix_sdk::ruma::{
        RoomVersionId,
        events::room::power_levels::{RoomPowerLevelsEventContent, RoomPowerLevelsSource},
        int, user_id,
    };
    use matrix_sdk::ruma::{owned_device_id, owned_user_id, room_id};
    use persephone_contracts::{CallCamera, CallScreen, CallVideoTile};

    /// Power levels for a room of version 10 with `edit` applied to the default content.
    fn levels(edit: impl FnOnce(&mut RoomPowerLevelsEventContent)) -> RoomPowerLevels {
        let rules = RoomVersionId::V10.rules().unwrap().authorization;
        let mut content = RoomPowerLevelsEventContent::new(&rules);
        edit(&mut content);
        RoomPowerLevels::new(
            RoomPowerLevelsSource::Original(content),
            &rules,
            [user_id!("@creator:example.org").to_owned()],
        )
    }

    /// GIVEN default levels, a raised state default or per-event overrides
    /// WHEN the member state permission is checked THEN the room's rule for
    /// the call member event type decides, and other event types do not.
    #[test]
    fn given_power_levels_then_member_state_permission_follows_the_room() {
        let alice = user_id!("@alice:example.org");
        let admin = user_id!("@admin:example.org");
        let call_type = || StateEventType::from(EVENT_TYPE);

        // Default content: state_default 50, a plain member has 0.
        let plain = levels(|_| {});
        assert!(!can_send_member_state(&plain, alice));

        // The user holds 50, the state default.
        let moderator = levels(|c| {
            c.users.insert(alice.to_owned(), int!(50));
        });
        assert!(can_send_member_state(&moderator, alice));

        // A raised state default shuts a moderator out.
        let strict = levels(|c| {
            c.state_default = int!(75);
            c.users.insert(alice.to_owned(), int!(50));
            c.users.insert(admin.to_owned(), int!(100));
        });
        assert!(!can_send_member_state(&strict, alice));
        assert!(can_send_member_state(&strict, admin));

        // An override for the call member type opens it to everyone.
        let open = levels(|c| {
            c.events.insert(call_type().into(), int!(0));
        });
        assert!(can_send_member_state(&open, alice));

        // An override for the call member type closes it, though the state
        // default would allow it.
        let closed = levels(|c| {
            c.state_default = int!(0);
            c.events.insert(call_type().into(), int!(100));
            c.users.insert(alice.to_owned(), int!(50));
            c.users.insert(admin.to_owned(), int!(100));
        });
        assert!(!can_send_member_state(&closed, alice));
        assert!(can_send_member_state(&closed, admin));

        // An override for another type changes nothing.
        let other = levels(|c| {
            c.events.insert(StateEventType::RoomName.into(), int!(0));
        });
        assert!(!can_send_member_state(&other, alice));
    }

    /// GIVEN a room without a power levels event WHEN the permission is
    /// checked THEN the Matrix defaults apply: the creator may send state
    /// events and others may not.
    #[test]
    fn given_no_power_levels_event_then_only_the_creator_may_send() {
        let rules = RoomVersionId::V10.rules().unwrap().authorization;
        let creator = user_id!("@creator:example.org");
        let none = RoomPowerLevels::new(RoomPowerLevelsSource::None, &rules, [creator.to_owned()]);
        assert!(can_send_member_state(&none, creator));
        assert!(!can_send_member_state(
            &none,
            user_id!("@alice:example.org")
        ));
    }

    /// GIVEN a room where the user may not send the member event WHEN the
    /// local reason is chosen THEN it is not_permitted, after the platform,
    /// join and encryption checks.
    #[test]
    fn given_not_permitted_then_the_reason_is_not_permitted() {
        assert_eq!(
            local_reason(true, RoomFacts::NotPermitted),
            Some(CallUnavailable::NotPermitted)
        );
        assert_eq!(
            local_reason(false, RoomFacts::NotPermitted),
            Some(CallUnavailable::UnsupportedPlatform)
        );
    }

    /// GIVEN a worker crash WHEN mapped THEN it has its own end code, and a
    /// plain stop keeps its code.
    #[test]
    fn given_worker_crash_then_its_own_end_code() {
        assert_eq!(end_code(EndReason::WorkerCrashed), CallEnd::WorkerCrashed);
        assert_eq!(end_code(EndReason::WorkerStopped), CallEnd::WorkerStopped);
    }

    /// GIVEN each worker failure code WHEN mapped THEN it becomes the page's
    /// code: containment, a connection failure, the audio device or internal.
    #[test]
    fn given_worker_failures_then_the_end_codes_follow_the_contract() {
        let code = |failure| end_code(EndReason::WorkerFailed(failure));
        assert_eq!(code(FailureCode::Containment), CallEnd::Containment);
        for failure in [
            FailureCode::SignallingRefused,
            FailureCode::Tls,
            FailureCode::Network,
        ] {
            assert_eq!(code(failure), CallEnd::ConnectionFailed, "{failure:?}");
        }
        assert_eq!(code(FailureCode::AudioDevice), CallEnd::AudioDevice);
        assert_eq!(code(FailureCode::DeviceNotFound), CallEnd::DeviceNotFound);
        for failure in [
            FailureCode::Decryption,
            FailureCode::Protocol,
            FailureCode::Internal,
        ] {
            assert_eq!(code(failure), CallEnd::Internal, "{failure:?}");
        }
        for (reason, expected) in [
            (EndReason::Left, CallEnd::Left),
            (EndReason::WorkerStopped, CallEnd::WorkerStopped),
            (EndReason::MembershipLost, CallEnd::MembershipLost),
            (EndReason::KeysUnavailable, CallEnd::KeysUnavailable),
            (EndReason::SessionEnded, CallEnd::SessionEnded),
            (EndReason::Internal, CallEnd::Internal),
        ] {
            assert_eq!(end_code(reason), expected);
        }
    }

    /// GIVEN each controller error WHEN mapped THEN refusals have a join
    /// code, the worker errors follow the contract, and a cancel, an ended
    /// session or no call has none.
    #[test]
    fn given_controller_errors_then_the_join_codes_follow_the_contract() {
        let sfu = CallError::Sfu(SfuError::Refused(Refusal::BadUrl));
        for (error, expected) in [
            (CallError::AlreadyInCall, CallJoinError::AlreadyInCall),
            (CallError::NotJoined, CallJoinError::NotJoined),
            (CallError::NotEncrypted, CallJoinError::NotEncrypted),
            (CallError::NoSfu, CallJoinError::NoCallServer),
            (CallError::Declined, CallJoinError::Declined),
            (CallError::ProxyUnreadable, CallJoinError::ProxyUnreadable),
            (
                CallError::DeviceSettingUnreadable,
                CallJoinError::DeviceSettingUnreadable,
            ),
            (
                CallError::Worker(StartError::Unsupported),
                CallJoinError::UnsupportedPlatform,
            ),
            (
                CallError::Worker(StartError::Containment(4)),
                CallJoinError::Containment,
            ),
            (
                CallError::Worker(StartError::NotReady),
                CallJoinError::Containment,
            ),
            (
                CallError::Worker(StartError::NoExe),
                CallJoinError::NoWorker,
            ),
            (
                CallError::Worker(StartError::Launch),
                CallJoinError::NoWorker,
            ),
            (CallError::NoDelayedEvents, CallJoinError::NoDelayedEvents),
            (CallError::Membership, CallJoinError::Membership),
            (CallError::OpenId, CallJoinError::CallServer),
            (sfu, CallJoinError::CallServer),
            (CallError::Keys, CallJoinError::Keys),
        ] {
            assert_eq!(join_error(error), Some(expected), "{error:?}");
        }
        for error in [
            CallError::NotInCall,
            CallError::Busy,
            CallError::Cancelled,
            CallError::SessionEnded,
        ] {
            assert_eq!(join_error(error), None, "{error:?}");
        }
    }

    /// GIVEN a refused join WHEN replied THEN it is an idle status with the
    /// code, except `already_in_call`, which shows the call that runs; a
    /// cancelled join shows the state.
    #[test]
    fn given_a_refused_join_then_the_reply_is_a_status_with_a_code() {
        let room = room_id!("!r:example.org").to_owned();
        let active = CallState::Active {
            room_id: room.clone(),
            muted: true,
            deafened: true,
            volume: 30,
            participants: Vec::new(),
            camera: CallCamera::Off,
            screen: CallScreen::Off,
            video: Vec::new(),
            ring_declined: false,
        };
        let refused = join_reply(CallError::Declined, &CallState::Idle);
        assert_eq!(refused.phase, CallPhase::Idle);
        assert_eq!(refused.join_error, Some(CallJoinError::Declined));
        assert_eq!(refused.room_id, None);

        let busy = join_reply(CallError::AlreadyInCall, &active);
        assert_eq!(busy.phase, CallPhase::Active);
        assert!(busy.muted);
        assert!(busy.deafened);
        assert_eq!(busy.volume, 30);
        assert_eq!(busy.join_error, Some(CallJoinError::AlreadyInCall));

        let ended = CallState::Ended {
            room_id: room.clone(),
            reason: EndReason::Left,
        };
        let cancelled = join_reply(CallError::Cancelled, &ended);
        assert_eq!(cancelled.phase, CallPhase::Ended);
        assert_eq!(cancelled.ended, Some(CallEnd::Left));
        assert_eq!(cancelled.join_error, None);
    }

    /// GIVEN each call state WHEN shown THEN it carries only the phase, the
    /// room, flags, bounded IDs and the end code.
    #[test]
    fn given_call_states_then_the_status_shows_codes_and_bounded_ids() {
        let room = room_id!("!r:example.org").to_owned();
        assert_eq!(status(&CallState::Idle), CallStatus::idle());
        let joining = status(&CallState::Joining {
            room_id: room.clone(),
        });
        assert_eq!(joining.phase, CallPhase::Joining);
        assert_eq!(joining.room_id.as_deref(), Some("!r:example.org"));

        let participant = Participant {
            user_id: owned_user_id!("@a:example.org"),
            device_id: owned_device_id!("DEV"),
            speaking: true,
            reachable: false,
            keyed: true,
            sharing: true,
        };
        let many = vec![participant; CALL_PARTICIPANTS_MAX + 5];
        let active = status(&CallState::Active {
            room_id: room.clone(),
            muted: false,
            deafened: false,
            volume: 100,
            participants: many,
            camera: CallCamera::On,
            screen: CallScreen::On,
            video: vec![
                CallVideoTile {
                    tile: 0,
                    participant: None,
                    own: true,
                    stream: 0,
                };
                CALL_VIDEO_TILES_MAX + 3
            ],
            ring_declined: true,
        });
        assert!(active.ring_declined);
        assert!(!CallStatus::idle().ring_declined);
        assert_eq!(active.camera, CallCamera::On);
        assert_eq!(active.screen, CallScreen::On);
        assert!(active.participants.iter().all(|p| p.sharing));
        assert_eq!(active.video.len(), CALL_VIDEO_TILES_MAX);
        assert_eq!(CALL_VIDEO_TILES_MAX, 6);
        assert_eq!(active.phase, CallPhase::Active);
        assert_eq!(active.participants.len(), CALL_PARTICIPANTS_MAX);
        assert_eq!(active.participants[0].user_id, "@a:example.org");
        assert_eq!(active.participants[0].device_id, "DEV");
        assert!(active.participants[0].speaking && active.participants[0].keyed);
        assert!(!active.participants[0].reachable);

        let ended = status(&CallState::Ended {
            room_id: room,
            reason: EndReason::SessionEnded,
        });
        assert_eq!(ended.phase, CallPhase::Ended);
        assert_eq!(ended.ended, Some(CallEnd::SessionEnded));
    }

    /// GIVEN each platform, room and server fact WHEN the reason is chosen
    /// THEN the first one that applies wins, in the documented order.
    #[test]
    fn given_facts_then_the_unavailable_reason_follows_the_order() {
        assert_eq!(
            local_reason(false, RoomFacts::NotJoined),
            Some(CallUnavailable::UnsupportedPlatform)
        );
        assert_eq!(
            local_reason(true, RoomFacts::NotJoined),
            Some(CallUnavailable::NotJoined)
        );
        assert_eq!(
            local_reason(true, RoomFacts::NotEncrypted),
            Some(CallUnavailable::NotEncrypted)
        );
        assert_eq!(local_reason(true, RoomFacts::Encrypted), None);
        assert_eq!(server_reason(ServerFacts::Offered), None);
        assert_eq!(
            server_reason(ServerFacts::None),
            Some(CallUnavailable::NoCallServer)
        );
        assert_eq!(
            server_reason(ServerFacts::Unreachable),
            Some(CallUnavailable::ServerUnreachable)
        );
    }
}

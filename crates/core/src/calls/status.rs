//! The call as the page sees it (ADR-0043).
//!
//! Every answer here is a code, a flag, a count or a validated Matrix ID.
//! No key, token, URL or peer free text crosses to the page. The mappings
//! are closed: a new core-side cause needs a new page-side code.

use super::MAX_CALL_MEMBERS;
use super::controller::{CallError, CallState, EndReason, now_ms};
use super::membership::{participant_identity, read_members};
use super::readiness::sfu_transport;
use super::worker::StartError;
use matrix_sdk::{Client, RoomState, ruma::RoomId};
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
        CallError::NotInCall | CallError::Cancelled | CallError::SessionEnded => return None,
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
            participants,
            camera,
            screen,
            video,
        } => CallStatus {
            phase: CallPhase::Active,
            room_id: Some(room_id.to_string()),
            muted: *muted,
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
    Encrypted,
}

/// What the homeserver said about a call server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerFacts {
    Offered,
    None,
    Unreachable,
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
                Ok(state) if state.is_encrypted() => RoomFacts::Encrypted,
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
    use matrix_sdk::ruma::{owned_device_id, owned_user_id, room_id};
    use persephone_contracts::{CallCamera, CallScreen, CallVideoTile};

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
            participants: Vec::new(),
            camera: CallCamera::Off,
            screen: CallScreen::Off,
            video: Vec::new(),
        };
        let refused = join_reply(CallError::Declined, &CallState::Idle);
        assert_eq!(refused.phase, CallPhase::Idle);
        assert_eq!(refused.join_error, Some(CallJoinError::Declined));
        assert_eq!(refused.room_id, None);

        let busy = join_reply(CallError::AlreadyInCall, &active);
        assert_eq!(busy.phase, CallPhase::Active);
        assert!(busy.muted);
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
        });
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

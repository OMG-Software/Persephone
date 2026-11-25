//! The call statistics that the page may see (ADR-0066).
//!
//! The worker's message is already bounded by the call protocol reader. This
//! module names each remote entry by the member it stands for, leaves out an
//! entry that no member stands for, and checks the result once more against
//! the limits the UI host enforces.

use persephone_call_protocol as worker;
use persephone_contracts::{
    CallStats, CallStatsAudioReceived, CallStatsAudioSent, CallStatsConnection, CallStatsLimit,
    CallStatsParticipant, CallStatsVideoReceived, CallStatsVideoSent,
};

// The page's bounds repeat the worker protocol's. The worker clamps each
// number to its protocol bound, and `CallStats::within_limits` drops a
// reading whole on a number over the page's bound, so the two must match.
const _: () = {
    assert!(persephone_contracts::CALL_STATS_KBPS_MAX == worker::MAX_STATS_KBPS);
    assert!(persephone_contracts::CALL_STATS_MS_MAX == worker::MAX_STATS_MS);
    assert!(persephone_contracts::CALL_STATS_FPS_MAX == worker::MAX_STATS_FPS);
    assert!(persephone_contracts::CALL_STATS_SIDE_MAX == worker::MAX_STATS_SIDE);
    assert!(persephone_contracts::CALL_STATS_CONNECTIONS_MAX == worker::MAX_STATS_CONNECTIONS);
    assert!(persephone_contracts::CALL_PARTICIPANTS_MAX == worker::MAX_PARTICIPANTS);
};

fn limit(limit: worker::QualityLimit) -> CallStatsLimit {
    match limit {
        worker::QualityLimit::None => CallStatsLimit::None,
        worker::QualityLimit::Cpu => CallStatsLimit::Cpu,
        worker::QualityLimit::Bandwidth => CallStatsLimit::Bandwidth,
        worker::QualityLimit::Other => CallStatsLimit::Other,
    }
}

fn audio_sent(audio: &worker::AudioSendStats) -> CallStatsAudioSent {
    CallStatsAudioSent {
        level: audio.level,
        bitrate_kbps: audio.bitrate_kbps,
        packets_sent: audio.packets_sent,
        packets_lost: audio.packets_lost,
        jitter_ms: audio.jitter_ms,
        rtt_ms: audio.rtt_ms,
    }
}

fn video_sent(video: &worker::VideoSendStats) -> CallStatsVideoSent {
    CallStatsVideoSent {
        fps: video.fps,
        width: video.width,
        height: video.height,
        bitrate_kbps: video.bitrate_kbps,
        limit: limit(video.limit),
        packets_lost: video.packets_lost,
    }
}

fn audio_received(audio: &worker::AudioReceiveStats) -> CallStatsAudioReceived {
    CallStatsAudioReceived {
        level: audio.level,
        bitrate_kbps: audio.bitrate_kbps,
        packets_received: audio.packets_received,
        packets_lost: audio.packets_lost,
        jitter_ms: audio.jitter_ms,
        concealed_percent: audio.concealed_percent,
    }
}

fn video_received(video: &worker::VideoReceiveStats) -> CallStatsVideoReceived {
    CallStatsVideoReceived {
        fps: video.fps,
        width: video.width,
        height: video.height,
        bitrate_kbps: video.bitrate_kbps,
        dropped_fps: video.dropped_fps,
        packets_lost: video.packets_lost,
    }
}

/// The page's view of `stats`. `member` gives the user ID and device ID of
/// the member that a worker participant (its connection and identity)
/// stands for, or `None` when the participant is no current member or its
/// connection does not speak for it. A device that more than one entry
/// stands for is listed once. `None` when the result is out of its limits.
pub fn view(
    stats: &worker::CallStats,
    member: impl Fn(u8, &str) -> Option<(String, String)>,
) -> Option<CallStats> {
    let mut participants: Vec<CallStatsParticipant> = Vec::new();
    for remote in &stats.remotes {
        let Some((user_id, device_id)) = member(remote.connection, &remote.identity) else {
            continue;
        };
        if participants
            .iter()
            .any(|seen| seen.user_id == user_id && seen.device_id == device_id)
        {
            continue;
        }
        participants.push(CallStatsParticipant {
            user_id,
            device_id,
            audio: remote.audio.as_ref().map(audio_received),
            video: remote.video.as_ref().map(video_received),
        });
    }
    let view = CallStats {
        microphone: stats.microphone.as_ref().map(audio_sent),
        camera: stats.camera.as_ref().map(video_sent),
        screen: stats.screen.as_ref().map(video_sent),
        participants,
        connections: stats
            .connections
            .iter()
            .map(|link| CallStatsConnection {
                connection: link.connection,
                outgoing_kbps: link.outgoing_kbps,
                send_kbps: link.send_kbps,
                receive_kbps: link.receive_kbps,
                rtt_ms: link.rtt_ms,
            })
            .collect(),
    };
    view.within_limits().then_some(view)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(connection: u8, identity: &str) -> worker::RemoteStats {
        worker::RemoteStats {
            connection,
            identity: identity.to_owned(),
            audio: Some(worker::AudioReceiveStats {
                level: 7,
                bitrate_kbps: Some(30),
                packets_received: 5,
                packets_lost: 1,
                jitter_ms: 2,
                concealed_percent: None,
            }),
            video: None,
        }
    }

    fn members(connection: u8, identity: &str) -> Option<(String, String)> {
        match (connection, identity) {
            (0, "bob-backend") | (1, "bob-other") => {
                Some(("@bob:example.org".into(), "BOB".into()))
            }
            (0, "eve-backend") => Some(("@eve:example.org".into(), "EVE".into())),
            _ => None,
        }
    }

    /// GIVEN a reading with members, strangers and a repeated device WHEN it
    /// is made for the page THEN only members are named, each device once,
    /// and the numbers carry over.
    #[test]
    fn given_a_reading_when_viewed_then_only_current_members_are_named_once() {
        let stats = worker::CallStats {
            microphone: Some(worker::AudioSendStats {
                level: 40,
                bitrate_kbps: Some(32),
                packets_sent: 9,
                packets_lost: Some(1),
                jitter_ms: Some(3),
                rtt_ms: Some(20),
            }),
            remotes: vec![
                remote(0, "bob-backend"),
                remote(0, "stranger"),
                remote(1, "bob-other"),
                remote(2, "bob-backend"),
                remote(0, "eve-backend"),
            ],
            connections: vec![worker::ConnectionStats {
                connection: 0,
                outgoing_kbps: Some(2500),
                send_kbps: None,
                receive_kbps: Some(40),
                rtt_ms: Some(18),
            }],
            ..worker::CallStats::default()
        };
        let view = view(&stats, members).expect("within limits");
        let names: Vec<_> = view
            .participants
            .iter()
            .map(|person| person.device_id.as_str())
            .collect();
        assert_eq!(names, ["BOB", "EVE"]);
        assert_eq!(view.participants[0].audio.as_ref().unwrap().level, 7);
        assert_eq!(view.microphone.as_ref().unwrap().rtt_ms, Some(20));
        assert_eq!(view.connections[0].outgoing_kbps, Some(2500));
        assert_eq!(view.connections[0].send_kbps, None);
        assert!(view.camera.is_none() && view.screen.is_none());
    }

    /// GIVEN a member with an ID that the page would refuse WHEN the reading
    /// is made THEN the whole reading is dropped.
    #[test]
    fn given_an_id_over_the_limit_when_viewed_then_the_reading_is_dropped() {
        let stats = worker::CallStats {
            remotes: vec![remote(0, "bob-backend")],
            ..worker::CallStats::default()
        };
        let long = |_: u8, _: &str| {
            Some((
                "u".repeat(persephone_contracts::CALL_USER_ID_BYTES_MAX + 1),
                "D".to_owned(),
            ))
        };
        assert!(view(&stats, long).is_none());
    }

    /// GIVEN a limit reason WHEN it is viewed THEN each worker value has its
    /// page value.
    #[test]
    fn given_each_quality_limit_when_viewed_then_it_maps_one_to_one() {
        for (from, to) in [
            (worker::QualityLimit::None, CallStatsLimit::None),
            (worker::QualityLimit::Cpu, CallStatsLimit::Cpu),
            (worker::QualityLimit::Bandwidth, CallStatsLimit::Bandwidth),
            (worker::QualityLimit::Other, CallStatsLimit::Other),
        ] {
            assert_eq!(limit(from), to);
        }
    }
}

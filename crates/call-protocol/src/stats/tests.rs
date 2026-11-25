use super::*;
use crate::{FROM_STATS, FromWorker, MAX_IDENTITY_BYTES, MAX_MESSAGE_BYTES, ToWorker, VERSION};

pub(crate) fn full_stats() -> CallStats {
    let audio = AudioReceiveStats {
        level: 100,
        bitrate_kbps: Some(MAX_STATS_KBPS),
        packets_received: u32::MAX,
        packets_lost: 7,
        jitter_ms: MAX_STATS_MS,
        concealed_percent: Some(100),
    };
    let video = VideoReceiveStats {
        fps: MAX_STATS_FPS,
        width: MAX_STATS_SIDE,
        height: 1,
        bitrate_kbps: None,
        frames_dropped: 3,
        packets_lost: u32::MAX,
    };
    let sent = VideoSendStats {
        fps: 30,
        width: 1280,
        height: 720,
        bitrate_kbps: Some(1500),
        limit: QualityLimit::Bandwidth,
        packets_lost: Some(2),
    };
    CallStats {
        microphone: Some(AudioSendStats {
            level: 42,
            bitrate_kbps: Some(32),
            packets_sent: 1000,
            packets_lost: Some(1),
            jitter_ms: Some(3),
            rtt_ms: Some(40),
        }),
        camera: Some(sent),
        screen: Some(VideoSendStats {
            limit: QualityLimit::None,
            packets_lost: None,
            ..sent
        }),
        remotes: vec![
            RemoteStats {
                connection: 0,
                identity: "@bob:example.org:D".to_owned(),
                audio: Some(audio),
                video: Some(video),
            },
            RemoteStats {
                connection: 2,
                identity: "@bob:example.org:D".to_owned(),
                audio: None,
                video: Some(video),
            },
        ],
        connections: vec![
            ConnectionStats {
                connection: 0,
                outgoing_kbps: Some(2500),
                send_kbps: Some(40),
                receive_kbps: None,
                rtt_ms: Some(20),
            },
            ConnectionStats {
                connection: 3,
                outgoing_kbps: None,
                send_kbps: None,
                receive_kbps: Some(MAX_STATS_KBPS),
                rtt_ms: None,
            },
        ],
    }
}

/// A message body with the version, the tag and `fields`.
fn body(fields: &[u8]) -> Vec<u8> {
    let mut out = vec![VERSION, FROM_STATS];
    out.extend_from_slice(fields);
    out
}

fn decode(fields: &[u8]) -> Result<FromWorker, DecodeError> {
    FromWorker::decode(&body(fields))
}

/// The fields of a message with an own microphone record and nothing else.
fn audio_send_fields(level: u8) -> Vec<u8> {
    // present: microphone; level; no bitrate; 4 bytes of packets; no loss,
    // jitter or round-trip time; then no remotes and no connections.
    vec![0b001, level, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

/// GIVEN every kind of statistic WHEN encoded and decoded THEN it is the
/// same.
#[test]
fn given_full_stats_when_round_tripped_then_unchanged() {
    for stats in [CallStats::default(), full_stats()] {
        let message = FromWorker::Stats(stats);
        assert_eq!(FromWorker::decode(&message.encode()), Ok(message));
    }
}

/// GIVEN statistics for every participant, with the longest identities WHEN
/// encoded THEN the message fits `MAX_MESSAGE_BYTES`.
#[test]
fn given_the_most_participants_with_long_identities_then_the_message_fits() {
    let mut stats = full_stats();
    let audio = stats.remotes[0].audio;
    let video = stats.remotes[0].video;
    stats.remotes = (0..MAX_PARTICIPANTS)
        .map(|index| RemoteStats {
            connection: 0,
            identity: format!("{index:0>width$}", width = MAX_IDENTITY_BYTES),
            audio,
            video,
        })
        .collect();
    let wire = FromWorker::Stats(stats.clone()).encode();
    assert!(wire.len() < MAX_MESSAGE_BYTES, "{}", wire.len());
    assert_eq!(FromWorker::decode(&wire), Ok(FromWorker::Stats(stats)));
}

/// GIVEN more entries than the limits WHEN encoded THEN the extra entries
/// are left out and the message decodes.
#[test]
fn given_too_many_entries_when_encoded_then_the_extras_are_left_out() {
    let stats = CallStats {
        remotes: (0..MAX_PARTICIPANTS + 5)
            .map(|index| RemoteStats {
                connection: 0,
                identity: format!("id{index}"),
                audio: None,
                video: None,
            })
            .collect(),
        connections: (0..8)
            .map(|connection| ConnectionStats {
                connection,
                outgoing_kbps: None,
                send_kbps: None,
                receive_kbps: None,
                rtt_ms: None,
            })
            .collect(),
        ..CallStats::default()
    };
    let Ok(FromWorker::Stats(read)) = FromWorker::decode(&FromWorker::Stats(stats).encode()) else {
        panic!("the message must decode");
    };
    assert_eq!(read.remotes.len(), MAX_PARTICIPANTS);
    assert_eq!(read.connections.len(), MAX_STATS_CONNECTIONS);
}

/// GIVEN a count of participants or connections over its limit WHEN decoded
/// THEN it is refused before any entry is read.
#[test]
fn given_counts_over_the_limits_when_decoded_then_they_are_refused() {
    assert_eq!(
        decode(&[0, (MAX_PARTICIPANTS + 1) as u8]),
        Err(DecodeError::TooLarge)
    );
    assert_eq!(
        decode(&[0, 0, (MAX_STATS_CONNECTIONS + 1) as u8]),
        Err(DecodeError::TooLarge)
    );
    assert_eq!(
        decode(&[0, 0, 0]),
        Ok(FromWorker::Stats(CallStats::default()))
    );
}

/// GIVEN a field over its bound WHEN decoded THEN it is refused, and the
/// bound itself is accepted.
#[test]
fn given_a_value_over_its_bound_when_decoded_then_it_is_refused() {
    assert_eq!(
        decode(&audio_send_fields(101)),
        Err(DecodeError::InvalidValue)
    );
    assert!(decode(&audio_send_fields(100)).is_ok());
    // A bitrate over 1 Gbit/s.
    for (kbps, ok) in [(MAX_STATS_KBPS, true), (MAX_STATS_KBPS + 1, false)] {
        let mut fields = vec![0b001, 0, 1];
        fields.extend_from_slice(&kbps.to_be_bytes());
        fields.extend_from_slice(&[0; 9]);
        assert_eq!(decode(&fields).is_ok(), ok, "{kbps}");
    }
    // A round-trip time over a minute.
    for (millis, ok) in [(MAX_STATS_MS, true), (MAX_STATS_MS + 1, false)] {
        let mut fields = vec![0b001, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        fields.extend_from_slice(&millis.to_be_bytes());
        fields.extend_from_slice(&[0, 0]);
        assert_eq!(decode(&fields).is_ok(), ok, "{millis}");
    }
    // A frame rate, a side and a limit code of a camera.
    for (fps, side, limit, ok) in [
        (MAX_STATS_FPS, MAX_STATS_SIDE, 3u8, true),
        (MAX_STATS_FPS + 1, 1u16, 0u8, false),
        (1, MAX_STATS_SIDE + 1, 0, false),
        (1, 1, 4, false),
    ] {
        let mut fields = vec![0b010, fps];
        fields.extend_from_slice(&side.to_be_bytes());
        fields.extend_from_slice(&1u16.to_be_bytes());
        fields.extend_from_slice(&[0, limit, 0, 0, 0]);
        assert_eq!(decode(&fields).is_ok(), ok, "{fps} {side} {limit}");
    }
}

/// GIVEN flag bytes that are not 0 or 1, or unknown presence bits WHEN
/// decoded THEN they are refused.
#[test]
fn given_bad_flags_when_decoded_then_they_are_refused() {
    assert_eq!(decode(&[0b1000]), Err(DecodeError::InvalidValue));
    let mut fields = audio_send_fields(1);
    fields[2] = 2;
    assert_eq!(decode(&fields), Err(DecodeError::InvalidValue));
    // A remote with unknown kind bits.
    assert_eq!(
        decode(&[0, 1, 0, 0, 1, b'a', 0b100]),
        Err(DecodeError::InvalidValue)
    );
}

/// GIVEN a participant or a connection listed twice, an unknown connection
/// number, or a bad identity WHEN decoded THEN it is refused.
#[test]
fn given_duplicate_or_bad_entries_when_decoded_then_they_are_refused() {
    let remote = [0u8, 0, 1, b'a', 0];
    let mut twice = vec![0, 2];
    twice.extend_from_slice(&remote);
    twice.extend_from_slice(&remote);
    twice.push(0);
    assert_eq!(decode(&twice), Err(DecodeError::InvalidValue));
    // The same identity on two connections is two participants.
    let mut other = remote;
    other[0] = 1;
    let mut two = vec![0, 2];
    two.extend_from_slice(&remote);
    two.extend_from_slice(&other);
    two.push(0);
    assert!(decode(&two).is_ok());
    // Connection 4 does not exist.
    assert_eq!(
        decode(&[0, 1, 4, 0, 1, b'a', 0, 0]),
        Err(DecodeError::InvalidValue)
    );
    // An empty identity and a control character.
    assert_eq!(decode(&[0, 1, 0, 0, 0, 0]), Err(DecodeError::TooShort));
    assert_eq!(
        decode(&[0, 1, 0, 0, 1, 0x07, 0, 0]),
        Err(DecodeError::InvalidCharacter)
    );
    // A connection twice.
    let connection = [0u8, 0, 0, 0, 0];
    let mut dup = vec![0, 0, 2];
    dup.extend_from_slice(&connection);
    dup.extend_from_slice(&connection);
    assert_eq!(decode(&dup), Err(DecodeError::InvalidValue));
}

/// GIVEN a message cut at every byte, with a trailing byte, or over the
/// message limit WHEN decoded THEN it is refused.
#[test]
fn given_truncated_trailing_or_oversize_stats_when_decoded_then_refused() {
    let wire = FromWorker::Stats(full_stats()).encode();
    for length in 0..wire.len() {
        assert!(FromWorker::decode(&wire[..length]).is_err(), "{length}");
    }
    let mut longer = wire.clone();
    longer.push(0);
    assert_eq!(FromWorker::decode(&longer), Err(DecodeError::TrailingBytes));
    let mut huge = wire;
    huge.resize(MAX_MESSAGE_BYTES + 1, 0);
    assert_eq!(FromWorker::decode(&huge), Err(DecodeError::TooLarge));
}

/// GIVEN the core's switch WHEN it is anything but 0 or 1 THEN it is
/// refused.
#[test]
fn given_the_stats_switch_when_decoded_then_only_a_bool_is_accepted() {
    for on in [true, false] {
        let message = ToWorker::SetStats { on };
        assert_eq!(ToWorker::decode(&message.encode()), Ok(message));
    }
    assert_eq!(
        ToWorker::decode(&[VERSION, 16, 2]),
        Err(DecodeError::InvalidValue)
    );
    assert_eq!(
        ToWorker::decode(&[VERSION, 16]),
        Err(DecodeError::Truncated)
    );
}

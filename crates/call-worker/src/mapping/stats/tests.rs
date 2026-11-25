use super::*;
use persephone_call_protocol::MAX_IDENTITY_BYTES;

fn audio_in(bytes: u64) -> RawRemoteAudio {
    RawRemoteAudio {
        track: "TR_audio".to_owned(),
        at_ms: 0,
        level: 0.5,
        bytes_received: bytes,
        packets_received: 100,
        packets_lost: 2,
        jitter_seconds: 0.012,
        total_samples: 0,
        concealed_samples: 0,
    }
}

fn remote(identity: &str, audio: Option<RawRemoteAudio>) -> RawRemote {
    RawRemote {
        connection: 0,
        identity: identity.to_owned(),
        audio,
        video: None,
    }
}

fn mic(bytes: u64) -> RawOwnAudio {
    RawOwnAudio {
        track: "TR_mic".to_owned(),
        at_ms: 0,
        level: 1.0,
        bytes_sent: bytes,
        packets_sent: 5,
        packets_lost: Some(-3),
        jitter_seconds: Some(0.0004),
        rtt_seconds: Some(0.0415),
    }
}

fn reading(at_ms: u64, bytes: u64) -> RawStats {
    RawStats {
        microphone: Some(RawOwnAudio {
            at_ms,
            ..mic(bytes)
        }),
        ..RawStats::default()
    }
}

fn bitrate(stats: &CallStats) -> Option<u32> {
    stats
        .microphone
        .as_ref()
        .and_then(|audio| audio.bitrate_kbps)
}

/// GIVEN a counter that grows WHEN the delta is taken THEN it is the
/// growth; GIVEN no previous value THEN there is none.
#[test]
fn given_a_growing_counter_when_the_delta_is_taken_then_it_is_the_growth() {
    assert_eq!(counter_delta(Some(10), 25, Width::Bits64), Some(15));
    assert_eq!(counter_delta(Some(10), 10, Width::Bits64), Some(0));
    assert_eq!(counter_delta(None, 25, Width::Bits64), None);
}

/// GIVEN a counter that falls WHEN the delta is taken THEN it is a reset
/// and has no delta.
#[test]
fn given_a_counter_that_falls_when_the_delta_is_taken_then_it_is_a_reset() {
    assert_eq!(counter_delta(Some(1000), 5, Width::Bits64), None);
    // A small fall of a 32-bit counter is no wrap.
    assert_eq!(counter_delta(Some(1000), 5, Width::Bits32), None);
    // A fall from a 64-bit value beyond 32 bits is never a wrap.
    assert_eq!(counter_delta(Some(1 << 40), 5, Width::Bits32), None);
}

/// GIVEN a 32-bit counter near its top that comes back near zero WHEN the
/// delta is taken THEN it is the growth across the wrap, but a 64-bit
/// counter is not wrapped.
#[test]
fn given_a_32_bit_counter_wraps_when_the_delta_is_taken_then_it_counts_across() {
    let top = u64::from(u32::MAX);
    assert_eq!(counter_delta(Some(top - 4), 5, Width::Bits32), Some(10));
    assert_eq!(counter_delta(Some(top), 0, Width::Bits32), Some(1));
    assert_eq!(counter_delta(Some(top - 4), 5, Width::Bits64), None);
}

/// GIVEN a zero interval WHEN a rate is made THEN there is none.
#[test]
fn given_a_zero_interval_when_a_rate_is_made_then_there_is_none() {
    assert_eq!(per_second(10, 0), None);
    assert_eq!(kbps(10, 0), None);
    assert_eq!(per_second(10, 500), Some(20));
}

/// GIVEN bytes over an interval WHEN the bitrate is made THEN it is in
/// kbit/s, and a huge value stops at the bound.
#[test]
fn given_bytes_over_an_interval_when_the_bitrate_is_made_then_it_is_in_kbps() {
    // 125 000 bytes in a second are 1000 kbit/s.
    assert_eq!(kbps(125_000, 1000), Some(1000));
    assert_eq!(kbps(125_000, 2000), Some(500));
    assert_eq!(kbps(u64::MAX, 1), Some(MAX_STATS_KBPS));
}

/// GIVEN odd WebRTC numbers WHEN they are converted THEN each stays inside
/// its bound and a number that is not a number is unknown.
#[test]
fn given_odd_numbers_when_converted_then_they_stay_inside_their_bounds() {
    assert_eq!(millis(0.0415), Some(42));
    assert_eq!(millis(1e9), Some(MAX_STATS_MS));
    assert_eq!(millis(-1.0), None);
    assert_eq!(millis(f64::NAN), None);
    assert_eq!(millis(f64::INFINITY), None);
    assert_eq!(count(-5), 0);
    assert_eq!(count(i64::MAX), u32::MAX);
    assert_eq!(frames_per_second(29.6), 30);
    assert_eq!(frames_per_second(1e9), MAX_STATS_FPS);
    assert_eq!(frames_per_second(f64::NAN), 0);
    assert_eq!(side(u32::MAX), MAX_STATS_SIDE);
    assert_eq!(kbps_from_bits(2_500_000.0), Some(2500));
    assert_eq!(kbps_from_bits(1e300), Some(MAX_STATS_KBPS));
    assert_eq!(kbps_from_bits(f64::NAN), None);
}

/// GIVEN the first reading WHEN it is made THEN no rate is known; GIVEN the
/// second a second later THEN the rate follows from the change.
#[test]
fn given_two_readings_when_made_then_the_second_has_the_rate() {
    let mut book = StatsBook::default();
    let first = book.next(&reading(1000, 10_000));
    assert_eq!(bitrate(&first), None);
    // Own values that need no earlier reading are there at once.
    let audio = first.microphone.unwrap();
    assert_eq!(audio.level, 100);
    assert_eq!(audio.packets_lost, Some(0), "a negative loss counts as 0");
    assert_eq!(audio.jitter_ms, Some(0));
    assert_eq!(audio.rtt_ms, Some(42));
    let second = book.next(&reading(2000, 10_000 + 4000));
    assert_eq!(bitrate(&second), Some(32));
    // Half a second later with the same growth doubles the rate.
    let third = book.next(&reading(2500, 10_000 + 8000));
    assert_eq!(bitrate(&third), Some(64));
}

/// GIVEN a counter that was reset WHEN the next reading is made THEN that
/// reading has no rate and the one after has it again.
#[test]
fn given_a_reset_counter_when_read_then_one_reading_has_no_rate() {
    let mut book = StatsBook::default();
    book.next(&reading(1000, 50_000));
    let after_reset = book.next(&reading(2000, 100));
    assert_eq!(bitrate(&after_reset), None);
    let next = book.next(&reading(3000, 100 + 12_500));
    assert_eq!(bitrate(&next), Some(100));
}

/// GIVEN two readings at the same time WHEN the second is made THEN it has
/// no rate and the third still compares with the first.
#[test]
fn given_a_zero_interval_when_read_then_no_rate_and_the_baseline_stays() {
    let mut book = StatsBook::default();
    book.next(&reading(1000, 0));
    let same = book.next(&reading(1000, 5000));
    assert_eq!(bitrate(&same), None);
    let later = book.next(&reading(2000, 12_500));
    assert_eq!(bitrate(&later), Some(100));
}

/// GIVEN a reading with an earlier time WHEN it is made THEN it has no rate
/// and becomes the new baseline.
#[test]
fn given_time_going_back_when_read_then_no_rate_and_a_new_baseline() {
    let mut book = StatsBook::default();
    book.next(&reading(5000, 1_000_000));
    let back = book.next(&reading(1000, 0));
    assert_eq!(bitrate(&back), None);
    let next = book.next(&reading(2000, 12_500));
    assert_eq!(bitrate(&next), Some(100));
}

/// GIVEN the statistics were turned off and on WHEN the book is reset THEN
/// the first reading has no rate, however long the pause.
#[test]
fn given_a_reset_book_when_read_then_the_first_reading_has_no_rate() {
    let mut book = StatsBook::default();
    book.next(&reading(1000, 0));
    book.reset();
    assert_eq!(bitrate(&book.next(&reading(60_000, 1_000_000))), None);
}

/// GIVEN a track that stops and starts WHEN readings are made THEN the new
/// track has no rate until its second reading.
#[test]
fn given_a_track_that_goes_and_returns_when_read_then_its_rate_starts_again() {
    let mut book = StatsBook::default();
    book.next(&reading(1000, 0));
    book.next(&RawStats::default());
    assert_eq!(bitrate(&book.next(&reading(3000, 99_000))), None);
    assert_eq!(
        bitrate(&book.next(&reading(4000, 99_000 + 125_000))),
        Some(1000)
    );
}

/// GIVEN remote audio that conceals samples WHEN read twice THEN the share
/// is of the samples of the last interval only.
#[test]
fn given_concealed_samples_when_read_then_the_share_is_of_the_last_interval() {
    let mut book = StatsBook::default();
    let with = |at_ms, total, concealed| RawStats {
        remotes: vec![remote(
            "@a:example.org:D",
            Some(RawRemoteAudio {
                at_ms,
                total_samples: total,
                concealed_samples: concealed,
                ..audio_in(0)
            }),
        )],
        ..RawStats::default()
    };
    let share = |stats: &CallStats| stats.remotes[0].audio.unwrap().concealed_percent;
    assert_eq!(share(&book.next(&with(1000, 48_000, 40_000))), None);
    assert_eq!(share(&book.next(&with(2000, 96_000, 40_000))), Some(0));
    assert_eq!(share(&book.next(&with(3000, 144_000, 52_000))), Some(25));
    // No samples in the interval: no share.
    assert_eq!(share(&book.next(&with(4000, 144_000, 52_000))), None);
    // More concealed than total is stopped at 100.
    assert_eq!(share(&book.next(&with(5000, 144_100, 99_000))), Some(100));
}

/// GIVEN remote video that drops frames WHEN read twice THEN the drop rate
/// is per second.
#[test]
fn given_dropped_frames_when_read_then_the_rate_is_per_second() {
    let mut book = StatsBook::default();
    let with = |at_ms, dropped| RawStats {
        remotes: vec![RawRemote {
            video: Some(RawRemoteVideo {
                at_ms,
                fps: 29.97,
                width: 1280,
                height: 720,
                frames_dropped: dropped,
                ..RawRemoteVideo::default()
            }),
            ..remote("@a:example.org:D", None)
        }],
        ..RawStats::default()
    };
    let rate = |stats: &CallStats| stats.remotes[0].video.unwrap().dropped_fps;
    assert_eq!(rate(&book.next(&with(1000, 7))), None);
    assert_eq!(rate(&book.next(&with(3000, 17))), Some(5));
    let last = book.next(&with(4000, 1_000_000));
    assert_eq!(rate(&last), Some(MAX_STATS_FPS));
    assert_eq!(last.remotes[0].video.unwrap().fps, 30);
}

/// GIVEN too many, repeated and invalid participants WHEN read THEN the
/// message holds only valid ones, each once, and at most the limit.
#[test]
fn given_bad_participants_when_read_then_only_valid_distinct_ones_stay() {
    let mut raw = RawStats::default();
    raw.remotes.push(remote("", None));
    raw.remotes.push(remote("bad\nname", None));
    raw.remotes
        .push(remote(&"x".repeat(MAX_IDENTITY_BYTES + 1), None));
    raw.remotes.push(RawRemote {
        connection: MAX_OTHER_CONNECTIONS + 1,
        ..remote("@far:example.org:D", None)
    });
    raw.remotes.push(remote("@a:example.org:D", None));
    raw.remotes
        .push(remote("@a:example.org:D", Some(audio_in(1))));
    raw.remotes.push(RawRemote {
        connection: 2,
        ..remote("@a:example.org:D", None)
    });
    raw.remotes
        .extend((0..MAX_PARTICIPANTS * 2).map(|i| remote(&format!("@u{i}:example.org:D"), None)));
    let stats = StatsBook::default().next(&raw);
    assert_eq!(stats.remotes.len(), MAX_PARTICIPANTS);
    assert_eq!(stats.remotes[0].identity, "@a:example.org:D");
    assert!(stats.remotes[0].audio.is_none(), "the first entry wins");
    assert_eq!(stats.remotes[1].connection, 2);
    // The message encodes and decodes: nothing the protocol refuses is left.
    let message = persephone_call_protocol::FromWorker::Stats(stats);
    assert_eq!(
        persephone_call_protocol::FromWorker::decode(&message.encode()),
        Ok(message)
    );
}

fn link(role: LinkRole, at_ms: u64, sent: u64, received: u64) -> RawLink {
    RawLink {
        role,
        at_ms,
        bytes_sent: sent,
        bytes_received: received,
    }
}

fn connection(number: u8, links: Vec<RawLink>, complete: bool) -> RawConnection {
    RawConnection {
        connection: number,
        outgoing_bits_per_second: Some(3_000_000.0),
        rtt_seconds: Some(0.02),
        links,
        complete,
    }
}

/// GIVEN connections that repeat or do not exist WHEN read THEN each real
/// one is there once and the rates follow the transport bytes.
#[test]
fn given_connections_when_read_then_rates_follow_the_transport_bytes() {
    let one = |number, at_ms, sent, received| {
        connection(
            number,
            vec![link(LinkRole::Single, at_ms, sent, received)],
            true,
        )
    };
    let raw = |at_ms, sent, received| RawStats {
        connections: vec![
            one(0, at_ms, sent, received),
            one(0, at_ms, 0, 0),
            one(1, at_ms, sent, received),
            one(MAX_OTHER_CONNECTIONS + 1, at_ms, 0, 0),
        ],
        ..RawStats::default()
    };
    let mut book = StatsBook::default();
    let first = book.next(&raw(1000, 0, 0));
    assert_eq!(first.connections.len(), 2);
    assert_eq!(first.connections[0].send_kbps, None);
    assert_eq!(first.connections[0].outgoing_kbps, Some(3000));
    assert_eq!(first.connections[0].rtt_ms, Some(20));
    let second = book.next(&raw(2000, 250_000, 125_000));
    assert_eq!(second.connections[0].send_kbps, Some(2000));
    assert_eq!(second.connections[0].receive_kbps, Some(1000));
    assert_eq!(second.connections[1].connection, 1);
}

/// GIVEN a participant that left WHEN readings go on THEN its counters are
/// forgotten, so the book holds no more than the reading shows.
#[test]
fn given_a_participant_that_left_when_read_then_its_counters_are_forgotten() {
    let mut book = StatsBook::default();
    let both = RawStats {
        remotes: vec![
            remote("@a:example.org:D", Some(audio_in(1))),
            remote("@b:example.org:D", Some(audio_in(1))),
        ],
        ..RawStats::default()
    };
    book.next(&both);
    let held = book.counters.len();
    book.next(&RawStats {
        remotes: vec![remote(
            "@a:example.org:D",
            Some(RawRemoteAudio {
                at_ms: 1000,
                ..audio_in(1)
            }),
        )],
        ..RawStats::default()
    });
    assert!(book.counters.len() < held);
    assert!(book.counters.keys().all(|(source, _)| matches!(
        source,
        Source::Remote(0, identity, _) if identity == "@a:example.org:D"
    )));
}

/// GIVEN a participant whose video moves from one track to another (the
/// camera after a screen share) WHEN read THEN the new track has no rate
/// until its second reading, however its counters compare with the old
/// track's.
#[test]
fn given_a_new_track_for_a_participant_when_read_then_no_rate_crosses_tracks() {
    let mut book = StatsBook::default();
    let with = |track: &str, at_ms, bytes, dropped| RawStats {
        remotes: vec![RawRemote {
            video: Some(RawRemoteVideo {
                track: track.to_owned(),
                at_ms,
                bytes_received: bytes,
                frames_dropped: dropped,
                ..RawRemoteVideo::default()
            }),
            ..remote("@a:example.org:D", None)
        }],
        ..RawStats::default()
    };
    let video = |stats: CallStats| stats.remotes[0].video.unwrap();
    book.next(&with("TR_screen", 1000, 100_000, 3));
    let switched = video(book.next(&with("TR_camera", 2000, 250_000, 9)));
    assert_eq!(switched.bitrate_kbps, None, "never compared with the share");
    assert_eq!(switched.dropped_fps, None);
    let next = video(book.next(&with("TR_camera", 3000, 250_000 + 125_000, 10)));
    assert_eq!(next.bitrate_kbps, Some(1000));
    assert_eq!(next.dropped_fps, Some(1));
    // The own microphone after a device change is a new track too.
    book.next(&reading(4000, 10_000));
    let new_mic = book.next(&RawStats {
        microphone: Some(RawOwnAudio {
            track: "TR_mic_2".to_owned(),
            at_ms: 5000,
            ..mic(500_000)
        }),
        ..RawStats::default()
    });
    assert_eq!(bitrate(&new_mic), None);
}

/// GIVEN two tracks read at different times in one reading WHEN the rates
/// are made THEN each uses the time between its own two reads, so a slow
/// read of another track never changes them.
#[test]
fn given_tracks_read_at_different_times_when_rated_then_each_uses_its_own_interval() {
    let mut book = StatsBook::default();
    let with = |mic_at, remote_at, bytes| RawStats {
        microphone: Some(RawOwnAudio {
            at_ms: mic_at,
            ..mic(bytes)
        }),
        remotes: vec![remote(
            "@a:example.org:D",
            Some(RawRemoteAudio {
                at_ms: remote_at,
                ..audio_in(bytes)
            }),
        )],
        ..RawStats::default()
    };
    book.next(&with(1000, 1000, 0));
    let stats = book.next(&with(2000, 2700, 125_000));
    assert_eq!(bitrate(&stats), Some(1000), "one second for the microphone");
    assert_eq!(
        stats.remotes[0].audio.unwrap().bitrate_kbps,
        Some(588),
        "1.7 seconds for the remote track"
    );
}

/// GIVEN a connection with a publisher and a subscriber transport WHEN one
/// transport was not read THEN the totals are unknown in that reading and
/// in the next one, and never jump; the reading after has the true rate.
#[test]
fn given_a_transport_that_was_not_read_when_rated_then_the_totals_are_unknown() {
    let mut book = StatsBook::default();
    let both = |at_ms, published, subscribed| RawStats {
        connections: vec![connection(
            0,
            vec![
                link(LinkRole::Publisher, at_ms, published, 0),
                link(LinkRole::Subscriber, at_ms, subscribed, 0),
            ],
            true,
        )],
        ..RawStats::default()
    };
    let send = |stats: CallStats| stats.connections[0].send_kbps;
    assert_eq!(send(book.next(&both(1000, 100_000, 50_000))), None);
    assert_eq!(send(book.next(&both(2000, 200_000, 60_000))), Some(880));
    // The publisher's read missed its deadline.
    let short = RawStats {
        connections: vec![connection(
            0,
            vec![link(LinkRole::Subscriber, 3000, 70_000, 0)],
            false,
        )],
        ..RawStats::default()
    };
    assert_eq!(send(book.next(&short)), None, "a short total is unknown");
    // The publisher has no sample before, so no jump on its return.
    assert_eq!(send(book.next(&both(4000, 400_000, 80_000))), None);
    assert_eq!(send(book.next(&both(5000, 420_000, 90_000))), Some(240));
}

//! Turns raw WebRTC counters into the call statistics message (ADR-0066).
//!
//! Pure: no LiveKit types, no I/O and no clock. The caller gives each
//! track's and each transport's numbers the time they were read, in
//! milliseconds, and the book makes each rate from the change of a counter
//! since its sample before, over the time between the two samples. A
//! track's counters are kept under its track ID, so a new track never
//! compares with an older one.

use std::collections::HashMap;

use persephone_call_protocol::{
    AudioReceiveStats, AudioSendStats, CallStats, ConnectionStats, MAX_OTHER_CONNECTIONS,
    MAX_PARTICIPANTS, MAX_STATS_CONNECTIONS, MAX_STATS_FPS, MAX_STATS_KBPS, MAX_STATS_MS,
    MAX_STATS_SIDE, QualityLimit, RemoteStats, VideoReceiveStats, VideoSendStats,
};

use super::{sound_level, valid_identity};

/// How many bits a counter has in WebRTC. A 32-bit counter that falls from
/// near its top to near zero has wrapped. Any other fall is a reset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Width {
    Bits32,
    Bits64,
}

/// How much `current` is above `previous`, or `None` when there is no
/// previous value or the counter was reset.
pub fn counter_delta(previous: Option<u64>, current: u64, width: Width) -> Option<u64> {
    let previous = previous?;
    if current >= previous {
        return Some(current - previous);
    }
    let top = u64::from(u32::MAX);
    // A 32-bit counter wraps from near its top to near zero. A fall by less
    // than half the range is a reset or a counter that went back.
    if width == Width::Bits32 && previous <= top && current <= top && previous - current > top / 2 {
        return Some(current + (top - previous) + 1);
    }
    None
}

/// `delta` each second over `interval_ms`. `None` for no interval.
pub fn per_second(delta: u64, interval_ms: u64) -> Option<u64> {
    if interval_ms == 0 {
        return None;
    }
    u64::try_from(u128::from(delta) * 1000 / u128::from(interval_ms)).ok()
}

/// The bitrate in kbit/s of `delta` bytes over `interval_ms`, at most
/// [`MAX_STATS_KBPS`]. `None` for no interval.
pub fn kbps(delta_bytes: u64, interval_ms: u64) -> Option<u32> {
    if interval_ms == 0 {
        return None;
    }
    // Bits per millisecond are kbit/s.
    let rate = u128::from(delta_bytes) * 8 / u128::from(interval_ms);
    Some(u32::try_from(rate.min(u128::from(MAX_STATS_KBPS))).unwrap_or(MAX_STATS_KBPS))
}

/// Seconds as WebRTC reports them to milliseconds, at most
/// [`MAX_STATS_MS`]. A value that is not a number or is negative is `None`.
pub fn millis(seconds: f64) -> Option<u16> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    // Clamped first, so the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let millis = (seconds * 1000.0).round().min(f64::from(MAX_STATS_MS)) as u16;
    Some(millis)
}

/// A count that WebRTC reports signed (lost packets can fall below zero when
/// duplicates arrive): never below 0 and at most `u32::MAX`.
pub fn count(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

fn unsigned(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn frames_per_second(value: f64) -> u8 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    // Clamped first, so the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let fps = value.round().min(f64::from(MAX_STATS_FPS)) as u8;
    fps
}

fn side(value: u32) -> u16 {
    u16::try_from(value.min(u32::from(MAX_STATS_SIDE))).unwrap_or(MAX_STATS_SIDE)
}

fn kbps_from_bits(bits_per_second: f64) -> Option<u32> {
    if !bits_per_second.is_finite() || bits_per_second < 0.0 {
        return None;
    }
    // Clamped first, so the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rate = (bits_per_second / 1000.0)
        .round()
        .min(f64::from(MAX_STATS_KBPS)) as u32;
    Some(rate)
}

// The raw readings: plain numbers in the units of WebRTC's statistics
// (seconds, bytes, 0.0 to 1.0 levels).

/// The own microphone track.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawOwnAudio {
    /// The track ID, which keys its counters, and the time of its read.
    pub track: String,
    pub at_ms: u64,
    /// Linear, 0.0 to 1.0.
    pub level: f64,
    pub bytes_sent: u64,
    pub packets_sent: u64,
    /// What the far end reports; `None` before its first report.
    pub packets_lost: Option<i64>,
    pub jitter_seconds: Option<f64>,
    pub rtt_seconds: Option<f64>,
}

/// The own camera or screen share track. A track with several layers gives
/// the sum of their bytes and the largest layer's size and frame rate.
#[derive(Clone, Debug, PartialEq)]
pub struct RawOwnVideo {
    pub track: String,
    pub at_ms: u64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    pub bytes_sent: u64,
    pub limit: QualityLimit,
    pub packets_lost: Option<i64>,
}

/// The audio of one remote participant: its microphone track (one per
/// participant; the smallest track id when it has more).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawRemoteAudio {
    pub track: String,
    pub at_ms: u64,
    /// Linear, 0.0 to 1.0.
    pub level: f64,
    pub bytes_received: u64,
    pub packets_received: u64,
    pub packets_lost: i64,
    pub jitter_seconds: f64,
    pub total_samples: u64,
    pub concealed_samples: u64,
}

/// The video of one remote participant.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawRemoteVideo {
    pub track: String,
    pub at_ms: u64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    pub bytes_received: u64,
    pub frames_dropped: u64,
    pub packets_lost: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawRemote {
    pub connection: u8,
    pub identity: String,
    pub audio: Option<RawRemoteAudio>,
    pub video: Option<RawRemoteVideo>,
}

/// Which transport of a connection a link is.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LinkRole {
    /// The own room's single peer connection, which sends and receives.
    Single,
    Publisher,
    Subscriber,
}

/// The byte counters of one transport, read at `at_ms`.
#[derive(Clone, Debug, PartialEq)]
pub struct RawLink {
    pub role: LinkRole,
    pub at_ms: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// One connection to an SFU.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawConnection {
    pub connection: u8,
    /// WebRTC's estimate of the bandwidth for sending, in bit/s.
    pub outgoing_bits_per_second: Option<f64>,
    pub rtt_seconds: Option<f64>,
    /// The transports whose bytes add up to the connection's totals, each
    /// role once.
    pub links: Vec<RawLink>,
    /// Whether every transport that the connection uses was read. When one
    /// was not, the totals would be short, so the byte rates are unknown.
    pub complete: bool,
}

/// The sending transport's numbers, for the Auto camera quality.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RawSending {
    /// WebRTC's estimate of the bandwidth for sending, in bit/s.
    pub outgoing_bits_per_second: Option<f64>,
    pub rtt_seconds: Option<f64>,
}

/// One reading of everything. Each part carries the time it was read, on a
/// clock that only goes forward.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawStats {
    pub microphone: Option<RawOwnAudio>,
    pub camera: Option<RawOwnVideo>,
    pub screen: Option<RawOwnVideo>,
    pub remotes: Vec<RawRemote>,
    pub connections: Vec<RawConnection>,
    /// The own room's sending transport: the publisher's, or the one
    /// transport when the room uses one peer connection. Never a
    /// subscriber's: its round-trip time says nothing about the sending
    /// link. The connections' numbers take each value from any transport.
    pub sending: Option<RawSending>,
}

/// What a counter belongs to: an own track by its track ID, a remote track
/// by connection, identity and track ID, or one transport of a connection.
/// A new track starts with no earlier value, so a switch (a camera after a
/// screen share, a new microphone) never gives a wrong rate.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Source {
    Microphone(String),
    Camera(String),
    Screen(String),
    Remote(u8, String, String),
    Link(u8, LinkRole),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Counter {
    BytesSent,
    BytesReceived,
    AudioBytes,
    VideoBytes,
    FramesDropped,
    TotalSamples,
    ConcealedSamples,
}

type Key = (Source, Counter);

/// A counter's value and the time it was read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Sample {
    value: u64,
    at_ms: u64,
}

/// Remembers the last sample of each counter, so a reading gives rates.
#[derive(Debug, Default)]
pub struct StatsBook {
    counters: HashMap<Key, Sample>,
}

/// The counters of one reading while it is made.
struct Reading<'a> {
    before: &'a HashMap<Key, Sample>,
    now: HashMap<Key, Sample>,
}

impl Reading<'_> {
    /// The change of a counter since its sample before, and the time
    /// between the two. `None` for a counter with no sample before, one that
    /// was reset, and a sample at the time of the one before or earlier. A
    /// sample at the same time keeps the one before, so the next sample
    /// still has a rate; any other sample is kept for the next reading.
    fn delta(
        &mut self,
        source: &Source,
        counter: Counter,
        sample: Sample,
        width: Width,
    ) -> Option<(u64, u64)> {
        let key = (source.clone(), counter);
        let before = self.before.get(&key).copied();
        if let Some(before) = before
            && before.at_ms == sample.at_ms
        {
            self.now.insert(key, before);
            return None;
        }
        self.now.insert(key, sample);
        let before = before?;
        let interval = sample
            .at_ms
            .checked_sub(before.at_ms)
            .filter(|interval| *interval > 0)?;
        Some((
            counter_delta(Some(before.value), sample.value, width)?,
            interval,
        ))
    }

    fn kbps(&mut self, source: &Source, counter: Counter, bytes: u64, at_ms: u64) -> Option<u32> {
        let sample = Sample {
            value: bytes,
            at_ms,
        };
        let (delta, interval) = self.delta(source, counter, sample, Width::Bits64)?;
        kbps(delta, interval)
    }
}

impl StatsBook {
    /// Forgets every counter, so the next reading has no rates. Call it when
    /// the statistics are turned on, so a pause is not a long interval.
    pub fn reset(&mut self) {
        self.counters.clear();
    }

    /// The statistics message for `raw`. A rate is `None` for a counter with
    /// no sample before (the first reading, a new track), one that was
    /// reset, and one read at the time of its sample before or earlier.
    /// Entries past the limits and entries the protocol would refuse are
    /// left out. Counters that the reading does not show are forgotten.
    pub fn next(&mut self, raw: &RawStats) -> CallStats {
        let before = std::mem::take(&mut self.counters);
        let mut reading = Reading {
            before: &before,
            now: HashMap::new(),
        };
        let stats =
            CallStats {
                microphone: raw
                    .microphone
                    .as_ref()
                    .map(|audio| own_audio(&mut reading, audio)),
                camera: raw.camera.as_ref().map(|video| {
                    own_video(&mut reading, &Source::Camera(video.track.clone()), video)
                }),
                screen: raw.screen.as_ref().map(|video| {
                    own_video(&mut reading, &Source::Screen(video.track.clone()), video)
                }),
                remotes: remotes(&mut reading, &raw.remotes),
                connections: connections(&mut reading, &raw.connections),
            };
        self.counters = reading.now;
        stats
    }
}

fn own_audio(reading: &mut Reading<'_>, audio: &RawOwnAudio) -> AudioSendStats {
    AudioSendStats {
        level: sound_level(audio.level),
        bitrate_kbps: reading.kbps(
            &Source::Microphone(audio.track.clone()),
            Counter::BytesSent,
            audio.bytes_sent,
            audio.at_ms,
        ),
        packets_sent: unsigned(audio.packets_sent),
        packets_lost: audio.packets_lost.map(count),
        jitter_ms: audio.jitter_seconds.and_then(millis),
        rtt_ms: audio.rtt_seconds.and_then(millis),
    }
}

fn own_video(reading: &mut Reading<'_>, source: &Source, video: &RawOwnVideo) -> VideoSendStats {
    VideoSendStats {
        fps: frames_per_second(video.fps),
        width: side(video.width),
        height: side(video.height),
        bitrate_kbps: reading.kbps(source, Counter::BytesSent, video.bytes_sent, video.at_ms),
        limit: video.limit,
        packets_lost: video.packets_lost.map(count),
    }
}

fn remotes(reading: &mut Reading<'_>, raw: &[RawRemote]) -> Vec<RemoteStats> {
    let mut out: Vec<RemoteStats> = Vec::new();
    for remote in raw {
        if out.len() >= MAX_PARTICIPANTS {
            break;
        }
        if !valid_identity(&remote.identity)
            || remote.connection > MAX_OTHER_CONNECTIONS
            || out.iter().any(|seen| {
                seen.connection == remote.connection && seen.identity == remote.identity
            })
        {
            continue;
        }
        let track = |track: &str| {
            Source::Remote(remote.connection, remote.identity.clone(), track.to_owned())
        };
        let audio = remote.audio.as_ref().map(|audio| {
            let source = track(&audio.track);
            let sample = |value| Sample {
                value,
                at_ms: audio.at_ms,
            };
            let total = reading
                .delta(
                    &source,
                    Counter::TotalSamples,
                    sample(audio.total_samples),
                    Width::Bits64,
                )
                .map(|(delta, _)| delta);
            let concealed = reading
                .delta(
                    &source,
                    Counter::ConcealedSamples,
                    sample(audio.concealed_samples),
                    Width::Bits64,
                )
                .map(|(delta, _)| delta);
            AudioReceiveStats {
                level: sound_level(audio.level),
                bitrate_kbps: reading.kbps(
                    &source,
                    Counter::AudioBytes,
                    audio.bytes_received,
                    audio.at_ms,
                ),
                packets_received: unsigned(audio.packets_received),
                packets_lost: count(audio.packets_lost),
                jitter_ms: millis(audio.jitter_seconds).unwrap_or(0),
                concealed_percent: concealed_percent(total, concealed),
            }
        });
        let video = remote.video.as_ref().map(|video| {
            let source = track(&video.track);
            let dropped = reading.delta(
                &source,
                Counter::FramesDropped,
                Sample {
                    value: video.frames_dropped,
                    at_ms: video.at_ms,
                },
                Width::Bits32,
            );
            VideoReceiveStats {
                fps: frames_per_second(video.fps),
                width: side(video.width),
                height: side(video.height),
                bitrate_kbps: reading.kbps(
                    &source,
                    Counter::VideoBytes,
                    video.bytes_received,
                    video.at_ms,
                ),
                dropped_fps: dropped
                    .and_then(|(delta, interval)| per_second(delta, interval))
                    .map(|rate| u8::try_from(rate.min(u64::from(MAX_STATS_FPS))).unwrap_or(0)),
                packets_lost: count(video.packets_lost),
            }
        });
        out.push(RemoteStats {
            connection: remote.connection,
            identity: remote.identity.clone(),
            audio,
            video,
        });
    }
    out
}

/// The share of the samples of one interval that WebRTC made up. `None`
/// when there was no interval or no sample.
fn concealed_percent(total: Option<u64>, concealed: Option<u64>) -> Option<u8> {
    let (total, concealed) = (total?, concealed?);
    if total == 0 {
        return None;
    }
    let percent = (u128::from(concealed) * 100 / u128::from(total)).min(100);
    u8::try_from(percent).ok()
}

fn connections(reading: &mut Reading<'_>, raw: &[RawConnection]) -> Vec<ConnectionStats> {
    let mut out: Vec<ConnectionStats> = Vec::new();
    for connection in raw {
        if out.len() >= MAX_STATS_CONNECTIONS
            || connection.connection > MAX_OTHER_CONNECTIONS
            || out
                .iter()
                .any(|seen| seen.connection == connection.connection)
        {
            continue;
        }
        out.push(ConnectionStats {
            connection: connection.connection,
            outgoing_kbps: connection.outgoing_bits_per_second.and_then(kbps_from_bits),
            send_kbps: link_kbps(reading, connection, Counter::BytesSent, |link| {
                link.bytes_sent
            }),
            receive_kbps: link_kbps(reading, connection, Counter::BytesReceived, |link| {
                link.bytes_received
            }),
            rtt_ms: connection.rtt_seconds.and_then(millis),
        });
    }
    out
}

/// A connection's rate of one counter: the sum of its transports' rates,
/// each from that transport's own samples. Unknown when a transport that
/// the connection uses was not read, or when one has no rate yet, so a
/// missing transport never makes a total that is short or one that jumps.
/// Each transport's sample is kept for the next reading either way.
fn link_kbps(
    reading: &mut Reading<'_>,
    connection: &RawConnection,
    counter: Counter,
    bytes: fn(&RawLink) -> u64,
) -> Option<u32> {
    let mut total = Some(0_u32);
    let mut roles: Vec<LinkRole> = Vec::new();
    for link in &connection.links {
        if roles.contains(&link.role) {
            continue;
        }
        roles.push(link.role);
        let source = Source::Link(connection.connection, link.role);
        let rate = reading.kbps(&source, counter, bytes(link), link.at_ms);
        total = total
            .zip(rate)
            .map(|(sum, rate)| sum.saturating_add(rate).min(MAX_STATS_KBPS));
    }
    total.filter(|_| connection.complete && !roles.is_empty())
}

#[cfg(test)]
mod tests;

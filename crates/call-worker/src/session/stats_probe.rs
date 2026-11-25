//! Reads the call's WebRTC statistics for the "technical details" panel
//! (ADR-0066).
//!
//! Like [`super::SoundProbe`] it runs one read at a time, on its own task and
//! under a deadline, so the event loop never waits for it. It reads only
//! while the core has turned the statistics on, and at most once a second.
//! The task gives back plain numbers (`RawStats`); `mapping::stats` makes
//! the message from them. Nothing here keeps a track, an address or a name:
//! only the numbers that the message carries leave the parse functions.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use livekit::{
    prelude::{LocalAudioTrack, LocalVideoTrack, RemoteAudioTrack, RemoteVideoTrack},
    webrtc::stats::{QualityLimitationReason, RtcStats},
};
use persephone_call_protocol::{FromWorker, MAX_OTHER_CONNECTIONS, QualityLimit};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::read_all;
use crate::mapping::stats::{
    RawConnection, RawOwnAudio, RawOwnVideo, RawRemote, RawRemoteAudio, RawRemoteVideo, RawStats,
    StatsBook,
};

/// The shortest time between two reads.
pub const STATS_INTERVAL: Duration = Duration::from_secs(1);
/// The time one read has for all its statistics. It is shorter than
/// [`STATS_TIMEOUT`], so the task reports what it has.
const STATS_READ_BUDGET: Duration = Duration::from_millis(700);
/// The longest one read may take altogether.
const STATS_TIMEOUT: Duration = Duration::from_millis(900);
const _: () = assert!(STATS_READ_BUDGET.as_millis() < STATS_TIMEOUT.as_millis());
const _: () = assert!(STATS_TIMEOUT.as_millis() < STATS_INTERVAL.as_millis());

/// What one read looks at. The tracks are cheap handles; at most one audio
/// and one video track of each remote participant, so at most
/// `2 * MAX_PARTICIPANTS` remote reads and three own ones.
#[derive(Default)]
pub struct Sources {
    /// The own room sends and receives on one peer connection (LiveKit's
    /// single peer connection), so its own and remote tracks read the same
    /// transport, which is counted once.
    pub one_connection: bool,
    pub microphone: Option<LocalAudioTrack>,
    pub camera: Option<LocalVideoTrack>,
    pub screen: Option<LocalVideoTrack>,
    pub remote_audio: Vec<(u8, String, RemoteAudioTrack)>,
    pub remote_video: Vec<(u8, String, RemoteVideoTrack)>,
}

/// The reading of one track: its statistics, or none when they could not be
/// read.
enum Read {
    Microphone(Vec<RtcStats>),
    Camera(Vec<RtcStats>),
    Screen(Vec<RtcStats>),
    RemoteAudio(u8, String, Vec<RtcStats>),
    RemoteVideo(u8, String, Vec<RtcStats>),
}

type Job = Pin<Box<dyn Future<Output = Read> + Send>>;

/// One read's report. Dropping it sends the numbers, or `None`, so the task
/// reports once however it ends. Only one read runs and the slot is freed
/// only by its report, so the channel has room.
struct Report {
    done: mpsc::Sender<(u64, Option<RawStats>)>,
    generation: u64,
    stats: Option<RawStats>,
}

impl Drop for Report {
    fn drop(&mut self) {
        let _ = self.done.try_send((self.generation, self.stats.take()));
    }
}

/// Reads the statistics while they are on.
pub struct StatsProbe {
    done: mpsc::Sender<(u64, Option<RawStats>)>,
    on: bool,
    /// Numbers the reads, so the report of a read that was stopped never
    /// frees the slot of a newer one.
    generation: u64,
    task: Option<JoinHandle<()>>,
    started: Option<Instant>,
    /// The clock of the readings: they count from here.
    base: std::time::Instant,
    book: StatsBook,
}

impl StatsProbe {
    pub fn new(done: mpsc::Sender<(u64, Option<RawStats>)>) -> Self {
        Self {
            done,
            on: false,
            generation: 0,
            task: None,
            started: None,
            base: std::time::Instant::now(),
            book: StatsBook::default(),
        }
    }

    /// Turns the reads on or off (`SetStats`). Turning them on starts a new
    /// series: the first reading has no rates.
    pub fn set(&mut self, on: bool) {
        if on == self.on {
            return;
        }
        self.on = on;
        self.generation += 1;
        self.started = None;
        self.book.reset();
        // A read that runs ends by its deadline and reports; its report is
        // dropped by its number. The slot is freed only by that report.
    }

    /// Stops a read that runs, for the end of the call.
    pub fn shutdown(&mut self) {
        self.on = false;
        self.generation += 1;
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    /// Starts a read when they are on, none runs and a second has gone since
    /// the last start. `sources` is only called then.
    pub fn tick(&mut self, now: Instant, sources: impl FnOnce() -> Sources) {
        if !self.on || self.task.is_some() {
            return;
        }
        if self
            .started
            .is_some_and(|at| now.saturating_duration_since(at) < STATS_INTERVAL)
        {
            return;
        }
        self.started = Some(now);
        let done = self.done.clone();
        let generation = self.generation;
        let base = self.base;
        let sources = sources();
        self.task = Some(tokio::spawn(async move {
            let mut report = Report {
                done,
                generation,
                stats: None,
            };
            report.stats = tokio::time::timeout(STATS_TIMEOUT, read(sources, base))
                .await
                .ok();
        }));
    }

    /// A read reported, so the slot is free. The message to send, when the
    /// read belongs to the current series and finished.
    pub fn end(&mut self, generation: u64, stats: Option<RawStats>) -> Option<FromWorker> {
        self.task = None;
        if generation != self.generation {
            return None;
        }
        stats.map(|raw| FromWorker::Stats(self.book.next(&raw)))
    }
}

/// Reads every track at once, each under one deadline.
async fn read(sources: Sources, base: std::time::Instant) -> RawStats {
    let deadline = Instant::now() + STATS_READ_BUDGET;
    let one_connection = sources.one_connection;
    let mut jobs: Vec<Job> = Vec::new();
    if let Some(track) = sources.microphone {
        jobs.push(Box::pin(async move {
            Read::Microphone(track.get_stats().await.unwrap_or_default())
        }));
    }
    if let Some(track) = sources.camera {
        jobs.push(Box::pin(async move {
            Read::Camera(track.get_stats().await.unwrap_or_default())
        }));
    }
    if let Some(track) = sources.screen {
        jobs.push(Box::pin(async move {
            Read::Screen(track.get_stats().await.unwrap_or_default())
        }));
    }
    for (connection, identity, track) in sources.remote_audio {
        jobs.push(Box::pin(async move {
            Read::RemoteAudio(
                connection,
                identity,
                track.get_stats().await.unwrap_or_default(),
            )
        }));
    }
    for (connection, identity, track) in sources.remote_video {
        jobs.push(Box::pin(async move {
            Read::RemoteVideo(
                connection,
                identity,
                track.get_stats().await.unwrap_or_default(),
            )
        }));
    }
    let reads = read_all(deadline, jobs).await.into_iter().flatten();
    let at_ms = u64::try_from(base.elapsed().as_millis()).unwrap_or(u64::MAX);
    assemble(at_ms, one_connection, reads)
}

/// Makes the raw reading from what the reads gave. A track whose read did
/// not finish is left out. `one_connection` says that the own room's tracks
/// all use one transport (see [`Sources::one_connection`]).
fn assemble(at_ms: u64, one_connection: bool, reads: impl Iterator<Item = Read>) -> RawStats {
    let mut raw = RawStats {
        at_ms,
        ..RawStats::default()
    };
    // The first transport of the own tracks (the publisher) and of the
    // remote tracks of each connection (the subscriber). Each track's
    // statistics hold only its own streams and the transport they use.
    let mut publisher: Option<Link> = None;
    let mut subscribers: HashMap<u8, Link> = HashMap::new();
    let mut remotes: HashMap<(u8, String), RawRemote> = HashMap::new();
    for read in reads {
        match read {
            Read::Microphone(stats) => {
                raw.microphone = own_audio(&stats);
                publisher = publisher.or_else(|| link(&stats));
            }
            Read::Camera(stats) => {
                raw.camera = own_video(&stats);
                publisher = publisher.or_else(|| link(&stats));
            }
            Read::Screen(stats) => {
                raw.screen = own_video(&stats);
                publisher = publisher.or_else(|| link(&stats));
            }
            Read::RemoteAudio(connection, identity, stats) => {
                if let Some(link) = link(&stats) {
                    subscribers.entry(connection).or_insert(link);
                }
                remote(&mut remotes, connection, identity).audio = remote_audio(&stats);
            }
            Read::RemoteVideo(connection, identity, stats) => {
                if let Some(link) = link(&stats) {
                    subscribers.entry(connection).or_insert(link);
                }
                remote(&mut remotes, connection, identity).video = remote_video(&stats);
            }
        }
    }
    raw.remotes = remotes.into_values().collect();
    raw.remotes
        .sort_by(|a, b| (a.connection, &a.identity).cmp(&(b.connection, &b.identity)));
    raw.connections = (0..=MAX_OTHER_CONNECTIONS)
        .filter_map(|connection| {
            let subscriber = subscribers.get(&connection);
            let (own, subscriber) = match connection {
                // One peer connection: both reads saw the same transport.
                0 if one_connection => (publisher.as_ref().or(subscriber), None),
                0 => (publisher.as_ref(), subscriber),
                _ => (None, subscriber),
            };
            connection_stats(connection, own, subscriber)
        })
        .collect();
    raw
}

fn remote(
    remotes: &mut HashMap<(u8, String), RawRemote>,
    connection: u8,
    identity: String,
) -> &mut RawRemote {
    remotes
        .entry((connection, identity.clone()))
        .or_insert_with(|| RawRemote {
            connection,
            identity,
            audio: None,
            video: None,
        })
}

/// The numbers of one peer connection's transport and its selected pair of
/// addresses. The addresses themselves are never read.
#[derive(Clone, Debug, Default, PartialEq)]
struct Link {
    bytes_sent: u64,
    bytes_received: u64,
    outgoing_bits_per_second: Option<f64>,
    rtt_seconds: Option<f64>,
}

fn link(stats: &[RtcStats]) -> Option<Link> {
    let mut transports = stats.iter().filter_map(|stat| match stat {
        RtcStats::Transport(transport) => Some(transport),
        _ => None,
    });
    let transport = transports.next()?;
    let selected = stats.iter().find_map(|stat| match stat {
        RtcStats::CandidatePair(pair)
            if pair.rtc.id == transport.transport.selected_candidate_pair_id =>
        {
            Some(&pair.candidate_pair)
        }
        _ => None,
    });
    Some(Link {
        bytes_sent: transport.transport.bytes_sent,
        bytes_received: transport.transport.bytes_received,
        outgoing_bits_per_second: selected
            .map(|pair| pair.available_outgoing_bitrate)
            .filter(|rate| *rate > 0.0),
        rtt_seconds: selected
            .map(|pair| pair.current_round_trip_time)
            .filter(|rtt| *rtt > 0.0),
    })
}

/// The connection's numbers: what is sent and received in total on its
/// transports, the estimate of the outgoing bandwidth, and the round-trip
/// time. `None` when no transport was read.
fn connection_stats(
    connection: u8,
    publisher: Option<&Link>,
    subscriber: Option<&Link>,
) -> Option<RawConnection> {
    let links: Vec<&Link> = publisher.into_iter().chain(subscriber).collect();
    if links.is_empty() {
        return None;
    }
    Some(RawConnection {
        connection,
        outgoing_bits_per_second: links.iter().find_map(|link| link.outgoing_bits_per_second),
        bytes_sent: Some(links.iter().map(|link| link.bytes_sent).sum()),
        bytes_received: Some(links.iter().map(|link| link.bytes_received).sum()),
        rtt_seconds: links.iter().find_map(|link| link.rtt_seconds),
    })
}

fn own_audio(stats: &[RtcStats]) -> Option<RawOwnAudio> {
    let mut audio: Option<RawOwnAudio> = None;
    for stat in stats {
        if let RtcStats::OutboundRtp(outbound) = stat
            && outbound.stream.kind == "audio"
        {
            let audio = audio.get_or_insert_with(RawOwnAudio::default);
            audio.bytes_sent = audio.bytes_sent.saturating_add(outbound.sent.bytes_sent);
            audio.packets_sent = audio
                .packets_sent
                .saturating_add(outbound.sent.packets_sent);
        }
    }
    let audio = audio.as_mut()?;
    for stat in stats {
        match stat {
            RtcStats::MediaSource(source) if source.source.kind == "audio" => {
                audio.level = audio.level.max(source.audio.audio_level);
            }
            // What the far end reports of our stream.
            RtcStats::RemoteInboundRtp(far) if far.stream.kind == "audio" => {
                audio.packets_lost = Some(far.received.packets_lost);
                audio.jitter_seconds = Some(far.received.jitter);
                audio.rtt_seconds = Some(far.remote_inbound.round_trip_time);
            }
            _ => {}
        }
    }
    Some(audio.clone())
}

fn limit(reason: QualityLimitationReason) -> QualityLimit {
    match reason {
        QualityLimitationReason::None => QualityLimit::None,
        QualityLimitationReason::Cpu => QualityLimit::Cpu,
        QualityLimitationReason::Bandwidth => QualityLimit::Bandwidth,
        QualityLimitationReason::Other => QualityLimit::Other,
    }
}

/// The own video track: the bytes of every layer, and the size, frame rate
/// and limit of the largest one.
fn own_video(stats: &[RtcStats]) -> Option<RawOwnVideo> {
    let mut video: Option<RawOwnVideo> = None;
    let mut largest = 0_u64;
    for stat in stats {
        let RtcStats::OutboundRtp(outbound) = stat else {
            continue;
        };
        if outbound.stream.kind != "video" {
            continue;
        }
        let size =
            u64::from(outbound.outbound.frame_width) * u64::from(outbound.outbound.frame_height);
        match video.as_mut() {
            None => {
                largest = size;
                video = Some(RawOwnVideo {
                    fps: outbound.outbound.frames_per_second,
                    width: outbound.outbound.frame_width,
                    height: outbound.outbound.frame_height,
                    bytes_sent: outbound.sent.bytes_sent,
                    limit: limit(outbound.outbound.quality_limitation_reason),
                    packets_lost: None,
                });
            }
            Some(video) => {
                video.bytes_sent = video.bytes_sent.saturating_add(outbound.sent.bytes_sent);
                if size > largest {
                    largest = size;
                    video.fps = outbound.outbound.frames_per_second;
                    video.width = outbound.outbound.frame_width;
                    video.height = outbound.outbound.frame_height;
                    video.limit = limit(outbound.outbound.quality_limitation_reason);
                }
            }
        }
    }
    let video = video.as_mut()?;
    for stat in stats {
        if let RtcStats::RemoteInboundRtp(far) = stat
            && far.stream.kind == "video"
        {
            video.packets_lost = Some(
                video
                    .packets_lost
                    .unwrap_or(0)
                    .saturating_add(far.received.packets_lost.max(0)),
            );
        }
    }
    Some(video.clone())
}

fn remote_audio(stats: &[RtcStats]) -> Option<RawRemoteAudio> {
    let mut audio: Option<RawRemoteAudio> = None;
    for stat in stats {
        let RtcStats::InboundRtp(inbound) = stat else {
            continue;
        };
        if inbound.stream.kind != "audio" {
            continue;
        }
        let audio = audio.get_or_insert_with(RawRemoteAudio::default);
        audio.level = audio.level.max(inbound.inbound.audio_level);
        audio.bytes_received = audio
            .bytes_received
            .saturating_add(inbound.inbound.bytes_received);
        audio.packets_received = audio
            .packets_received
            .saturating_add(inbound.received.packets_received);
        audio.packets_lost = audio
            .packets_lost
            .saturating_add(inbound.received.packets_lost);
        audio.jitter_seconds = audio.jitter_seconds.max(inbound.received.jitter);
        audio.total_samples = audio
            .total_samples
            .saturating_add(inbound.inbound.total_samples_received);
        audio.concealed_samples = audio
            .concealed_samples
            .saturating_add(inbound.inbound.concealed_samples);
    }
    audio
}

fn remote_video(stats: &[RtcStats]) -> Option<RawRemoteVideo> {
    let mut video: Option<RawRemoteVideo> = None;
    for stat in stats {
        let RtcStats::InboundRtp(inbound) = stat else {
            continue;
        };
        if inbound.stream.kind != "video" {
            continue;
        }
        let video = video.get_or_insert_with(RawRemoteVideo::default);
        video.fps = video.fps.max(inbound.inbound.frames_per_second);
        video.width = video.width.max(inbound.inbound.frame_width);
        video.height = video.height.max(inbound.inbound.frame_height);
        video.bytes_received = video
            .bytes_received
            .saturating_add(inbound.inbound.bytes_received);
        video.frames_dropped = video
            .frames_dropped
            .saturating_add(u64::from(inbound.inbound.frames_dropped));
        video.packets_lost = video
            .packets_lost
            .saturating_add(inbound.received.packets_lost);
    }
    video
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> (StatsProbe, mpsc::Receiver<(u64, Option<RawStats>)>) {
        let (tx, rx) = mpsc::channel(1);
        (StatsProbe::new(tx), rx)
    }

    /// Starts a read at `now` and counts the calls of the source builder.
    fn tick(probe: &mut StatsProbe, now: Instant) -> usize {
        let mut built = 0;
        probe.tick(now, || {
            built += 1;
            Sources::default()
        });
        built
    }

    /// GIVEN the statistics are off WHEN a tick comes THEN nothing is built
    /// or read.
    #[tokio::test]
    async fn given_statistics_off_when_ticked_then_nothing_is_read() {
        let (mut probe, mut reports) = probe();
        assert_eq!(tick(&mut probe, Instant::now()), 0);
        assert!(reports.try_recv().is_err());
    }

    /// GIVEN the statistics are on WHEN ticks come THEN one read starts, no
    /// other starts while it runs or within a second, and the report frees
    /// the slot and gives one message.
    #[tokio::test]
    async fn given_statistics_on_when_ticked_then_one_read_runs_at_most_once_a_second() {
        let (mut probe, mut reports) = probe();
        probe.set(true);
        let start = Instant::now();
        assert_eq!(tick(&mut probe, start), 1);
        assert_eq!(tick(&mut probe, start), 0, "one read at a time");
        let (generation, raw) = reports.recv().await.unwrap();
        let message = probe.end(generation, raw);
        assert!(matches!(message, Some(FromWorker::Stats(_))));
        assert_eq!(
            tick(&mut probe, start + Duration::from_millis(900)),
            0,
            "less than a second since the last start"
        );
        assert_eq!(tick(&mut probe, start + STATS_INTERVAL), 1);
        probe.shutdown();
    }

    /// GIVEN a read that runs WHEN the statistics go off and on again THEN
    /// the old read's report gives no message and the slot stays taken until
    /// it reports, so two reads never run together.
    #[tokio::test]
    async fn given_a_switch_during_a_read_when_it_reports_then_the_old_report_is_dropped() {
        let (mut probe, mut reports) = probe();
        probe.set(true);
        let start = Instant::now();
        assert_eq!(tick(&mut probe, start), 1);
        probe.set(false);
        probe.set(true);
        assert_eq!(tick(&mut probe, start + STATS_INTERVAL), 0, "still reading");
        let (generation, raw) = reports.recv().await.unwrap();
        assert!(probe.end(generation, raw).is_none(), "an old series");
        assert_eq!(
            tick(&mut probe, start + STATS_INTERVAL),
            1,
            "a new one starts"
        );
        probe.shutdown();
    }

    /// GIVEN a read that did not finish WHEN it reports nothing THEN no
    /// message goes and the next read can start.
    #[tokio::test]
    async fn given_a_read_that_did_not_finish_then_no_message_and_the_slot_is_free() {
        let (mut probe, _reports) = probe();
        probe.set(true);
        let start = Instant::now();
        assert_eq!(tick(&mut probe, start), 1);
        assert!(probe.end(probe.generation, None).is_none());
        assert_eq!(tick(&mut probe, start + STATS_INTERVAL), 1);
        probe.shutdown();
    }

    /// GIVEN the call ends WHEN the probe shuts down THEN no read starts
    /// again, even when a tick comes.
    #[tokio::test]
    async fn given_shutdown_when_ticked_then_no_read_starts() {
        let (mut probe, _reports) = probe();
        probe.set(true);
        probe.shutdown();
        assert_eq!(tick(&mut probe, Instant::now()), 0);
    }

    /// GIVEN reads that hold no statistics WHEN the reading is assembled THEN
    /// a track with nothing to show has no entry and no connection is made
    /// up.
    #[test]
    fn given_a_connection_without_transports_when_assembled_then_it_has_no_entry() {
        let raw = assemble(5, false, std::iter::empty());
        assert_eq!(raw.at_ms, 5);
        assert!(raw.connections.is_empty() && raw.remotes.is_empty());
        let raw = assemble(
            6,
            false,
            [
                Read::Microphone(Vec::new()),
                Read::RemoteAudio(1, "x".into(), Vec::new()),
            ]
            .into_iter(),
        );
        assert!(raw.microphone.is_none(), "no outbound statistic, no entry");
        assert_eq!(raw.remotes.len(), 1);
        assert!(raw.remotes[0].audio.is_none());
        assert!(raw.connections.is_empty());
    }

    /// The statistics of a track whose transport sent and received the
    /// given bytes.
    fn on_transport(bytes_sent: u64, bytes_received: u64) -> Vec<RtcStats> {
        let mut transport = livekit::webrtc::stats::TransportStats::default();
        transport.transport.bytes_sent = bytes_sent;
        transport.transport.bytes_received = bytes_received;
        vec![RtcStats::Transport(transport)]
    }

    fn reads() -> impl Iterator<Item = Read> {
        [
            Read::Microphone(on_transport(100, 40)),
            Read::RemoteAudio(0, "a".into(), on_transport(101, 41)),
            Read::RemoteVideo(1, "b".into(), on_transport(7, 9)),
        ]
        .into_iter()
    }

    /// GIVEN the own room uses one peer connection WHEN the reading is
    /// assembled THEN its transport is counted once, from the own tracks,
    /// and another connection keeps its own numbers.
    #[test]
    fn given_one_peer_connection_when_assembled_then_its_transport_counts_once() {
        let raw = assemble(1, true, reads());
        let bytes: Vec<_> = raw
            .connections
            .iter()
            .map(|c| (c.connection, c.bytes_sent, c.bytes_received))
            .collect();
        assert_eq!(bytes, [(0, Some(100), Some(40)), (1, Some(7), Some(9))]);
    }

    /// GIVEN the own room uses a publisher and a subscriber connection WHEN
    /// the reading is assembled THEN both transports are added.
    #[test]
    fn given_two_peer_connections_when_assembled_then_both_transports_add() {
        let raw = assemble(1, false, reads());
        assert_eq!(raw.connections[0].bytes_sent, Some(201));
        assert_eq!(raw.connections[0].bytes_received, Some(81));
    }

    /// GIVEN one peer connection and no own track WHEN the reading is
    /// assembled THEN the remote tracks' transport still gives the numbers.
    #[test]
    fn given_one_peer_connection_without_own_tracks_then_the_remote_transport_counts() {
        let raw = assemble(
            1,
            true,
            [Read::RemoteAudio(0, "a".into(), on_transport(3, 5))].into_iter(),
        );
        assert_eq!(raw.connections[0].bytes_sent, Some(3));
        assert_eq!(raw.connections[0].bytes_received, Some(5));
    }
}

//! Reads the call's WebRTC statistics for the "technical details" panel
//! (ADR-0066).
//!
//! Like [`super::SoundProbe`] it runs one read at a time, on its own task and
//! under a deadline, so the event loop never waits for it. It reads only
//! while the core has turned the statistics on, and at most once a second.
//! The task gives back plain numbers (`RawStats`); `mapping::stats` makes
//! the message from them. Nothing here keeps a track handle, an address or a
//! name: only the numbers that the message carries, and each track's ID with
//! the time of its read, leave the parse functions. The ID keys the track's
//! counters in the worker and never leaves it.

use std::collections::{BTreeSet, HashMap};
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
    LinkRole, RawConnection, RawLink, RawOwnAudio, RawOwnVideo, RawRemote, RawRemoteAudio,
    RawRemoteVideo, RawStats, StatsBook,
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

/// Which track a read is of.
enum ReadKind {
    Microphone,
    Camera,
    Screen,
    RemoteAudio(u8, String),
    RemoteVideo(u8, String),
}

/// The reading of one track: its statistics (none when they could not be
/// read), its track ID, and the time the read finished on the probe's
/// monotonic clock, which is the time of its counters. libwebrtc may answer
/// from a report it made up to 50 ms before, and its answer reaches the
/// probe a little later, so a rate is off by those delays' change between
/// two reads: a few percent of a one-second interval, which the next
/// interval gives back. The records' own `timestamp` is not used: by default
/// it comes from the system clock, which can step (`media_config.h`), and a
/// step would give a wrong rate of any size.
struct Read {
    kind: ReadKind,
    track: String,
    at_ms: u64,
    stats: Vec<RtcStats>,
}

impl Read {
    fn new(kind: ReadKind, track: String, base: std::time::Instant, stats: Vec<RtcStats>) -> Self {
        Self {
            kind,
            track,
            at_ms: since(base),
            stats,
        }
    }
}

/// Milliseconds since `base`.
fn since(base: std::time::Instant) -> u64 {
    u64::try_from(base.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The transports a read needs: the own room's publisher when an own track
/// is read, and each connection's subscriber when one of its remote tracks
/// is read.
#[derive(Default)]
struct Asked {
    own: bool,
    remote: BTreeSet<u8>,
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
    /// series: the first reading has no rates. The time of the last start is
    /// kept, so turning them off and on never reads more than once a second.
    pub fn set(&mut self, on: bool) {
        if on == self.on {
            return;
        }
        self.on = on;
        self.generation += 1;
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
    let mut asked = Asked::default();
    let mut jobs: Vec<Job> = Vec::new();
    if let Some(track) = sources.microphone {
        asked.own = true;
        let sid = track.sid().to_string();
        jobs.push(Box::pin(async move {
            let stats = track.get_stats().await.unwrap_or_default();
            Read::new(ReadKind::Microphone, sid, base, stats)
        }));
    }
    for (kind, track) in [
        (ReadKind::Camera, sources.camera),
        (ReadKind::Screen, sources.screen),
    ] {
        let Some(track) = track else {
            continue;
        };
        asked.own = true;
        let sid = track.sid().to_string();
        jobs.push(Box::pin(async move {
            let stats = track.get_stats().await.unwrap_or_default();
            Read::new(kind, sid, base, stats)
        }));
    }
    for (connection, identity, track) in sources.remote_audio {
        asked.remote.insert(connection);
        let sid = track.sid().to_string();
        jobs.push(Box::pin(async move {
            let stats = track.get_stats().await.unwrap_or_default();
            Read::new(
                ReadKind::RemoteAudio(connection, identity),
                sid,
                base,
                stats,
            )
        }));
    }
    for (connection, identity, track) in sources.remote_video {
        asked.remote.insert(connection);
        let sid = track.sid().to_string();
        jobs.push(Box::pin(async move {
            let stats = track.get_stats().await.unwrap_or_default();
            Read::new(
                ReadKind::RemoteVideo(connection, identity),
                sid,
                base,
                stats,
            )
        }));
    }
    let reads = read_all(deadline, jobs).await.into_iter().flatten();
    assemble(&asked, one_connection, reads)
}

/// Makes the raw reading from what the reads gave. A track whose read did
/// not finish is left out. `one_connection` says that the own room's tracks
/// all use one transport (see [`Sources::one_connection`]). A connection
/// whose needed transport (see [`Asked`]) no read gave is not complete.
fn assemble(asked: &Asked, one_connection: bool, reads: impl Iterator<Item = Read>) -> RawStats {
    let mut raw = RawStats::default();
    // The first transport of the own tracks (the publisher) and of the
    // remote tracks of each connection (the subscriber). Each track's
    // statistics hold only its own streams and the transport they use.
    let mut publisher: Option<Link> = None;
    let mut subscribers: HashMap<u8, Link> = HashMap::new();
    let mut remotes: HashMap<(u8, String), RawRemote> = HashMap::new();
    for read in reads {
        let found = link(&read);
        match &read.kind {
            ReadKind::Microphone => {
                raw.microphone = own_audio(&read);
                publisher = publisher.or(found);
            }
            ReadKind::Camera => {
                raw.camera = own_video(&read);
                publisher = publisher.or(found);
            }
            ReadKind::Screen => {
                raw.screen = own_video(&read);
                publisher = publisher.or(found);
            }
            ReadKind::RemoteAudio(connection, identity) => {
                if let Some(found) = found {
                    subscribers.entry(*connection).or_insert(found);
                }
                remote(&mut remotes, *connection, identity).audio = remote_audio(&read);
            }
            ReadKind::RemoteVideo(connection, identity) => {
                if let Some(found) = found {
                    subscribers.entry(*connection).or_insert(found);
                }
                remote(&mut remotes, *connection, identity).video = remote_video(&read);
            }
        }
    }
    raw.remotes = remotes.into_values().collect();
    raw.remotes
        .sort_by(|a, b| (a.connection, &a.identity).cmp(&(b.connection, &b.identity)));
    raw.connections = (0..=MAX_OTHER_CONNECTIONS)
        .filter_map(|connection| {
            let subscriber = subscribers.get(&connection);
            let remote_asked = asked.remote.contains(&connection);
            let links: Vec<(LinkRole, Option<&Link>, bool)> = match connection {
                // One peer connection: every read of the room saw the same
                // transport, so any one of them gives it.
                0 if one_connection => vec![(
                    LinkRole::Single,
                    publisher.as_ref().or(subscriber),
                    asked.own || remote_asked,
                )],
                0 => vec![
                    (LinkRole::Publisher, publisher.as_ref(), asked.own),
                    (LinkRole::Subscriber, subscriber, remote_asked),
                ],
                _ => vec![(LinkRole::Subscriber, subscriber, remote_asked)],
            };
            connection_stats(connection, &links)
        })
        .collect();
    raw
}

fn remote<'a>(
    remotes: &'a mut HashMap<(u8, String), RawRemote>,
    connection: u8,
    identity: &str,
) -> &'a mut RawRemote {
    remotes
        .entry((connection, identity.to_owned()))
        .or_insert_with(|| RawRemote {
            connection,
            identity: identity.to_owned(),
            audio: None,
            video: None,
        })
}

/// The numbers of one peer connection's transport and its selected pair of
/// addresses, and the time of the read that gave them. The addresses
/// themselves are never read.
#[derive(Clone, Debug, Default, PartialEq)]
struct Link {
    at_ms: u64,
    bytes_sent: u64,
    bytes_received: u64,
    outgoing_bits_per_second: Option<f64>,
    rtt_seconds: Option<f64>,
}

fn link(read: &Read) -> Option<Link> {
    let stats = &read.stats;
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
        at_ms: read.at_ms,
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

/// The connection's numbers from its transports (`role`, the link a read
/// gave, whether a read needed it): each transport's bytes, the estimate of
/// the outgoing bandwidth, and the round-trip time. `None` when no
/// transport was read. Not complete when a needed transport is missing.
fn connection_stats(
    connection: u8,
    transports: &[(LinkRole, Option<&Link>, bool)],
) -> Option<RawConnection> {
    let read: Vec<(LinkRole, &Link)> = transports
        .iter()
        .filter_map(|(role, link, _)| link.map(|link| (*role, link)))
        .collect();
    if read.is_empty() {
        return None;
    }
    Some(RawConnection {
        connection,
        outgoing_bits_per_second: read
            .iter()
            .find_map(|(_, link)| link.outgoing_bits_per_second),
        rtt_seconds: read.iter().find_map(|(_, link)| link.rtt_seconds),
        links: read
            .iter()
            .map(|(role, link)| RawLink {
                role: *role,
                at_ms: link.at_ms,
                bytes_sent: link.bytes_sent,
                bytes_received: link.bytes_received,
            })
            .collect(),
        complete: transports
            .iter()
            .all(|(_, link, needed)| link.is_some() || !needed),
    })
}

fn own_audio(read: &Read) -> Option<RawOwnAudio> {
    let stats = &read.stats;
    let mut audio: Option<RawOwnAudio> = None;
    for stat in stats {
        if let RtcStats::OutboundRtp(outbound) = stat
            && outbound.stream.kind == "audio"
        {
            let audio = audio.get_or_insert_with(|| RawOwnAudio {
                track: read.track.clone(),
                at_ms: read.at_ms,
                ..RawOwnAudio::default()
            });
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
fn own_video(read: &Read) -> Option<RawOwnVideo> {
    let stats = &read.stats;
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
                    track: read.track.clone(),
                    at_ms: read.at_ms,
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

fn remote_audio(read: &Read) -> Option<RawRemoteAudio> {
    let mut audio: Option<RawRemoteAudio> = None;
    for stat in &read.stats {
        let RtcStats::InboundRtp(inbound) = stat else {
            continue;
        };
        if inbound.stream.kind != "audio" {
            continue;
        }
        let audio = audio.get_or_insert_with(|| RawRemoteAudio {
            track: read.track.clone(),
            at_ms: read.at_ms,
            ..RawRemoteAudio::default()
        });
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

fn remote_video(read: &Read) -> Option<RawRemoteVideo> {
    let mut video: Option<RawRemoteVideo> = None;
    for stat in &read.stats {
        let RtcStats::InboundRtp(inbound) = stat else {
            continue;
        };
        if inbound.stream.kind != "video" {
            continue;
        }
        let video = video.get_or_insert_with(|| RawRemoteVideo {
            track: read.track.clone(),
            at_ms: read.at_ms,
            ..RawRemoteVideo::default()
        });
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

    /// GIVEN a read that finished WHEN the statistics go off and on again
    /// within a second THEN no read starts before a second since the last
    /// start, and the next one has no rates.
    #[tokio::test]
    async fn given_a_quick_off_and_on_when_ticked_then_reads_stay_a_second_apart() {
        let (mut probe, mut reports) = probe();
        probe.set(true);
        let start = Instant::now();
        assert_eq!(tick(&mut probe, start), 1);
        let (generation, raw) = reports.recv().await.unwrap();
        assert!(probe.end(generation, raw).is_some());
        probe.set(false);
        probe.set(true);
        assert_eq!(
            tick(&mut probe, start + Duration::from_millis(100)),
            0,
            "the toggle keeps the time of the last start"
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
        let raw = assemble(&Asked::default(), false, std::iter::empty());
        assert!(raw.connections.is_empty() && raw.remotes.is_empty());
        let raw = assemble(
            &asked(true, &[1]),
            false,
            [
                read(ReadKind::Microphone, 6, Vec::new()),
                read(ReadKind::RemoteAudio(1, "x".into()), 6, Vec::new()),
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

    fn read(kind: ReadKind, at_ms: u64, stats: Vec<RtcStats>) -> Read {
        Read {
            kind,
            track: "TR".to_owned(),
            at_ms,
            stats,
        }
    }

    fn asked(own: bool, remote: &[u8]) -> Asked {
        Asked {
            own,
            remote: remote.iter().copied().collect(),
        }
    }

    fn reads() -> impl Iterator<Item = Read> {
        [
            read(ReadKind::Microphone, 10, on_transport(100, 40)),
            read(
                ReadKind::RemoteAudio(0, "a".into()),
                11,
                on_transport(101, 41),
            ),
            read(ReadKind::RemoteVideo(1, "b".into()), 12, on_transport(7, 9)),
        ]
        .into_iter()
    }

    /// A link as its role, time, bytes sent and bytes received.
    type LinkRow = (LinkRole, u64, u64, u64);

    /// Each connection's number, links, and whether it is complete.
    fn links(raw: &RawStats) -> Vec<(u8, Vec<LinkRow>, bool)> {
        raw.connections
            .iter()
            .map(|c| {
                let links = c
                    .links
                    .iter()
                    .map(|l| (l.role, l.at_ms, l.bytes_sent, l.bytes_received))
                    .collect();
                (c.connection, links, c.complete)
            })
            .collect()
    }

    /// GIVEN the own room uses one peer connection WHEN the reading is
    /// assembled THEN its transport is counted once, from the own tracks,
    /// and another connection keeps its own numbers.
    #[test]
    fn given_one_peer_connection_when_assembled_then_its_transport_counts_once() {
        let raw = assemble(&asked(true, &[0, 1]), true, reads());
        assert_eq!(
            links(&raw),
            [
                (0, vec![(LinkRole::Single, 10, 100, 40)], true),
                (1, vec![(LinkRole::Subscriber, 12, 7, 9)], true),
            ]
        );
    }

    /// GIVEN the own room uses a publisher and a subscriber connection WHEN
    /// the reading is assembled THEN both transports are there, each with
    /// the time of its own read.
    #[test]
    fn given_two_peer_connections_when_assembled_then_both_transports_are_there() {
        let raw = assemble(&asked(true, &[0, 1]), false, reads());
        assert_eq!(
            links(&raw)[0],
            (
                0,
                vec![
                    (LinkRole::Publisher, 10, 100, 40),
                    (LinkRole::Subscriber, 11, 101, 41)
                ],
                true
            )
        );
    }

    /// GIVEN one peer connection and no own track WHEN the reading is
    /// assembled THEN the remote tracks' transport still gives the numbers.
    #[test]
    fn given_one_peer_connection_without_own_tracks_then_the_remote_transport_counts() {
        let raw = assemble(
            &asked(false, &[0]),
            true,
            [read(
                ReadKind::RemoteAudio(0, "a".into()),
                3,
                on_transport(3, 5),
            )]
            .into_iter(),
        );
        assert_eq!(links(&raw), [(0, vec![(LinkRole::Single, 3, 3, 5)], true)]);
    }

    /// GIVEN two peer connections and an own track whose read did not finish
    /// WHEN the reading is assembled THEN the connection has the subscriber
    /// only and is not complete, so its totals are unknown, never short.
    #[test]
    fn given_a_missing_publisher_read_when_assembled_then_the_connection_is_not_complete() {
        let raw = assemble(
            &asked(true, &[0]),
            false,
            [read(
                ReadKind::RemoteAudio(0, "a".into()),
                3,
                on_transport(3, 5),
            )]
            .into_iter(),
        );
        assert_eq!(
            links(&raw),
            [(0, vec![(LinkRole::Subscriber, 3, 3, 5)], false)]
        );
    }

    /// GIVEN reads of tracks WHEN the reading is assembled THEN each track's
    /// numbers carry its track ID and the time of its own read.
    #[test]
    fn given_reads_when_assembled_then_each_track_has_its_id_and_time() {
        let mut outbound = livekit::webrtc::stats::OutboundRtpStats::default();
        outbound.stream.kind = "audio".to_owned();
        let mut inbound = livekit::webrtc::stats::InboundRtpStats::default();
        inbound.stream.kind = "video".to_owned();
        let raw = assemble(
            &asked(true, &[0]),
            true,
            [
                Read {
                    kind: ReadKind::Microphone,
                    track: "TR_mic".to_owned(),
                    at_ms: 20,
                    stats: vec![RtcStats::OutboundRtp(outbound)],
                },
                Read {
                    kind: ReadKind::RemoteVideo(0, "a".into()),
                    track: "TR_cam".to_owned(),
                    at_ms: 27,
                    stats: vec![RtcStats::InboundRtp(inbound)],
                },
            ]
            .into_iter(),
        );
        let microphone = raw.microphone.unwrap();
        assert_eq!(
            (microphone.track.as_str(), microphone.at_ms),
            ("TR_mic", 20)
        );
        let video = raw.remotes[0].video.as_ref().unwrap();
        assert_eq!((video.track.as_str(), video.at_ms), ("TR_cam", 27));
    }
}

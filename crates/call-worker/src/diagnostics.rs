//! The worker's records for the core's diagnostic log (ADR-0058).
//!
//! Off when the worker starts. Only `ToWorker::SetDiagnostics { on: true }`
//! starts it. While on, the `log` records of LiveKit and libwebrtc and the
//! worker's own notes of room events go to the core as
//! `FromWorker::Diagnostic`. Nothing goes to stderr or a file.
//!
//! Bounded: at most [`QUEUE_RECORDS`] wait, at most [`RECORDS_PER_SECOND`]
//! are taken each second, and the rest are counted and reported as one
//! record. Records wait apart from the control queue, and the writer takes
//! them only after every waiting control message, so they never delay one.
//! Control characters become spaces, and each string is cut to the protocol
//! limits. The core redacts every record again.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use persephone_call_protocol::{
    FromWorker, MAX_DIAGNOSTIC_LEVEL, MAX_DIAGNOSTIC_MESSAGE_BYTES, MAX_DIAGNOSTIC_TARGET_BYTES,
};
use tokio::sync::mpsc::WeakSender;

use crate::output::Item;

/// Records that wait for the writer.
pub const QUEUE_RECORDS: usize = 256;
/// Records taken in one second; more are counted and dropped.
pub const RECORDS_PER_SECOND: u32 = 200;
/// The levels on the wire.
pub const ERROR: u8 = 1;
pub const WARN: u8 = 2;
pub const INFO: u8 = 3;
pub const DEBUG: u8 = MAX_DIAGNOSTIC_LEVEL;

/// The rate limit's current second.
struct Window {
    start: Instant,
    taken: u32,
}

pub struct Diagnostics {
    on: AtomicBool,
    queue: Mutex<VecDeque<FromWorker>>,
    window: Mutex<Window>,
    dropped: AtomicU64,
    /// Wakes the writer; weak, so it never keeps the writer alive.
    wake: Mutex<Option<WeakSender<Item>>>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            on: AtomicBool::new(false),
            queue: Mutex::new(VecDeque::new()),
            window: Mutex::new(Window {
                start: Instant::now(),
                taken: 0,
            }),
            dropped: AtomicU64::new(0),
            wake: Mutex::new(None),
        }
    }
}

/// The worker's one record queue; the `log` logger writes here.
pub fn global() -> &'static Diagnostics {
    static DIAGNOSTICS: OnceLock<Diagnostics> = OnceLock::new();
    DIAGNOSTICS.get_or_init(Diagnostics::default)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Diagnostics {
    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// Starts or stops the records. Off drops the records that wait. The
    /// switch changes under the queue's lock, and `push_admitted` checks it
    /// under the same lock, so no record is added after an "off" cleared
    /// the queue.
    pub fn set_on(&self, on: bool) {
        let mut queue = lock(&self.queue);
        self.on.store(on, Ordering::SeqCst);
        if !on {
            queue.clear();
            self.dropped.store(0, Ordering::Relaxed);
        }
    }

    /// The writer to wake when a record waits.
    pub fn attach(&self, wake: WeakSender<Item>) {
        *lock(&self.wake) = Some(wake);
    }

    /// Queues one record, cleaned and cut, if the log is on and the second's
    /// budget and the queue have room. It never blocks for long: one short
    /// lock for the budget and one for the queue. The worker's own code asks
    /// [`Diagnostics::admit`] before it builds a record; the tests use this.
    #[cfg(test)]
    pub fn push(&self, level: u8, target: &str, message: &str) {
        if self.admit() {
            self.push_admitted(level, target, message);
        }
    }

    /// Whether one more record is taken now: the log is on, the second's
    /// budget has room (taken here) and the queue is not full. A record that
    /// is not taken is counted. The logger asks before it formats a record,
    /// so a flood costs a count, not the formatting.
    pub(crate) fn admit(&self) -> bool {
        if !self.is_on() {
            return false;
        }
        if !self.take_budget(Instant::now()) || lock(&self.queue).len() >= QUEUE_RECORDS {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Queues a record that [`Diagnostics::admit`] took.
    pub(crate) fn push_admitted(&self, level: u8, target: &str, message: &str) {
        let mut target = clean(target, MAX_DIAGNOSTIC_TARGET_BYTES);
        if target.is_empty() {
            target.push('?');
        }
        let record = FromWorker::Diagnostic {
            level: level.clamp(ERROR, DEBUG),
            target,
            message: clean(message, MAX_DIAGNOSTIC_MESSAGE_BYTES),
        };
        {
            let mut queue = lock(&self.queue);
            // Again under the lock: an "off" may have come since `admit`.
            if !self.is_on() {
                return;
            }
            if queue.len() >= QUEUE_RECORDS {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            queue.push_back(record);
        }
        let wake = lock(&self.wake).as_ref().and_then(WeakSender::upgrade);
        if let Some(queue) = wake {
            // A full queue means the writer is busy and looks here anyway.
            let _ = queue.try_send(Item::Wake);
        }
    }

    fn take_budget(&self, now: Instant) -> bool {
        let mut window = lock(&self.window);
        if now.duration_since(window.start) >= Duration::from_secs(1) {
            window.start = now;
            window.taken = 0;
        }
        if window.taken >= RECORDS_PER_SECOND {
            return false;
        }
        window.taken += 1;
        true
    }

    /// The next record for the writer: first a count of dropped records,
    /// if any, then the oldest waiting one.
    pub fn take(&self) -> Option<FromWorker> {
        if !self.is_on() {
            return None;
        }
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            return Some(FromWorker::Diagnostic {
                level: WARN,
                target: "persephone::call_worker".to_owned(),
                message: format!("{dropped} records dropped: over the limit"),
            });
        }
        lock(&self.queue).pop_front()
    }
}

/// `text` with control characters as spaces, cut to `limit` bytes on a
/// character boundary, with `…` when cut.
fn clean(text: &str, limit: usize) -> String {
    let mut out = String::with_capacity(text.len().min(limit));
    for character in text.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if out.len() + character.len_utf8() > limit {
            mark_cut(&mut out, limit);
            break;
        }
        out.push(character);
    }
    out
}

/// Marks `out`, which was cut, with `…`, within `limit` bytes: a reader can
/// tell a cut record from a whole one. The core marks its own cuts the same
/// way; the marker counts against the limit here, because the core refuses
/// a record over the protocol's limit.
fn mark_cut(out: &mut String, limit: usize) {
    const MARK: char = '…';
    while !out.is_empty() && out.len() + MARK.len_utf8() > limit {
        out.pop();
    }
    if out.len() + MARK.len_utf8() <= limit {
        out.push(MARK);
    }
}

/// `args` formatted into at most `limit` bytes, on a character boundary,
/// with `…` when cut. The formatting stops at the limit, so a huge value is
/// never built whole. A `Display` error of the value also ends with `…`:
/// its text is not whole either.
pub(crate) fn format_bounded(args: std::fmt::Arguments<'_>, limit: usize) -> String {
    struct Bounded {
        out: String,
        limit: usize,
    }
    impl std::fmt::Write for Bounded {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            let room = self.limit - self.out.len();
            if text.len() <= room {
                self.out.push_str(text);
                return Ok(());
            }
            let mut end = room;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.out.push_str(&text[..end]);
            // Stops the formatting: nothing more is wanted.
            Err(std::fmt::Error)
        }
    }
    let mut bounded = Bounded {
        out: String::new(),
        limit,
    };
    if std::fmt::write(&mut bounded, args).is_err() {
        mark_cut(&mut bounded.out, limit);
    }
    bounded.out
}

/// Transport crates under LiveKit that are too busy at `debug` (TLS and
/// socket handshakes on every connect): only `info` and above from them, so
/// they do not spend the budget the room event notes need. The core uses
/// the same list, from the call protocol.
use persephone_call_protocol::QUIET_LOG_TARGETS as QUIET_TARGETS;

/// Whether a record at `level` from `target` is taken: never `trace`, and
/// from a quiet target only `info` and above.
fn wanted(target: &str, level: log::Level) -> bool {
    let root = target.split("::").next().unwrap_or(target);
    level <= log::Level::Info || (level == log::Level::Debug && !QUIET_TARGETS.contains(&root))
}

/// LiveKit and libwebrtc log through `log`. Trace is never taken.
struct Logger;

static LOGGER: Logger = Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        global().is_on() && wanted(metadata.target(), metadata.level())
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) || !global().admit() {
            return;
        }
        let level = match record.level() {
            log::Level::Error => ERROR,
            log::Level::Warn => WARN,
            log::Level::Info => INFO,
            log::Level::Debug | log::Level::Trace => DEBUG,
        };
        let message = format_bounded(*record.args(), MAX_DIAGNOSTIC_MESSAGE_BYTES);
        global().push_admitted(level, record.target(), &message);
    }

    fn flush(&self) {}
}

/// Makes the queue the process's `log` logger. Records are made only while
/// the switch is on.
pub fn install() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN records from LiveKit, libwebrtc and transport crates THEN
    /// LiveKit's and libwebrtc's `debug` is taken, a transport crate's only
    /// from `info`, and `trace` never.
    #[test]
    fn given_targets_then_transport_noise_is_left_out() {
        assert!(wanted("livekit::rtc_engine", log::Level::Debug));
        assert!(wanted("libwebrtc", log::Level::Debug));
        assert!(!wanted("rustls::client::hs", log::Level::Debug));
        assert!(!wanted("tokio_tungstenite", log::Level::Debug));
        assert!(wanted("rustls", log::Level::Warn));
        assert!(wanted("hyper_util::client", log::Level::Info));
        assert!(!wanted("livekit", log::Level::Trace));
        assert!(!wanted("rustls", log::Level::Trace));
    }

    fn message(record: Option<FromWorker>) -> Option<(u8, String, String)> {
        match record {
            Some(FromWorker::Diagnostic {
                level,
                target,
                message,
            }) => Some((level, target, message)),
            _ => None,
        }
    }

    /// GIVEN the switch off WHEN records arrive THEN none waits; GIVEN on
    /// THEN they wait in order, cleaned and with a target.
    #[test]
    fn given_switch_then_records_follow_it() {
        let diagnostics = Diagnostics::default();
        diagnostics.push(INFO, "livekit", "before");
        assert!(diagnostics.take().is_none());
        diagnostics.set_on(true);
        diagnostics.push(INFO, "livekit", "line\nbreak\t!");
        diagnostics.push(0, "", "low level");
        assert_eq!(
            message(diagnostics.take()),
            Some((INFO, "livekit".into(), "line break !".into()))
        );
        assert_eq!(
            message(diagnostics.take()),
            Some((ERROR, "?".into(), "low level".into()))
        );
        assert!(diagnostics.take().is_none());
        diagnostics.push(INFO, "t", "waits");
        diagnostics.set_on(false);
        diagnostics.set_on(true);
        assert!(
            diagnostics.take().is_none(),
            "off dropped the waiting record"
        );
    }

    /// GIVEN more records in one second than the budget THEN the rest are
    /// counted and reported once, before the next record.
    #[test]
    fn given_flood_then_records_are_counted() {
        let diagnostics = Diagnostics::default();
        diagnostics.set_on(true);
        let extra = 5;
        for _ in 0..RECORDS_PER_SECOND + extra {
            diagnostics.push(DEBUG, "libwebrtc", "x");
        }
        let first = message(diagnostics.take()).unwrap();
        assert_eq!(first.0, WARN);
        assert!(
            first.2.starts_with(&format!("{extra} records dropped")),
            "{first:?}"
        );
        let mut taken = 0;
        while diagnostics.take().is_some() {
            taken += 1;
        }
        assert_eq!(taken, QUEUE_RECORDS.min(RECORDS_PER_SECOND as usize));
    }

    /// GIVEN long or multi-byte text THEN it is cut within the limit on a
    /// character boundary.
    #[test]
    fn given_long_text_then_cut() {
        assert_eq!(clean("ééé", 4), "…");
        assert_eq!(clean("aéé", 5), "aéé");
        assert_eq!(clean("aééé", 5), "a…");
        assert_eq!(clean("ééé", 6), "ééé");
        let long = clean(&"a".repeat(5000), MAX_DIAGNOSTIC_MESSAGE_BYTES);
        assert_eq!(long.len(), MAX_DIAGNOSTIC_MESSAGE_BYTES);
        assert!(long.ends_with('…'));
    }

    /// GIVEN a value whose text is far over the limit WHEN it is formatted
    /// THEN the formatting stops at the limit, on a character boundary.
    #[test]
    fn given_huge_value_then_formatting_stops_at_the_limit() {
        let value = vec!["é"; 10_000];
        let text = format_bounded(format_args!("{value:?}"), 101);
        assert!(text.len() <= 101 && text.len() > 90, "{}", text.len());
        assert!(text.ends_with('…'), "{text}");
        assert_eq!(format_bounded(format_args!("{}", 42), 101), "42");
    }

    /// GIVEN a record admitted while on WHEN the log is turned off before it
    /// is queued THEN it is not queued, and the next "on" starts empty.
    #[test]
    fn given_off_between_admit_and_push_then_the_record_is_dropped() {
        let diagnostics = Diagnostics::default();
        diagnostics.set_on(true);
        assert!(diagnostics.admit());
        diagnostics.set_on(false);
        diagnostics.push_admitted(INFO, "livekit", "before off");
        diagnostics.set_on(true);
        assert!(diagnostics.take().is_none());
    }

    /// GIVEN a spent budget WHEN a record is asked for THEN it is refused
    /// and counted before anything is formatted.
    #[test]
    fn given_spent_budget_then_admit_refuses_and_counts() {
        let diagnostics = Diagnostics::default();
        assert!(!diagnostics.admit(), "off");
        diagnostics.set_on(true);
        for _ in 0..RECORDS_PER_SECOND {
            assert!(diagnostics.admit());
        }
        assert!(!diagnostics.admit());
        assert_eq!(diagnostics.dropped.load(Ordering::Relaxed), 1);
    }
}

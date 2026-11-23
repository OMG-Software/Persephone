//! The UI host's records for the core's diagnostic log (ADR-0058).
//!
//! Off when the process starts; the core's `diagnostics_changed` event turns
//! it on and off. While on, `log` records go to the core in batches of at
//! most [`MAX_DIAGNOSTIC_RECORDS`]: this crate's own from `debug`, every
//! other crate's from `info`. At most [`QUEUE_RECORDS`] wait; more are
//! counted and reported as one record. The UI host writes no file, and the
//! core redacts every record again.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use persephone_core_protocol::{
    Call, DiagnosticRecord, MAX_DIAGNOSTIC_BATCH_BYTES, MAX_DIAGNOSTIC_MESSAGE_BYTES,
    MAX_DIAGNOSTIC_RECORDS, MAX_DIAGNOSTIC_TARGET_BYTES, RecordLevel, RecordSource,
};

use crate::core_link::CoreLink;

/// Records that wait for the sender thread.
pub const QUEUE_RECORDS: usize = 1024;
/// The pause between two batches, so at most four go out each second: four
/// of [`MAX_DIAGNOSTIC_RECORDS`] are the core's 256 records a second for the
/// UI host.
const BATCH_GAP: Duration = Duration::from_millis(250);

#[derive(Default)]
pub struct Forwarder {
    on: AtomicBool,
    queue: Mutex<VecDeque<DiagnosticRecord>>,
    dropped: AtomicU64,
    waiting: Condvar,
}

/// The process's one forwarder; the `log` logger writes here.
pub fn global() -> &'static Forwarder {
    static FORWARDER: OnceLock<Forwarder> = OnceLock::new();
    FORWARDER.get_or_init(Forwarder::default)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `text` cut to at most `limit` bytes on a character boundary, with `…`
/// when cut.
fn cut(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut out = prefix(text, limit).to_owned();
    mark_cut(&mut out, limit);
    out
}

/// The longest start of `text` within `limit` bytes, on a character
/// boundary.
fn prefix(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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
fn format_bounded(args: std::fmt::Arguments<'_>, limit: usize) -> String {
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
            self.out.push_str(prefix(text, room));
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

impl Forwarder {
    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// Follows the core's switch. Off drops the records that wait. The
    /// switch changes under the queue's lock, and `push` checks it under the
    /// same lock, so no record is added after an "off" cleared the queue.
    pub fn set_on(&self, on: bool) {
        let mut queue = lock(&self.queue);
        self.on.store(on, Ordering::SeqCst);
        follow_switch(std::ptr::eq(self, global()), on);
        if !on {
            queue.clear();
            self.dropped.store(0, Ordering::Relaxed);
        }
        drop(queue);
        self.waiting.notify_all();
    }

    /// Queues one record, cut to the protocol's limits, while on.
    pub fn push(&self, level: RecordLevel, target: &str, message: &str) {
        if !self.is_on() {
            return;
        }
        let record = DiagnosticRecord {
            level,
            target: cut(target, MAX_DIAGNOSTIC_TARGET_BYTES),
            message: cut(message, MAX_DIAGNOSTIC_MESSAGE_BYTES),
        };
        let mut queue = lock(&self.queue);
        // Again under the lock: an "off" may have come since the check above.
        if !self.is_on() {
            return;
        }
        if queue.len() >= QUEUE_RECORDS {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        queue.push_back(record);
        self.waiting.notify_one();
    }

    /// Counts a batch the core refused or never got, so the next batch
    /// reports it. Nothing is counted while off.
    pub fn count_refused(&self, records: u64) {
        let queue = lock(&self.queue);
        if self.is_on() {
            self.dropped.fetch_add(records, Ordering::Relaxed);
            self.waiting.notify_one();
        }
        drop(queue);
    }

    /// The next batch, at most [`MAX_DIAGNOSTIC_RECORDS`] and
    /// [`MAX_DIAGNOSTIC_BATCH_BYTES`] as JSON: a count of dropped records
    /// first, if any. Waits up to `wait` for a record while the queue is
    /// empty or the log is off; empty when off or when none came.
    #[cfg(test)]
    pub fn take_batch(&self, wait: Duration) -> Vec<DiagnosticRecord> {
        self.take_counted(wait).0
    }

    /// [`Forwarder::take_batch`], and how many records the batch stands
    /// for: the dropped ones its count names, and the others. A batch that
    /// is refused gives this number back to [`Forwarder::count_refused`], so
    /// the count never shrinks to the one record that carried it.
    pub fn take_counted(&self, wait: Duration) -> (Vec<DiagnosticRecord>, u64) {
        let mut queue = lock(&self.queue);
        if queue.is_empty() || !self.is_on() {
            queue = self
                .waiting
                .wait_timeout(queue, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if !self.is_on() {
            return (Vec::new(), 0);
        }
        let mut batch = Vec::new();
        // The brackets of the JSON array.
        let mut bytes = 2;
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let count = DiagnosticRecord {
                level: RecordLevel::Warn,
                target: "persephone_desktop".into(),
                message: format!("{dropped} records dropped: over the limit"),
            };
            bytes += count.encoded_len() + 1;
            batch.push(count);
        }
        while batch.len() < MAX_DIAGNOSTIC_RECORDS {
            let Some(size) = queue.front().map(DiagnosticRecord::encoded_len) else {
                break;
            };
            // One comma each, counted generously.
            if bytes + size + 1 > MAX_DIAGNOSTIC_BATCH_BYTES {
                break;
            }
            bytes += size + 1;
            batch.extend(queue.pop_front());
        }
        let others = batch.len() as u64 - u64::from(dropped > 0);
        (batch, dropped + others)
    }
}

/// Whether a record is kept: this crate's from `debug`, others' from `info`.
pub fn wanted(target: &str, level: log::Level) -> bool {
    let own = target == "persephone_desktop" || target.starts_with("persephone_desktop::");
    level
        <= if own {
            log::Level::Debug
        } else {
            log::Level::Info
        }
}

/// The `log` logger; it pushes to `forwarder()`: the process's, or a
/// test's own.
struct Logger {
    forwarder: fn() -> &'static Forwarder,
}

static LOGGER: Logger = Logger { forwarder: global };

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        (self.forwarder)().is_on() && wanted(metadata.target(), metadata.level())
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = match record.level() {
            log::Level::Error => RecordLevel::Error,
            log::Level::Warn => RecordLevel::Warn,
            log::Level::Info => RecordLevel::Info,
            log::Level::Debug | log::Level::Trace => RecordLevel::Debug,
        };
        let message = format_bounded(*record.args(), MAX_DIAGNOSTIC_MESSAGE_BYTES);
        (self.forwarder)().push(level, record.target(), &message);
    }

    fn flush(&self) {}
}

/// Makes the forwarder the `log` logger and starts the thread that sends
/// its batches to the core. Nothing is sent until the core turns it on.
pub fn install(core: CoreLink) {
    // Off until the core turns the log on (see `follow_switch`).
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Off);
    }
    let _ = std::thread::Builder::new()
        .name("diagnostics".into())
        .spawn(move || {
            loop {
                let (records, count) = global().take_counted(Duration::from_secs(1));
                if records.is_empty() {
                    continue;
                }
                let call = Call::DiagnosticRecords {
                    source: RecordSource::Ui,
                    records,
                };
                // Best effort, but counted: the next batch reports the loss.
                if core.call(call).is_err() {
                    global().count_refused(count);
                }
                std::thread::sleep(BATCH_GAP);
            }
        });
}

/// The `log` crate's level follows this process's switch: `Off` while off,
/// so a library's `log::debug!` costs one atomic read, not a call into the
/// logger (ADR-0058). Only the process's own instance sets it, not one a
/// test makes.
fn follow_switch(is_global: bool, on: bool) {
    if is_global {
        log::set_max_level(if on {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Off
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static FORWARDER: std::sync::OnceLock<Forwarder> = std::sync::OnceLock::new();
    fn forwarder() -> &'static Forwarder {
        FORWARDER.get_or_init(Forwarder::default)
    }

    /// GIVEN the logger on its own forwarder WHEN records arrive off, then on
    /// THEN nothing waits while off, and on keeps this crate's `debug`, other
    /// crates' `info` and maps `trace` to nothing.
    #[test]
    fn given_log_records_then_the_wanted_ones_wait() {
        use log::Log as _;
        let logger = Logger { forwarder };
        let send = |level: log::Level, target: &str, text: &str| {
            logger.log(
                &log::Record::builder()
                    .level(level)
                    .target(target)
                    .args(format_args!("{text}"))
                    .build(),
            );
        };
        send(log::Level::Error, "persephone_desktop", "while off");
        forwarder().set_on(true);
        send(
            log::Level::Debug,
            "persephone_desktop::core_link",
            "relayed",
        );
        send(log::Level::Debug, "tao", "busy");
        send(log::Level::Warn, "wry", "webview warning");
        send(log::Level::Trace, "persephone_desktop", "trace");
        logger.flush();
        let batch = forwarder().take_batch(Duration::ZERO);
        forwarder().set_on(false);
        let seen: Vec<(RecordLevel, &str)> = batch
            .iter()
            .map(|record| (record.level, record.message.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                (RecordLevel::Debug, "relayed"),
                (RecordLevel::Warn, "webview warning"),
            ]
        );
    }

    /// GIVEN the switch off WHEN records arrive THEN none waits; GIVEN on
    /// THEN they leave in batches of at most the protocol limit, each
    /// within its bounds.
    #[test]
    fn given_switch_then_batches_follow_it() {
        let forwarder = Forwarder::default();
        forwarder.push(RecordLevel::Info, "t", "off");
        assert!(forwarder.take_batch(Duration::ZERO).is_empty());
        forwarder.set_on(true);
        let long = "é".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES);
        for _ in 0..MAX_DIAGNOSTIC_RECORDS + 3 {
            forwarder.push(RecordLevel::Debug, &"x".repeat(100), &long);
        }
        let mut sent = 0;
        loop {
            let batch = forwarder.take_batch(Duration::ZERO);
            if batch.is_empty() {
                break;
            }
            assert!(DiagnosticRecord::batch_fits(&batch));
            sent += batch.len();
        }
        assert_eq!(sent, MAX_DIAGNOSTIC_RECORDS + 3);
        forwarder.push(RecordLevel::Info, "t", "waits");
        forwarder.set_on(false);
        forwarder.set_on(true);
        assert!(forwarder.take_batch(Duration::ZERO).is_empty());
    }

    /// GIVEN records of control characters, within their own limits but six
    /// bytes each as JSON WHEN batches are taken THEN each fits the call's
    /// byte limit, and none is lost.
    #[test]
    fn given_escaped_records_then_each_batch_fits() {
        let forwarder = Forwarder::default();
        forwarder.set_on(true);
        let control = "\u{1}".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES);
        for _ in 0..MAX_DIAGNOSTIC_RECORDS {
            forwarder.push(RecordLevel::Debug, "t", &control);
        }
        let mut sent = 0;
        loop {
            let batch = forwarder.take_batch(Duration::ZERO);
            if batch.is_empty() {
                break;
            }
            assert!(batch.len() < MAX_DIAGNOSTIC_RECORDS);
            assert!(DiagnosticRecord::batch_fits(&batch));
            sent += batch.len();
        }
        assert_eq!(sent, MAX_DIAGNOSTIC_RECORDS);
    }

    /// GIVEN a refused batch WHEN it is counted THEN the next batch reports
    /// it; GIVEN the log off THEN nothing is counted.
    #[test]
    fn given_refused_batch_then_the_next_batch_reports_it() {
        let forwarder = Forwarder::default();
        forwarder.count_refused(5);
        forwarder.set_on(true);
        assert!(forwarder.take_batch(Duration::ZERO).is_empty());
        forwarder.count_refused(3);
        let batch = forwarder.take_batch(Duration::ZERO);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].message, "3 records dropped: over the limit");
    }

    /// GIVEN a count of 10 dropped records WHEN the batches that carry it
    /// are refused again and again THEN the count stays 10, and records
    /// lost with it are added, never the count record itself.
    #[test]
    fn given_refused_count_batches_then_the_count_never_shrinks() {
        let forwarder = Forwarder::default();
        forwarder.set_on(true);
        forwarder.count_refused(10);
        for _ in 0..3 {
            let (batch, lost) = forwarder.take_counted(Duration::ZERO);
            assert_eq!(batch[0].message, "10 records dropped: over the limit");
            assert_eq!(lost, 10);
            forwarder.count_refused(lost);
        }
        forwarder.push(RecordLevel::Info, "t", "kept");
        let (batch, lost) = forwarder.take_counted(Duration::ZERO);
        assert_eq!(batch.len(), 2);
        assert_eq!(lost, 11);
        forwarder.count_refused(lost);
        let (batch, _) = forwarder.take_counted(Duration::ZERO);
        assert_eq!(batch[0].message, "11 records dropped: over the limit");
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
        let long = cut(&"é".repeat(600), MAX_DIAGNOSTIC_MESSAGE_BYTES);
        assert!(long.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES && long.ends_with('…'));
        assert_eq!(cut("whole", 5), "whole");
    }

    /// GIVEN a full queue WHEN more arrive THEN they are counted and the
    /// count leads the next batch.
    #[test]
    fn given_full_queue_then_drops_are_counted() {
        let forwarder = Forwarder::default();
        forwarder.set_on(true);
        for _ in 0..QUEUE_RECORDS + 2 {
            forwarder.push(RecordLevel::Info, "t", "m");
        }
        let batch = forwarder.take_batch(Duration::ZERO);
        assert_eq!(batch[0].message, "2 records dropped: over the limit");
        assert_eq!(batch.len(), MAX_DIAGNOSTIC_RECORDS);
    }

    /// GIVEN targets THEN this crate's debug records are kept and other
    /// crates' only from info.
    #[test]
    fn given_targets_then_levels_differ() {
        assert!(wanted("persephone_desktop::core_link", log::Level::Debug));
        assert!(!wanted("wry::webview", log::Level::Debug));
        assert!(wanted("wry::webview", log::Level::Info));
        assert!(!wanted("persephone_desktop", log::Level::Trace));
    }
}

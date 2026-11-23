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
    Call, DiagnosticRecord, MAX_DIAGNOSTIC_MESSAGE_BYTES, MAX_DIAGNOSTIC_RECORDS,
    MAX_DIAGNOSTIC_TARGET_BYTES, RecordLevel, RecordSource,
};

use crate::core_link::CoreLink;

/// Records that wait for the sender thread.
pub const QUEUE_RECORDS: usize = 1024;
/// The pause between two batches, so at most five go out each second.
const BATCH_GAP: Duration = Duration::from_millis(200);

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

/// `text` cut to at most `limit` bytes on a character boundary.
fn cut(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

impl Forwarder {
    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// Follows the core's switch. Off drops the records that wait.
    pub fn set_on(&self, on: bool) {
        self.on.store(on, Ordering::SeqCst);
        if !on {
            lock(&self.queue).clear();
            self.dropped.store(0, Ordering::Relaxed);
        }
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
        if queue.len() >= QUEUE_RECORDS {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        queue.push_back(record);
        self.waiting.notify_one();
    }

    /// The next batch, at most [`MAX_DIAGNOSTIC_RECORDS`]: a count of dropped
    /// records first, if any. Waits up to `wait` for a record; empty when
    /// off or when none came.
    pub fn take_batch(&self, wait: Duration) -> Vec<DiagnosticRecord> {
        let mut queue = lock(&self.queue);
        if queue.is_empty() {
            queue = self
                .waiting
                .wait_timeout(queue, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if !self.is_on() {
            return Vec::new();
        }
        let mut batch = Vec::new();
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            batch.push(DiagnosticRecord {
                level: RecordLevel::Warn,
                target: "persephone_desktop".into(),
                message: format!("{dropped} records dropped: over the limit"),
            });
        }
        while batch.len() < MAX_DIAGNOSTIC_RECORDS {
            let Some(record) = queue.pop_front() else {
                break;
            };
            batch.push(record);
        }
        batch
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
        (self.forwarder)().push(level, record.target(), &record.args().to_string());
    }

    fn flush(&self) {}
}

/// Makes the forwarder the `log` logger and starts the thread that sends
/// its batches to the core. Nothing is sent until the core turns it on.
pub fn install(core: CoreLink) {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
    let _ = std::thread::Builder::new()
        .name("diagnostics".into())
        .spawn(move || {
            loop {
                let records = global().take_batch(Duration::from_secs(1));
                if records.is_empty() {
                    continue;
                }
                let call = Call::DiagnosticRecords {
                    source: RecordSource::Ui,
                    records,
                };
                // A refused or failed batch is dropped: the log is best effort.
                let _ = core.call(call);
                std::thread::sleep(BATCH_GAP);
            }
        });
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
        let first = forwarder.take_batch(Duration::ZERO);
        assert_eq!(first.len(), MAX_DIAGNOSTIC_RECORDS);
        assert!(first.iter().all(DiagnosticRecord::within_bounds));
        assert_eq!(forwarder.take_batch(Duration::ZERO).len(), 3);
        forwarder.push(RecordLevel::Info, "t", "waits");
        forwarder.set_on(false);
        forwarder.set_on(true);
        assert!(forwarder.take_batch(Duration::ZERO).is_empty());
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

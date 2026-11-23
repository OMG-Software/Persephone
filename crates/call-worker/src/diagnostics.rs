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

    /// Starts or stops the records. Off drops the records that wait.
    pub fn set_on(&self, on: bool) {
        self.on.store(on, Ordering::SeqCst);
        if !on {
            lock(&self.queue).clear();
            self.dropped.store(0, Ordering::Relaxed);
        }
    }

    /// The writer to wake when a record waits.
    pub fn attach(&self, wake: WeakSender<Item>) {
        *lock(&self.wake) = Some(wake);
    }

    /// Queues one record, cleaned and cut, if the log is on and the second's
    /// budget and the queue have room. It never blocks for long: one short
    /// lock for the budget and one for the queue.
    pub fn push(&self, level: u8, target: &str, message: &str) {
        if !self.is_on() {
            return;
        }
        if !self.take_budget(Instant::now()) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
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
/// character boundary.
fn clean(text: &str, limit: usize) -> String {
    let mut out = String::with_capacity(text.len().min(limit));
    for character in text.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if out.len() + character.len_utf8() > limit {
            break;
        }
        out.push(character);
    }
    out
}

/// LiveKit and libwebrtc log through `log`. Trace is never taken.
struct Logger;

static LOGGER: Logger = Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        global().is_on() && metadata.level() <= log::Level::Debug
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = match record.level() {
            log::Level::Error => ERROR,
            log::Level::Warn => WARN,
            log::Level::Info => INFO,
            log::Level::Debug | log::Level::Trace => DEBUG,
        };
        global().push(level, record.target(), &record.args().to_string());
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
        assert_eq!(clean("ééé", 3), "é");
        assert_eq!(
            clean(&"a".repeat(5000), MAX_DIAGNOSTIC_MESSAGE_BYTES).len(),
            2048
        );
    }
}

//! The opt-in diagnostic log (ADR-0058).
//!
//! * Off at every start. Only [`Log::turn_on`], after the user said yes in
//!   the core's own window, starts it. Nothing stores the choice.
//! * One file, `diagnostics/persephone.jsonl` in the profile's directory, one
//!   JSON object for each line. At start, [`erase`] writes random bytes over
//!   an old file, flushes, truncates and removes it. This is best effort: an
//!   SSD or a copy-on-write file system can keep the old blocks.
//! * Every string goes through [`redact`] before it is written.
//! * Bounded: a queue of [`QUEUE_LINES`] lines, [`MAX_LINE_BYTES`] for each
//!   line and [`MAX_FILE_BYTES`] for the file, and
//!   [`MAX_RECORDS_PER_SECOND`] from each other process. A full queue or a
//!   rate over the limit drops records before any work is done on them, and
//!   counts them; the next line, or the end of the file, reports the count.
//!
//! The core's own `tracing` events and `log` records come in through
//! [`install`]; the other processes send theirs to the core, which calls
//! [`Log::record`] with their [`Source`].

use serde::Serialize;
use serde_json::{Map, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub mod events;
pub mod redact;

/// The log's folder in the profile's directory.
pub const DIR_NAME: &str = "diagnostics";
/// The log's file name.
pub const FILE_NAME: &str = "persephone.jsonl";
/// The file stops growing here; one `log_full` line marks the end.
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// One line at most, after redaction.
pub const MAX_LINE_BYTES: usize = 16 * 1024;
/// One string at most, after redaction.
pub const MAX_STRING_BYTES: usize = 4 * 1024;
/// Lines waiting for the writer thread.
pub const QUEUE_LINES: usize = 4096;
/// Records each second from each other process (the UI host, the page and
/// the call worker), counted in the core whatever they promise.
pub const MAX_RECORDS_PER_SECOND: u32 = 256;
/// The overwrite is written in pieces of this size.
const ERASE_CHUNK: usize = 64 * 1024;

/// The process that reported a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Core,
    Ui,
    Page,
    CallWorker,
}

/// How much a record matters, the same five steps as `tracing` and `log`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// The log's path in a profile's directory.
pub fn path_in(profile_dir: &Path) -> PathBuf {
    profile_dir.join(DIR_NAME).join(FILE_NAME)
}

/// The process's one log. The `tracing` subscriber and the `log` logger
/// write here.
pub fn global() -> &'static Log {
    static LOG: OnceLock<Log> = OnceLock::new();
    LOG.get_or_init(Log::default)
}

/// State shared with the writer thread.
#[derive(Default)]
struct Shared {
    /// The next line's number in this run.
    seq: AtomicU64,
    /// Lines lost to a full queue or a rate limit since the last written
    /// line.
    dropped: AtomicU64,
    /// Lines sent to the writer and not yet taken by it.
    queued: AtomicUsize,
    /// The file reached [`MAX_FILE_BYTES`] in this run.
    full: AtomicBool,
}

struct Writer {
    queue: SyncSender<String>,
    thread: JoinHandle<()>,
}

/// A diagnostic log. Off until [`Log::turn_on`].
pub struct Log {
    on: AtomicBool,
    shared: Arc<Shared>,
    writer: Mutex<Option<Writer>>,
    /// Held for the whole of [`Log::turn_on`] and [`Log::turn_off`], so an
    /// "on" waits until an "off" has drained the old writer: two writers on
    /// one file could split each other's lines. Never taken by `record`.
    lifecycle: Mutex<()>,
    /// Goes up with each "off" request (see [`Log::epoch`]).
    epoch: AtomicU64,
    /// Each other process's records in the current second.
    rates: Mutex<[Window; 3]>,
    /// The file of this run, once it was turned on.
    path: Mutex<Option<PathBuf>>,
    /// The switch for processes that send their records to the core: a
    /// call task tells its worker (ADR-0058).
    switch: tokio::sync::watch::Sender<bool>,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            on: AtomicBool::new(false),
            shared: Arc::default(),
            writer: Mutex::new(None),
            lifecycle: Mutex::new(()),
            epoch: AtomicU64::new(0),
            rates: Mutex::default(),
            path: Mutex::new(None),
            switch: tokio::sync::watch::channel(false).0,
        }
    }
}

impl Log {
    /// Follows the switch: the newest value only.
    pub fn watch(&self) -> tokio::sync::watch::Receiver<bool> {
        self.switch.subscribe()
    }

    /// Whether the user has the log on. A full file keeps it on, but writes
    /// nothing more (see [`Log::is_full`]).
    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// Whether this run's file reached [`MAX_FILE_BYTES`]. It stays full
    /// until the next start, which deletes the file; turning the log off and
    /// on again does not reset it.
    pub fn is_full(&self) -> bool {
        self.shared.full.load(Ordering::Relaxed)
    }

    /// The file this run wrote to, if the log was ever on in this run. The
    /// file stays until the next start, also after the log is turned off.
    pub fn path(&self) -> Option<PathBuf> {
        lock(&self.path).clone()
    }

    /// Opens `path` for appending (owner-only on Unix, never through a
    /// link) and starts writing. A second call while on does nothing.
    pub fn turn_on(&self, path: &Path) -> io::Result<()> {
        self.turn_on_unless_off_since(path, self.epoch())
            .map(|_| ())
    }

    /// Counts the "off" requests of this run. Code that asks the user before
    /// turning the log on reads it before asking, and passes it to
    /// [`Log::turn_on_unless_off_since`].
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Like [`Log::turn_on`], but does nothing and returns `false` when the
    /// log was turned off (or asked to be off) after `epoch` was read: an
    /// "off" sent while the question was open wins over the "yes".
    pub fn turn_on_unless_off_since(&self, path: &Path, epoch: u64) -> io::Result<bool> {
        let _lifecycle = lock(&self.lifecycle);
        if self.epoch() != epoch {
            return Ok(false);
        }
        let mut writer = lock(&self.writer);
        if writer.is_some() {
            return Ok(true);
        }
        let file = open_for_append(path)?;
        let written = file.metadata()?.len();
        let (queue, lines) = mpsc::sync_channel(QUEUE_LINES);
        let shared = self.shared.clone();
        let thread = std::thread::Builder::new()
            .name("diagnostics".into())
            .spawn(move || write_lines(file, written, &lines, &shared))?;
        *writer = Some(Writer { queue, thread });
        *lock(&self.path) = Some(path.to_owned());
        self.on.store(true, Ordering::SeqCst);
        drop(writer);
        self.switch.send_replace(true);
        self.record(
            Source::Core,
            Level::Info,
            "persephone::diagnostics",
            "diagnostic log on",
            &[("version", Value::from(env!("CARGO_PKG_VERSION")))],
        );
        Ok(true)
    }

    /// Stops writing. Lines in the queue are written and flushed first, so
    /// this can take a while: the core runs it off its serving loop. The
    /// file stays until the next start. It also cancels an "on" that is
    /// still waiting for the user (see [`Log::epoch`]), even when the log
    /// is already off.
    pub fn turn_off(&self) {
        let _lifecycle = lock(&self.lifecycle);
        self.epoch.fetch_add(1, Ordering::SeqCst);
        if !self.on.load(Ordering::SeqCst) {
            return;
        }
        self.record(
            Source::Core,
            Level::Info,
            "persephone::diagnostics",
            "diagnostic log off",
            &[],
        );
        self.on.store(false, Ordering::SeqCst);
        self.switch.send_replace(false);
        let writer = lock(&self.writer).take();
        if let Some(Writer { queue, thread }) = writer {
            drop(queue);
            let _ = thread.join();
        }
    }

    /// Writes one record, if the log is on. Every string is redacted and
    /// cut to [`MAX_STRING_BYTES`]; a field with a secret's name is written
    /// as `[redacted]`.
    pub fn record(
        &self,
        source: Source,
        level: Level,
        target: &str,
        message: &str,
        fields: &[(&str, Value)],
    ) {
        // Checked before the record is redacted and built, so a flood or a
        // slow writer costs a count, not the work of a line nobody keeps.
        if !self.accepts() {
            return;
        }
        if !self.admit(source) {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let seq = self.shared.seq.fetch_add(1, Ordering::Relaxed);
        let line = line(seq, source, level, target, message, fields);
        // Counted before it is sent, so the writer's count never goes below
        // zero.
        self.shared.queued.fetch_add(1, Ordering::Relaxed);
        let sent = match &*lock(&self.writer) {
            Some(writer) => writer.queue.try_send(line),
            None => Err(TrySendError::Disconnected(String::new())),
        };
        if let Err(error) = sent {
            self.shared.queued.fetch_sub(1, Ordering::Relaxed);
            if let TrySendError::Full(_) = error {
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Whether a record would be kept now: the log is on, its file is not
    /// full and the writer's queue has room. Code that gathers a record's
    /// fields (events, `tracing`, `log`) asks first, so a dropped record
    /// costs no work. A full queue counts the record as dropped here.
    pub fn accepts(&self) -> bool {
        if !self.is_on() || self.is_full() {
            return false;
        }
        if self.shared.queued.load(Ordering::Relaxed) >= QUEUE_LINES {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Whether one more record from `source` fits its rate: the core's own
    /// always does; each other process gets at most
    /// [`MAX_RECORDS_PER_SECOND`], whatever it promises to send.
    fn admit(&self, source: Source) -> bool {
        let index = match source {
            Source::Core => return true,
            Source::Ui => 0,
            Source::Page => 1,
            Source::CallWorker => 2,
        };
        let mut rates = lock(&self.rates);
        let window = &mut rates[index];
        let now = Instant::now();
        if window
            .start
            .is_none_or(|start| now.duration_since(start) >= Duration::from_secs(1))
        {
            *window = Window {
                start: Some(now),
                count: 0,
            };
        }
        if window.count >= MAX_RECORDS_PER_SECOND {
            return false;
        }
        window.count += 1;
        true
    }
}

/// One second of records from one process.
#[derive(Clone, Copy, Default)]
struct Window {
    start: Option<Instant>,
    count: u32,
}

/// One JSON line, without its line break.
fn line(
    seq: u64,
    source: Source,
    level: Level,
    target: &str,
    message: &str,
    fields: &[(&str, Value)],
) -> String {
    let mut object = Map::new();
    object.insert("ts".into(), Value::from(now_ms()));
    object.insert("seq".into(), Value::from(seq));
    object.insert(
        "source".into(),
        serde_json::to_value(source).unwrap_or(Value::Null),
    );
    object.insert(
        "level".into(),
        serde_json::to_value(level).unwrap_or(Value::Null),
    );
    object.insert("target".into(), Value::from(redact::text(target, 256)));
    object.insert(
        "message".into(),
        Value::from(redact::text(message, MAX_STRING_BYTES)),
    );
    if !fields.is_empty() {
        let mut map = Map::new();
        for (name, value) in fields {
            let name = redact::text(name, 128);
            let value = if redact::is_secret_name(&name) {
                Value::from(redact::REDACTED)
            } else {
                redact::value(value, 0)
            };
            map.insert(name, value);
        }
        object.insert("fields".into(), Value::Object(map));
    }
    let text = |object: &Map<String, Value>| serde_json::to_string(object).unwrap_or_default();
    let mut written = text(&object);
    if written.len() > MAX_LINE_BYTES {
        // Too long even with each string cut: the fields go.
        object.remove("fields");
        object.insert("truncated".into(), Value::Bool(true));
        written = text(&object);
    }
    if written.len() > MAX_LINE_BYTES {
        // Still too long: JSON escaping can make a control character six
        // bytes (`\u0001`). A message of an eighth of the line fits even
        // then.
        object.insert(
            "message".into(),
            Value::from(redact::text(message, MAX_LINE_BYTES / 8)),
        );
        written = text(&object);
    }
    written
}

/// The longest start of `text` that is at most `limit` bytes, on a
/// character boundary. Nothing is copied.
pub fn prefix(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `args` formatted into at most `limit` bytes, with `…` when cut. The
/// formatting stops at the limit, so a huge `Debug` value is never built
/// whole.
pub fn format_bounded(args: std::fmt::Arguments<'_>, limit: usize) -> String {
    struct Bounded {
        out: String,
        limit: usize,
        cut: bool,
    }
    impl std::fmt::Write for Bounded {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            let room = self.limit - self.out.len();
            if text.len() <= room {
                self.out.push_str(text);
                Ok(())
            } else {
                self.out.push_str(prefix(text, room));
                self.cut = true;
                // Stops the formatting: nothing more is wanted.
                Err(std::fmt::Error)
            }
        }
    }
    let mut bounded = Bounded {
        out: String::new(),
        limit,
        cut: false,
    };
    let _ = std::fmt::write(&mut bounded, args);
    if bounded.cut {
        bounded.out.push('…');
    }
    bounded.out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| {
            u64::try_from(time.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The writer thread: writes queued lines until the queue closes, flushing
/// whenever the queue is empty.
fn write_lines(file: File, mut written: u64, lines: &Receiver<String>, shared: &Shared) {
    let mut out = BufWriter::new(file);
    let put = |out: &mut BufWriter<File>, text: &str, written: &mut u64| -> bool {
        if shared.full.load(Ordering::Relaxed) {
            return false;
        }
        // Room is kept for the closing `log_full` line.
        let reserve = 256;
        if *written + text.len() as u64 + 1 + reserve > MAX_FILE_BYTES {
            shared.full.store(true, Ordering::Relaxed);
            let seq = shared.seq.fetch_add(1, Ordering::Relaxed);
            let end = line(
                seq,
                Source::Core,
                Level::Warn,
                "persephone::diagnostics",
                "log_full",
                &[],
            );
            let _ = writeln!(out, "{end}");
            return false;
        }
        if writeln!(out, "{text}").is_err() {
            return false;
        }
        *written += text.len() as u64 + 1;
        true
    };
    // Writes the count of records dropped since the last line, if any.
    let note_dropped = |out: &mut BufWriter<File>, written: &mut u64| {
        let dropped = shared.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let seq = shared.seq.fetch_add(1, Ordering::Relaxed);
            let note = line(
                seq,
                Source::Core,
                Level::Warn,
                "persephone::diagnostics",
                "records dropped: over a limit",
                &[("count", Value::from(dropped))],
            );
            put(out, &note, written);
        }
    };
    while let Ok(first) = lines.recv() {
        let mut next = Some(first);
        while let Some(text) = next {
            shared.queued.fetch_sub(1, Ordering::Relaxed);
            note_dropped(&mut out, &mut written);
            put(&mut out, &text, &mut written);
            next = lines.try_recv().ok();
        }
        let _ = out.flush();
    }
    // Records dropped after the last line, such as a closing line that did
    // not fit the queue: the file still ends with their count.
    note_dropped(&mut out, &mut written);
    let _ = out.flush();
    let _ = out.get_ref().sync_all();
}

/// Opens the log for appending: creates its folder (owner-only on Unix) and
/// the file (owner-only on Unix); refuses a link or anything that is not a
/// plain file.
fn open_for_append(path: &Path) -> io::Result<File> {
    let folder = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(folder)?;
    if fs::symlink_metadata(folder)?.file_type().is_symlink() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    no_follow(&mut options);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    plain_file(&file)?;
    Ok(file)
}

#[cfg(unix)]
fn no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(windows)]
fn no_follow(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(unix, windows)))]
fn no_follow(_: &mut OpenOptions) {}

/// The open file is a plain file, not a link, folder or device.
fn plain_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
    }
    if !metadata.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(())
}

/// Deletes an old log at start: random bytes over its whole length, a flush
/// to disk, truncation, a second flush, then removal. A link at the path is
/// removed itself; what it points at is not touched. Returns whether
/// anything was there. Best effort: see the module notes.
pub fn erase(path: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() {
        if metadata.file_type().is_dir() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        fs::remove_file(path)?;
        return Ok(true);
    }
    let overwritten = overwrite(path);
    // Removed also when the overwrite failed: the file must not stay.
    fs::remove_file(path)?;
    overwritten.map(|()| true)
}

fn overwrite(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true);
    no_follow(&mut options);
    let mut file = options.open(path)?;
    plain_file(&file)?;
    let length = file.metadata()?.len();
    let mut chunk = vec![0u8; ERASE_CHUNK];
    let mut left = length;
    while left > 0 {
        let size = usize::try_from(left.min(ERASE_CHUNK as u64)).unwrap_or(ERASE_CHUNK);
        getrandom::fill(&mut chunk[..size]).map_err(|_| io::Error::other("no random bytes"))?;
        file.write_all(&chunk[..size])?;
        left -= size as u64;
    }
    file.sync_all()?;
    file.set_len(0)?;
    file.sync_all()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// --- The core's own `tracing` events and `log` records ------------------------

/// Targets that are too busy at `debug`: only `info` and above from them.
const QUIET_TARGETS: &[&str] = &[
    "h2",
    "hyper",
    "hyper_util",
    "mio",
    "reqwest",
    "rustls",
    "tokio",
    "tokio_util",
    "tungstenite",
    "tokio_tungstenite",
    "want",
];

/// Whether a record at `level` from `target` is kept.
pub fn wanted(target: &str, level: Level) -> bool {
    if level == Level::Trace {
        return false;
    }
    let root = target.split("::").next().unwrap_or(target);
    level <= Level::Info || !QUIET_TARGETS.contains(&root)
}

thread_local! {
    /// Set while this thread writes a record, so a record made while
    /// recording is dropped rather than recursing.
    static RECORDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn guarded(write: impl FnOnce()) {
    let entered = RECORDING.with(|flag| !flag.replace(true));
    if entered {
        write();
        RECORDING.with(|flag| flag.set(false));
    }
}

/// Makes the log the process's `tracing` subscriber and `log` logger. Call
/// once, early in `main`. Nothing is written until the log is turned on.
pub fn install() {
    let _ = tracing::subscriber::set_global_default(Subscriber::new(global));
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
}

fn from_tracing(level: tracing::Level) -> Level {
    match level {
        tracing::Level::ERROR => Level::Error,
        tracing::Level::WARN => Level::Warn,
        tracing::Level::INFO => Level::Info,
        tracing::Level::DEBUG => Level::Debug,
        tracing::Level::TRACE => Level::Trace,
    }
}

/// From a `log` level, also used for the other processes' records.
pub fn from_log(level: log::Level) -> Level {
    match level {
        log::Level::Error => Level::Error,
        log::Level::Warn => Level::Warn,
        log::Level::Info => Level::Info,
        log::Level::Debug => Level::Debug,
        log::Level::Trace => Level::Trace,
    }
}

/// A minimal `tracing` subscriber: events only. Spans get an ID, so code
/// that enters spans works, but their fields are not kept. It writes to
/// `log()`: the process's log, or a test's own.
struct Subscriber {
    log: fn() -> &'static Log,
    next_span: AtomicU64,
}

impl Subscriber {
    fn new(log: fn() -> &'static Log) -> Self {
        Self {
            log,
            next_span: AtomicU64::new(0),
        }
    }
}

impl tracing::Subscriber for Subscriber {
    fn register_callsite(
        &self,
        metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        if wanted(metadata.target(), from_tracing(*metadata.level())) {
            // Asked again each time: the switch can change at any moment.
            tracing::subscriber::Interest::sometimes()
        } else {
            tracing::subscriber::Interest::never()
        }
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::DEBUG)
    }

    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        (self.log)().is_on() && wanted(metadata.target(), from_tracing(*metadata.level()))
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        guarded(|| {
            // Asked before the fields are gathered: a dropped record costs
            // no formatting.
            if !(self.log)().accepts() {
                return;
            }
            let metadata = event.metadata();
            let mut fields = Fields::default();
            event.record(&mut fields);
            let named: Vec<(&str, Value)> = fields
                .named
                .iter()
                .map(|(name, value)| (*name, value.clone()))
                .collect();
            (self.log)().record(
                Source::Core,
                from_tracing(*metadata.level()),
                metadata.target(),
                &fields.message,
                &named,
            );
        });
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// An event's `message` and its other fields.
#[derive(Default)]
struct Fields {
    message: String,
    named: Vec<(&'static str, Value)>,
}

/// At most this many fields of one event are kept.
const MAX_FIELDS: usize = 32;

impl Fields {
    fn put(&mut self, field: &tracing::field::Field, value: Value) {
        if field.name() == "message" {
            self.message = match value {
                Value::String(text) => text,
                other => other.to_string(),
            };
        } else if self.named.len() < MAX_FIELDS {
            self.named.push((field.name(), value));
        }
    }
}

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let text = format_bounded(format_args!("{value:?}"), MAX_STRING_BYTES);
        self.put(field, Value::String(text));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        let mut text = prefix(value, MAX_STRING_BYTES).to_owned();
        if text.len() < value.len() {
            text.push('…');
        }
        self.put(field, Value::String(text));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.put(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.put(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.put(field, Value::Bool(value));
    }
}

/// Cuts `text` to at most `limit` bytes, on a character boundary.
pub fn cut(text: &mut String, limit: usize) {
    if text.len() > limit {
        let end = prefix(text, limit).len();
        text.truncate(end);
        text.push('…');
    }
}

/// The `log` logger; it writes to `log()`, like [`Subscriber`].
struct Logger {
    log: fn() -> &'static Log,
}

static LOGGER: Logger = Logger { log: global };

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        (self.log)().is_on() && wanted(metadata.target(), from_log(metadata.level()))
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) || !(self.log)().accepts() {
            return;
        }
        guarded(|| {
            let message = format_bounded(*record.args(), MAX_STRING_BYTES);
            (self.log)().record(
                Source::Core,
                from_log(record.level()),
                record.target(),
                &message,
                &[],
            );
        });
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// GIVEN a new log WHEN nothing turned it on THEN it records nothing and
    /// names no file.
    #[test]
    fn given_new_log_then_it_is_off() {
        let log = Log::default();
        assert!(!log.is_on());
        log.record(Source::Core, Level::Error, "t", "m", &[]);
        assert_eq!(log.path(), None);
    }

    /// GIVEN a log turned on WHEN records arrive and it is turned off THEN
    /// each is one JSON line with its source, level and target, and the file
    /// stays.
    #[test]
    fn given_log_on_when_recorded_then_lines_are_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let log = Log::default();
        log.turn_on(&path).unwrap();
        assert!(log.is_on());
        log.record(
            Source::Page,
            Level::Debug,
            "bridge",
            "room_list ok",
            &[("ms", Value::from(3))],
        );
        log.turn_off();
        assert!(!log.is_on());
        log.record(Source::Core, Level::Error, "t", "after off", &[]);
        let lines = lines(&path);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0]["message"], "diagnostic log on");
        assert_eq!(lines[1]["source"], "page");
        assert_eq!(lines[1]["level"], "debug");
        assert_eq!(lines[1]["target"], "bridge");
        assert_eq!(lines[1]["fields"]["ms"], 3);
        assert_eq!(lines[2]["message"], "diagnostic log off");
        assert!(lines[0]["seq"].as_u64() < lines[1]["seq"].as_u64());
        assert_eq!(log.path(), Some(path));
    }

    /// GIVEN a log turned off and on again in one run THEN the second part is
    /// added to the same file.
    #[test]
    fn given_log_on_again_then_it_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let log = Log::default();
        log.turn_on(&path).unwrap();
        log.turn_off();
        log.turn_on(&path).unwrap();
        log.turn_off();
        assert_eq!(lines(&path).len(), 4);
    }

    /// GIVEN an "on" that read the epoch before asking WHEN an "off" comes
    /// while it waits, with the log already off THEN the "on" does nothing;
    /// a fresh epoch turns it on.
    #[test]
    fn given_off_while_asking_then_the_on_is_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let log = Log::default();
        let asked_at = log.epoch();
        log.turn_off();
        assert!(!log.turn_on_unless_off_since(&path, asked_at).unwrap());
        assert!(!log.is_on() && !path.exists());
        assert!(log.turn_on_unless_off_since(&path, log.epoch()).unwrap());
        assert!(log.is_on());
        log.turn_off();
    }

    /// GIVEN a log whose file is full WHEN records arrive and it is turned
    /// off and on again THEN it still reports on and full, and writes nothing
    /// more in this run.
    #[test]
    fn given_full_file_then_log_stays_on_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let log = Log::default();
        log.turn_on(&path).unwrap();
        // Off joins the writer, so the flag is set after every line is out.
        log.turn_off();
        log.shared.full.store(true, Ordering::Relaxed);
        log.turn_on(&path).unwrap();
        log.record(Source::Core, Level::Error, "t", "after full", &[]);
        assert!(log.is_on() && log.is_full());
        log.turn_off();
        assert!(!log.is_on() && log.is_full());
        let lines = lines(&path);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0]["message"], "diagnostic log on");
        assert_eq!(lines[1]["message"], "diagnostic log off");
    }

    /// GIVEN secrets in the message and in fields WHEN recorded THEN none of
    /// them reaches the file.
    #[test]
    fn given_secrets_when_recorded_then_file_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let log = Log::default();
        log.turn_on(&path).unwrap();
        log.record(
            Source::CallWorker,
            Level::Info,
            "livekit",
            "connecting to wss://sfu.example/rtc?access_token=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.c2ln&auto_subscribe=1",
            &[
                ("password", Value::from("hunter2")),
                ("request", Value::from(r#"{"access_token":"syt_YWxpY2U_abcdefghijk_0Z1y2X"}"#)),
                ("state_key", Value::from("_@alice:example.org_DEVICE")),
            ],
        );
        log.turn_off();
        let text = fs::read_to_string(&path).unwrap();
        for secret in ["hunter2", "eyJhbGciOiJIUzI1NiJ9", "syt_YWxpY2U", "c2ln&"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert!(text.contains("auto_subscribe=1"));
        assert!(text.contains("_@alice:example.org_DEVICE"));
    }

    /// GIVEN a record far over the line limit THEN its line stays within it.
    #[test]
    fn given_huge_record_then_line_is_bounded() {
        let big = "x".repeat(MAX_LINE_BYTES * 2);
        let names: Vec<String> = (0..8).map(|index| format!("f{index}")).collect();
        let fields: Vec<(&str, Value)> = names
            .iter()
            .map(|name| (name.as_str(), Value::from(big.as_str())))
            .collect();
        let text = line(1, Source::Core, Level::Info, "t", &big, &fields);
        assert!(text.len() <= MAX_LINE_BYTES, "{}", text.len());
        let line: Value = serde_json::from_str(&text).unwrap();
        assert!(line.get("fields").is_none(), "the fields go first");
        assert_eq!(line["target"], "t");
    }

    /// GIVEN a message of control characters, which JSON writes as six
    /// bytes each WHEN the line is built THEN it still fits the limit.
    #[test]
    fn given_escaped_message_then_line_is_bounded() {
        let message = "\u{1}".repeat(MAX_STRING_BYTES);
        let text = line(1, Source::Core, Level::Info, "t", &message, &[]);
        assert!(text.len() <= MAX_LINE_BYTES, "{}", text.len());
        assert!(serde_json::from_str::<Value>(&text).is_ok());
    }

    /// GIVEN a value whose text is far over the limit WHEN it is formatted
    /// bounded THEN the result stops at the limit, on a character boundary.
    #[test]
    fn given_huge_debug_value_then_formatting_stops_at_the_limit() {
        let value = vec!["é"; 10_000];
        let text = format_bounded(format_args!("{value:?}"), 101);
        assert!(text.len() <= 101 + '…'.len_utf8(), "{}", text.len());
        assert!(text.ends_with('…'));
        assert_eq!(format_bounded(format_args!("{}", 42), 101), "42");
    }

    /// GIVEN a writer near the size limit with dropped records counted WHEN
    /// lines arrive THEN the count is written, then one `log_full` line, and
    /// nothing after it.
    #[test]
    fn given_writer_near_the_limit_then_it_ends_with_log_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let file = File::create(&path).unwrap();
        let shared = Shared::default();
        shared.dropped.store(3, Ordering::Relaxed);
        let (queue, lines_in) = mpsc::sync_channel(4);
        queue.send(r#"{"n":1}"#.to_owned()).unwrap();
        queue.send("x".repeat(400)).unwrap();
        queue.send(r#"{"n":3}"#.to_owned()).unwrap();
        drop(queue);
        // Room for the dropped note and the first line only.
        write_lines(file, MAX_FILE_BYTES - 700, &lines_in, &shared);
        assert!(shared.full.load(Ordering::Relaxed));
        let lines = lines(&path);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0]["message"], "records dropped: over a limit");
        assert_eq!(lines[0]["fields"]["count"], 3);
        assert_eq!(lines[1]["n"], 1);
        assert_eq!(lines[2]["message"], "log_full");
    }

    /// GIVEN dropped records and no line after them WHEN the writer stops
    /// THEN the file still ends with their count.
    #[test]
    fn given_drops_after_the_last_line_then_the_file_ends_with_the_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let shared = Shared::default();
        shared.dropped.store(2, Ordering::Relaxed);
        let (queue, lines_in) = mpsc::sync_channel::<String>(1);
        drop(queue);
        write_lines(File::create(&path).unwrap(), 0, &lines_in, &shared);
        let lines = lines(&path);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["fields"]["count"], 2);
    }

    /// GIVEN another process sending faster than its rate WHEN its records
    /// arrive THEN at most the rate is written, the rest are counted, and the
    /// core's own records still get through.
    #[test]
    fn given_a_flood_from_the_page_then_its_rate_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let log = Log::default();
        log.turn_on(&path).unwrap();
        let flood = MAX_RECORDS_PER_SECOND as usize + 44;
        for _ in 0..flood {
            log.record(Source::Page, Level::Debug, "page::bridge", "flood", &[]);
        }
        log.record(Source::Core, Level::Warn, "persephone::test", "own", &[]);
        log.turn_off();
        let lines = lines(&path);
        let page = lines.iter().filter(|line| line["source"] == "page").count();
        // The window may roll over once on a slow machine.
        assert!(
            (MAX_RECORDS_PER_SECOND as usize..=flood).contains(&page),
            "{page}"
        );
        let counted: u64 = lines
            .iter()
            .filter(|line| line["message"] == "records dropped: over a limit")
            .map(|line| line["fields"]["count"].as_u64().unwrap())
            .sum();
        assert_eq!(page + counted as usize, flood);
        assert!(lines.iter().any(|line| line["message"] == "own"));
    }

    /// GIVEN a full writer queue WHEN a record arrives THEN it is counted,
    /// not built, and the count is written once there is room.
    #[test]
    fn given_a_full_queue_then_the_record_is_only_counted() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let log = Log::default();
        log.turn_on(&path).unwrap();
        // Wait until the writer took the "on" line, so the count stays put.
        while log.shared.queued.load(Ordering::Relaxed) != 0 {
            std::thread::yield_now();
        }
        log.shared.queued.store(QUEUE_LINES, Ordering::Relaxed);
        log.record(Source::Core, Level::Error, "t", "skipped", &[]);
        assert_eq!(log.shared.dropped.load(Ordering::Relaxed), 1);
        log.shared.queued.store(0, Ordering::Relaxed);
        log.turn_off();
        let messages: Vec<Value> = lines(&path)
            .into_iter()
            .map(|line| line["message"].clone())
            .collect();
        assert_eq!(
            messages,
            [
                "diagnostic log on",
                "records dropped: over a limit",
                "diagnostic log off"
            ]
        );
    }

    static EVENTS_LOG: OnceLock<Log> = OnceLock::new();
    fn events_log() -> &'static Log {
        EVENTS_LOG.get_or_init(Log::default)
    }

    /// GIVEN the subscriber on its own log WHEN events of each field type,
    /// a quiet target and a span arrive THEN kept events are lines with their
    /// fields, the secret field is redacted, and the quiet `debug` is not.
    #[test]
    fn given_tracing_events_then_they_become_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let log = events_log();
        log.turn_on(&path).unwrap();
        tracing::subscriber::with_default(Subscriber::new(events_log), || {
            let span = tracing::info_span!("sync");
            let _entered = span.enter();
            tracing::info!(
                target: "persephone::test",
                count = 3u64,
                offset = -2i64,
                flag = true,
                name = "room",
                list = ?[1, 2],
                "hello {}",
                1
            );
            tracing::debug!(target: "hyper::proto", "quiet");
            tracing::warn!(target: "persephone::test", password = "hunter2", "secret field");
        });
        log.turn_off();
        let lines = lines(&path);
        let events: Vec<&Value> = lines
            .iter()
            .filter(|line| line["target"] != "persephone::diagnostics")
            .collect();
        assert_eq!(events.len(), 2, "{lines:?}");
        assert_eq!(events[0]["message"], "hello 1");
        assert_eq!(events[0]["level"], "info");
        assert_eq!(events[0]["fields"]["count"], 3);
        assert_eq!(events[0]["fields"]["offset"], -2);
        assert_eq!(events[0]["fields"]["flag"], true);
        assert_eq!(events[0]["fields"]["name"], "room");
        assert_eq!(events[0]["fields"]["list"], "[1, 2]");
        assert_eq!(events[1]["fields"]["password"], redact::REDACTED);
        assert!(!fs::read_to_string(&path).unwrap().contains("hunter2"));
    }

    static RECORDS_LOG: OnceLock<Log> = OnceLock::new();
    fn records_log() -> &'static Log {
        RECORDS_LOG.get_or_init(Log::default)
    }

    /// GIVEN the logger on its own log WHEN it is off, then on THEN nothing
    /// is kept while off, and on keeps a library's `debug` but not a quiet
    /// target's `debug` or any `trace`.
    #[test]
    fn given_log_records_then_the_wanted_ones_become_lines() {
        use log::Log as _;
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let logger = Logger { log: records_log };
        let send = |level: log::Level, target: &str, text: &str| {
            logger.log(
                &log::Record::builder()
                    .level(level)
                    .target(target)
                    .args(format_args!("{text}"))
                    .build(),
            );
        };
        send(log::Level::Error, "livekit", "while off");
        records_log().turn_on(&path).unwrap();
        send(log::Level::Debug, "libwebrtc", "ice candidate");
        send(log::Level::Debug, "rustls", "quiet");
        send(log::Level::Trace, "livekit", "trace");
        logger.flush();
        records_log().turn_off();
        let messages: Vec<String> = lines(&path)
            .iter()
            .map(|line| line["message"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(
            messages,
            ["diagnostic log on", "ice candidate", "diagnostic log off"]
        );
    }

    /// GIVEN an old log WHEN erased THEN it is gone; GIVEN none THEN nothing
    /// happens.
    #[test]
    fn given_old_log_when_erased_then_it_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        assert!(!erase(&path).unwrap());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'a'; ERASE_CHUNK * 2 + 17]).unwrap();
        assert!(erase(&path).unwrap());
        assert!(!path.exists());
    }

    /// GIVEN a link at the log's path WHEN erased THEN only the link goes;
    /// the file it pointed at keeps its content.
    #[cfg(unix)]
    #[test]
    fn given_link_when_erased_then_target_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target = dir.path().join("other");
        fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(erase(&path).unwrap());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert!(fs::symlink_metadata(&path).is_err());
    }

    /// GIVEN a link at the log's path WHEN turned on THEN it is refused.
    #[cfg(unix)]
    #[test]
    fn given_link_when_turned_on_then_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target = dir.path().join("other");
        fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(Log::default().turn_on(&path).is_err());
    }

    /// GIVEN busy network crates THEN only their `info` and above is kept;
    /// GIVEN any target THEN `trace` is never kept.
    #[test]
    fn given_targets_then_levels_are_filtered() {
        assert!(wanted("matrix_sdk::sync", Level::Debug));
        assert!(wanted("libwebrtc", Level::Debug));
        assert!(!wanted("hyper_util::client", Level::Debug));
        assert!(wanted("hyper_util::client", Level::Info));
        assert!(!wanted("matrix_sdk", Level::Trace));
    }

    /// GIVEN multi-byte text WHEN cut THEN it ends on a character boundary.
    #[test]
    fn given_multibyte_when_cut_then_valid() {
        let mut text = "ééé".to_owned();
        cut(&mut text, 3);
        assert_eq!(text, "é…");
    }
}

//! Images: received images loaded on request, and images picked to send
//! (ADR-0009; `docs/architecture/image-messaging.md`).
//!
//! * A received `m.image` is projected as a timeline item without fetching
//!   anything ([`timeline_item`]). When the user asks, [`fetch`] downloads it
//!   through the authenticated media API, or the legacy one when the
//!   homeserver lacks it, chosen as the SDK chooses. The SDK's own download
//!   buffers the whole body without a limit, so the core makes the request
//!   itself, with the session's access token and the core's HTTP policy
//!   (`homeserver.rs`). At most [`MAX_ENCODED_BYTES`] are accepted: a larger
//!   declared size or `Content-Length` is refused before the body is read,
//!   and the body is read in chunks into a buffer reserved up front, which
//!   never grows. An encrypted file is decrypted in place with the SDK's
//!   [`AttachmentDecryptor`], which checks its SHA-256 hash; nothing here
//!   implements cryptography.
//! * The encoded bytes go to the contained worker ([`ImageWorker`]), one run
//!   at a time. The core applies the EXIF orientation to the validated RGBA
//!   pixels and scales them down by area averaging to fit
//!   [`MAX_SHOWN_SIDE`] square, in one pass, and keeps them under an opaque
//!   random handle in the `RetainedPixels` budget, evicting the least
//!   recently used image when the budget or [`MAX_HANDLES`] is full.
//! * The page fetches the pixels in bands of at most `MAX_BAND_BYTES`
//!   ([`MediaService::band`]). Each handle belongs to the session generation
//!   that made it: a lock, a sign-out or a new UI connection drops them all
//!   ([`MediaService::purge`]), and a handle from another generation is
//!   `not_found`.
//! * A picked file is stripped by the worker, then the stripped copy is
//!   decoded again for the preview, so the user sees what will be sent. The
//!   draft keeps the stripped bytes ([`ImageUpload`]) until it is sent or
//!   discarded; at most [`MAX_DRAFTS`] exist.
//! * Admission never queues without bound: at most [`MAX_LOADS`] loads and
//!   [`MAX_PICKS`] picks run at once, and a call beyond that is refused with
//!   `limit_exceeded`. Admitted work shares one worker slot, so it waits for
//!   at most that many runs.

use crate::views::project_name;
use matrix_sdk::{
    Client, HttpError, RoomState,
    deserialized_responses::{TimelineEvent, TimelineEventKind},
    ruma::{
        EventId, MxcUri, RoomId, UInt,
        api::{
            Metadata, SupportedVersions,
            client::{authenticated_media, media},
            path_builder::PathBuilder,
        },
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
            room::{
                MediaSource,
                message::{ImageMessageEventContent, MessageType},
            },
        },
    },
};
use matrix_sdk_crypto::{AttachmentDecryptor, MediaEncryptionInfo};
use persephone_contracts::{ImageMime, ImagePick, ImageView, TimelineItem, UnixMillis};
use persephone_core_protocol::{ErrorCode, IMAGE_HANDLE_BYTES, MAX_BAND_BYTES};
use persephone_media_protocol::{
    BudgetClass, BudgetPermit, FrameKind, MAX_DIMENSION, MIB, RGBA_BYTES_PER_PIXEL, ReceivedFrame,
    ResourceBudgets,
};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    fmt::Display,
    io::Read,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use zeroize::Zeroizing;

/// Largest encoded image, downloaded or picked (the media plan's limit).
pub const MAX_ENCODED_BYTES: u64 = 20 * MIB;
/// Copies of an image or file alive at the peak of one send, counted in its
/// length (matrix-sdk 0.19.1 `Room::send_attachment`): the outbox's; the
/// SDK's input, kept for the whole call; the upload buffer, which the
/// request keeps for a retry after a token refresh (a clone in a room that
/// is not encrypted, or the encrypted copy, whose capacity can reach twice
/// the length as it grows); the clone of the request that is sent; and the
/// HTTP body serialized from it. Five in a room that is not encrypted, six
/// in one that is.
pub const SENT_COPIES: u64 = 6;
/// Largest image (after stripping) or file sent, so that every copy of a
/// send fits the 40 MiB `EncodedInFlight` budget. Downloads and saved files
/// make no such copies and keep [`MAX_ENCODED_BYTES`].
pub const MAX_SENT_BYTES: u64 = 6 * MIB;
const _: () =
    assert!(SENT_COPIES * MAX_SENT_BYTES <= persephone_media_protocol::ENCODED_IN_FLIGHT_LIMIT);
/// Received and preview images are scaled down to fit this square; 2048 x
/// 2048 RGBA is 16 MiB, so four fit the `RetainedPixels` budget.
pub const MAX_SHOWN_SIDE: u32 = 2048;
/// Most `load_image` calls in progress at once (downloading, decrypting or
/// decoding). Two 20 MiB downloads fill the 40 MiB `EncodedInFlight` budget.
pub const MAX_LOADS: usize = 2;
/// Most `pick_image` calls in progress at once (dialog, read and strip).
pub const MAX_PICKS: usize = 1;
/// Most picked images kept as drafts.
pub const MAX_DRAFTS: usize = 1;
/// Most image handles held at once, drafts included; the least recently used
/// loaded image is dropped to make room.
pub const MAX_HANDLES: usize = 32;
/// Longest a download may take, from request to the last byte. A lock waits
/// for a download in progress (it holds a lease), but the download checks for
/// a lock every `DOWNLOAD_LOCK_POLL` while it waits for the response headers
/// or the next chunk, and stops (`locked`) once the lock began, even when the
/// server sends nothing.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// How often a download in progress checks whether a lock began. Much shorter
/// than the lock's wait for leases, so a stalled server cannot make a lock
/// time out.
pub const DOWNLOAD_LOCK_POLL: Duration = Duration::from_millis(250);
/// Memory limit of one worker run (the media plan's default): the Job's
/// committed-memory limit on Windows, `RLIMIT_AS` on Linux; not enforced on
/// macOS, where the decoder guest's memory cap is the bound.
pub const WORKER_MEMORY_BYTES: usize = 256 * 1024 * 1024;
/// Wall and CPU limit of one decode. Provisional: recorded benchmarks show 5
/// to 8 s for a 16-megapixel decode in the worker.
pub const DECODE_TIME_LIMIT: Duration = Duration::from_secs(20);
/// Wall and CPU limit of one strip, which decodes twice.
pub const STRIP_TIME_LIMIT: Duration = Duration::from_secs(40);
/// The worker executable, next to the core's own.
pub const WORKER_EXE: &str = if cfg!(windows) {
    "persephone-media-worker.exe"
} else {
    "persephone-media-worker"
};
/// The AppContainer profile the worker runs in on Windows; Linux and macOS
/// have no named profile and ignore it.
pub const WORKER_PROFILE: &str = "Persephone.MediaWorker";
/// Bytes decrypted per step; the decrypted bytes overwrite the encrypted
/// ones in the same buffer.
const DECRYPT_CHUNK_BYTES: usize = 64 * 1024;
/// Longest media ID accepted from an event.
const MAX_MEDIA_ID_BYTES: usize = 255;

fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Encoded image bytes in a buffer reserved, in the `EncodedInFlight`
/// budget, before it is allocated. The buffer never grows past the
/// reservation and is zeroed when dropped.
pub struct Encoded {
    bytes: Zeroizing<Vec<u8>>,
    limit: usize,
    _permit: BudgetPermit,
}

impl Encoded {
    /// Reserves and allocates room for `limit` bytes: `rejected` above
    /// [`MAX_ENCODED_BYTES`] or for nothing, `limit_exceeded` when the budget
    /// is full.
    pub fn reserve(budgets: &ResourceBudgets, limit: u64) -> Result<Self, ErrorCode> {
        if limit == 0 || limit > MAX_ENCODED_BYTES {
            return Err(ErrorCode::Rejected);
        }
        let permit = budgets
            .try_acquire(BudgetClass::EncodedInFlight, limit)
            .map_err(|_| ErrorCode::LimitExceeded)?;
        let limit = usize::try_from(limit).map_err(|_| ErrorCode::Rejected)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(limit)
            .map_err(|_| ErrorCode::LimitExceeded)?;
        Ok(Self {
            bytes: Zeroizing::new(bytes),
            limit,
            _permit: permit,
        })
    }

    /// Appends `data`, or refuses (`rejected`) if it would pass the
    /// reservation.
    pub fn extend(&mut self, data: &[u8]) -> Result<(), ErrorCode> {
        match self.bytes.len().checked_add(data.len()) {
            Some(length) if length <= self.limit => {
                self.bytes.extend_from_slice(data);
                Ok(())
            }
            _ => Err(ErrorCode::Rejected),
        }
    }

    /// Fills the whole reservation from `reader`, which must then be at its
    /// end: a source shorter or longer than the size checked before (a file
    /// that changed while it was read) is `rejected`.
    pub fn fill_exactly(&mut self, reader: &mut impl Read) -> Result<(), ErrorCode> {
        self.bytes.clear();
        self.bytes.resize(self.limit, 0);
        reader
            .read_exact(&mut self.bytes)
            .map_err(|_| ErrorCode::Rejected)?;
        let mut more = [0u8; 1];
        match reader.read(&mut more) {
            Ok(0) => Ok(()),
            _ => Err(ErrorCode::Rejected),
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// RGBA pixels the core shows, reserved in the `RetainedPixels` budget.
pub struct Pixels {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    _permit: BudgetPermit,
}

impl Pixels {
    /// Pixels made directly, reserved like decoded ones (tests).
    #[cfg(test)]
    pub(crate) fn for_tests(
        budgets: &ResourceBudgets,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) -> Result<Self, ErrorCode> {
        assert_eq!(rgba.len() as u64, u64::from(width) * u64::from(height) * 4);
        let permit = budgets
            .try_acquire(BudgetClass::RetainedPixels, rgba.len() as u64)
            .map_err(|_| ErrorCode::LimitExceeded)?;
        Ok(Self {
            width,
            height,
            rgba,
            _permit: permit,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

/// The size of a `width` x `height` image once its EXIF orientation is
/// applied: orientations 5 to 8 swap the sides.
pub fn oriented_size(width: u32, height: u32, orientation: u8) -> (u32, u32) {
    if (5..=8).contains(&orientation) {
        (height, width)
    } else {
        (width, height)
    }
}

/// The largest size within `max` x `max` with the same proportions, never
/// larger than the input and never zero.
pub fn fit_within(width: u32, height: u32, max: u32) -> (u32, u32) {
    if width <= max && height <= max {
        return (width, height);
    }
    let scaled = |short: u32, long: u32| {
        let value = (u64::from(short) * u64::from(max) + u64::from(long) / 2) / u64::from(long);
        u32::try_from(value.clamp(1, u64::from(max))).unwrap_or(max)
    };
    if width >= height {
        (max, scaled(height, width))
    } else {
        (scaled(width, height), max)
    }
}

/// Where the shown pixel (`x`, `y`) of an image comes from in the decoded
/// `width` x `height` pixels, for EXIF orientation 1 to 8.
fn source_of(x: u32, y: u32, width: u32, height: u32, orientation: u8) -> (u32, u32) {
    match orientation {
        2 => (width - 1 - x, y),
        3 => (width - 1 - x, height - 1 - y),
        4 => (x, height - 1 - y),
        5 => (y, x),
        6 => (y, height - 1 - x),
        7 => (width - 1 - y, height - 1 - x),
        8 => (width - 1 - y, x),
        _ => (x, y),
    }
}

/// Writes `source` (`width` x `height` RGBA, as decoded) into `out`
/// (`out_width` x `out_height` RGBA) with the orientation applied. Each
/// output pixel is the average of the box of oriented pixels it covers, so
/// an output the size of the oriented image is an exact copy. The output
/// must not be larger than the oriented image.
pub fn orient_and_scale(
    source: &[u8],
    (width, height): (u32, u32),
    orientation: u8,
    out: &mut [u8],
    (out_width, out_height): (u32, u32),
) {
    let (shown_width, shown_height) = oriented_size(width, height, orientation);
    let span = |index: u32, out_len: u32, len: u32| {
        let start = u64::from(index) * u64::from(len) / u64::from(out_len);
        let end = (u64::from(index) + 1) * u64::from(len) / u64::from(out_len);
        (start as u32, (end as u32).max(start as u32 + 1))
    };
    let row = width as usize * 4;
    let mut target = out.as_chunks_mut::<4>().0.iter_mut();
    for out_y in 0..out_height {
        let (y0, y1) = span(out_y, out_height, shown_height);
        for out_x in 0..out_width {
            let (x0, x1) = span(out_x, out_width, shown_width);
            let mut sum = [0u64; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let (sx, sy) = source_of(x, y, width, height, orientation);
                    let at = sy as usize * row + sx as usize * 4;
                    for (total, value) in sum.iter_mut().zip(&source[at..at + 4]) {
                        *total += u64::from(*value);
                    }
                }
            }
            let count = u64::from(x1 - x0) * u64::from(y1 - y0);
            if let Some(pixel) = target.next() {
                for (value, total) in pixel.iter_mut().zip(sum) {
                    *value = ((total + count / 2) / count) as u8;
                }
            }
        }
    }
}

/// Rows per band for an image `width` pixels wide: as many as fit
/// `MAX_BAND_BYTES`, at least one.
pub fn band_rows(width: u32) -> u32 {
    let row = u64::from(width.max(1)) * RGBA_BYTES_PER_PIXEL;
    u32::try_from((MAX_BAND_BYTES / row).max(1)).unwrap_or(u32::MAX)
}

fn view_of(handle: String, pixels: &Pixels) -> ImageView {
    let rows = band_rows(pixels.width);
    ImageView {
        handle,
        width: pixels.width,
        height: pixels.height,
        bands: pixels.height.div_ceil(rows),
        band_rows: rows,
    }
}

/// One band of an image's rows. It keeps the pixels alive (and reserved)
/// while it is written, even if the image is released meanwhile.
pub struct Band {
    pub width: u32,
    pub rows: u32,
    pub index: u32,
    pixels: Arc<Pixels>,
    start: usize,
    end: usize,
}

impl Band {
    pub fn bytes(&self) -> &[u8] {
        &self.pixels.rgba[self.start..self.end]
    }
}

/// A handle as the page names it: exactly [`IMAGE_HANDLE_BYTES`] lowercase
/// hex digits.
pub fn valid_handle(handle: &str) -> bool {
    handle.len() == IMAGE_HANDLE_BYTES
        && handle
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn new_handle() -> Result<String, ErrorCode> {
    let mut random = [0u8; IMAGE_HANDLE_BYTES / 2];
    getrandom::fill(&mut random).map_err(|_| ErrorCode::Internal)?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The kind of an encoded file, from its first bytes.
pub fn sniff(bytes: &[u8]) -> Option<ImageMime> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageMime::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageMime::Jpeg)
    } else {
        None
    }
}

/// The stripped copy of a picked file, ready to send. Reserved in the
/// `EncodedInFlight` budget and zeroed when dropped.
pub struct ImageUpload {
    bytes: Zeroizing<Vec<u8>>,
    mime: ImageMime,
    /// The image's size as shown (after orientation, before any scaling).
    width: u32,
    height: u32,
    _permit: BudgetPermit,
    budgets: Arc<ResourceBudgets>,
}

impl ImageUpload {
    /// Copies a stripped file from the worker into zeroed-on-drop memory.
    /// `rejected` unless it is a PNG or JPEG file of at most
    /// [`MAX_SENT_BYTES`].
    fn from_stripped(
        budgets: &Arc<ResourceBudgets>,
        stripped: ReceivedFrame,
    ) -> Result<Self, ErrorCode> {
        if stripped.header().kind != FrameKind::EncodedImage {
            return Err(ErrorCode::Rejected);
        }
        let mime = sniff(stripped.body()).ok_or(ErrorCode::Rejected)?;
        let length = stripped.body().len();
        if length as u64 > MAX_SENT_BYTES {
            return Err(ErrorCode::Rejected);
        }
        let permit = budgets
            .try_acquire(BudgetClass::EncodedInFlight, length as u64)
            .map_err(|_| ErrorCode::LimitExceeded)?;
        let mut bytes = Zeroizing::new(Vec::new());
        bytes
            .try_reserve_exact(length)
            .map_err(|_| ErrorCode::LimitExceeded)?;
        bytes.extend_from_slice(stripped.body());
        Ok(Self {
            bytes,
            mime,
            width: 0,
            height: 0,
            _permit: permit,
            budgets: budgets.clone(),
        })
    }

    /// An upload made directly from bytes (tests).
    #[cfg(test)]
    pub(crate) fn for_tests(budgets: &Arc<ResourceBudgets>, bytes: &[u8]) -> Self {
        Self {
            bytes: Zeroizing::new(bytes.to_vec()),
            mime: sniff(bytes).unwrap_or(ImageMime::Png),
            width: 1,
            height: 1,
            _permit: budgets
                .try_acquire(BudgetClass::EncodedInFlight, bytes.len() as u64)
                .expect("test upload fits the budget"),
            budgets: budgets.clone(),
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn mime(&self) -> ImageMime {
        self.mime
    }

    pub fn mime_type(&self) -> &'static str {
        match self.mime {
            ImageMime::Png => "image/png",
            ImageMime::Jpeg => "image/jpeg",
        }
    }

    /// The generic name sent with the image, so the original file name does
    /// not leave the device.
    pub fn file_name(&self) -> &'static str {
        match self.mime {
            ImageMime::Png => "image.png",
            ImageMime::Jpeg => "image.jpg",
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// A copy for the SDK, which takes the data by value, reserved like the
    /// upload itself together with the copies the SDK makes while it uploads
    /// (all but the upload's own of [`SENT_COPIES`]). No SDK copy is zeroed.
    pub fn copy_for_send(&self) -> Result<(Vec<u8>, BudgetPermit), ErrorCode> {
        let permit = self
            .budgets
            .try_acquire(
                BudgetClass::EncodedInFlight,
                (SENT_COPIES - 1) * self.bytes.len() as u64,
            )
            .map_err(|_| ErrorCode::LimitExceeded)?;
        let mut copy = Vec::new();
        copy.try_reserve_exact(self.bytes.len())
            .map_err(|_| ErrorCode::LimitExceeded)?;
        copy.extend_from_slice(&self.bytes);
        Ok((copy, permit))
    }
}

/// Decoded pixels from the worker, not yet oriented.
pub struct DecodedPixels {
    /// The validated `RgbaPixels` frame, which keeps its `WorkerOutput`
    /// reservation.
    pub frame: ReceivedFrame,
    /// EXIF orientation, 1 to 8.
    pub orientation: u8,
}

/// Runs the contained decoder. Errors are public codes: `rejected` for an
/// image the worker refused (malformed, too large decoded, too slow),
/// `unavailable` when the worker cannot run here.
pub trait ImageWorker: Send + Sync {
    fn decode(&self, encoded: &[u8], budgets: &ResourceBudgets)
    -> Result<DecodedPixels, ErrorCode>;

    /// The stripped copy, as an `EncodedImage` frame no longer than the input.
    fn strip(&self, encoded: &[u8], budgets: &ResourceBudgets) -> Result<ReceivedFrame, ErrorCode>;
}

/// No contained worker: every run is `unavailable` (platforms other than
/// Windows, Linux and macOS; ADR-0003).
pub struct NoWorker;

impl ImageWorker for NoWorker {
    fn decode(&self, _: &[u8], _: &ResourceBudgets) -> Result<DecodedPixels, ErrorCode> {
        Err(ErrorCode::Unavailable)
    }

    fn strip(&self, _: &[u8], _: &ResourceBudgets) -> Result<ReceivedFrame, ErrorCode> {
        Err(ErrorCode::Unavailable)
    }
}

/// The platform's worker ([`crate::platform`]): the contained worker next to
/// the core on Windows, Linux and macOS, [`NoWorker`] where containment is
/// not implemented.
pub fn platform_worker() -> Arc<dyn ImageWorker> {
    crate::platform::os::image_worker()
}

/// The contained worker on Windows, Linux and macOS.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
pub mod contained;

struct Draft {
    room_id: String,
    upload: Arc<ImageUpload>,
}

struct Entry {
    generation: u64,
    /// When it was last used, by the store's counter.
    used: u64,
    pixels: Arc<Pixels>,
    /// Set for a picked image; a draft is never evicted.
    draft: Option<Draft>,
}

#[derive(Default)]
struct Store {
    entries: HashMap<String, Entry>,
    tick: u64,
}

impl Store {
    /// Drops every entry not of the `current` generation.
    fn keep(&mut self, current: Option<u64>) {
        self.entries
            .retain(|_, entry| Some(entry.generation) == current);
    }

    fn touch(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Drops the least recently used loaded image; `false` if there is none.
    fn evict_one(&mut self) -> bool {
        let oldest = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.draft.is_none())
            .min_by_key(|(_, entry)| entry.used)
            .map(|(handle, _)| handle.clone());
        oldest.is_some_and(|handle| self.entries.remove(&handle).is_some())
    }

    fn drafts(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.draft.is_some())
            .count()
    }

    /// Adds an entry under a new handle if `generation` is still current.
    fn insert(
        &mut self,
        generation: u64,
        current: Option<u64>,
        pixels: Arc<Pixels>,
        draft: Option<Draft>,
    ) -> Result<String, ErrorCode> {
        self.keep(current);
        if current != Some(generation) {
            return Err(ErrorCode::Locked);
        }
        if draft.is_some() && self.drafts() >= MAX_DRAFTS {
            return Err(ErrorCode::LimitExceeded);
        }
        while self.entries.len() >= MAX_HANDLES {
            if !self.evict_one() {
                return Err(ErrorCode::LimitExceeded);
            }
        }
        let handle = loop {
            let handle = new_handle()?;
            if !self.entries.contains_key(&handle) {
                break handle;
            }
        };
        let used = self.touch();
        self.entries.insert(
            handle.clone(),
            Entry {
                generation,
                used,
                pixels,
                draft,
            },
        );
        Ok(handle)
    }
}

/// An admission slot, released when dropped.
pub struct Slot<'a>(&'a AtomicUsize);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn take_slot(counter: &AtomicUsize, max: usize) -> Result<Slot<'_>, ErrorCode> {
    counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |taken| {
            (taken < max).then_some(taken + 1)
        })
        .map(|_| Slot(counter))
        .map_err(|_| ErrorCode::LimitExceeded)
}

/// A draft taken out of the store to be queued, which can be put back if the
/// outbox refuses it.
pub struct TakenDraft {
    handle: String,
    generation: u64,
    pixels: Arc<Pixels>,
    draft: Draft,
}

impl TakenDraft {
    pub fn upload(&self) -> Arc<ImageUpload> {
        self.draft.upload.clone()
    }
}

/// The core's images: admission, the worker slot and the handle store.
pub struct MediaService {
    budgets: Arc<ResourceBudgets>,
    worker: Arc<dyn ImageWorker>,
    store: Mutex<Store>,
    loads: AtomicUsize,
    picks: AtomicUsize,
    /// One worker run at a time; admitted work waits here, bounded by the
    /// admission limits.
    worker_slot: Mutex<()>,
}

impl MediaService {
    pub fn new(budgets: Arc<ResourceBudgets>, worker: Arc<dyn ImageWorker>) -> Self {
        Self {
            budgets,
            worker,
            store: Mutex::default(),
            loads: AtomicUsize::new(0),
            picks: AtomicUsize::new(0),
            worker_slot: Mutex::new(()),
        }
    }

    pub fn budgets(&self) -> &Arc<ResourceBudgets> {
        &self.budgets
    }

    /// Admits one `load_image`, or `limit_exceeded`.
    pub fn begin_load(&self) -> Result<Slot<'_>, ErrorCode> {
        take_slot(&self.loads, MAX_LOADS)
    }

    /// Admits one `pick_image`, or `limit_exceeded` (also while a draft
    /// waits to be sent or discarded).
    pub fn begin_pick(&self) -> Result<Slot<'_>, ErrorCode> {
        if guard(&self.store).drafts() >= MAX_DRAFTS {
            return Err(ErrorCode::LimitExceeded);
        }
        take_slot(&self.picks, MAX_PICKS)
    }

    /// Decodes one image in the worker.
    pub fn decode(&self, encoded: &[u8]) -> Result<DecodedPixels, ErrorCode> {
        let _slot = guard(&self.worker_slot);
        self.worker.decode(encoded, &self.budgets)
    }

    /// Reserves `bytes` of `RetainedPixels`, dropping least recently used
    /// loaded images while the budget is full.
    fn reserve_pixels(&self, bytes: u64) -> Result<BudgetPermit, ErrorCode> {
        loop {
            match self.budgets.try_acquire(BudgetClass::RetainedPixels, bytes) {
                Ok(permit) => return Ok(permit),
                Err(_) => {
                    if !guard(&self.store).evict_one() {
                        return Err(ErrorCode::LimitExceeded);
                    }
                }
            }
        }
    }

    /// Orients and scales decoded pixels for showing. Returns them with the
    /// image's oriented size before scaling.
    pub fn shown(&self, decoded: DecodedPixels) -> Result<(Pixels, (u32, u32)), ErrorCode> {
        let header = decoded.frame.header();
        let (width, height) = (header.width, header.height);
        let expected = u64::from(width) * u64::from(height) * RGBA_BYTES_PER_PIXEL;
        if header.kind != FrameKind::RgbaPixels
            || width == 0
            || height == 0
            || width > MAX_DIMENSION
            || height > MAX_DIMENSION
            || decoded.frame.body().len() as u64 != expected
            || !(1..=8).contains(&decoded.orientation)
        {
            return Err(ErrorCode::Rejected);
        }
        let oriented = oriented_size(width, height, decoded.orientation);
        let shown = fit_within(oriented.0, oriented.1, MAX_SHOWN_SIDE);
        let bytes = u64::from(shown.0) * u64::from(shown.1) * RGBA_BYTES_PER_PIXEL;
        let permit = self.reserve_pixels(bytes)?;
        let length = usize::try_from(bytes).map_err(|_| ErrorCode::Rejected)?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(length)
            .map_err(|_| ErrorCode::LimitExceeded)?;
        rgba.resize(length, 0);
        orient_and_scale(
            decoded.frame.body(),
            (width, height),
            decoded.orientation,
            &mut rgba,
            shown,
        );
        Ok((
            Pixels {
                width: shown.0,
                height: shown.1,
                rgba,
                _permit: permit,
            },
            oriented,
        ))
    }

    /// Keeps loaded pixels under a new handle if `generation` is still the
    /// open one; `current` is asked under the store's lock, so nothing kept
    /// here outlives a lock (which purges after advancing the generation).
    pub fn keep_view(
        &self,
        generation: u64,
        current: &dyn Fn() -> Option<u64>,
        pixels: Pixels,
    ) -> Result<ImageView, ErrorCode> {
        let pixels = Arc::new(pixels);
        let mut store = guard(&self.store);
        let handle = store.insert(generation, current(), pixels.clone(), None)?;
        Ok(view_of(handle, &pixels))
    }

    /// Strips a picked file, decodes the stripped copy for its preview, and
    /// keeps both as a draft for `room_id` if `generation` is still current.
    pub fn prepare_draft(
        &self,
        generation: u64,
        current: &dyn Fn() -> Option<u64>,
        room_id: &str,
        original: Encoded,
    ) -> Result<ImagePick, ErrorCode> {
        let stripped = {
            let _slot = guard(&self.worker_slot);
            self.worker.strip(original.bytes(), &self.budgets)?
        };
        drop(original);
        if current() != Some(generation) {
            return Err(ErrorCode::Locked);
        }
        let mut upload = ImageUpload::from_stripped(&self.budgets, stripped)?;
        let decoded = self.decode(upload.bytes())?;
        let (pixels, (width, height)) = self.shown(decoded)?;
        upload.width = width;
        upload.height = height;
        let size = u32::try_from(upload.len()).map_err(|_| ErrorCode::Rejected)?;
        let mime = upload.mime;
        let pixels = Arc::new(pixels);
        let mut store = guard(&self.store);
        let handle = store.insert(
            generation,
            current(),
            pixels.clone(),
            Some(Draft {
                room_id: room_id.to_owned(),
                upload: Arc::new(upload),
            }),
        )?;
        Ok(ImagePick::Draft {
            preview: view_of(handle, &pixels),
            size,
            mime,
        })
    }

    /// Band `index` of an image of the `current` generation.
    pub fn band(&self, current: Option<u64>, handle: &str, index: u32) -> Result<Band, ErrorCode> {
        if !valid_handle(handle) {
            return Err(ErrorCode::InvalidArgument);
        }
        let mut store = guard(&self.store);
        store.keep(current);
        let used = store.touch();
        let entry = store.entries.get_mut(handle).ok_or(ErrorCode::NotFound)?;
        entry.used = used;
        let pixels = entry.pixels.clone();
        drop(store);
        let rows = band_rows(pixels.width);
        if index >= pixels.height.div_ceil(rows) {
            return Err(ErrorCode::InvalidArgument);
        }
        let top = index * rows;
        let count = rows.min(pixels.height - top);
        let row = pixels.width as usize * 4;
        let start = top as usize * row;
        Ok(Band {
            width: pixels.width,
            rows: count,
            index,
            end: start + count as usize * row,
            start,
            pixels,
        })
    }

    fn remove(
        &self,
        current: Option<u64>,
        handle: &str,
        draft: bool,
    ) -> Result<Option<Entry>, ErrorCode> {
        if !valid_handle(handle) {
            return Err(ErrorCode::InvalidArgument);
        }
        let mut store = guard(&self.store);
        store.keep(current);
        match store.entries.get(handle) {
            Some(entry) if entry.draft.is_some() == draft => Ok(store.entries.remove(handle)),
            _ => Ok(None),
        }
    }

    /// Drops a loaded image; `not_found` for a draft or an unknown handle.
    pub fn release(&self, current: Option<u64>, handle: &str) -> Result<(), ErrorCode> {
        self.remove(current, handle, false)?
            .map(drop)
            .ok_or(ErrorCode::NotFound)
    }

    /// Drops a draft; `not_found` for a loaded image or an unknown handle.
    pub fn discard(&self, current: Option<u64>, handle: &str) -> Result<(), ErrorCode> {
        self.remove(current, handle, true)?
            .map(drop)
            .ok_or(ErrorCode::NotFound)
    }

    /// Takes the draft `handle` picked for `room_id`.
    pub fn take_draft(
        &self,
        current: Option<u64>,
        handle: &str,
        room_id: &str,
    ) -> Result<TakenDraft, ErrorCode> {
        if !valid_handle(handle) {
            return Err(ErrorCode::InvalidArgument);
        }
        let mut store = guard(&self.store);
        store.keep(current);
        let matches = store
            .entries
            .get(handle)
            .and_then(|entry| entry.draft.as_ref())
            .is_some_and(|draft| draft.room_id == room_id);
        if !matches {
            return Err(ErrorCode::NotFound);
        }
        let entry = store.entries.remove(handle).ok_or(ErrorCode::NotFound)?;
        Ok(TakenDraft {
            handle: handle.to_owned(),
            generation: entry.generation,
            pixels: entry.pixels,
            draft: entry.draft.ok_or(ErrorCode::NotFound)?,
        })
    }

    /// Puts a draft back under its handle, if its generation is current.
    pub fn restore_draft(&self, current: &dyn Fn() -> Option<u64>, taken: TakenDraft) {
        let mut store = guard(&self.store);
        store.keep(current());
        if current() == Some(taken.generation) {
            let used = store.touch();
            store.entries.insert(
                taken.handle,
                Entry {
                    generation: taken.generation,
                    used,
                    pixels: taken.pixels,
                    draft: Some(taken.draft),
                },
            );
        }
    }

    /// Drops every image and draft (lock, sign-out, new UI connection).
    pub fn purge(&self) {
        let entries = std::mem::take(&mut guard(&self.store).entries);
        drop(entries);
    }

    /// Number of handles held (for tests).
    pub fn handles(&self) -> usize {
        guard(&self.store).entries.len()
    }
}

/// The timeline item for an `m.image` event. Nothing is fetched. The name is
/// the file name (or the body when there is none), projected like a display
/// name; the declared width and height are kept only together and only
/// within the decoder's limit, and a size of zero or beyond `u32` is none.
pub fn timeline_item(
    event_id: String,
    sender: String,
    origin_server_ts: Option<UnixMillis>,
    image: &ImageMessageEventContent,
) -> TimelineItem {
    let info = image.info.as_deref();
    let side = |value: Option<UInt>| {
        value
            .and_then(|value| u32::try_from(u64::from(value)).ok())
            .filter(|value| (1..=MAX_DIMENSION).contains(value))
    };
    let (width, height) = match (
        side(info.and_then(|info| info.width)),
        side(info.and_then(|info| info.height)),
    ) {
        (Some(width), Some(height)) => (Some(width), Some(height)),
        _ => (None, None),
    };
    let size = info
        .and_then(|info| info.size)
        .and_then(|size| u32::try_from(u64::from(size)).ok())
        .filter(|size| *size > 0);
    TimelineItem::Image {
        event_id,
        sender,
        origin_server_ts,
        name: project_name(image.filename.as_deref().unwrap_or(&image.body)),
        size,
        width,
        height,
        encrypted: matches!(image.source, MediaSource::Encrypted(_)),
    }
}

/// The image content of a decrypted (or plain) `m.image` event; `None` for
/// anything else, including an event that could not be decrypted.
pub fn image_content(event: &TimelineEvent) -> Option<ImageMessageEventContent> {
    if matches!(event.kind, TimelineEventKind::UnableToDecrypt { .. }) {
        return None;
    }
    if event
        .raw()
        .get_field::<String>("type")
        .ok()
        .flatten()
        .is_some_and(|kind| kind == "m.room.encrypted")
    {
        return None;
    }
    match event.raw().deserialize() {
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncMessageLikeEvent::Original(message),
        ))) => match message.content.msgtype {
            MessageType::Image(image) => Some(image),
            _ => None,
        },
        _ => None,
    }
}

/// The download URL for `uri` on the homeserver at `base`, and whether it
/// needs the access token. The authenticated endpoint is used when the
/// homeserver advertises it (the SDK's own rule); otherwise the legacy one
/// when advertised; otherwise `unavailable`. A media ID outside the Matrix
/// grammar is `rejected`.
pub fn media_url(
    base: &str,
    uri: &MxcUri,
    versions: &SupportedVersions,
) -> Result<(String, bool), ErrorCode> {
    let (server, media_id) = uri.parts().map_err(|_| ErrorCode::Rejected)?;
    if media_id.is_empty()
        || media_id.len() > MAX_MEDIA_ID_BYTES
        || !media_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(ErrorCode::Rejected);
    }
    let args: [&dyn Display; 2] = [&server, &media_id];
    let authenticated = authenticated_media::get_content::v1::Request::PATH_BUILDER;
    if authenticated.is_supported(versions) {
        return authenticated
            .make_endpoint_url(Cow::Borrowed(versions), base, &args, "")
            .map(|url| (url, true))
            .map_err(|_| ErrorCode::Unavailable);
    }
    #[allow(deprecated)]
    let legacy = media::get_content::v3::Request::PATH_BUILDER;
    if legacy.is_supported(versions) {
        return legacy
            .make_endpoint_url(Cow::Borrowed(versions), base, &args, "")
            .map(|url| (url, false))
            .map_err(|_| ErrorCode::Unavailable);
    }
    Err(ErrorCode::Unavailable)
}

/// Downloads at most `cap` bytes from `url`. A `Content-Length` over the cap
/// is refused before the body is read; the body is read in chunks into a
/// buffer reserved up front, and refused as soon as it would pass the cap or
/// the declared length. After each chunk, and every `DOWNLOAD_LOCK_POLL`
/// while the request or a chunk is still awaited, `current` is asked whether
/// the session is still the one that started: once it is not, the download
/// stops (`locked`) without waiting for the server. A body that ends early
/// is `unavailable` (the transfer failed). A 404 is `not_found`. Any other 4xx except 429, such as 403 or 451, is
/// `rejected`: the server refuses this file, and asking again gives the same
/// answer. A 429, a 5xx, any other status and a network failure are
/// `unavailable`.
pub async fn download(
    http: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    cap: u64,
    budgets: &ResourceBudgets,
    current: &(dyn Fn() -> bool + Sync),
) -> Result<Encoded, ErrorCode> {
    let mut request = http.get(url).timeout(DOWNLOAD_TIMEOUT);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let mut response = until_locked(request.send(), current)
        .await?
        .map_err(|_| ErrorCode::Unavailable)?;
    match response.status().as_u16() {
        200 => {}
        404 => return Err(ErrorCode::NotFound),
        429 => return Err(ErrorCode::Unavailable),
        400..=499 => return Err(ErrorCode::Rejected),
        _ => return Err(ErrorCode::Unavailable),
    }
    let declared = response.content_length();
    if declared.is_some_and(|length| length == 0 || length > cap) {
        return Err(ErrorCode::Rejected);
    }
    let mut encoded = Encoded::reserve(budgets, declared.unwrap_or(cap))?;
    while let Some(chunk) = until_locked(response.chunk(), current)
        .await?
        .map_err(|_| ErrorCode::Unavailable)?
    {
        if !current() {
            return Err(ErrorCode::Locked);
        }
        encoded.extend(&chunk)?;
    }
    let length = encoded.bytes().len() as u64;
    if length == 0 || declared.is_some_and(|declared| declared != length) {
        return Err(ErrorCode::Unavailable);
    }
    Ok(encoded)
}

/// Awaits `future`, asking `current` every `DOWNLOAD_LOCK_POLL` whether the
/// session is still the one that started; once it is not, `future` is
/// dropped and the result is `locked`. The same pinned `future` is polled
/// across ticks, so no progress depends on it being cancellation-safe.
async fn until_locked<F: std::future::Future>(
    future: F,
    current: &(dyn Fn() -> bool + Sync),
) -> Result<F::Output, ErrorCode> {
    let mut future = std::pin::pin!(future);
    loop {
        tokio::select! {
            biased;
            output = &mut future => return Ok(output),
            () = tokio::time::sleep(DOWNLOAD_LOCK_POLL) => {
                if !current() {
                    return Err(ErrorCode::Locked);
                }
            }
        }
    }
}

/// Reads a buffer that is being decrypted in place: bytes are taken from
/// `position`, and the decrypted ones are written back behind it.
struct InPlace<'a> {
    bytes: &'a RefCell<Vec<u8>>,
    position: &'a Cell<usize>,
}

impl Read for InPlace<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let bytes = self.bytes.borrow();
        let position = self.position.get();
        let count = out.len().min(bytes.len() - position);
        out[..count].copy_from_slice(&bytes[position..position + count]);
        self.position.set(position + count);
        Ok(count)
    }
}

/// Decrypts an encrypted attachment in place with the SDK's decryptor, which
/// checks the file's SHA-256 hash at the end. A wrong hash, key or version is
/// `rejected`, and the partly decrypted bytes are zeroed with the buffer.
pub fn decrypt_in_place(encoded: &mut Encoded, info: MediaEncryptionInfo) -> Result<(), ErrorCode> {
    let bytes = RefCell::new(std::mem::take(&mut *encoded.bytes));
    let position = Cell::new(0);
    let result = (|| {
        let mut source = InPlace {
            bytes: &bytes,
            position: &position,
        };
        let mut decryptor =
            AttachmentDecryptor::new(&mut source, info).map_err(|_| ErrorCode::Rejected)?;
        let mut chunk = Zeroizing::new(vec![0u8; DECRYPT_CHUNK_BYTES]);
        let mut written = 0;
        loop {
            let count = decryptor
                .read(&mut chunk)
                .map_err(|_| ErrorCode::Rejected)?;
            if count == 0 {
                return Ok(());
            }
            // The decryptor returns exactly the bytes it read, so the write
            // stays behind the read position.
            bytes.borrow_mut()[written..written + count].copy_from_slice(&chunk[..count]);
            written += count;
        }
    })();
    *encoded.bytes = bytes.into_inner();
    result
}

/// Finds the `m.image` event `event_id` in the joined room `room_id` and
/// downloads (and, if encrypted, decrypts) its file. `current` is asked
/// between chunks; see [`download`].
pub async fn fetch(
    client: &Client,
    http: &reqwest::Client,
    room_id: &str,
    event_id: &str,
    budgets: &ResourceBudgets,
    current: &(dyn Fn() -> bool + Sync),
) -> Result<Encoded, ErrorCode> {
    let room_id = <&RoomId>::try_from(room_id).map_err(|_| ErrorCode::InvalidArgument)?;
    let event_id = <&EventId>::try_from(event_id).map_err(|_| ErrorCode::InvalidArgument)?;
    let room = client
        .get_room(room_id)
        .filter(|room| room.state() == RoomState::Joined)
        .ok_or(ErrorCode::NotFound)?;
    let event = room
        .event(event_id, None)
        .await
        .map_err(|error| match error {
            matrix_sdk::Error::Http(http) if matches!(*http, HttpError::Api(_)) => {
                ErrorCode::NotFound
            }
            _ => ErrorCode::Unavailable,
        })?;
    let image = image_content(&event).ok_or(ErrorCode::NotFound)?;
    if image
        .info
        .as_ref()
        .and_then(|info| info.size)
        .is_some_and(|size| u64::from(size) > MAX_ENCODED_BYTES)
    {
        return Err(ErrorCode::Rejected);
    }
    fetch_source(client, http, &image.source, budgets, current).await
}

/// The event `event_id` in the joined room `room_id`, from the homeserver,
/// decrypted when it can be.
pub async fn room_event(
    client: &Client,
    room_id: &str,
    event_id: &str,
) -> Result<TimelineEvent, ErrorCode> {
    let room_id = <&RoomId>::try_from(room_id).map_err(|_| ErrorCode::InvalidArgument)?;
    let event_id = <&EventId>::try_from(event_id).map_err(|_| ErrorCode::InvalidArgument)?;
    let room = client
        .get_room(room_id)
        .filter(|room| room.state() == RoomState::Joined)
        .ok_or(ErrorCode::NotFound)?;
    room.event(event_id, None)
        .await
        .map_err(|error| match error {
            matrix_sdk::Error::Http(http) if matches!(*http, HttpError::Api(_)) => {
                ErrorCode::NotFound
            }
            _ => ErrorCode::Unavailable,
        })
}

/// Downloads `source` (at most [`MAX_ENCODED_BYTES`]) and decrypts it when
/// it is encrypted.
pub async fn fetch_source(
    client: &Client,
    http: &reqwest::Client,
    source: &MediaSource,
    budgets: &ResourceBudgets,
    current: &(dyn Fn() -> bool + Sync),
) -> Result<Encoded, ErrorCode> {
    let (uri, encryption) = match source {
        MediaSource::Plain(uri) => (uri.clone(), None),
        MediaSource::Encrypted(file) => (
            file.url.clone(),
            Some(MediaEncryptionInfo::from((**file).clone())),
        ),
    };
    let versions = client
        .supported_versions()
        .await
        .map_err(|_| ErrorCode::Unavailable)?;
    let (url, authenticated) = media_url(client.homeserver().as_str(), &uri, &versions)?;
    let token = if authenticated {
        Some(Zeroizing::new(
            client.access_token().ok_or(ErrorCode::Unavailable)?,
        ))
    } else {
        None
    };
    let mut encoded = download(
        http,
        &url,
        token.as_deref().map(String::as_str),
        MAX_ENCODED_BYTES,
        budgets,
        current,
    )
    .await?;
    drop(token);
    if let Some(info) = encryption {
        decrypt_in_place(&mut encoded, info)?;
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests;

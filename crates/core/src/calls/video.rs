//! The newest call video picture on each tile (ADR-0048).
//!
//! Pictures do not use the worker's event queue: that queue blocks the read
//! thread when the controller is slow, and a picture is worth nothing once a
//! newer one exists. The store keeps at most one picture per tile (six in
//! all: five camera tiles of at most
//! [`persephone_call_protocol::MAX_VIDEO_BYTES`] and the screen tile of at
//! most [`persephone_call_protocol::MAX_SCREEN_BYTES`], about 8.3 MB), so a
//! slow page loses pictures and never grows memory. A picture's limit is
//! its tile's (ADR-0053), checked before it is kept.
//!
//! Two threads share one [`VideoStore`]:
//!
//! * the worker read thread calls [`VideoStore::insert`] for each picture
//!   and [`VideoStore::stream_changed`] for each `VideoStarted` and
//!   `VideoStopped` it reads;
//! * the controller task calls [`VideoStore::sync`],
//!   [`VideoStore::stream_applied`], [`VideoStore::disable_tile`] and
//!   [`VideoStore::close`], and the request loop calls
//!   [`VideoStore::newest`].
//!
//! The store lock is held only to read or replace small values. Pixel bytes
//! are shared (`Arc`), so a reader copies a pointer, not a picture, and
//! never writes to the host pipe under the lock. The notifier runs after the
//! lock is released.
//!
//! A picture is kept only when the controller has said its tile may show:
//! tile 0 while the camera is on, tiles 1 to 5 (5 is the screen share) after
//! the worker's `VideoStarted` for a current participant. [`VideoStore::close`] ends the
//! store for good: it drops every picture and refuses later ones.
//!
//! A tile's stream can change (a freed tile goes to a waiting track) before
//! the controller applies the change. The read thread counts the stream
//! changes it reads, the controller counts the ones it applies, and a picture
//! is kept only when the two counts agree. So a new stream's pixels never
//! show under the old stream's participant. Pictures do not go through the
//! controller's queue: they only need the counts to agree.
//!
//! Each tile also has a stream number. A new store gives every tile a fresh
//! one, and each applied change gives that tile the next one. The numbers
//! come from one counter for the whole process, so no two calls share one.
//! Each kept picture carries its tile's number, the page's call status
//! names it for each tile, and [`VideoStore::newest`] gives a picture only
//! to a request for the same number. So a page whose status is older than a
//! tile's reassignment, or than the call itself, gets no picture of another
//! stream under the old participant's name.

use persephone_call_protocol::{MAX_VIDEO_ROTATION, VIDEO_TILES, picture_fits};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Tiles in a call: 0 is the own camera, 1 to 4 are remote cameras and 5 is
/// the remote screen share.
const TILES: usize = VIDEO_TILES as usize;
/// The own camera's tile.
const OWN_TILE: usize = 0;

/// Largest picture number: the page reads it as a JavaScript number.
const MAX_SEQ: u64 = 9_007_199_254_740_991;

/// The next stream count. It wraps inside the JavaScript-safe range.
fn next_count(count: u64) -> u64 {
    (count + 1) & MAX_SEQ
}

/// The process's stream number counter: shared by every store, so a stream
/// number names one stream of one call.
static NEXT_STREAM: AtomicU64 = AtomicU64::new(1);

/// A stream number no other stream of this process had (until the counter
/// wraps after 2^53 streams). It is JavaScript-safe.
fn next_stream() -> u64 {
    NEXT_STREAM.fetch_add(1, Ordering::Relaxed) & MAX_SEQ
}

/// The bytes of a picture, with any budget reservation that covers them
/// (the worker reader's [`persephone_call_protocol::Pixels`]).
pub type Rgba = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// One kept picture. Cloning shares the pixel bytes.
#[derive(Clone)]
pub struct Picture {
    /// Rises by one for each kept picture, across all tiles; the first is 1.
    pub seq: u64,
    /// The tile's stream number when the picture was kept.
    pub stream: u64,
    pub rotation: u8,
    pub width: u32,
    pub height: u32,
    pub rgba: Rgba,
}

impl Picture {
    /// The RGBA bytes.
    pub fn bytes(&self) -> &[u8] {
        (*self.rgba).as_ref()
    }
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Picture")
            .field("seq", &self.seq)
            .field("stream", &self.stream)
            .field("rotation", &self.rotation)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.bytes().len())
            .finish()
    }
}

/// Tells the owner that a picture arrived. It must not block.
pub type Notifier = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct State {
    /// Which tiles may keep a picture now.
    allowed: [bool; TILES],
    /// Tiles that no sync may allow. Only the own tile is blocked: by a
    /// camera-off, until [`VideoStore::camera_on_taken`].
    blocked: [bool; TILES],
    /// The number of the user's latest camera request.
    camera_requests: u64,
    /// Stream changes of each tile that the read thread has read.
    changed: [u64; TILES],
    /// Stream changes of each tile that the controller has applied.
    applied: [u64; TILES],
    /// The stream number of each tile ([`next_stream`]).
    streams: [u64; TILES],
    pictures: [Option<Picture>; TILES],
    seq: u64,
    closed: bool,
    notifier: Option<Notifier>,
}

/// The shared picture store of one call worker. Clones share it.
#[derive(Clone)]
pub struct VideoStore {
    state: Arc<Mutex<State>>,
}

impl Default for VideoStore {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoStore {
    /// A store whose tiles have fresh stream numbers.
    pub fn new() -> Self {
        let state = State {
            streams: std::array::from_fn(|_| next_stream()),
            ..State::default()
        };
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    /// The stream number of each tile now (tile 0 first).
    pub fn streams(&self) -> [u64; TILES] {
        self.state().streams
    }

    /// The stream number of `tile` now; 0 for a tile out of range.
    pub fn stream(&self, tile: u8) -> u64 {
        self.state()
            .streams
            .get(usize::from(tile))
            .copied()
            .unwrap_or(0)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A poisoned lock only means a thread panicked; the data stays plain.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sets what runs after each kept picture. Replaces an earlier one.
    pub fn set_notifier(&self, notifier: Notifier) {
        self.state().notifier = Some(notifier);
    }

    /// Keeps `rgba` as the newest picture of `tile`, if the tile may show,
    /// the controller has applied every stream change of the tile, and the
    /// picture is inside the video limits. Returns whether it was kept; a
    /// refused picture is dropped.
    pub fn insert(
        &self,
        tile: u8,
        rotation: u8,
        width: u32,
        height: u32,
        rgba: impl AsRef<[u8]> + Send + Sync + 'static,
    ) -> bool {
        let pixels = u64::from(width) * u64::from(height);
        // The limits are the tile's, checked by index before anything is kept.
        if usize::from(tile) >= TILES
            || rotation > MAX_VIDEO_ROTATION
            || !picture_fits(tile, width, height)
            || u64::try_from(rgba.as_ref().len()).ok() != Some(pixels * 4)
        {
            return false;
        }
        // The bytes are wrapped before the lock: only pointers move inside it.
        let rgba: Rgba = Arc::new(rgba);
        let notifier = {
            let mut state = self.state();
            let index = usize::from(tile);
            if state.closed
                || !state.allowed[index]
                || state.changed[index] != state.applied[index]
                || state.seq >= MAX_SEQ
            {
                return false;
            }
            state.seq += 1;
            let seq = state.seq;
            let stream = state.streams[index];
            state.pictures[index] = Some(Picture {
                seq,
                stream,
                rotation,
                width,
                height,
                rgba,
            });
            state.notifier.clone()
        };
        if let Some(notifier) = notifier {
            notifier();
        }
        true
    }

    /// The newest picture of `tile` if its number is above `after` and it
    /// belongs to stream `stream`. A request for an older stream gets
    /// nothing: the tile may show someone else now.
    pub fn newest(&self, tile: u8, after: u64, stream: u64) -> Option<Picture> {
        let state = self.state();
        state
            .pictures
            .get(usize::from(tile))?
            .as_ref()
            .filter(|picture| picture.seq > after && picture.stream == stream)
            .cloned()
    }

    /// Sets which tiles may show: the own tile (`own`) and tiles 1 to 5
    /// (`remote[0]` is tile 1, `remote[4]` the screen tile). A tile that may not show loses its picture.
    pub fn sync(&self, own: bool, remote: [bool; TILES - 1]) {
        let mut state = self.state();
        if state.closed {
            return;
        }
        let allowed = [own, remote[0], remote[1], remote[2], remote[3], remote[4]];
        for (tile, allow) in allowed.into_iter().enumerate() {
            let allow = allow && !state.blocked[tile];
            state.allowed[tile] = allow;
            if !allow {
                state.pictures[tile] = None;
            }
        }
    }

    /// The read thread read a `VideoStarted` or `VideoStopped` of `tile`. It
    /// calls this before it passes the event on. Pictures of the tile are
    /// refused until the controller has applied the change.
    pub fn stream_changed(&self, tile: u8) {
        let mut state = self.state();
        let index = usize::from(tile);
        if index < TILES {
            state.changed[index] = next_count(state.changed[index]);
            state.pictures[index] = None;
        }
    }

    /// The controller applied a `VideoStarted` or `VideoStopped` of `tile`:
    /// the tile starts empty, and pictures are kept again once every change
    /// the read thread passed on is applied. The tile gets a new stream
    /// number, which this returns (0 for a tile out of range).
    pub fn stream_applied(&self, tile: u8) -> u64 {
        let mut state = self.state();
        let index = usize::from(tile);
        if index >= TILES {
            return 0;
        }
        state.applied[index] = next_count(state.applied[index]);
        state.pictures[index] = None;
        state.streams[index] = next_stream();
        state.streams[index]
    }

    /// Records a camera request of the user and returns its number. An off
    /// drops the own picture at once and refuses later ones, also across
    /// later syncs: a sync made from an older view (the camera still on)
    /// cannot let a late picture in. Only the camera-on request with the
    /// latest number lifts the block ([`VideoStore::camera_on_taken`]).
    pub fn camera_request(&self, on: bool) -> u64 {
        let mut state = self.state();
        state.camera_requests = state.camera_requests.wrapping_add(1);
        if !on {
            state.allowed[OWN_TILE] = false;
            state.blocked[OWN_TILE] = true;
            state.pictures[OWN_TILE] = None;
        }
        state.camera_requests
    }

    /// The call task took camera-on request `request`. If no later request
    /// came, syncs may allow the own tile again; it allows nothing by itself.
    /// An older request changes nothing, so a camera-on queued before an off
    /// cannot undo that off.
    pub fn camera_on_taken(&self, request: u64) {
        let mut state = self.state();
        if state.camera_requests == request {
            state.blocked[OWN_TILE] = false;
        }
    }

    /// Ends the store: drops every picture and refuses later ones.
    pub fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        state.allowed = [false; TILES];
        state.pictures = Default::default();
        state.notifier = None;
    }

    /// How many pictures are kept (for tests).
    #[cfg(test)]
    fn kept(&self) -> usize {
        self.state().pictures.iter().flatten().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn rgba(width: u32, height: u32, fill: u8) -> Vec<u8> {
        vec![fill; (width * height * 4) as usize]
    }

    fn open_store() -> VideoStore {
        let store = VideoStore::new();
        store.sync(true, [true; 5]);
        store
    }

    /// GIVEN a store where no tile may show WHEN pictures arrive THEN every
    /// one is dropped.
    #[test]
    fn given_no_allowed_tile_then_pictures_are_dropped() {
        let store = VideoStore::new();
        for tile in 0..5 {
            assert!(!store.insert(tile, 0, 2, 2, rgba(2, 2, 1)));
            assert!(store.newest(tile, 0, store.stream(tile)).is_none());
        }
    }

    /// GIVEN allowed tiles WHEN pictures arrive THEN numbers rise across
    /// tiles, a newer picture replaces the old one on its tile, and `after`
    /// hides a picture the page has seen.
    #[test]
    fn given_pictures_then_the_newest_per_tile_is_kept_with_rising_numbers() {
        let store = open_store();
        assert!(store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        assert!(store.insert(1, 3, 4, 2, rgba(4, 2, 2)));
        assert!(store.insert(0, 1, 2, 2, rgba(2, 2, 3)));
        let own = store.newest(0, 0, store.stream(0)).expect("own picture");
        assert_eq!((own.seq, own.rotation, own.bytes()[0]), (3, 1, 3));
        let remote = store.newest(1, 0, store.stream(1)).expect("remote picture");
        assert_eq!((remote.seq, remote.width, remote.height), (2, 4, 2));
        assert!(
            store.newest(0, 3, store.stream(0)).is_none(),
            "nothing newer than 3"
        );
        assert!(store.newest(0, 2, store.stream(0)).is_some());
        assert_eq!(store.kept(), 2, "one picture per tile");
    }

    /// GIVEN bad tile, rotation, size or length WHEN inserted THEN the picture
    /// is dropped and the stored one stays.
    #[test]
    fn given_pictures_outside_the_limits_then_they_are_dropped() {
        let store = open_store();
        assert!(store.insert(1, 0, 2, 2, rgba(2, 2, 7)));
        let cases: [(u8, u8, u32, u32, usize); 12] = [
            (6, 0, 2, 2, 16),
            (1, 0, 1280, 720, 1280 * 720 * 4),
            (4, 0, 1280, 720, 1280 * 720 * 4),
            (5, 0, 1281, 1, 1281 * 4),
            (5, 0, 1280, 721, 1280 * 721 * 4),
            (1, 4, 2, 2, 16),
            (1, 0, 0, 2, 0),
            (1, 0, 2, 0, 0),
            (1, 0, 641, 1, 641 * 4),
            (1, 0, 640, 361, 640 * 361 * 4),
            (1, 0, 2, 2, 15),
            (1, 0, 2, 2, 17),
        ];
        for (tile, rotation, width, height, len) in cases {
            assert!(
                !store.insert(tile, rotation, width, height, vec![9; len]),
                "{tile} {rotation} {width}x{height} {len}"
            );
        }
        let kept = store
            .newest(1, 0, store.stream(1))
            .expect("the first picture stays");
        assert_eq!((kept.seq, kept.bytes()[0]), (1, 7));
        let largest = store.insert(1, 0, 640, 360, rgba(640, 360, 1));
        assert!(largest, "the largest allowed picture is kept");
    }

    /// GIVEN the screen tile WHEN pictures of 1280x720 and 720x1280 arrive
    /// THEN both are kept (3,686,400 bytes each) and the picture of a camera
    /// tile is still held to 640x360; all six tiles together hold at most
    /// about 8.3 MB.
    #[test]
    fn given_the_screen_tile_then_it_keeps_a_1280_by_720_picture() {
        let store = open_store();
        assert!(store.insert(5, 0, 1280, 720, rgba(1280, 720, 1)));
        assert!(store.insert(5, 0, 720, 1280, rgba(720, 1280, 2)));
        let kept = store
            .newest(5, 0, store.stream(5))
            .expect("the screen picture");
        assert_eq!(
            (kept.width, kept.height, kept.bytes().len()),
            (720, 1280, 3_686_400)
        );
        assert!(!store.insert(3, 0, 1280, 720, rgba(1280, 720, 1)));
        let mut total = 0;
        for tile in 0..4 {
            assert!(store.insert(tile, 0, 640, 360, rgba(640, 360, 3)));
        }
        assert!(store.insert(4, 0, 640, 360, rgba(640, 360, 3)));
        for tile in 0..6 {
            total += store
                .newest(tile, 0, store.stream(tile))
                .map_or(0, |picture| picture.bytes().len());
        }
        assert_eq!(total, 5 * 921_600 + 3_686_400);
        assert!(total < 8_400_000);
    }

    /// GIVEN a tile that stops being allowed WHEN synced THEN its picture goes
    /// and later pictures are dropped; the other tiles keep theirs.
    #[test]
    fn given_a_tile_that_may_not_show_then_its_picture_is_cleared() {
        let store = open_store();
        assert!(store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        assert!(store.insert(2, 0, 2, 2, rgba(2, 2, 1)));
        store.sync(false, [true, false, true, true, true]);
        assert!(store.newest(0, 0, store.stream(0)).is_none());
        assert!(store.newest(2, 0, store.stream(2)).is_none());
        assert!(!store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        assert!(!store.insert(2, 0, 2, 2, rgba(2, 2, 1)));
        assert!(store.insert(1, 0, 2, 2, rgba(2, 2, 1)));
        assert_eq!(store.kept(), 1);
    }

    /// GIVEN a stored picture WHEN its stream changes and the controller
    /// applies the change THEN it goes, the tile accepts the next picture,
    /// and the numbers keep rising; a tile out of range changes nothing.
    #[test]
    fn given_an_applied_stream_change_then_the_tile_accepts_later_pictures() {
        let store = open_store();
        assert!(store.insert(3, 0, 2, 2, rgba(2, 2, 1)));
        store.stream_changed(3);
        store.stream_changed(9);
        assert_eq!(store.stream_applied(9), 0, "out of range");
        assert!(store.newest(3, 0, store.stream(3)).is_none());
        let before = store.stream(3);
        let stream = store.stream_applied(3);
        assert_ne!(stream, before, "a new stream number");
        assert_eq!(store.stream(3), stream);
        assert!(store.insert(3, 0, 2, 2, rgba(2, 2, 1)));
        assert_eq!(store.newest(3, 0, stream).unwrap().seq, 2);
        assert!(
            store.newest(3, 0, before).is_none(),
            "the old number gets nothing"
        );
    }

    /// GIVEN tile 1 shows stream A WHEN the read thread reads that A stopped
    /// and B started, and B's pictures arrive before the controller applies
    /// both changes THEN B's pictures are refused; after the second change
    /// is applied they are kept. Other tiles are not affected.
    #[test]
    fn given_unapplied_stream_change_then_new_pictures_wait_for_the_controller() {
        let store = open_store();
        assert!(store.insert(1, 0, 2, 2, rgba(2, 2, 1)));
        store.stream_changed(1); // VideoStopped (A)
        store.stream_changed(1); // VideoStarted (B)
        assert!(
            store.newest(1, 0, store.stream(1)).is_none(),
            "A's picture goes at once"
        );
        assert!(!store.insert(1, 0, 2, 2, rgba(2, 2, 2)), "B waits");
        assert!(
            store.insert(2, 0, 2, 2, rgba(2, 2, 3)),
            "tile 2 is not affected"
        );
        let first = store.stream_applied(1);
        assert!(
            !store.insert(1, 0, 2, 2, rgba(2, 2, 2)),
            "one change is left"
        );
        let second = store.stream_applied(1);
        assert!(store.insert(1, 0, 2, 2, rgba(2, 2, 2)));
        assert_eq!(store.newest(1, 0, second).unwrap().bytes()[0], 2);
        assert!(
            store.newest(1, 0, first).is_none(),
            "a request for an older stream gets nothing"
        );
    }

    /// GIVEN a picture on the own tile WHEN the camera is turned off THEN it
    /// goes, and later pictures are refused, also after a sync that would
    /// allow the tile, until the latest camera-on is taken and synced.
    #[test]
    fn given_a_camera_off_then_pictures_wait_for_the_latest_on_and_a_sync() {
        let store = open_store();
        assert!(store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        let off = store.camera_request(false);
        assert!(store.newest(0, 0, store.stream(0)).is_none());
        assert!(!store.insert(0, 0, 2, 2, rgba(2, 2, 2)));
        assert!(store.insert(1, 0, 2, 2, rgba(2, 2, 2)), "remote tiles stay");
        store.sync(true, [true; 5]);
        assert!(!store.insert(0, 0, 2, 2, rgba(2, 2, 3)), "still blocked");
        store.camera_on_taken(off);
        let on = store.camera_request(true);
        assert!(!store.insert(0, 0, 2, 2, rgba(2, 2, 3)), "not taken yet");
        store.camera_on_taken(on);
        assert!(!store.insert(0, 0, 2, 2, rgba(2, 2, 3)), "not allowed yet");
        store.sync(true, [true; 5]);
        assert!(store.insert(0, 0, 2, 2, rgba(2, 2, 3)));
    }

    /// GIVEN a camera-on followed by an off before the task took either WHEN
    /// the task takes the older on THEN the own tile stays blocked, also
    /// across a sync from a view that still has the camera on.
    #[test]
    fn given_an_on_queued_before_an_off_then_it_cannot_lift_the_block() {
        let store = open_store();
        let on = store.camera_request(true);
        let _off = store.camera_request(false);
        store.camera_on_taken(on);
        store.sync(true, [false; 5]);
        assert!(!store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        let again = store.camera_request(true);
        store.camera_on_taken(again);
        store.sync(true, [false; 5]);
        assert!(
            store.insert(0, 0, 2, 2, rgba(2, 2, 1)),
            "a later on lifts it"
        );
    }

    /// GIVEN a closed store WHEN pictures arrive or tiles sync THEN nothing
    /// is kept, and the earlier pictures are gone.
    #[test]
    fn given_a_closed_store_then_nothing_is_kept() {
        let store = open_store();
        assert!(store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        store.close();
        assert_eq!(store.kept(), 0);
        store.sync(true, [true; 5]);
        assert!(!store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        assert!(store.newest(0, 0, store.stream(0)).is_none());
    }

    /// GIVEN a notifier WHEN pictures are kept or dropped THEN it runs once
    /// for each kept picture only, and it may use the store.
    #[test]
    fn given_a_notifier_then_it_runs_for_kept_pictures_only() {
        let store = open_store();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(AtomicUsize::new(0));
        let (counter, probe, inner) = (count.clone(), seen.clone(), store.clone());
        store.set_notifier(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            // The lock is free here, so the notifier can read the store.
            if inner.newest(0, 0, inner.stream(0)).is_some() {
                probe.fetch_add(1, Ordering::SeqCst);
            }
        }));
        assert!(store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        assert!(!store.insert(0, 9, 2, 2, rgba(2, 2, 1)));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    /// GIVEN a picture number at the JavaScript limit WHEN another arrives
    /// THEN it is dropped, so a number never passes the safe range.
    #[test]
    fn given_the_last_safe_number_then_later_pictures_are_dropped() {
        let store = open_store();
        store.state().seq = MAX_SEQ - 1;
        assert!(store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        assert_eq!(store.newest(0, 0, store.stream(0)).unwrap().seq, MAX_SEQ);
        assert!(!store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
    }

    /// GIVEN a reader that holds a picture WHEN the tile gets a newer one
    /// THEN the reader's bytes are unchanged (they are shared, not reused).
    #[test]
    fn given_a_held_picture_then_a_newer_one_does_not_change_it() {
        let store = open_store();
        assert!(store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        let held = store.newest(0, 0, store.stream(0)).unwrap();
        assert!(store.insert(0, 0, 2, 2, rgba(2, 2, 2)));
        assert_eq!(held.bytes()[0], 1);
        assert_eq!(store.newest(0, 0, store.stream(0)).unwrap().bytes()[0], 2);
    }

    /// GIVEN a stream count at the largest JavaScript-safe number WHEN it
    /// moves on THEN it wraps to 0, so the page can read every number.
    #[test]
    fn given_the_largest_stream_count_then_the_next_one_wraps_to_zero() {
        assert_eq!(next_count(0), 1);
        assert_eq!(next_count(MAX_SEQ - 1), MAX_SEQ);
        assert_eq!(next_count(MAX_SEQ), 0);
    }

    /// GIVEN two calls' stores WHEN each has a picture on tile 1 THEN their
    /// stream numbers differ, so a request with the first call's number gets
    /// nothing from the second call's store, and each store answers its own.
    #[test]
    fn given_two_calls_then_their_stream_numbers_never_match() {
        let first = open_store();
        let second = open_store();
        assert!(first.insert(1, 0, 2, 2, rgba(2, 2, 1)));
        assert!(second.insert(1, 0, 2, 2, rgba(2, 2, 2)));
        for tile in 0..5 {
            assert_ne!(first.stream(tile), second.stream(tile));
        }
        assert!(second.newest(1, 0, first.stream(1)).is_none());
        assert_eq!(second.newest(1, 0, second.stream(1)).unwrap().bytes()[0], 2);
        assert_eq!(first.newest(1, 0, first.stream(1)).unwrap().bytes()[0], 1);
    }
}

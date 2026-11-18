//! The newest call video picture on each tile (ADR-0048).
//!
//! Pictures do not use the worker's event queue: that queue blocks the read
//! thread when the controller is slow, and a picture is worth nothing once a
//! newer one exists. The store keeps at most one picture per tile (five in
//! all, each at most [`persephone_call_protocol::MAX_VIDEO_BYTES`]), so a
//! slow page loses pictures and never grows memory.
//!
//! Two threads share one [`VideoStore`]:
//!
//! * the worker read thread calls [`VideoStore::insert`] for each picture;
//! * the controller task calls [`VideoStore::sync`], [`VideoStore::clear_tile`]
//!   and [`VideoStore::close`], and the request loop calls
//!   [`VideoStore::newest`].
//!
//! The store lock is held only to read or replace small values. Pixel bytes
//! are shared (`Arc`), so a reader copies a pointer, not a picture, and
//! never writes to the host pipe under the lock. The notifier runs after the
//! lock is released.
//!
//! A picture is kept only when the controller has said its tile may show:
//! tile 0 while the camera is on, tiles 1 to 4 after the worker's
//! `VideoStarted` for a current participant. [`VideoStore::close`] ends the
//! store for good: it drops every picture and refuses later ones.

use persephone_call_protocol::{MAX_VIDEO_PIXELS, MAX_VIDEO_ROTATION, MAX_VIDEO_SIDE, VIDEO_TILES};
use std::sync::{Arc, Mutex, MutexGuard};

/// Tiles in a call: 0 is the own camera, 1 to 4 are remote.
const TILES: usize = VIDEO_TILES as usize;

/// Largest picture number: the page reads it as a JavaScript number.
const MAX_SEQ: u64 = 9_007_199_254_740_991;

/// One kept picture. Cloning shares the pixel bytes.
#[derive(Clone, Debug)]
pub struct Picture {
    /// Rises by one for each kept picture, across all tiles; the first is 1.
    pub seq: u64,
    pub rotation: u8,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
}

/// Tells the owner that a picture arrived. It must not block.
pub type Notifier = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct State {
    /// Which tiles may keep a picture now.
    allowed: [bool; TILES],
    pictures: [Option<Picture>; TILES],
    seq: u64,
    closed: bool,
    notifier: Option<Notifier>,
}

/// The shared picture store of one call worker. Clones share it.
#[derive(Clone, Default)]
pub struct VideoStore {
    state: Arc<Mutex<State>>,
}

impl VideoStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A poisoned lock only means a thread panicked; the data stays plain.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sets what runs after each kept picture. Replaces an earlier one.
    pub fn set_notifier(&self, notifier: Notifier) {
        self.state().notifier = Some(notifier);
    }

    /// Keeps `rgba` as the newest picture of `tile`, if the tile may show and
    /// the picture is inside the video limits. Returns whether it was kept;
    /// a refused picture is dropped.
    pub fn insert(&self, tile: u8, rotation: u8, width: u32, height: u32, rgba: Vec<u8>) -> bool {
        let pixels = u64::from(width) * u64::from(height);
        if usize::from(tile) >= TILES
            || rotation > MAX_VIDEO_ROTATION
            || !(1..=MAX_VIDEO_SIDE).contains(&width)
            || !(1..=MAX_VIDEO_SIDE).contains(&height)
            || pixels > u64::from(MAX_VIDEO_PIXELS)
            || u64::try_from(rgba.len()).ok() != Some(pixels * 4)
        {
            return false;
        }
        // The bytes are wrapped before the lock: only pointers move inside it.
        let rgba = Arc::new(rgba);
        let notifier = {
            let mut state = self.state();
            if state.closed || !state.allowed[usize::from(tile)] || state.seq >= MAX_SEQ {
                return false;
            }
            state.seq += 1;
            let seq = state.seq;
            state.pictures[usize::from(tile)] = Some(Picture {
                seq,
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

    /// The newest picture of `tile` if its number is above `after`.
    pub fn newest(&self, tile: u8, after: u64) -> Option<Picture> {
        let state = self.state();
        state
            .pictures
            .get(usize::from(tile))?
            .as_ref()
            .filter(|picture| picture.seq > after)
            .cloned()
    }

    /// Sets which tiles may show: the own tile (`own`) and tiles 1 to 4
    /// (`remote[0]` is tile 1). A tile that may not show loses its picture.
    pub fn sync(&self, own: bool, remote: [bool; TILES - 1]) {
        let mut state = self.state();
        if state.closed {
            return;
        }
        let allowed = [own, remote[0], remote[1], remote[2], remote[3]];
        for (tile, allow) in allowed.into_iter().enumerate() {
            state.allowed[tile] = allow;
            if !allow {
                state.pictures[tile] = None;
            }
        }
    }

    /// Drops the picture of `tile`, as when its stream stops or changes.
    pub fn clear_tile(&self, tile: u8) {
        if let Some(slot) = self.state().pictures.get_mut(usize::from(tile)) {
            *slot = None;
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
        store.sync(true, [true; 4]);
        store
    }

    /// GIVEN a store where no tile may show WHEN pictures arrive THEN every
    /// one is dropped.
    #[test]
    fn given_no_allowed_tile_then_pictures_are_dropped() {
        let store = VideoStore::new();
        for tile in 0..5 {
            assert!(!store.insert(tile, 0, 2, 2, rgba(2, 2, 1)));
            assert!(store.newest(tile, 0).is_none());
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
        let own = store.newest(0, 0).expect("own picture");
        assert_eq!((own.seq, own.rotation, own.rgba[0]), (3, 1, 3));
        let remote = store.newest(1, 0).expect("remote picture");
        assert_eq!((remote.seq, remote.width, remote.height), (2, 4, 2));
        assert!(store.newest(0, 3).is_none(), "nothing newer than 3");
        assert!(store.newest(0, 2).is_some());
        assert_eq!(store.kept(), 2, "one picture per tile");
    }

    /// GIVEN bad tile, rotation, size or length WHEN inserted THEN the picture
    /// is dropped and the stored one stays.
    #[test]
    fn given_pictures_outside_the_limits_then_they_are_dropped() {
        let store = open_store();
        assert!(store.insert(1, 0, 2, 2, rgba(2, 2, 7)));
        let cases: [(u8, u8, u32, u32, usize); 8] = [
            (5, 0, 2, 2, 16),
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
        let kept = store.newest(1, 0).expect("the first picture stays");
        assert_eq!((kept.seq, kept.rgba[0]), (1, 7));
        let largest = store.insert(1, 0, 640, 360, rgba(640, 360, 1));
        assert!(largest, "the largest allowed picture is kept");
    }

    /// GIVEN a tile that stops being allowed WHEN synced THEN its picture goes
    /// and later pictures are dropped; the other tiles keep theirs.
    #[test]
    fn given_a_tile_that_may_not_show_then_its_picture_is_cleared() {
        let store = open_store();
        assert!(store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        assert!(store.insert(2, 0, 2, 2, rgba(2, 2, 1)));
        store.sync(false, [true, false, true, true]);
        assert!(store.newest(0, 0).is_none());
        assert!(store.newest(2, 0).is_none());
        assert!(!store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        assert!(!store.insert(2, 0, 2, 2, rgba(2, 2, 1)));
        assert!(store.insert(1, 0, 2, 2, rgba(2, 2, 1)));
        assert_eq!(store.kept(), 1);
    }

    /// GIVEN a stored picture WHEN its tile is cleared THEN it goes, the tile
    /// still accepts the next picture, and the numbers keep rising.
    #[test]
    fn given_a_cleared_tile_then_it_accepts_later_pictures_with_a_higher_number() {
        let store = open_store();
        assert!(store.insert(3, 0, 2, 2, rgba(2, 2, 1)));
        store.clear_tile(3);
        store.clear_tile(9);
        assert!(store.newest(3, 0).is_none());
        assert!(store.insert(3, 0, 2, 2, rgba(2, 2, 1)));
        assert_eq!(store.newest(3, 0).unwrap().seq, 2);
    }

    /// GIVEN a closed store WHEN pictures arrive or tiles sync THEN nothing
    /// is kept, and the earlier pictures are gone.
    #[test]
    fn given_a_closed_store_then_nothing_is_kept() {
        let store = open_store();
        assert!(store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        store.close();
        assert_eq!(store.kept(), 0);
        store.sync(true, [true; 4]);
        assert!(!store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        assert!(store.newest(0, 0).is_none());
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
            if inner.newest(0, 0).is_some() {
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
        assert_eq!(store.newest(0, 0).unwrap().seq, MAX_SEQ);
        assert!(!store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
    }

    /// GIVEN a reader that holds a picture WHEN the tile gets a newer one
    /// THEN the reader's bytes are unchanged (they are shared, not reused).
    #[test]
    fn given_a_held_picture_then_a_newer_one_does_not_change_it() {
        let store = open_store();
        assert!(store.insert(0, 0, 2, 2, rgba(2, 2, 1)));
        let held = store.newest(0, 0).unwrap();
        assert!(store.insert(0, 0, 2, 2, rgba(2, 2, 2)));
        assert_eq!(held.rgba[0], 1);
        assert_eq!(store.newest(0, 0).unwrap().rgba[0], 2);
    }
}

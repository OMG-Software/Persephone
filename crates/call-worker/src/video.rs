//! The pure parts of video (ADR-0048): no LiveKit types, no I/O, no clock
//! reads. Tile assignment, fit scaling, the picture rate limit, rotation and
//! camera choice live here so they can be unit-tested.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use persephone_call_protocol::{
    CameraState, MAX_VIDEO_PIXELS, MAX_VIDEO_ROTATION, MAX_VIDEO_SIDE, SCREEN_TILE, max_fps,
    picture_fits,
};

/// The tile of the own camera.
pub const OWN_TILE: u8 = 0;
/// Remote camera tiles: 1 up to and including `SCREEN_TILE - 1`.
const REMOTE_TILES: usize = (SCREEN_TILE - 1) as usize;
/// Remote tracks that may wait for a free tile.
pub const MAX_WAITING: usize = 32;
/// The long side of the picture box. The short side is [`BOX_SHORT`].
const BOX_LONG: u32 = MAX_VIDEO_SIDE;
const BOX_SHORT: u32 = MAX_VIDEO_PIXELS / MAX_VIDEO_SIDE;
/// The box of a published screen share (ADR-0053): 1920x1080.
const PUBLISH_LONG: u32 = 1920;
const PUBLISH_SHORT: u32 = 1080;

/// The size to send for a picture of `width` x `height`: the largest size that
/// fits 640x360 (360x640 for a tall picture) with the same aspect. A picture
/// that already fits keeps its size, so a small camera is never enlarged.
/// Both sides are at least 1. Returns `None` for an empty picture.
pub fn fit_within_box(width: u32, height: u32) -> Option<(u32, u32)> {
    fit_in(width, height, BOX_LONG, BOX_SHORT)
}

/// The size to publish for a screen capture of `width` x `height`: the
/// largest size that fits 1920x1080 (1080x1920 for a tall screen) with the
/// same aspect. A capture that already fits keeps its size.
pub fn fit_publish_box(width: u32, height: u32) -> Option<(u32, u32)> {
    fit_in(width, height, PUBLISH_LONG, PUBLISH_SHORT)
}

/// The size to keep for a received picture on `tile`: the camera box, or for
/// the screen tile the 1280x720 screen box (720x1280 for a tall one).
pub fn fit_for_tile(tile: u8, width: u32, height: u32) -> Option<(u32, u32)> {
    if tile == SCREEN_TILE {
        fit_in(
            width,
            height,
            persephone_call_protocol::MAX_SCREEN_SIDE,
            persephone_call_protocol::MAX_SCREEN_PIXELS / persephone_call_protocol::MAX_SCREEN_SIDE,
        )
    } else {
        fit_within_box(width, height)
    }
}

fn fit_in(width: u32, height: u32, long: u32, short: u32) -> Option<(u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    let (box_w, box_h) = if height > width {
        (short, long)
    } else {
        (long, short)
    };
    if width <= box_w && height <= box_h {
        return Some((width, height));
    }
    let (width, height) = (u64::from(width), u64::from(height));
    let (box_w, box_h) = (u64::from(box_w), u64::from(box_h));
    // Compare width/height with box_w/box_h without a division.
    let (out_w, out_h) = if width * box_h >= height * box_w {
        (box_w, (height * box_w / width).max(1))
    } else {
        ((width * box_h / height).max(1), box_h)
    };
    Some((out_w as u32, out_h as u32))
}

/// Quarter turns clockwise for a rotation in degrees. Only the four right
/// angles are valid.
pub fn quarter_turns(degrees: u32) -> Option<u8> {
    let turns = match degrees {
        0 => 0,
        90 => 1,
        180 => 2,
        270 => 3,
        _ => return None,
    };
    debug_assert!(turns <= MAX_VIDEO_ROTATION);
    Some(turns)
}

/// The bytes in an RGBA picture on `tile`, or `None` when the size is outside
/// the limits of that tile (checked before any buffer is made).
pub fn rgba_len(tile: u8, width: u32, height: u32) -> Option<usize> {
    picture_fits(tile, width, height).then(|| width as usize * height as usize * 4)
}

/// Lets through at most the tile's pictures per second (15 for a camera, 5
/// for the screen), on a fixed schedule. A picture that comes a little late
/// does not move the schedule; a long gap restarts it. The screen tile also
/// keeps a full interval after each picture, so no one-second window holds
/// more than 5.
#[derive(Debug)]
pub struct RateLimiter {
    due: Option<Instant>,
    interval: Duration,
    /// The screen tile: the time of the last picture let through, when each
    /// picture must come a full interval after it.
    strict_last: Option<Option<Instant>>,
}

impl Default for RateLimiter {
    /// The camera limit.
    fn default() -> Self {
        Self::for_tile(OWN_TILE)
    }
}

impl RateLimiter {
    /// The limit of `tile`.
    pub fn for_tile(tile: u8) -> Self {
        Self {
            due: None,
            interval: Duration::from_nanos(1_000_000_000 / u64::from(max_fps(tile))),
            strict_last: (tile == SCREEN_TILE).then_some(None),
        }
    }
    /// A picture may come this much before its time, so that the jitter of a
    /// 30 fps source does not push the rate far below 15 fps. The schedule does
    /// not move, so the average rate never exceeds the limit.
    const EARLY: Duration = Duration::from_millis(8);

    pub fn admit(&mut self, now: Instant) -> bool {
        if let Some(Some(last)) = self.strict_last
            && now.saturating_duration_since(last) < self.interval
        {
            return false;
        }
        if !self.scheduled(now) {
            return false;
        }
        if let Some(last) = self.strict_last.as_mut() {
            *last = Some(now);
        }
        true
    }

    /// The fixed schedule: whether `now` is on time, and the next time.
    fn scheduled(&mut self, now: Instant) -> bool {
        let Some(due) = self.due else {
            self.due = Some(now + self.interval);
            return true;
        };
        if now + Self::EARLY < due {
            return false;
        }
        // On time or slightly late: keep the schedule, but never fall so far
        // behind that a burst could follow.
        self.due = Some((due + self.interval).max(now + self.interval / 2));
        true
    }
}

/// What happened when a track asked for a tile.
#[derive(Debug, Eq, PartialEq)]
pub enum Added {
    /// The track has this tile (1 to 4).
    Tile(u8),
    /// All tiles are busy; the track waits.
    Queued,
    /// The track is already known, or the waiting queue is full.
    Refused,
}

/// A tile that was freed, and the waiting track that took it, if any.
#[derive(Debug, Eq, PartialEq)]
pub struct Freed<K> {
    pub tile: u8,
    pub promoted: Option<K>,
}

/// Gives remote video tracks to tiles 1 to 4. When all are busy, up to
/// [`MAX_WAITING`] tracks wait. A freed tile goes to the longest waiting track.
#[derive(Debug)]
pub struct TileAssigner<K> {
    tiles: [Option<K>; REMOTE_TILES],
    waiting: VecDeque<K>,
}

impl<K> Default for TileAssigner<K> {
    fn default() -> Self {
        Self {
            tiles: std::array::from_fn(|_| None),
            waiting: VecDeque::new(),
        }
    }
}

impl<K: Clone + Eq> TileAssigner<K> {
    fn tile_number(slot: usize) -> u8 {
        slot as u8 + 1
    }

    pub fn add(&mut self, key: K) -> Added {
        if self.tiles.iter().flatten().any(|known| *known == key) || self.waiting.contains(&key) {
            return Added::Refused;
        }
        if let Some(slot) = self.tiles.iter().position(Option::is_none) {
            self.tiles[slot] = Some(key);
            return Added::Tile(Self::tile_number(slot));
        }
        if self.waiting.len() >= MAX_WAITING {
            return Added::Refused;
        }
        self.waiting.push_back(key);
        Added::Queued
    }

    /// Removes every track that `matches`. Waiting tracks leave silently. Each
    /// freed tile is listed in tile order, with the track that took it.
    pub fn remove_where(&mut self, matches: impl Fn(&K) -> bool) -> Vec<Freed<K>> {
        self.waiting.retain(|key| !matches(key));
        let mut freed = Vec::new();
        for slot in 0..REMOTE_TILES {
            if self.tiles[slot].as_ref().is_some_and(&matches) {
                self.tiles[slot] = self.waiting.pop_front();
                freed.push(Freed {
                    tile: Self::tile_number(slot),
                    promoted: self.tiles[slot].clone(),
                });
            }
        }
        freed
    }

    /// The track on a tile, if any.
    #[cfg(test)]
    pub fn on_tile(&self, tile: u8) -> Option<&K> {
        let slot = usize::from(tile).checked_sub(1)?;
        self.tiles.get(slot)?.as_ref()
    }

    #[cfg(test)]
    pub fn waiting_len(&self) -> usize {
        self.waiting.len()
    }
}

/// The one decoded remote screen share (ADR-0053). The first track that a
/// member publishes takes the screen tile; the others wait, up to
/// [`MAX_WAITING`], and the longest-waiting one takes the tile when the
/// decoded one ends.
#[derive(Debug)]
pub struct ScreenQueue<K> {
    current: Option<K>,
    waiting: VecDeque<K>,
}

impl<K> Default for ScreenQueue<K> {
    fn default() -> Self {
        Self {
            current: None,
            waiting: VecDeque::new(),
        }
    }
}

/// What a removal took out of a [`ScreenQueue`].
#[derive(Debug, Eq, PartialEq)]
pub struct ScreenRemoved<K> {
    /// Every track that matched, decoded or waiting.
    pub removed: Vec<K>,
    /// The decoded one ended: the track that took the tile, if any.
    pub freed: Option<Freed<K>>,
}

impl<K: Clone + Eq> ScreenQueue<K> {
    pub fn add(&mut self, key: K) -> Added {
        if self.current.as_ref() == Some(&key) || self.waiting.contains(&key) {
            return Added::Refused;
        }
        if self.current.is_none() {
            self.current = Some(key);
            return Added::Tile(SCREEN_TILE);
        }
        if self.waiting.len() >= MAX_WAITING {
            return Added::Refused;
        }
        self.waiting.push_back(key);
        Added::Queued
    }

    /// Removes every track that `matches`.
    pub fn remove_where(&mut self, matches: impl Fn(&K) -> bool) -> ScreenRemoved<K> {
        let mut removed = Vec::new();
        let mut kept = VecDeque::new();
        for key in self.waiting.drain(..) {
            if matches(&key) {
                removed.push(key);
            } else {
                kept.push_back(key);
            }
        }
        self.waiting = kept;
        let freed = if self.current.as_ref().is_some_and(&matches) {
            removed.extend(self.current.take());
            self.current = self.waiting.pop_front();
            Some(Freed {
                tile: SCREEN_TILE,
                promoted: self.current.clone(),
            })
        } else {
            None
        };
        ScreenRemoved { removed, freed }
    }

    /// Whether any track, decoded or waiting, `matches`.
    pub fn any(&self, matches: impl Fn(&K) -> bool) -> bool {
        self.current.iter().chain(self.waiting.iter()).any(matches)
    }

    #[cfg(test)]
    pub fn current(&self) -> Option<&K> {
        self.current.as_ref()
    }

    #[cfg(test)]
    pub fn waiting_len(&self) -> usize {
        self.waiting.len()
    }
}

/// Picks the camera. `wanted` is the chosen ID; `None` is the first camera the
/// system lists. A chosen ID that is not listed is `NotFound`; there is never a
/// fall back to another camera. An empty list with no choice is `Unavailable`.
pub fn pick_camera<'a>(
    wanted: Option<&str>,
    available: impl IntoIterator<Item = &'a str>,
) -> Result<String, CameraState> {
    let mut available = available.into_iter();
    match wanted {
        Some(wanted) => {
            if available.any(|id| id == wanted) {
                Ok(wanted.to_owned())
            } else {
                Err(CameraState::NotFound)
            }
        }
        None => available
            .next()
            .map(str::to_owned)
            .ok_or(CameraState::Unavailable),
    }
}

/// One format a camera offers.
// macOS picks its format through AVFoundation; Windows and Linux use this.
#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureMode {
    pub width: u32,
    pub height: u32,
    /// Frames per second, rounded down.
    pub fps: u32,
}

/// The index of the best mode: the smallest picture that still has at least
/// 640x360 (360x640 when the camera is tall), else the largest picture. On a
/// tie, the faster mode wins, counted up to 30 fps. A mode with a side of 0
/// is skipped.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn pick_capture_mode(modes: &[CaptureMode]) -> Option<usize> {
    let area = |mode: &CaptureMode| u64::from(mode.width) * u64::from(mode.height);
    let big_enough = |mode: &CaptureMode| {
        let (long, short) = (mode.width.max(mode.height), mode.width.min(mode.height));
        long >= BOX_LONG && short >= BOX_SHORT
    };
    let usable = || {
        modes
            .iter()
            .enumerate()
            .filter(|(_, mode)| mode.width > 0 && mode.height > 0)
    };
    // Smaller area is better among big enough modes; larger area otherwise.
    let best = if usable().any(|(_, mode)| big_enough(mode)) {
        usable()
            .filter(|(_, mode)| big_enough(mode))
            .min_by_key(|(index, mode)| (area(mode), std::cmp::Reverse(mode.fps.min(30)), *index))
    } else {
        usable().min_by_key(|(index, mode)| {
            (
                std::cmp::Reverse(area(mode)),
                std::cmp::Reverse(mode.fps.min(30)),
                *index,
            )
        })
    };
    best.map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    /// GIVEN pictures of many shapes WHEN fitted THEN they fit the box with the
    /// same aspect, are never enlarged and never have a zero side.
    #[test]
    fn given_pictures_when_fitted_then_they_fit_the_box() {
        let cases = [
            ((1920, 1080), (640, 360)),
            ((1280, 720), (640, 360)),
            ((640, 360), (640, 360)),
            ((320, 240), (320, 240)),
            ((1, 1), (1, 1)),
            ((1920, 1440), (480, 360)),
            ((1080, 1920), (360, 640)),
            ((1000, 1500), (360, 540)),
            ((360, 640), (360, 640)),
            ((4000, 1), (640, 1)),
            ((1, 4000), (1, 640)),
            ((640, 480), (480, 360)),
            ((1000, 1000), (360, 360)),
        ];
        for ((w, h), want) in cases {
            assert_eq!(fit_within_box(w, h), Some(want), "{w}x{h}");
            let (fw, fh) = want;
            assert!(
                rgba_len(OWN_TILE, fw, fh).is_some(),
                "{w}x{h} inside wire limits"
            );
        }
    }

    /// GIVEN an empty picture WHEN fitted THEN there is no size.
    #[test]
    fn given_empty_picture_when_fitted_then_there_is_no_size() {
        assert_eq!(fit_within_box(0, 360), None);
        assert_eq!(fit_within_box(640, 0), None);
    }

    /// GIVEN the extreme sizes a u32 allows WHEN fitted THEN there is no
    /// overflow and the result fits the wire.
    #[test]
    fn given_huge_picture_when_fitted_then_there_is_no_overflow() {
        let (w, h) = fit_within_box(u32::MAX, u32::MAX).unwrap();
        assert!(rgba_len(OWN_TILE, w, h).is_some());
        let (w, h) = fit_within_box(u32::MAX, 1).unwrap();
        assert_eq!((w, h), (640, 1));
    }

    /// GIVEN sizes outside the wire limits WHEN the byte count is asked THEN
    /// there is none.
    #[test]
    fn given_sizes_outside_the_limits_when_counted_then_they_are_refused() {
        assert_eq!(rgba_len(OWN_TILE, 640, 360), Some(921_600));
        assert_eq!(rgba_len(OWN_TILE, 360, 640), Some(921_600));
        assert_eq!(rgba_len(OWN_TILE, 1, 1), Some(4));
        for (w, h) in [
            (0, 1),
            (1, 0),
            (641, 1),
            (1, 641),
            (640, 361),
            (u32::MAX, 1),
        ] {
            assert_eq!(rgba_len(OWN_TILE, w, h), None, "{w}x{h}");
        }
    }

    /// GIVEN the screen tile WHEN sizes are counted THEN 1280x720 passes (and
    /// 720x1280), larger ones are refused, and the same size is refused on a
    /// camera tile.
    #[test]
    fn given_screen_sizes_when_counted_then_the_tile_decides() {
        assert_eq!(rgba_len(SCREEN_TILE, 1280, 720), Some(3_686_400));
        assert_eq!(rgba_len(SCREEN_TILE, 720, 1280), Some(3_686_400));
        for (w, h) in [(1281, 720), (1280, 721), (0, 1), (1, 0), (u32::MAX, 1)] {
            assert_eq!(rgba_len(SCREEN_TILE, w, h), None, "{w}x{h}");
        }
        for tile in 0..SCREEN_TILE {
            assert_eq!(rgba_len(tile, 1280, 720), None, "tile {tile}");
        }
        assert_eq!(rgba_len(persephone_call_protocol::VIDEO_TILES, 1, 1), None);
    }

    /// GIVEN screens of many sizes WHEN fitted for the screen tile and for
    /// publishing THEN they keep the aspect, are never enlarged and fit.
    #[test]
    fn given_screens_when_fitted_then_they_fit_their_boxes() {
        let tile_cases = [
            ((3840, 2160), (1280, 720)),
            ((1920, 1080), (1280, 720)),
            ((1280, 720), (1280, 720)),
            ((800, 600), (800, 600)),
            ((1080, 1920), (720, 1280)),
            ((720, 1280), (720, 1280)),
            ((u32::MAX, 1), (1280, 1)),
        ];
        for ((w, h), want) in tile_cases {
            assert_eq!(fit_for_tile(SCREEN_TILE, w, h), Some(want), "{w}x{h}");
            assert!(rgba_len(SCREEN_TILE, want.0, want.1).is_some());
        }
        assert_eq!(fit_for_tile(1, 1920, 1080), Some((640, 360)));
        let publish_cases = [
            ((3840, 2160), (1920, 1080)),
            ((2560, 1440), (1920, 1080)),
            ((1920, 1080), (1920, 1080)),
            ((1366, 768), (1366, 768)),
            ((2160, 3840), (1080, 1920)),
            ((5120, 1440), (1920, 540)),
        ];
        for ((w, h), want) in publish_cases {
            assert_eq!(fit_publish_box(w, h), Some(want), "{w}x{h}");
        }
        assert_eq!(fit_publish_box(0, 1080), None);
    }

    /// GIVEN screen pictures arriving at 60 per second WHEN limited for the
    /// screen tile THEN at most 5 pass in one second.
    #[test]
    fn given_a_fast_screen_when_limited_then_five_per_second_pass() {
        let start = Instant::now();
        let mut limiter = RateLimiter::for_tile(SCREEN_TILE);
        let passed = (0..120)
            .filter(|frame| limiter.admit(at(start, frame * 1000 / 60)))
            .count();
        assert_eq!(passed, 10, "two seconds of 60 per second");
        let mut camera = RateLimiter::for_tile(0);
        let passed = (0..60)
            .filter(|frame| camera.admit(at(start, frame * 1000 / 60)))
            .count();
        assert!((14..=16).contains(&passed), "{passed}");
    }

    /// GIVEN screen pictures that come a little before each schedule time
    /// WHEN limited for the screen tile THEN no one-second window holds more
    /// than 5, while a camera keeps its early allowance.
    #[test]
    fn given_early_screen_pictures_when_limited_then_no_second_holds_more_than_five() {
        let start = Instant::now();
        let mut limiter = RateLimiter::for_tile(SCREEN_TILE);
        let times = [0, 192, 392, 592, 792, 992];
        let passed: Vec<u64> = times
            .into_iter()
            .filter(|millis| limiter.admit(at(start, *millis)))
            .collect();
        assert!(passed.len() <= 5, "{passed:?}");

        // A jittery 30 per second source for ten seconds.
        let mut limiter = RateLimiter::for_tile(SCREEN_TILE);
        let mut passed = Vec::new();
        for frame in 0..300_u64 {
            let millis = frame * 1000 / 30 + [0, 7, 3, 9][(frame % 4) as usize];
            if limiter.admit(at(start, millis)) {
                passed.push(millis);
            }
        }
        for (index, first) in passed.iter().enumerate() {
            let in_window = passed[index..]
                .iter()
                .take_while(|millis| **millis < first + 1000)
                .count();
            assert!(in_window <= 5, "{first}: {in_window}");
        }
        assert!(passed.len() >= 40, "only {}", passed.len());

        // The camera is not strict: a picture 8 ms early still passes.
        let mut camera = RateLimiter::for_tile(0);
        assert!(camera.admit(start));
        assert!(camera.admit(at(start, 59)));
    }

    /// GIVEN two screen shares WHEN they start THEN the first is decoded and
    /// the second waits; when the first ends the second takes the tile.
    #[test]
    fn given_two_screen_shares_then_one_is_decoded_and_the_other_waits() {
        let mut queue = ScreenQueue::default();
        assert_eq!(queue.add("a"), Added::Tile(SCREEN_TILE));
        assert_eq!(queue.add("b"), Added::Queued);
        assert_eq!(queue.add("c"), Added::Queued);
        assert_eq!(queue.add("a"), Added::Refused);
        assert_eq!(queue.add("b"), Added::Refused);
        assert_eq!(queue.current(), Some(&"a"));
        let ended = queue.remove_where(|key| *key == "a");
        assert_eq!(ended.removed, vec!["a"]);
        assert_eq!(
            ended.freed,
            Some(Freed {
                tile: SCREEN_TILE,
                promoted: Some("b")
            })
        );
        assert_eq!(queue.current(), Some(&"b"));
        assert_eq!(queue.waiting_len(), 1);
    }

    /// GIVEN a waiting share WHEN it ends THEN nothing changes for the decoded
    /// one; and when the last one ends the tile is free.
    #[test]
    fn given_waiting_share_ends_then_the_decoded_one_stays() {
        let mut queue = ScreenQueue::default();
        queue.add("a");
        queue.add("b");
        let ended = queue.remove_where(|key| *key == "b");
        assert_eq!(ended.removed, vec!["b"]);
        assert_eq!(ended.freed, None);
        assert_eq!(queue.current(), Some(&"a"));
        let ended = queue.remove_where(|key| *key == "a");
        assert_eq!(
            ended.freed,
            Some(Freed {
                tile: SCREEN_TILE,
                promoted: None
            })
        );
        assert_eq!(queue.current(), None);
        assert_eq!(queue.add("c"), Added::Tile(SCREEN_TILE));
    }

    /// GIVEN a full waiting queue WHEN one more share arrives THEN it is
    /// refused.
    #[test]
    fn given_a_full_screen_queue_then_one_more_is_refused() {
        let mut queue = ScreenQueue::default();
        assert_eq!(queue.add(0_usize), Added::Tile(SCREEN_TILE));
        for key in 1..=MAX_WAITING {
            assert_eq!(queue.add(key), Added::Queued);
        }
        assert_eq!(queue.add(MAX_WAITING + 1), Added::Refused);
        assert!(queue.any(|key| *key == 3));
        assert!(!queue.any(|key| *key == MAX_WAITING + 1));
    }

    /// GIVEN the four right angles WHEN mapped THEN they are quarter turns;
    /// any other angle is refused.
    #[test]
    fn given_rotation_degrees_when_mapped_then_only_right_angles_pass() {
        assert_eq!(quarter_turns(0), Some(0));
        assert_eq!(quarter_turns(90), Some(1));
        assert_eq!(quarter_turns(180), Some(2));
        assert_eq!(quarter_turns(270), Some(3));
        for bad in [1, 45, 91, 360, 450, u32::MAX] {
            assert_eq!(quarter_turns(bad), None, "{bad}");
        }
    }

    /// GIVEN a source faster than the limit WHEN pictures are offered THEN at
    /// most 15 per second pass, and the first passes at once.
    #[test]
    fn given_fast_source_when_limited_then_at_most_15_per_second() {
        for step_us in [1_000_u64, 8_333, 16_667, 33_333, 40_000] {
            let start = Instant::now();
            let mut limiter = RateLimiter::default();
            assert!(limiter.admit(start));
            let mut passed = 1_u32;
            let mut t = step_us;
            while t < 10_000_000 {
                if limiter.admit(start + Duration::from_micros(t)) {
                    passed += 1;
                }
                t += step_us;
            }
            // 10 seconds: 150 on schedule, one more for the first picture.
            assert!(passed <= 151, "step {step_us}: {passed}");
            if step_us <= 33_333 {
                assert!(passed >= 140, "step {step_us}: only {passed}");
            }
        }
    }

    /// GIVEN a source slower than the limit WHEN pictures are offered THEN all
    /// pass.
    #[test]
    fn given_slow_source_when_limited_then_every_picture_passes() {
        let start = Instant::now();
        let mut limiter = RateLimiter::default();
        for i in 0..20 {
            assert!(limiter.admit(at(start, i * 100)), "picture {i}");
        }
    }

    /// GIVEN a long pause WHEN pictures resume THEN the schedule restarts and
    /// no burst follows.
    #[test]
    fn given_long_pause_when_resumed_then_no_burst_follows() {
        let start = Instant::now();
        let mut limiter = RateLimiter::default();
        assert!(limiter.admit(start));
        assert!(limiter.admit(at(start, 5_000)));
        let burst = (1..=3)
            .filter(|i| limiter.admit(at(start, 5_000 + i * 2)))
            .count();
        assert_eq!(burst, 0);
    }

    /// GIVEN more tracks than tiles WHEN they are added THEN tiles 1 to 4 fill
    /// in order, then tracks wait, and a known track is refused.
    #[test]
    fn given_tracks_when_added_then_tiles_fill_then_queue() {
        let mut tiles = TileAssigner::<u32>::default();
        for (key, tile) in [(10, 1), (11, 2), (12, 3), (13, 4)] {
            assert_eq!(tiles.add(key), Added::Tile(tile));
        }
        assert_eq!(tiles.add(14), Added::Queued);
        assert_eq!(tiles.add(10), Added::Refused);
        assert_eq!(tiles.add(14), Added::Refused);
        assert_eq!(tiles.waiting_len(), 1);
        assert_eq!(tiles.on_tile(0), None);
        assert_eq!(tiles.on_tile(5), None);
    }

    /// GIVEN waiting tracks WHEN a tile is freed THEN the longest waiting track
    /// takes that tile.
    #[test]
    fn given_waiting_tracks_when_tile_freed_then_longest_waiting_takes_it() {
        let mut tiles = TileAssigner::<u32>::default();
        for key in 0..4 {
            tiles.add(key);
        }
        tiles.add(100);
        tiles.add(101);
        let freed = tiles.remove_where(|key| *key == 2);
        assert_eq!(
            freed,
            vec![Freed {
                tile: 3,
                promoted: Some(100)
            }]
        );
        assert_eq!(tiles.on_tile(3), Some(&100));
        assert_eq!(tiles.waiting_len(), 1);
        let freed = tiles.remove_where(|key| *key == 0);
        assert_eq!(freed[0].promoted, Some(101));
    }

    /// GIVEN no waiting track WHEN a tile is freed THEN the tile is empty and
    /// can be given again, lowest first.
    #[test]
    fn given_no_waiting_track_when_tile_freed_then_it_is_reused() {
        let mut tiles = TileAssigner::<u32>::default();
        for key in 0..4 {
            tiles.add(key);
        }
        let freed = tiles.remove_where(|key| *key == 1 || *key == 3);
        let freed_tiles: Vec<_> = freed.iter().map(|f| (f.tile, f.promoted)).collect();
        assert_eq!(freed_tiles, vec![(2, None), (4, None)]);
        assert_eq!(tiles.add(50), Added::Tile(2));
        assert_eq!(tiles.add(51), Added::Tile(4));
    }

    /// GIVEN a waiting track WHEN it is removed THEN it leaves without a freed
    /// tile; removing an unknown track does nothing.
    #[test]
    fn given_waiting_track_when_removed_then_no_tile_is_freed() {
        let mut tiles = TileAssigner::<u32>::default();
        for key in 0..5 {
            tiles.add(key);
        }
        assert!(tiles.remove_where(|key| *key == 4).is_empty());
        assert_eq!(tiles.waiting_len(), 0);
        assert!(tiles.remove_where(|key| *key == 99).is_empty());
    }

    /// GIVEN a full queue WHEN another track comes THEN it is refused and the
    /// queue does not grow.
    #[test]
    fn given_full_queue_when_track_added_then_it_is_refused() {
        let mut tiles = TileAssigner::<u32>::default();
        for key in 0..4 {
            assert!(matches!(tiles.add(key), Added::Tile(_)));
        }
        for key in 100..100 + MAX_WAITING as u32 {
            assert_eq!(tiles.add(key), Added::Queued);
        }
        assert_eq!(tiles.add(999), Added::Refused);
        assert_eq!(tiles.waiting_len(), MAX_WAITING);
    }

    /// GIVEN one participant with a tile and one waiting WHEN all of that
    /// participant's tracks go THEN the other takes over.
    #[test]
    fn given_participant_leaves_when_all_tracks_removed_then_next_is_promoted() {
        let mut tiles = TileAssigner::<(&str, u8)>::default();
        for key in [("a", 1), ("b", 1), ("c", 1), ("d", 1), ("e", 1)] {
            tiles.add(key);
        }
        let freed = tiles.remove_where(|(who, _)| *who == "a");
        assert_eq!(freed.len(), 1);
        assert_eq!(freed[0].tile, 1);
        assert_eq!(freed[0].promoted, Some(("e", 1)));
    }

    /// GIVEN a chosen ID WHEN it is listed THEN it is picked; when it is not
    /// listed THEN it is NotFound and no other camera is used.
    #[test]
    fn given_chosen_camera_when_absent_then_not_found_without_fallback() {
        let list = ["cam-a", "cam-b"];
        assert_eq!(pick_camera(Some("cam-b"), list), Ok("cam-b".to_owned()));
        assert_eq!(pick_camera(Some("cam-z"), list), Err(CameraState::NotFound));
        assert_eq!(pick_camera(Some("cam-z"), []), Err(CameraState::NotFound));
        assert_eq!(pick_camera(Some(""), list), Err(CameraState::NotFound));
    }

    /// GIVEN no choice WHEN cameras are listed THEN the first is used; with no
    /// camera at all the state is Unavailable.
    #[test]
    fn given_no_choice_when_listed_then_first_camera_else_unavailable() {
        assert_eq!(pick_camera(None, ["x", "y"]), Ok("x".to_owned()));
        assert_eq!(pick_camera(None, []), Err(CameraState::Unavailable));
    }

    fn mode(width: u32, height: u32, fps: u32) -> CaptureMode {
        CaptureMode { width, height, fps }
    }

    /// GIVEN camera modes WHEN one is picked THEN it is the smallest that is
    /// big enough, the faster one on a tie, else the largest.
    #[test]
    fn given_camera_modes_when_picked_then_closest_big_enough_wins() {
        let modes = [
            mode(320, 240, 30),
            mode(640, 480, 30),
            mode(1280, 720, 30),
            mode(640, 360, 15),
            mode(640, 360, 30),
        ];
        assert_eq!(pick_capture_mode(&modes), Some(4));
        let small = [mode(160, 120, 30), mode(320, 240, 15), mode(320, 240, 30)];
        assert_eq!(pick_capture_mode(&small), Some(2));
        let tall = [mode(360, 640, 30), mode(720, 1280, 30)];
        assert_eq!(pick_capture_mode(&tall), Some(0));
    }

    /// GIVEN no modes or only empty modes WHEN one is picked THEN there is
    /// none.
    #[test]
    fn given_no_usable_modes_when_picked_then_none() {
        assert_eq!(pick_capture_mode(&[]), None);
        assert_eq!(
            pick_capture_mode(&[mode(0, 480, 30), mode(640, 0, 30)]),
            None
        );
        assert_eq!(
            pick_capture_mode(&[mode(0, 480, 30), mode(640, 480, 30)]),
            Some(1)
        );
    }
}

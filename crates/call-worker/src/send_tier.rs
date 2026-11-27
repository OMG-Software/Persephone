//! The send tiers of the own camera (ADR-0070, proposed). Pure: no LiveKit
//! types, no I/O, no clock reads, so it is unit-tested on every OS.
//!
//! A tier is a picture box and the VP8 bitrate that goes with it at 15
//! frames per second. The wire carries a [`VideoQuality`]; the worker owns
//! this table, so no size crosses the wire.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use persephone_call_protocol::VideoQuality;

/// One step of the own camera's send size, smallest first.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SendTier {
    /// The Auto floor, and the cap while a video effect runs.
    P360,
    P480,
    P720,
    P1080,
    P1440,
}

impl SendTier {
    /// Every tier, smallest first.
    pub const ALL: [Self; 5] = [Self::P360, Self::P480, Self::P720, Self::P1080, Self::P1440];

    /// The long and the short side of the box. A tall picture swaps them.
    pub const fn long_short(self) -> (u32, u32) {
        match self {
            Self::P360 => (640, 360),
            Self::P480 => (854, 480),
            Self::P720 => (1280, 720),
            Self::P1080 => (1920, 1080),
            Self::P1440 => (2560, 1440),
        }
    }

    /// The VP8 bitrate in bit/s at 15 frames per second.
    pub const fn bitrate(self) -> u64 {
        match self {
            Self::P360 => 600_000,
            Self::P480 => 900_000,
            Self::P720 => 1_500_000,
            Self::P1080 => 2_500_000,
            Self::P1440 => 4_000_000,
        }
    }

    /// The next larger tier, if any.
    pub fn up(self) -> Option<Self> {
        Self::ALL.get(self.index() + 1).copied()
    }

    /// The next smaller tier, if any.
    pub fn down(self) -> Option<Self> {
        self.index().checked_sub(1).map(|index| Self::ALL[index])
    }

    fn index(self) -> usize {
        self as usize
    }

    /// The most the user's quality lets the camera send: the named tier, or
    /// the largest one for `Auto` (the controller then stays at or below it).
    pub fn top_for(quality: VideoQuality) -> Self {
        match quality {
            VideoQuality::Auto | VideoQuality::P1440 => Self::P1440,
            VideoQuality::P480 => Self::P480,
            VideoQuality::P720 => Self::P720,
            VideoQuality::P1080 => Self::P1080,
        }
    }

    /// The macOS session presets to try, as sizes, best first (ADR-0070).
    /// macOS gives at most 1280x720 and nobody tested other values there:
    /// from `P720` up the 1280x720 preset comes first, below it the order
    /// stays 960x540, 1280x720, 640x480 as before.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn preset_order(self) -> [(u32, u32); 3] {
        if self >= Self::P720 {
            [(1280, 720), (960, 540), (640, 480)]
        } else {
            [(960, 540), (1280, 720), (640, 480)]
        }
    }

    /// The tier to send now: `self` while the picture is plain, at most
    /// `P360` while a video effect runs. The effect code (`effects.rs`) and
    /// its segmenter work on pictures of at most 640x360.
    pub fn with_effect(self, plain: bool) -> Self {
        if plain { self } else { self.min(Self::P360) }
    }

    /// The top tier that a camera allows: `top`, or the tier that its first
    /// picture covers when that is smaller. With no picture seen, `top`: the
    /// send path never enlarges a picture.
    pub fn capped_by_capture(self, first: Option<(u32, u32)>) -> Self {
        first.map_or(self, |(width, height)| {
            self.min(Self::covered_by(width, height))
        })
    }

    /// The largest tier whose box a capture of `width` x `height` covers on
    /// both sides, in either orientation. A capture below 640x360 still
    /// gives `P360`: it is sent at its own size and never enlarged.
    pub fn covered_by(width: u32, height: u32) -> Self {
        let (long, short) = (width.max(height), width.min(height));
        Self::ALL
            .into_iter()
            .rev()
            .find(|tier| {
                let (tier_long, tier_short) = tier.long_short();
                long >= tier_long && short >= tier_short
            })
            .unwrap_or(Self::P360)
    }
}

/// The tier in use now, shared between the session (the writer) and the
/// camera's capture thread, which reads it on every frame, like
/// `SharedEffect`.
#[derive(Debug)]
pub struct SharedTier(AtomicU8);

impl SharedTier {
    pub fn new(tier: SendTier) -> Self {
        Self(AtomicU8::new(tier as u8))
    }

    pub fn get(&self) -> SendTier {
        let index = usize::from(self.0.load(Ordering::Relaxed));
        SendTier::ALL.get(index).copied().unwrap_or(SendTier::P360)
    }

    pub fn set(&self, tier: SendTier) {
        self.0.store(tier as u8, Ordering::Relaxed);
    }
}

/// The size of the first picture that a camera gave, kept by the capture
/// thread for the session. 0 until a picture comes.
#[derive(Debug, Default)]
pub struct FirstSize(AtomicU64);

impl FirstSize {
    /// Keeps the size of the first picture; later ones change nothing.
    pub fn note(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let packed = (u64::from(width) << 32) | u64::from(height);
        let _ = self
            .0
            .compare_exchange(0, packed, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// The size, once a picture with two sides over 0 came.
    pub fn get(&self) -> Option<(u32, u32)> {
        let packed = self.0.load(Ordering::Relaxed);
        let (width, height) = ((packed >> 32) as u32, packed as u32);
        (width > 0 && height > 0).then_some((width, height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN the tier table WHEN read THEN boxes and bitrates grow with the
    /// tier and the boxes keep the 16 by 9 shape.
    #[test]
    fn given_the_tiers_then_boxes_and_bitrates_grow() {
        let mut last = ((0, 0), 0);
        for tier in SendTier::ALL {
            let (long, short) = tier.long_short();
            assert!(long > last.0.0 && short > last.0.1, "{tier:?}");
            assert!(tier.bitrate() > last.1, "{tier:?}");
            assert!((i64::from(long) * 9 - i64::from(short) * 16).abs() <= 16);
            last = ((long, short), tier.bitrate());
        }
        assert_eq!(SendTier::P360.long_short(), (640, 360));
        assert_eq!(SendTier::P1440.long_short(), (2560, 1440));
        assert_eq!(SendTier::P360.bitrate(), 600_000);
        assert_eq!(SendTier::P1440.bitrate(), 4_000_000);
    }

    /// GIVEN each wire quality WHEN its top tier is asked THEN a fixed one
    /// names its tier and Auto reaches the largest.
    #[test]
    fn given_a_quality_then_its_top_tier_follows() {
        let tops: Vec<_> = VideoQuality::ALL.map(SendTier::top_for).to_vec();
        assert_eq!(
            tops,
            [
                SendTier::P1440,
                SendTier::P480,
                SendTier::P720,
                SendTier::P1080,
                SendTier::P1440
            ]
        );
    }

    /// GIVEN captures of many sizes WHEN the covered tier is asked THEN it is
    /// the largest box that the capture fills on both sides.
    #[test]
    fn given_a_capture_then_the_covered_tier_is_the_largest_that_fits() {
        let cases = [
            ((2560, 1440), SendTier::P1440),
            ((3840, 2160), SendTier::P1440),
            ((1920, 1080), SendTier::P1080),
            ((1280, 720), SendTier::P720),
            ((1280, 960), SendTier::P720),
            ((854, 480), SendTier::P480),
            ((720, 1280), SendTier::P720),
            ((640, 480), SendTier::P360),
            ((640, 360), SendTier::P360),
            ((320, 240), SendTier::P360),
            ((1, 1), SendTier::P360),
        ];
        for ((w, h), want) in cases {
            assert_eq!(SendTier::covered_by(w, h), want, "{w}x{h}");
        }
    }

    /// GIVEN a tier WHEN the macOS presets are ordered THEN 1280x720 comes
    /// first from P720 up and the old order stays below it.
    #[test]
    fn given_a_tier_then_the_macos_preset_order_follows() {
        let old = [(960, 540), (1280, 720), (640, 480)];
        let new = [(1280, 720), (960, 540), (640, 480)];
        for tier in [SendTier::P360, SendTier::P480] {
            assert_eq!(tier.preset_order(), old, "{tier:?}");
        }
        for tier in [SendTier::P720, SendTier::P1080, SendTier::P1440] {
            assert_eq!(tier.preset_order(), new, "{tier:?}");
        }
    }

    /// GIVEN a running effect WHEN the tier is asked THEN it is at most P360;
    /// with no effect it is the tier itself.
    #[test]
    fn given_an_effect_then_the_tier_is_capped_at_p360() {
        for tier in SendTier::ALL {
            assert_eq!(tier.with_effect(true), tier);
            assert_eq!(tier.with_effect(false), SendTier::P360);
        }
    }

    /// GIVEN a camera's first picture WHEN the top tier is capped THEN a
    /// small camera lowers it, a large one leaves it, and no picture leaves
    /// it as it is.
    #[test]
    fn given_the_first_picture_then_the_top_tier_is_capped() {
        assert_eq!(
            SendTier::P1440.capped_by_capture(Some((1280, 720))),
            SendTier::P720
        );
        assert_eq!(
            SendTier::P480.capped_by_capture(Some((2560, 1440))),
            SendTier::P480
        );
        assert_eq!(
            SendTier::P1080.capped_by_capture(Some((320, 240))),
            SendTier::P360
        );
        assert_eq!(SendTier::P1080.capped_by_capture(None), SendTier::P1080);
    }

    /// GIVEN pictures WHEN their size is kept THEN only the first one is.
    #[test]
    fn given_pictures_then_only_the_first_size_is_kept() {
        let first = FirstSize::default();
        assert_eq!(first.get(), None);
        first.note(1280, 720);
        first.note(640, 360);
        assert_eq!(first.get(), Some((1280, 720)));
        let empty = FirstSize::default();
        empty.note(0, 720);
        assert_eq!(empty.get(), None);
    }

    /// GIVEN the ends of the table WHEN a step is asked past them THEN there
    /// is none.
    #[test]
    fn given_the_ends_then_no_step_goes_past_them() {
        assert_eq!(SendTier::P360.down(), None);
        assert_eq!(SendTier::P1440.up(), None);
        assert_eq!(SendTier::P360.up(), Some(SendTier::P480));
        assert_eq!(SendTier::P1440.down(), Some(SendTier::P1080));
    }

    /// GIVEN a shared tier WHEN set THEN every tier reads back, and a bad
    /// stored value reads as the smallest tier.
    #[test]
    fn given_a_shared_tier_then_it_reads_back() {
        let shared = SharedTier::new(SendTier::P480);
        assert_eq!(shared.get(), SendTier::P480);
        for tier in SendTier::ALL {
            shared.set(tier);
            assert_eq!(shared.get(), tier);
        }
        shared.0.store(200, Ordering::Relaxed);
        assert_eq!(shared.get(), SendTier::P360);
    }
}

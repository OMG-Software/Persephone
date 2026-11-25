//! The pure rules of a macOS display share (ADR-0064): which display the core
//! named, how large the capture is, and which frames the worker takes. No OS
//! type, so the tests run on every OS. `macos.rs` is the only user outside
//! tests.

// Other targets build this module only for the tests.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::time::{Duration, Instant};

/// The most displays the worker reads from CoreGraphics. The core reads the
/// same number; an index (a `u8` on the wire) beyond it is `NotFound`.
pub const MAX_DISPLAYS: usize = 16;
/// The capture is asked for at most this size (ADR-0053).
pub const OUTPUT_MAX_WIDTH: u32 = 1920;
pub const OUTPUT_MAX_HEIGHT: u32 = 1080;
/// ScreenCaptureKit is asked for one frame every 1/5 s.
pub const FRAMES_PER_SECOND: i32 = 5;
/// A frame is taken at most this often. It is a little under the 200 ms the
/// stream is asked for, so a frame that arrives a few ms early still counts,
/// and a burst never gets through.
pub const MIN_FRAME_GAP: Duration = Duration::from_millis(150);

/// The display list entry (by index) that the core named, or `None` for an
/// index that the list does not have.
pub fn display_at(active: &[u32], index: u8) -> Option<u32> {
    active.get(usize::from(index)).copied()
}

/// Whether a display's size in points is the size the core saw when the user
/// chose it. A different size, or one that is not positive, is not the
/// chosen display.
pub fn size_matches(actual_width: i64, actual_height: i64, width: u32, height: u32) -> bool {
    actual_width == i64::from(width) && actual_height == i64::from(height)
}

/// The size to ask ScreenCaptureKit for: the display's size in points,
/// scaled down (never up) to fit 1920x1080 with the aspect ratio kept, with
/// each side even and at least 2. `None` for a size the capture limits
/// refuse.
pub fn output_size(width: u32, height: u32) -> Option<(u32, u32)> {
    if !super::capture_size_ok(width, height) {
        return None;
    }
    let (w, h) = (u64::from(width), u64::from(height));
    let (max_w, max_h) = (u64::from(OUTPUT_MAX_WIDTH), u64::from(OUTPUT_MAX_HEIGHT));
    let (out_w, out_h) = if w <= max_w && h <= max_h {
        (w, h)
    } else if w * max_h >= h * max_w {
        (max_w, h * max_w / w)
    } else {
        (w * max_h / h, max_h)
    };
    let even = |side: u64| (side & !1).max(2);
    Some((even(out_w) as u32, even(out_h) as u32))
}

/// Whether a delivered picture is one the worker takes: not larger than the
/// size that was asked for (plus nothing), and a size the capture limits
/// accept. The check runs before any pixel is read.
pub fn frame_fits(width: usize, height: usize) -> bool {
    let (Ok(w), Ok(h)) = (u32::try_from(width), u32::try_from(height)) else {
        return false;
    };
    super::capture_size_ok(w, h) && w <= OUTPUT_MAX_WIDTH && h <= OUTPUT_MAX_HEIGHT
}

/// Lets a frame through at most once in [`MIN_FRAME_GAP`].
#[derive(Debug, Default)]
pub struct Admit {
    last: Option<Instant>,
}

impl Admit {
    pub fn due(&mut self, now: Instant) -> bool {
        let due = self
            .last
            .is_none_or(|at| now.saturating_duration_since(at) >= MIN_FRAME_GAP);
        if due {
            self.last = Some(now);
        }
        due
    }
}

/// How a ScreenCaptureKit error code ends a capture or a start. The numbers
/// are `SCStreamError` values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cause {
    /// The user refused Screen Recording (-3801), or the system did.
    Denied,
    /// The user stopped the share from the system's own control (-3817).
    UserStopped,
    /// Anything else.
    Other,
}

const USER_DECLINED: i64 = -3801;
const USER_STOPPED: i64 = -3817;

pub fn cause_of(code: i64) -> Cause {
    match code {
        USER_DECLINED => Cause::Denied,
        USER_STOPPED => Cause::UserStopped,
        _ => Cause::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN a display list WHEN an index is looked up THEN the entry at
    /// that place is named and an index past the end names nothing.
    #[test]
    fn given_a_display_list_when_an_index_is_looked_up_then_only_a_listed_one_is_named() {
        let active = [69_733, 3];
        assert_eq!(display_at(&active, 0), Some(active[0]));
        assert_eq!(display_at(&active, 1), Some(3));
        assert_eq!(display_at(&active, 2), None);
        assert_eq!(display_at(&[], 0), None);
        assert_eq!(display_at(&active, u8::MAX), None);
    }

    /// GIVEN the size the core saw WHEN the display's size is compared THEN
    /// only the same size matches.
    #[test]
    fn given_a_chosen_size_when_compared_then_only_the_same_size_matches() {
        assert!(size_matches(1512, 982, 1512, 982));
        assert!(!size_matches(1512, 982, 1512, 983));
        assert!(!size_matches(982, 1512, 1512, 982));
        assert!(!size_matches(0, 0, 1, 1));
        assert!(!size_matches(-1512, 982, u32::MAX, 982));
    }

    /// GIVEN display sizes WHEN the output size is chosen THEN it fits
    /// 1920x1080 with the aspect ratio, is never larger than the display,
    /// and has even sides.
    #[test]
    fn given_display_sizes_when_the_output_is_chosen_then_it_fits_and_keeps_the_ratio() {
        assert_eq!(output_size(1440, 900), Some((1440, 900)));
        assert_eq!(output_size(1920, 1080), Some((1920, 1080)));
        assert_eq!(output_size(3840, 2160), Some((1920, 1080)));
        assert_eq!(output_size(2560, 1440), Some((1920, 1080)));
        assert_eq!(output_size(3440, 1440), Some((1920, 802)));
        assert_eq!(output_size(1080, 1920), Some((606, 1080)));
        assert_eq!(output_size(1281, 721), Some((1280, 720)));
        assert_eq!(output_size(2, 2), Some((2, 2)));
        for (width, height) in [(3000, 2200), (1921, 1081)] {
            let (w, h) = output_size(width, height).expect("size");
            assert!(
                w <= OUTPUT_MAX_WIDTH && h <= OUTPUT_MAX_HEIGHT,
                "{width}x{height}"
            );
            assert!(w % 2 == 0 && h % 2 == 0);
        }
    }

    /// GIVEN sizes outside the capture limits WHEN the output size is chosen
    /// THEN there is none.
    #[test]
    fn given_sizes_outside_the_limits_when_the_output_is_chosen_then_there_is_none() {
        for (width, height) in [
            (0, 900),
            (1, 1),
            (7681, 100),
            (5120, 2880),
            (7680, 4320),
            (u32::MAX, 2),
        ] {
            assert_eq!(output_size(width, height), None, "{width}x{height}");
        }
    }

    /// GIVEN delivered picture sizes WHEN checked THEN only a size within
    /// 1920x1080 and the capture limits is taken.
    #[test]
    fn given_picture_sizes_when_checked_then_only_a_size_within_the_request_fits() {
        assert!(frame_fits(1920, 1080));
        assert!(frame_fits(1080, 1080));
        assert!(frame_fits(2, 2));
        assert!(!frame_fits(1921, 1080));
        assert!(!frame_fits(1920, 1081));
        assert!(!frame_fits(1, 1));
        assert!(!frame_fits(0, 1080));
        assert!(!frame_fits(usize::MAX, 2));
    }

    /// GIVEN frames that arrive faster than 5 a second WHEN admitted THEN a
    /// frame inside the gap is dropped and the first after it passes.
    #[test]
    fn given_a_burst_of_frames_when_admitted_then_the_gap_holds() {
        let start = Instant::now();
        let mut admit = Admit::default();
        assert!(admit.due(start));
        assert!(!admit.due(start));
        assert!(!admit.due(start + Duration::from_millis(149)));
        assert!(admit.due(start + Duration::from_millis(150)));
        assert!(!admit.due(start + Duration::from_millis(200)));
        // A clock that steps back never admits and never panics.
        assert!(!admit.due(start));
    }

    /// GIVEN ScreenCaptureKit error codes WHEN classified THEN a declined
    /// permission and a stop by the user are told apart from the rest.
    #[test]
    fn given_error_codes_when_classified_then_the_cause_is_named() {
        assert_eq!(cause_of(-3801), Cause::Denied);
        assert_eq!(cause_of(-3817), Cause::UserStopped);
        assert_eq!(cause_of(-3802), Cause::Other);
        assert_eq!(cause_of(0), Cause::Other);
    }
}

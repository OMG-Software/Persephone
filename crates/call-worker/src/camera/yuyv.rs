//! The Linux camera format policy and the YUYV conversion (ADR-0049). Both
//! Linux camera backends (the PipeWire portal and V4L2) use it. It has no
//! I/O and no Linux types, so it is unit-tested on every OS.
//!
//! Only NV12, I420 and YUYV are accepted, at most 1280x720 (or 720x1280 for a
//! tall camera). The worker never decodes MJPEG. YUYV is converted to I420
//! here, with every size checked before a buffer is made.

use super::{CapturedFrame, Format, capture_size_ok};
use crate::video::{CaptureMode, pick_capture_mode};

/// The longest and the shortest side the Linux backends take.
const MAX_LONG_SIDE: u32 = 1280;
const MAX_SHORT_SIDE: u32 = 720;

/// A pixel format the Linux backends take, best first. NV12 and I420 need no
/// conversion.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PixelFormat {
    Nv12,
    I420,
    /// Packed `Y0 U Y1 V` (V4L2 `YUYV`, SPA `YUY2`).
    Yuyv,
}

/// Whether a camera may deliver this picture size: inside the capture limits
/// and at most 1280x720 (720x1280 when tall).
pub fn size_allowed(width: u32, height: u32) -> bool {
    capture_size_ok(width, height)
        && width.max(height) <= MAX_LONG_SIDE
        && width.min(height) <= MAX_SHORT_SIDE
}

/// One format a camera offers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Offer {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    /// Frames per second, rounded down; 0 when unknown.
    pub fps: u32,
}

/// The index of the best allowed offer, or `None` when no offer is allowed
/// (for example a camera that gives only MJPEG, which is not an `Offer`, or
/// only pictures above 1280x720). Size and speed follow
/// [`pick_capture_mode`]; on a tie the format that needs no conversion wins.
/// YUYV needs even widths, so an odd-width YUYV offer is skipped.
pub fn choose(offers: &[Offer]) -> Option<usize> {
    let mut allowed: Vec<(usize, &Offer)> = offers
        .iter()
        .enumerate()
        .filter(|(_, offer)| {
            size_allowed(offer.width, offer.height)
                && (offer.format != PixelFormat::Yuyv || offer.width.is_multiple_of(2))
        })
        .collect();
    // Stable sort: equal formats keep the camera's order.
    allowed.sort_by_key(|(_, offer)| offer.format);
    let modes: Vec<CaptureMode> = allowed
        .iter()
        .map(|(_, offer)| CaptureMode {
            width: offer.width,
            height: offer.height,
            fps: offer.fps,
        })
        .collect();
    pick_capture_mode(&modes).map(|best| allowed[best].0)
}

/// Hands one picture to `on_frame` as NV12 or I420. `stride` is the bytes per
/// row of the Y plane (of the whole picture for YUYV). YUYV is converted into
/// `converted`, which keeps its memory between calls. A picture that is too
/// short or has a bad size or stride is skipped. Returns whether a frame was
/// delivered.
pub fn deliver_frame<F: FnMut(CapturedFrame<'_>)>(
    format: PixelFormat,
    width: u32,
    height: u32,
    stride: u32,
    bytes: &[u8],
    converted: &mut Vec<u8>,
    on_frame: &mut F,
) -> bool {
    let frame = match format {
        PixelFormat::Nv12 => CapturedFrame::checked(Format::Nv12, width, height, stride, bytes),
        PixelFormat::I420 => CapturedFrame::checked(Format::I420, width, height, stride, bytes),
        PixelFormat::Yuyv => {
            if !yuyv_to_i420(bytes, width, height, stride as usize, converted) {
                return false;
            }
            CapturedFrame::checked(Format::I420, width, height, width, converted)
        }
    };
    match frame {
        Some(frame) => {
            on_frame(frame);
            true
        }
        None => false,
    }
}

/// The size of an I420 picture of `width` x `height` (Y stride = width):
/// `(y_len, chroma_len, total)`. `None` for a size outside the capture limits.
pub fn i420_sizes(width: u32, height: u32) -> Option<(usize, usize, usize)> {
    if !capture_size_ok(width, height) {
        return None;
    }
    let (w, h) = (width as usize, height as usize);
    let y = w.checked_mul(h)?;
    let chroma = w.div_ceil(2).checked_mul(h.div_ceil(2))?;
    let total = y.checked_add(chroma.checked_mul(2)?)?;
    Some((y, chroma, total))
}

/// Converts one YUYV picture to I420 with a Y stride equal to `width`.
/// `stride` is the source bytes per row. The result replaces `out`.
/// Returns `false` and leaves `out` empty for an odd width, a size outside the
/// limits, a stride shorter than a row, or a source shorter than the picture.
/// Vertical chroma is the mean of the two rows of each pair.
pub fn yuyv_to_i420(src: &[u8], width: u32, height: u32, stride: usize, out: &mut Vec<u8>) -> bool {
    out.clear();
    if !width.is_multiple_of(2) {
        return false;
    }
    let Some((y_len, chroma_len, total)) = i420_sizes(width, height) else {
        return false;
    };
    let (w, h) = (width as usize, height as usize);
    let row_bytes = w * 2;
    let Some(needed) = (h - 1)
        .checked_mul(stride)
        .and_then(|rows| rows.checked_add(row_bytes))
    else {
        return false;
    };
    if stride < row_bytes || src.len() < needed {
        return false;
    }
    out.resize(total, 0);
    let (y_plane, chroma) = out.split_at_mut(y_len);
    let (u_plane, v_plane) = chroma.split_at_mut(chroma_len);
    let cw = w / 2;
    for row in 0..h {
        let line = &src[row * stride..row * stride + row_bytes];
        for x in 0..cw {
            y_plane[row * w + 2 * x] = line[4 * x];
            y_plane[row * w + 2 * x + 1] = line[4 * x + 2];
        }
        if row.is_multiple_of(2) {
            // The partner row is the next one, or this row again for an odd
            // height.
            let next = if row + 1 < h {
                &src[(row + 1) * stride..(row + 1) * stride + row_bytes]
            } else {
                line
            };
            let chroma_row = (row / 2) * cw;
            for x in 0..cw {
                let mean = |at: usize| {
                    (u16::from(line[4 * x + at]) + u16::from(next[4 * x + at])).div_ceil(2) as u8
                };
                u_plane[chroma_row + x] = mean(1);
                v_plane[chroma_row + x] = mean(3);
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(format: PixelFormat, width: u32, height: u32, fps: u32) -> Offer {
        Offer {
            format,
            width,
            height,
            fps,
        }
    }

    /// GIVEN picture sizes WHEN checked THEN only sizes up to 1280x720 pass,
    /// in either orientation.
    #[test]
    fn given_sizes_then_only_up_to_1280_by_720_pass() {
        for (w, h) in [(640, 360), (1280, 720), (720, 1280), (320, 240), (2, 2)] {
            assert!(size_allowed(w, h), "{w}x{h}");
        }
        for (w, h) in [
            (1920, 1080),
            (1281, 720),
            (1280, 721),
            (0, 0),
            (1, 1),
            (4096, 4096),
        ] {
            assert!(!size_allowed(w, h), "{w}x{h}");
        }
    }

    /// GIVEN offers of several sizes WHEN one is chosen THEN a picture above
    /// 1280x720 is never chosen and the smallest big-enough one wins.
    #[test]
    fn given_offers_then_the_choice_stays_inside_the_limit() {
        let offers = [
            offer(PixelFormat::Yuyv, 1920, 1080, 30),
            offer(PixelFormat::Yuyv, 1280, 720, 10),
            offer(PixelFormat::Yuyv, 640, 360, 30),
        ];
        assert_eq!(choose(&offers), Some(2));
        assert_eq!(choose(&offers[..1]), None);
        assert_eq!(choose(&[]), None);
    }

    /// GIVEN equal offers in two formats WHEN one is chosen THEN the format
    /// that needs no conversion wins.
    #[test]
    fn given_equal_offers_then_nv12_wins_over_yuyv() {
        let offers = [
            offer(PixelFormat::Yuyv, 640, 360, 30),
            offer(PixelFormat::I420, 640, 360, 30),
            offer(PixelFormat::Nv12, 640, 360, 30),
        ];
        assert_eq!(choose(&offers), Some(2));
        assert_eq!(choose(&offers[..2]), Some(1));
    }

    /// GIVEN a YUYV offer with an odd width WHEN chosen THEN it is skipped.
    #[test]
    fn given_odd_width_yuyv_then_it_is_skipped() {
        assert_eq!(choose(&[offer(PixelFormat::Yuyv, 641, 360, 30)]), None);
        assert_eq!(choose(&[offer(PixelFormat::Nv12, 641, 360, 30)]), Some(0));
    }

    /// GIVEN a 4x2 YUYV picture WHEN converted THEN the planes hold the luma
    /// and the mean chroma of the two rows.
    #[test]
    fn given_yuyv_picture_when_converted_then_planes_are_right() {
        // Row 0: (Y0 U Y1 V) x2; row 1 has different chroma.
        let src = [
            10, 100, 11, 200, 12, 110, 13, 210, // row 0
            20, 102, 21, 202, 22, 112, 23, 212, // row 1
        ];
        let mut out = Vec::new();
        assert!(yuyv_to_i420(&src, 4, 2, 8, &mut out));
        assert_eq!(out.len(), 8 + 2 + 2);
        assert_eq!(&out[..8], &[10, 11, 12, 13, 20, 21, 22, 23]);
        assert_eq!(&out[8..10], &[101, 111]);
        assert_eq!(&out[10..12], &[201, 211]);
    }

    /// GIVEN a source with row padding and an odd height WHEN converted THEN
    /// the padding is skipped and the last chroma row uses its own row.
    #[test]
    fn given_padded_odd_height_when_converted_then_it_is_read_by_stride() {
        let stride = 6;
        let mut src = vec![0xEE_u8; stride * 3];
        for row in 0..3 {
            src[row * stride..row * stride + 4].copy_from_slice(&[row as u8, 50, row as u8, 60]);
        }
        let mut out = Vec::new();
        assert!(yuyv_to_i420(&src, 2, 3, stride, &mut out));
        // Y: 2x3; U and V: 1x2 each.
        assert_eq!(out, [0, 0, 1, 1, 2, 2, 50, 50, 60, 60]);
    }

    /// GIVEN bad input WHEN converted THEN it is refused and `out` is empty.
    #[test]
    fn given_bad_input_when_converted_then_it_is_refused() {
        let src = vec![0_u8; 4 * 2 * 2];
        for (w, h, stride, len) in [
            (3, 2, 8, src.len()),     // odd width
            (4, 2, 7, src.len()),     // stride shorter than a row
            (4, 2, 8, src.len() - 1), // short source
            (0, 2, 8, src.len()),     // empty
            (4, 0, 8, src.len()),
            (9000, 9000, 18000, src.len()), // beyond the limits
        ] {
            let mut out = vec![1_u8; 3];
            assert!(
                !yuyv_to_i420(&src[..len], w, h, stride, &mut out),
                "{w}x{h}"
            );
            assert!(out.is_empty());
        }
        let mut out = Vec::new();
        assert!(!yuyv_to_i420(&src, 4, 2, usize::MAX, &mut out));
    }

    /// GIVEN pictures in each format WHEN delivered THEN a whole picture
    /// reaches the callback in NV12 or I420, and a short or bad one does not.
    #[test]
    fn given_pictures_when_delivered_then_only_whole_ones_reach_the_callback() {
        let mut converted = Vec::new();
        let mut seen = Vec::new();
        let mut on_frame = |frame: CapturedFrame<'_>| seen.push((frame.format, frame.data.len()));
        let nv12 = vec![0_u8; 4 * 2 * 3 / 2];
        assert!(deliver_frame(
            PixelFormat::Nv12,
            4,
            2,
            4,
            &nv12,
            &mut converted,
            &mut on_frame
        ));
        assert!(deliver_frame(
            PixelFormat::I420,
            4,
            2,
            4,
            &nv12,
            &mut converted,
            &mut on_frame
        ));
        let yuyv = vec![0_u8; 4 * 2 * 2];
        assert!(deliver_frame(
            PixelFormat::Yuyv,
            4,
            2,
            8,
            &yuyv,
            &mut converted,
            &mut on_frame
        ));
        // Short Nv12, short YUYV, and a stride shorter than the width.
        assert!(!deliver_frame(
            PixelFormat::Nv12,
            4,
            2,
            4,
            &nv12[..11],
            &mut converted,
            &mut on_frame
        ));
        assert!(!deliver_frame(
            PixelFormat::Yuyv,
            4,
            2,
            8,
            &yuyv[..15],
            &mut converted,
            &mut on_frame
        ));
        assert!(!deliver_frame(
            PixelFormat::Nv12,
            4,
            2,
            3,
            &nv12,
            &mut converted,
            &mut on_frame
        ));
        assert_eq!(
            seen,
            [(Format::Nv12, 12), (Format::I420, 12), (Format::I420, 12)]
        );
    }

    /// GIVEN sizes WHEN the I420 length is asked THEN odd sizes round up.
    #[test]
    fn given_sizes_when_i420_length_asked_then_chroma_rounds_up() {
        assert_eq!(i420_sizes(4, 2), Some((8, 2, 12)));
        assert_eq!(i420_sizes(5, 3), Some((15, 6, 27)));
        assert_eq!(i420_sizes(1, 1), None);
    }
}

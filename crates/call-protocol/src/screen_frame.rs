//! The frame pipe from the Windows capture helper to the call worker
//! (ADR-0067). One pipe carries items of one share, in order. Each item is a
//! media-protocol `Control` frame that holds a short message, and a frame
//! item is followed by one `RgbaPixels` frame with the picture. The pixel
//! frame holds BGRA bytes in this protocol, not RGBA: the message names the
//! format. Both frame headers go through `FrameHeader::decode`.
//!
//! The reader checks every field of the message and the pixel header before
//! it allocates the body: the share number, the format, the size (the same
//! limits as the capture, [`screen_capture_size_ok`]), the stride and the
//! body length. A picture has no padding: its stride is `width * 4` and its
//! body is `height * stride` bytes, at most [`MAX_FRAME_BYTES`]. The helper
//! copies each picture out of its GPU texture row by row, so the pipe never
//! carries the texture's row padding.
//!
//! Neither side trusts the other. The helper ends the pipe with an end item
//! that names why (a closed set), then closes it; a pipe that closes without
//! an end item is a helper that failed.

use std::io::{self, Read, Write};

use persephone_media_protocol::{FrameHeader, FrameKind, write_frame};

use crate::{MAX_CAPTURE_PIXELS, screen_capture_size_ok};

/// The version byte of an item's message.
pub const SCREEN_FRAME_VERSION: u8 = 1;
/// The most bytes in one picture: 3840x2160 pixels, four bytes each.
pub const MAX_FRAME_BYTES: usize = MAX_CAPTURE_PIXELS as usize * 4;

const TAG_FRAME: u8 = 1;
const TAG_END: u8 = 2;
/// The media-protocol header: kind, body length, width, height.
const HEADER_LEN: usize = 17;
/// Version, tag, share, format, width, height, stride and length.
const FRAME_MESSAGE_LEN: usize = 1 + 1 + 4 + 1 + 4 + 4 + 4 + 4;
/// Version, tag, share and reason.
const END_MESSAGE_LEN: usize = 1 + 1 + 4 + 1;
const FORMAT_BGRA: u8 = 1;

/// Why the helper ended the pipe. A closed set with no text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ScreenEnd {
    /// The core asked for the end, or the worker closed the pipe.
    Stopped = 0,
    /// The chosen monitor is gone, moved or has another size.
    NotFound = 1,
    /// The capture refused or failed.
    Unavailable = 2,
}

impl ScreenEnd {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Stopped),
            1 => Some(Self::NotFound),
            2 => Some(Self::Unavailable),
            _ => None,
        }
    }
}

/// One picture: BGRA bytes, `width * 4` bytes per row, `height` rows.
#[derive(Debug, Eq, PartialEq)]
pub struct ScreenFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl ScreenFrame {
    /// Bytes per row. A picture on this pipe has no padding.
    pub fn stride(&self) -> u32 {
        self.width * 4
    }
}

/// What one item of the pipe says.
#[derive(Debug, Eq, PartialEq)]
pub enum ScreenItem {
    Frame(ScreenFrame),
    End(ScreenEnd),
}

/// Why an item was refused or could not be read. Closed; no text.
#[derive(Debug, Eq, PartialEq)]
pub enum ScreenFrameError {
    /// The pipe ended before an item began, or inside one.
    Closed,
    /// Any other read error.
    Io(io::ErrorKind),
    /// A header that `FrameHeader::decode` refuses, a kind or a length that
    /// does not fit its place.
    BadHeader,
    UnknownVersion,
    UnknownTag,
    /// The item belongs to another share.
    WrongShare,
    UnknownFormat,
    /// A reason that is not in the closed set.
    UnknownReason,
    /// A size over the capture limits or under 2x2.
    BadSize,
    /// A stride other than `width * 4`.
    BadStride,
    /// A body length other than `height * stride`, or one that the pixel
    /// header does not repeat.
    LengthMismatch,
    /// The body could not be allocated.
    AllocationFailure,
}

fn read_error(error: &io::Error) -> ScreenFrameError {
    match error.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe => ScreenFrameError::Closed,
        kind => ScreenFrameError::Io(kind),
    }
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Reads the next item of share `share`. Nothing is allocated for a body
/// before every field that names its size has passed.
pub fn read_item<R: Read>(reader: &mut R, share: u32) -> Result<ScreenItem, ScreenFrameError> {
    let mut wire = [0_u8; HEADER_LEN];
    reader.read_exact(&mut wire).map_err(|e| read_error(&e))?;
    let header = FrameHeader::decode(wire).map_err(|_| ScreenFrameError::BadHeader)?;
    if header.kind != FrameKind::Control {
        return Err(ScreenFrameError::BadHeader);
    }
    // The only two lengths a message has; a message of any other length is
    // refused before a byte of it is read.
    let length = usize::try_from(header.length).map_err(|_| ScreenFrameError::BadHeader)?;
    if length != FRAME_MESSAGE_LEN && length != END_MESSAGE_LEN {
        return Err(ScreenFrameError::BadHeader);
    }
    let mut message = [0_u8; FRAME_MESSAGE_LEN];
    reader
        .read_exact(&mut message[..length])
        .map_err(|e| read_error(&e))?;
    if message[0] != SCREEN_FRAME_VERSION {
        return Err(ScreenFrameError::UnknownVersion);
    }
    match (message[1], length) {
        (TAG_END, END_MESSAGE_LEN) => {
            if be32(&message, 2) != share {
                return Err(ScreenFrameError::WrongShare);
            }
            ScreenEnd::from_byte(message[6])
                .map(ScreenItem::End)
                .ok_or(ScreenFrameError::UnknownReason)
        }
        (TAG_FRAME, FRAME_MESSAGE_LEN) => read_picture(reader, share, &message),
        (TAG_END | TAG_FRAME, _) => Err(ScreenFrameError::BadHeader),
        _ => Err(ScreenFrameError::UnknownTag),
    }
}

fn read_picture<R: Read>(
    reader: &mut R,
    share: u32,
    message: &[u8; FRAME_MESSAGE_LEN],
) -> Result<ScreenItem, ScreenFrameError> {
    if be32(message, 2) != share {
        return Err(ScreenFrameError::WrongShare);
    }
    if message[6] != FORMAT_BGRA {
        return Err(ScreenFrameError::UnknownFormat);
    }
    let (width, height) = (be32(message, 7), be32(message, 11));
    let (stride, length) = (be32(message, 15), be32(message, 19));
    if !screen_capture_size_ok(width, height) {
        return Err(ScreenFrameError::BadSize);
    }
    if u64::from(stride) != u64::from(width) * 4 {
        return Err(ScreenFrameError::BadStride);
    }
    let expected = u64::from(stride) * u64::from(height);
    if u64::from(length) != expected || expected > MAX_FRAME_BYTES as u64 {
        return Err(ScreenFrameError::LengthMismatch);
    }
    let mut wire = [0_u8; HEADER_LEN];
    reader.read_exact(&mut wire).map_err(|e| read_error(&e))?;
    let pixels = FrameHeader::decode(wire).map_err(|_| ScreenFrameError::BadHeader)?;
    if pixels.kind != FrameKind::RgbaPixels {
        return Err(ScreenFrameError::BadHeader);
    }
    if (pixels.width, pixels.height) != (width, height) {
        return Err(ScreenFrameError::BadSize);
    }
    if pixels.length != expected {
        return Err(ScreenFrameError::LengthMismatch);
    }
    let length = length as usize;
    let mut data = Vec::new();
    data.try_reserve_exact(length)
        .map_err(|_| ScreenFrameError::AllocationFailure)?;
    data.resize(length, 0);
    reader.read_exact(&mut data).map_err(|e| read_error(&e))?;
    Ok(ScreenItem::Frame(ScreenFrame {
        width,
        height,
        data,
    }))
}

fn message_header(length: usize) -> FrameHeader {
    FrameHeader {
        kind: FrameKind::Control,
        length: length as u64,
        width: 0,
        height: 0,
    }
}

/// Writes one picture of share `share`: `width * 4` bytes per row. The
/// picture must pass the checks that [`read_item`] makes; nothing is written
/// for one that does not.
pub fn write_picture<W: Write>(
    writer: &mut W,
    share: u32,
    width: u32,
    height: u32,
    data: &[u8],
) -> io::Result<()> {
    let invalid = |text: &str| io::Error::new(io::ErrorKind::InvalidInput, text.to_owned());
    if !screen_capture_size_ok(width, height) {
        return Err(invalid("picture size is outside the capture limits"));
    }
    let stride = width * 4;
    if data.len() as u64 != u64::from(stride) * u64::from(height) {
        return Err(invalid("picture body is not height * stride bytes"));
    }
    let length = data.len() as u32;
    let mut message = Vec::with_capacity(FRAME_MESSAGE_LEN);
    message.push(SCREEN_FRAME_VERSION);
    message.push(TAG_FRAME);
    message.extend_from_slice(&share.to_be_bytes());
    message.push(FORMAT_BGRA);
    for value in [width, height, stride, length] {
        message.extend_from_slice(&value.to_be_bytes());
    }
    write_frame(writer, message_header(message.len()), &message)?;
    let header = FrameHeader {
        kind: FrameKind::RgbaPixels,
        length: u64::from(length),
        width,
        height,
    };
    write_frame(writer, header, data)
}

/// Writes the end item of share `share`.
pub fn write_end<W: Write>(writer: &mut W, share: u32, reason: ScreenEnd) -> io::Result<()> {
    let mut message = Vec::with_capacity(END_MESSAGE_LEN);
    message.push(SCREEN_FRAME_VERSION);
    message.push(TAG_END);
    message.extend_from_slice(&share.to_be_bytes());
    message.push(reason as u8);
    write_frame(writer, message_header(message.len()), &message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(share: u32, width: u32, height: u32) -> Vec<u8> {
        let mut wire = Vec::new();
        let body = vec![7_u8; width as usize * height as usize * 4];
        write_picture(&mut wire, share, width, height, &body).expect("a picture");
        wire
    }

    /// The wire of a picture with one field of its message changed by
    /// `change`, and the length fields of both headers kept as they are.
    fn altered(change: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        let mut wire = picture(5, 4, 2);
        change(&mut wire);
        wire
    }

    /// Offsets in a picture's wire: the message follows the first header.
    const MESSAGE: usize = HEADER_LEN;

    fn read(wire: &[u8], share: u32) -> Result<ScreenItem, ScreenFrameError> {
        read_item(&mut &wire[..], share)
    }

    /// GIVEN a picture and an end item written WHEN they are read THEN both
    /// come back as written, in order.
    #[test]
    fn given_written_items_when_read_then_they_come_back_in_order() {
        let mut wire = picture(5, 4, 2);
        write_end(&mut wire, 5, ScreenEnd::NotFound).expect("an end");
        let mut input = &wire[..];
        let item = read_item(&mut input, 5).expect("a picture");
        let ScreenItem::Frame(frame) = item else {
            panic!("a picture expected");
        };
        assert_eq!((frame.width, frame.height, frame.stride()), (4, 2, 16));
        assert_eq!(frame.data, vec![7; 32]);
        assert_eq!(
            read_item(&mut input, 5),
            Ok(ScreenItem::End(ScreenEnd::NotFound))
        );
        assert_eq!(read_item(&mut input, 5), Err(ScreenFrameError::Closed));
    }

    /// GIVEN a picture of another share WHEN it is read THEN it is refused,
    /// and so is an end item of another share.
    #[test]
    fn given_another_share_when_read_then_it_is_refused() {
        assert_eq!(
            read(&picture(5, 4, 2), 6),
            Err(ScreenFrameError::WrongShare)
        );
        let mut end = Vec::new();
        write_end(&mut end, 5, ScreenEnd::Stopped).expect("an end");
        assert_eq!(read(&end, 6), Err(ScreenFrameError::WrongShare));
    }

    /// GIVEN messages with a size over the limits, under 2x2 or with a side
    /// of zero WHEN read THEN each is refused before the pixel header is
    /// read: the input holds no pixel frame, and the error is a size error,
    /// not a closed pipe.
    #[test]
    fn given_a_bad_size_when_read_then_it_is_refused_before_the_body() {
        for (width, height) in [
            (3841_u32, 2161_u32),
            (7681, 2),
            (2, 7681),
            (4000, 4000),
            (0, 4),
            (1, 1),
            (u32::MAX, u32::MAX),
        ] {
            let wire = altered(|wire| {
                wire.truncate(MESSAGE + FRAME_MESSAGE_LEN);
                wire[MESSAGE + 7..MESSAGE + 11].copy_from_slice(&width.to_be_bytes());
                wire[MESSAGE + 11..MESSAGE + 15].copy_from_slice(&height.to_be_bytes());
            });
            assert_eq!(
                read(&wire, 5),
                Err(ScreenFrameError::BadSize),
                "{width}x{height}"
            );
        }
    }

    /// GIVEN a wrong stride, a body length that is not height * stride, an
    /// unknown format, version, tag or reason WHEN read THEN each is refused
    /// with its own error, before any body is read.
    #[test]
    fn given_bad_message_fields_when_read_then_each_is_refused() {
        let set = |at: usize, bytes: &[u8]| {
            altered(|wire| {
                wire.truncate(MESSAGE + FRAME_MESSAGE_LEN);
                wire[MESSAGE + at..MESSAGE + at + bytes.len()].copy_from_slice(bytes);
            })
        };
        // Stride 4x2 picture: 16. A padded stride is not accepted.
        assert_eq!(
            read(&set(15, &20_u32.to_be_bytes()), 5),
            Err(ScreenFrameError::BadStride)
        );
        assert_eq!(
            read(&set(15, &12_u32.to_be_bytes()), 5),
            Err(ScreenFrameError::BadStride)
        );
        assert_eq!(
            read(&set(19, &31_u32.to_be_bytes()), 5),
            Err(ScreenFrameError::LengthMismatch)
        );
        assert_eq!(
            read(&set(19, &u32::MAX.to_be_bytes()), 5),
            Err(ScreenFrameError::LengthMismatch)
        );
        assert_eq!(read(&set(6, &[2]), 5), Err(ScreenFrameError::UnknownFormat));
        assert_eq!(read(&set(6, &[0]), 5), Err(ScreenFrameError::UnknownFormat));
        assert_eq!(
            read(&set(0, &[2]), 5),
            Err(ScreenFrameError::UnknownVersion)
        );
        assert_eq!(read(&set(1, &[3]), 5), Err(ScreenFrameError::UnknownTag));
        // A frame tag with the end message's length is a bad header.
        let mut end = Vec::new();
        write_end(&mut end, 5, ScreenEnd::Stopped).expect("an end");
        end[HEADER_LEN + 1] = TAG_FRAME;
        assert_eq!(read(&end, 5), Err(ScreenFrameError::BadHeader));
        end[HEADER_LEN + 1] = TAG_END;
        end[HEADER_LEN + 6] = 3;
        assert_eq!(read(&end, 5), Err(ScreenFrameError::UnknownReason));
    }

    /// GIVEN a first header of the wrong kind, an empty or over-long
    /// message, or garbage WHEN read THEN it is refused after the header
    /// alone: no message byte is needed.
    #[test]
    fn given_a_bad_first_header_when_read_then_it_is_refused() {
        let header = |kind: u8, length: u64| {
            let mut wire = vec![kind];
            wire.extend_from_slice(&length.to_be_bytes());
            wire.extend_from_slice(&[0; 8]);
            wire
        };
        // The pixel kind first, an encoded image, an unknown kind.
        for kind in [3_u8, 2, 9] {
            assert_eq!(
                read(&header(kind, 23), 5),
                Err(ScreenFrameError::BadHeader),
                "kind {kind}"
            );
        }
        // A control frame of a length that no message has, or of none.
        for length in [0_u64, 1, 22, 24, 128 * 1024, u64::MAX] {
            assert_eq!(
                read(&header(1, length), 5),
                Err(ScreenFrameError::BadHeader),
                "length {length}"
            );
        }
    }

    /// GIVEN a pixel header that disagrees with the message (other size,
    /// other length, the control kind, the encoded kind) WHEN read THEN it
    /// is refused before the body is allocated: the body is not even in the
    /// input.
    #[test]
    fn given_a_pixel_header_that_disagrees_when_read_then_it_is_refused() {
        let at = MESSAGE + FRAME_MESSAGE_LEN;
        let cut = |change: &dyn Fn(&mut Vec<u8>)| {
            let mut wire = picture(5, 4, 2);
            wire.truncate(at + HEADER_LEN);
            change(&mut wire);
            wire
        };
        // 8x1 is also 32 bytes, so media-protocol takes the header; the
        // message says 4x2, so the reader refuses it.
        let wide = cut(&|w| {
            w[at + 9..at + 13].copy_from_slice(&8_u32.to_be_bytes());
            w[at + 13..at + 17].copy_from_slice(&1_u32.to_be_bytes());
        });
        assert_eq!(read(&wide, 5), Err(ScreenFrameError::BadSize));
        // One side changed alone: media-protocol refuses the length.
        assert_eq!(
            read(
                &cut(&|w| w[at + 9..at + 13].copy_from_slice(&5_u32.to_be_bytes())),
                5
            ),
            Err(ScreenFrameError::BadHeader)
        );
        // media-protocol itself refuses a length other than w * h * 4.
        assert_eq!(
            read(
                &cut(&|w| w[at + 1..at + 9].copy_from_slice(&31_u64.to_be_bytes())),
                5
            ),
            Err(ScreenFrameError::BadHeader)
        );
        assert_eq!(
            read(&cut(&|w| w[at] = 1), 5),
            Err(ScreenFrameError::BadHeader)
        );
        assert_eq!(
            read(&cut(&|w| w[at] = 2), 5),
            Err(ScreenFrameError::BadHeader)
        );
    }

    /// GIVEN a cut input WHEN read THEN the pipe is closed, in the first
    /// header, the message, the pixel header and the body.
    #[test]
    fn given_a_cut_input_when_read_then_the_pipe_is_closed() {
        let wire = picture(5, 4, 2);
        for cut in [
            0,
            5,
            MESSAGE + 3,
            MESSAGE + FRAME_MESSAGE_LEN + 4,
            wire.len() - 1,
        ] {
            assert_eq!(
                read(&wire[..cut], 5),
                Err(ScreenFrameError::Closed),
                "cut at {cut}"
            );
        }
    }

    /// GIVEN a picture the reader would refuse WHEN it is written THEN the
    /// writer refuses it and writes nothing.
    #[test]
    fn given_a_bad_picture_when_written_then_nothing_is_written() {
        let mut wire = Vec::new();
        assert!(write_picture(&mut wire, 1, 3841, 2161, &[]).is_err());
        assert!(write_picture(&mut wire, 1, 1, 1, &[0; 4]).is_err());
        assert!(write_picture(&mut wire, 1, 4, 2, &[0; 31]).is_err());
        assert!(write_picture(&mut wire, 1, 4, 2, &[0; 33]).is_err());
        assert!(wire.is_empty());
    }

    /// GIVEN the largest picture WHEN written and read THEN it passes: the
    /// 3840x2160 limit and the 7680 side limit are the capture's.
    #[test]
    fn given_the_largest_pictures_when_round_tripped_then_they_pass() {
        for (width, height) in [(3840_u32, 2160_u32), (7680, 2), (2, 7680)] {
            let wire = picture(9, width, height);
            let Ok(ScreenItem::Frame(frame)) = read(&wire, 9) else {
                panic!("{width}x{height}");
            };
            assert_eq!(frame.data.len(), width as usize * height as usize * 4);
        }
        assert_eq!(MAX_FRAME_BYTES, 3840 * 2160 * 4);
    }
}

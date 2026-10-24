//! Metadata removal for PNG and JPEG (ADR-0009).
//!
//! The strip runs in place: kept segments and chunks are moved towards the
//! start of the input buffer, and the buffer is then truncated. The write
//! position never passes the read position, so the output is never larger than
//! the input and no second copy of the file is allocated. The synthesized EXIF
//! segment is written into the space of the EXIF segment it replaces, which is
//! at least as large (see [`crate::exif::MINIMAL_APP1_LEN`]).
//!
//! Every length is checked against the buffer before use. Unknown or
//! malformed structure rejects. Bytes after the end marker (JPEG EOI or PNG
//! IEND), such as an appended preview image or video, are dropped.

use crate::{Status, decode, exif};

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
/// PNG limits chunk lengths to 2^31 - 1.
const PNG_MAX_CHUNK_LEN: u32 = 0x7fff_ffff;
const PNG_KEPT_CHUNKS: [&[u8; 4]; 5] = [b"IHDR", b"PLTE", b"tRNS", b"IDAT", b"IEND"];

const SOI: u8 = 0xd8;
const EOI: u8 = 0xd9;
const SOS: u8 = 0xda;
const DQT: u8 = 0xdb;
const DHT: u8 = 0xc4;
const DRI: u8 = 0xdd;
const APP1: u8 = 0xe1;
const APP14: u8 = 0xee;
const COM: u8 = 0xfe;

/// Strips metadata from `image` in place. Before the strip, the original is
/// decoded and fingerprinted; after it, the stripped copy is decoded and must
/// give the same fingerprint. Only one decoded image is alive at a time.
///
/// Guest memory: the 176 MiB fixed guest memory holds the input buffer (at
/// most 20 MiB, reused for the output) plus one decode. That is the same peak
/// as a plain decode, which also holds its input while decoding; no second
/// encoded copy and no second 64 MiB pixel buffer are ever allocated.
pub fn strip_in_place(image: &mut Vec<u8>) -> Result<(), Status> {
    if image.len() > crate::MAX_INPUT_BYTES {
        return Err(Status::InputTooLarge);
    }
    let original = fingerprint(image)?;
    if image.starts_with(PNG_SIGNATURE) {
        strip_png(image)?;
    } else if image.starts_with(&[0xff, SOI, 0xff]) {
        strip_jpeg(image)?;
    } else {
        return Err(Status::UnsupportedFormat);
    }
    check_same(&original, image)
}

/// Accepts `stripped` only if it decodes to the original's fingerprint.
fn check_same(original: &Fingerprint, stripped: &[u8]) -> Result<(), Status> {
    match fingerprint(stripped) {
        Ok(fingerprint) if fingerprint == *original => Ok(()),
        _ => Err(Status::StripMismatch),
    }
}

/// Returns a stripped copy of `input`; see [`strip_in_place`].
pub fn strip(input: &[u8]) -> Result<Vec<u8>, Status> {
    if input.len() > crate::MAX_INPUT_BYTES {
        return Err(Status::InputTooLarge);
    }
    let mut image = input.to_vec();
    strip_in_place(&mut image)?;
    Ok(image)
}

#[derive(Debug, PartialEq, Eq)]
struct Fingerprint {
    width: u32,
    height: u32,
    orientation: u8,
    hash: u64,
}

fn fingerprint(input: &[u8]) -> Result<Fingerprint, Status> {
    let image = decode(input)?;
    Ok(Fingerprint {
        width: image.width,
        height: image.height,
        orientation: image.orientation,
        hash: pixel_hash(&image.pixels),
    })
}

/// A non-cryptographic 64-bit hash over the RGBA output, eight bytes per
/// step. Each step is a bijection of the state for a fixed word, so outputs
/// that differ in a single word always hash differently. It detects
/// accidental change; it is not a defence against a chosen collision.
fn pixel_hash(pixels: &[u8]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = 0xcbf2_9ce4_8422_2325 ^ pixels.len() as u64;
    let (words, rest) = pixels.as_chunks::<8>();
    for word in words {
        hash = (hash ^ u64::from_le_bytes(*word)).wrapping_mul(PRIME);
        hash ^= hash >> 32;
    }
    for &byte in rest {
        hash = (hash ^ u64::from(byte)).wrapping_mul(PRIME);
    }
    hash
}

// --- PNG ----------------------------------------------------------------------

fn strip_png(image: &mut Vec<u8>) -> Result<(), Status> {
    let mut read = PNG_SIGNATURE.len();
    let mut write = read;
    loop {
        let header = image.get(read..read + 8).ok_or(Status::Malformed)?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let kind: [u8; 4] = [header[4], header[5], header[6], header[7]];
        if length > PNG_MAX_CHUNK_LEN || !kind.iter().all(u8::is_ascii_alphabetic) {
            return Err(Status::Malformed);
        }
        // Length, type, data and CRC.
        let end = (length as usize)
            .checked_add(12)
            .and_then(|size| read.checked_add(size))
            .filter(|&end| end <= image.len())
            .ok_or(Status::Malformed)?;
        let first = read == PNG_SIGNATURE.len();
        if first != (&kind == b"IHDR") {
            return Err(Status::Malformed);
        }
        if PNG_KEPT_CHUNKS.contains(&&kind) {
            image.copy_within(read..end, write);
            write += end - read;
        } else if kind[0].is_ascii_uppercase() {
            // A critical chunk this strip does not know.
            return Err(Status::UnsupportedStructure);
        }
        read = end;
        if &kind == b"IEND" {
            break;
        }
    }
    image.truncate(write);
    Ok(())
}

// --- JPEG ---------------------------------------------------------------------

/// One marker segment. `start` is the 0xFF directly before the marker byte,
/// after any fill bytes; `payload` follows the length field.
struct Segment {
    marker: u8,
    start: usize,
    payload: usize,
    end: usize,
}

/// Reads the marker segment at `at`, skipping 0xFF fill bytes before it.
fn segment_at(input: &[u8], at: usize) -> Result<Segment, Status> {
    if input.get(at) != Some(&0xff) {
        return Err(Status::Malformed);
    }
    let mut start = at;
    while input.get(start + 1) == Some(&0xff) {
        start += 1;
    }
    let marker = *input.get(start + 1).ok_or(Status::Malformed)?;
    let standalone = matches!(marker, SOI | EOI | 0xd0..=0xd7 | 0x01);
    if standalone {
        return Ok(Segment {
            marker,
            start,
            payload: start + 2,
            end: start + 2,
        });
    }
    let length = input.get(start + 2..start + 4).ok_or(Status::Malformed)?;
    let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
    if length < 2 {
        return Err(Status::Malformed);
    }
    let end = start + 2 + length;
    if end > input.len() {
        return Err(Status::Malformed);
    }
    Ok(Segment {
        marker,
        start,
        payload: start + 4,
        end,
    })
}

/// Returns the index of the marker that ends the entropy-coded data starting
/// at `from`. Stuffed bytes (FF 00), restart markers and fill bytes are data.
fn entropy_end(input: &[u8], from: usize) -> Result<usize, Status> {
    let mut at = from;
    loop {
        let next = input
            .get(at..)
            .and_then(|rest| rest.iter().position(|&byte| byte == 0xff))
            .ok_or(Status::Malformed)?
            + at;
        match *input.get(next + 1).ok_or(Status::Malformed)? {
            0x00 | 0xd0..=0xd7 => at = next + 2,
            0xff => at = next + 1,
            _ => return Ok(next),
        }
    }
}

const fn is_sof(marker: u8) -> bool {
    matches!(marker, 0xc0..=0xcf) && !matches!(marker, DHT | 0xc8 | 0xcc)
}

fn is_exif(input: &[u8], segment: &Segment) -> bool {
    segment.marker == APP1 && input[segment.payload..segment.end].starts_with(exif::EXIF_HEADER)
}

/// The orientation of a JPEG: from the first EXIF APP1 segment before the
/// first scan, or 1. The strip uses the same rule, so a stripped copy reports
/// the orientation of its original.
pub(crate) fn jpeg_orientation(input: &[u8]) -> u8 {
    let mut at = 2;
    while let Ok(segment) = segment_at(input, at) {
        if matches!(segment.marker, SOS | EOI) {
            break;
        }
        if is_exif(input, &segment) {
            return exif::orientation(&input[segment.payload..segment.end]);
        }
        at = segment.end;
    }
    1
}

fn strip_jpeg(image: &mut Vec<u8>) -> Result<(), Status> {
    let mut read = 2;
    let mut write = 2;
    let mut frame_seen = false;
    let mut scan_seen = false;
    let mut exif_seen = false;
    loop {
        let segment = segment_at(image, read)?;
        let keep = match segment.marker {
            EOI => {
                image.copy_within(segment.start..segment.end, write);
                write += 2;
                break;
            }
            APP14 | DQT | DHT | DRI => true,
            marker if is_sof(marker) => {
                // One frame only; hierarchical (DHP) and multi-frame files reject.
                if frame_seen {
                    return Err(Status::UnsupportedStructure);
                }
                frame_seen = true;
                true
            }
            SOS => {
                if !frame_seen {
                    return Err(Status::Malformed);
                }
                scan_seen = true;
                let end = entropy_end(image, segment.end)?;
                image.copy_within(segment.start..end, write);
                write += end - segment.start;
                read = end;
                continue;
            }
            APP1 if !scan_seen && !exif_seen && is_exif(image, &segment) => {
                exif_seen = true;
                let orientation = exif::orientation(&image[segment.payload..segment.end]);
                if orientation != 1 {
                    // An EXIF segment that holds a valid Orientation entry is
                    // at least MINIMAL_APP1_LEN long, and write <= start.
                    if write + exif::MINIMAL_APP1_LEN > segment.end {
                        return Err(Status::Malformed);
                    }
                    image[write..write + exif::MINIMAL_APP1_LEN]
                        .copy_from_slice(&exif::minimal_app1(orientation));
                    write += exif::MINIMAL_APP1_LEN;
                }
                false
            }
            0xe0..=0xef | COM => false,
            // SOI again, DNL, DHP, EXP, DAC, JPGn, TEM, a restart marker
            // outside a scan, or a reserved marker.
            _ => return Err(Status::UnsupportedStructure),
        };
        if keep {
            image.copy_within(segment.start..segment.end, write);
            write += segment.end - segment.start;
        }
        read = segment.end;
    }
    if !scan_seen {
        return Err(Status::Malformed);
    }
    image.truncate(write);
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Fixtures: `../../media-sandbox/tests/fixtures/*.jpg` and the two files
    //! in `tests/fixtures/` are synthetic Pillow 12.2 images with no metadata
    //! other than a JFIF APP0 or an Adobe APP14 segment.
    //! `restart-48x32-baseline.jpg`: an RGB gradient
    //! `(x * 5, y * 8, (x + y) * 3)`, `quality=90, restart_marker_blocks=1`
    //! (DRI and RST0 to RST4). `cmyk-8x8-adobe.jpg`: solid CMYK
    //! `(10, 200, 30, 40)`, `quality=90` (Adobe APP14). Metadata segments are
    //! added in test code.

    use super::*;
    use crate::exif::tests::exif_payload;

    const SOLID: &[u8] =
        include_bytes!("../../media-sandbox/tests/fixtures/solid-16x8-baseline.jpg");
    const PROGRESSIVE: &[u8] =
        include_bytes!("../../media-sandbox/tests/fixtures/gradient-33x17-progressive.jpg");
    const RESTART: &[u8] = include_bytes!("../tests/fixtures/restart-48x32-baseline.jpg");
    const ADOBE: &[u8] = include_bytes!("../tests/fixtures/cmyk-8x8-adobe.jpg");

    fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xff, marker];
        bytes.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    /// Inserts `segments` directly after SOI.
    fn with_segments(jpeg: &[u8], segments: &[Vec<u8>]) -> Vec<u8> {
        let mut out = jpeg[..2].to_vec();
        for segment in segments {
            out.extend_from_slice(segment);
        }
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    /// Markers of every segment up to and including EOI.
    fn markers(jpeg: &[u8]) -> Vec<u8> {
        let mut found = vec![];
        let mut at = 2;
        loop {
            let segment = segment_at(jpeg, at).unwrap();
            found.push(segment.marker);
            match segment.marker {
                EOI => return found,
                SOS => at = entropy_end(jpeg, segment.end).unwrap(),
                _ => at = segment.end,
            }
        }
    }

    fn exif(orientation: u32) -> Vec<u8> {
        segment(
            APP1,
            &exif_payload(
                false,
                &[
                    // Make, a GPS IFD pointer and Orientation.
                    (0x010f, 2, 4, 0x4142_4300),
                    (0x8825, 4, 1, 0),
                    (0x0112, 3, 1, orientation),
                ],
            ),
        )
    }

    fn metadata() -> Vec<Vec<u8>> {
        vec![
            segment(APP1, b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta secret/>"),
            segment(0xe2, b"ICC_PROFILE\0\x01\x01profile-bytes"),
            segment(0xed, b"Photoshop 3.0\08BIM"),
            segment(COM, b"camera serial 1234"),
        ]
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn given_jpeg_with_metadata_when_stripped_then_only_coding_segments_and_orientation_remain() {
        let mut segments = vec![exif(6)];
        segments.extend(metadata());
        let input = with_segments(SOLID, &segments);
        assert_eq!(decode(&input).unwrap().orientation, 6);
        let stripped = strip(&input).unwrap();
        assert!(stripped.len() < input.len());
        assert_eq!(
            markers(&stripped),
            [APP1, DQT, DQT, 0xc0, DHT, DHT, DHT, DHT, SOS, EOI]
        );
        assert_eq!(&stripped[2..38], &exif::minimal_app1(6));
        for secret in [
            &b"JFIF"[..],
            b"xmpmeta",
            b"ICC_PROFILE",
            b"8BIM",
            b"serial",
            b"ABC",
        ] {
            assert!(!contains(&stripped, secret), "{secret:?}");
        }
        assert_eq!(decode(&stripped).unwrap(), decode(&input).unwrap());
    }

    #[test]
    fn given_orientation_one_or_absent_then_no_exif_segment_is_written() {
        for segments in [vec![exif(1)], vec![exif(42)], metadata()] {
            let stripped = strip(&with_segments(SOLID, &segments)).unwrap();
            assert!(!markers(&stripped).contains(&APP1));
            assert_eq!(decode(&stripped).unwrap().orientation, 1);
        }
    }

    #[test]
    fn given_two_exif_segments_then_the_first_decides_and_both_are_removed() {
        let input = with_segments(SOLID, &[exif(3), exif(8)]);
        assert_eq!(decode(&input).unwrap().orientation, 3);
        let stripped = strip(&input).unwrap();
        assert_eq!(markers(&stripped).iter().filter(|&&m| m == APP1).count(), 1);
        assert_eq!(decode(&stripped).unwrap().orientation, 3);
    }

    #[test]
    fn given_adobe_jpeg_then_app14_is_kept_byte_for_byte() {
        let adobe = segment_at(ADOBE, 2).unwrap();
        assert_eq!(adobe.marker, APP14);
        let input = with_segments(ADOBE, &metadata());
        let stripped = strip(&input).unwrap();
        assert_eq!(&stripped[2..2 + adobe.end - 2], &ADOBE[2..adobe.end]);
        assert_eq!(decode(&stripped).unwrap(), decode(ADOBE).unwrap());
    }

    #[test]
    fn given_progressive_and_restart_jpegs_then_scans_are_copied_unchanged() {
        for jpeg in [PROGRESSIVE, RESTART] {
            let jfif = segment_at(jpeg, 2).unwrap();
            assert_eq!(jfif.marker, 0xe0);
            let stripped = strip(&with_segments(jpeg, &[exif(5)])).unwrap();
            // The output is the minimal EXIF segment followed by the original
            // without its JFIF segment.
            assert_eq!(&stripped[..2], &jpeg[..2]);
            assert_eq!(&stripped[2..38], &exif::minimal_app1(5));
            assert_eq!(&stripped[38..], &jpeg[jfif.end..]);
        }
        assert!(markers(RESTART).contains(&DRI));
        assert!(contains(RESTART, &[0xff, 0xd4]));
    }

    #[test]
    fn given_metadata_after_the_first_scan_or_data_after_eoi_then_it_is_removed() {
        let mut input = with_segments(PROGRESSIVE, &[]);
        let last_scan = input.len() - 2;
        input.splice(last_scan..last_scan, segment(COM, b"late comment"));
        input.splice(last_scan..last_scan, exif(6));
        input.extend_from_slice(b"appended video bytes");
        let stripped = strip(&input).unwrap();
        assert!(stripped.ends_with(&[0xff, EOI]));
        assert!(!contains(&stripped, b"late comment"));
        assert!(!contains(&stripped, b"appended"));
        // EXIF after the first scan does not count as orientation.
        assert!(!markers(&stripped).contains(&APP1));
    }

    #[test]
    fn given_truncated_or_garbage_jpeg_then_strip_rejects() {
        let input = with_segments(SOLID, &[exif(6)]);
        for cut in [3, 6, 40, input.len() / 2, input.len() - 1] {
            assert!(strip(&input[..cut]).is_err(), "cut at {cut}");
        }
        assert_eq!(strip(b"\xff\xd8\xffgarbage"), Err(Status::Malformed));
        assert_eq!(strip(b"GIF89a"), Err(Status::UnsupportedFormat));
    }

    #[test]
    fn given_segment_length_past_the_end_or_below_two_then_strip_rejects() {
        let mut image = with_segments(SOLID, &[segment(COM, b"x")]);
        image[4..6].copy_from_slice(&0xfff0u16.to_be_bytes());
        assert!(strip_jpeg(&mut image.clone()).is_err());
        image[4..6].copy_from_slice(&1u16.to_be_bytes());
        assert_eq!(strip_jpeg(&mut image), Err(Status::Malformed));
        // The decoder refuses these too; strip rejects either way.
        assert!(strip(&image).is_err());
    }

    #[test]
    fn given_unsupported_markers_then_strip_rejects_structure() {
        for marker in [0xdc, 0xde, 0xcc, 0xf0, 0xd0, SOI] {
            let mut image = with_segments(SOLID, &[]);
            let extra = if marker == 0xd0 || marker == SOI {
                vec![0xff, marker]
            } else {
                segment(marker, &[0, 0])
            };
            image.splice(2..2, extra);
            assert_eq!(
                strip_jpeg(&mut image),
                Err(Status::UnsupportedStructure),
                "{marker:#x}"
            );
        }
        let mut two_frames = SOLID.to_vec();
        let frame = segment_at(SOLID, 2 + 18 + 69 + 69).unwrap();
        assert_eq!(frame.marker, 0xc0);
        let copy = SOLID[frame.start..frame.end].to_vec();
        two_frames.splice(frame.end..frame.end, copy);
        assert_eq!(
            strip_jpeg(&mut two_frames),
            Err(Status::UnsupportedStructure)
        );
    }

    #[test]
    fn given_scan_without_frame_or_missing_eoi_then_strip_rejects() {
        let mut no_eoi = SOLID[..SOLID.len() - 2].to_vec();
        assert_eq!(strip_jpeg(&mut no_eoi), Err(Status::Malformed));
        let mut no_scan = SOLID[..2].to_vec();
        no_scan.extend_from_slice(&[0xff, EOI]);
        assert_eq!(strip_jpeg(&mut no_scan), Err(Status::Malformed));
        let mut early_scan = SOLID[..2].to_vec();
        early_scan.extend_from_slice(&segment(SOS, &[0; 10]));
        early_scan.extend_from_slice(&[0xff, EOI]);
        assert_eq!(strip_jpeg(&mut early_scan), Err(Status::Malformed));
    }

    #[test]
    fn given_jpeg_orientation_then_only_first_exif_before_the_scan_counts() {
        assert_eq!(jpeg_orientation(SOLID), 1);
        assert_eq!(jpeg_orientation(&with_segments(SOLID, &[exif(7)])), 7);
        let mut late = SOLID.to_vec();
        let end = late.len() - 2;
        late.splice(end..end, exif(7));
        assert_eq!(jpeg_orientation(&late), 1);
        // A broken segment length stops the walk with the default.
        let mut broken = with_segments(SOLID, &[segment(COM, b"x"), exif(7)]);
        broken[4..6].copy_from_slice(&0xfff0u16.to_be_bytes());
        assert_eq!(jpeg_orientation(&broken), 1);
    }

    // --- PNG ----------------------------------------------------------------

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut bytes = (data.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        let crc = crc32(&bytes[4..]);
        bytes.extend_from_slice(&crc.to_be_bytes());
        bytes
    }

    /// Chunk types in order.
    fn chunk_types(png: &[u8]) -> Vec<[u8; 4]> {
        let mut found = vec![];
        let mut at = 8;
        while at < png.len() {
            let length = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            found.push(png[at + 4..at + 8].try_into().unwrap());
            at += length + 12;
        }
        found
    }

    /// An indexed PNG with a palette and transparency, encoded by `png`.
    fn indexed_png() -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, 3, 2);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(vec![255, 0, 0, 0, 255, 0, 0, 0, 255]);
        encoder.set_trns(vec![255, 128, 0]);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[0, 1, 2, 2, 1, 0])
            .unwrap();
        out
    }

    /// Inserts `chunks` directly after IHDR.
    fn with_chunks(png: &[u8], chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut out = png[..33].to_vec();
        for chunk in chunks {
            out.extend_from_slice(chunk);
        }
        out.extend_from_slice(&png[33..]);
        out
    }

    #[test]
    fn given_png_with_ancillary_chunks_when_stripped_then_kept_chunks_are_byte_identical() {
        let original = indexed_png();
        assert_eq!(
            chunk_types(&original),
            [*b"IHDR", *b"PLTE", *b"tRNS", *b"IDAT", *b"IEND"]
        );
        let dropped = [
            chunk(b"tEXt", b"Comment\0secret"),
            chunk(b"iTXt", b"XML:com.adobe.xmp\0\0\0\0\0secret"),
            chunk(b"zTXt", b"Title\0\0x"),
            chunk(b"eXIf", &exif_payload(true, &[(0x0112, 3, 1, 6)])[6..]),
            chunk(b"iCCP", b"profile\0\0x"),
            chunk(b"tIME", &[7, 234, 10, 24, 12, 0, 0]),
            chunk(b"pHYs", &[0, 0, 11, 19, 0, 0, 11, 19, 1]),
            chunk(b"gAMA", &45455u32.to_be_bytes()),
            chunk(b"prVt", b"private"),
        ];
        let mut input = with_chunks(&original, &dropped);
        // An ancillary chunk between IDAT and IEND, and trailing bytes.
        let iend = input.len() - 12;
        input.splice(iend..iend, chunk(b"tEXt", b"late\0secret"));
        input.extend_from_slice(b"trailing");
        let stripped = strip(&input).unwrap();
        assert_eq!(stripped, original);
        assert_eq!(decode(&stripped).unwrap().orientation, 1);
    }

    #[test]
    fn given_corrupt_chunk_length_or_type_then_strip_rejects() {
        let original = indexed_png();
        // PLTE starts at byte 33.
        for length in [u32::MAX, 0x8000_0000, 10_000] {
            let mut bad = original.clone();
            bad[33..37].copy_from_slice(&length.to_be_bytes());
            assert_eq!(strip_png(&mut bad), Err(Status::Malformed), "{length}");
            assert!(strip(&bad).is_err());
        }
        let mut bad_type = original.clone();
        bad_type[37] = b'1';
        assert_eq!(strip_png(&mut bad_type), Err(Status::Malformed));
        let mut missing_iend = original[..original.len() - 12].to_vec();
        assert_eq!(strip_png(&mut missing_iend), Err(Status::Malformed));
        let mut not_ihdr_first = original[..8].to_vec();
        not_ihdr_first.extend_from_slice(&original[33..]);
        assert_eq!(strip_png(&mut not_ihdr_first), Err(Status::Malformed));
        let mut two_ihdr = with_chunks(&original, &[original[8..33].to_vec()]);
        assert_eq!(strip_png(&mut two_ihdr), Err(Status::Malformed));
    }

    #[test]
    fn given_unknown_critical_chunk_then_strip_rejects_structure() {
        let mut input = with_chunks(&indexed_png(), &[chunk(b"ABCD", b"x")]);
        assert_eq!(strip_png(&mut input), Err(Status::UnsupportedStructure));
    }

    #[test]
    fn given_animated_png_when_stripped_then_animation_chunks_are_dropped() {
        let mut apng = Vec::new();
        let mut encoder = png::Encoder::new(&mut apng, 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(2, 0).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[10; 16]).unwrap();
        writer.write_image_data(&[200; 16]).unwrap();
        writer.finish().unwrap();
        let types = chunk_types(&apng);
        for kind in [b"acTL", b"fcTL", b"fdAT"] {
            assert!(types.contains(kind), "{types:?}");
        }
        let stripped = strip(&apng).unwrap();
        assert_eq!(chunk_types(&stripped), [*b"IHDR", *b"IDAT", *b"IEND"]);
        assert_eq!(decode(&stripped).unwrap().pixels, [10; 16]);
    }

    #[test]
    fn given_stripped_copy_with_other_pixels_or_orientation_then_check_reports_mismatch() {
        let source = indexed_png();
        let original = fingerprint(&source).unwrap();
        assert_eq!(check_same(&original, &source), Ok(()));
        let mut other = Vec::new();
        let mut encoder = png::Encoder::new(&mut other, 3, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut pixels = decode(&source).unwrap().pixels;
        pixels[5] ^= 1;
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&pixels)
            .unwrap();
        assert_eq!(check_same(&original, &other), Err(Status::StripMismatch));
        assert_eq!(
            check_same(&original, b"garbage"),
            Err(Status::StripMismatch)
        );
        let rotated = fingerprint(&with_segments(SOLID, &[exif(6)])).unwrap();
        assert_eq!(check_same(&rotated, SOLID), Err(Status::StripMismatch));
    }

    #[test]
    fn given_pixel_hash_then_single_byte_and_length_changes_are_detected() {
        let base: Vec<u8> = (0..1000u32).map(|i| (i * 31) as u8).collect();
        let hash = pixel_hash(&base);
        for index in [0, 7, 8, 500, 999] {
            let mut changed = base.clone();
            changed[index] ^= 0x80;
            assert_ne!(pixel_hash(&changed), hash, "{index}");
        }
        assert_ne!(pixel_hash(&base[..999]), hash);
        assert_eq!(pixel_hash(&base), hash);
    }

    #[test]
    fn given_input_over_limit_then_strip_refuses_before_decoding() {
        assert_eq!(
            strip(&vec![0; crate::MAX_INPUT_BYTES + 1]),
            Err(Status::InputTooLarge)
        );
    }
}

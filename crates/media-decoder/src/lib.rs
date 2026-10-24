//! Untrusted image decoder, built as a `wasm32-unknown-unknown` guest for the
//! `persephone-media-sandbox` host (ADR-0003).
//!
//! The guest exports two operations, `decode` and `strip` (ADR-0009). Its only
//! capabilities are four host imports in the `persephone` module: `input_len`,
//! `read_input`, `write_output` (decoded pixels) and `write_stripped` (the
//! stripped file). It has no WASI, so it cannot open sockets or files, read
//! clocks or the environment, or start processes. The logic below is ordinary
//! Rust, so host tests can also exercise it natively.

use std::io::Cursor;

mod exif;
mod strip;

pub use strip::{strip, strip_in_place};

/// Largest encoded input the guest accepts (ADR-0003 starting budget).
pub const MAX_INPUT_BYTES: usize = 20 * 1024 * 1024;
/// Largest width or height accepted, shared with the worker frame protocol.
pub use persephone_media_protocol::MAX_DIMENSION;
/// Largest pixel count accepted (16,000,000), shared with the worker frame
/// protocol so every decodable image also fits one RGBA pixel frame.
pub use persephone_media_protocol::MAX_PIXELS;
/// Allocation ceiling handed to the PNG decoder for its internal buffers.
const PNG_INTERNAL_LIMIT_BYTES: usize = 64 * 1024 * 1024;

/// Status codes returned by the exported `decode` and `strip` functions. Zero
/// is success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Status {
    Ok = 0,
    UnsupportedFormat = 1,
    Malformed = 2,
    TooLarge = 3,
    InputTooLarge = 4,
    HostRejectedInput = 5,
    HostRejectedOutput = 6,
    /// Strip found valid but unsupported structure, such as an unknown
    /// critical PNG chunk or a JPEG marker outside the kept set.
    UnsupportedStructure = 7,
    /// The stripped copy did not decode to the same pixels, dimensions and
    /// orientation as the original.
    StripMismatch = 8,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    /// EXIF orientation, 1 to 8. The pixels are not rotated. Always 1 for PNG.
    pub orientation: u8,
    pub pixels: Vec<u8>,
}

/// Checks dimensions before any pixel buffer is allocated.
fn check_dimensions(width: u32, height: u32) -> Result<usize, Status> {
    if width == 0 || height == 0 {
        return Err(Status::Malformed);
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(Status::TooLarge);
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_PIXELS {
        return Err(Status::TooLarge);
    }
    usize::try_from(pixels * 4).map_err(|_| Status::TooLarge)
}

/// Decodes PNG or JPEG input to 8-bit RGBA. Only the first frame of an
/// animated PNG is decoded.
pub fn decode(input: &[u8]) -> Result<Rgba, Status> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Status::InputTooLarge);
    }
    if input.starts_with(b"\x89PNG\r\n\x1a\n") {
        decode_png(input)
    } else if input.starts_with(&[0xff, 0xd8, 0xff]) {
        decode_jpeg(input)
    } else {
        Err(Status::UnsupportedFormat)
    }
}

fn decode_png(input: &[u8]) -> Result<Rgba, Status> {
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(input),
        png::Limits {
            bytes: PNG_INTERNAL_LIMIT_BYTES,
        },
    );
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(png_status)?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    let rgba_len = check_dimensions(width, height)?;
    let frame_len = reader.output_buffer_size().ok_or(Status::TooLarge)?;
    if frame_len > rgba_len {
        return Err(Status::Malformed);
    }
    // Reserve the full RGBA size now so widening below never reallocates,
    // which would briefly hold two frame-sized buffers.
    let mut frame = Vec::with_capacity(rgba_len);
    frame.resize(frame_len, 0);
    let output = reader.next_frame(&mut frame).map_err(png_status)?;
    if (output.width, output.height) != (width, height) || output.bit_depth != png::BitDepth::Eight
    {
        return Err(Status::Malformed);
    }
    let channels = match output.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        // normalize_to_color8 expands palettes; anything else is unexpected.
        png::ColorType::Indexed => return Err(Status::Malformed),
    };
    frame.truncate(output.line_size * height as usize);
    let pixels = widen_to_rgba(frame, channels, rgba_len)?;
    finish(width, height, 1, pixels, rgba_len)
}

/// Expands 8-bit samples with `channels` per pixel to RGBA in place. Pixels
/// are rewritten from the last to the first, so each destination lies at or
/// after its source and no unread sample is overwritten.
fn widen_to_rgba(
    mut samples: Vec<u8>,
    channels: usize,
    rgba_len: usize,
) -> Result<Vec<u8>, Status> {
    let pixels = rgba_len / 4;
    if samples.len() != pixels * channels {
        return Err(Status::Malformed);
    }
    if channels == 4 {
        return Ok(samples);
    }
    samples.resize(rgba_len, 0);
    for pixel in (0..pixels).rev() {
        let source = pixel * channels;
        let rgba = match channels {
            3 => [
                samples[source],
                samples[source + 1],
                samples[source + 2],
                0xff,
            ],
            2 => {
                let gray = samples[source];
                [gray, gray, gray, samples[source + 1]]
            }
            _ => {
                let gray = samples[source];
                [gray, gray, gray, 0xff]
            }
        };
        samples[pixel * 4..pixel * 4 + 4].copy_from_slice(&rgba);
    }
    Ok(samples)
}

fn png_status(error: png::DecodingError) -> Status {
    match error {
        png::DecodingError::LimitsExceeded => Status::TooLarge,
        _ => Status::Malformed,
    }
}

fn decode_jpeg(input: &[u8]) -> Result<Rgba, Status> {
    use zune_jpeg::zune_core::{colorspace::ColorSpace, options::DecoderOptions};

    // A file cut short must not be shown in part (docs/architecture/
    // image-messaging.md). The structure check proves that every scan ends
    // at a marker and that EOI follows. Strict mode turns zune-jpeg's
    // recovery from bad entropy data (an invalid Huffman code, data that
    // runs out, an unexpected marker) into an error; without it the decoder
    // fills the rest of the image with grey and reports success.
    //
    // Limit: zune-jpeg zero-fills a scan that ends at a valid marker before
    // all of its blocks are decoded, in strict mode too, and its public API
    // does not report this. A file truncated inside a scan and then given a
    // new EOI can therefore still decode, with wrong blocks at the end. Such
    // a file needs a deliberate edit; a transfer that stops early has no EOI.
    strip::check_jpeg_structure(input)?;
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGBA)
        .set_max_width(MAX_DIMENSION as usize)
        .set_max_height(MAX_DIMENSION as usize)
        .set_strict_mode(true)
        // Hand-written unsafe fast paths are not needed inside the sandbox.
        .set_use_unsafe(false);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(Cursor::new(input), options);
    decoder.decode_headers().map_err(|_| Status::Malformed)?;
    let (width, height) = decoder.dimensions().ok_or(Status::Malformed)?;
    let width = u32::try_from(width).map_err(|_| Status::TooLarge)?;
    let height = u32::try_from(height).map_err(|_| Status::TooLarge)?;
    let rgba_len = check_dimensions(width, height)?;
    let pixels = decoder.decode().map_err(|_| Status::Malformed)?;
    finish(
        width,
        height,
        strip::jpeg_orientation(input),
        pixels,
        rgba_len,
    )
}

fn finish(
    width: u32,
    height: u32,
    orientation: u8,
    pixels: Vec<u8>,
    rgba_len: usize,
) -> Result<Rgba, Status> {
    if pixels.len() != rgba_len {
        return Err(Status::Malformed);
    }
    Ok(Rgba {
        width,
        height,
        orientation,
        pixels,
    })
}

#[cfg(target_arch = "wasm32")]
mod guest {
    use super::{MAX_INPUT_BYTES, Status, decode as decode_image, strip_in_place};

    #[link(wasm_import_module = "persephone")]
    unsafe extern "C" {
        fn input_len() -> u32;
        fn read_input(destination: *mut u8, length: u32) -> i32;
        fn write_output(
            width: u32,
            height: u32,
            source: *const u8,
            length: u32,
            orientation: u32,
        ) -> i32;
        fn write_stripped(source: *const u8, length: u32) -> i32;
    }

    /// Reads the input from the host, decodes it and hands RGBA pixels and
    /// the EXIF orientation back.
    #[unsafe(no_mangle)]
    pub extern "C" fn decode() -> i32 {
        run_decode().unwrap_or_else(|status| status) as i32
    }

    /// Reads the input from the host, strips its metadata in place, checks
    /// that the pixels are unchanged and hands the stripped file back.
    #[unsafe(no_mangle)]
    pub extern "C" fn strip() -> i32 {
        run_strip().unwrap_or_else(|status| status) as i32
    }

    fn read_all() -> Result<Vec<u8>, Status> {
        // SAFETY: the host import takes no arguments.
        let length = unsafe { input_len() } as usize;
        if length > MAX_INPUT_BYTES {
            return Err(Status::InputTooLarge);
        }
        let mut input = vec![0u8; length];
        // SAFETY: `input` is a writable buffer of exactly `length` bytes.
        if unsafe { read_input(input.as_mut_ptr(), length as u32) } != 0 {
            return Err(Status::HostRejectedInput);
        }
        Ok(input)
    }

    fn run_decode() -> Result<Status, Status> {
        let input = read_all()?;
        let image = decode_image(&input)?;
        drop(input);
        // SAFETY: `pixels` is a readable buffer of exactly the given length.
        let written = unsafe {
            write_output(
                image.width,
                image.height,
                image.pixels.as_ptr(),
                image.pixels.len() as u32,
                u32::from(image.orientation),
            )
        };
        if written != 0 {
            return Err(Status::HostRejectedOutput);
        }
        // The host reads these pixels from guest memory after `decode`
        // returns. Freeing them would let the allocator write its bookkeeping
        // into the region first, so the buffer is deliberately leaked; each
        // instance performs one operation and is then discarded.
        std::mem::forget(image);
        Ok(Status::Ok)
    }

    fn run_strip() -> Result<Status, Status> {
        let mut image = read_all()?;
        strip_in_place(&mut image)?;
        // SAFETY: `image` is a readable buffer of exactly the given length.
        let written = unsafe { write_stripped(image.as_ptr(), image.len() as u32) };
        if written != 0 {
            return Err(Status::HostRejectedOutput);
        }
        // Leaked for the same reason as the decoded pixels above.
        std::mem::forget(image);
        Ok(Status::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(width: u32, height: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(data)
            .unwrap();
        out
    }

    #[test]
    fn given_rgb_and_gray_png_when_decoded_then_output_is_rgba() {
        let rgb = png_bytes(2, 1, png::ColorType::Rgb, &[1, 2, 3, 4, 5, 6]);
        assert_eq!(
            decode(&rgb).unwrap(),
            Rgba {
                width: 2,
                height: 1,
                orientation: 1,
                pixels: vec![1, 2, 3, 255, 4, 5, 6, 255]
            }
        );
        let gray = png_bytes(1, 2, png::ColorType::GrayscaleAlpha, &[9, 10, 11, 12]);
        assert_eq!(decode(&gray).unwrap().pixels, [9, 9, 9, 10, 11, 11, 11, 12]);
    }

    #[test]
    fn given_every_channel_count_when_widened_in_place_then_rgba_is_exact() {
        assert_eq!(
            widen_to_rgba(vec![1, 2, 3, 4, 5, 6], 3, 8).unwrap(),
            [1, 2, 3, 255, 4, 5, 6, 255]
        );
        assert_eq!(
            widen_to_rgba(vec![7, 8, 9, 10], 2, 8).unwrap(),
            [7, 7, 7, 8, 9, 9, 9, 10]
        );
        assert_eq!(
            widen_to_rgba(vec![11, 12], 1, 8).unwrap(),
            [11, 11, 11, 255, 12, 12, 12, 255]
        );
        assert_eq!(widen_to_rgba(vec![1, 2, 3, 4], 4, 4).unwrap(), [1, 2, 3, 4]);
        assert_eq!(widen_to_rgba(vec![1, 2, 3], 2, 8), Err(Status::Malformed));
        let gray = png_bytes(3, 1, png::ColorType::Grayscale, &[20, 40, 60]);
        assert_eq!(
            decode(&gray).unwrap().pixels,
            [20, 20, 20, 255, 40, 40, 40, 255, 60, 60, 60, 255]
        );
    }

    #[test]
    fn given_unknown_truncated_or_oversized_input_then_decode_refuses() {
        assert_eq!(decode(b"GIF89a"), Err(Status::UnsupportedFormat));
        let png = png_bytes(2, 2, png::ColorType::Rgba, &[0; 16]);
        assert_eq!(decode(&png[..png.len() - 20]), Err(Status::Malformed));
        assert_eq!(
            decode(&vec![0; MAX_INPUT_BYTES + 1]),
            Err(Status::InputTooLarge)
        );
    }

    #[test]
    fn given_dimension_limits_then_checks_happen_before_allocation() {
        assert_eq!(check_dimensions(0, 1), Err(Status::Malformed));
        assert_eq!(
            check_dimensions(MAX_DIMENSION + 1, 1),
            Err(Status::TooLarge)
        );
        assert_eq!(
            check_dimensions(MAX_DIMENSION, MAX_DIMENSION),
            Err(Status::TooLarge)
        );
        assert_eq!(check_dimensions(4000, 4000), Ok(4000 * 4000 * 4));
        assert_eq!(check_dimensions(4001, 4000), Err(Status::TooLarge));
    }

    /// A PNG header that claims 100000 x 100000 pixels with almost no data
    /// must be refused from its header alone.
    #[test]
    fn given_png_header_claiming_huge_dimensions_then_too_large_without_decoding() {
        let mut png = png_bytes(1, 1, png::ColorType::Rgba, &[0; 4]);
        // IHDR width and height start at byte 16; fix up the chunk CRC too.
        png[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        png[20..24].copy_from_slice(&100_000u32.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(decode(&png), Err(Status::TooLarge));
    }

    // --- Truncated and corrupt input -------------------------------------

    const SOLID: &[u8] =
        include_bytes!("../../media-sandbox/tests/fixtures/solid-16x8-baseline.jpg");
    const PROGRESSIVE: &[u8] =
        include_bytes!("../../media-sandbox/tests/fixtures/gradient-33x17-progressive.jpg");
    const RESTART: &[u8] = include_bytes!("../tests/fixtures/restart-48x32-baseline.jpg");
    const ADOBE: &[u8] = include_bytes!("../tests/fixtures/cmyk-8x8-adobe.jpg");
    const JPEGS: [(&str, &[u8]); 4] = [
        ("baseline", SOLID),
        ("progressive", PROGRESSIVE),
        ("restart", RESTART),
        ("adobe", ADOBE),
    ];

    /// The decode as it was before strict mode and the structure check:
    /// used to prove that valid files still give the same pixels.
    fn lenient_jpeg(input: &[u8]) -> Option<Vec<u8>> {
        use zune_jpeg::zune_core::{colorspace::ColorSpace, options::DecoderOptions};
        let options = DecoderOptions::default()
            .jpeg_set_out_colorspace(ColorSpace::RGBA)
            .set_use_unsafe(false);
        zune_jpeg::JpegDecoder::new_with_options(Cursor::new(input), options)
            .decode()
            .ok()
    }

    /// The offset where the entropy-coded data of the first scan starts.
    fn first_scan_data(jpeg: &[u8]) -> usize {
        let mut at = 2;
        loop {
            assert_eq!(jpeg[at], 0xff);
            let length = usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
            if jpeg[at + 1] == 0xda {
                return at + 2 + length;
            }
            at += 2 + length;
        }
    }

    #[test]
    fn given_valid_jpeg_fixtures_when_decoded_strictly_then_pixels_equal_the_lenient_decode() {
        for (name, jpeg) in JPEGS {
            let strict = decode(jpeg).unwrap_or_else(|status| panic!("{name}: {status:?}"));
            assert_eq!(Some(strict.pixels), lenient_jpeg(jpeg), "{name}");
            // Bytes after EOI are ignored, as before.
            let mut trailing = jpeg.to_vec();
            trailing.extend_from_slice(b"appended bytes");
            assert_eq!(decode(&trailing), decode(jpeg), "{name}");
        }
    }

    /// GIVEN a JPEG cut at any byte after its signature WHEN decoded THEN it
    /// is refused as malformed, so no partial image is returned.
    #[test]
    fn given_jpeg_truncated_anywhere_when_decoded_then_malformed() {
        for (name, jpeg) in JPEGS {
            let scan = first_scan_data(jpeg);
            assert!(scan < jpeg.len() - 2, "{name}");
            for cut in 3..jpeg.len() {
                assert_eq!(
                    decode(&jpeg[..cut]),
                    Err(Status::Malformed),
                    "{name} cut at {cut}"
                );
            }
        }
    }

    #[test]
    fn given_jpeg_with_eoi_removed_when_decoded_then_malformed() {
        for (name, jpeg) in JPEGS {
            assert!(jpeg.ends_with(&[0xff, 0xd9]), "{name}");
            assert_eq!(
                decode(&jpeg[..jpeg.len() - 2]),
                Err(Status::Malformed),
                "{name}"
            );
        }
    }

    /// GIVEN garbage in place of part of the first scan WHEN decoded THEN it
    /// is refused. The garbage is stuffed 0xFF bytes (FF 00): a run of one
    /// bits that is not a valid Huffman code. The structure is intact, so
    /// only strict mode catches it; the lenient decode succeeds.
    #[test]
    fn given_jpeg_with_garbage_in_its_scan_when_decoded_then_malformed() {
        for (name, jpeg) in JPEGS {
            let start = first_scan_data(jpeg);
            let mut corrupt = jpeg.to_vec();
            corrupt.splice(start..start + 4, [0xff, 0x00].repeat(8));
            assert_eq!(decode(&corrupt), Err(Status::Malformed), "{name}");
            assert!(lenient_jpeg(&corrupt).is_some(), "{name}: lenient decode");
        }
    }

    /// GIVEN a PNG cut inside its IDAT chunk, or with a short IDAT that still
    /// has a valid CRC and IEND WHEN decoded THEN it is refused as malformed.
    #[test]
    fn given_png_truncated_inside_idat_when_decoded_then_malformed() {
        let (width, height) = (32u32, 32u32);
        let pixels: Vec<u8> = (0..width * height * 4)
            .map(|i| (i * 7 % 251) as u8)
            .collect();
        let png = png_bytes(width, height, png::ColorType::Rgba, &pixels);
        assert_eq!(decode(&png).unwrap().pixels, pixels);
        // IHDR ends at byte 33; this encoder writes one IDAT next.
        assert_eq!(&png[37..41], b"IDAT");
        let idat_len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        let idat_data = 41;
        assert!(idat_len > 64);
        for cut in (idat_data..idat_data + idat_len + 4).step_by(7) {
            assert_eq!(decode(&png[..cut]), Err(Status::Malformed), "cut at {cut}");
        }
        let short = idat_len / 2;
        let mut rebuilt = png[..33].to_vec();
        let mut chunk = (short as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(b"IDAT");
        chunk.extend_from_slice(&png[idat_data..idat_data + short]);
        let crc = crc32(&chunk[4..]);
        chunk.extend_from_slice(&crc.to_be_bytes());
        rebuilt.extend_from_slice(&chunk);
        rebuilt.extend_from_slice(&png[png.len() - 12..]);
        assert_eq!(&rebuilt[rebuilt.len() - 8..rebuilt.len() - 4], b"IEND");
        assert_eq!(decode(&rebuilt), Err(Status::Malformed));
    }

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
}

//! Bounded reader and writer for the EXIF Orientation tag (TIFF IFD0 tag
//! 0x0112) in a JPEG APP1 segment.
//!
//! Only IFD0 is read. Every offset is checked against the segment before use,
//! and anything unexpected gives the default orientation 1: an unknown byte
//! order, a wrong TIFF magic, an IFD outside the segment, an Orientation entry
//! that is not one SHORT, or a value outside 1 to 8. Other tags are ignored.

/// Payload prefix of an EXIF APP1 segment.
pub(crate) const EXIF_HEADER: &[u8; 6] = b"Exif\0\0";
const ORIENTATION_TAG: u16 = 0x0112;
const TIFF_SHORT: u16 = 3;
const TIFF_HEADER_LEN: usize = 8;
const IFD_ENTRY_LEN: usize = 12;

/// Length of [`minimal_app1`]: marker (2), length field (2), EXIF header (6),
/// TIFF header (8), entry count (2), one entry (12), next-IFD offset (4).
pub(crate) const MINIMAL_APP1_LEN: usize = 36;

/// Returns the orientation from an APP1 payload (the bytes after the segment
/// length), or 1 when it is absent or invalid.
pub(crate) fn orientation(payload: &[u8]) -> u8 {
    read_orientation(payload).unwrap_or(1)
}

fn read_orientation(payload: &[u8]) -> Option<u8> {
    let tiff = payload.strip_prefix(EXIF_HEADER)?;
    let big_endian = match tiff.get(..2)? {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let bytes: [u8; 2] = tiff.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let bytes: [u8; 4] = tiff.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd = usize::try_from(u32_at(4)?).ok()?;
    if ifd < TIFF_HEADER_LEN {
        return None;
    }
    let count = usize::from(u16_at(ifd)?);
    let entries = ifd.checked_add(2)?;
    // The whole IFD, including its next-IFD offset, must lie in the segment.
    let end = entries
        .checked_add(count.checked_mul(IFD_ENTRY_LEN)?)?
        .checked_add(4)?;
    if end > tiff.len() {
        return None;
    }
    for index in 0..count {
        let entry = entries + index * IFD_ENTRY_LEN;
        if u16_at(entry)? != ORIENTATION_TAG {
            continue;
        }
        if u16_at(entry + 2)? != TIFF_SHORT || u32_at(entry + 4)? != 1 {
            return None;
        }
        // A single SHORT is stored in the first two bytes of the value field.
        let value = u16_at(entry + 8)?;
        return u8::try_from(value)
            .ok()
            .filter(|value| (1..=8).contains(value));
    }
    None
}

/// One big-endian APP1 segment whose IFD0 holds only the Orientation tag.
pub(crate) fn minimal_app1(orientation: u8) -> [u8; MINIMAL_APP1_LEN] {
    let mut segment = [0u8; MINIMAL_APP1_LEN];
    segment[..2].copy_from_slice(&[0xff, 0xe1]);
    segment[2..4].copy_from_slice(&((MINIMAL_APP1_LEN - 2) as u16).to_be_bytes());
    segment[4..10].copy_from_slice(EXIF_HEADER);
    // TIFF header: big-endian, magic 42, IFD0 directly after the header.
    segment[10..18].copy_from_slice(&[b'M', b'M', 0, 42, 0, 0, 0, 8]);
    segment[18..20].copy_from_slice(&1u16.to_be_bytes());
    segment[20..22].copy_from_slice(&ORIENTATION_TAG.to_be_bytes());
    segment[22..24].copy_from_slice(&TIFF_SHORT.to_be_bytes());
    segment[24..28].copy_from_slice(&1u32.to_be_bytes());
    segment[28..30].copy_from_slice(&u16::from(orientation).to_be_bytes());
    // Bytes 30..32 pad the value field; 32..36 is a zero next-IFD offset.
    segment
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An EXIF APP1 payload with IFD0 holding `entries` (tag, type, count,
    /// value), written in the given byte order.
    pub(crate) fn exif_payload(big_endian: bool, entries: &[(u16, u16, u32, u32)]) -> Vec<u8> {
        let u16b = |value: u16| {
            if big_endian {
                value.to_be_bytes()
            } else {
                value.to_le_bytes()
            }
        };
        let u32b = |value: u32| {
            if big_endian {
                value.to_be_bytes()
            } else {
                value.to_le_bytes()
            }
        };
        let mut payload = EXIF_HEADER.to_vec();
        payload.extend_from_slice(if big_endian { b"MM" } else { b"II" });
        payload.extend_from_slice(&u16b(42));
        payload.extend_from_slice(&u32b(8));
        payload.extend_from_slice(&u16b(entries.len() as u16));
        for &(tag, kind, count, value) in entries {
            payload.extend_from_slice(&u16b(tag));
            payload.extend_from_slice(&u16b(kind));
            payload.extend_from_slice(&u32b(count));
            if kind == TIFF_SHORT && count == 1 {
                payload.extend_from_slice(&u16b(value as u16));
                payload.extend_from_slice(&[0, 0]);
            } else {
                payload.extend_from_slice(&u32b(value));
            }
        }
        payload.extend_from_slice(&[0; 4]);
        payload
    }

    #[test]
    fn given_orientation_in_either_byte_order_then_it_is_read() {
        for big_endian in [true, false] {
            for value in 1..=8u32 {
                let payload = exif_payload(
                    big_endian,
                    &[(0x010f, 2, 4, 0), (ORIENTATION_TAG, TIFF_SHORT, 1, value)],
                );
                assert_eq!(orientation(&payload), value as u8, "{big_endian} {value}");
            }
        }
    }

    #[test]
    fn given_missing_or_out_of_range_orientation_then_default_is_one() {
        for entries in [
            &[][..],
            &[(0x010f, 2, 4, 0)],
            &[(ORIENTATION_TAG, TIFF_SHORT, 1, 0)],
            &[(ORIENTATION_TAG, TIFF_SHORT, 1, 9)],
            &[(ORIENTATION_TAG, TIFF_SHORT, 1, 0x0106)],
            // Wrong type (LONG) or count.
            &[(ORIENTATION_TAG, 4, 1, 6)],
            &[(ORIENTATION_TAG, TIFF_SHORT, 2, 6)],
        ] {
            assert_eq!(orientation(&exif_payload(true, entries)), 1, "{entries:?}");
        }
    }

    #[test]
    fn given_malformed_tiff_structure_then_default_is_one() {
        let valid = exif_payload(false, &[(ORIENTATION_TAG, TIFF_SHORT, 1, 6)]);
        assert_eq!(orientation(&valid), 6);
        // Not EXIF, unknown byte order, wrong magic.
        let mut order = valid.clone();
        order[6..8].copy_from_slice(b"IM");
        let mut magic = valid.clone();
        magic[8] = 43;
        let mut cases = vec![b"http://ns.adobe.com/xap/1.0/\0".to_vec(), order, magic];
        // IFD offset inside the header, past the end, or near u32::MAX.
        for offset in [4u32, 1000, u32::MAX - 1] {
            let mut bad = valid.clone();
            bad[10..14].copy_from_slice(&offset.to_le_bytes());
            cases.push(bad);
        }
        // Entry count larger than the segment holds.
        let mut count = valid.clone();
        count[14..16].copy_from_slice(&500u16.to_le_bytes());
        cases.push(count);
        // Truncated anywhere, including just before the next-IFD offset.
        for cut in [6, 9, 15, 20, valid.len() - 1] {
            cases.push(valid[..cut].to_vec());
        }
        for case in cases {
            assert_eq!(orientation(&case), 1, "{case:?}");
        }
    }

    #[test]
    fn given_minimal_app1_then_it_reads_back_and_has_the_documented_length() {
        for value in 1..=8 {
            let segment = minimal_app1(value);
            assert_eq!(&segment[..2], &[0xff, 0xe1]);
            let length = usize::from(u16::from_be_bytes([segment[2], segment[3]]));
            assert_eq!(length + 2, MINIMAL_APP1_LEN);
            assert_eq!(orientation(&segment[4..]), value);
        }
    }
}

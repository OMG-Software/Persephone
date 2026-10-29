use super::*;
use matrix_sdk::ruma::OwnedMxcUri;
use matrix_sdk_crypto::AttachmentEncryptor;
use persephone_media_protocol::{FrameHeader, read_frame};
use std::{
    collections::BTreeMap,
    io::{Cursor, Write},
    net::TcpListener,
    sync::atomic::AtomicBool,
    time::Instant,
};

fn budgets() -> Arc<ResourceBudgets> {
    Arc::new(ResourceBudgets::default())
}

fn frame(
    budgets: &ResourceBudgets,
    kind: FrameKind,
    width: u32,
    height: u32,
    body: &[u8],
) -> ReceivedFrame {
    let mut wire = FrameHeader {
        kind,
        length: body.len() as u64,
        width,
        height,
    }
    .encode()
    .to_vec();
    wire.extend_from_slice(body);
    read_frame(&mut Cursor::new(wire), budgets).unwrap()
}

/// One grey level per pixel, so a pixel is named by a single number.
fn grey(levels: &[u8]) -> Vec<u8> {
    levels.iter().flat_map(|&v| [v, v, v, 255]).collect()
}

fn levels(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| pixel[0])
        .collect()
}

/// A worker that decodes anything not ending in `bad` to a fixed size
/// and orientation, and strips by dropping `trailing` bytes.
struct FakeWorker {
    size: (u32, u32),
    orientation: u8,
    trailing: usize,
    runs: AtomicUsize,
}

impl FakeWorker {
    fn new(size: (u32, u32), orientation: u8) -> Self {
        Self {
            size,
            orientation,
            trailing: 0,
            runs: AtomicUsize::new(0),
        }
    }
}

impl ImageWorker for FakeWorker {
    fn decode(
        &self,
        encoded: &[u8],
        budgets: &ResourceBudgets,
    ) -> Result<DecodedPixels, ErrorCode> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if encoded.ends_with(b"bad") {
            return Err(ErrorCode::Rejected);
        }
        let (width, height) = self.size;
        let body = vec![7u8; (width * height * 4) as usize];
        Ok(DecodedPixels {
            frame: frame(budgets, FrameKind::RgbaPixels, width, height, &body),
            orientation: self.orientation,
        })
    }

    fn strip(&self, encoded: &[u8], budgets: &ResourceBudgets) -> Result<ReceivedFrame, ErrorCode> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let kept = &encoded[..encoded.len() - self.trailing];
        Ok(frame(budgets, FrameKind::EncodedImage, 0, 0, kept))
    }
}

fn service(worker: FakeWorker) -> MediaService {
    MediaService::new(budgets(), Arc::new(worker))
}

fn view(media: &MediaService, generation: u64, width: u32, height: u32) -> ImageView {
    let pixels = Pixels::for_tests(
        media.budgets(),
        width,
        height,
        vec![0; (width * height * 4) as usize],
    )
    .unwrap();
    media
        .keep_view(generation, &|| Some(generation), pixels)
        .unwrap()
}

/// GIVEN a 3 x 2 image with a distinct value per pixel WHEN each EXIF
/// orientation is applied THEN the shown image is the one the EXIF
/// specification describes (mirrors, rotations and their combinations).
#[test]
fn given_each_orientation_then_the_shown_pixels_follow_the_exif_rule() {
    // a b c
    // d e f
    let (a, b, c, d, e, f) = (10, 20, 30, 40, 50, 60);
    let source = grey(&[a, b, c, d, e, f]);
    for (orientation, size, shown) in [
        (1, (3, 2), vec![a, b, c, d, e, f]),
        (2, (3, 2), vec![c, b, a, f, e, d]),
        (3, (3, 2), vec![f, e, d, c, b, a]),
        (4, (3, 2), vec![d, e, f, a, b, c]),
        (5, (2, 3), vec![a, d, b, e, c, f]),
        (6, (2, 3), vec![d, a, e, b, f, c]),
        (7, (2, 3), vec![f, c, e, b, d, a]),
        (8, (2, 3), vec![c, f, b, e, a, d]),
    ] {
        assert_eq!(oriented_size(3, 2, orientation), size, "{orientation}");
        let mut out = vec![0; 24];
        orient_and_scale(&source, (3, 2), orientation, &mut out, size);
        assert_eq!(levels(&out), shown, "orientation {orientation}");
    }
}

/// GIVEN images of many sizes WHEN fitted THEN the result is within 2048 on
/// both sides, keeps the proportions to within rounding, is never larger
/// than the input and never zero.
#[test]
fn given_sizes_then_the_fit_stays_within_the_shown_square() {
    for ((width, height), expected) in [
        ((4000, 3000), (2048, 1536)),
        ((3000, 4000), (1536, 2048)),
        ((2048, 2048), (2048, 2048)),
        ((2049, 2049), (2048, 2048)),
        ((8192, 1), (2048, 1)),
        ((1, 8192), (1, 2048)),
        ((100, 50), (100, 50)),
        ((1, 1), (1, 1)),
    ] {
        assert_eq!(
            fit_within(width, height, MAX_SHOWN_SIDE),
            expected,
            "{width}x{height}"
        );
    }
}

/// GIVEN a 4 x 2 image WHEN scaled to 2 x 1 THEN each pixel is the rounded
/// average of the 2 x 2 box it covers, in every channel.
#[test]
fn given_a_downscale_then_each_pixel_averages_its_box() {
    let source = grey(&[0, 10, 100, 200, 20, 31, 255, 255]);
    let mut out = vec![0; 8];
    orient_and_scale(&source, (4, 2), 1, &mut out, (2, 1));
    // (0 + 10 + 20 + 31) / 4 = 15.25; (100 + 200 + 255 + 255) / 4 = 202.5.
    assert_eq!(out, vec![15, 15, 15, 255, 203, 203, 203, 255]);
}

/// GIVEN decoded pixels larger than the shown square, rotated WHEN prepared
/// for showing THEN they are oriented first and then fitted, the full
/// oriented size is reported, and the reservation matches the shown size.
#[test]
fn given_large_rotated_pixels_then_they_are_fitted_after_orientation() {
    let media = service(FakeWorker::new((4096, 1024), 6));
    let decoded = media.decode(b"img").unwrap();
    let (pixels, oriented) = media.shown(decoded).unwrap();
    assert_eq!(oriented, (1024, 4096));
    assert_eq!((pixels.width(), pixels.height()), (512, 2048));
    assert_eq!(
        media.budgets().class_used(BudgetClass::RetainedPixels),
        512 * 2048 * 4
    );
    assert_eq!(
        media.budgets().class_used(BudgetClass::WorkerOutput),
        0,
        "the worker's frame is released"
    );
    for orientation in [0, 9] {
        let media = service(FakeWorker::new((2, 2), orientation));
        let decoded = media.decode(b"img").unwrap();
        assert_eq!(media.shown(decoded).err(), Some(ErrorCode::Rejected));
    }
}

/// GIVEN a 1024-pixel-wide image of 600 rows WHEN its bands are asked THEN
/// each full band is 256 rows (1 MiB), the last holds the remaining 88, the
/// bytes are the image's rows in order, and a band past the end is invalid.
#[test]
fn given_an_image_then_bands_cover_it_and_the_last_is_partial() {
    let media = service(FakeWorker::new((1, 1), 1));
    let rgba: Vec<u8> = (0..1024 * 600 * 4).map(|i| (i / 4096) as u8).collect();
    let pixels = Pixels::for_tests(media.budgets(), 1024, 600, rgba.clone()).unwrap();
    let shown = media.keep_view(1, &|| Some(1), pixels).unwrap();
    assert_eq!((shown.bands, shown.band_rows), (3, 256));
    let mut joined = Vec::new();
    for index in 0..3 {
        let band = media.band(Some(1), &shown.handle, index).unwrap();
        assert_eq!(band.width, 1024);
        assert_eq!(band.rows, if index < 2 { 256 } else { 88 });
        assert_eq!(band.bytes().len(), band.rows as usize * 4096);
        assert!(band.bytes().len() as u64 <= MAX_BAND_BYTES);
        joined.extend_from_slice(band.bytes());
    }
    assert_eq!(joined, rgba);
    assert_eq!(
        media.band(Some(1), &shown.handle, 3).err(),
        Some(ErrorCode::InvalidArgument)
    );
    assert_eq!(
        media.band(Some(1), &shown.handle, u32::MAX).err(),
        Some(ErrorCode::InvalidArgument)
    );
    assert_eq!(band_rows(MAX_SHOWN_SIDE), 128);
    assert_eq!(band_rows(1), 262_144);
}

/// GIVEN handles in the wrong form WHEN used THEN they are invalid; GIVEN an
/// unknown well-formed one THEN it is not found.
#[test]
fn given_malformed_or_unknown_handles_then_they_are_refused() {
    let media = service(FakeWorker::new((1, 1), 1));
    for handle in [
        "",
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0",
        "../../../../etc/passwd/0123456789",
        "0123456789abcdef0123456789abcdeg",
    ] {
        assert_eq!(
            media.band(Some(1), handle, 0).err(),
            Some(ErrorCode::InvalidArgument),
            "{handle}"
        );
        assert_eq!(
            media.release(Some(1), handle),
            Err(ErrorCode::InvalidArgument)
        );
        assert_eq!(
            media.discard(Some(1), handle),
            Err(ErrorCode::InvalidArgument)
        );
    }
    let unknown = "0123456789abcdef0123456789abcdef";
    assert_eq!(
        media.band(Some(1), unknown, 0).err(),
        Some(ErrorCode::NotFound)
    );
    assert_eq!(media.release(Some(1), unknown), Err(ErrorCode::NotFound));
    let shown = view(&media, 1, 2, 2);
    assert!(valid_handle(&shown.handle));
    assert_ne!(
        shown.handle,
        view(&media, 1, 2, 2).handle,
        "handles are random"
    );
}

/// GIVEN a loaded image WHEN the generation changes (a lock), or it is
/// released, or everything is purged THEN its handle is not found and its
/// pixels are no longer reserved; a band taken earlier keeps its own pixels
/// until it is dropped.
#[test]
fn given_a_lock_or_release_then_handles_go_stale_and_pixels_are_freed() {
    let media = service(FakeWorker::new((1, 1), 1));
    let shown = view(&media, 1, 4, 4);
    let band = media.band(Some(1), &shown.handle, 0).unwrap();
    assert_eq!(
        media.band(Some(2), &shown.handle, 0).err(),
        Some(ErrorCode::NotFound)
    );
    assert_eq!(
        media.band(None, &shown.handle, 0).err(),
        Some(ErrorCode::NotFound)
    );
    assert_eq!(media.handles(), 0, "the stale handle was dropped");
    assert_eq!(band.bytes().len(), 64, "the band in flight is still whole");
    drop(band);
    assert_eq!(media.budgets().class_used(BudgetClass::RetainedPixels), 0);

    let shown = view(&media, 2, 4, 4);
    assert_eq!(
        media.discard(Some(2), &shown.handle),
        Err(ErrorCode::NotFound),
        "not a draft"
    );
    assert_eq!(media.release(Some(2), &shown.handle), Ok(()));
    assert_eq!(
        media.band(Some(2), &shown.handle, 0).err(),
        Some(ErrorCode::NotFound)
    );
    assert_eq!(
        media.release(Some(2), &shown.handle),
        Err(ErrorCode::NotFound)
    );

    let shown = view(&media, 2, 4, 4);
    media.purge();
    assert_eq!(
        media.band(Some(2), &shown.handle, 0).err(),
        Some(ErrorCode::NotFound)
    );
    assert_eq!(media.budgets().class_used(BudgetClass::RetainedPixels), 0);

    // A result finished after the lock is not kept, and frees its pixels.
    let pixels = Pixels::for_tests(media.budgets(), 4, 4, vec![0; 64]).unwrap();
    assert_eq!(
        media.keep_view(2, &|| Some(3), pixels).err(),
        Some(ErrorCode::Locked)
    );
    let pixels = Pixels::for_tests(media.budgets(), 4, 4, vec![0; 64]).unwrap();
    assert_eq!(
        media.keep_view(2, &|| None, pixels).err(),
        Some(ErrorCode::Locked)
    );
    assert_eq!(media.budgets().class_used(BudgetClass::RetainedPixels), 0);
}

/// GIVEN four 16 MiB images (the whole budget) WHEN a fifth is prepared
/// THEN the least recently used one is evicted, not one used since; WHEN
/// every image is still being sent as a band THEN eviction cannot free room
/// and the fifth is refused.
#[test]
fn given_a_full_pixel_budget_then_the_least_recently_used_image_makes_room() {
    let media = service(FakeWorker::new((2048, 2048), 1));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let (pixels, _) = media.shown(media.decode(b"img").unwrap()).unwrap();
        handles.push(media.keep_view(1, &|| Some(1), pixels).unwrap().handle);
    }
    // Use the first, so the second is now the least recently used.
    drop(media.band(Some(1), &handles[0], 0).unwrap());
    let (pixels, _) = media.shown(media.decode(b"img").unwrap()).unwrap();
    handles.push(media.keep_view(1, &|| Some(1), pixels).unwrap().handle);
    assert!(media.band(Some(1), &handles[0], 0).is_ok());
    assert_eq!(
        media.band(Some(1), &handles[1], 0).err(),
        Some(ErrorCode::NotFound)
    );
    assert_eq!(media.handles(), 4);
    assert!(media.budgets().class_used(BudgetClass::RetainedPixels) <= RETAINED_LIMIT);

    let held: Vec<Band> = handles
        .iter()
        .filter_map(|handle| media.band(Some(1), handle, 0).ok())
        .collect();
    assert_eq!(held.len(), 4);
    let decoded = media.decode(b"img").unwrap();
    assert_eq!(media.shown(decoded).err(), Some(ErrorCode::LimitExceeded));
    drop(held);
    assert_eq!(media.budgets().class_used(BudgetClass::RetainedPixels), 0);
}

const RETAINED_LIMIT: u64 = persephone_media_protocol::RETAINED_PIXELS_LIMIT;

/// GIVEN many small images WHEN more than `MAX_HANDLES` are kept THEN the
/// oldest are evicted and the count stays at the limit.
#[test]
fn given_many_small_images_then_the_handle_count_is_bounded() {
    let media = service(FakeWorker::new((1, 1), 1));
    let first = view(&media, 1, 1, 1).handle;
    for _ in 0..MAX_HANDLES {
        view(&media, 1, 1, 1);
    }
    assert_eq!(media.handles(), MAX_HANDLES);
    assert_eq!(
        media.band(Some(1), &first, 0).err(),
        Some(ErrorCode::NotFound)
    );
}

/// GIVEN the admission limits WHEN more loads or picks start THEN the extra
/// ones are refused at once, and a finished one makes room.
#[test]
fn given_admission_limits_then_extra_work_is_refused_not_queued() {
    let media = service(FakeWorker::new((1, 1), 1));
    let first = media.begin_load().unwrap();
    let second = media.begin_load().unwrap();
    assert_eq!(media.begin_load().err(), Some(ErrorCode::LimitExceeded));
    drop(first);
    let third = media.begin_load().unwrap();
    drop((second, third));
    let pick = media.begin_pick().unwrap();
    assert_eq!(media.begin_pick().err(), Some(ErrorCode::LimitExceeded));
    drop(pick);
    assert!(media.begin_pick().is_ok());
}

fn encoded(budgets: &ResourceBudgets, bytes: &[u8]) -> Encoded {
    let mut encoded = Encoded::reserve(budgets, bytes.len() as u64).unwrap();
    encoded.extend(bytes).unwrap();
    encoded
}

const JPEG: &[u8] = b"\xff\xd8\xff\xe0 jpeg body \xff\xd9";

/// GIVEN an image to send WHEN it is copied for the SDK THEN the upload
/// stays in `EncodedInFlight` and the copy, with the SDK's own copies, is
/// reserved in `OutgoingSend`: [`SENT_COPIES`] times the image in all, all
/// released when the send ends.
#[test]
fn given_an_image_copied_for_send_then_the_sdk_copies_are_reserved_apart() {
    let budgets = budgets();
    let upload = ImageUpload::for_tests(&budgets, JPEG);
    let (copy, permit) = upload.copy_for_send().unwrap();
    assert_eq!(copy, JPEG);
    let length = JPEG.len() as u64;
    assert_eq!(budgets.class_used(BudgetClass::EncodedInFlight), length);
    assert_eq!(
        budgets.class_used(BudgetClass::OutgoingSend),
        (SENT_COPIES - 1) * length
    );
    drop((copy, permit));
    assert_eq!(budgets.class_used(BudgetClass::OutgoingSend), 0);
}

/// GIVEN an image of the largest size sent, while a received image of the
/// largest size downloads WHEN it is copied for the SDK THEN every copy
/// fits; a second send at the same time does not.
#[test]
fn given_the_largest_image_then_its_send_fits_beside_downloads() {
    let budgets = budgets();
    let largest = vec![0xff; MAX_SENT_BYTES as usize];
    let upload = ImageUpload::for_tests(&budgets, &largest);
    let downloading = Encoded::reserve(&budgets, MAX_ENCODED_BYTES).unwrap();
    let (copy, _permit) = upload.copy_for_send().unwrap();
    assert_eq!(copy.len() as u64, MAX_SENT_BYTES);
    assert_eq!(upload.copy_for_send().err(), Some(ErrorCode::LimitExceeded));
    drop(downloading);
}

/// GIVEN a picked JPEG WHEN the draft is prepared THEN the worker strips it
/// and decodes the stripped copy (two runs), the preview has the oriented,
/// fitted size, and the draft sends exactly the stripped bytes with the
/// generic name; it can be taken only for its own room, put back, and
/// discarded, which frees its bytes and pixels.
#[test]
fn given_a_picked_file_then_its_stripped_copy_becomes_one_draft() {
    let mut worker = FakeWorker::new((30, 20), 8);
    worker.trailing = 4;
    let worker = Arc::new(worker);
    let media = MediaService::new(budgets(), worker.clone());
    let original = encoded(media.budgets(), &[JPEG, b"junk"].concat());
    let draft = media
        .prepare_draft(1, &|| Some(1), "!r:s", original)
        .unwrap();
    assert_eq!(worker.runs.load(Ordering::SeqCst), 2);
    let ImagePick::Draft {
        preview,
        size,
        mime,
    } = draft
    else {
        panic!("a draft");
    };
    assert_eq!((preview.width, preview.height), (20, 30));
    assert_eq!((size as usize, mime), (JPEG.len(), ImageMime::Jpeg));
    assert!(
        media.band(Some(1), &preview.handle, 0).is_ok(),
        "the preview has bands"
    );
    assert_eq!(
        media.begin_pick().err(),
        Some(ErrorCode::LimitExceeded),
        "one draft at a time"
    );
    assert_eq!(
        media.release(Some(1), &preview.handle),
        Err(ErrorCode::NotFound)
    );

    assert_eq!(
        media.take_draft(Some(1), &preview.handle, "!other:s").err(),
        Some(ErrorCode::NotFound)
    );
    let taken = media.take_draft(Some(1), &preview.handle, "!r:s").unwrap();
    let upload = taken.upload();
    assert_eq!(upload.bytes(), JPEG);
    assert_eq!(
        (upload.file_name(), upload.mime_type()),
        ("image.jpg", "image/jpeg")
    );
    assert_eq!(upload.size(), (20, 30));
    drop(upload);
    media.restore_draft(&|| Some(1), taken);
    assert_eq!(media.discard(Some(1), &preview.handle), Ok(()));
    assert_eq!(media.handles(), 0);
    assert_eq!(media.budgets().class_used(BudgetClass::EncodedInFlight), 0);
    assert_eq!(media.budgets().class_used(BudgetClass::RetainedPixels), 0);
}

/// GIVEN a strip that returns something other than PNG or JPEG, a decode the
/// worker refuses, or a lock during the work WHEN a draft is prepared THEN
/// nothing is kept and every reservation is released.
#[test]
fn given_a_failed_or_stale_pick_then_no_draft_is_kept() {
    for (bytes, current, expected) in [
        (
            &b"GIF89a not an accepted type"[..],
            Some(1),
            ErrorCode::Rejected,
        ),
        (
            b"\xff\xd8\xff the decoder refuses this: bad",
            Some(1),
            ErrorCode::Rejected,
        ),
        (JPEG, Some(2), ErrorCode::Locked),
        (JPEG, None, ErrorCode::Locked),
    ] {
        let media = service(FakeWorker::new((4, 4), 1));
        let original = encoded(media.budgets(), bytes);
        assert_eq!(
            media.prepare_draft(1, &|| current, "!r:s", original).err(),
            Some(expected)
        );
        assert_eq!(media.handles(), 0);
        assert_eq!(media.budgets().aggregate_used(), 0);
    }
    // A draft taken for sending is not put back after a lock.
    let media = service(FakeWorker::new((4, 4), 1));
    let original = encoded(media.budgets(), JPEG);
    let Ok(ImagePick::Draft { preview, .. }) =
        media.prepare_draft(1, &|| Some(1), "!r:s", original)
    else {
        panic!("a draft");
    };
    let taken = media.take_draft(Some(1), &preview.handle, "!r:s").unwrap();
    media.restore_draft(&|| Some(2), taken);
    assert_eq!(media.handles(), 0);
    assert_eq!(media.budgets().aggregate_used(), 0);
}

/// GIVEN a reservation WHEN more is appended than reserved, or a source is
/// shorter or longer than its checked size THEN it is refused; the buffer
/// never grows past the reservation.
#[test]
fn given_a_reservation_then_it_is_never_exceeded() {
    let budgets = budgets();
    assert_eq!(
        Encoded::reserve(&budgets, 0).err(),
        Some(ErrorCode::Rejected)
    );
    assert_eq!(
        Encoded::reserve(&budgets, MAX_ENCODED_BYTES + 1).err(),
        Some(ErrorCode::Rejected)
    );
    let mut small = Encoded::reserve(&budgets, 4).unwrap();
    small.extend(b"abc").unwrap();
    assert_eq!(small.extend(b"de"), Err(ErrorCode::Rejected));
    assert_eq!(small.bytes(), b"abc");
    // The file grew or shrank between the size check and the read.
    for source in [&b"abcde"[..], b"abc"] {
        let mut exact = Encoded::reserve(&budgets, 4).unwrap();
        assert_eq!(
            exact.fill_exactly(&mut Cursor::new(source)),
            Err(ErrorCode::Rejected)
        );
    }
    let mut exact = Encoded::reserve(&budgets, 4).unwrap();
    exact.fill_exactly(&mut Cursor::new(b"abcd")).unwrap();
    assert_eq!(exact.bytes(), b"abcd");
    // Two 20 MiB transfers fill the encoded budget; a third is refused.
    let first = Encoded::reserve(&budgets, MAX_ENCODED_BYTES).unwrap();
    drop((small, exact));
    let second = Encoded::reserve(&budgets, MAX_ENCODED_BYTES).unwrap();
    assert_eq!(
        Encoded::reserve(&budgets, 1).err(),
        Some(ErrorCode::LimitExceeded)
    );
    drop((first, second));
}

fn encrypt(plain: &[u8]) -> (Vec<u8>, MediaEncryptionInfo) {
    let mut cursor = Cursor::new(plain.to_vec());
    let mut encryptor = AttachmentEncryptor::new(&mut cursor);
    let mut encrypted = Vec::new();
    encryptor.read_to_end(&mut encrypted).unwrap();
    (encrypted, encryptor.finish())
}

/// The SDK's encryption info is not `Clone`.
fn copy(info: &MediaEncryptionInfo) -> MediaEncryptionInfo {
    MediaEncryptionInfo {
        encryption_info: info.encryption_info.clone(),
        hashes: info.hashes.clone(),
    }
}

/// GIVEN a file encrypted with the SDK's attachment encryptor, larger than
/// one decryption step WHEN decrypted in place THEN the plain file comes
/// back; GIVEN one changed byte THEN the hash check refuses it.
#[test]
fn given_an_encrypted_attachment_then_it_decrypts_in_place_and_tampering_is_refused() {
    let budgets = budgets();
    let plain: Vec<u8> = (0..200_000u32).map(|i| (i * 31 % 251) as u8).collect();
    let (encrypted, info) = encrypt(&plain);
    assert_ne!(encrypted, plain);
    let mut file = encoded(&budgets, &encrypted);
    decrypt_in_place(&mut file, copy(&info)).unwrap();
    assert_eq!(file.bytes(), &plain[..]);

    let mut tampered = encrypted.clone();
    tampered[150_000] ^= 1;
    let mut file = encoded(&budgets, &tampered);
    assert_eq!(
        decrypt_in_place(&mut file, copy(&info)),
        Err(ErrorCode::Rejected)
    );

    let mut truncated = encoded(&budgets, &encrypted[..encrypted.len() - 1]);
    assert_eq!(
        decrypt_in_place(&mut truncated, copy(&info)),
        Err(ErrorCode::Rejected)
    );

    let (_, other) = encrypt(&plain);
    let mut file = encoded(&budgets, &encrypted);
    assert_eq!(
        decrypt_in_place(&mut file, other),
        Err(ErrorCode::Rejected),
        "another key"
    );
}

fn versions(versions: &[&str]) -> SupportedVersions {
    let versions: Vec<String> = versions.iter().map(|v| (*v).to_owned()).collect();
    SupportedVersions::from_parts(&versions, &BTreeMap::new())
}

/// GIVEN homeservers of different versions WHEN a media URL is built THEN
/// the authenticated endpoint (with the token) is used where advertised,
/// the legacy one (without it) otherwise, and none for a server advertising
/// neither; a media ID outside the Matrix grammar is refused.
#[test]
fn given_server_versions_then_the_media_endpoint_follows_the_sdk_choice() {
    let uri = OwnedMxcUri::from("mxc://example.org/AbC_12-x");
    let base = "https://matrix.example.org/";
    assert_eq!(
        media_url(base, &uri, &versions(&["v1.1", "v1.11"])),
        Ok((
            "https://matrix.example.org/_matrix/client/v1/media/download/example.org/AbC_12-x"
                .to_owned(),
            true
        ))
    );
    assert_eq!(
        media_url(base, &uri, &versions(&["v1.1", "v1.10"])),
        Ok((
            "https://matrix.example.org/_matrix/media/v3/download/example.org/AbC_12-x".to_owned(),
            false
        ))
    );
    assert_eq!(
        media_url(base, &uri, &versions(&[])),
        Err(ErrorCode::Unavailable)
    );
    for bad in [
        "mxc://example.org/a%2Fb",
        "mxc://example.org/..",
        "mxc://example.org/a/b",
        "mxc://example.org/",
        "https://example.org/x",
    ] {
        assert_eq!(
            media_url(base, &OwnedMxcUri::from(bad), &versions(&["v1.11"])),
            Err(ErrorCode::Rejected),
            "{bad}"
        );
    }
}

/// A one-connection HTTP server on loopback. `respond` writes the response
/// and returns how many body bytes it managed to write.
fn server(
    respond: impl FnOnce(&mut std::net::TcpStream) -> u64 + Send + 'static,
) -> (String, std::thread::JoinHandle<(String, u64)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/media", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).unwrap_or(0) == 0 {
                break;
            }
            head.push(byte[0]);
        }
        let written = respond(&mut stream);
        (String::from_utf8_lossy(&head).into_owned(), written)
    });
    (url, thread)
}

fn fetch_with(
    url: &str,
    token: Option<&str>,
    cap: u64,
    current: &(dyn Fn() -> bool + Sync),
) -> (Result<Encoded, ErrorCode>, Arc<ResourceBudgets>) {
    let budgets = budgets();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let http = crate::homeserver::http_client(None).unwrap();
    let result = runtime.block_on(download(&http, url, token, cap, &budgets, current));
    (result, budgets)
}

fn write(stream: &mut std::net::TcpStream, bytes: &[u8]) -> bool {
    stream.write_all(bytes).and_then(|_| stream.flush()).is_ok()
}

/// The start of a download's answer with the headers Matrix v1.19 requires:
/// the content type and (since v1.12) the disposition.
const FILE: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment\r\n";

/// An error answer with `status` and the body Matrix v1.19 gives every
/// error: `errcode` and `error`.
fn matrix_error(status: &str, errcode: &str, headers: &str) -> Vec<u8> {
    let body = format!(r#"{{"errcode":"{errcode}","error":"Synthetic failure"}}"#);
    format!(
        "HTTP/1.1 {status}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// GIVEN a homeserver answering with the file WHEN it is downloaded THEN the
/// bytes arrive whole, the request carries the bearer token, and only the
/// declared length was reserved.
#[test]
fn given_a_download_within_the_cap_then_it_arrives_whole_with_the_token() {
    let (url, served) = server(|stream| {
        write(
            stream,
            format!("{FILE}Content-Length: 5\r\nConnection: close\r\n\r\nhello").as_bytes(),
        );
        5
    });
    let (result, budgets) = fetch_with(&url, Some("syt_token"), 1024, &|| true);
    let file = result.unwrap();
    assert_eq!(file.bytes(), b"hello");
    assert_eq!(budgets.class_used(BudgetClass::EncodedInFlight), 5);
    let (head, _) = served.join().unwrap();
    assert!(
        head.to_ascii_lowercase()
            .contains("authorization: bearer syt_token"),
        "{head}"
    );
    // The legacy endpoint gets no token.
    let (url, served) = server(|stream| {
        write(
            stream,
            format!("{FILE}Content-Length: 2\r\nConnection: close\r\n\r\nok").as_bytes(),
        );
        2
    });
    assert!(fetch_with(&url, None, 1024, &|| true).0.is_ok());
    let (head, _) = served.join().unwrap();
    assert!(
        !head.to_ascii_lowercase().contains("authorization"),
        "{head}"
    );
}

/// GIVEN a declared length over the cap WHEN downloaded THEN it is refused
/// from the headers alone: no body was sent, and nothing is reserved.
#[test]
fn given_a_declared_length_over_the_cap_then_the_body_is_never_read() {
    let (url, served) = server(|stream| {
        // Announce 21 MiB but send no body; the client must not wait for it.
        write(
            stream,
            format!("{FILE}Content-Length: {}\r\n\r\n", MAX_ENCODED_BYTES + 1).as_bytes(),
        );
        0
    });
    let (result, budgets) = fetch_with(&url, None, MAX_ENCODED_BYTES, &|| true);
    assert_eq!(result.err(), Some(ErrorCode::Rejected));
    assert_eq!(budgets.aggregate_used(), 0);
    served.join().unwrap();
}

/// GIVEN a body without a declared length that keeps coming WHEN downloaded
/// THEN it is refused once it passes the cap, while the server still had far
/// more to send: the whole body was never received or allocated.
#[test]
fn given_an_unbounded_body_then_the_download_stops_at_the_cap() {
    const CAP: u64 = MIB;
    const OFFERED: u64 = 64 * MIB;
    let (url, served) = server(|stream| {
        if !write(
            stream,
            format!("{FILE}Transfer-Encoding: chunked\r\n\r\n").as_bytes(),
        ) {
            return 0;
        }
        let chunk = vec![b'x'; 64 * 1024];
        let mut sent = 0;
        while sent < OFFERED {
            let mut wire = format!("{:x}\r\n", chunk.len()).into_bytes();
            wire.extend_from_slice(&chunk);
            wire.extend_from_slice(b"\r\n");
            if !write(stream, &wire) {
                break;
            }
            sent += chunk.len() as u64;
        }
        sent
    });
    let (result, budgets) = fetch_with(&url, None, CAP, &|| true);
    assert_eq!(result.err(), Some(ErrorCode::Rejected));
    assert_eq!(budgets.aggregate_used(), 0);
    let (_, sent) = served.join().unwrap();
    assert!(sent < OFFERED / 2, "the server sent {sent} bytes");
}

/// GIVEN a body cut short, a missing file, a server error, an empty file, a
/// refusal (403, 451), a rate limit (429), a redirect or a lock during the
/// download WHEN downloaded THEN each is refused with its code and nothing
/// stays reserved.
#[test]
fn given_failed_downloads_then_each_is_refused_with_its_code() {
    let cases: Vec<(Vec<u8>, ErrorCode)> = vec![
        (
            format!("{FILE}Content-Length: 100\r\nConnection: close\r\n\r\nonly part").into_bytes(),
            ErrorCode::Unavailable,
        ),
        (
            matrix_error("404 Not Found", "M_NOT_FOUND", ""),
            ErrorCode::NotFound,
        ),
        (
            matrix_error("500 Internal Server Error", "M_UNKNOWN", ""),
            ErrorCode::Unavailable,
        ),
        (
            format!("{FILE}Content-Length: 0\r\nConnection: close\r\n\r\n").into_bytes(),
            ErrorCode::Rejected,
        ),
        // Matrix allows only 307 and 308 here; this one is not followed.
        (
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ErrorCode::Unavailable,
        ),
        // The server refuses this file: asking again gives the same answer.
        (
            matrix_error("403 Forbidden", "M_FORBIDDEN", ""),
            ErrorCode::Rejected,
        ),
        (
            matrix_error("451 Unavailable For Legal Reasons", "M_FORBIDDEN", ""),
            ErrorCode::Rejected,
        ),
        // Rate limits and server faults pass; the page may try again.
        (
            matrix_error("429 Too Many Requests", "M_LIMIT_EXCEEDED", "Retry-After: 1\r\n"),
            ErrorCode::Unavailable,
        ),
        (
            matrix_error("503 Service Unavailable", "M_UNKNOWN", ""),
            ErrorCode::Unavailable,
        ),
    ];
    for (response, expected) in cases {
        let shown = String::from_utf8_lossy(&response).into_owned();
        let (url, served) = server(move |stream| {
            write(stream, &response);
            0
        });
        let (result, budgets) = fetch_with(&url, None, 1024, &|| true);
        assert_eq!(result.err(), Some(expected), "{shown}");
        assert_eq!(budgets.aggregate_used(), 0);
        served.join().unwrap();
    }
    let (url, served) = server(|stream| {
        write(
            stream,
            format!("{FILE}Content-Length: 4\r\nConnection: close\r\n\r\ndata").as_bytes(),
        );
        4
    });
    let locked = AtomicBool::new(true);
    let (result, _) = fetch_with(&url, None, 1024, &|| !locked.load(Ordering::SeqCst));
    assert_eq!(result.err(), Some(ErrorCode::Locked));
    served.join().unwrap();
}

/// Keeps a connection open without sending anything until the client closes
/// it, or for at most 10 s, so the server thread always ends.
fn stall_until_closed(stream: &mut std::net::TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut byte = [0u8; 1];
    let _ = stream.read(&mut byte);
}

/// Locks a download 0.5 s after it starts, and checks that it stops with
/// `locked` long before `DOWNLOAD_TIMEOUT`, with nothing left reserved.
fn assert_locked_promptly(url: &str) {
    let start = Instant::now();
    let (result, budgets) = fetch_with(url, None, 1024, &|| {
        start.elapsed() < Duration::from_millis(500)
    });
    let elapsed = start.elapsed();
    assert_eq!(result.err(), Some(ErrorCode::Locked));
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    assert_eq!(budgets.aggregate_used(), 0);
}

/// GIVEN a server that sends the headers and then stalls WHEN a lock begins
/// during the download THEN the download stops with `locked` within a few
/// polls, without waiting for a chunk or the 60 s timeout.
#[test]
fn given_a_stalled_body_when_a_lock_begins_then_the_download_stops_promptly() {
    let (url, served) = server(|stream| {
        write(
            stream,
            format!("{FILE}Content-Length: 4\r\nConnection: close\r\n\r\n").as_bytes(),
        );
        stall_until_closed(stream);
        0
    });
    assert_locked_promptly(&url);
    served.join().unwrap();
}

/// GIVEN a server that accepts the request but never answers WHEN a lock
/// begins THEN the download stops with `locked` within a few polls, without
/// waiting for headers or the 60 s timeout.
#[test]
fn given_no_response_headers_when_a_lock_begins_then_the_download_stops_promptly() {
    let (url, served) = server(|stream| {
        stall_until_closed(stream);
        0
    });
    assert_locked_promptly(&url);
    served.join().unwrap();
}

#[test]
fn given_file_signatures_then_only_png_and_jpeg_are_recognised() {
    assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some(ImageMime::Png));
    assert_eq!(sniff(b"\xff\xd8\xff\xe1"), Some(ImageMime::Jpeg));
    for other in [
        &b"GIF89a"[..],
        b"\x89PNG\r\n",
        b"\xff\xd8",
        b"",
        b"RIFF....WEBP",
    ] {
        assert_eq!(sniff(other), None);
    }
}

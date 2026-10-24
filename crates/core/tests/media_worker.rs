//! The core's image pipeline with the real contained media worker. `python
//! tools/dev.py rust` builds the worker binary these tests launch.
#![cfg(windows)]

use persephone_contracts::{ImageMime, ImagePick};
use persephone_core::{
    media::{MediaService, contained::ContainedWorker},
    pick,
};
use persephone_core_protocol::ErrorCode;
use persephone_media_broker::AppContainerProfile;
use persephone_media_protocol::{BudgetClass, ResourceBudgets};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

fn worker_exe() -> PathBuf {
    // Tests run from target/<profile>/deps; the worker binary is one level up.
    let path = std::env::current_exe()
        .unwrap()
        .parent()
        .and_then(|deps| deps.parent())
        .unwrap()
        .join("persephone-media-worker.exe");
    assert!(
        path.is_file(),
        "worker binary missing at {}; build persephone-media-worker first",
        path.display()
    );
    path
}

fn media() -> MediaService {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "Persephone.MediaWorker.CoreTest.{}.{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let profile = AppContainerProfile::temporary(&name).unwrap();
    MediaService::new(
        Arc::new(ResourceBudgets::default()),
        Arc::new(ContainedWorker::with_profile(worker_exe(), profile)),
    )
}

/// A synthetic baseline JPEG (16 x 8) with an EXIF segment holding
/// `orientation` and a comment inserted after SOI, as in the broker tests.
fn jpeg_with_metadata(orientation: u16) -> Vec<u8> {
    let fixture = include_bytes!("../../media-sandbox/tests/fixtures/solid-16x8-baseline.jpg");
    let mut exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
    exif.extend_from_slice(&orientation.to_le_bytes());
    exif.extend_from_slice(&[0; 6]);
    let mut out = fixture[..2].to_vec();
    out.extend_from_slice(&[0xff, 0xe1]);
    out.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&exif);
    out.extend_from_slice(&[0xff, 0xfe, 0, 8]);
    out.extend_from_slice(b"secret");
    out.extend_from_slice(&fixture[2..]);
    out
}

/// GIVEN a picked JPEG with a comment and orientation 6 WHEN the core makes
/// its draft THEN the worker's stripped copy is what will be sent (no
/// comment, still a JPEG, no longer than the file), the preview is decoded
/// from it with the rotation applied (16 x 8 becomes 8 x 16), and the file
/// on disk is unchanged.
#[test]
fn given_a_picked_jpeg_then_the_draft_is_the_stripped_and_rotated_copy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("photo.jpg");
    let original = jpeg_with_metadata(6);
    std::fs::write(&path, &original).unwrap();
    let media = media();
    let read = pick::read_image_file(&path, media.budgets()).unwrap();
    let draft = media
        .prepare_draft(1, &|| Some(1), "!r:example.org", read)
        .unwrap();
    let ImagePick::Draft {
        preview,
        size,
        mime,
    } = draft
    else {
        panic!("a draft");
    };
    assert_eq!((preview.width, preview.height), (8, 16));
    assert_eq!(mime, ImageMime::Jpeg);
    assert!(size as usize <= original.len());
    let band = media.band(Some(1), &preview.handle, 0).unwrap();
    assert_eq!((band.width, band.rows), (8, 16));
    drop(band);
    let taken = media
        .take_draft(Some(1), &preview.handle, "!r:example.org")
        .unwrap();
    let upload = taken.upload();
    assert_eq!(upload.len(), size as usize);
    assert!(upload.bytes().starts_with(&[0xff, 0xd8]));
    assert!(
        !upload.bytes().windows(6).any(|w| w == b"secret"),
        "the comment was stripped"
    );
    assert_eq!(upload.size(), (8, 16), "the event carries the shown size");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original,
        "the file is unchanged"
    );
}

/// GIVEN a received JPEG WHEN decoded and prepared THEN it is shown at its
/// size; GIVEN bytes that are not an image or a truncated JPEG THEN the
/// worker refuses them, nothing is shown and nothing stays reserved.
#[test]
fn given_received_bytes_then_only_valid_images_are_shown() {
    let media = media();
    let decoded = media.decode(&jpeg_with_metadata(1)).unwrap();
    let (pixels, oriented) = media.shown(decoded).unwrap();
    assert_eq!(oriented, (16, 8));
    assert_eq!((pixels.width(), pixels.height()), (16, 8));
    assert_eq!(pixels.rgba().len(), 16 * 8 * 4);
    drop(pixels);
    let jpeg = jpeg_with_metadata(1);
    for hostile in [
        b"<svg onload=alert(1)>".to_vec(),
        jpeg[..jpeg.len() / 2].to_vec(),
        vec![0xff, 0xd8, 0xff],
    ] {
        assert_eq!(media.decode(&hostile).err(), Some(ErrorCode::Rejected));
    }
    assert_eq!(media.budgets().aggregate_used(), 0);
    assert_eq!(media.budgets().class_used(BudgetClass::WorkerOutput), 0);
}

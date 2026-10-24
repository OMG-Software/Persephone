//! End-to-end runs of the real media worker under the broker's containment.
//! `cargo test --workspace` builds the worker binary these tests launch.
#![cfg(windows)]

use persephone_media_broker::{
    AppContainerProfile, Mitigations, RunError, WORKER_EXIT_CONTAINMENT, WorkerPolicy, run_decode,
    run_strip,
};
use persephone_media_protocol::{FrameKind, ResourceBudgets, WorkerFailure};
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

/// Self-check bits the worker reports (crates/media-worker/src/containment.rs).
const MISSING_APPCONTAINER: u32 = 1 << 0;
const MISSING_JOB: u32 = 1 << 1;
const MISSING_DYNAMIC_CODE_BLOCK: u32 = 1 << 2;
const MISSING_WIN32K_LOCKDOWN: u32 = 1 << 3;

fn worker_exe() -> PathBuf {
    // tests run from target/<profile>/deps; the worker binary is one level up.
    let path = std::env::current_exe()
        .unwrap()
        .parent()
        .and_then(|deps| deps.parent())
        .unwrap()
        .join("persephone-media-worker.exe");
    assert!(
        path.is_file(),
        "worker binary missing at {}; run `cargo test --workspace` or build persephone-media-worker",
        path.display()
    );
    path
}

fn profile() -> AppContainerProfile {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "Persephone.MediaWorker.Test.{}.{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    AppContainerProfile::temporary(&name).unwrap()
}

fn png(width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
    let rgba: Vec<u8> = (0..width * height * 4)
        .map(|i| (i * 7 % 251) as u8)
        .collect();
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&rgba)
        .unwrap();
    (out, rgba)
}

/// A synthetic baseline JPEG (16 x 8, Pillow; see the sandbox fixtures) with
/// an EXIF segment holding `orientation` and a comment inserted after SOI.
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

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn given_png_when_decoded_by_contained_worker_then_pixels_match_within_limits() {
    let (encoded, rgba) = png(64, 32);
    let policy = WorkerPolicy::default();
    let run = run_decode(
        &profile(),
        &worker_exe(),
        &encoded,
        &policy,
        &ResourceBudgets::default(),
    )
    .unwrap();
    let image = match run.outcome {
        Ok(image) => image,
        Err(error) => panic!("decode failed: {error:?}; stats {:?}", run.stats),
    };
    assert_eq!(image.orientation, 1);
    let frame = image.pixels;
    assert_eq!(frame.header().kind, FrameKind::RgbaPixels);
    assert_eq!((frame.header().width, frame.header().height), (64, 32));
    assert_eq!(frame.body(), &rgba[..]);
    assert_eq!(run.stats.exit_code, 0);
    assert!(run.stats.peak_process_memory > 0);
    assert!(run.stats.peak_process_memory <= policy.memory_limit_bytes);
}

#[test]
fn given_jpeg_with_exif_when_decoded_by_contained_worker_then_orientation_is_returned() {
    let run = run_decode(
        &profile(),
        &worker_exe(),
        &jpeg_with_metadata(6),
        &WorkerPolicy::default(),
        &ResourceBudgets::default(),
    )
    .unwrap();
    let image = run.outcome.unwrap_or_else(|error| panic!("{error:?}"));
    assert_eq!(image.orientation, 6);
    // Not rotated: the decoded frame keeps the stored 16 x 8 layout.
    let header = image.pixels.header();
    assert_eq!((header.width, header.height), (16, 8));
}

#[test]
fn given_jpeg_with_metadata_when_stripped_by_contained_worker_then_metadata_is_gone_and_pixels_hold()
 {
    let input = jpeg_with_metadata(6);
    let budgets = ResourceBudgets::default();
    let run = run_strip(
        &profile(),
        &worker_exe(),
        &input,
        &WorkerPolicy::default(),
        &budgets,
    )
    .unwrap();
    assert_eq!(run.stats.exit_code, 0);
    let stripped = run
        .outcome
        .unwrap_or_else(|error| panic!("strip failed: {error:?}; stats {:?}", run.stats));
    assert_eq!(stripped.header().kind, FrameKind::EncodedImage);
    assert!(
        budgets.aggregate_used() > 0,
        "the reply keeps its reservation"
    );
    let stripped = stripped.body();
    assert!(stripped.len() < input.len());
    assert!(!contains(stripped, b"secret"));
    assert!(!contains(stripped, b"JFIF"));

    let decode = |encoded: &[u8]| {
        run_decode(
            &profile(),
            &worker_exe(),
            encoded,
            &WorkerPolicy::default(),
            &ResourceBudgets::default(),
        )
        .unwrap()
        .outcome
        .unwrap_or_else(|error| panic!("{error:?}"))
    };
    let (original, copy) = (decode(&input), decode(stripped));
    assert_eq!(copy.orientation, 6);
    assert_eq!(copy.pixels.body(), original.pixels.body());
}

#[test]
fn given_png_with_text_chunk_when_stripped_by_contained_worker_then_clean_png_is_returned() {
    let (clean, _) = png(8, 4);
    let mut text = 14u32.to_be_bytes().to_vec();
    text.extend_from_slice(b"tEXtComment\0secret");
    text.extend_from_slice(&crc32(&text[4..]).to_be_bytes());
    let mut input = clean[..33].to_vec();
    input.extend_from_slice(&text);
    input.extend_from_slice(&clean[33..]);
    let run = run_strip(
        &profile(),
        &worker_exe(),
        &input,
        &WorkerPolicy::default(),
        &ResourceBudgets::default(),
    )
    .unwrap();
    let stripped = run.outcome.unwrap_or_else(|error| panic!("{error:?}"));
    assert_eq!(stripped.body(), &clean[..]);
}

#[test]
fn given_unsupported_or_truncated_image_when_stripped_then_contained_worker_reports_guest_status() {
    let input = jpeg_with_metadata(3);
    for (encoded, status) in [(&b"GIF89a...."[..], 1), (&input[..input.len() - 2], 2)] {
        let run = run_strip(
            &profile(),
            &worker_exe(),
            encoded,
            &WorkerPolicy::default(),
            &ResourceBudgets::default(),
        )
        .unwrap();
        match run.outcome {
            Err(RunError::Worker(WorkerFailure::GuestRejected(actual))) => {
                assert_eq!(actual, status)
            }
            other => panic!("expected guest status {status}, got {:?}", other.err()),
        }
        assert_eq!(run.stats.exit_code, 1);
    }
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

/// The worker must refuse when launched without the broker's containment.
#[test]
fn given_worker_started_without_containment_then_it_refuses_before_reading_input() {
    let output = Command::new(worker_exe())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(WORKER_EXIT_CONTAINMENT as i32));
    let reply = &output.stdout;
    assert_eq!(reply.len(), 17 + 6, "one control frame and nothing else");
    assert_eq!(reply[0], FrameKind::Control as u8);
    let Some(WorkerFailure::ContainmentCheck(mask)) = WorkerFailure::decode(&reply[17..]) else {
        panic!("expected a containment failure, got {reply:?}");
    };
    for bit in [
        MISSING_APPCONTAINER,
        MISSING_DYNAMIC_CODE_BLOCK,
        MISSING_WIN32K_LOCKDOWN,
    ] {
        assert_eq!(mask & bit, bit, "mask {mask:#b}");
    }
    // The test process may itself run inside a Job (CI, terminals), so the
    // Job bit is not asserted here; the contained runs prove it is satisfied.
    let _ = MISSING_JOB;
}

/// Positive controls: dropping either mitigation must make the self-check
/// fail, which shows the check observes the real process policy.
#[test]
fn given_a_missing_mitigation_then_contained_worker_refuses_with_that_bit() {
    let (encoded, _) = png(4, 4);
    for (mitigations, expected) in [
        (
            Mitigations {
                prohibit_dynamic_code: false,
                ..Mitigations::default()
            },
            MISSING_DYNAMIC_CODE_BLOCK,
        ),
        (
            Mitigations {
                disable_win32k: false,
                ..Mitigations::default()
            },
            MISSING_WIN32K_LOCKDOWN,
        ),
    ] {
        let policy = WorkerPolicy {
            mitigations,
            ..WorkerPolicy::default()
        };
        let budgets = ResourceBudgets::default();
        let decode = run_decode(&profile(), &worker_exe(), &encoded, &policy, &budgets).unwrap();
        let strip = run_strip(&profile(), &worker_exe(), &encoded, &policy, &budgets).unwrap();
        for (outcome, stats) in [
            (decode.outcome.err(), decode.stats),
            (strip.outcome.err(), strip.stats),
        ] {
            match outcome {
                Some(RunError::Worker(WorkerFailure::ContainmentCheck(mask))) => {
                    assert_eq!(mask, expected, "only the dropped control is reported")
                }
                other => panic!("expected containment refusal, got {other:?}"),
            }
            assert_eq!(stats.exit_code, WORKER_EXIT_CONTAINMENT);
        }
    }
}

#[test]
fn given_malformed_image_then_contained_worker_reports_guest_status() {
    let (encoded, _) = png(8, 8);
    let truncated = &encoded[..encoded.len() - 20];
    let run = run_decode(
        &profile(),
        &worker_exe(),
        truncated,
        &WorkerPolicy::default(),
        &ResourceBudgets::default(),
    )
    .unwrap();
    assert!(
        matches!(
            run.outcome,
            Err(RunError::Worker(WorkerFailure::GuestRejected(2)))
        ),
        "{:?}",
        run.outcome.err()
    );
}

/// GIVEN a received JPEG cut inside its scan (no EOI) WHEN the contained
/// worker decodes it THEN the worker reports the guest's malformed status and
/// returns no pixels, instead of a partly grey image.
#[test]
fn given_truncated_jpeg_then_contained_worker_fails_without_pixels() {
    let fixture = include_bytes!("../../media-sandbox/tests/fixtures/solid-16x8-baseline.jpg");
    // The scan data are bytes 623 to 630, followed by EOI; cut after 627.
    assert_eq!(&fixture[609..611], &[0xff, 0xda]);
    let truncated = &fixture[..fixture.len() - 6];
    let run = run_decode(
        &profile(),
        &worker_exe(),
        truncated,
        &WorkerPolicy::default(),
        &ResourceBudgets::default(),
    )
    .unwrap();
    assert!(
        matches!(
            run.outcome,
            Err(RunError::Worker(WorkerFailure::GuestRejected(2)))
        ),
        "{:?}",
        run.outcome.err()
    );
    assert_eq!(run.stats.exit_code, 1);
}

#[test]
fn given_deadline_shorter_than_the_operation_then_broker_terminates_worker() {
    let (encoded, _) = png(1024, 1024);
    let policy = WorkerPolicy {
        wall_deadline: Duration::from_millis(50),
        ..WorkerPolicy::default()
    };
    let budgets = ResourceBudgets::default();
    let decode = run_decode(&profile(), &worker_exe(), &encoded, &policy, &budgets).unwrap();
    let strip = run_strip(&profile(), &worker_exe(), &encoded, &policy, &budgets).unwrap();
    for (outcome, stats) in [
        (decode.outcome.err(), decode.stats),
        (strip.outcome.err(), strip.stats),
    ] {
        assert!(
            matches!(outcome, Some(RunError::DeadlineExceeded)),
            "{outcome:?}"
        );
        assert!(stats.elapsed < Duration::from_secs(5));
    }
}

#[test]
fn given_memory_limit_below_decode_needs_then_worker_does_not_produce_pixels() {
    let (encoded, _) = png(1024, 1024);
    let policy = WorkerPolicy {
        // A 1-megapixel decode peaked at about 19 MB in a debug worker.
        memory_limit_bytes: 12 * 1024 * 1024,
        ..WorkerPolicy::default()
    };
    let run = run_decode(
        &profile(),
        &worker_exe(),
        &encoded,
        &policy,
        &ResourceBudgets::default(),
    )
    .unwrap();
    match &run.outcome {
        Ok(_) => panic!("decode unexpectedly succeeded: {:?}", run.stats),
        Err(RunError::DeadlineExceeded) => panic!("hit the deadline, not the memory limit"),
        Err(_) => {}
    }
    assert!(
        run.stats.peak_process_memory <= policy.memory_limit_bytes,
        "{:?}",
        run.stats
    );
}

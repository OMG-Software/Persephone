//! End-to-end runs of the real media worker under the broker's containment.
//! `cargo test --workspace` builds the worker binary these tests launch.
#![cfg(windows)]

use persephone_media_broker::{
    AppContainerProfile, Mitigations, RunError, WORKER_EXIT_CONTAINMENT, WorkerPolicy, run_worker,
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

#[test]
fn given_png_when_decoded_by_contained_worker_then_pixels_match_within_limits() {
    let (encoded, rgba) = png(64, 32);
    let policy = WorkerPolicy::default();
    let run = run_worker(
        &profile(),
        &worker_exe(),
        &encoded,
        &policy,
        &ResourceBudgets::default(),
    )
    .unwrap();
    let frame = match run.outcome {
        Ok(frame) => frame,
        Err(error) => panic!("decode failed: {error:?}; stats {:?}", run.stats),
    };
    assert_eq!(frame.header().kind, FrameKind::RgbaPixels);
    assert_eq!((frame.header().width, frame.header().height), (64, 32));
    assert_eq!(frame.body(), &rgba[..]);
    assert_eq!(run.stats.exit_code, 0);
    assert!(run.stats.peak_process_memory > 0);
    assert!(run.stats.peak_process_memory <= policy.memory_limit_bytes);
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
        let run = run_worker(
            &profile(),
            &worker_exe(),
            &encoded,
            &policy,
            &ResourceBudgets::default(),
        )
        .unwrap();
        match run.outcome {
            Err(RunError::Worker(WorkerFailure::ContainmentCheck(mask))) => {
                assert_eq!(mask, expected, "only the dropped control is reported")
            }
            other => panic!("expected containment refusal, got {:?}", other.err()),
        }
        assert_eq!(run.stats.exit_code, WORKER_EXIT_CONTAINMENT);
    }
}

#[test]
fn given_malformed_image_then_contained_worker_reports_guest_status() {
    let (encoded, _) = png(8, 8);
    let truncated = &encoded[..encoded.len() - 20];
    let run = run_worker(
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

#[test]
fn given_deadline_shorter_than_decode_then_broker_terminates_worker() {
    let (encoded, _) = png(1024, 1024);
    let policy = WorkerPolicy {
        wall_deadline: Duration::from_millis(50),
        ..WorkerPolicy::default()
    };
    let run = run_worker(
        &profile(),
        &worker_exe(),
        &encoded,
        &policy,
        &ResourceBudgets::default(),
    )
    .unwrap();
    assert!(
        matches!(run.outcome, Err(RunError::DeadlineExceeded)),
        "{:?}",
        run.outcome.err()
    );
    assert!(run.stats.elapsed < Duration::from_secs(5));
}

#[test]
fn given_memory_limit_below_decode_needs_then_worker_does_not_produce_pixels() {
    let (encoded, _) = png(1024, 1024);
    let policy = WorkerPolicy {
        // A 1-megapixel decode peaked at about 19 MB in a debug worker.
        memory_limit_bytes: 12 * 1024 * 1024,
        ..WorkerPolicy::default()
    };
    let run = run_worker(
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

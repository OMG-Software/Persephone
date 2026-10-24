//! Contained-worker benchmark for the largest budgeted images. Ignored by
//! default; run in release with images generated outside the repository:
//!
//! PERSEPHONE_BENCH_DIR=<dir> cargo test --release -p persephone-media-broker
//!     --test benchmark -- --ignored --nocapture
#![cfg(windows)]

use persephone_media_broker::{AppContainerProfile, WorkerPolicy, run_decode};
use persephone_media_protocol::ResourceBudgets;
use std::path::PathBuf;

#[test]
#[ignore = "benchmark; needs PERSEPHONE_BENCH_DIR and a release build"]
fn benchmark_contained_worker_on_largest_budgeted_images() {
    let dir =
        PathBuf::from(std::env::var_os("PERSEPHONE_BENCH_DIR").expect("PERSEPHONE_BENCH_DIR"));
    let worker = std::env::current_exe()
        .unwrap()
        .parent()
        .and_then(|deps| deps.parent())
        .unwrap()
        .join("persephone-media-worker.exe");
    let profile = AppContainerProfile::temporary(&format!(
        "Persephone.MediaWorker.Bench.{}",
        std::process::id()
    ))
    .unwrap();
    let policy = WorkerPolicy::default();
    let mut images: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    images.sort();
    for path in images {
        let encoded = std::fs::read(&path).unwrap();
        let run = run_decode(
            &profile,
            &worker,
            &encoded,
            &policy,
            &ResourceBudgets::default(),
        )
        .unwrap();
        let stats = run.stats;
        println!(
            "{}: {} | wall {} ms | user CPU {} ms | peak {:.1} MiB of {} MiB | exit {}",
            path.file_name().unwrap().to_string_lossy(),
            match &run.outcome {
                Ok(image) => format!(
                    "{}x{} orientation {}",
                    image.pixels.header().width,
                    image.pixels.header().height,
                    image.orientation
                ),
                Err(error) => format!("{error:?}"),
            },
            stats.elapsed.as_millis(),
            stats.user_cpu.as_millis(),
            stats.peak_process_memory as f64 / 1_048_576.0,
            policy.memory_limit_bytes / 1_048_576,
            stats.exit_code,
        );
    }
}

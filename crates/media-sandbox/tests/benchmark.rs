//! Decode-time benchmark for the largest budgeted images. Ignored by default;
//! run in release with images generated outside the repository:
//!
//! PERSEPHONE_BENCH_DIR=<dir> cargo test --release -p persephone-media-sandbox
//!     --test benchmark -- --ignored --nocapture

use persephone_media_sandbox::{Limits, Sandbox};
use std::{path::PathBuf, time::Instant};

#[test]
#[ignore = "benchmark; needs PERSEPHONE_BENCH_DIR and a release build"]
fn benchmark_largest_budgeted_images() {
    let dir =
        PathBuf::from(std::env::var_os("PERSEPHONE_BENCH_DIR").expect("PERSEPHONE_BENCH_DIR"));
    let guest = std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../../.local/build/rust/wasm32-unknown-unknown/release/persephone_media_decoder.wasm",
    ))
    .unwrap();
    let started = Instant::now();
    let sandbox = Sandbox::new(&guest).unwrap();
    println!("module validation: {} ms", started.elapsed().as_millis());
    let limits = Limits {
        fuel: u64::MAX / 2,
        ..Limits::default()
    };
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("png" | "jpg")
            )
        })
        .collect();
    names.sort();
    for path in names {
        let input = std::fs::read(&path).unwrap();
        let native = Instant::now();
        let reference = persephone_media_decoder::decode(&input).unwrap();
        let native_ms = native.elapsed().as_millis();
        let sandboxed = Instant::now();
        let (image, fuel) = sandbox.decode_measured(&input, &limits).unwrap();
        let sandbox_ms = sandboxed.elapsed().as_millis();
        assert_eq!(
            image.rgba, reference.pixels,
            "sandbox output differs from native"
        );
        println!(
            "{}: {} MiB in, {}x{}, native {native_ms} ms, sandbox {sandbox_ms} ms ({:.1}x), fuel {fuel}",
            path.file_name().unwrap().to_string_lossy(),
            input.len() / 1_048_576,
            image.width,
            image.height,
            sandbox_ms as f64 / native_ms.max(1) as f64,
        );
    }
}

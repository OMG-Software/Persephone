//! Capability and resource checks for the WebAssembly decoder sandbox
//! (ADR-0003). Hostile modules are written in WAT; the real guest is the
//! `persephone-media-decoder` build that `tools/dev.py check` produces first.

use persephone_media_sandbox::{ALLOWED_IMPORTS, IMPORT_MODULE, Limits, Sandbox, SandboxError};
use std::path::PathBuf;

fn guest_wasm() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../../.local/build/rust/wasm32-unknown-unknown/release/persephone_media_decoder.wasm",
    );
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "guest module missing at {} ({error}); build it with `python tools/dev.py check` \
             or `cargo build --release -p persephone-media-decoder --target wasm32-unknown-unknown`",
            path.display()
        )
    })
}

fn sandbox(wat: &str) -> Result<Sandbox, SandboxError> {
    Sandbox::new(&wat::parse_str(wat).unwrap())
}

fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(rgba)
        .unwrap();
    out
}

fn small_limits() -> Limits {
    Limits {
        fuel: 1_000_000,
        ..Limits::default()
    }
}

// --- The real guest -------------------------------------------------------

#[test]
fn given_built_guest_then_it_passes_the_exact_import_allowlist() {
    // Sandbox::new refuses any import outside the allowlist, including WASI.
    Sandbox::new(&guest_wasm()).unwrap();
}

#[test]
fn given_png_when_decoded_in_sandbox_then_rgba_matches_source() {
    let source: Vec<u8> = (0..3 * 2 * 4).map(|value| value as u8 * 9).collect();
    let image = Sandbox::new(&guest_wasm())
        .unwrap()
        .decode(&png(3, 2, &source), &Limits::default())
        .unwrap();
    assert_eq!((image.width, image.height), (3, 2));
    assert_eq!(image.rgba, source);
}

#[test]
fn given_baseline_and_progressive_jpeg_when_decoded_in_sandbox_then_dimensions_and_colours_hold() {
    let sandbox = Sandbox::new(&guest_wasm()).unwrap();
    let solid = sandbox
        .decode(
            include_bytes!("fixtures/solid-16x8-baseline.jpg"),
            &Limits::default(),
        )
        .unwrap();
    assert_eq!((solid.width, solid.height), (16, 8));
    for pixel in solid.rgba.as_chunks::<4>().0.iter() {
        for (actual, expected) in pixel.iter().zip([200u8, 30, 90, 255]) {
            assert!(actual.abs_diff(expected) <= 6, "pixel {pixel:?}");
        }
    }
    let gradient = sandbox
        .decode(
            include_bytes!("fixtures/gradient-33x17-progressive.jpg"),
            &Limits::default(),
        )
        .unwrap();
    assert_eq!((gradient.width, gradient.height), (33, 17));
    assert_eq!(gradient.rgba.len(), 33 * 17 * 4);
    assert!(
        gradient
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| pixel[3] == 255)
    );
}

#[test]
fn given_hostile_or_unsupported_images_then_guest_refuses_with_its_status() {
    let sandbox = Sandbox::new(&guest_wasm()).unwrap();
    let limits = Limits::default();
    assert_eq!(
        sandbox.decode(b"GIF89a....", &limits),
        Err(SandboxError::GuestRejected(1))
    );
    let valid = png(4, 4, &[7; 64]);
    assert_eq!(
        sandbox.decode(&valid[..valid.len() - 20], &limits),
        Err(SandboxError::GuestRejected(2))
    );
    let mut huge = png(1, 1, &[0; 4]);
    huge[16..20].copy_from_slice(&100_000u32.to_be_bytes());
    huge[20..24].copy_from_slice(&100_000u32.to_be_bytes());
    let crc = crc32(&huge[12..29]);
    huge[29..33].copy_from_slice(&crc.to_be_bytes());
    assert_eq!(
        sandbox.decode(&huge, &limits),
        Err(SandboxError::GuestRejected(3))
    );
}

#[test]
fn given_real_guest_and_tiny_fuel_then_decode_stops_with_fuel_exhausted() {
    let sandbox = Sandbox::new(&guest_wasm()).unwrap();
    let limits = Limits {
        fuel: 10_000,
        ..Limits::default()
    };
    assert_eq!(
        sandbox.decode(&png(64, 64, &[1; 64 * 64 * 4]), &limits),
        Err(SandboxError::FuelExhausted)
    );
}

#[test]
fn given_input_over_host_limit_then_it_is_refused_before_the_guest_runs() {
    let sandbox = Sandbox::new(&guest_wasm()).unwrap();
    let limits = Limits {
        max_input_bytes: 8,
        ..Limits::default()
    };
    assert_eq!(
        sandbox.decode(&[0; 9], &limits),
        Err(SandboxError::InputTooLarge)
    );
}

// --- Import allowlist -------------------------------------------------------

const VALID_EXPORTS: &str =
    r#"(memory (export "memory") 1) (func (export "decode") (result i32) i32.const 0)"#;

#[test]
fn given_wasi_or_unknown_imports_then_module_is_refused_before_instantiation() {
    for import in [
        r#"(import "wasi_snapshot_preview1" "sock_open" (func (param i32 i32 i32) (result i32)))"#,
        r#"(import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32)))"#,
        r#"(import "persephone" "connect" (func (param i32 i32) (result i32)))"#,
        r#"(import "env" "input_len" (func (result i32)))"#,
    ] {
        let result = sandbox(&format!("(module {import} {VALID_EXPORTS})"));
        assert!(
            matches!(result, Err(SandboxError::ForbiddenImport { .. })),
            "{import}: {result:?}"
        );
    }
}

#[test]
fn given_allowed_name_with_wrong_signature_or_kind_then_module_is_refused() {
    for import in [
        r#"(import "persephone" "input_len" (func (result i64)))"#,
        r#"(import "persephone" "read_input" (func (param i32) (result i32)))"#,
        r#"(import "persephone" "input_len" (memory 1))"#,
        r#"(import "persephone" "input_len" (global i32))"#,
        r#"(import "persephone" "input_len" (table 1 funcref))"#,
    ] {
        let result = sandbox(&format!("(module {import} {VALID_EXPORTS})"));
        assert!(
            matches!(result, Err(SandboxError::ForbiddenImport { .. })),
            "{import}: {result:?}"
        );
    }
}

#[test]
fn given_every_allowed_import_then_module_is_accepted() {
    let imports: String = ALLOWED_IMPORTS
        .iter()
        .map(|(name, params, results)| {
            let params: String = params.iter().map(|_| " i32").collect();
            let results: String = results.iter().map(|_| " i32").collect();
            format!(
                r#"(import "{IMPORT_MODULE}" "{name}" (func (param{params}) (result{results})))"#
            )
        })
        .collect();
    sandbox(&format!("(module {imports} {VALID_EXPORTS})")).unwrap();
}

#[test]
fn given_missing_or_mistyped_exports_then_module_is_refused() {
    for module in [
        r#"(module (func (export "decode") (result i32) i32.const 0))"#,
        r#"(module (memory (export "memory") 1))"#,
        r#"(module (memory (export "memory") 1) (func (export "decode") (param i32) (result i32) i32.const 0))"#,
    ] {
        let result = sandbox(module);
        assert!(
            matches!(result, Err(SandboxError::InvalidExports(_))),
            "{module}: {result:?}"
        );
    }
}

#[test]
fn given_invalid_wasm_then_module_is_refused_by_validation() {
    // Type error: returns i64 from an i32 function. Rejected only if wasmi's
    // `validate` feature is enabled.
    let result = sandbox(
        r#"(module (memory (export "memory") 1) (func (export "decode") (result i32) i64.const 0))"#,
    );
    assert!(
        matches!(result, Err(SandboxError::InvalidModule(_))),
        "{result:?}"
    );
    assert!(matches!(
        Sandbox::new(b"\0asm\x01\0\0\0garbage"),
        Err(SandboxError::InvalidModule(_))
    ));
}

// --- Resource limits ---------------------------------------------------------

#[test]
fn given_infinite_loop_then_fuel_stops_it() {
    let sandbox = sandbox(
        r#"(module (memory (export "memory") 1)
             (func (export "decode") (result i32) (loop $forever (br $forever)) i32.const 0))"#,
    )
    .unwrap();
    assert_eq!(
        sandbox.decode(&[], &small_limits()),
        Err(SandboxError::FuelExhausted)
    );
}

#[test]
fn given_memory_growth_past_cap_then_growth_fails_inside_the_guest() {
    // memory.grow returns -1 when the store limit refuses it.
    let sandbox = sandbox(
        r#"(module (memory (export "memory") 1)
             (func (export "decode") (result i32)
               (if (result i32) (i32.eq (memory.grow (i32.const 4096)) (i32.const -1))
                 (then i32.const 77) (else i32.const 0))))"#,
    )
    .unwrap();
    let limits = Limits {
        max_memory_bytes: 16 * 65536,
        ..small_limits()
    };
    assert_eq!(
        sandbox.decode(&[], &limits),
        Err(SandboxError::GuestRejected(77))
    );
}

#[test]
fn given_initial_memory_over_cap_then_instantiation_fails() {
    let sandbox =
        sandbox(r#"(module (memory (export "memory") 64) (func (export "decode") (result i32) i32.const 0))"#)
            .unwrap();
    let limits = Limits {
        max_memory_bytes: 16 * 65536,
        ..small_limits()
    };
    assert!(matches!(
        sandbox.decode(&[], &limits),
        Err(SandboxError::Instantiation(_))
    ));
}

#[test]
fn given_trap_or_unbounded_recursion_then_host_reports_a_trap() {
    for body in ["unreachable", "call $recurse"] {
        let sandbox = sandbox(&format!(
            r#"(module (memory (export "memory") 1)
                 (func $recurse (result i32) call $recurse)
                 (func (export "decode") (result i32) {body}))"#
        ))
        .unwrap();
        let result = sandbox.decode(&[], &Limits::default());
        assert!(
            matches!(result, Err(SandboxError::Trap(_))),
            "{body}: {result:?}"
        );
    }
}

// --- Output validation --------------------------------------------------------

fn output_module(body: &str) -> Sandbox {
    sandbox(&format!(
        r#"(module
             (import "persephone" "write_output" (func $out (param i32 i32 i32 i32) (result i32)))
             (memory (export "memory") 1)
             (func (export "decode") (result i32) {body} drop i32.const 0))"#
    ))
    .unwrap()
}

#[test]
fn given_guest_output_breaking_host_rules_then_decode_fails_even_if_guest_reports_success() {
    for (body, why) in [
        (
            "(call $out (i32.const 2) (i32.const 2) (i32.const 0) (i32.const 15))",
            "length mismatch",
        ),
        (
            "(call $out (i32.const 9000) (i32.const 1) (i32.const 0) (i32.const 36000))",
            "dimension",
        ),
        (
            "(call $out (i32.const 0) (i32.const 1) (i32.const 0) (i32.const 0))",
            "zero",
        ),
        (
            "(call $out (i32.const 2) (i32.const 2) (i32.const 65530) (i32.const 16))",
            "out of bounds",
        ),
        (
            "(drop (call $out (i32.const 1) (i32.const 1) (i32.const 0) (i32.const 4))) \
             (call $out (i32.const 1) (i32.const 1) (i32.const 0) (i32.const 4))",
            "written twice",
        ),
    ] {
        let result = output_module(body).decode(&[], &small_limits());
        assert!(
            matches!(result, Err(SandboxError::InvalidOutput(_))),
            "{why}: {result:?}"
        );
    }
}

#[test]
fn given_valid_output_then_it_is_returned_and_success_without_output_is_refused() {
    let image =
        output_module("(call $out (i32.const 1) (i32.const 2) (i32.const 0) (i32.const 8))")
            .decode(&[], &small_limits())
            .unwrap();
    assert_eq!((image.width, image.height, image.rgba.len()), (1, 2, 8));
    let silent = sandbox(&format!("(module {VALID_EXPORTS})")).unwrap();
    assert_eq!(
        silent.decode(&[], &small_limits()),
        Err(SandboxError::NoOutput)
    );
}

#[test]
fn given_read_input_with_bad_range_then_host_refuses_the_copy() {
    let sandbox = sandbox(
        r#"(module
             (import "persephone" "read_input" (func $read (param i32 i32) (result i32)))
             (memory (export "memory") 1)
             (func (export "decode") (result i32) (call $read (i32.const 65535) (i32.const 4))))"#,
    )
    .unwrap();
    // Out-of-bounds destination: the host returns 3 and copies nothing.
    assert_eq!(
        sandbox.decode(&[1, 2, 3, 4], &small_limits()),
        Err(SandboxError::GuestRejected(3))
    );
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

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

/// A little-endian EXIF APP1 segment whose IFD0 holds Orientation.
fn exif_segment(orientation: u16) -> Vec<u8> {
    let mut payload = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
    payload.extend_from_slice(&orientation.to_le_bytes());
    payload.extend_from_slice(&[0; 6]);
    let mut segment = vec![0xff, 0xe1];
    segment.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    segment.extend_from_slice(&payload);
    segment
}

/// The baseline fixture with `segments` inserted after SOI.
fn solid_jpeg_with(segments: &[Vec<u8>]) -> Vec<u8> {
    let fixture = include_bytes!("fixtures/solid-16x8-baseline.jpg");
    let mut out = fixture[..2].to_vec();
    for segment in segments {
        out.extend_from_slice(segment);
    }
    out.extend_from_slice(&fixture[2..]);
    out
}

/// `png` with one extra chunk after IHDR.
fn png_with_chunk(png: &[u8], kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    let crc = crc32(&chunk[4..]);
    chunk.extend_from_slice(&crc.to_be_bytes());
    let mut out = png[..33].to_vec();
    out.extend_from_slice(&chunk);
    out.extend_from_slice(&png[33..]);
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
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
    assert_eq!((image.width, image.height, image.orientation), (3, 2, 1));
    assert_eq!(image.rgba, source);
}

#[test]
fn given_jpeg_with_exif_orientation_when_decoded_in_sandbox_then_orientation_is_reported() {
    let sandbox = Sandbox::new(&guest_wasm()).unwrap();
    for orientation in [1, 6, 8] {
        let image = sandbox
            .decode(
                &solid_jpeg_with(&[exif_segment(orientation)]),
                &Limits::default(),
            )
            .unwrap();
        assert_eq!(image.orientation, orientation as u8);
    }
    let invalid = sandbox
        .decode(&solid_jpeg_with(&[exif_segment(9)]), &Limits::default())
        .unwrap();
    assert_eq!(invalid.orientation, 1);
}

#[test]
fn given_jpeg_with_metadata_when_stripped_in_sandbox_then_metadata_is_gone_and_pixels_hold() {
    let sandbox = Sandbox::new(&guest_wasm()).unwrap();
    let comment = vec![0xff, 0xfe, 0, 8, b's', b'e', b'c', b'r', b'e', b't'];
    let input = solid_jpeg_with(&[exif_segment(6), comment]);
    let stripped = sandbox.strip(&input, &Limits::for_strip()).unwrap();
    assert!(stripped.len() < input.len());
    assert!(!contains(&stripped, b"secret"));
    assert!(!contains(&stripped, b"JFIF"));
    let original = sandbox.decode(&input, &Limits::default()).unwrap();
    let copy = sandbox.decode(&stripped, &Limits::default()).unwrap();
    assert_eq!(copy, original);
    assert_eq!(copy.orientation, 6);
}

#[test]
fn given_png_with_text_chunk_when_stripped_in_sandbox_then_only_image_chunks_remain() {
    let clean = png(2, 2, &[3; 16]);
    let input = png_with_chunk(&clean, b"tEXt", b"Comment\0secret");
    let stripped = Sandbox::new(&guest_wasm())
        .unwrap()
        .strip(&input, &Limits::for_strip())
        .unwrap();
    assert_eq!(stripped, clean);
}

#[test]
fn given_unsupported_or_malformed_input_when_stripped_in_sandbox_then_guest_refuses() {
    let sandbox = Sandbox::new(&guest_wasm()).unwrap();
    assert_eq!(
        sandbox.strip(b"GIF89a....", &Limits::for_strip()),
        Err(SandboxError::GuestRejected(1))
    );
    let input = solid_jpeg_with(&[exif_segment(6)]);
    assert_eq!(
        sandbox.strip(&input[..input.len() - 2], &Limits::for_strip()),
        Err(SandboxError::GuestRejected(2))
    );
    // An unknown critical chunk: the PNG decoder or the strip refuses it.
    let critical = png_with_chunk(&png(1, 1, &[0; 4]), b"ABCD", b"x");
    let result = sandbox.strip(&critical, &Limits::for_strip());
    assert!(
        matches!(result, Err(SandboxError::GuestRejected(2 | 7))),
        "{result:?}"
    );
}

#[test]
fn given_strip_limits_then_only_fuel_differs_from_decode_limits() {
    let (decode, strip) = (Limits::default(), Limits::for_strip());
    assert_eq!(strip.fuel, decode.fuel * 2);
    assert_eq!(strip.max_memory_bytes, decode.max_memory_bytes);
    assert_eq!(strip.max_input_bytes, decode.max_input_bytes);
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

#[test]
fn given_built_guest_then_its_linear_memory_is_fixed_at_the_sandbox_cap() {
    let engine = wasmi::Engine::default();
    let module = wasmi::Module::new(&engine, guest_wasm()).unwrap();
    let Some(wasmi::ExternType::Memory(memory)) = module.get_export("memory") else {
        panic!("guest exports no memory");
    };
    let cap_pages = (Limits::default().max_memory_bytes / 65_536) as u64;
    assert_eq!(memory.minimum(), cap_pages, "initial memory");
    assert_eq!(memory.maximum(), Some(cap_pages), "maximum memory");
}

// --- Import allowlist -------------------------------------------------------

const VALID_EXPORTS: &str = r#"(memory (export "memory") 1)
    (func (export "decode") (result i32) i32.const 0)
    (func (export "strip") (result i32) i32.const 0)"#;

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
        // The decode-only signature from before orientation was reported.
        r#"(import "persephone" "write_output" (func (param i32 i32 i32 i32) (result i32)))"#,
        r#"(import "persephone" "write_stripped" (func (param i32 i32 i32) (result i32)))"#,
        r#"(import "persephone" "write_stripped" (func (param i32 i32)))"#,
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
        r#"(module (func (export "decode") (result i32) i32.const 0)
                   (func (export "strip") (result i32) i32.const 0))"#,
        r#"(module (memory (export "memory") 1))"#,
        r#"(module (memory (export "memory") 1) (func (export "decode") (result i32) i32.const 0))"#,
        r#"(module (memory (export "memory") 1) (func (export "strip") (result i32) i32.const 0))"#,
        r#"(module (memory (export "memory") 1)
                   (func (export "decode") (param i32) (result i32) i32.const 0)
                   (func (export "strip") (result i32) i32.const 0))"#,
        r#"(module (memory (export "memory") 1)
                   (func (export "decode") (result i32) i32.const 0)
                   (func (export "strip") (result i64) i64.const 0))"#,
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
        r#"(module (memory (export "memory") 1) (func (export "decode") (result i32) i64.const 0)
                   (func (export "strip") (result i32) i32.const 0))"#,
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
             (func (export "decode") (result i32) (loop $forever (br $forever)) i32.const 0)
             (func (export "strip") (result i32) i32.const 0))"#,
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
                 (then i32.const 77) (else i32.const 0)))
             (func (export "strip") (result i32) i32.const 0))"#,
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
    let sandbox = sandbox(
        r#"(module (memory (export "memory") 64) (func (export "decode") (result i32) i32.const 0)
                     (func (export "strip") (result i32) i32.const 0))"#,
    )
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
                 (func (export "decode") (result i32) {body})
                 (func (export "strip") (result i32) i32.const 0))"#
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

/// A module whose `decode` and `strip` exports run the given bodies; both
/// output imports are declared.
fn output_module(decode: &str, strip: &str) -> Sandbox {
    sandbox(&format!(
        r#"(module
             (import "persephone" "write_output" (func $out (param i32 i32 i32 i32 i32) (result i32)))
             (import "persephone" "write_stripped" (func $stripped (param i32 i32) (result i32)))
             (memory (export "memory") 1)
             (func (export "decode") (result i32) {decode} drop i32.const 0)
             (func (export "strip") (result i32) {strip} drop i32.const 0))"#
    ))
    .unwrap()
}

const NOTHING: &str = "(i32.const 0)";

#[test]
fn given_guest_output_breaking_host_rules_then_decode_fails_even_if_guest_reports_success() {
    for (body, why) in [
        (
            "(call $out (i32.const 2) (i32.const 2) (i32.const 0) (i32.const 15) (i32.const 1))",
            "length mismatch",
        ),
        (
            "(call $out (i32.const 9000) (i32.const 1) (i32.const 0) (i32.const 36000) (i32.const 1))",
            "dimension",
        ),
        (
            "(call $out (i32.const 0) (i32.const 1) (i32.const 0) (i32.const 0) (i32.const 1))",
            "zero",
        ),
        (
            "(call $out (i32.const 2) (i32.const 2) (i32.const 65530) (i32.const 16) (i32.const 1))",
            "out of bounds",
        ),
        (
            "(drop (call $out (i32.const 1) (i32.const 1) (i32.const 0) (i32.const 4) (i32.const 1))) \
             (call $out (i32.const 1) (i32.const 1) (i32.const 0) (i32.const 4) (i32.const 1))",
            "written twice",
        ),
        (
            "(call $out (i32.const 1) (i32.const 1) (i32.const 0) (i32.const 4) (i32.const 0))",
            "orientation zero",
        ),
        (
            "(call $out (i32.const 1) (i32.const 1) (i32.const 0) (i32.const 4) (i32.const 9))",
            "orientation nine",
        ),
        (
            "(call $out (i32.const 1) (i32.const 1) (i32.const 0) (i32.const 4) (i32.const 0x106))",
            "orientation past u8",
        ),
        (
            "(call $stripped (i32.const 0) (i32.const 1))",
            "strip output during decode",
        ),
    ] {
        let result = output_module(body, NOTHING).decode(&[7; 8], &small_limits());
        assert!(
            matches!(result, Err(SandboxError::InvalidOutput(_))),
            "{why}: {result:?}"
        );
    }
}

#[test]
fn given_guest_strip_output_breaking_host_rules_then_strip_fails_even_if_guest_reports_success() {
    for (body, why) in [
        (
            "(call $stripped (i32.const 0) (i32.const 9))",
            "longer than input",
        ),
        ("(call $stripped (i32.const 0) (i32.const 0))", "empty"),
        (
            "(call $stripped (i32.const 65530) (i32.const 8))",
            "out of bounds",
        ),
        (
            "(call $stripped (i32.const -1) (i32.const 8))",
            "pointer wraps",
        ),
        (
            "(drop (call $stripped (i32.const 0) (i32.const 4))) \
             (call $stripped (i32.const 0) (i32.const 4))",
            "written twice",
        ),
        (
            "(call $out (i32.const 1) (i32.const 1) (i32.const 0) (i32.const 4) (i32.const 1))",
            "pixel output during strip",
        ),
    ] {
        let result = output_module(NOTHING, body).strip(&[7; 8], &small_limits());
        assert!(
            matches!(result, Err(SandboxError::InvalidOutput(_))),
            "{why}: {result:?}"
        );
    }
}

#[test]
fn given_valid_output_then_it_is_returned_and_success_without_output_is_refused() {
    let module = output_module(
        "(call $out (i32.const 1) (i32.const 2) (i32.const 0) (i32.const 8) (i32.const 7))",
        "(call $stripped (i32.const 0) (i32.const 8))",
    );
    let image = module.decode(&[], &small_limits()).unwrap();
    assert_eq!(
        (
            image.width,
            image.height,
            image.orientation,
            image.rgba.len()
        ),
        (1, 2, 7, 8)
    );
    // Guest memory starts zeroed; the module does not copy its input there.
    assert_eq!(module.strip(&[5; 8], &small_limits()).unwrap(), [0; 8]);
    let silent = sandbox(&format!("(module {VALID_EXPORTS})")).unwrap();
    assert_eq!(
        silent.decode(&[], &small_limits()),
        Err(SandboxError::NoOutput)
    );
    assert_eq!(
        silent.strip(&[1], &small_limits()),
        Err(SandboxError::NoOutput)
    );
}

#[test]
fn given_read_input_with_bad_range_then_host_refuses_the_copy() {
    let sandbox = sandbox(
        r#"(module
             (import "persephone" "read_input" (func $read (param i32 i32) (result i32)))
             (memory (export "memory") 1)
             (func (export "decode") (result i32) (call $read (i32.const 65535) (i32.const 4)))
             (func (export "strip") (result i32) (call $read (i32.const 65535) (i32.const 4))))"#,
    )
    .unwrap();
    // Out-of-bounds destination: the host returns 3 and copies nothing.
    assert_eq!(
        sandbox.decode(&[1, 2, 3, 4], &small_limits()),
        Err(SandboxError::GuestRejected(3))
    );
    assert_eq!(
        sandbox.strip(&[1, 2, 3, 4], &small_limits()),
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

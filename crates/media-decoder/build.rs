//! Fixes the guest's linear memory at one size for the wasm32 build.
//!
//! Initial and maximum memory are equal, so the guest never calls
//! `memory.grow` successfully. wasmi backs linear memory with a `Vec` and
//! grows it by reallocating, which briefly holds old and new buffers and can
//! exceed the worker's Job memory limit; a fixed size avoids that and makes the
//! worker's commit predictable. The size covers the heaviest measured
//! 4000 × 4000 input (a progressive 4:4:4 JPEG, 158 MiB) with about 11%
//! headroom. It must stay equal to `persephone-media-sandbox`'s default
//! `Limits::max_memory_bytes`.

const GUEST_MEMORY_BYTES: u64 = 176 * 1024 * 1024;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        assert_eq!(GUEST_MEMORY_BYTES % 65_536, 0, "must be whole Wasm pages");
        println!("cargo::rustc-link-arg-cdylib=--initial-memory={GUEST_MEMORY_BYTES}");
        println!("cargo::rustc-link-arg-cdylib=--max-memory={GUEST_MEMORY_BYTES}");
    }
}

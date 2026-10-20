//! Embeds the WebAssembly decoder guest in the worker binary, so the module a
//! worker runs is fixed when the worker is built. `tools/dev.py check` builds
//! the guest first; set PERSEPHONE_MEDIA_GUEST to use another build.

use std::{env, path::PathBuf};

fn main() {
    println!("cargo::rerun-if-env-changed=PERSEPHONE_MEDIA_GUEST");
    let guest = env::var_os("PERSEPHONE_MEDIA_GUEST")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join(
                "../../.local/build/rust/wasm32-unknown-unknown/release/persephone_media_decoder.wasm",
            )
        });
    if !guest.is_file() {
        panic!(
            "decoder guest not found at {}; build it first with `cargo build --release \
             -p persephone-media-decoder --target wasm32-unknown-unknown` \
             (python tools/dev.py check does this)",
            guest.display()
        );
    }
    println!("cargo::rerun-if-changed={}", guest.display());
    println!(
        "cargo::rustc-env=PERSEPHONE_MEDIA_GUEST_PATH={}",
        guest.display()
    );
}

# Persephone patch: webrtc-sys 0.3.54 (ADR-0043)

Upstream: crates.io `webrtc-sys` 0.3.54, Apache-2.0 (see `NOTICE.md`).
`LICENSE-APACHE` is the standard Apache-2.0 text, copied from the `serde`
1.0.229 crate, because the published crate has none. Used through
`[patch.crates-io]` in the root `Cargo.toml`. Reapply this patch for each
LiveKit update.

## `src/frame_cryptor.cpp`

- `KeyProvider::KeyProvider` clamps the key ring size with
  `std::clamp(options.key_ring_size, 1, 256)` instead of `1, 255`. Without
  this, the Rust change in `libwebrtc` has no effect: a ring of 256 would be
  cut to 255 here and key index 255 would be refused.

`build.rs` is unchanged. Build with `LK_CUSTOM_WEBRTC` set to the verified
libwebrtc, so the build never downloads one.

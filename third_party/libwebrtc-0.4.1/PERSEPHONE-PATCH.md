# Persephone patch: libwebrtc 0.4.1 (ADR-0043)

Upstream: crates.io `libwebrtc` 0.4.1, Apache-2.0. The published crate has no
license file; `LICENSE-APACHE` is the standard Apache-2.0 text, copied from the
`serde` 1.0.229 crate. Used through `[patch.crates-io]` in the root
`Cargo.toml`. Reapply this patch for each LiveKit update.

## `src/native/frame_cryptor.rs`

- `MAX_KEY_RING_SIZE` is 256, not 255, so the key ring can have 256 slots and
  key index 255 (used by Element Call) is valid. The unit test checks 256.

The same change is needed in `webrtc-sys` (see its PERSEPHONE-PATCH.md): that
crate clamps the ring size again in C++. The prebuilt libwebrtc allows 256
(`MAX_KEYRING_SIZE` in `api/crypto/frame_crypto_transformer.h`).

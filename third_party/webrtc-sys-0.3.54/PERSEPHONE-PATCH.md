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

## Remote audio volume (ADR-0055)

- `include/livekit/audio_track.h` and `src/audio_track.cpp`:
  `AudioTrack::set_volume(double) const` clamps the volume to 0.0..1.0 (NaN
  becomes 0.0) and calls `SetVolume` on the track's audio source on the
  signaling thread.
  For a remote track, WebRTC's `RemoteAudioSource` passes it to the
  receiver's `OnSetVolume`, which expects that thread. A local track's
  source ignores it.
- `src/audio_track.rs`: the cxx bridge declares `set_volume`.
- No native object, thread, callback or observer is added, so the strong
  reference graph does not change.
- The lifecycle workload this crate's `AGENTS.md` asks for is
  `crates/call-worker/src/native_volume_tests.rs`. These crates are patch
  sources, not workspace members, so their own tests do not run.

Each change is marked `PERSEPHONE PATCH (ADR-0055)`.

`build.rs` is unchanged. Build with `LK_CUSTOM_WEBRTC` set to the verified
libwebrtc, so the build never downloads one.

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

## Remote audio volume (ADR-0055)

- `src/native/audio_track.rs` and `src/audio_track.rs`:
  `RtcAudioTrack::set_volume(f64)` calls `webrtc-sys`'s
  `AudioTrack::set_volume`, which clamps to 0.0..1.0 (NaN becomes 0.0). The call worker uses
  it for the call volume and deafen of remote tracks.

Each change is marked `PERSEPHONE PATCH (ADR-0055)`.

## Microphone gain and automatic gain control (ADR-0062)

- `src/native/audio_track.rs` and `src/audio_track.rs`:
  `RtcAudioTrack::set_auto_gain_control(bool) -> bool` calls `webrtc-sys`'s
  `AudioTrack::set_auto_gain_control`. It is true when WebRTC applied or
  stored the options. The call worker uses it for the automatic microphone
  level of the published track.
- `src/native/capture_processing.rs` (new, exported as
  `native::capture_processing`): `set_capture_gain_percent(u16) -> bool` (false
  over `MAX_CAPTURE_GAIN_PERCENT`, 400, checked here and again in C++) and
  `capture_processing() -> Option<CaptureProcessing>` (the oldest live
  factory; it waits for the worker thread) call the free functions of
  `webrtc-sys`. The call worker uses them for the manual
  microphone gain and for its diagnostic notes.

Each change is marked `PERSEPHONE PATCH (ADR-0062)`.

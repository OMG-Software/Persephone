//! The camera backend for targets without one yet: nothing is listed and no
//! capture starts. Add an OS backend as `camera/<os>.rs` behind the same
//! functions (see `crates/call-worker/AGENTS.md`).

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;

use persephone_call_protocol::CameraState;

use super::CapturedFrame;
use crate::send_tier::SendTier;

pub fn list() -> Vec<(String, String)> {
    Vec::new()
}

pub fn capture<F, E>(
    _device_id: &str,
    _target: SendTier,
    _stop: &AtomicBool,
    open_result: Sender<Result<(), CameraState>>,
    _on_frame: F,
    _on_end: E,
) where
    F: FnMut(CapturedFrame<'_>) + Send,
    E: FnOnce() + Send,
{
    let _ = open_result.send(Err(CameraState::Unavailable));
}

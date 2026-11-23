//! The screen backend for targets without one (macOS until its own decision
//! and a measurement on a Mac, ADR-0053): no target is accepted and no
//! capture starts, so the worker reports screen sharing as unavailable.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;

use persephone_call_protocol::{ScreenState, ScreenTarget};

use super::{CapturedFrame, Ended, Grant};

pub fn accepts(_target: &ScreenTarget) -> bool {
    false
}

pub fn capture<F, E>(
    _target: ScreenTarget,
    _grant: Grant,
    _stop: &Arc<AtomicBool>,
    open_result: Sender<Result<(), ScreenState>>,
    _on_frame: F,
    _on_end: E,
) where
    F: FnMut(CapturedFrame<'_>) + Send + 'static,
    E: FnOnce(Ended) + Send,
{
    let _ = open_result.send(Err(ScreenState::Unavailable));
}

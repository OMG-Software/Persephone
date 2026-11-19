//! The Windows screen backend (ADR-0053): Windows.Graphics.Capture of one
//! monitor, through `wgc.rs`.
//!
//! The core named the monitor by its index in display order and its size.
//! The worker reads its own list when the share starts; a monitor that is
//! gone, or has another size at that index, is `NotFound`. The code never
//! shows a picker and never takes another monitor. The capture border stays
//! on. A refused open (the AppContainer lacks the capability) is `Refused`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use persephone_call_protocol::{ScreenState, ScreenTarget};

use super::wgc::{self, Stage};
use super::{CapturedFrame, Ended, Format, Grant};

/// How long the loop sleeps when no frame waits. A new frame is looked for
/// about 30 times a second at most; the session sends at most 5.
const POLL_SLEEP: Duration = Duration::from_millis(33);
/// A frame is copied at most this often. The session's limiter then keeps
/// the rate at 5 per second; frames in between are dropped before the copy.
const MIN_COPY_GAP: Duration = Duration::from_millis(150);

/// Whether Windows.Graphics.Capture runs in the call worker's AppContainer.
/// The measurement of 19 November 2026 says it does not: `CreateForMonitor`
/// fails with `E_ACCESSDENIED` with the worker's capabilities and with each
/// candidate capability (`docs/evidence/screen-share-windows.md`). So
/// Windows reports screen sharing as unavailable (ADR-0053). The capture code
/// stays, because the containment gate runs it; set this to `true` only for
/// a capability set that the gate shows to work, and record it there.
const CAPTURE_IN_CONTAINER: bool = false;

pub fn accepts(target: &ScreenTarget) -> bool {
    CAPTURE_IN_CONTAINER && matches!(target, ScreenTarget::Monitor { .. })
}

/// What a failed start means for the share.
pub fn state_for(stage: Stage) -> ScreenState {
    match stage {
        Stage::NoMonitor | Stage::SizeChanged => ScreenState::NotFound,
        // The OS says no: the AppContainer may lack the capability.
        Stage::Item | Stage::Start | Stage::Session => ScreenState::Refused,
        _ => ScreenState::Unavailable,
    }
}

/// The thread body of a capture. Reports the result of opening once, then
/// delivers pictures until `stop`, an error or the end of the source.
pub fn capture<F, E>(
    target: ScreenTarget,
    _grant: Grant,
    stop: &Arc<AtomicBool>,
    open_result: Sender<Result<(), ScreenState>>,
    mut on_frame: F,
    on_end: E,
) where
    F: FnMut(CapturedFrame<'_>) + Send + 'static,
    E: FnOnce(Ended) + Send,
{
    let ScreenTarget::Monitor {
        index,
        width,
        height,
    } = target
    else {
        let _ = open_result.send(Err(ScreenState::Unavailable));
        return;
    };
    let Ok(_com) = wgc::ComGuard::start() else {
        let _ = open_result.send(Err(ScreenState::Unavailable));
        return;
    };
    let mut capture = match wgc::open(usize::from(index), Some((width, height))) {
        Ok(capture) => capture,
        Err(stage) => {
            let _ = open_result.send(Err(state_for(stage)));
            return;
        }
    };
    let _ = open_result.send(Ok(()));
    drop(open_result);
    let mut last_copy: Option<Instant> = None;
    let ended = loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        if capture.source_closed() {
            break Ended::Source;
        }
        let mut admit = || {
            let now = Instant::now();
            let due = last_copy.is_none_or(|at| now.duration_since(at) >= MIN_COPY_GAP);
            if due {
                last_copy = Some(now);
            }
            due
        };
        let mut deliver = |width: u32, height: u32, stride: u32, bytes: &[u8]| {
            if let Some(frame) = CapturedFrame::checked(Format::Bgra, width, height, stride, bytes)
            {
                on_frame(frame);
            }
        };
        match capture.poll(&mut admit, &mut deliver) {
            Ok(true) => {}
            Ok(false) => std::thread::sleep(POLL_SLEEP),
            Err(_) => break Ended::Failed,
        }
    };
    // Release the screen before the end is reported.
    drop(capture);
    if !stop.load(Ordering::Acquire) {
        on_end(ended);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN each stage a start can fail at WHEN mapped THEN a missing or
    /// resized monitor is not found, an OS refusal is refused, and the rest
    /// is unavailable.
    #[test]
    fn given_start_failures_when_mapped_then_the_state_names_the_cause() {
        assert_eq!(state_for(Stage::NoMonitor), ScreenState::NotFound);
        assert_eq!(state_for(Stage::SizeChanged), ScreenState::NotFound);
        assert_eq!(state_for(Stage::Item), ScreenState::Refused);
        assert_eq!(state_for(Stage::Start), ScreenState::Refused);
        assert_eq!(state_for(Stage::Session), ScreenState::Refused);
        for stage in [
            Stage::Com,
            Stage::Interop,
            Stage::Device,
            Stage::Pool,
            Stage::BadSize,
        ] {
            assert_eq!(state_for(stage), ScreenState::Unavailable, "{stage:?}");
        }
    }

    /// GIVEN the measurement that capture does not run in the worker's
    /// AppContainer WHEN any monitor is opened THEN the share is unavailable
    /// and no capture thread starts.
    #[test]
    fn given_capture_not_measured_to_work_in_the_container_then_it_is_unavailable() {
        let result = super::super::open(
            ScreenTarget::Monitor {
                index: 0,
                width: 1920,
                height: 1080,
            },
            Grant::default(),
            |_| {},
            |_| {},
        );
        assert_eq!(result.err(), Some(ScreenState::Unavailable));
    }

    /// GIVEN a monitor at an index nobody has WHEN the capture code opens it
    /// THEN it reports `NotFound` and no other monitor is taken.
    #[test]
    fn given_an_unknown_monitor_when_opened_then_it_is_not_found() {
        let (open_tx, open_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        capture(
            ScreenTarget::Monitor {
                index: 15,
                width: 1,
                height: 1,
            },
            Grant::default(),
            &stop,
            open_tx,
            |_| {},
            |_| {},
        );
        assert_eq!(open_rx.recv().ok(), Some(Err(ScreenState::NotFound)));
    }
}

//! The Windows screen backend (ADR-0053): GDI `BitBlt` of one monitor,
//! through `gdi.rs`. Windows.Graphics.Capture and DXGI Desktop Duplication
//! are denied in the call AppContainer; GDI runs there with no new
//! capability and no relaxed mitigation (`docs/evidence/screen-share-windows.md`).
//!
//! The core named the monitor by its index in display order and its size.
//! The worker reads its own list when the share starts; a monitor that is
//! gone, or has another size at that index, is `NotFound`. The code never
//! shows a picker and never takes another monitor. GDI draws no capture
//! border and no cursor: the call bar says that the screen is shared.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use persephone_call_protocol::{ScreenState, ScreenTarget};

use super::gdi::{self, Stage};
use super::{CapturedFrame, Ended, Format, Grant};

/// How long the loop sleeps when no frame waits. A new frame is looked for
/// about 30 times a second at most; the session sends at most 5.
const POLL_SLEEP: Duration = Duration::from_millis(33);
/// A frame is copied at most this often. The session's limiter then keeps
/// the rate at 5 per second; frames in between are dropped before the copy.
const MIN_COPY_GAP: Duration = Duration::from_millis(150);

/// Whether the capture method runs in the call worker's AppContainer. The
/// gate measured it on 25 November 2026: GDI `BitBlt` copies a real picture
/// there with the worker's three capabilities and its mitigations. Windows.
/// Graphics.Capture and DXGI duplication fail with `E_ACCESSDENIED`
/// (`docs/evidence/screen-share-windows.md`). Set this to `false` if the gate
/// stops showing a picture for this code.
const CAPTURE_IN_CONTAINER: bool = true;

pub fn accepts(target: &ScreenTarget) -> bool {
    CAPTURE_IN_CONTAINER && matches!(target, ScreenTarget::Monitor { .. })
}

/// What a failed start means for the share.
pub fn state_for(stage: Stage) -> ScreenState {
    match stage {
        Stage::NoMonitor | Stage::SizeChanged => ScreenState::NotFound,
        // The OS says no: the screen is not readable now.
        Stage::Dc | Stage::Blit => ScreenState::Refused,
        Stage::Bitmap | Stage::BadSize => ScreenState::Unavailable,
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
    let mut capture = match gdi::open(usize::from(index), Some((width, height))) {
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
        assert_eq!(state_for(Stage::Dc), ScreenState::Refused);
        assert_eq!(state_for(Stage::Blit), ScreenState::Refused);
        for stage in [Stage::Bitmap, Stage::BadSize] {
            assert_eq!(state_for(stage), ScreenState::Unavailable, "{stage:?}");
        }
    }

    /// GIVEN a portal target WHEN the backend is asked THEN it does not
    /// accept it: Windows shares a monitor only (Portal is the Linux target).
    #[test]
    fn given_a_non_monitor_target_then_the_backend_does_not_accept_it() {
        assert!(accepts(&ScreenTarget::Monitor {
            index: 0,
            width: 2,
            height: 2,
        }));
        assert!(!accepts(&ScreenTarget::Portal { node: 7 }));
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

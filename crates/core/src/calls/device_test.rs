//! Device tests (ADR-0059): the pure rules.
//!
//! One contained call worker tests one microphone, speaker or camera and
//! ends. It connects to nothing. The microphone's level goes to the level
//! meter like a call's own level, and the camera's picture goes to tile 0 of
//! a picture store of its own. The controller runs the test
//! ([`super::controller::CallController::start_device_test`]); this module
//! decides what each worker message means for the page.

use std::time::Duration;

use persephone_call_protocol::{CameraState, DeviceTest, FailureCode, FromWorker};
use persephone_contracts::{CallDeviceTestKind, CallDeviceTestState};

/// The longest a test may run before the core ends it: the camera test's 30
/// seconds and its open (with a permission dialog on macOS).
pub const TEST_LIMIT: Duration = Duration::from_secs(100);
/// How long the worker has to end after `Leave`.
pub const LEAVE_WAIT: Duration = Duration::from_secs(5);
/// How long the worker has to exit at the end.
pub const FINISH_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the test looks for worker messages.
pub const POLL: Duration = Duration::from_millis(20);

/// The worker's name for the test.
pub fn worker_test(kind: CallDeviceTestKind) -> DeviceTest {
    match kind {
        CallDeviceTestKind::Microphone => DeviceTest::Microphone,
        CallDeviceTestKind::Speaker => DeviceTest::Speaker,
        CallDeviceTestKind::Camera => DeviceTest::Camera,
    }
}

/// Whether a test in this state has ended.
pub fn is_final(state: CallDeviceTestState) -> bool {
    !matches!(
        state,
        CallDeviceTestState::Starting | CallDeviceTestState::Running
    )
}

/// The page's state for a worker failure.
pub fn failure_state(code: FailureCode) -> CallDeviceTestState {
    match code {
        FailureCode::DeviceNotFound => CallDeviceTestState::NotFound,
        FailureCode::DeviceDenied => CallDeviceTestState::Denied,
        FailureCode::Unsupported => CallDeviceTestState::Unsupported,
        FailureCode::AudioDevice => CallDeviceTestState::Unavailable,
        FailureCode::Containment
        | FailureCode::SignallingRefused
        | FailureCode::Tls
        | FailureCode::Network
        | FailureCode::Decryption
        | FailureCode::Protocol
        | FailureCode::Internal
        | FailureCode::DeviceKept => CallDeviceTestState::Failed,
    }
}

/// The state after one worker message, or `None` when the message changes
/// nothing. An ended test stays as it ended. `stopped` is true once the
/// user stopped the test: a test that ends before its device ran is then
/// `idle`, not a failure. A device that reports it runs after the stop (it
/// was still opening) does not make the test run, so the user's stop is not
/// shown as a finished test.
pub fn next_state(
    current: CallDeviceTestState,
    message: &FromWorker,
    stopped: bool,
) -> Option<CallDeviceTestState> {
    if is_final(current) {
        return None;
    }
    let running = matches!(
        message,
        FromWorker::Connected
            | FromWorker::Camera {
                state: CameraState::On
            }
    );
    if stopped && running && current == CallDeviceTestState::Starting {
        return None;
    }
    let next = match message {
        FromWorker::Connected
        | FromWorker::Camera {
            state: CameraState::On,
        } => CallDeviceTestState::Running,
        // A camera that ran but was not released in time still ran: the
        // `Left` that follows ends the test as done.
        FromWorker::Camera {
            state: CameraState::Unavailable,
        } if current == CallDeviceTestState::Running => return None,
        FromWorker::Camera {
            state: CameraState::NotFound,
        } => CallDeviceTestState::NotFound,
        FromWorker::Camera {
            state: CameraState::Unavailable,
        } => CallDeviceTestState::Unavailable,
        FromWorker::Failed { code } => failure_state(*code),
        FromWorker::Left => match current {
            CallDeviceTestState::Running => CallDeviceTestState::Done,
            _ if stopped => CallDeviceTestState::Idle,
            _ => CallDeviceTestState::Failed,
        },
        _ => return None,
    };
    (next != current).then_some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN a microphone test WHEN the worker runs and ends it THEN the
    /// state goes starting, running, done, and later messages change
    /// nothing.
    #[test]
    fn given_a_test_that_runs_and_ends_then_it_is_done() {
        let mut state = CallDeviceTestState::Starting;
        for (message, expected) in [
            (FromWorker::Connected, Some(CallDeviceTestState::Running)),
            (FromWorker::SoundLevel { level: 40 }, None),
            (FromWorker::Connected, None),
            (FromWorker::Left, Some(CallDeviceTestState::Done)),
        ] {
            let next = next_state(state, &message, false);
            assert_eq!(next, expected, "{message:?}");
            state = next.unwrap_or(state);
        }
        assert_eq!(next_state(state, &FromWorker::Connected, false), None);
    }

    /// GIVEN the worker's failures and camera states THEN each has its page
    /// state, and no free text is involved.
    #[test]
    fn given_failures_then_each_has_its_state() {
        let start = CallDeviceTestState::Starting;
        let failed = |code| next_state(start, &FromWorker::Failed { code }, false);
        assert_eq!(
            failed(FailureCode::DeviceNotFound),
            Some(CallDeviceTestState::NotFound)
        );
        assert_eq!(
            failed(FailureCode::DeviceDenied),
            Some(CallDeviceTestState::Denied)
        );
        assert_eq!(
            failed(FailureCode::Unsupported),
            Some(CallDeviceTestState::Unsupported)
        );
        assert_eq!(
            failed(FailureCode::AudioDevice),
            Some(CallDeviceTestState::Unavailable)
        );
        assert_eq!(
            failed(FailureCode::Containment),
            Some(CallDeviceTestState::Failed)
        );
        let camera = |state| next_state(start, &FromWorker::Camera { state }, false);
        assert_eq!(
            camera(CameraState::NotFound),
            Some(CallDeviceTestState::NotFound)
        );
        assert_eq!(
            camera(CameraState::Unavailable),
            Some(CallDeviceTestState::Unavailable)
        );
        assert_eq!(camera(CameraState::On), Some(CallDeviceTestState::Running));
        assert_eq!(camera(CameraState::Off), None);
    }

    /// GIVEN a camera test that ran WHEN the camera is not released in time
    /// THEN the test still ends as done; before it ran, `Unavailable` is
    /// the result.
    #[test]
    fn given_a_camera_held_after_it_ran_then_the_test_is_done() {
        let held = FromWorker::Camera {
            state: CameraState::Unavailable,
        };
        let running = CallDeviceTestState::Running;
        assert_eq!(next_state(running, &held, false), None);
        assert_eq!(
            next_state(running, &FromWorker::Left, false),
            Some(CallDeviceTestState::Done)
        );
        assert_eq!(
            next_state(CallDeviceTestState::Starting, &held, false),
            Some(CallDeviceTestState::Unavailable)
        );
    }

    /// GIVEN a test that ends before its device ran WHEN the user stopped it
    /// THEN it is idle; WHEN nobody stopped it THEN it failed.
    #[test]
    fn given_an_early_end_then_only_a_stop_is_not_a_failure() {
        let start = CallDeviceTestState::Starting;
        assert_eq!(
            next_state(start, &FromWorker::Left, true),
            Some(CallDeviceTestState::Idle)
        );
        assert_eq!(
            next_state(start, &FromWorker::Left, false),
            Some(CallDeviceTestState::Failed)
        );
    }

    /// GIVEN a test the user stopped while its device opened WHEN the worker
    /// then reports the device runs and leaves THEN the test is idle, not
    /// done; WHEN the device ran before the stop THEN the test is done.
    #[test]
    fn given_a_stop_while_the_device_opens_then_a_late_start_is_ignored() {
        let start = CallDeviceTestState::Starting;
        let on = FromWorker::Camera {
            state: CameraState::On,
        };
        assert_eq!(next_state(start, &FromWorker::Connected, true), None);
        assert_eq!(next_state(start, &on, true), None);
        assert_eq!(
            next_state(start, &FromWorker::Left, true),
            Some(CallDeviceTestState::Idle)
        );
        let running = CallDeviceTestState::Running;
        assert_eq!(
            next_state(running, &FromWorker::Left, true),
            Some(CallDeviceTestState::Done)
        );
    }

    #[test]
    fn given_each_kind_then_the_worker_test_matches() {
        assert_eq!(
            worker_test(CallDeviceTestKind::Microphone),
            DeviceTest::Microphone
        );
        assert_eq!(
            worker_test(CallDeviceTestKind::Speaker),
            DeviceTest::Speaker
        );
        assert_eq!(worker_test(CallDeviceTestKind::Camera), DeviceTest::Camera);
    }
}

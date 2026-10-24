//! The OS-independent part of one worker run: the pre-launch size check, the
//! writer and reader threads, the deadline, and the reply checks. Each OS
//! supplies a [`Launcher`] that starts the worker under its containment.

use crate::{DecodedImage, LaunchError, RunError, WorkerPolicy, WorkerRun, WorkerStats};
use persephone_media_protocol::{
    FrameError, FrameHeader, FrameKind, PixelInfo, ReceivedFrame, ResourceBudgets, WorkerFailure,
    WorkerOperation, read_frame, write_frame,
};
use std::{
    fs::File,
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// How long to wait for the worker to exit after it was killed.
const EXIT_WAIT_AFTER_KILL: Duration = Duration::from_millis(5000);

/// One OS's way to start a contained worker.
pub(crate) trait Launcher {
    type Worker: Worker;

    /// Starts `worker` under the OS containment for `policy`. The worker
    /// gets only the two pipes; the broker keeps their other ends. The worker
    /// must not run outside its containment at any time.
    fn spawn(
        &self,
        worker: &Path,
        policy: &WorkerPolicy,
    ) -> Result<Spawned<Self::Worker>, LaunchError>;
}

/// A started worker and the broker's ends of its stdin and stdout.
pub(crate) struct Spawned<W> {
    pub worker: W,
    pub stdin: File,
    pub stdout: File,
}

/// How a wait for the worker ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wait {
    Exited,
    TimedOut,
    /// The wait itself failed.
    Failed,
}

/// A running contained worker. Dropping it must end anything left of it.
pub(crate) trait Worker {
    /// Waits up to `timeout` for the worker to exit.
    fn wait(&self, timeout: Duration) -> Wait;

    /// Ends the worker and anything in its containment, without waiting.
    fn kill(&self);

    /// Measurements after the worker exited (or was killed).
    fn stats(&self, elapsed: Duration) -> WorkerStats;
}

/// Launches a fresh contained worker, decodes `encoded`, and returns the
/// validated RGBA pixel frame with its EXIF orientation. The pixel frame keeps
/// its `WorkerOutput` budget reservation.
pub(crate) fn decode<L: Launcher>(
    launcher: &L,
    worker: &Path,
    encoded: &[u8],
    policy: &WorkerPolicy,
    budgets: &ResourceBudgets,
) -> Result<WorkerRun<DecodedImage>, LaunchError> {
    let (raw, stats) = run_worker(
        launcher,
        worker,
        WorkerOperation::Decode,
        encoded,
        policy,
        budgets,
    )?;
    let outcome = raw.and_then(|reply| classify_decode(reply, stats.exit_code));
    Ok(WorkerRun { outcome, stats })
}

/// Launches a fresh contained worker, strips the metadata from `encoded`, and
/// returns the stripped file as an `EncodedImage` frame. The frame keeps its
/// `EncodedInFlight` budget reservation and is never longer than `encoded`.
pub(crate) fn strip<L: Launcher>(
    launcher: &L,
    worker: &Path,
    encoded: &[u8],
    policy: &WorkerPolicy,
    budgets: &ResourceBudgets,
) -> Result<WorkerRun<ReceivedFrame>, LaunchError> {
    let (raw, stats) = run_worker(
        launcher,
        worker,
        WorkerOperation::Strip,
        encoded,
        policy,
        budgets,
    )?;
    let outcome = raw.and_then(|reply| classify_strip(reply, stats.exit_code, encoded.len()));
    Ok(WorkerRun { outcome, stats })
}

/// The frames read back from one worker. `second` is read only after a
/// decode's pixel frame, which must be followed by its pixel information.
struct Reply {
    first: Result<ReceivedFrame, FrameError>,
    second: Option<Result<ReceivedFrame, FrameError>>,
}

/// Launches a fresh contained worker, sends the operation and `encoded`, and
/// reads its reply. Every path out of this function leaves the worker
/// terminated: it is killed on deadline, and dropping it on return ends
/// anything left. The reply is classified by the caller after the worker has
/// exited.
fn run_worker<L: Launcher>(
    launcher: &L,
    worker: &Path,
    operation: WorkerOperation,
    encoded: &[u8],
    policy: &WorkerPolicy,
    budgets: &ResourceBudgets,
) -> Result<(Result<Reply, RunError>, WorkerStats), LaunchError> {
    let request = FrameHeader {
        kind: FrameKind::EncodedImage,
        length: encoded.len() as u64,
        width: 0,
        height: 0,
    };
    if FrameHeader::decode(request.encode()).is_err() {
        return Err(LaunchError::InputTooLarge);
    }
    let Spawned {
        worker: process,
        mut stdin,
        mut stdout,
    } = launcher.spawn(worker, policy)?;
    let started = Instant::now();

    let (wait, written, reply) = thread::scope(|scope| {
        let writer = scope.spawn(move || {
            let written = write_frame(&mut stdin, WorkerOperation::header(), &operation.encode())
                .and_then(|()| write_frame(&mut stdin, request, encoded))
                .is_ok();
            drop(stdin);
            written
        });
        let reader = scope.spawn(move || {
            let first = read_frame(&mut stdout, budgets);
            let pixels =
                matches!(&first, Ok(frame) if frame.header().kind == FrameKind::RgbaPixels);
            let second = (operation == WorkerOperation::Decode && pixels)
                .then(|| read_frame(&mut stdout, budgets));
            Reply { first, second }
        });
        let wait = match process.wait(policy.wall_deadline) {
            Wait::Exited => Wait::Exited,
            Wait::TimedOut => {
                process.kill();
                process.wait(EXIT_WAIT_AFTER_KILL);
                Wait::TimedOut
            }
            Wait::Failed => {
                process.kill();
                Wait::TimedOut
            }
        };
        // Both threads finish once the worker's pipe ends are closed.
        (wait, writer.join().unwrap_or(false), reader.join())
    });
    let stats = process.stats(started.elapsed());
    // Dropping the worker ends anything left in its containment.
    drop(process);

    let outcome = if wait == Wait::TimedOut {
        Err(RunError::DeadlineExceeded)
    } else {
        check_reply(reply, written, stats.exit_code)
    };
    Ok((outcome, stats))
}

/// Maps a missing or unreadable first frame to its error; the reply is kept
/// otherwise.
fn check_reply(
    reply: thread::Result<Reply>,
    written: bool,
    exit_code: u32,
) -> Result<Reply, RunError> {
    match reply {
        Ok(reply @ Reply { first: Ok(_), .. }) => Ok(reply),
        Ok(Reply {
            first: Err(FrameError::Io(_)),
            ..
        })
        | Err(_)
            if !written =>
        {
            Err(RunError::RequestWrite)
        }
        Ok(Reply {
            first: Err(FrameError::Io(_)),
            ..
        })
        | Err(_) => Err(RunError::AbnormalExit(exit_code)),
        Ok(Reply {
            first: Err(error), ..
        }) => Err(RunError::Protocol(error)),
    }
}

/// A control frame in first position must be a worker failure message.
fn failure(frame: &ReceivedFrame) -> RunError {
    match WorkerFailure::decode(frame.body()) {
        Some(failure) => RunError::Worker(failure),
        None => RunError::UnexpectedReply,
    }
}

fn classify_decode(reply: Reply, exit_code: u32) -> Result<DecodedImage, RunError> {
    let Reply { first, second } = reply;
    let pixels = first.map_err(RunError::Protocol)?;
    match pixels.header().kind {
        FrameKind::RgbaPixels if exit_code == 0 => {
            let info = match second {
                Some(Ok(info)) if info.header().kind == FrameKind::Control => {
                    PixelInfo::decode(info.body()).ok_or(RunError::UnexpectedReply)?
                }
                Some(Err(FrameError::Io(_))) | None | Some(Ok(_)) => {
                    return Err(RunError::UnexpectedReply);
                }
                Some(Err(error)) => return Err(RunError::Protocol(error)),
            };
            Ok(DecodedImage {
                pixels,
                orientation: info.orientation(),
            })
        }
        FrameKind::Control => Err(failure(&pixels)),
        _ => Err(RunError::UnexpectedReply),
    }
}

fn classify_strip(
    reply: Reply,
    exit_code: u32,
    input_len: usize,
) -> Result<ReceivedFrame, RunError> {
    let stripped = reply.first.map_err(RunError::Protocol)?;
    match stripped.header().kind {
        FrameKind::EncodedImage if exit_code == 0 && stripped.body().len() <= input_len => {
            Ok(stripped)
        }
        FrameKind::Control => Err(failure(&stripped)),
        _ => Err(RunError::UnexpectedReply),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use persephone_media_protocol::BudgetClass;
    use std::io::Cursor;

    fn frame(budgets: &ResourceBudgets, header: FrameHeader, body: &[u8]) -> ReceivedFrame {
        let mut wire = Vec::new();
        write_frame(&mut wire, header, body).unwrap();
        read_frame(&mut Cursor::new(wire), budgets).unwrap()
    }

    fn pixels(budgets: &ResourceBudgets) -> ReceivedFrame {
        let header = FrameHeader {
            kind: FrameKind::RgbaPixels,
            length: 8,
            width: 1,
            height: 2,
        };
        frame(budgets, header, &[9; 8])
    }

    fn encoded(budgets: &ResourceBudgets, length: usize) -> ReceivedFrame {
        let header = FrameHeader {
            kind: FrameKind::EncodedImage,
            length: length as u64,
            width: 0,
            height: 0,
        };
        frame(budgets, header, &vec![1; length])
    }

    fn control(budgets: &ResourceBudgets, header: FrameHeader, body: &[u8]) -> ReceivedFrame {
        frame(budgets, header, body)
    }

    fn reply(first: ReceivedFrame, second: Option<Result<ReceivedFrame, FrameError>>) -> Reply {
        Reply {
            first: Ok(first),
            second,
        }
    }

    #[test]
    fn given_pixels_and_pixel_info_then_decode_reply_carries_orientation_and_reservation() {
        let budgets = ResourceBudgets::default();
        let info = control(
            &budgets,
            PixelInfo::header(),
            &PixelInfo::new(6).unwrap().encode(),
        );
        let decoded = classify_decode(reply(pixels(&budgets), Some(Ok(info))), 0).unwrap();
        assert_eq!(decoded.orientation, 6);
        assert_eq!(decoded.pixels.body(), &[9; 8]);
        assert_eq!(budgets.class_used(BudgetClass::WorkerOutput), 8);
        // The pixel information frame is released once read.
        assert_eq!(budgets.class_used(BudgetClass::ControlQueue), 0);
        drop(decoded);
        assert_eq!(budgets.aggregate_used(), 0);
    }

    #[test]
    fn given_pixels_without_valid_pixel_info_then_decode_reply_is_refused() {
        let budgets = ResourceBudgets::default();
        let bad_info = [
            None,
            Some(Err(FrameError::Io(std::io::ErrorKind::UnexpectedEof))),
            Some(Ok(control(&budgets, PixelInfo::header(), &[1, 9]))),
            Some(Ok(control(
                &budgets,
                WorkerFailure::header(),
                &WorkerFailure::Trap.encode(),
            ))),
            Some(Ok(pixels(&budgets))),
        ];
        for second in bad_info {
            assert!(matches!(
                classify_decode(reply(pixels(&budgets), second), 0),
                Err(RunError::UnexpectedReply)
            ));
        }
        let info = control(
            &budgets,
            PixelInfo::header(),
            &PixelInfo::new(1).unwrap().encode(),
        );
        assert!(matches!(
            classify_decode(reply(pixels(&budgets), Some(Ok(info))), 1),
            Err(RunError::UnexpectedReply)
        ));
        assert_eq!(budgets.aggregate_used(), 0);
    }

    #[test]
    fn given_failure_or_wrong_kind_then_each_operation_reports_it() {
        let budgets = ResourceBudgets::default();
        let failed = || {
            control(
                &budgets,
                WorkerFailure::header(),
                &WorkerFailure::GuestRejected(8).encode(),
            )
        };
        assert!(matches!(
            classify_decode(reply(failed(), None), 1),
            Err(RunError::Worker(WorkerFailure::GuestRejected(8)))
        ));
        assert!(matches!(
            classify_strip(reply(failed(), None), 1, 100),
            Err(RunError::Worker(WorkerFailure::GuestRejected(8)))
        ));
        assert!(matches!(
            classify_decode(reply(encoded(&budgets, 4), None), 0),
            Err(RunError::UnexpectedReply)
        ));
        assert!(matches!(
            classify_strip(reply(pixels(&budgets), None), 0, 100),
            Err(RunError::UnexpectedReply)
        ));
        let unknown = control(&budgets, PixelInfo::header(), &[1, 1]);
        assert!(matches!(
            classify_strip(reply(unknown, None), 0, 100),
            Err(RunError::UnexpectedReply)
        ));
    }

    #[test]
    fn given_stripped_reply_then_it_must_not_be_longer_than_input_or_follow_a_failed_exit() {
        let budgets = ResourceBudgets::default();
        let stripped = classify_strip(reply(encoded(&budgets, 10), None), 0, 10).unwrap();
        assert_eq!(stripped.body().len(), 10);
        assert_eq!(budgets.class_used(BudgetClass::EncodedInFlight), 10);
        drop(stripped);
        assert!(matches!(
            classify_strip(reply(encoded(&budgets, 11), None), 0, 10),
            Err(RunError::UnexpectedReply)
        ));
        assert!(matches!(
            classify_strip(reply(encoded(&budgets, 4), None), 1, 10),
            Err(RunError::UnexpectedReply)
        ));
        assert_eq!(budgets.aggregate_used(), 0);
    }

    #[test]
    fn given_unreadable_first_frame_then_request_write_exit_and_protocol_errors_stay_distinct() {
        let io = || Reply {
            first: Err(FrameError::Io(std::io::ErrorKind::UnexpectedEof)),
            second: None,
        };
        assert!(matches!(
            check_reply(Ok(io()), false, 0),
            Err(RunError::RequestWrite)
        ));
        assert!(matches!(
            check_reply(Ok(io()), true, 5),
            Err(RunError::AbnormalExit(5))
        ));
        let protocol = Reply {
            first: Err(FrameError::TooLarge),
            second: None,
        };
        assert!(matches!(
            check_reply(Ok(protocol), true, 0),
            Err(RunError::Protocol(FrameError::TooLarge))
        ));
    }
}

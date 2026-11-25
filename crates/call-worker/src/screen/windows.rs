//! The Windows screen backend (ADR-0053, ADR-0067): the worker does not
//! capture the screen. Windows.Graphics.Capture is denied in the call
//! AppContainer, and GDI is no longer used here. The core starts a screen
//! helper outside the container for each share and gives this process the
//! read end of the helper's frame pipe (`ToWorker::ScreenFrames`, then
//! `SetScreen`). This backend reads the bounded items of that pipe
//! (`persephone_call_protocol::screen_frame`) and hands each picture to the
//! same frame callback that the other backends use, so publishing does not
//! change.
//!
//! The helper takes only the monitor that the user approved and writes at
//! most 5 pictures a second. The worker checks the pipe's handle, the share
//! number, the size, the stride and the length of every item before it
//! allocates or uses one; any bad item ends the share as `Unavailable`. The
//! open waits for the first item: a picture means `On`, an end item says
//! `NotFound` or `Unavailable`, and silence is `Unavailable`. The helper
//! says why it ends in its last item; a pipe that closes without one is a
//! failed helper.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use persephone_call_protocol::screen_frame::{ScreenEnd, ScreenFrameError, ScreenItem, read_item};
use persephone_call_protocol::{ScreenState, ScreenTarget};

use super::frame_pipe::{PipeReader, Wait};
use super::{CapturedFrame, Ended, Format, Grant};

/// The longest wait for the first item. It is shorter than the open's own
/// timeout, so the result is always this backend's.
const FIRST_ITEM_TIMEOUT: Duration = Duration::from_secs(8);
/// How long the loop waits for the next item before it looks at the stop
/// flag again.
const IDLE_WAIT: Duration = Duration::from_millis(100);

pub fn accepts(target: &ScreenTarget) -> bool {
    matches!(target, ScreenTarget::Monitor { .. })
}

/// The share state that an end item means at the start.
fn start_state(end: ScreenEnd) -> ScreenState {
    match end {
        ScreenEnd::Stopped => ScreenState::Off,
        ScreenEnd::NotFound => ScreenState::NotFound,
        ScreenEnd::Unavailable => ScreenState::Unavailable,
    }
}

/// How a running share ends for an end item.
fn ended_for(end: ScreenEnd) -> Ended {
    match end {
        ScreenEnd::Stopped => Ended::Source,
        ScreenEnd::NotFound => Ended::NotFound,
        ScreenEnd::Unavailable => Ended::Failed,
    }
}

/// Waits for the first item and says what the open is.
fn first_item(pipe: &mut PipeReader, share: u32) -> Result<ScreenItem, ScreenState> {
    match pipe.wait_for_data(FIRST_ITEM_TIMEOUT) {
        Wait::Data => {}
        // The share was stopped while it opened: the caller drops it.
        Wait::Stopped | Wait::Idle | Wait::Closed => return Err(ScreenState::Unavailable),
    }
    read_item(pipe, share).map_err(|_| ScreenState::Unavailable)
}

/// The thread body of a capture. Reports the result of opening once, then
/// delivers pictures until `stop`, an end item or an error.
pub fn capture<F, E>(
    target: ScreenTarget,
    grant: Grant,
    stop: &Arc<AtomicBool>,
    open_result: Sender<Result<(), ScreenState>>,
    mut on_frame: F,
    on_end: E,
) where
    F: FnMut(CapturedFrame<'_>) + Send + 'static,
    E: FnOnce(Ended) + Send,
{
    let Grant { share, frames } = grant;
    let fail = |state: ScreenState| {
        let _ = open_result.send(Err(state));
    };
    if !accepts(&target) {
        return fail(ScreenState::Unavailable);
    }
    // No pipe came with the start, or the handle is not a pipe: nothing is
    // read, and no other source is tried.
    let Some(frames) = frames else {
        return fail(ScreenState::Unavailable);
    };
    let Ok(mut pipe) = PipeReader::new(frames, stop.clone()) else {
        return fail(ScreenState::Unavailable);
    };
    let first = match first_item(&mut pipe, share) {
        Ok(ScreenItem::Frame(frame)) => frame,
        Ok(ScreenItem::End(end)) => return fail(start_state(end)),
        Err(state) => return fail(state),
    };
    let _ = open_result.send(Ok(()));
    drop(open_result);
    let deliver = |on_frame: &mut F, width: u32, height: u32, data: &[u8]| {
        if let Some(frame) = CapturedFrame::checked(Format::Bgra, width, height, width * 4, data) {
            on_frame(frame);
        }
    };
    deliver(&mut on_frame, first.width, first.height, &first.data);
    drop(first);
    let ended = loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match pipe.wait_for_data(IDLE_WAIT) {
            Wait::Idle => continue,
            Wait::Stopped => return,
            // The helper went without an end item: it failed or was killed.
            Wait::Closed => break Ended::Failed,
            Wait::Data => {}
        }
        match read_item(&mut pipe, share) {
            Ok(ScreenItem::Frame(frame)) => {
                deliver(&mut on_frame, frame.width, frame.height, &frame.data);
            }
            Ok(ScreenItem::End(end)) => break ended_for(end),
            // The stop flag cut the read short: no report.
            Err(ScreenFrameError::Io(std::io::ErrorKind::TimedOut))
                if stop.load(Ordering::Acquire) =>
            {
                return;
            }
            Err(_) => break Ended::Failed,
        }
    };
    // Close the pipe before the end is reported: the helper then ends too.
    drop(pipe);
    if !stop.load(Ordering::Acquire) {
        on_end(ended);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use persephone_call_protocol::screen_frame::{write_end, write_picture};
    use persephone_call_protocol::{DesktopRect, MonitorName};
    use std::fs::File;
    use std::io::Write;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::sync::Mutex;
    use std::sync::mpsc;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::CreatePipe;

    fn monitor() -> ScreenTarget {
        ScreenTarget::Monitor {
            name: MonitorName::new(br"\\.\DISPLAY1").expect("a name"),
            width: 4,
            height: 2,
            rect: DesktopRect::new(0, 0, 4, 2).expect("a rectangle"),
        }
    }

    fn pipe() -> (OwnedHandle, File) {
        let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
        // SAFETY: both out values are valid; each handle is owned below.
        unsafe { CreatePipe(&mut read, &mut write, None, 0) }.expect("a pipe");
        // SAFETY: both handles were just made and belong to nobody else.
        unsafe {
            (
                OwnedHandle::from_raw_handle(read.0),
                File::from_raw_handle(write.0),
            )
        }
    }

    /// Everything a capture reported.
    #[derive(Default)]
    struct Seen {
        frames: Vec<(u32, u32, usize)>,
        ended: Option<Ended>,
    }

    /// Runs `capture` on its own thread over a pipe and gives the open
    /// result, what was seen, and the write end.
    fn run(
        share: u32,
        frames: Option<OwnedHandle>,
        target: ScreenTarget,
    ) -> (
        mpsc::Receiver<Result<(), ScreenState>>,
        Arc<Mutex<Seen>>,
        Arc<AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        let (open_tx, open_rx) = mpsc::channel();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (frame_seen, end_seen, flag) = (seen.clone(), seen.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            capture(
                target,
                Grant::from_frames(share, frames),
                &flag,
                open_tx,
                move |frame| {
                    frame_seen.lock().unwrap().frames.push((
                        frame.width,
                        frame.height,
                        frame.data.len(),
                    ));
                },
                move |ended| end_seen.lock().unwrap().ended = Some(ended),
            );
        });
        (open_rx, seen, stop, thread)
    }

    fn open_result(rx: &mpsc::Receiver<Result<(), ScreenState>>) -> Result<(), ScreenState> {
        rx.recv_timeout(Duration::from_secs(10))
            .expect("an open result")
    }

    /// GIVEN a pipe with pictures and an end item WHEN the backend runs THEN
    /// it opens on the first picture, delivers each picture as a BGRA frame
    /// and reports the end item's meaning.
    #[test]
    fn given_good_items_when_captured_then_frames_are_delivered_and_the_end_is_reported() {
        let (read, mut write) = pipe();
        let (open, seen, _stop, thread) = run(9, Some(read), monitor());
        write_picture(&mut write, 9, 4, 2, &[1; 32]).expect("a picture");
        assert_eq!(open_result(&open), Ok(()));
        write_picture(&mut write, 9, 4, 2, &[2; 32]).expect("a picture");
        write_end(&mut write, 9, ScreenEnd::NotFound).expect("an end");
        thread.join().expect("the capture thread");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.frames, vec![(4, 2, 32); 2]);
        assert_eq!(seen.ended, Some(Ended::NotFound));
    }

    /// GIVEN no pipe, a handle that is not a pipe or a target of another
    /// kind WHEN the backend runs THEN the open is `Unavailable` and nothing
    /// is read.
    #[test]
    fn given_no_pipe_or_a_file_when_captured_then_the_open_is_unavailable() {
        let (open, seen, _stop, thread) = run(1, None, monitor());
        assert_eq!(open_result(&open), Err(ScreenState::Unavailable));
        thread.join().expect("the capture thread");
        let file = File::open(std::env::current_exe().expect("this program")).expect("a file");
        // SAFETY: the backend gives a refused handle back without closing it,
        // so the file's own owner closes it once.
        let alias = unsafe {
            OwnedHandle::from_raw_handle(std::os::windows::io::AsRawHandle::as_raw_handle(&file))
        };
        let (open, _seen, _stop, thread) = run(1, Some(alias), monitor());
        assert_eq!(open_result(&open), Err(ScreenState::Unavailable));
        thread.join().expect("the capture thread");
        let (read, _write) = pipe();
        let display = ScreenTarget::Display {
            id: 1,
            width: 4,
            height: 2,
        };
        let (open, _seen, _stop, thread) = run(1, Some(read), display);
        assert_eq!(open_result(&open), Err(ScreenState::Unavailable));
        thread.join().expect("the capture thread");
        assert!(seen.lock().unwrap().frames.is_empty());
    }

    /// GIVEN an end item as the first item WHEN the backend opens THEN the
    /// open is that item's state: not found, unavailable or off.
    #[test]
    fn given_an_end_item_first_when_opened_then_its_state_is_the_result() {
        for (end, state) in [
            (ScreenEnd::NotFound, ScreenState::NotFound),
            (ScreenEnd::Unavailable, ScreenState::Unavailable),
            (ScreenEnd::Stopped, ScreenState::Off),
        ] {
            let (read, mut write) = pipe();
            write_end(&mut write, 2, end).expect("an end");
            let (open, _seen, _stop, thread) = run(2, Some(read), monitor());
            assert_eq!(open_result(&open), Err(state), "{end:?}");
            thread.join().expect("the capture thread");
        }
    }

    /// GIVEN bad first bytes, a picture of another share or a pipe that
    /// closes with nothing WHEN the backend opens THEN the open is
    /// unavailable.
    #[test]
    fn given_bad_first_bytes_when_opened_then_the_open_is_unavailable() {
        let (read, mut write) = pipe();
        write.write_all(&[0xEE; 64]).expect("garbage");
        let (open, _seen, _stop, thread) = run(2, Some(read), monitor());
        assert_eq!(open_result(&open), Err(ScreenState::Unavailable));
        thread.join().expect("the capture thread");
        let (read, mut write) = pipe();
        write_picture(&mut write, 3, 4, 2, &[0; 32]).expect("a picture");
        let (open, _seen, _stop, thread) = run(2, Some(read), monitor());
        assert_eq!(open_result(&open), Err(ScreenState::Unavailable));
        thread.join().expect("the capture thread");
        let (read, write) = pipe();
        drop(write);
        let (open, _seen, _stop, thread) = run(2, Some(read), monitor());
        assert_eq!(open_result(&open), Err(ScreenState::Unavailable));
        thread.join().expect("the capture thread");
    }

    /// GIVEN a running share WHEN a bad item comes, the pipe closes without
    /// an end item, or the stop flag is set THEN a bad item or a closed pipe
    /// ends the share as failed, and a stop ends it with no report.
    #[test]
    fn given_a_running_share_when_it_goes_wrong_or_stops_then_the_end_is_right() {
        let (read, mut write) = pipe();
        let (open, seen, _stop, thread) = run(5, Some(read), monitor());
        write_picture(&mut write, 5, 4, 2, &[0; 32]).expect("a picture");
        assert_eq!(open_result(&open), Ok(()));
        write_picture(&mut write, 6, 4, 2, &[0; 32]).expect("another share");
        thread.join().expect("the capture thread");
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.frames.len(), 1, "the other share's picture is dropped");
            assert_eq!(seen.ended, Some(Ended::Failed));
        }

        let (read, mut write) = pipe();
        let (open, seen, _stop, thread) = run(5, Some(read), monitor());
        write_picture(&mut write, 5, 4, 2, &[0; 32]).expect("a picture");
        assert_eq!(open_result(&open), Ok(()));
        drop(write);
        thread.join().expect("the capture thread");
        assert_eq!(seen.lock().unwrap().ended, Some(Ended::Failed));

        let (read, mut write) = pipe();
        let (open, seen, stop, thread) = run(5, Some(read), monitor());
        write_picture(&mut write, 5, 4, 2, &[0; 32]).expect("a picture");
        assert_eq!(open_result(&open), Ok(()));
        stop.store(true, Ordering::Release);
        thread.join().expect("the capture thread");
        assert_eq!(seen.lock().unwrap().ended, None, "a stop reports nothing");
    }

    /// GIVEN each end item WHEN mapped THEN it means the state and the end
    /// that the helper's reason says.
    #[test]
    fn given_end_items_when_mapped_then_they_mean_their_reason() {
        assert_eq!(start_state(ScreenEnd::Stopped), ScreenState::Off);
        assert_eq!(ended_for(ScreenEnd::Stopped), Ended::Source);
        assert_eq!(
            ended_for(ScreenEnd::NotFound).state(),
            ScreenState::NotFound
        );
        assert_eq!(
            ended_for(ScreenEnd::Unavailable).state(),
            ScreenState::Unavailable
        );
        assert!(accepts(&monitor()));
        assert!(!accepts(&ScreenTarget::Portal { node: 1 }));
    }
}

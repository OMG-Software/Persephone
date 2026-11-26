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
//!
//! The helper sends a picture only when the screen changed, so a picture
//! that is dropped can stay the last one for a long time. This backend
//! never drops the newest: it hands pictures on at most once for each
//! [`PICTURE_GAP`], counted from the end of the last hand-on, a little more
//! than the worker's screen limit, and keeps the newest one that came too
//! soon until its time comes. While the screen stays still, it hands the
//! newest picture on again every [`REPEAT_GAP`], for the whole share. The
//! feed drops pictures until the track is published, so the first one of a
//! still screen would otherwise never go. And the encoder makes a picture
//! for a remote that subscribes later, or that lost one, only from a new
//! input picture; whether it would send the last one again on its own was
//! not measured, so the repeat does not stop after the start. Its cost, one
//! encode of an unchanged picture a second, was not measured either. Two
//! buffers serve the whole share; a picture of the
//! same size is read into one without a new allocation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use persephone_call_protocol::screen_frame::{
    ScreenEnd, ScreenFrameError, ScreenRead, read_item_into,
};
use persephone_call_protocol::{MAX_SCREEN_FPS, ScreenState, ScreenTarget};

use super::frame_pipe::{PipeReader, STALL_TIMEOUT, Wait};
use super::{CapturedFrame, Ended, Format, Grant, OPEN_TIMEOUT};

/// The longest wait for the first item. With the item's own
/// [`STALL_TIMEOUT`] (3 s) it stays 1 s under the open's own timeout
/// ([`OPEN_TIMEOUT`], 10 s, counted from before this thread starts), so the
/// result is always this backend's.
const FIRST_ITEM_TIMEOUT: Duration = Duration::from_secs(6);
const _: () = assert!(
    FIRST_ITEM_TIMEOUT.as_millis() + STALL_TIMEOUT.as_millis() + 1000 <= OPEN_TIMEOUT.as_millis()
);
/// How long the loop waits for the next item before it looks at the stop
/// flag again.
const IDLE_WAIT: Duration = Duration::from_millis(100);
/// The least time between two pictures that this backend hands on: the
/// worker's screen limit and 20 ms more. The limiter of the screen feed
/// drops a picture that comes too soon; with this gap it never has to, so
/// the newest picture is never lost there.
pub const PICTURE_GAP: Duration = Duration::from_millis(1000 / MAX_SCREEN_FPS as u64 + 20);
/// How often the newest picture goes again while no new one comes, for the
/// whole share (see the module documentation for why it never stops).
pub const REPEAT_GAP: Duration = Duration::from_secs(1);

/// When the last hand-on ended. The feed's limiter runs inside the
/// callback, so a gap counted from the end of the callback is never shorter
/// at the limiter, however long the callback took to reach it.
struct Pace {
    last: Instant,
}

impl Pace {
    fn due(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last) >= PICTURE_GAP
    }

    fn repeat_due(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last) >= REPEAT_GAP
    }

    /// How long until the next picture may go.
    fn left(&self, now: Instant) -> Duration {
        PICTURE_GAP.saturating_sub(now.saturating_duration_since(self.last))
    }
}

/// The newest picture: its size, and whether it still waits for its first
/// hand-on (it came too soon) or went already (it goes again while the
/// screen stays still).
struct Kept {
    size: (u32, u32),
    waiting: bool,
}

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

/// Waits for the first item and says what the open is. A picture goes into
/// `buffer`.
fn first_item(
    pipe: &mut PipeReader,
    share: u32,
    buffer: &mut Vec<u8>,
) -> Result<ScreenRead, ScreenState> {
    match pipe.wait_for_data(FIRST_ITEM_TIMEOUT) {
        Wait::Data => {}
        // The share was stopped while it opened: the caller drops it.
        Wait::Stopped | Wait::Idle | Wait::Closed => return Err(ScreenState::Unavailable),
    }
    pipe.begin_item();
    read_item_into(pipe, share, buffer).map_err(|_| ScreenState::Unavailable)
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
    // The picture being read, and the newest one (`kept`).
    let mut buffer = Vec::new();
    let mut newest = Vec::new();
    let first = match first_item(&mut pipe, share, &mut buffer) {
        Ok(ScreenRead::Frame { width, height }) => (width, height),
        Ok(ScreenRead::End(end)) => return fail(start_state(end)),
        Err(state) => return fail(state),
    };
    let _ = open_result.send(Ok(()));
    drop(open_result);
    // Hands a picture on and says when that ended. `read_item_into` gave
    // exactly `height * width * 4` bytes of a size within the limits, so
    // `checked` refuses no picture here; the feed's own limits apply in
    // `on_frame`.
    let deliver = |on_frame: &mut F, (width, height): (u32, u32), data: &[u8]| {
        if let Some(frame) = CapturedFrame::checked(Format::Bgra, width, height, width * 4, data) {
            on_frame(frame);
        }
        Instant::now()
    };
    let mut pace = Pace {
        last: deliver(&mut on_frame, first, &buffer),
    };
    std::mem::swap(&mut buffer, &mut newest);
    let mut kept = Kept {
        size: first,
        waiting: false,
    };
    let ended = loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        let due = if kept.waiting {
            pace.due(now)
        } else {
            pace.repeat_due(now)
        };
        if due {
            kept.waiting = false;
            pace.last = deliver(&mut on_frame, kept.size, &newest);
        }
        // With a picture waiting, look again when it is due.
        let now = Instant::now();
        let wait = if kept.waiting {
            pace.left(now).clamp(Duration::from_millis(1), IDLE_WAIT)
        } else {
            IDLE_WAIT
        };
        match pipe.wait_for_data(wait) {
            Wait::Idle => continue,
            Wait::Stopped => return,
            // The helper went without an end item: it failed or was killed.
            Wait::Closed => break Ended::Failed,
            Wait::Data => {}
        }
        // The whole item has `STALL_TIMEOUT`, however its bytes are spread.
        pipe.begin_item();
        match read_item_into(&mut pipe, share, &mut buffer) {
            Ok(ScreenRead::Frame { width, height }) => {
                // The newer picture takes the older one's place, also one
                // that still waits: that one never goes. The top of the loop
                // hands it on when it is due.
                std::mem::swap(&mut buffer, &mut newest);
                kept = Kept {
                    size: (width, height),
                    waiting: true,
                };
            }
            Ok(ScreenRead::End(end)) => break ended_for(end),
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
        /// The first byte of each picture, and when it came.
        fills: Vec<(u8, Instant)>,
        ended: Option<Ended>,
    }

    /// The open result, what was seen, the stop flag and the thread.
    type Started = (
        mpsc::Receiver<Result<(), ScreenState>>,
        Arc<Mutex<Seen>>,
        Arc<AtomicBool>,
        std::thread::JoinHandle<()>,
    );

    /// Runs `capture` on its own thread over a pipe and gives the open
    /// result, what was seen, and the write end.
    fn run(share: u32, frames: Option<OwnedHandle>, target: ScreenTarget) -> Started {
        run_slow(share, frames, target, 0, Duration::ZERO)
    }

    /// As [`run`], but callback number `slow_one` (from 0) waits `slow`
    /// before it notes its picture, as a callback whose thread paused before
    /// the feed's limiter.
    fn run_slow(
        share: u32,
        frames: Option<OwnedHandle>,
        target: ScreenTarget,
        slow_one: usize,
        slow: Duration,
    ) -> Started {
        let (open_tx, open_rx) = mpsc::channel();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (frame_seen, end_seen, flag) = (seen.clone(), seen.clone(), stop.clone());
        let mut calls = 0_usize;
        let thread = std::thread::spawn(move || {
            capture(
                target,
                Grant::from_frames(share, frames),
                &flag,
                open_tx,
                move |frame| {
                    if calls == slow_one {
                        std::thread::sleep(slow);
                    }
                    calls += 1;
                    let mut seen = frame_seen.lock().unwrap();
                    seen.frames
                        .push((frame.width, frame.height, frame.data.len()));
                    seen.fills.push((frame.data[0], Instant::now()));
                },
                move |ended| end_seen.lock().unwrap().ended = Some(ended),
            );
        });
        (open_rx, seen, stop, thread)
    }

    /// Waits until `count` pictures were seen, at most 5 s.
    fn wait_for_fills(seen: &Mutex<Seen>, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while seen.lock().unwrap().fills.len() < count {
            assert!(Instant::now() < deadline, "{count} pictures go");
            std::thread::sleep(Duration::from_millis(10));
        }
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
        // The next picture of a changed screen comes after the gap.
        std::thread::sleep(PICTURE_GAP + Duration::from_millis(50));
        write_picture(&mut write, 9, 4, 2, &[2; 32]).expect("a picture");
        write_end(&mut write, 9, ScreenEnd::NotFound).expect("an end");
        thread.join().expect("the capture thread");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.frames, vec![(4, 2, 32); 2]);
        assert_eq!(seen.ended, Some(Ended::NotFound));
    }

    /// GIVEN pictures that come faster than the worker's limit WHEN the
    /// screen then stays still THEN the newest of them still goes once its
    /// time has come, no older one goes after a newer one, and none comes
    /// sooner than the gap: the last picture of a burst is not lost. Which
    /// pictures in between go depends on when the thread reads them.
    #[test]
    fn given_a_burst_of_pictures_when_the_screen_settles_then_the_newest_goes() {
        let (read, mut write) = pipe();
        let (open, seen, stop, thread) = run(9, Some(read), monitor());
        write_picture(&mut write, 9, 4, 2, &[1; 32]).expect("a picture");
        assert_eq!(open_result(&open), Ok(()));
        for fill in [2, 3, 4] {
            write_picture(&mut write, 9, 4, 2, &[fill; 32]).expect("a picture");
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while seen.lock().unwrap().fills.last().map(|(fill, _)| *fill) != Some(4) {
            assert!(Instant::now() < deadline, "the newest picture goes");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Nothing else waits, so nothing else comes before the repeat.
        std::thread::sleep(PICTURE_GAP * 2);
        stop.store(true, Ordering::Release);
        thread.join().expect("the capture thread");
        let seen = seen.lock().unwrap();
        let fills: Vec<u8> = seen.fills.iter().map(|(fill, _)| *fill).collect();
        assert_eq!(fills.first(), Some(&1));
        assert_eq!(fills.last(), Some(&4), "the newest picture goes last");
        assert!(
            fills.windows(2).all(|pair| pair[0] < pair[1]),
            "no older picture after a newer one: {fills:?}"
        );
        assert!(
            seen.fills
                .windows(2)
                .all(|pair| pair[1].1.duration_since(pair[0].1) >= PICTURE_GAP),
            "none sooner than the gap"
        );
    }

    /// GIVEN one picture WHEN the screen then stays still THEN the picture
    /// goes again after the repeat gap, so a picture that the feed dropped
    /// before the track was published still reaches the call.
    #[test]
    fn given_a_still_screen_when_time_passes_then_the_newest_picture_goes_again() {
        let (read, mut write) = pipe();
        let (open, seen, stop, thread) = run(9, Some(read), monitor());
        write_picture(&mut write, 9, 4, 2, &[7; 32]).expect("a picture");
        assert_eq!(open_result(&open), Ok(()));
        wait_for_fills(&seen, 2);
        stop.store(true, Ordering::Release);
        thread.join().expect("the capture thread");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.fills[0].0, 7);
        assert_eq!(seen.fills[1].0, 7);
        assert!(seen.fills[1].1.duration_since(seen.fills[0].1) >= REPEAT_GAP);
        assert_eq!(seen.ended, None, "a stop reports nothing");
    }

    /// GIVEN a callback that pauses before the feed's limiter could see its
    /// picture WHEN the next picture waits THEN it goes at least the
    /// worker's screen limit after the slow picture was seen, so the limiter
    /// does not drop it.
    #[test]
    fn given_a_slow_callback_when_the_next_picture_waits_then_the_limiter_gap_holds() {
        let limit = Duration::from_millis(1000 / u64::from(MAX_SCREEN_FPS));
        let slow = Duration::from_millis(80);
        let (read, mut write) = pipe();
        let (open, seen, stop, thread) = run_slow(9, Some(read), monitor(), 1, slow);
        write_picture(&mut write, 9, 4, 2, &[1; 32]).expect("a picture");
        assert_eq!(open_result(&open), Ok(()));
        std::thread::sleep(PICTURE_GAP + Duration::from_millis(50));
        // Picture 2 goes at once and its callback is slow; picture 3 waits.
        write_picture(&mut write, 9, 4, 2, &[2; 32]).expect("a picture");
        write_picture(&mut write, 9, 4, 2, &[3; 32]).expect("a picture");
        wait_for_fills(&seen, 3);
        stop.store(true, Ordering::Release);
        thread.join().expect("the capture thread");
        let seen = seen.lock().unwrap();
        let fills: Vec<u8> = seen.fills.iter().map(|(fill, _)| *fill).collect();
        assert_eq!(fills, [1, 2, 3]);
        assert!(seen.fills[2].1.duration_since(seen.fills[1].1) >= limit);
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

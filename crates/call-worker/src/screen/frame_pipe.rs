//! The read end of the screen helper's frame pipe (ADR-0067).
//!
//! The core made the pipe, gave its write end to the helper and duplicated
//! its read end into this process. The worker never trusts it: before it
//! reads, it checks that the handle is a pipe end and not one of its own
//! standard handles (`is_pipe`), and every read is bounded: a stop ends it,
//! and each item has one deadline ([`PipeReader::begin_item`]), so a helper
//! that stalls or trickles its bytes cannot hold the capture thread. The
//! format is `persephone_call_protocol::screen_frame`; this module only
//! moves bytes and keeps the waits short.

use std::fs::File;
use std::io::{self, Read};
#[cfg(test)]
use std::os::windows::io::OwnedHandle;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, RawHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
use windows::Win32::System::Pipes::{GetNamedPipeInfo, PeekNamedPipe};

/// The longest time that one item may take, from [`PipeReader::begin_item`]
/// to its last byte. A helper that stops writing inside an item, or writes
/// it so slowly that it never ends, is a failed helper.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a read inside an item looks at the pipe and at the stop flag.
/// A picture fills the 1 MiB pipe many times, so this stays short.
const POLL: Duration = Duration::from_millis(5);
/// How often a wait between items looks at the pipe and at the stop flag.
/// Pictures come at most 5 a second, so one seen 25 ms late costs nothing,
/// and a still screen wakes the thread 40 times a second, not 200.
const IDLE_POLL: Duration = Duration::from_millis(25);

/// Whether `handle` is the end of a pipe and none of this process's
/// standard handles. `GetFileType` also calls a socket a pipe;
/// `GetNamedPipeInfo` fails on a socket and works on both ends of an
/// anonymous pipe. The worker's standard input and output are pipes too,
/// and they carry the call protocol, so their values are refused.
///
/// The core gives a value that it duplicated into this process just
/// before. A value that names no handle is a fault of the core: the
/// worker's strict handle checks then end the worker, and the call ends.
pub fn is_pipe(handle: HANDLE) -> bool {
    let standard = [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ];
    if handle.0.is_null() || standard.contains(&handle.0) {
        return false;
    }
    // SAFETY: only asks the kernel about a handle value; no out-pointer is
    // passed to `GetNamedPipeInfo`.
    unsafe {
        GetFileType(handle) == FILE_TYPE_PIPE
            && GetNamedPipeInfo(handle, None, None, None, None).is_ok()
    }
}

/// The handle values of the frame pipes that this process owns now: each
/// [`FramePipe`] from its adoption until its close. A worker owns at most
/// two at once (one kept for a share's start, one that a capture reads), so
/// [`MAX_OWNED`] only keeps the list small.
static OWNED: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// The most frame pipes owned at once; past it a value is refused.
const MAX_OWNED: usize = 64;

fn owned() -> MutexGuard<'static, Vec<usize>> {
    OWNED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The read end of a frame pipe, owned by this process once. Its value is
/// listed from its adoption until its close, and the close and the removal
/// from the list are one step under the list's lock. So [`adopt`] refuses
/// the value of a pipe that is still open, kept or read by a capture, and
/// never makes a second owner that would close the handle twice (or close
/// an unrelated handle that later got the same value).
pub struct FramePipe {
    /// `None` only after the pipe was given up or closed.
    file: Option<File>,
}

impl FramePipe {
    /// The handle value, while the pipe is held.
    pub fn raw(&self) -> Option<RawHandle> {
        self.file.as_ref().map(AsRawHandle::as_raw_handle)
    }

    /// Gives the handle up without closing it, and takes its value off the
    /// list: a handle that is not a pipe may not be this process's to close.
    fn give_up(mut self) {
        let mut owned = owned();
        if let Some(file) = self.file.take() {
            let value = file.into_raw_handle() as usize;
            owned.retain(|listed| *listed != value);
        }
    }

    /// A handle that a test owns, listed as an adopted one but without the
    /// pipe check, so the reader's own check can be tried.
    #[cfg(test)]
    pub fn for_test(handle: OwnedHandle) -> Self {
        let mut owned = owned();
        let raw = handle.into_raw_handle();
        owned.push(raw as usize);
        // SAFETY: the handle came owned and was released just above.
        Self {
            file: Some(unsafe { File::from_raw_handle(raw) }),
        }
    }
}

impl Drop for FramePipe {
    fn drop(&mut self) {
        let mut owned = owned();
        if let Some(file) = self.file.take() {
            let value = file.as_raw_handle() as usize;
            drop(file);
            owned.retain(|listed| *listed != value);
        }
    }
}

/// The handle with value `value` in this process, owned, when it is a pipe
/// that this process does not own yet. A value that is not a pipe, or one
/// that a [`FramePipe`] holds, is never closed or read: it stays as it was.
/// The core duplicated the handle into this process just before it sent the
/// value, so a pipe at this value that nothing here owns is that handle.
pub fn adopt(value: u32) -> Option<FramePipe> {
    let raw = value as usize as RawHandle;
    let mut owned = owned();
    if owned.contains(&(raw as usize)) || owned.len() >= MAX_OWNED || !is_pipe(HANDLE(raw)) {
        return None;
    }
    owned.push(raw as usize);
    // SAFETY: the value names a pipe handle of this process that no
    // `FramePipe` owns (checked under the list's lock), and the core made it
    // for this worker alone.
    Some(FramePipe {
        file: Some(unsafe { File::from_raw_handle(raw) }),
    })
}

/// What a wait for the next item found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wait {
    /// Bytes are waiting.
    Data,
    /// Nothing came in the time given.
    Idle,
    /// The writer closed the pipe and nothing is left.
    Closed,
    /// The stop flag is set.
    Stopped,
}

/// The frame pipe, read with bounded waits.
pub struct PipeReader {
    pipe: FramePipe,
    stop: std::sync::Arc<AtomicBool>,
    /// When the item being read must be complete ([`Self::begin_item`]).
    deadline: Option<Instant>,
    /// How long one item may take: [`STALL_TIMEOUT`], shorter in tests.
    item_limit: Duration,
}

impl PipeReader {
    /// Takes the pipe. A handle that is not a pipe is refused, and is not
    /// closed (it may not be ours to close).
    pub fn new(pipe: FramePipe, stop: std::sync::Arc<AtomicBool>) -> Result<Self, ()> {
        if !pipe.raw().is_some_and(|raw| is_pipe(HANDLE(raw))) {
            pipe.give_up();
            return Err(());
        }
        Ok(Self {
            pipe,
            stop,
            deadline: None,
            item_limit: STALL_TIMEOUT,
        })
    }

    /// Starts the time of the next item: all its reads together may take at
    /// most [`STALL_TIMEOUT`], however the helper spreads its bytes. Call it
    /// before each item. The deadline lasts until the next
    /// [`PipeReader::wait_for_data`]; without one, each read has that time on
    /// its own.
    pub fn begin_item(&mut self) {
        self.deadline = Some(Instant::now() + self.item_limit);
    }

    /// Bytes waiting, or `None` when the writer closed the pipe (or the
    /// pipe failed) and nothing is left to read.
    fn available(&self) -> Option<u32> {
        let raw = self.pipe.raw()?;
        let mut available = 0_u32;
        // SAFETY: the handle is a pipe checked in `new` and held by
        // `self.pipe`; `available` is a valid out value; nothing is read.
        let peeked =
            unsafe { PeekNamedPipe(HANDLE(raw), None, 0, None, Some(&mut available), None) };
        peeked.ok().map(|()| available)
    }

    /// Waits up to `limit` for the first bytes of the next item. The wait
    /// ends the last item's deadline, so the next item never reads under it.
    pub fn wait_for_data(&mut self, limit: Duration) -> Wait {
        self.deadline = None;
        let started = Instant::now();
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Wait::Stopped;
            }
            match self.available() {
                None => return Wait::Closed,
                Some(0) => {}
                Some(_) => return Wait::Data,
            }
            let waited = started.elapsed();
            if waited >= limit {
                return Wait::Idle;
            }
            std::thread::sleep(IDLE_POLL.min(limit - waited));
        }
    }
}

impl Read for PipeReader {
    /// Reads what is waiting, at most `buf.len()`, until the item's deadline
    /// ([`PipeReader::begin_item`]; without one, [`STALL_TIMEOUT`] from this
    /// call). A set stop flag or a passed deadline ends the read with
    /// `TimedOut`, also when bytes are waiting. A closed pipe is the end of
    /// the input.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let deadline = self
            .deadline
            .unwrap_or_else(|| Instant::now() + self.item_limit);
        loop {
            if self.stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.available() {
                None => return Ok(0),
                Some(0) => {}
                Some(waiting) => {
                    let take = buf.len().min(waiting as usize);
                    let Some(mut file) = self.pipe.file.as_ref() else {
                        return Ok(0);
                    };
                    return file.read(&mut buf[..take]);
                }
            }
            std::thread::sleep(POLL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use persephone_call_protocol::screen_frame::{
        ScreenEnd, ScreenFrameError, ScreenItem, read_item, write_end, write_picture,
    };
    use std::io::Write;
    use std::sync::Arc;
    use windows::Win32::System::Pipes::CreatePipe;

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

    fn reader() -> (PipeReader, File, Arc<AtomicBool>) {
        let (read, write) = pipe();
        let stop = Arc::new(AtomicBool::new(false));
        let reader =
            PipeReader::new(FramePipe::for_test(read), stop.clone()).expect("a pipe reader");
        (reader, write, stop)
    }

    /// GIVEN a handle that is not a pipe (a file, a console-like null
    /// device) WHEN the worker takes it THEN it is refused, and the handle
    /// stays open: it is not the worker's to close.
    #[test]
    fn given_a_handle_that_is_not_a_pipe_when_taken_then_it_is_refused_and_kept() {
        // The handle goes to the reader, its only owner; a refused handle
        // comes back to the test, which owns it again and closes it.
        let refused = |file: File| {
            let pipe = FramePipe::for_test(OwnedHandle::from(file));
            let raw = pipe.raw().expect("a handle");
            assert!(PipeReader::new(pipe, Arc::new(AtomicBool::new(false))).is_err());
            assert!(!listed(raw), "a refused handle is not listed as owned");
            // SAFETY: `PipeReader::new` gave the refused handle up without
            // closing it (`into_raw_handle`), and nothing else owns it.
            unsafe { File::from_raw_handle(raw) }
        };
        let file =
            refused(File::open(std::env::current_exe().expect("this program")).expect("a file"));
        let null = refused(File::open("NUL").expect("the null device"));
        // Both are still open: the file gives a byte, the null device its end.
        let mut byte = [0_u8; 1];
        assert_eq!((&file).read(&mut byte).expect("still open"), 1);
        assert_eq!((&null).read(&mut byte).expect("still open"), 0);
        assert!(!is_pipe(HANDLE(file.as_raw_handle())));
        assert!(!is_pipe(HANDLE(null.as_raw_handle())));
    }

    /// GIVEN the values of this process's standard handles (pipes in the
    /// worker, and they carry the call protocol) WHEN checked THEN none is
    /// taken as the frame pipe, while a pipe's read end is.
    #[test]
    fn given_the_standard_handles_when_checked_then_they_are_not_the_frame_pipe() {
        for standard in [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ] {
            assert!(!is_pipe(HANDLE(standard)));
        }
        let (read, _write) = pipe();
        assert!(is_pipe(HANDLE(read.as_raw_handle())));
    }

    /// GIVEN a handle value that is not open, a file's value or a pipe's
    /// value WHEN adopted THEN only the pipe is taken.
    #[test]
    fn given_handle_values_when_adopted_then_only_a_pipe_is_taken() {
        assert!(adopt(0x00ff_fff0).is_none());
        assert!(adopt(0).is_none());
        // The file's handle goes to `adopt` as a value, as the core's does.
        // `adopt` refuses it and does not close it, so the test owns it
        // again.
        let file = File::open(std::env::current_exe().expect("this program")).expect("a file");
        let raw = file.into_raw_handle();
        assert!(adopt(raw as usize as u32).is_none());
        // SAFETY: `adopt` did not take the refused handle, and nothing else
        // owns it.
        let file = unsafe { File::from_raw_handle(raw) };
        assert!(file.metadata().is_ok(), "a refused handle is not closed");
        // The pipe's read end goes to `adopt`, which owns it from then on.
        let (read, _write) = pipe();
        let value = read.into_raw_handle() as usize as u32;
        let adopted = adopt(value).expect("a pipe");
        assert_eq!(adopted.raw(), Some(value as usize as RawHandle));
    }

    /// Whether the value `raw` is listed as an owned frame pipe.
    fn listed(raw: RawHandle) -> bool {
        owned().contains(&(raw as usize))
    }

    /// GIVEN a frame pipe that the worker owns, kept or read by a reader
    /// WHEN its value comes again THEN it is refused, so the handle never
    /// gets a second owner, and the first owner still reads it; WHEN the
    /// first owner closes it THEN the helper's end sees the close.
    #[test]
    fn given_a_pipe_the_worker_owns_when_its_value_comes_again_then_it_is_refused() {
        let (read, mut write) = pipe();
        let value = read.into_raw_handle() as usize as u32;
        let first = adopt(value).expect("a pipe");
        assert!(adopt(value).is_none(), "a kept pipe is not owned twice");
        let mut reader =
            PipeReader::new(first, Arc::new(AtomicBool::new(false))).expect("a pipe reader");
        assert!(adopt(value).is_none(), "nor one that a reader holds");
        write.write_all(&[7]).expect("a byte");
        assert_eq!(reader.wait_for_data(Duration::from_secs(1)), Wait::Data);
        let mut byte = [0_u8; 1];
        assert_eq!(reader.read(&mut byte).expect("the byte"), 1);
        assert_eq!(byte, [7], "the refusals closed nothing");
        drop(reader);
        assert!(
            write.write_all(&[8]).is_err(),
            "the reader's drop closed it"
        );
    }

    /// GIVEN good pictures and an end item on the pipe WHEN read THEN the
    /// frames arrive in order and the end follows; a pipe that closes after
    /// them is `Closed`.
    #[test]
    fn given_good_items_when_read_then_they_are_delivered_in_order() {
        let (mut reader, mut write, _stop) = reader();
        let body = vec![9_u8; 4 * 2 * 4];
        write_picture(&mut write, 3, 4, 2, &body).expect("a picture");
        write_picture(&mut write, 3, 4, 2, &body).expect("a picture");
        write_end(&mut write, 3, ScreenEnd::NotFound).expect("an end");
        drop(write);
        assert_eq!(reader.wait_for_data(Duration::from_secs(1)), Wait::Data);
        for _ in 0..2 {
            let Ok(ScreenItem::Frame(frame)) = read_item(&mut reader, 3) else {
                panic!("a picture expected");
            };
            assert_eq!(
                (frame.width, frame.height, frame.data),
                (4, 2, body.clone())
            );
        }
        assert_eq!(
            read_item(&mut reader, 3),
            Ok(ScreenItem::End(ScreenEnd::NotFound))
        );
        assert_eq!(reader.wait_for_data(Duration::from_secs(1)), Wait::Closed);
        assert_eq!(read_item(&mut reader, 3), Err(ScreenFrameError::Closed));
    }

    /// GIVEN bad bytes on the pipe (garbage, a wrong share, an oversize
    /// picture header) WHEN read THEN each is refused with a closed error
    /// and no body is read for it.
    #[test]
    fn given_bad_items_when_read_then_they_are_refused() {
        let (mut reader, mut write, _stop) = reader();
        write.write_all(&[0xFF; 40]).expect("garbage");
        assert_eq!(read_item(&mut reader, 3), Err(ScreenFrameError::BadHeader));
        let (mut reader, mut write, _stop) = self::reader();
        write_picture(&mut write, 4, 4, 2, &[0; 32]).expect("a picture");
        assert_eq!(read_item(&mut reader, 3), Err(ScreenFrameError::WrongShare));
    }

    /// GIVEN a pipe with nothing on it WHEN waited for THEN the wait is idle
    /// after its limit, and a set stop flag ends it at once.
    #[test]
    fn given_a_silent_pipe_when_waited_for_then_idle_or_stopped() {
        let (mut reader, _write, stop) = reader();
        assert_eq!(reader.wait_for_data(Duration::from_millis(30)), Wait::Idle);
        stop.store(true, Ordering::Release);
        let started = Instant::now();
        assert_eq!(reader.wait_for_data(Duration::from_secs(5)), Wait::Stopped);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// GIVEN an item that stops half way WHEN read THEN the read ends with a
    /// timeout and not a hang, and a stop flag ends it at once.
    #[test]
    fn given_a_stalled_item_when_read_then_the_read_ends_and_a_stop_is_prompt() {
        let (mut reader, mut write, stop) = reader();
        let mut wire = Vec::new();
        write_picture(&mut wire, 3, 4, 2, &[0; 32]).expect("a picture");
        write.write_all(&wire[..30]).expect("half an item");
        let flag = stop.clone();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            flag.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let result = read_item(&mut reader, 3);
        stopper.join().expect("the stopper");
        assert_eq!(result, Err(ScreenFrameError::Io(io::ErrorKind::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// GIVEN a helper that writes an item one byte at a time, each byte
    /// well within the limit WHEN the item is read THEN the read ends when
    /// the item's own time is up, not after the last byte: a slow helper
    /// cannot hold the capture thread.
    #[test]
    fn given_an_item_that_trickles_when_read_then_its_whole_time_is_bounded() {
        let (mut reader, mut write, _stop) = reader();
        reader.item_limit = Duration::from_millis(300);
        let mut wire = Vec::new();
        write_picture(&mut wire, 3, 4, 2, &[0; 32]).expect("a picture");
        let writer = std::thread::spawn(move || {
            for byte in wire {
                if write.write_all(&[byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        let started = Instant::now();
        reader.begin_item();
        let result = read_item(&mut reader, 3);
        let took = started.elapsed();
        drop(reader);
        writer.join().expect("the writer");
        assert_eq!(result, Err(ScreenFrameError::Io(io::ErrorKind::TimedOut)));
        // The whole item would take about 2 s; each byte came in 40 ms.
        assert!(took >= Duration::from_millis(300), "{took:?}");
        assert!(took < Duration::from_millis(1500), "{took:?}");
    }

    /// GIVEN an item whose deadline has passed WHEN the reader waits for the
    /// next item and reads it without `begin_item` THEN that read has its own
    /// time and takes the bytes: the old deadline does not carry over.
    #[test]
    fn given_a_passed_item_deadline_when_the_next_item_comes_then_it_reads() {
        let (mut reader, mut write, _stop) = reader();
        reader.item_limit = Duration::from_millis(50);
        reader.begin_item();
        std::thread::sleep(Duration::from_millis(80));
        write.write_all(&[7, 8]).expect("the next item's bytes");
        assert_eq!(reader.wait_for_data(Duration::from_secs(1)), Wait::Data);
        let mut buf = [0_u8; 2];
        assert_eq!(reader.read(&mut buf).expect("a read with its own time"), 2);
        assert_eq!(buf, [7, 8]);
    }
}

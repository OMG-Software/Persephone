//! The read end of the screen helper's frame pipe (ADR-0067).
//!
//! The core made the pipe, gave its write end to the helper and duplicated
//! its read end into this process. The worker never trusts it: before it
//! reads, it checks that the handle is a pipe (`GetFileType`), and every read
//! is bounded, so a helper that stalls cannot block the capture thread past
//! a stop. The format is `persephone_call_protocol::screen_frame`; this
//! module only moves bytes and keeps the waits short.

use std::fs::File;
use std::io::{self, Read};
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
use windows::Win32::System::Pipes::PeekNamedPipe;

/// How long a read waits for the next bytes of an item that has begun. A
/// helper that stops writing inside an item is a failed helper.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a wait looks at the pipe and at the stop flag.
const POLL: Duration = Duration::from_millis(5);

/// Whether `handle` is a pipe.
pub fn is_pipe(handle: HANDLE) -> bool {
    // SAFETY: only asks the kernel about a handle value; an invalid value
    // gives `FILE_TYPE_UNKNOWN`.
    unsafe { GetFileType(handle) == FILE_TYPE_PIPE }
}

/// The handle with value `value` in this process, owned, when it is a pipe.
/// A value that is not a pipe is never closed or read: it stays as it was.
/// The core duplicated the handle into this process just before it sent the
/// value, so a pipe at this value is that handle.
pub fn adopt(value: u32) -> Option<OwnedHandle> {
    let raw = value as usize as RawHandle;
    if !is_pipe(HANDLE(raw)) {
        return None;
    }
    // SAFETY: the value names a pipe handle of this process that nothing
    // else owns (the core made it for this worker alone).
    Some(unsafe { OwnedHandle::from_raw_handle(raw) })
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
    file: File,
    stop: std::sync::Arc<AtomicBool>,
}

impl PipeReader {
    /// Takes the pipe. A handle that is not a pipe is refused, and is not
    /// closed (it may not be ours to close).
    pub fn new(handle: OwnedHandle, stop: std::sync::Arc<AtomicBool>) -> Result<Self, ()> {
        if !is_pipe(HANDLE(handle.as_raw_handle())) {
            let _ = handle.into_raw_handle();
            return Err(());
        }
        Ok(Self {
            file: File::from(handle),
            stop,
        })
    }

    /// Bytes waiting, or `None` when the writer closed the pipe (or the
    /// pipe failed) and nothing is left to read.
    fn available(&self) -> Option<u32> {
        let mut available = 0_u32;
        // SAFETY: the handle is a pipe made in `new`; `available` is a valid
        // out value; nothing is read.
        let peeked = unsafe {
            PeekNamedPipe(
                HANDLE(self.file.as_raw_handle()),
                None,
                0,
                None,
                Some(&mut available),
                None,
            )
        };
        peeked.ok().map(|()| available)
    }

    /// Waits up to `limit` for the first bytes of the next item.
    pub fn wait_for_data(&self, limit: Duration) -> Wait {
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
            if started.elapsed() >= limit {
                return Wait::Idle;
            }
            std::thread::sleep(POLL);
        }
    }
}

impl Read for PipeReader {
    /// Reads what is waiting, at most `buf.len()`. Waits up to
    /// [`STALL_TIMEOUT`] for bytes; a set stop flag or a stall ends the wait
    /// with `TimedOut`. A closed pipe is the end of the input.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let started = Instant::now();
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.available() {
                None => return Ok(0),
                Some(0) => {}
                Some(waiting) => {
                    let take = buf.len().min(waiting as usize);
                    return self.file.read(&mut buf[..take]);
                }
            }
            if started.elapsed() >= STALL_TIMEOUT {
                return Err(io::ErrorKind::TimedOut.into());
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
        let reader = PipeReader::new(read, stop.clone()).expect("a pipe reader");
        (reader, write, stop)
    }

    /// GIVEN a handle that is not a pipe (a file, a console-like null
    /// device) WHEN the worker takes it THEN it is refused, and the handle
    /// stays open: it is not the worker's to close.
    #[test]
    fn given_a_handle_that_is_not_a_pipe_when_taken_then_it_is_refused_and_kept() {
        let file = File::open(std::env::current_exe().expect("this program")).expect("a file");
        let raw = file.as_raw_handle();
        // SAFETY: a second owner of the same handle; its `into_raw_handle`
        // below gives it back without closing it.
        let owned = unsafe { OwnedHandle::from_raw_handle(raw) };
        assert!(PipeReader::new(owned, Arc::new(AtomicBool::new(false))).is_err());
        // The file is still open and readable.
        let mut byte = [0_u8; 1];
        assert_eq!((&file).read(&mut byte).expect("still open"), 1);
        let null = File::open("NUL").expect("the null device");
        // SAFETY: as above, for the null device.
        let owned = unsafe { OwnedHandle::from_raw_handle(null.as_raw_handle()) };
        assert!(PipeReader::new(owned, Arc::new(AtomicBool::new(false))).is_err());
        assert!(!is_pipe(HANDLE(file.as_raw_handle())));
        assert!(!is_pipe(HANDLE(null.as_raw_handle())));
    }

    /// GIVEN a handle value that is not open, a file's value or a pipe's
    /// value WHEN adopted THEN only the pipe is taken.
    #[test]
    fn given_handle_values_when_adopted_then_only_a_pipe_is_taken() {
        assert!(adopt(0x00ff_fff0).is_none());
        assert!(adopt(0).is_none());
        let file = File::open(std::env::current_exe().expect("this program")).expect("a file");
        assert!(adopt(file.as_raw_handle() as usize as u32).is_none());
        let (read, _write) = pipe();
        let value = read.as_raw_handle() as usize as u32;
        let adopted = adopt(value).expect("a pipe");
        // Two owners of one handle: give one back without closing it.
        let _ = adopted.into_raw_handle();
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
        let (reader, _write, stop) = reader();
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
}

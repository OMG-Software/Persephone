//! The camera channel, worker side (ADR-0049, Linux). Descriptor 3 is one end
//! of a Unix socket pair; the core holds the other end. On the portal camera
//! path the core sends the PipeWire remote it got from the camera portal as
//! one descriptor (`SCM_RIGHTS`) with exactly one data byte.
//!
//! The worker reads the channel without blocking, takes at most one
//! descriptor per message and closes every other one, and keeps at most one
//! pending descriptor: a newer one replaces and closes the older. Nothing
//! here opens a path or makes a socket, so it needs no containment
//! allowance. The parsing rules are plain functions with unit tests; only
//! the `recvmsg` part is Linux code.
//!
//! This module uses no other module of the worker.

// Linux only; the portal camera backend takes the handle (ADR-0049). Other
// targets compile the plain helpers for their unit tests only.
#![allow(dead_code)]

/// The descriptor number of the channel in the worker.
pub const CHANNEL_FD: i32 = 3;
/// The most descriptors one message may carry that the worker looks at. A
/// message with more is refused as a whole (the kernel closes the rest).
pub const MAX_FDS_PER_MESSAGE: usize = 4;

/// What one received message means.
#[derive(Debug, Eq, PartialEq)]
pub enum Verdict {
    /// Keep the descriptor at this index; close every other one.
    Keep(usize),
    /// Close every descriptor. The message is not a valid handle message.
    Reject,
}

/// Decides on one message: exactly one data byte, no truncated control
/// data, and at least one descriptor. The first descriptor is the handle;
/// any extra descriptors are closed.
pub fn judge(data_len: usize, fd_count: usize, control_truncated: bool) -> Verdict {
    if data_len == 1 && fd_count >= 1 && !control_truncated {
        Verdict::Keep(0)
    } else {
        Verdict::Reject
    }
}

/// At most one pending item. A newer item replaces the older one, which is
/// dropped (for a descriptor: closed).
#[derive(Debug)]
pub struct Pending<T> {
    slot: Option<T>,
}

impl<T> Default for Pending<T> {
    fn default() -> Self {
        Self { slot: None }
    }
}

impl<T> Pending<T> {
    /// Stores `item`; returns whether it replaced an older one.
    pub fn put(&mut self, item: T) -> bool {
        self.slot.replace(item).is_some()
    }

    /// Takes the pending item, if any.
    pub fn take(&mut self) -> Option<T> {
        self.slot.take()
    }
}

#[cfg(target_os = "linux")]
#[allow(unused_imports)] // the portal backend uses `take_handle` (Phase 3b)
pub use linux::{CameraChannel, init, take_handle};

#[cfg(target_os = "linux")]
mod linux {
    use super::{CHANNEL_FD, MAX_FDS_PER_MESSAGE, Pending, Verdict, judge};
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};

    /// Bytes of control-message space: room for `MAX_FDS_PER_MESSAGE`
    /// descriptors. An array of `u64` aligns it for `cmsghdr`.
    const CONTROL_BYTES: usize = 32;

    /// The worker's end of the channel and its one pending descriptor.
    pub struct CameraChannel {
        fd: OwnedFd,
        pending: Mutex<Pending<OwnedFd>>,
        closed: AtomicBool,
    }

    impl CameraChannel {
        /// Takes ownership of descriptor 3 when it is a socket, and makes it
        /// non-blocking. `None` when it is anything else or absent.
        ///
        /// # Safety
        /// Descriptor 3 must not be owned by anything else in this process.
        pub unsafe fn adopt_fd3() -> Option<Self> {
            // SAFETY: stat is plain data; fstat only reads the descriptor.
            let mut stat: libc::stat = unsafe { zeroed() };
            // SAFETY: `stat` is a valid out-parameter.
            if unsafe { libc::fstat(CHANNEL_FD, &mut stat) } != 0
                || stat.st_mode & libc::S_IFMT != libc::S_IFSOCK
            {
                return None;
            }
            // SAFETY: the caller owns descriptor 3, and it is a socket.
            Self::from_fd(unsafe { OwnedFd::from_raw_fd(CHANNEL_FD) })
        }

        /// A channel over `fd`; makes it non-blocking.
        pub fn from_fd(fd: OwnedFd) -> Option<Self> {
            let raw = fd.as_raw_fd();
            // SAFETY: fcntl on a descriptor this value owns.
            let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
            // SAFETY: as above.
            if flags < 0
                || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0
            {
                return None;
            }
            Some(Self {
                fd,
                pending: Mutex::new(Pending::default()),
                closed: AtomicBool::new(false),
            })
        }

        /// Reads every waiting message. A valid handle replaces the pending
        /// one. Returns false once the core closed its end or the channel
        /// failed.
        pub fn poll(&self) -> bool {
            let raw = self.fd.as_raw_fd();
            loop {
                match receive(raw) {
                    Ok(Some(fd)) => {
                        let mut pending = self
                            .pending
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        // The replaced descriptor is dropped, so closed.
                        pending.put(fd);
                    }
                    // A message that was not a valid handle; its descriptors
                    // are already closed.
                    Ok(None) => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        return !self.closed.load(Ordering::Acquire);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        self.closed.store(true, Ordering::Release);
                        return false;
                    }
                }
            }
        }

        /// Reads the channel, then takes the newest pending descriptor.
        pub fn take(&self) -> Option<OwnedFd> {
            self.poll();
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        }
    }

    /// One `recvmsg`. `Ok(Some(fd))`: a valid handle message. `Ok(None)`:
    /// a message that was not one (its descriptors are closed). `WouldBlock`:
    /// nothing waits. End of stream is `UnexpectedEof`.
    fn receive(raw: RawFd) -> io::Result<Option<OwnedFd>> {
        let mut byte = [0_u8; 1];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: byte.len(),
        };
        let mut control = [0_u64; CONTROL_BYTES / 8];
        // SAFETY: msghdr is plain data; zero is a valid empty value.
        let mut message: libc::msghdr = unsafe { zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = CONTROL_BYTES as _;
        debug_assert!(
            // SAFETY: CMSG_SPACE is a pure size computation.
            unsafe { libc::CMSG_SPACE((MAX_FDS_PER_MESSAGE * size_of::<RawFd>()) as u32) } as usize
                <= CONTROL_BYTES
        );
        // SAFETY: `message` points at buffers that outlive the call.
        // MSG_CMSG_CLOEXEC makes every received descriptor close-on-exec.
        let received = unsafe {
            libc::recvmsg(
                raw,
                &mut message,
                libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
            )
        };
        if received < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut fds = take_fds(&message);
        if received == 0 && fds.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let truncated = message.msg_flags & libc::MSG_CTRUNC != 0;
        match judge(received as usize, fds.len(), truncated) {
            // Every other descriptor is closed when `fds` drops.
            Verdict::Keep(index) => Ok(Some(fds.swap_remove(index))),
            Verdict::Reject => Ok(None),
        }
    }

    /// Takes ownership of every `SCM_RIGHTS` descriptor of a received message.
    fn take_fds(message: &libc::msghdr) -> Vec<OwnedFd> {
        let mut fds = Vec::new();
        // SAFETY: the CMSG macros walk the control buffer `recvmsg` filled,
        // within `msg_controllen`. Each descriptor in it is new in this
        // process and owned by nothing else.
        unsafe {
            let mut header = libc::CMSG_FIRSTHDR(message);
            while !header.is_null() {
                if (*header).cmsg_level == libc::SOL_SOCKET
                    && (*header).cmsg_type == libc::SCM_RIGHTS
                {
                    let data = libc::CMSG_DATA(header);
                    let length = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    for index in 0..length / size_of::<RawFd>() {
                        let raw: RawFd = std::ptr::read_unaligned(data.cast::<RawFd>().add(index));
                        if raw >= 0 {
                            fds.push(OwnedFd::from_raw_fd(raw));
                        }
                    }
                }
                header = libc::CMSG_NXTHDR(message, header);
            }
        }
        fds
    }

    static CHANNEL: OnceLock<Option<CameraChannel>> = OnceLock::new();

    /// Adopts descriptor 3 once, at the start of the process, before any
    /// other code can open a descriptor with that number.
    pub fn init() {
        CHANNEL.get_or_init(|| {
            // SAFETY: called once from `main` before the worker opens
            // anything; descriptor 3 is the launcher's channel.
            unsafe { CameraChannel::adopt_fd3() }
        });
    }

    /// The newest descriptor the core sent and the worker has not taken, or
    /// `None` (no message, no channel, or a channel the core closed).
    pub fn take_handle() -> Option<OwnedFd> {
        CHANNEL.get()?.as_ref()?.take()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs::File;
        use std::os::unix::net::UnixStream;

        /// Sends one byte with the given descriptors, as the core does.
        fn send(socket: &UnixStream, data: &[u8], fds: &[RawFd]) {
            let mut iov = libc::iovec {
                iov_base: data.as_ptr() as *mut _,
                iov_len: data.len(),
            };
            // Room for the largest message of the tests.
            let mut control = [0_u64; 32];
            // SAFETY: plain data.
            let mut message: libc::msghdr = unsafe { zeroed() };
            message.msg_iov = &mut iov;
            message.msg_iovlen = 1;
            if !fds.is_empty() {
                let bytes = std::mem::size_of_val(fds);
                message.msg_control = control.as_mut_ptr().cast();
                // SAFETY: pure size computation.
                message.msg_controllen = unsafe { libc::CMSG_SPACE(bytes as u32) } as _;
                // SAFETY: the control buffer is large enough for `fds`.
                unsafe {
                    let header = libc::CMSG_FIRSTHDR(&message);
                    (*header).cmsg_level = libc::SOL_SOCKET;
                    (*header).cmsg_type = libc::SCM_RIGHTS;
                    (*header).cmsg_len = libc::CMSG_LEN(bytes as u32) as _;
                    std::ptr::copy_nonoverlapping(
                        fds.as_ptr().cast::<u8>(),
                        libc::CMSG_DATA(header),
                        bytes,
                    );
                }
            }
            // SAFETY: `message` points at live buffers.
            let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &message, 0) };
            assert_eq!(sent, data.len() as isize);
        }

        fn channel() -> (CameraChannel, UnixStream) {
            let (core, worker) = UnixStream::pair().unwrap();
            let channel = CameraChannel::from_fd(OwnedFd::from(worker)).unwrap();
            (channel, core)
        }

        fn open_file() -> File {
            File::open("/dev/null").unwrap()
        }

        fn is_open(fd: RawFd) -> bool {
            // SAFETY: F_GETFD on any number is safe.
            unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
        }

        /// GIVEN no message WHEN polled THEN nothing is pending and the
        /// call does not block.
        #[test]
        fn given_no_message_when_polled_then_nothing_is_pending() {
            let (channel, _core) = channel();
            assert!(channel.take().is_none());
        }

        /// GIVEN one byte with one descriptor WHEN taken THEN the worker
        /// holds a new descriptor.
        #[test]
        fn given_one_byte_and_one_descriptor_then_it_is_taken() {
            let (channel, core) = channel();
            let file = open_file();
            send(&core, b"x", &[file.as_raw_fd()]);
            let fd = channel.take().expect("the handle");
            assert!(is_open(fd.as_raw_fd()));
            assert!(channel.take().is_none());
        }

        /// A non-blocking pipe: the read end and the write end.
        fn pipe() -> (OwnedFd, OwnedFd) {
            let mut ends: [libc::c_int; 2] = [0; 2];
            // SAFETY: `ends` has room for the two descriptors.
            let made =
                unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
            assert_eq!(made, 0);
            // SAFETY: `pipe2` returned two new descriptors that nothing else owns.
            unsafe {
                (
                    std::os::fd::FromRawFd::from_raw_fd(ends[0]),
                    std::os::fd::FromRawFd::from_raw_fd(ends[1]),
                )
            }
        }

        /// Reads one byte from a non-blocking descriptor: the raw result of
        /// `read` and the error number.
        fn read_one(fd: &OwnedFd) -> (isize, i32) {
            let mut byte = 0_u8;
            // SAFETY: the buffer is one byte long.
            let n = unsafe { libc::read(fd.as_raw_fd(), (&raw mut byte).cast(), 1) };
            (
                n,
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            )
        }

        /// GIVEN two handles before a take WHEN taken THEN only the newer
        /// one is kept and the older one is closed.
        #[test]
        fn given_two_handles_then_the_older_is_closed() {
            let (channel, core) = channel();
            let (reader, writer) = pipe();
            send(&core, b"x", &[writer.as_raw_fd()]);
            // The test gives up its own copy: only the channel holds the write end.
            drop(writer);
            // Read the first by hand so it is pending.
            assert!(channel.poll());
            let (n, error) = read_one(&reader);
            assert_eq!(n, -1);
            assert_eq!(error, libc::EAGAIN, "the write end is still open");
            let second = open_file();
            send(&core, b"x", &[second.as_raw_fd()]);
            let newer = channel.take().expect("newer handle");
            let (n, _) = read_one(&reader);
            assert_eq!(
                n, 0,
                "the older descriptor is closed, so the pipe is at EOF"
            );
            assert!(is_open(newer.as_raw_fd()));
            assert!(channel.take().is_none());
        }

        /// GIVEN a message with a wrong length, no descriptor, or extra
        /// descriptors WHEN read THEN the bad ones give nothing and extras
        /// are closed.
        #[test]
        fn given_bad_messages_then_they_are_refused_and_closed() {
            let (channel, core) = channel();
            let file = open_file();
            // No descriptor.
            send(&core, b"x", &[]);
            assert!(channel.take().is_none());
            // Two descriptors in one message: the first is kept.
            send(&core, b"x", &[file.as_raw_fd(), file.as_raw_fd()]);
            let kept = channel.take().expect("first descriptor");
            drop(kept);
            // More than the buffer holds: refused whole.
            let many = vec![file.as_raw_fd(); MAX_FDS_PER_MESSAGE + 4];
            send(&core, b"x", &many);
            assert!(channel.take().is_none());
        }

        /// GIVEN the core closed its end WHEN polled THEN the channel
        /// reports it, and a handle sent before is still taken.
        #[test]
        fn given_closed_core_end_then_poll_reports_it() {
            let (channel, core) = channel();
            let file = open_file();
            send(&core, b"x", &[file.as_raw_fd()]);
            drop(core);
            assert!(!channel.poll());
            assert!(channel.take().is_some());
            assert!(!channel.poll());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// GIVEN a message WHEN judged THEN only exactly one byte with a
    /// descriptor and whole control data is kept.
    #[test]
    fn given_messages_then_only_one_byte_with_a_descriptor_is_kept() {
        assert_eq!(judge(1, 1, false), Verdict::Keep(0));
        // Extra descriptors are closed, the first is kept.
        assert_eq!(judge(1, 3, false), Verdict::Keep(0));
        assert_eq!(judge(0, 1, false), Verdict::Reject);
        assert_eq!(judge(2, 1, false), Verdict::Reject);
        assert_eq!(judge(1, 0, false), Verdict::Reject);
        assert_eq!(judge(1, 1, true), Verdict::Reject);
        assert_eq!(judge(0, 0, false), Verdict::Reject);
    }

    struct Closes(Arc<AtomicUsize>);

    impl Drop for Closes {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// GIVEN a pending item WHEN a newer one arrives THEN the older one is
    /// dropped at once and only the newer one can be taken.
    #[test]
    fn given_a_pending_item_when_a_newer_one_arrives_then_the_older_is_closed() {
        let closed = Arc::new(AtomicUsize::new(0));
        let mut pending = Pending::default();
        assert!(!pending.put(Closes(closed.clone())));
        assert_eq!(closed.load(Ordering::SeqCst), 0);
        assert!(pending.put(Closes(closed.clone())));
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        let taken = pending.take().expect("the newer item");
        assert!(pending.take().is_none());
        drop(taken);
        assert_eq!(closed.load(Ordering::SeqCst), 2);
    }
}

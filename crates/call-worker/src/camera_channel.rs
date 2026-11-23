//! The camera channel, worker side (ADR-0049 and ADR-0053, Linux).
//! Descriptor 3 is one end of a Unix socket pair; the core holds the other
//! end. On the portal paths the core sends the PipeWire remote it got from
//! the camera or the screen cast portal as one descriptor (`SCM_RIGHTS`)
//! with exactly one data byte. The byte names the kind: `c` for a camera,
//! `s` for a screen. A one-byte message with any other value closes the
//! whole channel: every pending descriptor closes and none is given out
//! after it.
//!
//! The worker reads the channel without blocking and takes at most one
//! descriptor per message, closing every other one. The core sends one
//! descriptor directly before each camera-on (`SetCamera { remote: true }`)
//! or screen start (`SetScreen { target: Portal }`), so the worker keeps the
//! pending descriptors of each kind in order, and each such command takes
//! the oldest of its kind. At most `MAX_PENDING` of each kind wait; one more
//! of a kind breaks that kind, which then closes its descriptors and gives
//! out none (the camera or screen stays unavailable). Nothing
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
/// The most descriptors of one kind that may wait for their command. The
/// core sends the next one only after a desktop dialog, so more than one is
/// rare.
pub const MAX_PENDING: usize = 4;
/// The data byte of a camera descriptor.
pub const KIND_CAMERA: u8 = b'c';
/// The data byte of a screen descriptor.
pub const KIND_SCREEN: u8 = b's';

/// What a descriptor is for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Camera,
    Screen,
}

/// What one received message means.
#[derive(Debug, Eq, PartialEq)]
pub enum Verdict {
    /// Keep the descriptor at this index for this kind; close every other
    /// one.
    Keep(usize, Kind),
    /// Close every descriptor. The message is not a valid handle message.
    Reject,
    /// The kind byte is neither `c` nor `s`: close every descriptor and the
    /// whole channel.
    Close,
}

/// Decides on one message: exactly one data byte, no truncated control
/// data, and at least one descriptor. The byte names the kind. The first
/// descriptor is the handle; any extra descriptors are closed. A single data
/// byte of another value is a protocol violation and closes the channel.
pub fn judge(data: &[u8], fd_count: usize, control_truncated: bool) -> Verdict {
    let [byte] = data else {
        return Verdict::Reject;
    };
    let kind = match *byte {
        KIND_CAMERA => Kind::Camera,
        KIND_SCREEN => Kind::Screen,
        _ => return Verdict::Close,
    };
    if fd_count >= 1 && !control_truncated {
        Verdict::Keep(0, kind)
    } else {
        Verdict::Reject
    }
}

/// The pending items, oldest first, at most `MAX_PENDING`. One more breaks
/// the queue: it drops (for a descriptor: closes) every item and keeps none
/// after, so no item can go to another item's command.
#[derive(Debug)]
pub struct Pending<T> {
    items: std::collections::VecDeque<T>,
    broken: bool,
}

impl<T> Default for Pending<T> {
    fn default() -> Self {
        Self {
            items: std::collections::VecDeque::new(),
            broken: false,
        }
    }
}

impl<T> Pending<T> {
    /// Stores `item` after the others. Returns false when it was dropped:
    /// the queue was full (and is now broken) or already broken.
    pub fn put(&mut self, item: T) -> bool {
        if !self.broken && self.items.len() >= MAX_PENDING {
            self.broken = true;
            self.items.clear();
        }
        if self.broken {
            return false;
        }
        self.items.push_back(item);
        true
    }

    /// Takes the oldest pending item, if any.
    pub fn take(&mut self) -> Option<T> {
        self.items.pop_front()
    }
}

/// The pending items of both kinds. A byte of another value closes the
/// whole thing: every item drops and none is stored or given out after.
#[derive(Debug)]
pub struct Channels<T> {
    camera: Pending<T>,
    screen: Pending<T>,
    closed: bool,
}

impl<T> Default for Channels<T> {
    fn default() -> Self {
        Self {
            camera: Pending::default(),
            screen: Pending::default(),
            closed: false,
        }
    }
}

impl<T> Channels<T> {
    fn queue(&mut self, kind: Kind) -> &mut Pending<T> {
        match kind {
            Kind::Camera => &mut self.camera,
            Kind::Screen => &mut self.screen,
        }
    }

    /// Stores `item` for `kind`. Returns false when it was dropped.
    pub fn put(&mut self, kind: Kind, item: T) -> bool {
        !self.closed && self.queue(kind).put(item)
    }

    /// Takes the oldest pending item of `kind`.
    pub fn take(&mut self, kind: Kind) -> Option<T> {
        if self.closed {
            return None;
        }
        self.queue(kind).take()
    }

    /// Closes the channel: drops every item and refuses all later ones.
    pub fn close(&mut self) {
        self.closed = true;
        self.camera = Pending::default();
        self.screen = Pending::default();
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

#[cfg(target_os = "linux")]
#[allow(unused_imports)] // the portal backends use `take_handle`
pub use linux::{CameraChannel, init, take_handle};

#[cfg(target_os = "linux")]
mod linux {
    use super::{CHANNEL_FD, Channels, Kind, MAX_FDS_PER_MESSAGE, Verdict, judge};
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};

    /// Bytes of control-message space: room for `MAX_FDS_PER_MESSAGE`
    /// descriptors. An array of `u64` aligns it for `cmsghdr`.
    const CONTROL_BYTES: usize = 32;

    /// The worker's end of the channel and its pending descriptors.
    pub struct CameraChannel {
        fd: OwnedFd,
        pending: Mutex<Channels<OwnedFd>>,
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
                pending: Mutex::new(Channels::default()),
                closed: AtomicBool::new(false),
            })
        }

        /// Reads every waiting message. A valid handle joins the pending
        /// ones. Returns false once the core closed its end or the channel
        /// failed.
        pub fn poll(&self) -> bool {
            let raw = self.fd.as_raw_fd();
            loop {
                match receive(raw) {
                    Ok(Received::Handle(kind, fd)) => {
                        let mut pending = self
                            .pending
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        // A refused descriptor is dropped, so closed.
                        pending.put(kind, fd);
                    }
                    // Another kind byte: the channel and every pending
                    // descriptor close.
                    Ok(Received::Violation) => {
                        self.pending
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .close();
                        self.closed.store(true, Ordering::Release);
                        return false;
                    }
                    // A message that was not a valid handle; its descriptors
                    // are already closed.
                    Ok(Received::Ignored) => {}
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

        /// Reads the channel, then takes the oldest pending descriptor of
        /// `kind`.
        pub fn take(&self, kind: Kind) -> Option<OwnedFd> {
            self.poll();
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take(kind)
        }
    }

    /// What one `recvmsg` gave.
    enum Received {
        /// A valid handle message.
        Handle(Kind, OwnedFd),
        /// A message that was not one (its descriptors are closed).
        Ignored,
        /// A one-byte message with a kind byte of another value.
        Violation,
    }

    /// One `recvmsg`. `WouldBlock`: nothing waits. End of stream is
    /// `UnexpectedEof`.
    fn receive(raw: RawFd) -> io::Result<Received> {
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
        let length = (received as usize).min(byte.len());
        match judge(&byte[..length], fds.len(), truncated) {
            // Every other descriptor is closed when `fds` drops.
            Verdict::Keep(index, kind) => Ok(Received::Handle(kind, fds.swap_remove(index))),
            Verdict::Reject => Ok(Received::Ignored),
            Verdict::Close => Ok(Received::Violation),
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

    /// The oldest descriptor of `kind` the core sent and the worker has not
    /// taken, or `None` (no message, no channel, or a broken queue). Call it
    /// once for each command that carries one.
    pub fn take_handle(kind: Kind) -> Option<OwnedFd> {
        CHANNEL.get()?.as_ref()?.take(kind)
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
            assert!(channel.take(Kind::Camera).is_none());
        }

        /// GIVEN one byte with one descriptor WHEN taken THEN the worker
        /// holds a new descriptor.
        #[test]
        fn given_one_byte_and_one_descriptor_then_it_is_taken() {
            let (channel, core) = channel();
            let file = open_file();
            send(&core, b"c", &[file.as_raw_fd()]);
            let fd = channel.take(Kind::Camera).expect("the handle");
            assert!(is_open(fd.as_raw_fd()));
            assert!(channel.take(Kind::Camera).is_none());
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

        /// GIVEN two handles before a take WHEN taken THEN each take gets
        /// the oldest one left, so each command gets its own.
        #[test]
        fn given_two_handles_then_they_are_taken_oldest_first() {
            let (channel, core) = channel();
            let (reader, writer) = pipe();
            send(&core, b"c", &[writer.as_raw_fd()]);
            // The test gives up its own copy: only the channel holds the write end.
            drop(writer);
            let second = open_file();
            send(&core, b"c", &[second.as_raw_fd()]);
            let first = channel.take(Kind::Camera).expect("the older handle");
            let (n, error) = read_one(&reader);
            assert_eq!(n, -1);
            assert_eq!(error, libc::EAGAIN, "the first taken is the pipe's end");
            drop(first);
            let (n, _) = read_one(&reader);
            assert_eq!(n, 0, "closed with its command, so the pipe is at EOF");
            let newer = channel.take(Kind::Camera).expect("the newer handle");
            assert!(is_open(newer.as_raw_fd()));
            assert!(channel.take(Kind::Camera).is_none());
        }

        /// GIVEN a message with a wrong length, no descriptor, or extra
        /// descriptors WHEN read THEN the bad ones give nothing and extras
        /// are closed.
        #[test]
        fn given_bad_messages_then_they_are_refused_and_closed() {
            let (channel, core) = channel();
            let file = open_file();
            // No descriptor.
            send(&core, b"c", &[]);
            assert!(channel.take(Kind::Camera).is_none());
            // Two descriptors in one message: the first is kept.
            send(&core, b"c", &[file.as_raw_fd(), file.as_raw_fd()]);
            let kept = channel.take(Kind::Camera).expect("first descriptor");
            drop(kept);
            // More than the buffer holds: refused whole.
            let many = vec![file.as_raw_fd(); MAX_FDS_PER_MESSAGE + 4];
            send(&core, b"c", &many);
            assert!(channel.take(Kind::Camera).is_none());
        }

        /// GIVEN a camera and a screen descriptor WHEN taken by kind THEN
        /// each command gets the descriptor of its own kind.
        #[test]
        fn given_both_kinds_then_each_is_taken_by_its_kind() {
            let (channel, core) = channel();
            let (reader, writer) = pipe();
            send(&core, b"s", &[writer.as_raw_fd()]);
            drop(writer);
            let file = open_file();
            send(&core, b"c", &[file.as_raw_fd()]);
            let camera = channel.take(Kind::Camera).expect("the camera handle");
            assert!(is_open(camera.as_raw_fd()));
            let (n, error) = read_one(&reader);
            assert_eq!((n, error), (-1, libc::EAGAIN), "the screen end is held");
            assert!(channel.take(Kind::Camera).is_none());
            let screen = channel.take(Kind::Screen).expect("the screen handle");
            drop(screen);
            assert_eq!(read_one(&reader).0, 0, "closed with its command");
        }

        /// GIVEN pending descriptors WHEN a message has another kind byte
        /// THEN the channel closes: every pending descriptor closes and
        /// none is given out, even one sent after.
        #[test]
        fn given_another_kind_byte_then_the_whole_channel_closes() {
            let (channel, core) = channel();
            let (reader, writer) = pipe();
            send(&core, b"s", &[writer.as_raw_fd()]);
            drop(writer);
            let file = open_file();
            send(&core, b"x", &[file.as_raw_fd()]);
            send(&core, b"c", &[file.as_raw_fd()]);
            assert!(channel.take(Kind::Screen).is_none());
            assert!(channel.take(Kind::Camera).is_none());
            assert!(!channel.poll());
            assert_eq!(
                read_one(&reader).0,
                0,
                "the pending screen descriptor closed"
            );
        }

        /// GIVEN the core closed its end WHEN polled THEN the channel
        /// reports it, and a handle sent before is still taken.
        #[test]
        fn given_closed_core_end_then_poll_reports_it() {
            let (channel, core) = channel();
            let file = open_file();
            send(&core, b"c", &[file.as_raw_fd()]);
            drop(core);
            assert!(!channel.poll());
            assert!(channel.take(Kind::Camera).is_some());
            assert!(!channel.poll());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// GIVEN a message WHEN judged THEN only exactly one `c` or `s` byte
    /// with a descriptor and whole control data is kept, with its kind.
    #[test]
    fn given_messages_then_only_one_kind_byte_with_a_descriptor_is_kept() {
        assert_eq!(judge(b"c", 1, false), Verdict::Keep(0, Kind::Camera));
        assert_eq!(judge(b"s", 1, false), Verdict::Keep(0, Kind::Screen));
        // Extra descriptors are closed, the first is kept.
        assert_eq!(judge(b"s", 3, false), Verdict::Keep(0, Kind::Screen));
        assert_eq!(judge(b"", 1, false), Verdict::Reject);
        assert_eq!(judge(b"cc", 1, false), Verdict::Reject);
        assert_eq!(judge(b"c", 0, false), Verdict::Reject);
        assert_eq!(judge(b"s", 1, true), Verdict::Reject);
        assert_eq!(judge(b"", 0, false), Verdict::Reject);
    }

    /// GIVEN a one-byte message with any other value than `c` or `s`
    /// WHEN judged THEN it closes the channel, with or without a descriptor.
    #[test]
    fn given_any_other_kind_byte_then_the_channel_closes() {
        for byte in (0..=u8::MAX).filter(|byte| !matches!(byte, b'c' | b's')) {
            assert_eq!(judge(&[byte], 1, false), Verdict::Close, "{byte}");
            assert_eq!(judge(&[byte], 0, false), Verdict::Close, "{byte}");
        }
    }

    /// GIVEN descriptors of both kinds WHEN taken THEN each kind has its own
    /// queue, oldest first, and one kind never takes the other's.
    #[test]
    fn given_two_kinds_then_each_has_its_own_queue() {
        let mut channels = Channels::default();
        assert!(channels.put(Kind::Camera, 1));
        assert!(channels.put(Kind::Screen, 10));
        assert!(channels.put(Kind::Camera, 2));
        assert_eq!(channels.take(Kind::Screen), Some(10));
        assert_eq!(channels.take(Kind::Screen), None);
        assert_eq!(channels.take(Kind::Camera), Some(1));
        assert_eq!(channels.take(Kind::Camera), Some(2));
        assert_eq!(channels.take(Kind::Camera), None);
    }

    /// GIVEN four pending of each kind WHEN a fifth arrives for one kind
    /// THEN that kind breaks and closes its items; the other kind is kept.
    #[test]
    fn given_a_full_kind_when_one_more_arrives_then_only_that_kind_breaks() {
        let closed = Arc::new(AtomicUsize::new(0));
        let mut channels = Channels::default();
        for _ in 0..MAX_PENDING {
            assert!(channels.put(Kind::Camera, Closes(closed.clone())));
            assert!(channels.put(Kind::Screen, Closes(closed.clone())));
        }
        assert!(!channels.put(Kind::Screen, Closes(closed.clone())));
        assert_eq!(closed.load(Ordering::SeqCst), MAX_PENDING + 1);
        assert!(channels.take(Kind::Screen).is_none());
        assert!(channels.take(Kind::Camera).is_some());
    }

    /// GIVEN pending descriptors of both kinds WHEN the channel closes
    /// THEN all of them close at once and none is given out or stored after.
    #[test]
    fn given_a_closed_channel_then_everything_is_dropped_and_nothing_follows() {
        let closed = Arc::new(AtomicUsize::new(0));
        let mut channels = Channels::default();
        for kind in [Kind::Camera, Kind::Screen] {
            assert!(channels.put(kind, Closes(closed.clone())));
            assert!(channels.put(kind, Closes(closed.clone())));
        }
        channels.close();
        assert_eq!(closed.load(Ordering::SeqCst), 4);
        assert!(channels.is_closed());
        for kind in [Kind::Camera, Kind::Screen] {
            assert!(channels.take(kind).is_none());
            assert!(!channels.put(kind, Closes(closed.clone())));
        }
        assert_eq!(closed.load(Ordering::SeqCst), 6);
    }

    struct Closes(Arc<AtomicUsize>);

    impl Drop for Closes {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// GIVEN pending items WHEN taken THEN they come oldest first.
    #[test]
    fn given_pending_items_then_they_are_taken_oldest_first() {
        let mut pending = Pending::default();
        for item in 1..=MAX_PENDING {
            assert!(pending.put(item));
        }
        let taken: Vec<usize> = std::iter::from_fn(|| pending.take()).collect();
        assert_eq!(taken, (1..=MAX_PENDING).collect::<Vec<_>>());
    }

    /// GIVEN a full queue WHEN one more item arrives THEN every item is
    /// dropped at once, and the queue keeps no later item: none can go to
    /// another item's command.
    #[test]
    fn given_a_full_queue_when_one_more_arrives_then_it_breaks_closed() {
        let closed = Arc::new(AtomicUsize::new(0));
        let mut pending = Pending::default();
        for _ in 0..MAX_PENDING {
            assert!(pending.put(Closes(closed.clone())));
        }
        assert_eq!(closed.load(Ordering::SeqCst), 0);
        assert!(!pending.put(Closes(closed.clone())));
        assert_eq!(closed.load(Ordering::SeqCst), MAX_PENDING + 1);
        assert!(pending.take().is_none());
        assert!(!pending.put(Closes(closed.clone())));
        assert_eq!(closed.load(Ordering::SeqCst), MAX_PENDING + 2);
        assert!(pending.take().is_none());
    }
}

//! The UI's connection to the key-holding core (ADR-0004).
//!
//! The core starts the UI with a pipe on the UI's stdin and stdout. The UI
//! takes both ends at startup as non-inheritable handles and points its
//! standard handles at `NUL`, so nothing printed later and no web view child
//! process can reach the core channel. Each call is one bounded request frame
//! answered by one bounded reply frame; between replies the core may send
//! content-free change events. One reader thread routes both.
//!
//! An `image_band` call is answered by a pixel reply and one bounded pixel
//! frame instead (or by an ordinary error reply). The reader checks that the
//! band goes to a caller that asked for exactly that band; anything else
//! breaks the channel, like a reply nobody asked for.

use persephone_core_protocol::{
    Call, CoreMessage, ErrorCode, Event, FromCore, Outcome, PixelBand, Request, receive_from_core,
    send,
};
use persephone_media_protocol::ResourceBudgets;
use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, Write},
    sync::{
        Arc, Mutex, MutexGuard,
        mpsc::{SyncSender, sync_channel},
    },
};

#[derive(Debug, PartialEq, Eq)]
pub enum CoreError {
    /// The channel is closed or broken.
    Unavailable,
    /// The core answered with an error code.
    Core(ErrorCode),
    /// The core's reply did not match the expected contract.
    BadReply,
}

/// Something that can answer calls on the core's behalf: the real pipe, or a
/// stand-in in tests.
pub trait CoreTransport: Send + Sync {
    /// One call answered by one reply. `image_band` is not a call of this
    /// kind: use [`CoreTransport::image_band`].
    fn call(&self, call: Call) -> Result<serde_json::Value, CoreError>;

    /// One band of rows of a loaded or picked image.
    fn image_band(&self, handle: String, band: u32) -> Result<Band, CoreError>;
}

/// A band of RGBA rows the core sent: `rows` rows of `width` pixels. The
/// protocol reader checked that `pixels` is exactly `width * rows * 4` bytes
/// and at most `MAX_BAND_BYTES` before it allocated them.
#[derive(Debug, PartialEq, Eq)]
pub struct Band {
    pub width: u32,
    pub rows: u32,
    pub pixels: Vec<u8>,
}

pub type CoreLink = Arc<dyn CoreTransport>;

/// Receives the core's events on the reader thread.
pub type EventSink = Box<dyn Fn(Event) + Send>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What a waiting caller accepts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// A reply.
    Reply,
    /// A pixel band with this index, or an error reply.
    Band(u32),
}

enum Answer {
    Outcome(Outcome),
    /// The band index the core announced, and the band.
    Band(u32, Band),
}

struct Waiter {
    expect: Expect,
    answer_to: SyncSender<Answer>,
}

#[derive(Default)]
struct Waiting {
    replies: HashMap<u32, Waiter>,
    broken: bool,
}

/// Routes one frame from the core to its caller. `false` means the frame
/// broke the contract: nobody asked for it, or it is not the answer its
/// caller asked for.
fn route(waiting: &Mutex<Waiting>, from_core: FromCore, on_event: &EventSink) -> bool {
    let (id, answer) = match from_core {
        FromCore::Message(CoreMessage::Event(event)) => {
            on_event(event);
            return true;
        }
        FromCore::Message(CoreMessage::Reply(reply)) => (reply.id, Answer::Outcome(reply.outcome)),
        FromCore::Pixels(PixelBand { reply, pixels }) => {
            // Copied out at once, so the frame's budget reservation ends here;
            // the caller's buffer is bounded by the band limit.
            let band = Band {
                width: reply.width,
                rows: reply.rows,
                pixels: pixels.body().to_vec(),
            };
            (reply.id, Answer::Band(reply.band, band))
        }
    };
    let Some(waiter) = lock(waiting).replies.remove(&id) else {
        return false;
    };
    let fits = match (&answer, waiter.expect) {
        (Answer::Outcome(_), Expect::Reply) => true,
        // A band is answered with pixels or an error, never with a value.
        (Answer::Outcome(Outcome::Error(_)), Expect::Band(_)) => true,
        (Answer::Outcome(Outcome::Ok(_)), Expect::Band(_)) => false,
        (Answer::Band(..), Expect::Reply) => false,
        (Answer::Band(sent, _), Expect::Band(asked)) => *sent == asked,
    };
    if !fits {
        // Dropping the waiter wakes its caller as unavailable.
        return false;
    }
    let _ = waiter.answer_to.send(answer);
    true
}

impl Waiting {
    /// Marks the channel unusable; dropping the senders wakes every caller.
    fn break_channel(&mut self) {
        self.broken = true;
        self.replies.clear();
    }
}

struct Writer {
    to_core: Box<dyn Write + Send>,
    next_id: u32,
}

/// Calls from any thread over the pipe the core created. Each caller waits for
/// the reply with its request ID; any failure, or a reply nobody asked for,
/// breaks the channel for every caller.
pub struct PipeTransport {
    writer: Mutex<Writer>,
    waiting: Arc<Mutex<Waiting>>,
}

impl PipeTransport {
    /// Starts the reader thread on `from_core`.
    pub fn start(
        mut from_core: impl Read + Send + 'static,
        to_core: impl Write + Send + 'static,
        on_event: EventSink,
    ) -> io::Result<Self> {
        let waiting = Arc::new(Mutex::new(Waiting::default()));
        let reader_waiting = waiting.clone();
        std::thread::Builder::new()
            .name("core-reader".into())
            .spawn(move || {
                let budgets = ResourceBudgets::default();
                while let Ok(from) = receive_from_core(&mut from_core, &budgets) {
                    if !route(&reader_waiting, from, &on_event) {
                        break;
                    }
                }
                lock(&reader_waiting).break_channel();
            })?;
        Ok(Self {
            writer: Mutex::new(Writer {
                to_core: Box::new(to_core),
                next_id: 1,
            }),
            waiting,
        })
    }

    /// Takes the core channel from this process's stdin and stdout.
    pub fn from_stdio(on_event: EventSink) -> io::Result<Self> {
        let (from_core, to_core) = take_stdio()?;
        Self::start(from_core, to_core, on_event)
    }
}

impl PipeTransport {
    /// Sends one request and waits for the answer its caller expects.
    fn exchange(&self, call: Call, expect: Expect) -> Result<Answer, CoreError> {
        let (answer_to, answer) = sync_channel(1);
        {
            let mut writer = lock(&self.writer);
            let id = writer.next_id;
            writer.next_id = writer.next_id.wrapping_add(1);
            {
                let mut waiting = lock(&self.waiting);
                if waiting.broken || waiting.replies.contains_key(&id) {
                    return Err(CoreError::Unavailable);
                }
                waiting.replies.insert(id, Waiter { expect, answer_to });
            }
            // A failed write leaves the stream position unknown, so the
            // channel is not reused.
            if send(&mut writer.to_core, &Request { id, call }).is_err() {
                lock(&self.waiting).break_channel();
                return Err(CoreError::Unavailable);
            }
        }
        match answer.recv() {
            Ok(Answer::Outcome(Outcome::Error(code))) => Err(CoreError::Core(code)),
            Ok(answer) => Ok(answer),
            Err(_) => Err(CoreError::Unavailable),
        }
    }
}

impl CoreTransport for PipeTransport {
    fn call(&self, call: Call) -> Result<serde_json::Value, CoreError> {
        // Its answer is a pixel band, which only `image_band` accepts; sent
        // here, it would break the channel.
        if matches!(call, Call::ImageBand { .. }) {
            return Err(CoreError::BadReply);
        }
        match self.exchange(call, Expect::Reply)? {
            Answer::Outcome(Outcome::Ok(value)) => Ok(value),
            // `route` delivers nothing else to a reply waiter.
            Answer::Outcome(Outcome::Error(code)) => Err(CoreError::Core(code)),
            Answer::Band(..) => Err(CoreError::BadReply),
        }
    }

    fn image_band(&self, handle: String, band: u32) -> Result<Band, CoreError> {
        match self.exchange(Call::ImageBand { handle, band }, Expect::Band(band))? {
            Answer::Band(_, pixels) => Ok(pixels),
            // `route` delivers only bands and errors to a band waiter.
            Answer::Outcome(Outcome::Error(code)) => Err(CoreError::Core(code)),
            Answer::Outcome(Outcome::Ok(_)) => Err(CoreError::BadReply),
        }
    }
}

#[cfg(windows)]
fn take_stdio() -> io::Result<(File, File)> {
    use std::{
        os::windows::io::{FromRawHandle, OwnedHandle},
        ptr::{null, null_mut},
    };
    use windows_sys::Win32::{
        Foundation::{
            DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_READ,
            GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
        },
        Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
        System::{
            Console::{
                GetStdHandle, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
            },
            Threading::GetCurrentProcess,
        },
    };

    /// Moves a standard handle to a new non-inheritable handle and closes the
    /// original, which was inheritable.
    fn take(which: STD_HANDLE) -> io::Result<OwnedHandle> {
        // SAFETY: reads this process's standard handle table.
        let original = unsafe { GetStdHandle(which) };
        if original.is_null() || original == INVALID_HANDLE_VALUE {
            return Err(io::Error::other(
                "standard handle missing; started without the core?",
            ));
        }
        let mut moved: HANDLE = null_mut();
        // SAFETY: duplicates within this process; DUPLICATE_CLOSE_SOURCE closes
        // `original` whether or not the call succeeds.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                original,
                GetCurrentProcess(),
                &mut moved,
                0,
                0,
                DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `moved` is a new handle owned only here.
        Ok(unsafe { OwnedHandle::from_raw_handle(moved as _) })
    }

    let from_core = take(STD_INPUT_HANDLE)?;
    let to_core = take(STD_OUTPUT_HANDLE)?;
    let nul: Vec<u16> = "NUL".encode_utf16().chain(Some(0)).collect();
    // SAFETY: opens the null device with default, non-inheritable security.
    let null_device = unsafe {
        CreateFileW(
            nul.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if null_device == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // The null-device handle stays open for the life of the process.
    // SAFETY: replaces the standard handles with a valid handle.
    unsafe {
        SetStdHandle(STD_INPUT_HANDLE, null_device);
        SetStdHandle(STD_OUTPUT_HANDLE, null_device);
    }
    Ok((File::from(from_core), File::from(to_core)))
}

/// Moves the core's pipes off descriptors 0 and 1 to new close-on-exec
/// descriptors, and points 0 and 1 at `/dev/null`, so that the web view's
/// child processes inherit nothing that reaches the core.
#[cfg(unix)]
fn take_stdio() -> io::Result<(File, File)> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    fn take(fd: libc::c_int, null: &File) -> io::Result<OwnedFd> {
        // SAFETY: duplicates an open descriptor to the lowest free number
        // above 2, with close-on-exec; the result is owned only here.
        let moved = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if moved < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `moved` is a new descriptor that nothing else owns.
        let moved = unsafe { OwnedFd::from_raw_fd(moved) };
        // SAFETY: both descriptors are open; dup2 replaces `fd` atomically.
        if unsafe { libc::dup2(null.as_raw_fd(), fd) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(moved)
    }

    let null_in = File::open("/dev/null")?;
    let null_out = File::options().write(true).open("/dev/null")?;
    let from_core = take(libc::STDIN_FILENO, &null_in)?;
    let to_core = take(libc::STDOUT_FILENO, &null_out)?;
    Ok((File::from(from_core), File::from(to_core)))
}

#[cfg(not(any(windows, unix)))]
fn take_stdio() -> io::Result<(File, File)> {
    Err(io::Error::other(
        "the core channel is implemented for Windows, Linux and macOS only",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use persephone_core_protocol::{PixelReply, Reply, receive, send_pixel_reply};
    use std::sync::mpsc::channel;

    const HANDLE: &str = "0123456789abcdef0123456789abcdef";

    /// A transport on two pipes, with the core's ends and the events it
    /// delivered.
    struct Scripted {
        link: Arc<PipeTransport>,
        from_ui: io::PipeReader,
        to_ui: io::PipeWriter,
        events: std::sync::mpsc::Receiver<Event>,
    }

    fn scripted() -> Scripted {
        let (from_ui, to_core) = io::pipe().unwrap();
        let (from_core, to_ui) = io::pipe().unwrap();
        let (events_to, events) = channel();
        let link = Arc::new(
            PipeTransport::start(
                from_core,
                to_core,
                Box::new(move |event| {
                    let _ = events_to.send(event);
                }),
            )
            .unwrap(),
        );
        Scripted {
            link,
            from_ui,
            to_ui,
            events,
        }
    }

    fn band_call(
        link: &Arc<PipeTransport>,
        band: u32,
    ) -> std::thread::JoinHandle<Result<Band, CoreError>> {
        let link = link.clone();
        std::thread::spawn(move || link.image_band(HANDLE.into(), band))
    }

    fn wait_broken(link: &PipeTransport) {
        while !lock(&link.waiting).broken {
            std::thread::yield_now();
        }
    }

    /// GIVEN two band requests and a text call WHEN the core answers them out
    /// of order with events between THEN each band reaches the caller that
    /// asked for it, whole, and the events and the reply are delivered too.
    #[test]
    fn given_interleaved_pixel_replies_and_events_then_each_band_reaches_its_caller() {
        let mut core = scripted();
        let budgets = ResourceBudgets::default();
        let first = band_call(&core.link, 0);
        let first_request: Request = receive(&mut core.from_ui, &budgets).unwrap();
        let second = band_call(&core.link, 1);
        let second_request: Request = receive(&mut core.from_ui, &budgets).unwrap();
        let status = {
            let link = core.link.clone();
            std::thread::spawn(move || link.call(Call::AppStatus {}))
        };
        let status_request: Request = receive(&mut core.from_ui, &budgets).unwrap();
        assert_eq!(
            first_request.call,
            Call::ImageBand {
                handle: HANDLE.into(),
                band: 0
            }
        );

        let room = Event::RoomChanged {
            room_id: "!r:example.invalid".into(),
        };
        send(&mut core.to_ui, &CoreMessage::Event(room.clone())).unwrap();
        let second_pixels: Vec<u8> = (0..8).collect();
        send_pixel_reply(
            &mut core.to_ui,
            &PixelReply {
                id: second_request.id,
                width: 2,
                rows: 1,
                band: 1,
            },
            &second_pixels,
        )
        .unwrap();
        send(
            &mut core.to_ui,
            &CoreMessage::Event(Event::SecurityChanged {}),
        )
        .unwrap();
        reply(
            &mut core.to_ui,
            status_request.id,
            Outcome::Ok(serde_json::json!("status")),
        );
        let first_pixels: Vec<u8> = (100..116).collect();
        send_pixel_reply(
            &mut core.to_ui,
            &PixelReply {
                id: first_request.id,
                width: 2,
                rows: 2,
                band: 0,
            },
            &first_pixels,
        )
        .unwrap();

        assert_eq!(
            second.join().unwrap(),
            Ok(Band {
                width: 2,
                rows: 1,
                pixels: second_pixels
            })
        );
        assert_eq!(
            first.join().unwrap(),
            Ok(Band {
                width: 2,
                rows: 2,
                pixels: first_pixels
            })
        );
        assert_eq!(status.join().unwrap(), Ok(serde_json::json!("status")));
        assert_eq!(core.events.recv().unwrap(), room);
        assert_eq!(core.events.recv().unwrap(), Event::SecurityChanged {});

        // An error reply to a band request is an ordinary error.
        let missing = band_call(&core.link, 3);
        let request: Request = receive(&mut core.from_ui, &budgets).unwrap();
        reply(
            &mut core.to_ui,
            request.id,
            Outcome::Error(ErrorCode::NotFound),
        );
        assert_eq!(
            missing.join().unwrap(),
            Err(CoreError::Core(ErrorCode::NotFound))
        );
        assert!(!lock(&core.link.waiting).broken);
    }

    /// GIVEN answers that do not fit the request WHEN they arrive THEN the
    /// channel breaks for every caller: pixels for a text call, pixels for
    /// another band, a value for a band, or pixels nobody asked for.
    #[test]
    fn given_a_pixel_reply_that_does_not_fit_then_the_channel_breaks() {
        let budgets = ResourceBudgets::default();
        let pixels = [9u8; 4];
        for case in ["pixels_for_text", "other_band", "value_for_band", "unasked"] {
            let mut core = scripted();
            let (waiter, id) = match case {
                "pixels_for_text" => {
                    let link = core.link.clone();
                    let waiter = std::thread::spawn(move || {
                        link.call(Call::AppStatus {}).map(|_| Band {
                            width: 0,
                            rows: 0,
                            pixels: Vec::new(),
                        })
                    });
                    let request: Request = receive(&mut core.from_ui, &budgets).unwrap();
                    (Some(waiter), request.id)
                }
                "unasked" => (None, 77),
                _ => {
                    let waiter = band_call(&core.link, 0);
                    let request: Request = receive(&mut core.from_ui, &budgets).unwrap();
                    (Some(waiter), request.id)
                }
            };
            let band = if case == "other_band" { 1 } else { 0 };
            if case == "value_for_band" {
                reply(&mut core.to_ui, id, Outcome::Ok(serde_json::Value::Null));
            } else {
                send_pixel_reply(
                    &mut core.to_ui,
                    &PixelReply {
                        id,
                        width: 1,
                        rows: 1,
                        band,
                    },
                    &pixels,
                )
                .unwrap();
            }
            if let Some(waiter) = waiter {
                assert_eq!(
                    waiter.join().unwrap(),
                    Err(CoreError::Unavailable),
                    "{case}"
                );
            }
            wait_broken(&core.link);
            assert_eq!(
                core.link.call(Call::AppStatus {}),
                Err(CoreError::Unavailable),
                "{case}"
            );
            assert_eq!(
                core.link.image_band(HANDLE.into(), 0),
                Err(CoreError::Unavailable),
                "{case}"
            );
        }
    }

    /// GIVEN `image_band` sent as an ordinary call THEN it is refused before
    /// it is written, so its pixel answer cannot break the channel.
    #[test]
    fn given_an_image_band_through_call_then_nothing_is_sent() {
        let core = scripted();
        assert_eq!(
            core.link.call(Call::ImageBand {
                handle: HANDLE.into(),
                band: 0
            }),
            Err(CoreError::BadReply)
        );
        assert_eq!(lock(&core.link.writer).next_id, 1, "nothing was written");
        assert!(!lock(&core.link.waiting).broken);
    }

    fn reply(to_ui: &mut impl Write, id: u32, outcome: Outcome) {
        send(to_ui, &CoreMessage::Reply(Reply { id, outcome })).unwrap();
    }

    /// A scripted core answers two requests in reverse order with an event
    /// between them, so routing by ID is exercised, then sends a reply nobody
    /// asked for, which breaks the channel.
    #[test]
    fn given_replies_out_of_order_and_events_between_then_each_caller_gets_its_own() {
        let (mut from_ui, to_core) = io::pipe().unwrap();
        let (from_core, mut to_ui) = io::pipe().unwrap();
        let (events_to, events) = channel();
        let link = Arc::new(
            PipeTransport::start(
                from_core,
                to_core,
                Box::new(move |event| events_to.send(event).unwrap()),
            )
            .unwrap(),
        );
        let status = {
            let link = link.clone();
            std::thread::spawn(move || link.call(Call::AppStatus {}))
        };
        let budgets = ResourceBudgets::default();
        let first: Request = receive(&mut from_ui, &budgets).unwrap();
        let page = {
            let link = link.clone();
            std::thread::spawn(move || {
                link.call(Call::TimelinePage {
                    room_id: "!r:example.invalid".into(),
                    cursor: None,
                })
            })
        };
        let second: Request = receive(&mut from_ui, &budgets).unwrap();
        assert_eq!(first.call, Call::AppStatus {});

        let room_id = "!r:example.invalid".to_string();
        send(
            &mut to_ui,
            &CoreMessage::Event(Event::RoomChanged {
                room_id: room_id.clone(),
            }),
        )
        .unwrap();
        reply(&mut to_ui, second.id, Outcome::Error(ErrorCode::NotFound));
        reply(
            &mut to_ui,
            first.id,
            Outcome::Ok(serde_json::json!("status")),
        );

        assert_eq!(
            page.join().unwrap(),
            Err(CoreError::Core(ErrorCode::NotFound))
        );
        assert_eq!(status.join().unwrap(), Ok(serde_json::json!("status")));
        assert_eq!(events.recv().unwrap(), Event::RoomChanged { room_id });

        reply(&mut to_ui, 999, Outcome::Ok(serde_json::Value::Null));
        while !lock(&link.waiting).broken {
            std::thread::yield_now();
        }
        assert_eq!(link.call(Call::AppStatus {}), Err(CoreError::Unavailable));
    }

    #[test]
    fn given_the_core_closes_then_waiting_and_later_calls_are_unavailable() {
        let (mut from_ui, to_core) = io::pipe().unwrap();
        let (from_core, to_ui) = io::pipe().unwrap();
        let link = Arc::new(PipeTransport::start(from_core, to_core, Box::new(|_| {})).unwrap());
        let waiting = {
            let link = link.clone();
            std::thread::spawn(move || link.call(Call::AppStatus {}))
        };
        let _: Request = receive(&mut from_ui, &ResourceBudgets::default()).unwrap();
        drop(to_ui);
        assert_eq!(waiting.join().unwrap(), Err(CoreError::Unavailable));
        assert_eq!(link.call(Call::AppStatus {}), Err(CoreError::Unavailable));
    }
}

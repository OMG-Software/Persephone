//! The UI's connection to the key-holding core (ADR-0004).
//!
//! The core starts the UI with a pipe on the UI's stdin and stdout. The UI
//! takes both ends at startup as non-inheritable handles and points its
//! standard handles at `NUL`, so nothing printed later and no web view child
//! process can reach the core channel. Each call is one bounded request frame
//! answered by one bounded reply frame; between replies the core may send
//! content-free change events. One reader thread routes both.

use persephone_core_protocol::{
    Call, CoreMessage, ErrorCode, Event, Outcome, Request, receive, send,
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
    fn call(&self, call: Call) -> Result<serde_json::Value, CoreError>;
}

pub type CoreLink = Arc<dyn CoreTransport>;

/// Receives the core's events on the reader thread.
pub type EventSink = Box<dyn Fn(Event) + Send>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Default)]
struct Waiting {
    replies: HashMap<u32, SyncSender<Outcome>>,
    broken: bool,
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
                loop {
                    match receive::<_, CoreMessage>(&mut from_core, &budgets) {
                        Ok(CoreMessage::Reply(reply)) => {
                            let Some(caller) = lock(&reader_waiting).replies.remove(&reply.id)
                            else {
                                break;
                            };
                            let _ = caller.send(reply.outcome);
                        }
                        Ok(CoreMessage::Event(event)) => on_event(event),
                        Err(_) => break,
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

impl CoreTransport for PipeTransport {
    fn call(&self, call: Call) -> Result<serde_json::Value, CoreError> {
        let (reply_to, reply) = sync_channel(1);
        {
            let mut writer = lock(&self.writer);
            let id = writer.next_id;
            writer.next_id = writer.next_id.wrapping_add(1);
            {
                let mut waiting = lock(&self.waiting);
                if waiting.broken || waiting.replies.contains_key(&id) {
                    return Err(CoreError::Unavailable);
                }
                waiting.replies.insert(id, reply_to);
            }
            // A failed write leaves the stream position unknown, so the
            // channel is not reused.
            if send(&mut writer.to_core, &Request { id, call }).is_err() {
                lock(&self.waiting).break_channel();
                return Err(CoreError::Unavailable);
            }
        }
        match reply.recv() {
            Ok(Outcome::Ok(value)) => Ok(value),
            Ok(Outcome::Error(code)) => Err(CoreError::Core(code)),
            Err(_) => Err(CoreError::Unavailable),
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

#[cfg(not(windows))]
fn take_stdio() -> io::Result<(File, File)> {
    // Linux and macOS handling of the inherited descriptors is not built yet.
    Err(io::Error::other(
        "the core channel is implemented for Windows only",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use persephone_core_protocol::Reply;
    use std::sync::mpsc::channel;

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

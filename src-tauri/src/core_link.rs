//! The UI's connection to the key-holding core (ADR-0004).
//!
//! The core starts the UI with a pipe on the UI's stdin and stdout. The UI
//! takes both ends at startup as non-inheritable handles and points its
//! standard handles at `NUL`, so nothing printed later and no web view child
//! process can reach the core channel. Each call is one bounded request frame
//! and one bounded reply frame.

use persephone_core_protocol::{Call, ErrorCode, Outcome, Reply, Request, receive, send};
use persephone_media_protocol::ResourceBudgets;
use std::{
    fs::File,
    io,
    sync::{Arc, Mutex},
};

#[derive(Debug, PartialEq, Eq)]
pub enum CoreError {
    /// The channel is closed or a previous call broke it.
    Unavailable,
    /// The core answered with an error code.
    Core(ErrorCode),
    /// The core's reply did not match the request.
    BadReply,
}

/// Something that can answer calls on the core's behalf: the real pipe, or a
/// stand-in in tests.
pub trait CoreTransport: Send + Sync {
    fn call(&self, call: Call) -> Result<serde_json::Value, CoreError>;
}

pub type CoreLink = Arc<dyn CoreTransport>;

struct PipeState {
    from_core: File,
    to_core: File,
    next_id: u32,
    broken: bool,
}

/// One request at a time over the pipe the core created.
pub struct PipeTransport {
    state: Mutex<PipeState>,
    budgets: ResourceBudgets,
}

impl PipeTransport {
    pub fn new(from_core: File, to_core: File) -> Self {
        Self {
            state: Mutex::new(PipeState {
                from_core,
                to_core,
                next_id: 1,
                broken: false,
            }),
            budgets: ResourceBudgets::default(),
        }
    }

    /// Takes the core channel from this process's stdin and stdout.
    pub fn from_stdio() -> io::Result<Self> {
        let (from_core, to_core) = take_stdio()?;
        Ok(Self::new(from_core, to_core))
    }
}

impl CoreTransport for PipeTransport {
    fn call(&self, call: Call) -> Result<serde_json::Value, CoreError> {
        let mut state = self.state.lock().map_err(|_| CoreError::Unavailable)?;
        if state.broken {
            return Err(CoreError::Unavailable);
        }
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        // Any failure leaves the stream position unknown, so the channel is
        // not reused.
        let reply = send(&mut state.to_core, &Request { id, call })
            .ok()
            .and_then(|()| receive::<_, Reply>(&mut state.from_core, &self.budgets).ok());
        let Some(reply) = reply else {
            state.broken = true;
            return Err(CoreError::Unavailable);
        };
        if reply.id != id {
            state.broken = true;
            return Err(CoreError::BadReply);
        }
        match reply.outcome {
            Outcome::Ok(value) => Ok(value),
            Outcome::Error(code) => Err(CoreError::Core(code)),
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

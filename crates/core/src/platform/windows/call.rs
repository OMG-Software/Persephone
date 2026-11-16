//! The Windows call host: the call worker runs in the call AppContainer
//! (`internetClient` and `microphone` only) under a Job, and the core checks
//! its `Ready` message before it hands out the link (ADR-0043).

use crate::calls::worker::{
    CALL_WORKER_EXE, CALL_WORKER_PROFILE, COMMAND_QUEUE, CallHost, CallWorkerLink, EVENT_QUEUE,
    READY_TIMEOUT, StartError, WorkerProcess,
};
use persephone_call_protocol::{FromWorker, ReadError, ToWorker, read_message, write_message};
use persephone_media_broker::{
    ContainmentProfile, LongLivedWorker, StartedCallWorker, start_call_worker,
};
use persephone_media_protocol::ResourceBudgets;
use std::{
    io::Read,
    path::PathBuf,
    ptr::null_mut,
    sync::{Arc, Mutex, mpsc as std_mpsc},
    thread,
    time::Duration,
};
use tokio::sync::mpsc;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW,
};

/// What the user reads before a call connects around the SOCKS5 proxy.
const DIRECT_CONNECTION_TEXT: &str = concat!(
    "This profile uses a SOCKS5 proxy, but calls cannot use it.\n\n",
    "If you continue, this call connects directly. The call server and the ",
    "TURN server will see this computer's IP address. Messages and all other ",
    "connections still use the proxy.\n\n",
    "Connect this call directly?",
);

/// Starts the call worker in its AppContainer and Job.
pub struct ContainedCallHost {
    exe: PathBuf,
    profile: Mutex<Option<Arc<ContainmentProfile>>>,
}

impl ContainedCallHost {
    /// The call worker executable next to the running core.
    pub fn next_to_core() -> Self {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|core| core.parent().map(|dir| dir.join(CALL_WORKER_EXE)))
            .unwrap_or_else(|| PathBuf::from(CALL_WORKER_EXE));
        Self::at(exe)
    }

    /// The call worker at `exe`; the profile is created on first use and
    /// kept.
    pub fn at(exe: PathBuf) -> Self {
        Self {
            exe,
            profile: Mutex::new(None),
        }
    }

    fn profile(&self) -> Result<Arc<ContainmentProfile>, StartError> {
        let mut profile = self
            .profile
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(profile) = profile.as_ref() {
            return Ok(profile.clone());
        }
        let created = Arc::new(
            ContainmentProfile::create_or_open(CALL_WORKER_PROFILE)
                .map_err(|_| StartError::Launch)?,
        );
        *profile = Some(created.clone());
        Ok(created)
    }
}

/// The Job and process of the worker, as the link needs them.
struct JobProcess(LongLivedWorker);

impl WorkerProcess for JobProcess {
    fn kill(&self) {
        self.0.kill();
    }

    fn wait_exit(&self, timeout: Duration) -> bool {
        self.0.wait_exit(timeout)
    }
}

/// Decides from the worker's first message. `None` means no message came:
/// the time ran out or the pipe closed.
fn judge_first_message(first: Option<Result<FromWorker, ReadError>>) -> Result<(), StartError> {
    match first {
        Some(Ok(FromWorker::Ready {
            containment_bits: 0,
        })) => Ok(()),
        Some(Ok(FromWorker::Ready { containment_bits })) => {
            Err(StartError::Containment(containment_bits))
        }
        _ => Err(StartError::NotReady),
    }
}

/// Reads the worker's messages. The first goes to `first`; the rest go to
/// `events`. It stops on any read error or when the receiver is gone, which
/// drops `events` so the controller sees the end.
fn read_loop<R: Read>(
    mut reader: R,
    first: std_mpsc::Sender<Result<FromWorker, ReadError>>,
    events: mpsc::Sender<FromWorker>,
) {
    let budgets = ResourceBudgets::default();
    let opening = read_message::<_, FromWorker>(&mut reader, &budgets);
    let usable = matches!(
        opening,
        Ok(FromWorker::Ready {
            containment_bits: 0
        })
    );
    if first.send(opening).is_err() || !usable {
        return;
    }
    while let Ok(message) = read_message::<_, FromWorker>(&mut reader, &budgets) {
        if events.blocking_send(message).is_err() {
            return;
        }
    }
}

/// Writes the controller's messages until the queue closes or a write fails.
fn write_loop<W: std::io::Write>(mut writer: W, mut commands: mpsc::Receiver<ToWorker>) {
    while let Some(message) = commands.blocking_recv() {
        if write_message(&mut writer, &message).is_err() {
            return;
        }
    }
}

impl CallHost for ContainedCallHost {
    fn start(&self) -> Result<CallWorkerLink, StartError> {
        if !self.exe.is_file() {
            return Err(StartError::NoExe);
        }
        let profile = self.profile()?;
        let StartedCallWorker {
            worker,
            stdin,
            stdout,
        } = start_call_worker(&profile, &self.exe).map_err(|_| StartError::Launch)?;
        let process = JobProcess(worker);

        let (first_tx, first_rx) = std_mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel(EVENT_QUEUE);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE);
        let reader = thread::Builder::new()
            .name("call-worker-read".into())
            .spawn(move || read_loop(stdout, first_tx, event_tx));
        if reader.is_err() {
            process.kill();
            return Err(StartError::Launch);
        }
        let first = first_rx.recv_timeout(READY_TIMEOUT).ok();
        if let Err(error) = judge_first_message(first) {
            process.kill();
            return Err(error);
        }
        let writer = thread::Builder::new()
            .name("call-worker-write".into())
            .spawn(move || write_loop(stdin, command_rx));
        if writer.is_err() {
            process.kill();
            return Err(StartError::Launch);
        }
        Ok(CallWorkerLink::from_parts(
            command_tx,
            event_rx,
            Box::new(process),
        ))
    }

    fn confirm_direct_connection(&self) -> bool {
        let text: Vec<u16> = DIRECT_CONNECTION_TEXT
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let caption: Vec<u16> = "Persephone".encode_utf16().chain(Some(0)).collect();
        // SAFETY: both strings are NUL-terminated and live for the call; the
        // box has no owner window.
        let answer = unsafe {
            MessageBoxW(
                null_mut(),
                text.as_ptr(),
                caption.as_ptr(),
                MB_YESNO | MB_DEFBUTTON2 | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
            )
        };
        answer == IDYES
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn first_of(messages: &[FromWorker]) -> Option<Result<FromWorker, ReadError>> {
        let mut wire = Vec::new();
        for message in messages {
            write_message(&mut wire, message).unwrap();
        }
        Some(read_message(
            &mut Cursor::new(wire),
            &ResourceBudgets::default(),
        ))
    }

    /// GIVEN a worker whose first message is Ready with no missing control
    /// WHEN the core judges it THEN the start goes on.
    #[test]
    fn given_ready_with_clear_bits_then_start_goes_on() {
        let first = first_of(&[FromWorker::Ready {
            containment_bits: 0,
        }]);
        assert_eq!(judge_first_message(first), Ok(()));
    }

    /// GIVEN a worker that reports missing controls WHEN the core judges it
    /// THEN the start fails with those bits.
    #[test]
    fn given_ready_with_bits_then_start_fails_with_containment() {
        let first = first_of(&[FromWorker::Ready {
            containment_bits: 0x11,
        }]);
        assert_eq!(
            judge_first_message(first),
            Err(StartError::Containment(0x11))
        );
    }

    /// GIVEN a worker whose first message is not Ready WHEN the core judges
    /// it THEN the start fails as not ready.
    #[test]
    fn given_other_first_message_then_start_fails_not_ready() {
        let first = first_of(&[FromWorker::Connected]);
        assert_eq!(judge_first_message(first), Err(StartError::NotReady));
    }

    /// GIVEN a closed pipe, garbage, or no message in time WHEN the core
    /// judges it THEN the start fails as not ready.
    #[test]
    fn given_eof_garbage_or_timeout_then_start_fails_not_ready() {
        let empty = Some(read_message::<_, FromWorker>(
            &mut Cursor::new(Vec::new()),
            &ResourceBudgets::default(),
        ));
        let garbage = Some(read_message::<_, FromWorker>(
            &mut Cursor::new(vec![0xff; 64]),
            &ResourceBudgets::default(),
        ));
        for first in [empty, garbage, None] {
            assert_eq!(judge_first_message(first), Err(StartError::NotReady));
        }
    }

    /// GIVEN a worker that sends Ready and then events WHEN the reader runs
    /// THEN Ready goes to the handshake only and the events follow in order.
    #[test]
    fn given_ready_then_events_when_read_then_events_follow_the_handshake() {
        let mut wire = Vec::new();
        for message in [
            FromWorker::Ready {
                containment_bits: 0,
            },
            FromWorker::Connected,
        ] {
            write_message(&mut wire, &message).unwrap();
        }
        let (first_tx, first_rx) = std_mpsc::channel();
        let (event_tx, mut event_rx) = mpsc::channel(EVENT_QUEUE);
        read_loop(Cursor::new(wire), first_tx, event_tx);
        assert_eq!(
            first_rx.recv().unwrap(),
            Ok(FromWorker::Ready {
                containment_bits: 0
            })
        );
        assert_eq!(event_rx.try_recv().unwrap(), FromWorker::Connected);
        // The sender was dropped at the end of the stream.
        assert!(event_rx.try_recv().is_err());
    }

    /// GIVEN a worker that reports missing controls WHEN the reader runs
    /// THEN it forwards no event after the handshake.
    #[test]
    fn given_ready_with_bits_when_read_then_no_event_is_forwarded() {
        let mut wire = Vec::new();
        for message in [
            FromWorker::Ready {
                containment_bits: 1,
            },
            FromWorker::Connected,
        ] {
            write_message(&mut wire, &message).unwrap();
        }
        let (first_tx, _first_rx) = std_mpsc::channel();
        let (event_tx, mut event_rx) = mpsc::channel(EVENT_QUEUE);
        read_loop(Cursor::new(wire), first_tx, event_tx);
        assert!(event_rx.try_recv().is_err());
    }

    /// GIVEN the built call worker WHEN the host starts it THEN it reports
    /// Ready with no missing control and the link ends it. Needs the
    /// worker built under `.local/build` (see `docs/development.md` and
    /// `crates/call-worker/AGENTS.md`). Run with
    /// `cargo test --locked -p persephone-core --lib platform::windows::call -- --ignored`.
    #[test]
    #[ignore = "needs the built persephone-call-worker.exe under .local/build"]
    fn given_the_built_worker_when_started_then_it_is_ready_and_contained() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/build");
        let exe = ["debug", "release"]
            .iter()
            .flat_map(|profile| {
                ["rust", "call-worker", "rust-call"]
                    .iter()
                    .map(|dir| root.join(dir).join(profile).join(CALL_WORKER_EXE))
                    .collect::<Vec<_>>()
            })
            .find(|path| path.is_file())
            .expect("persephone-call-worker.exe not found under .local/build");
        let link = ContainedCallHost::at(exe)
            .start()
            .expect("the contained worker reports Ready with bits 0");
        link.finish(Duration::from_secs(5));
    }
}

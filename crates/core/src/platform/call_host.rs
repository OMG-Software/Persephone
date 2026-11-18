//! The call host that the three desktop platforms share (ADR-0043, ADR-0049).
//!
//! A [`Launcher`] starts the call worker under its OS's containment. The
//! host reads the worker's `Ready` message within [`READY_TIMEOUT`], requires
//! every containment bit to be clear, asks the OS to confirm the containment
//! (Linux and macOS: after `Ready` and before any input) and only then starts
//! the writer and returns the link. The read and write threads, the `Ready`
//! gate and the picture store wiring are the same on every OS.

use crate::calls::video::VideoStore;
use crate::calls::worker::{
    CALL_WORKER_EXE, COMMAND_QUEUE, CallHost, CallWorkerLink, CameraPath, EVENT_QUEUE,
    READY_TIMEOUT, StartError, WorkerProcess,
};
use persephone_call_protocol::{
    FromWorker, ReadError, ToWorker, WorkerItem, read_message, read_worker_item, write_message,
};
use persephone_media_protocol::ResourceBudgets;
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::mpsc as std_mpsc,
    thread,
};
use tokio::sync::mpsc;

/// What the user reads before a call connects around the SOCKS5 proxy.
#[cfg_attr(
    not(any(windows, target_os = "linux", target_os = "macos")),
    allow(dead_code)
)]
pub const DIRECT_CONNECTION_TEXT: &str = concat!(
    "This profile uses a SOCKS5 proxy, but calls cannot use it.\n\n",
    "If you continue, this call connects directly. The call server and the ",
    "TURN server will see this computer's IP address. Messages and all other ",
    "connections still use the proxy.\n\n",
    "Connect this call directly?",
);

/// The containment bits reported when the worker said it was contained but
/// the OS check after `Ready` did not confirm it. The worker's own bits use
/// the low bits and bit 31, so this one cannot collide with them.
pub const NOT_CONFIRMED_BITS: u32 = 1 << 30;

/// A started worker: its process, the core's ends of its pipes and, on Unix,
/// of its camera channel.
pub struct Launched {
    pub process: Box<dyn WorkerProcess>,
    /// Writes to the worker's standard input.
    pub stdin: File,
    /// Reads from the worker's standard output.
    pub stdout: File,
    /// The core's end of the camera channel (Linux only).
    #[cfg(unix)]
    pub camera_channel: Option<std::os::unix::net::UnixStream>,
}

/// One OS's way to start the call worker contained and to ask the user about
/// a direct connection.
pub trait Launcher: Send + Sync {
    /// Starts the worker at `exe` under the OS containment with this camera
    /// path. The worker must get no input before the host has checked its
    /// `Ready` message and [`WorkerProcess::confirm_contained`].
    fn launch(&self, exe: &Path, camera: CameraPath) -> Result<Launched, StartError>;

    /// The camera path to start the next worker with. Linux decides it from
    /// the session (ADR-0049); the other OSes have none.
    fn camera_path(&self) -> CameraPath {
        CameraPath::None
    }

    /// Asks, in a native dialog owned by the core, whether the call may
    /// connect directly. `true` only for an explicit yes.
    fn confirm_direct_connection(&self) -> bool;
}

/// Starts the call worker through `L` and returns the link once the worker is
/// ready and contained.
pub struct ContainedCallHost<L> {
    exe: PathBuf,
    launcher: L,
}

impl<L: Launcher> ContainedCallHost<L> {
    /// The call worker at `exe`.
    pub fn with_launcher(exe: PathBuf, launcher: L) -> Self {
        Self { exe, launcher }
    }

    /// The call worker executable next to the running core.
    pub fn next_to_core_with(launcher: L) -> Self {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|core| core.parent().map(|dir| dir.join(CALL_WORKER_EXE)))
            .unwrap_or_else(|| PathBuf::from(CALL_WORKER_EXE));
        Self::with_launcher(exe, launcher)
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
/// `events`, except video pictures: those go to `video`, which keeps the
/// newest one per tile and never blocks this thread (ADR-0048). A tile's
/// `VideoStarted` or `VideoStopped` is counted in `video` before it is
/// passed on, so the tile's next pictures wait until the controller has
/// applied it. It stops on
/// any read error, including a bad or missing pixel frame, or when the
/// receiver is gone, which drops `events` so the controller sees the end.
fn read_loop<R: Read>(
    mut reader: R,
    first: std_mpsc::Sender<Result<FromWorker, ReadError>>,
    events: mpsc::Sender<FromWorker>,
    video: VideoStore,
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
    while let Ok(item) = read_worker_item(&mut reader, &budgets) {
        match item {
            WorkerItem::Message(message) => {
                if let FromWorker::VideoStarted { tile, .. } | FromWorker::VideoStopped { tile } =
                    &message
                {
                    video.stream_changed(*tile);
                }
                if events.blocking_send(message).is_err() {
                    return;
                }
            }
            // A picture the store refuses is dropped; it is not an error.
            WorkerItem::Video {
                tile,
                rotation,
                width,
                height,
                rgba,
            } => {
                video.insert(tile, rotation, width, height, rgba);
            }
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

impl<L: Launcher> CallHost for ContainedCallHost<L> {
    fn start(&self) -> Result<CallWorkerLink, StartError> {
        if !self.exe.is_file() {
            return Err(StartError::NoExe);
        }
        let camera_path = self.launcher.camera_path();
        let Launched {
            process,
            stdin,
            stdout,
            #[cfg(unix)]
            camera_channel,
        } = self.launcher.launch(&self.exe, camera_path)?;

        let (first_tx, first_rx) = std_mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel(EVENT_QUEUE);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE);
        let video = VideoStore::new();
        let reader_video = video.clone();
        let reader = thread::Builder::new()
            .name("call-worker-read".into())
            .spawn(move || read_loop(stdout, first_tx, event_tx, reader_video));
        if reader.is_err() {
            process.kill();
            return Err(StartError::Launch);
        }
        let first = first_rx.recv_timeout(READY_TIMEOUT).ok();
        if let Err(error) = judge_first_message(first) {
            process.kill();
            return Err(error);
        }
        // The worker says it is contained. Ask the OS before any input goes
        // to it (Linux and macOS; Windows contains the worker before it runs).
        if !process.confirm_contained() {
            process.kill();
            return Err(StartError::Containment(NOT_CONFIRMED_BITS));
        }
        let writer = thread::Builder::new()
            .name("call-worker-write".into())
            .spawn(move || write_loop(stdin, command_rx));
        if writer.is_err() {
            process.kill();
            return Err(StartError::Launch);
        }
        let link = CallWorkerLink::from_parts(command_tx, event_rx, process)
            .with_video(video)
            .with_camera_path(camera_path);
        #[cfg(unix)]
        let link = match camera_channel {
            Some(channel) => link.with_camera_channel(channel),
            None => link,
        };
        Ok(link)
    }

    fn confirm_direct_connection(&self) -> bool {
        self.launcher.confirm_direct_connection()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use persephone_call_protocol::write_video_frame;
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
        read_loop(Cursor::new(wire), first_tx, event_tx, VideoStore::new());
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

    /// GIVEN a worker that sends a picture between two events WHEN the reader
    /// runs THEN the events follow in order without it, and the picture is in
    /// the store, not in the event queue.
    #[test]
    fn given_a_picture_between_events_when_read_then_it_goes_to_the_store() {
        let mut wire = Vec::new();
        write_message(
            &mut wire,
            &FromWorker::Ready {
                containment_bits: 0,
            },
        )
        .unwrap();
        write_message(&mut wire, &FromWorker::Connected).unwrap();
        write_video_frame(&mut wire, 1, 2, 2, 2, &[7; 16]).unwrap();
        write_message(&mut wire, &FromWorker::Left).unwrap();
        let (first_tx, _first_rx) = std_mpsc::channel();
        let (event_tx, mut event_rx) = mpsc::channel(EVENT_QUEUE);
        let video = VideoStore::new();
        video.sync(false, [true, false, false, false]);
        read_loop(Cursor::new(wire), first_tx, event_tx, video.clone());
        assert_eq!(event_rx.try_recv().unwrap(), FromWorker::Connected);
        assert_eq!(event_rx.try_recv().unwrap(), FromWorker::Left);
        assert!(event_rx.try_recv().is_err());
        let picture = video.newest(1, 0, 0).expect("kept picture");
        assert_eq!(
            (picture.rotation, picture.width, picture.bytes()[0]),
            (2, 2, 7)
        );
    }

    /// GIVEN tile 1 changes from one stream to another WHEN the new stream's
    /// picture arrives before the controller applies the changes THEN the
    /// store refuses it, and the events still reach the controller in order.
    #[test]
    fn given_a_tile_change_when_read_then_new_pictures_wait_for_the_controller() {
        let mut wire = Vec::new();
        write_message(
            &mut wire,
            &FromWorker::Ready {
                containment_bits: 0,
            },
        )
        .unwrap();
        write_message(&mut wire, &FromWorker::VideoStopped { tile: 1 }).unwrap();
        write_message(
            &mut wire,
            &FromWorker::VideoStarted {
                tile: 1,
                identity: "@b:example.org:DEV".to_owned(),
            },
        )
        .unwrap();
        write_video_frame(&mut wire, 1, 0, 2, 2, &[7; 16]).unwrap();
        let (first_tx, _first_rx) = std_mpsc::channel();
        let (event_tx, mut event_rx) = mpsc::channel(EVENT_QUEUE);
        let video = VideoStore::new();
        video.sync(false, [true, false, false, false]);
        read_loop(Cursor::new(wire), first_tx, event_tx, video.clone());
        assert!(video.newest(1, 0, 0).is_none(), "the picture waited");
        assert_eq!(
            event_rx.try_recv().unwrap(),
            FromWorker::VideoStopped { tile: 1 }
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            FromWorker::VideoStarted { tile: 1, .. }
        ));
        // The controller applies both; the next picture is kept.
        video.stream_applied(1);
        video.stream_applied(1);
        assert!(video.insert(1, 0, 2, 2, vec![8; 16]));
    }

    /// GIVEN a picture for a tile that may not show WHEN read THEN the reader
    /// goes on and the store drops it.
    #[test]
    fn given_a_picture_for_a_closed_tile_when_read_then_it_is_dropped() {
        let mut wire = Vec::new();
        write_message(
            &mut wire,
            &FromWorker::Ready {
                containment_bits: 0,
            },
        )
        .unwrap();
        write_video_frame(&mut wire, 3, 0, 2, 2, &[7; 16]).unwrap();
        write_message(&mut wire, &FromWorker::Connected).unwrap();
        let (first_tx, _first_rx) = std_mpsc::channel();
        let (event_tx, mut event_rx) = mpsc::channel(EVENT_QUEUE);
        let video = VideoStore::new();
        read_loop(Cursor::new(wire), first_tx, event_tx, video.clone());
        assert!(video.newest(3, 0, 0).is_none());
        assert_eq!(event_rx.try_recv().unwrap(), FromWorker::Connected);
    }

    /// GIVEN a picture announcement without its pixel frame WHEN read THEN
    /// the reader stops, so the controller sees a failed worker, and nothing
    /// after it is forwarded.
    #[test]
    fn given_a_picture_without_pixels_when_read_then_the_reader_stops() {
        let mut wire = Vec::new();
        write_message(
            &mut wire,
            &FromWorker::Ready {
                containment_bits: 0,
            },
        )
        .unwrap();
        write_message(
            &mut wire,
            &FromWorker::VideoFrame {
                tile: 1,
                rotation: 0,
            },
        )
        .unwrap();
        write_message(&mut wire, &FromWorker::Connected).unwrap();
        let (first_tx, _first_rx) = std_mpsc::channel();
        let (event_tx, mut event_rx) = mpsc::channel(EVENT_QUEUE);
        let video = VideoStore::new();
        video.sync(true, [true; 4]);
        read_loop(Cursor::new(wire), first_tx, event_tx, video.clone());
        assert!(event_rx.try_recv().is_err());
        assert!(video.newest(1, 0, 0).is_none());
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
        read_loop(Cursor::new(wire), first_tx, event_tx, VideoStore::new());
        assert!(event_rx.try_recv().is_err());
    }

    /// A launcher whose worker is a canned byte stream and whose containment
    /// check answers `contained`. The pipes are real files so that the host's
    /// threads run as they do with a worker.
    #[cfg(unix)]
    fn file_of(pipe: impl Into<std::os::fd::OwnedFd>) -> File {
        File::from(pipe.into())
    }

    #[cfg(windows)]
    fn file_of(pipe: impl Into<std::os::windows::io::OwnedHandle>) -> File {
        File::from(pipe.into())
    }

    struct Scripted {
        ready_bits: u32,
        contained: bool,
        path: CameraPath,
        kills: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    struct ScriptedProcess {
        contained: bool,
        kills: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl WorkerProcess for ScriptedProcess {
        fn kill(&self) {
            self.kills.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        fn wait_exit(&self, _timeout: std::time::Duration) -> bool {
            true
        }

        fn confirm_contained(&self) -> bool {
            self.contained
        }
    }

    impl Launcher for Scripted {
        fn launch(&self, _exe: &Path, camera: CameraPath) -> Result<Launched, StartError> {
            assert_eq!(camera, self.path, "the host passes its camera path on");
            // A pipe holds the canned stdout; a second one takes stdin.
            let (reader, writer) = std::io::pipe().unwrap();
            let mut writer = writer;
            write_message(
                &mut writer,
                &FromWorker::Ready {
                    containment_bits: self.ready_bits,
                },
            )
            .unwrap();
            // Keep the write end open so the reader sees no end of file.
            std::mem::forget(writer);
            let (_unused, sink) = std::io::pipe().unwrap();
            Ok(Launched {
                process: Box::new(ScriptedProcess {
                    contained: self.contained,
                    kills: self.kills.clone(),
                }),
                stdin: file_of(sink),
                stdout: file_of(reader),
                #[cfg(unix)]
                camera_channel: None,
            })
        }

        fn camera_path(&self) -> CameraPath {
            self.path
        }

        fn confirm_direct_connection(&self) -> bool {
            false
        }
    }

    fn scripted(
        ready_bits: u32,
        contained: bool,
    ) -> (
        ContainedCallHost<Scripted>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let kills = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let exe = std::env::current_exe().expect("this test binary exists");
        let launcher = Scripted {
            ready_bits,
            contained,
            path: CameraPath::V4l2,
            kills: kills.clone(),
        };
        (ContainedCallHost::with_launcher(exe, launcher), kills)
    }

    /// GIVEN a worker that reports Ready with clear bits and that the OS
    /// confirms WHEN the host starts it THEN the link is returned with the
    /// camera path.
    #[test]
    fn given_ready_and_confirmed_when_started_then_the_link_is_returned() {
        let (host, kills) = scripted(0, true);
        let link = host.start().expect("a link");
        assert_eq!(link.camera_path(), CameraPath::V4l2);
        assert_eq!(kills.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// GIVEN a worker whose Ready has clear bits but that the OS does not
    /// confirm as contained WHEN the host starts it THEN it is killed before
    /// any input and the start fails with the not-confirmed bit.
    #[test]
    fn given_ready_but_not_confirmed_when_started_then_the_worker_is_killed() {
        let (host, kills) = scripted(0, false);
        assert_eq!(
            host.start().err(),
            Some(StartError::Containment(NOT_CONFIRMED_BITS))
        );
        assert_eq!(kills.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// GIVEN a worker that reports a missing control WHEN the host starts it
    /// THEN it is killed and the start fails with the worker's bits.
    #[test]
    fn given_ready_with_bits_when_started_then_the_worker_is_killed() {
        let (host, kills) = scripted(1 << 2, true);
        assert_eq!(host.start().err(), Some(StartError::Containment(1 << 2)));
        assert_eq!(kills.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// GIVEN a missing worker file WHEN the host starts THEN it refuses
    /// before it launches anything.
    #[test]
    fn given_no_worker_file_when_started_then_no_exe() {
        let (mut host, kills) = scripted(0, true);
        host.exe = PathBuf::from("/nonexistent/persephone-call-worker");
        assert_eq!(host.start().err(), Some(StartError::NoExe));
        assert_eq!(kills.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}

//! Starting the Windows screen helper (ADR-0067).
//!
//! After the user said Yes and the core read the monitors again, the core
//! starts one helper for the share through the media broker: the user's
//! normal token, a Job, the call worker's mitigations without the Win32k
//! lockdown, no child processes and only its two pipes. The helper captures
//! the monitor and writes pictures to the frame pipe; the core never reads
//! the pipe. It gives the pipe's read end to the call worker
//! (`CallWorkerLink::send_screen_frames`), so the screen never passes
//! through the core or the web view.
//!
//! The helper executable sits next to the core, like the call worker. A
//! missing or failing helper leaves the share `Unavailable`: the core never
//! falls back to capturing in the worker or in itself.

use std::fs::File;
use std::os::windows::io::OwnedHandle;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use persephone_call_protocol::ScreenTarget;
use persephone_call_protocol::screen_helper::HelperStart;
use persephone_media_broker::{ScreenHelperPipes, ScreenHelperProcess, start_screen_helper};

use crate::calls::screen::ScreenRefusal;
use crate::calls::screen_helper::HelperProcess;

/// The screen helper executable next to the core.
pub const SCREEN_HELPER_EXE: &str = "persephone-screen-helper.exe";

/// What a launch gives: the helper process and the read end of its frame
/// pipe.
pub struct LaunchedHelper {
    pub process: Arc<dyn HelperProcess>,
    pub frames: OwnedHandle,
}

/// Something that starts a helper for a share. The real one starts the
/// executable; a test brings its own.
pub trait HelperLaunch: Send + Sync {
    /// Starts the helper for `target` as share `share`. Any failure is
    /// `Unavailable`; nothing else is tried.
    fn launch(&self, share: u32, target: &ScreenTarget) -> Result<LaunchedHelper, ScreenRefusal>;
}

/// Starts the helper executable through the media broker.
pub struct BrokerHelper {
    exe: PathBuf,
}

impl BrokerHelper {
    /// The helper executable next to the running core.
    pub fn next_to_core() -> Self {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|core| core.parent().map(|dir| dir.join(SCREEN_HELPER_EXE)))
            .unwrap_or_else(|| PathBuf::from(SCREEN_HELPER_EXE));
        Self { exe }
    }

    /// The helper executable at `exe`.
    #[cfg(test)]
    pub fn at(exe: PathBuf) -> Self {
        Self { exe }
    }
}

/// The broker's helper process, as the core's share logic uses it. The
/// control pipe's write end is the core's way to ask for the end: dropping
/// it is the request.
struct BrokerProcess {
    process: ScreenHelperProcess,
    control: Mutex<Option<File>>,
}

impl HelperProcess for BrokerProcess {
    fn close_control(&self) {
        let closed = self
            .control
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        drop(closed);
    }

    fn wait_exit(&self, timeout: Duration) -> bool {
        self.process.wait_exit(timeout)
    }

    fn terminate(&self) {
        self.process.terminate();
    }

    fn exit_code(&self) -> Option<u32> {
        self.process.exit_code()
    }

    fn peak_memory(&self) -> usize {
        self.process.peak_memory()
    }
}

impl HelperLaunch for BrokerHelper {
    fn launch(&self, share: u32, target: &ScreenTarget) -> Result<LaunchedHelper, ScreenRefusal> {
        let pipes = ScreenHelperPipes::new().map_err(|_| ScreenRefusal::Unavailable)?;
        // The command line is built from the target the user approved, with
        // the two handle values the helper inherits. A target that is not a
        // monitor the capture takes makes no start.
        let start = HelperStart::new(share, pipes.control_value(), pipes.frames_value(), target)
            .ok_or(ScreenRefusal::Unavailable)?;
        let started = start_screen_helper(&self.exe, &start.to_args(), pipes)
            .map_err(|_| ScreenRefusal::Unavailable)?;
        Ok(LaunchedHelper {
            process: Arc::new(BrokerProcess {
                process: started.process,
                control: Mutex::new(Some(started.control)),
            }),
            frames: started.frames,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::screen_helper::HelperShare;
    use persephone_call_protocol::screen_frame::{ScreenItem, read_item};
    use persephone_call_protocol::screen_helper::EXIT_STOPPED;
    use persephone_call_protocol::{DesktopRect, MonitorName};
    use std::sync::mpsc;

    /// The built helper under `.local/build`, debug first.
    fn built_helper() -> PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/build");
        ["debug", "release"]
            .iter()
            .flat_map(|profile| {
                ["rust"]
                    .iter()
                    .map(|dir| root.join(dir).join(profile).join(SCREEN_HELPER_EXE))
                    .collect::<Vec<_>>()
            })
            .find(|path| path.is_file())
            .expect("persephone-screen-helper.exe not found under .local/build")
    }

    /// GIVEN a target that is not a monitor, or a missing executable WHEN
    /// the helper is launched THEN it is unavailable and nothing runs.
    #[test]
    fn given_no_monitor_or_no_executable_when_launched_then_it_is_unavailable() {
        let launcher = BrokerHelper::at(PathBuf::from(r"C:\Persephone\no-such-helper.exe"));
        let monitor = ScreenTarget::Monitor {
            name: MonitorName::new(br"\\.\DISPLAY1").expect("a name"),
            width: 1920,
            height: 1080,
            rect: DesktopRect::new(0, 0, 1920, 1080).expect("a rectangle"),
        };
        assert!(matches!(
            launcher.launch(1, &monitor),
            Err(ScreenRefusal::Unavailable)
        ));
        let portal = ScreenTarget::Portal { node: 1 };
        assert!(matches!(
            launcher.launch(1, &portal),
            Err(ScreenRefusal::Unavailable)
        ));
        let huge = ScreenTarget::Monitor {
            name: MonitorName::new(br"\\.\DISPLAY1").expect("a name"),
            width: 7000,
            height: 4000,
            rect: DesktopRect::new(0, 0, 7000, 4000).expect("a rectangle"),
        };
        assert!(matches!(
            launcher.launch(1, &huge),
            Err(ScreenRefusal::Unavailable)
        ));
    }

    /// GIVEN the built helper and monitor 0 WHEN the core launches it for a
    /// share THEN the frame pipe delivers one picture of the monitor's size
    /// that is not black, and stop-and-join ends the helper with exit code 0.
    /// It captures the real screen (the yellow border shows for a moment) and
    /// needs the helper built under `.local/build`. Run with
    /// `cargo test --locked -p persephone-core --lib platform::windows::screen_helper -- --ignored`.
    #[test]
    #[ignore = "captures the real screen and needs the built persephone-screen-helper.exe"]
    fn given_the_built_helper_when_launched_then_one_real_picture_arrives() {
        let monitors = super::super::screen::first_capturable_target().expect("a monitor to share");
        let launched = BrokerHelper::at(built_helper())
            .launch(1, &monitors)
            .expect("the helper starts");
        let process = launched.process.clone();
        let helper = HelperShare::start(1, launched.process).expect("a helper share");
        let frames = launched.frames;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut file = File::from(frames);
            let _ = sender.send(read_item(&mut file, 1));
        });
        let item = receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("an item in 30 s");
        let Ok(ScreenItem::Frame(frame)) = item else {
            panic!("a picture expected, got {item:?}");
        };
        let ScreenTarget::Monitor { rect, .. } = monitors else {
            unreachable!("a monitor target");
        };
        let (left, top, right, bottom) = rect.edges();
        assert_eq!(
            (frame.width as i32, frame.height as i32),
            (right - left, bottom - top)
        );
        let (pixels, _) = frame.data.as_chunks::<4>();
        let lit = pixels
            .iter()
            .filter(|pixel| pixel[..3].iter().any(|&byte| byte != 0))
            .count();
        assert!(lit > 0, "the picture is black");
        assert_eq!(helper.stop_and_join(), Some(EXIT_STOPPED));
        // The Job's memory cap is 512 MiB; this is the measured peak.
        let peak = process.peak_memory();
        eprintln!(
            "screen helper peak committed memory: {} MiB for a {}x{} picture",
            peak >> 20,
            frame.width,
            frame.height
        );
        assert!(peak > 0 && peak < 512 << 20, "{peak}");
    }
}

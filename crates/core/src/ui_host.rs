//! Starting, containing, watching and ending the keyless UI process
//! (ADR-0004).
//!
//! The core starts `persephone-ui` next to its own executable with
//! [`STARTED_BY_CORE`] and serves it over the UI's stdin and stdout. Each
//! platform's [`UiHost`] contains the UI before it can create its web view
//! and reports what the window watch (`ui_watch.rs` in the core binary)
//! needs. A platform without implemented containment refuses to start the UI
//! ([`Unsupported`]): the core never starts an uncontained UI.

use std::{
    env, fmt, io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

/// Argument that marks the UI as started by the core; the UI refuses to run
/// as a UI without it.
pub const STARTED_BY_CORE: &str = "--started-by-core";

/// The UI executable's file name, next to the core's own.
pub const UI_EXE: &str = if cfg!(windows) {
    "persephone-ui.exe"
} else {
    "persephone-ui"
};

/// The UI executable next to the running core.
pub fn ui_exe() -> io::Result<PathBuf> {
    Ok(env::current_exe()?.with_file_name(UI_EXE))
}

/// The command that starts the UI at `ui`: [`STARTED_BY_CORE`], stdin and
/// stdout piped for the core channel, stderr inherited. A host adds its own
/// containment before it spawns the command.
pub fn ui_command(ui: &Path) -> Command {
    let mut command = Command::new(ui);
    command
        .arg(STARTED_BY_CORE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    command
}

/// Why the UI did not start. Each is reported as a code only, and the core
/// exits with status 2.
#[derive(Debug)]
pub enum StartError {
    /// No UI containment is implemented for this platform.
    Unsupported,
    /// The core's own executable could not be located.
    NoExe,
    /// The UI process could not be started.
    Spawn { ui: PathBuf, error: io::Error },
    /// The UI started but could not be contained. The host has killed and
    /// reaped it.
    Contain(io::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("UI containment is implemented for Windows only"),
            Self::NoExe => f.write_str("cannot locate its own executable"),
            Self::Spawn { ui, error } => {
                write!(f, "cannot start the UI at {}: {error}", ui.display())
            }
            Self::Contain(error) => write!(f, "cannot contain the UI: {error}"),
        }
    }
}

/// A started, contained UI. The caller takes the child's stdin and stdout
/// for the core channel, waits for the child, and drops `tree` only after
/// that wait.
pub struct StartedUi {
    pub child: Child,
    /// The containment around the UI and its web view processes. Dropping it
    /// ends anything left in it.
    pub tree: Box<dyn UiTree>,
}

/// One platform's way to start and contain the UI, and to ask the user about
/// a failed window.
pub trait UiHost: Send + Sync {
    /// Starts the UI ([`ui_exe`], [`ui_command`]) and contains it before it
    /// can create its web view.
    fn start(&self) -> Result<StartedUi, StartError>;

    /// Asks, in a native dialog owned by the core, whether to start the
    /// window again. Blocks until answered; `true` means retry.
    fn ask_to_retry(&self) -> bool;
}

/// The contained process tree of one UI run: the UI and its web view.
pub trait UiTree: Send + Sync {
    /// Number of processes running in the tree.
    fn active_processes(&self) -> io::Result<u32>;

    /// Ends every process in the tree, then waits up to `timeout` for them
    /// to exit, so a new web view does not start while the old one still
    /// holds its data folder. Returns whether the tree emptied in time.
    fn terminate(&self, timeout: Duration) -> bool;
}

/// The platform's UI host ([`crate::platform`]); [`Unsupported`] where UI
/// containment is not implemented.
pub fn platform() -> Box<dyn UiHost> {
    crate::platform::os::ui_host()
}

/// Refuses to start the UI, so an unsupported platform never runs an
/// uncontained one.
pub struct Unsupported;

impl UiHost for Unsupported {
    fn start(&self) -> Result<StartedUi, StartError> {
        Err(StartError::Unsupported)
    }

    fn ask_to_retry(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN no UI containment WHEN the UI is started THEN it is refused
    /// with the fixed message, and nothing is started.
    #[test]
    fn given_no_containment_then_the_ui_is_not_started() {
        let error = Unsupported.start().err().expect("refused");
        assert!(matches!(error, StartError::Unsupported));
        assert_eq!(
            error.to_string(),
            "UI containment is implemented for Windows only"
        );
        assert!(!Unsupported.ask_to_retry());
    }

    /// GIVEN the UI path WHEN its command is built THEN it carries only the
    /// started-by-core marker.
    #[test]
    fn given_the_ui_path_then_the_command_passes_only_the_marker() {
        let command = ui_command(Path::new("persephone-ui"));
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, [STARTED_BY_CORE]);
        assert_eq!(ui_exe().unwrap().file_name().unwrap(), UI_EXE);
    }
}

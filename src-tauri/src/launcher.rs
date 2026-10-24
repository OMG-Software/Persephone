//! Launcher mode: `persephone-ui` started without the core's marker.
//!
//! The application's entry process is the core, `persephone` (ADR-0004). A
//! bundle or a user may still start `persephone-ui` directly. Then, before
//! any Tauri or channel state exists, it starts the core next to its own
//! executable instead: on Unix it replaces itself with the core (`exec`, same
//! process ID, no UI code runs); on Windows it starts the core detached and
//! exits 0. No argument is passed on. The core then starts
//! `persephone-ui --started-by-core` as usual.

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Command,
};

/// The core executable's file name, next to the UI's own.
pub const CORE_EXE: &str = if cfg!(windows) {
    "persephone.exe"
} else {
    "persephone"
};

/// What this process was started as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Started by the core: run the UI on the core channel.
    Ui,
    /// Started directly: start the core instead.
    Launcher,
}

/// The mode for these process arguments (the program name included): the UI
/// only when one argument is exactly the core's marker.
pub fn mode(args: impl IntoIterator<Item = OsString>, marker: &str) -> Mode {
    if args.into_iter().any(|arg| arg == marker) {
        Mode::Ui
    } else {
        Mode::Launcher
    }
}

/// The core next to the UI executable `ui`. Refused when that is `ui`
/// itself (a core renamed into the UI's place), which would start this
/// launcher again.
pub fn core_next_to(ui: &Path) -> io::Result<PathBuf> {
    let core = ui.with_file_name(CORE_EXE);
    if core == ui {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the core path is this executable",
        ));
    }
    Ok(core)
}

/// Starts the core next to this executable. On Unix this returns only if
/// the `exec` failed; on Windows it returns once the core has started.
pub fn start_core() -> io::Result<()> {
    let core = core_next_to(&std::env::current_exe()?)?;
    let mut command = Command::new(&core);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        command
            .stdin(std::process::Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map(drop)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = command;
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

/// Runs launcher mode and never returns: exit 0 once the core started
/// (Windows), otherwise a short message and exit 2.
pub fn run() -> ! {
    match start_core() {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("persephone-ui: cannot start persephone: {error}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::STARTED_BY_CORE as MARKER;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    /// GIVEN the core's marker among the arguments THEN the UI runs; GIVEN
    /// no arguments, other arguments or a near miss THEN this is a launcher.
    #[test]
    fn given_arguments_then_only_the_exact_marker_runs_the_ui() {
        assert_eq!(mode(args(&["persephone-ui", MARKER]), MARKER), Mode::Ui);
        assert_eq!(
            mode(args(&["persephone-ui", "--x", MARKER]), MARKER),
            Mode::Ui
        );
        for other in [
            &["persephone-ui"][..],
            &["persephone-ui", "--profile", "dir"],
            &["persephone-ui", "--started-by-core=1"],
            &["persephone-ui", "--STARTED-BY-CORE"],
            &["persephone-ui", " --started-by-core"],
        ] {
            assert_eq!(mode(args(other), MARKER), Mode::Launcher, "{other:?}");
        }
    }

    /// GIVEN a non-UTF-8 argument THEN mode selection does not fail and it
    /// is no marker.
    #[cfg(unix)]
    #[test]
    fn given_a_non_utf8_argument_then_it_is_not_the_marker() {
        use std::os::unix::ffi::OsStringExt;
        let odd = OsString::from_vec(vec![0x2d, 0xff, 0xfe]);
        assert_eq!(
            mode([OsString::from("persephone-ui"), odd], MARKER),
            Mode::Launcher
        );
    }

    /// GIVEN the UI's path THEN the core is the sibling with the core's name.
    #[test]
    fn given_the_ui_path_then_the_core_is_next_to_it() {
        let dir = Path::new("install").join("bin");
        let ui = dir.join(if cfg!(windows) {
            "persephone-ui.exe"
        } else {
            "persephone-ui"
        });
        assert_eq!(core_next_to(&ui).unwrap(), dir.join(CORE_EXE));
    }

    /// GIVEN this executable is itself named like the core THEN it is
    /// refused, so the launcher never starts itself.
    #[test]
    fn given_the_ui_named_like_the_core_then_it_is_refused() {
        let ui = Path::new("bin").join(CORE_EXE);
        assert_eq!(
            core_next_to(&ui).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

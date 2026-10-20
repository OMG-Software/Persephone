//! `persephone`: the application entry point.
//!
//! Starts the keyless UI process (`persephone-ui`, next to this executable),
//! contains it, and serves its requests over its stdin and stdout. When the UI
//! exits or misbehaves, the core shuts it down and exits.

use persephone_core::{ServeEnd, serve};
use std::{
    env,
    process::{Command, ExitCode, Stdio},
};

#[cfg(windows)]
mod ui_job;

/// Argument that marks the UI as started by the core; the UI refuses to run
/// without it.
const STARTED_BY_CORE: &str = "--started-by-core";

fn main() -> ExitCode {
    let Ok(exe) = env::current_exe() else {
        eprintln!("persephone: cannot locate its own executable");
        return ExitCode::from(2);
    };
    let ui = exe.with_file_name(if cfg!(windows) {
        "persephone-ui.exe"
    } else {
        "persephone-ui"
    });
    let mut child = match Command::new(&ui)
        .arg(STARTED_BY_CORE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "persephone: cannot start the UI at {}: {error}",
                ui.display()
            );
            return ExitCode::from(2);
        }
    };
    // Contain the UI before it creates its web view. The Job holds the UI and
    // its web view processes; closing it on exit ends anything left.
    #[cfg(windows)]
    let job = match ui_job::contain(&child) {
        Ok(job) => job,
        Err(error) => {
            eprintln!("persephone: cannot contain the UI: {error}");
            let _ = child.kill();
            let _ = child.wait();
            return ExitCode::from(2);
        }
    };
    let (Some(from_ui), Some(to_ui)) = (child.stdout.take(), child.stdin.take()) else {
        let _ = child.kill();
        return ExitCode::from(2);
    };
    let end = serve(from_ui, to_ui);
    if end != ServeEnd::UiClosed {
        eprintln!("persephone: stopping the UI: {end:?}");
        let _ = child.kill();
    }
    let status = child.wait();
    #[cfg(windows)]
    drop(job);
    match (end, status) {
        (ServeEnd::UiClosed, Ok(status)) if status.success() => ExitCode::SUCCESS,
        _ => ExitCode::from(1),
    }
}

//! `persephone`: the application entry point.
//!
//! Opens the protected local profile if one is given, starts the keyless UI
//! process (`persephone-ui`, next to this executable), contains it, and serves
//! its requests over its stdin and stdout. When the UI exits or misbehaves, the
//! core shuts it down, closes the stores and exits.
//!
//! Options:
//! * `--profile <dir>` opens (or creates) the encrypted profile in `<dir>`.
//!   Without it nothing is stored.
//! * `--demo-live` posts a synthetic message to the planning room every few
//!   seconds, standing in for incoming Matrix events until the SDK is
//!   integrated.

use persephone_contracts::StoreState;
use persephone_core::{Core, ServeEnd, serve, session::Session, vault};
use std::{
    env,
    ffi::OsString,
    path::PathBuf,
    process::{Command, ExitCode, Stdio},
    thread,
    time::Duration,
};

#[cfg(windows)]
mod ui_job;

/// Argument that marks the UI as started by the core; the UI refuses to run
/// without it.
const STARTED_BY_CORE: &str = "--started-by-core";
const DEMO_ROOM: &str = "!planning:example.invalid";
/// The demo stops after this many messages so memory stays bounded.
const DEMO_MESSAGES: u32 = 1000;
/// How long exit waits for admitted work before giving up on a clean close.
const EXIT_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Options {
    profile: Option<PathBuf>,
    demo_live: bool,
}

fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Options, String> {
    let mut options = Options::default();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--demo-live") => options.demo_live = true,
            Some("--profile") if options.profile.is_none() => {
                let dir = args.next().ok_or("--profile needs a directory")?;
                options.profile = Some(dir.into());
            }
            _ => return Err(format!("unexpected argument {}", arg.to_string_lossy())),
        }
    }
    Ok(options)
}

fn main() -> ExitCode {
    let options = match parse(env::args_os().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("persephone: {error}\nusage: persephone [--profile <dir>] [--demo-live]");
            return ExitCode::from(2);
        }
    };
    let session = match options.profile {
        Some(dir) => Session::open(dir, vault::platform()),
        None => Session::not_configured(),
    };
    let state = session.state();
    if !matches!(state, StoreState::Protected | StoreState::NotConfigured) {
        // A code only: no path or vault detail.
        eprintln!("persephone: local store locked: {state:?}");
    }
    let core = Core::default();
    #[cfg(feature = "dev-disposable-servers")]
    let core = match env::var_os("PERSEPHONE_DEV_PROFILE") {
        Some(path) => {
            eprintln!("persephone: DEVELOPMENT BUILD: credentials come from the harness profile");
            core.with_credentials(std::sync::Arc::new(
                persephone_core::credentials::dev::HarnessProfile::new(path.into()),
            ))
        }
        None => core,
    };
    let core = core.with_session(session);
    let code = run_ui(&core, options.demo_live);
    // Exit locks through the same path as a requested lock: admission closes,
    // admitted work finishes, then the stores close.
    match core.session().lock(EXIT_LOCK_TIMEOUT, || {}).0 {
        StoreState::LockFailed => {
            eprintln!("persephone: a store did not close cleanly");
            ExitCode::from(1)
        }
        StoreState::Locking => {
            eprintln!("persephone: admitted work did not finish; stores were not closed cleanly");
            ExitCode::from(1)
        }
        _ => code,
    }
}

fn run_ui(core: &Core, demo_live: bool) -> ExitCode {
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
    let end = thread::scope(|scope| {
        if demo_live {
            scope.spawn(|| demo(core));
        }
        serve(core, from_ui, to_ui)
    });
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

fn demo(core: &Core) {
    for n in 0..DEMO_MESSAGES {
        if !core.wait_running(Duration::from_secs(3)) {
            return;
        }
        core.post_message(
            DEMO_ROOM,
            "@demo:example.invalid",
            format!("Live demo message {n}"),
        );
    }
}

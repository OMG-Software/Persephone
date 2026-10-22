//! `persephone`: the application entry point.
//!
//! Opens the protected local profile if one is given, starts the keyless UI
//! process (`persephone-ui`, next to this executable), contains it, and serves
//! its requests over its stdin and stdout. When the UI exits or misbehaves, the
//! core shuts it down, closes the stores and exits.
//!
//! On Windows the core also watches the UI's window ([`ui_watch`]). If the
//! page never reaches the core, or the web view's processes exit while the UI
//! keeps running, the core asks in a native dialog whether to start the
//! window again. Retry ends the old UI tree and starts a new UI; the session,
//! its sync and any sign-in keep running in the core.
//!
//! Options:
//! * `--profile <dir>` opens (or creates) the encrypted profile in `<dir>`.
//!   Without it nothing is stored.
//! * `--demo-live` posts a synthetic message to the planning room every few
//!   seconds, standing in for incoming Matrix events until the SDK is
//!   integrated.

use persephone_contracts::StoreState;
use persephone_core::{Core, ServeEnd, serve, session::Session, vault};
#[cfg(windows)]
use std::time::Instant;
use std::{
    env,
    ffi::OsString,
    path::PathBuf,
    process::{Command, ExitCode, Stdio},
    sync::{Condvar, Mutex},
    thread,
    time::Duration,
};

#[cfg(windows)]
mod ui_job;
#[cfg_attr(not(windows), allow(dead_code))]
mod ui_watch;

/// Argument that marks the UI as started by the core; the UI refuses to run
/// without it.
const STARTED_BY_CORE: &str = "--started-by-core";
const DEMO_ROOM: &str = "!planning:example.invalid";
/// The demo stops after this many messages so memory stays bounded.
const DEMO_MESSAGES: u32 = 1000;
/// How long exit waits for admitted work before giving up on a clean close.
const EXIT_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the window watch looks at the page and the UI's Job.
#[cfg(windows)]
const WATCH_PERIOD: Duration = Duration::from_millis(500);
/// How long a replaced UI tree may take to exit before a new UI starts.
#[cfg(windows)]
const REPLACE_TIMEOUT: Duration = Duration::from_secs(10);

/// How one UI run ended.
enum Run {
    Exit(ExitCode),
    /// The window failed and the user asked for a new one.
    Retry,
}

/// What the window watch decided for one UI run.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The run ended on its own.
    None,
    Retry,
    Close,
}

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
            let mut source = persephone_core::credentials::dev::HarnessProfile::new(path.into());
            if let Some(file) = env::var_os("PERSEPHONE_DEV_RECOVERY_FILE") {
                source = source.with_recovery_file(file.into());
            }
            core.with_credentials(std::sync::Arc::new(source))
        }
        None => core,
    };
    let core = core.with_session(session);
    let code = loop {
        match run_ui(&core, options.demo_live) {
            Run::Exit(code) => break code,
            Run::Retry => {
                eprintln!("persephone: starting the window again");
                core.reopen();
            }
        }
    };
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

fn run_ui(core: &Core, demo_live: bool) -> Run {
    // The UI's memory cap and kill-on-exit (ui_job) exist only on Windows;
    // elsewhere the core does not start an uncontained UI.
    if cfg!(not(windows)) {
        eprintln!("persephone: UI containment is implemented for Windows only");
        return Run::Exit(ExitCode::from(2));
    }
    let Ok(exe) = env::current_exe() else {
        eprintln!("persephone: cannot locate its own executable");
        return Run::Exit(ExitCode::from(2));
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
            return Run::Exit(ExitCode::from(2));
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
            return Run::Exit(ExitCode::from(2));
        }
    };
    let (Some(from_ui), Some(to_ui)) = (child.stdout.take(), child.stdin.take()) else {
        let _ = child.kill();
        return Run::Exit(ExitCode::from(2));
    };
    let finished = (Mutex::new(false), Condvar::new());
    let (end, verdict) = thread::scope(|scope| {
        if demo_live {
            scope.spawn(|| demo(core));
        }
        #[cfg(windows)]
        let watcher = scope.spawn(|| watch(core, &job, &finished));
        let end = serve(core, from_ui, to_ui);
        *finished.0.lock().unwrap_or_else(|p| p.into_inner()) = true;
        finished.1.notify_all();
        #[cfg(windows)]
        let verdict = watcher.join().unwrap_or(Verdict::None);
        #[cfg(not(windows))]
        let verdict = Verdict::None;
        (end, verdict)
    });
    if end != ServeEnd::UiClosed && verdict == Verdict::None {
        eprintln!("persephone: stopping the UI: {end:?}");
        let _ = child.kill();
    }
    let status = child.wait();
    #[cfg(windows)]
    drop(job);
    match (verdict, end, status) {
        (Verdict::Retry, _, _) => Run::Retry,
        (Verdict::Close, _, _) => Run::Exit(ExitCode::from(1)),
        (Verdict::None, ServeEnd::UiClosed, Ok(status)) if status.success() => {
            Run::Exit(ExitCode::SUCCESS)
        }
        _ => Run::Exit(ExitCode::from(1)),
    }
}

/// Watches one UI run until it finishes. On a failed window it asks whether
/// to retry, then ends the UI tree either way, which ends the run.
#[cfg(windows)]
fn watch(
    core: &Core,
    job: &std::os::windows::io::OwnedHandle,
    finished: &(Mutex<bool>, Condvar),
) -> Verdict {
    let mut window = ui_watch::Watch::new(Instant::now());
    let (done, wake) = finished;
    let mut done = done.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        done = wake
            .wait_timeout(done, WATCH_PERIOD)
            .unwrap_or_else(|p| p.into_inner())
            .0;
        if *done {
            return Verdict::None;
        }
        let processes = ui_job::active_processes(job).ok();
        let Some(failure) = window.observe(Instant::now(), core.ui_ready(), processes) else {
            continue;
        };
        drop(done);
        // A code only: nothing from the page or the web view.
        eprintln!("persephone: the window failed: {failure:?}");
        let retry = ui_job::ask_to_retry();
        if !ui_job::terminate(job, REPLACE_TIMEOUT) {
            eprintln!("persephone: the old window did not exit; closing");
            return Verdict::Close;
        }
        return if retry {
            Verdict::Retry
        } else {
            Verdict::Close
        };
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

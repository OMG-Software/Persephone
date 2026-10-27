//! `persephone`: the application entry point.
//!
//! Opens the protected local profile if one is given, starts the keyless UI
//! process (`persephone-ui`, next to this executable), contains it, and serves
//! its requests over its stdin and stdout. When the UI exits or misbehaves, the
//! core shuts it down, closes the stores and exits.
//!
//! The platform's [`UiHost`] starts and contains the UI; outside Windows it
//! refuses for now, and the core exits with status 2. The core also watches
//! the UI's window ([`ui_watch`]). If the page never reaches the core, or the
//! web view's processes exit while the UI keeps running, the core asks in a
//! native dialog whether to start the window again. Retry ends the old UI
//! tree and starts a new UI; the session, its sync and any sign-in keep
//! running in the core.
//!
//! On macOS the process main thread is kept for AppKit: the work above runs
//! on a `core-main` thread and the main thread runs
//! `platform::macos::run_main_loop`. Windows and Linux run it on the main
//! thread.
//!
//! Options:
//! * No option opens (or creates) the default profile (ADR-0013,
//!   [`store::default_dir`]). If the OS gives no directory for it, the window
//!   starts `store_unavailable`; nothing falls back to synthetic data.
//! * `--profile <dir>` opens (or creates) the encrypted profile in `<dir>`.
//! * `--account <n>` opens the profile in slot `n` (2 to 8) beside the
//!   default or given one instead (ADR-0015, [`profiles`]). A profile switch
//!   from the window closes it, locks the stores as exit does, starts a new
//!   core with these options for the chosen slot, and exits.
//! * `--demo` serves synthetic rooms instead of a profile. Nothing is stored.
//! * `--demo-live`, only with `--demo`, posts a synthetic message to the
//!   planning room every few seconds.

// Release builds are GUI-subsystem binaries with no console window; stderr
// diagnostics then go nowhere unless the parent redirects them.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use persephone_contracts::StoreState;
use persephone_core::{
    Core, ServeEnd,
    credentials::{self, CredentialSource},
    passphrase::Unlock,
    profiles::{self, Profiles},
    serve,
    session::Session,
    store,
    ui_host::{self, StartedUi, UiHost, UiTree},
    vault,
};
use persephone_core_protocol::MAX_PROFILES;
use std::{
    env,
    ffi::OsString,
    path::PathBuf,
    process::ExitCode,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

mod ui_watch;

const DEMO_ROOM: &str = "!planning:example.invalid";
/// The demo stops after this many messages so memory stays bounded.
const DEMO_MESSAGES: u32 = 1000;
/// How long exit waits for admitted work before giving up on a clean close.
const EXIT_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the window watch looks at the page and the UI's tree.
const WATCH_PERIOD: Duration = Duration::from_millis(500);
/// How long a replaced UI tree may take to exit before a new UI starts.
const REPLACE_TIMEOUT: Duration = Duration::from_secs(10);

/// How one UI run ended.
enum Run {
    Exit(ExitCode),
    /// The window failed and the user asked for a new one.
    Retry,
    /// The user asked for another profile; the window is closed.
    Switch(u32),
}

/// What the window watch decided for one UI run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The run ended on its own.
    None,
    Retry,
    Close,
    /// The user asked for another profile, and the window closed.
    Switch(u32),
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    /// The profile to open; `None` means the default one, unless `demo`.
    profile: Option<PathBuf>,
    /// The profile slot beside it (ADR-0015); `None` is slot 1.
    account: Option<u32>,
    demo: bool,
    demo_live: bool,
}

fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Options, String> {
    let mut options = Options::default();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--demo") => options.demo = true,
            Some("--demo-live") => options.demo_live = true,
            Some("--profile") if options.profile.is_none() => {
                let dir = args.next().ok_or("--profile needs a directory")?;
                options.profile = Some(dir.into());
            }
            Some(profiles::ACCOUNT_OPTION) if options.account.is_none() => {
                let slot = args
                    .next()
                    .and_then(|slot| slot.to_str()?.parse::<u32>().ok())
                    .filter(|slot| (2..=MAX_PROFILES).contains(slot))
                    .ok_or("--account needs a slot number from 2 to 8")?;
                options.account = Some(slot);
            }
            _ => return Err(format!("unexpected argument {}", arg.to_string_lossy())),
        }
    }
    if options.demo && (options.profile.is_some() || options.account.is_some()) {
        return Err("--demo cannot be used with --profile or --account".into());
    }
    if options.demo_live && !options.demo {
        return Err("--demo-live needs --demo".into());
    }
    Ok(options)
}

fn main() -> ExitCode {
    let options = match parse(env::args_os().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!(
                "persephone: {error}\nusage: persephone [--profile <dir>] [--account <n>] | --demo [--demo-live]"
            );
            return ExitCode::from(2);
        }
    };
    #[cfg(target_os = "macos")]
    return macos_main(options);
    #[cfg(not(target_os = "macos"))]
    run(options)
}

/// Runs [`run`] on the `core-main` thread and keeps the main thread for the
/// AppKit run loop (`platform::macos::run_main_loop`).
#[cfg(target_os = "macos")]
fn macos_main(options: Options) -> ExitCode {
    persephone_core::platform::macos::run_main_loop(move || run(options))
}

/// The core's work: open the profile (asking for its passphrase), serve the
/// UI until it ends, then lock the stores on exit.
fn run(options: Options) -> ExitCode {
    let source: Arc<dyn CredentialSource> = Arc::from(credentials::platform());
    #[cfg(feature = "dev-disposable-servers")]
    let source: Arc<dyn CredentialSource> = match env::var_os("PERSEPHONE_DEV_PROFILE") {
        Some(path) => {
            eprintln!("persephone: DEVELOPMENT BUILD: credentials come from the harness profile");
            let mut harness = credentials::dev::HarnessProfile::new(path.into());
            if let Some(file) = env::var_os("PERSEPHONE_DEV_RECOVERY_FILE") {
                harness = harness.with_recovery_file(file.into());
            }
            if let Some(file) = env::var_os("PERSEPHONE_DEV_PASSPHRASE_FILE") {
                harness = harness.with_passphrase_file(file.into());
            }
            if let Some(file) = env::var_os("PERSEPHONE_DEV_IDENTITY_ANSWER_FILE") {
                harness = harness.with_identity_answer_file(file.into());
            }
            Arc::new(harness)
        }
        None => source,
    };
    // The passphrase is asked here, in the core's own window, before the UI
    // starts (ADR-0006). Synthetic data only when asked for (ADR-0013).
    let slots = if options.demo {
        None
    } else {
        options
            .profile
            .clone()
            .or_else(store::default_dir)
            .and_then(|base| {
                Profiles::new(base, options.profile.clone(), options.account.unwrap_or(1))
            })
    };
    let session = match &slots {
        _ if options.demo => Session::not_configured(),
        Some(slots) => Session::open(slots.dir(), vault::platform(), Unlock::new(source.clone())),
        None => Session::unavailable(),
    };
    let state = session.state();
    if !matches!(state, StoreState::Protected | StoreState::NotConfigured) {
        // A code only: no path or vault detail.
        eprintln!("persephone: local store locked: {state:?}");
    }
    let core = Core::default()
        .with_credentials(source)
        .with_session(session);
    let core = match slots {
        Some(slots) => core.with_profiles(slots),
        None => core,
    };
    // The harness answers the image file dialog from a file under `.local`.
    #[cfg(feature = "dev-disposable-servers")]
    let core = match env::var_os("PERSEPHONE_DEV_PICK_FILE") {
        Some(path) => {
            eprintln!("persephone: DEVELOPMENT BUILD: picked images come from the harness file");
            core.with_picker(Arc::new(persephone_core::pick::dev::AnswerFile::new(
                path.into(),
            )))
        }
        None => core,
    };
    let host = ui_host::platform();
    let mut switch = None;
    let code = loop {
        match run_ui(&core, &*host, options.demo_live) {
            Run::Exit(code) => break code,
            Run::Retry => {
                eprintln!("persephone: starting the window again");
                core.reopen();
            }
            Run::Switch(slot) => {
                switch = Some(slot);
                break ExitCode::SUCCESS;
            }
        }
    };
    // Exit locks through the same path as a requested lock: admission closes,
    // admitted work finishes, then the stores close.
    let state = core.session().lock(EXIT_LOCK_TIMEOUT, || {}).0;
    // A switch starts the new core last, once this one's stores are locked.
    // The new core opens another profile, so even a lock that did not finish
    // hands nothing over: this process exits next, and its stores with it.
    if let (Some(slot), Some(slots)) = (switch, core.profiles())
        && let Err(error) = slots.start_core(slot)
    {
        eprintln!("persephone: cannot start the core for the other profile: {error}");
        return ExitCode::from(1);
    }
    match state {
        StoreState::LockFailed => {
            eprintln!("persephone: a store did not close cleanly");
            ExitCode::from(1)
        }
        StoreState::Locking => {
            eprintln!("persephone: admitted work did not finish; stores were not closed cleanly");
            ExitCode::from(1)
        }
        StoreState::Unlocking => {
            // The worker may be opening the stores; the process ends under it.
            eprintln!("persephone: exiting while an unlock was in progress");
            ExitCode::from(1)
        }
        _ => code,
    }
}

fn run_ui(core: &Core, host: &dyn UiHost, demo_live: bool) -> Run {
    // The host contains the UI before it creates its web view; a platform
    // without UI containment refuses here.
    let StartedUi { mut child, tree } = match host.start() {
        Ok(started) => started,
        Err(error) => {
            eprintln!("persephone: {error}");
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
        let watcher = scope.spawn(|| watch(core, host, &*tree, &finished));
        let end = serve(core, from_ui, to_ui);
        *finished.0.lock().unwrap_or_else(|p| p.into_inner()) = true;
        finished.1.notify_all();
        let verdict = watcher.join().unwrap_or(Verdict::None);
        (end, verdict)
    });
    if end != ServeEnd::UiClosed && verdict == Verdict::None {
        eprintln!("persephone: stopping the UI: {end:?}");
        let _ = child.kill();
    }
    let status = child.wait();
    // Closing the tree after the wait ends anything left in it.
    drop(tree);
    match (verdict, end, status) {
        (Verdict::Switch(slot), _, _) => Run::Switch(slot),
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
fn watch(
    core: &Core,
    host: &dyn UiHost,
    tree: &dyn UiTree,
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
        if let Some(slot) = core.profiles().and_then(Profiles::requested) {
            drop(done);
            eprintln!("persephone: switching to another profile");
            if !tree.terminate(REPLACE_TIMEOUT) {
                eprintln!("persephone: the old window did not exit; closing");
                return Verdict::Close;
            }
            return Verdict::Switch(slot);
        }
        let processes = tree.active_processes().ok();
        let Some(failure) = window.observe(Instant::now(), core.ui_ready(), processes) else {
            continue;
        };
        drop(done);
        // A code only: nothing from the page or the web view.
        eprintln!("persephone: the window failed: {failure:?}");
        let retry = host.ask_to_retry();
        if !tree.terminate(REPLACE_TIMEOUT) {
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

#[cfg(test)]
mod tests {
    use super::{Options, parse};
    use std::{ffi::OsString, path::PathBuf};

    fn parsed(args: &[&str]) -> Result<Options, String> {
        parse(args.iter().map(OsString::from))
    }

    /// GIVEN no option THEN the default profile is opened (no profile path,
    /// no demo); GIVEN `--profile` THEN that directory.
    #[test]
    fn given_no_option_or_a_profile_then_a_real_profile_is_opened() {
        assert_eq!(parsed(&[]), Ok(Options::default()));
        assert_eq!(
            parsed(&["--profile", "dir"]),
            Ok(Options {
                profile: Some(PathBuf::from("dir")),
                ..Options::default()
            })
        );
    }

    /// GIVEN `--account`, alone or with `--profile`, THEN that slot beside the
    /// default or given profile is opened.
    #[test]
    fn given_an_account_slot_then_that_profile_beside_the_base_is_opened() {
        assert_eq!(
            parsed(&["--account", "2"]),
            Ok(Options {
                account: Some(2),
                ..Options::default()
            })
        );
        assert_eq!(
            parsed(&["--account", "8", "--profile", "dir"]),
            Ok(Options {
                profile: Some(PathBuf::from("dir")),
                account: Some(8),
                ..Options::default()
            })
        );
    }

    /// GIVEN `--demo`, with or without `--demo-live`, THEN synthetic rooms
    /// are served and no profile is named.
    #[test]
    fn given_demo_then_synthetic_rooms_are_served() {
        let demo = Options {
            demo: true,
            ..Options::default()
        };
        assert_eq!(parsed(&["--demo"]), Ok(demo));
        for args in [&["--demo", "--demo-live"][..], &["--demo-live", "--demo"]] {
            assert_eq!(
                parsed(args),
                Ok(Options {
                    demo: true,
                    demo_live: true,
                    ..Options::default()
                }),
                "{args:?}"
            );
        }
    }

    /// GIVEN demo options mixed with a profile, `--demo-live` alone, a second
    /// profile, a missing directory or an unknown option THEN parsing fails.
    #[test]
    fn given_conflicting_or_unknown_options_then_parsing_fails() {
        for args in [
            &["--demo", "--profile", "dir"][..],
            &["--profile", "dir", "--demo"],
            &["--demo-live"],
            &["--profile", "dir", "--demo-live"],
            &["--profile", "a", "--profile", "b"],
            &["--account"],
            &["--account", "1"],
            &["--account", "9"],
            &["--account", "-2"],
            &["--account", "two"],
            &["--account", "2", "--account", "3"],
            &["--demo", "--account", "2"],
            &["--profile"],
            &["--Demo"],
            &["dir"],
        ] {
            assert!(parsed(args).is_err(), "{args:?}");
        }
    }
}

/// The window watch with the macOS UI tree. WKWebView's processes are XPC
/// services outside the UI's process group, so the tree's count is
/// unreadable and the watch judges the window by readiness only
/// (`platform::macos::ui`).
#[cfg(all(test, target_os = "macos"))]
mod macos_watch_tests {
    use super::ui_watch::{Failure, READY_TIMEOUT, Watch};
    use persephone_core::{
        platform::macos::ui::{ProcessGroup, spawn_in_group},
        ui_host::UiTree,
    };
    use std::{
        process::Command,
        time::{Duration, Instant},
    };

    /// GIVEN a UI running in its own group WHEN the watch observes it THEN a
    /// ready page never fails, and a page that never sends a request fails
    /// only after the timeout, as on Windows.
    #[test]
    fn given_the_macos_ui_tree_then_the_watch_judges_readiness_only() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]);
        let mut child = spawn_in_group(command).expect("start /bin/sh");
        let tree = ProcessGroup::of(&child);
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);

        let mut ready = Watch::new(start);
        for second in 0..600 {
            assert_eq!(
                ready.observe(at(second), true, tree.active_processes().ok()),
                None
            );
        }
        let mut waiting = Watch::new(start);
        let before = READY_TIMEOUT.as_secs() - 1;
        assert_eq!(
            waiting.observe(at(before), false, tree.active_processes().ok()),
            None
        );
        assert_eq!(
            waiting.observe(
                at(READY_TIMEOUT.as_secs()),
                false,
                tree.active_processes().ok()
            ),
            Some(Failure::NotReady)
        );
        assert!(tree.terminate(Duration::from_secs(10)));
        child.wait().expect("reap the UI stand-in");
    }
}

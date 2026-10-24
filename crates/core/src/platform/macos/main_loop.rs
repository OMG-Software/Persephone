//! The process main thread on macOS (ADR-0011).
//!
//! AppKit requires its windows on the main thread, so the core keeps that
//! thread for the AppKit run loop. [`run_main_loop`] starts the core's usual
//! work on a `core-main` thread, runs the application with the Accessory
//! activation policy (no Dock icon or menu bar; the visible app is the UI
//! process), and returns the `core-main` exit code when that thread ends.
//! Prompts and panels reach the main thread through [`main_thread`], which
//! uses the main dispatch queue. The run loop serves that queue, so the
//! passphrase prompt that `Session::open` shows on `core-main` at startup
//! works the same way as later prompts.
//!
//! Without a running main loop (unit tests, or the library used by another
//! program) [`main_thread`] refuses instead of waiting for a loop that never
//! runs, so every prompt fails closed.

use dispatch2::DispatchQueue;
use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventModifierFlags, NSEventType,
    NSMenu, NSMenuItem,
};
use objc2_foundation::{NSPoint, NSString};
use std::{
    panic::{self, AssertUnwindSafe},
    process::ExitCode,
    sync::atomic::{AtomicBool, Ordering},
    thread,
};

/// Stack for the `core-main` thread: the size of the macOS main thread's,
/// whose work it takes over.
const CORE_MAIN_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Exit status when `core-main` panicked, as for a panic in `main`.
const PANIC_EXIT: u8 = 101;

/// Set once the main thread is about to run the AppKit loop, before
/// `core-main` starts. Never cleared: the process exits when the loop ends.
static MAIN_LOOP: AtomicBool = AtomicBool::new(false);

/// Runs `core_main` (the core's usual work) on the `core-main` thread and the
/// AppKit application on this, the main thread, until `core_main` returns.
/// Returns its exit code; 101 if it panicked; 1 if the thread could not
/// start.
///
/// Called from any other thread, there is no AppKit loop to run: `core_main`
/// runs inline and its prompts fail closed.
pub fn run_main_loop(core_main: impl FnOnce() -> ExitCode + Send + 'static) -> ExitCode {
    let Some(mtm) = MainThreadMarker::new() else {
        return core_main();
    };
    let app = NSApplication::sharedApplication(mtm);
    let _accepted = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    // Without a main menu, Command+C and Command+V do nothing in the
    // prompts' text fields. The menu bar is not shown for an Accessory app,
    // but its key equivalents still apply.
    app.setMainMenu(Some(&edit_menu(mtm)));
    // Before `core-main` starts, so its first prompt is queued for the loop
    // instead of refused.
    MAIN_LOOP.store(true, Ordering::Release);
    let core = thread::Builder::new()
        .name("core-main".into())
        .stack_size(CORE_MAIN_STACK_BYTES)
        .spawn(move || {
            let code = panic::catch_unwind(AssertUnwindSafe(core_main))
                .unwrap_or(ExitCode::from(PANIC_EXIT));
            // Runs inside `run` below, between prompts: the main queue does
            // not run a block while another of its blocks shows a modal
            // window.
            DispatchQueue::main().exec_async(stop_application);
            code
        });
    let core = match core {
        Ok(core) => core,
        Err(error) => {
            eprintln!("persephone: cannot start the core thread: {error}");
            return ExitCode::from(1);
        }
    };
    app.run();
    // `run` returns only after `stop_application`, which `core-main`
    // queues as its last step, so this join does not wait long.
    core.join().unwrap_or(ExitCode::from(PANIC_EXIT))
}

/// Stops the application's run loop. `stop:` takes effect only after the
/// next event, so an empty application event follows it.
fn stop_application() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.stop(None);
    let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::ApplicationDefined,
        NSPoint::new(0.0, 0.0),
        NSEventModifierFlags(0),
        0.0,
        0,
        None,
        0,
        0,
        0,
    );
    if let Some(event) = event {
        app.postEvent_atStart(&event, true);
    }
}

/// A main menu with only the standard Edit items, so the text fields of the
/// prompts can cut, copy, paste and select all from the keyboard. It has no
/// Quit item: the core ends only through its own exit path, which locks the
/// stores.
fn edit_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let bar = NSMenu::new(mtm);
    let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Edit"));
    for (title, action, key) in [
        ("Cut", sel!(cut:), "x"),
        ("Copy", sel!(copy:), "c"),
        ("Paste", sel!(paste:), "v"),
        ("Select All", sel!(selectAll:), "a"),
    ] {
        // SAFETY: each action is a standard responder selector with no
        // target: AppKit sends it along the responder chain, which ignores it
        // where no responder implements it.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        edit.addItem(&item);
    }
    // The first item of a main menu is the application menu; it stays empty.
    let application = NSMenuItem::new(mtm);
    application.setSubmenu(Some(&NSMenu::new(mtm)));
    bar.addItem(&application);
    let edit_item = NSMenuItem::new(mtm);
    edit_item.setSubmenu(Some(&edit));
    bar.addItem(&edit_item);
    bar
}

/// Runs `task` on the process main thread and returns its result, blocking
/// the calling thread until it finishes. Called on the main thread, `task`
/// runs inline. A panic in `task` is resumed on the calling thread.
///
/// Returns `None`, without running `task`, when no main loop runs in this
/// process ([`run_main_loop`] was not called), so a caller fails closed
/// instead of waiting forever.
pub fn main_thread<R: Send>(task: impl FnOnce() -> R + Send) -> Option<R> {
    hand_off(
        MainThreadMarker::new().is_some(),
        MAIN_LOOP.load(Ordering::Acquire),
        |work| DispatchQueue::main().exec_sync(work),
        task,
    )
}

/// The decision behind [`main_thread`], with the queue passed in so it can
/// be tested without AppKit: `exec_sync` must run its argument on the loop
/// thread and return after it ran.
fn hand_off<R: Send>(
    on_main: bool,
    loop_running: bool,
    exec_sync: impl FnOnce(Box<dyn FnOnce() + Send + '_>),
    task: impl FnOnce() -> R + Send,
) -> Option<R> {
    if on_main {
        return Some(task());
    }
    if !loop_running {
        return None;
    }
    let mut outcome = None;
    // The panic is caught on the loop thread: unwinding out of a dispatch
    // callout would abort the process.
    exec_sync(Box::new(|| {
        outcome = Some(panic::catch_unwind(AssertUnwindSafe(task)));
    }));
    match outcome? {
        Ok(result) => Some(result),
        Err(payload) => panic::resume_unwind(payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::ThreadId;

    /// A loop thread for the hand-off: each call runs `work` on a new
    /// thread and waits for it, as `exec_sync` does on the main queue.
    fn other_thread(work: Box<dyn FnOnce() + Send + '_>) {
        thread::scope(|scope| scope.spawn(work).join().expect("work does not unwind"));
    }

    /// GIVEN the caller is the main thread WHEN work is handed off THEN it
    /// runs inline, without the queue, even before the loop runs.
    #[test]
    fn given_the_main_thread_then_work_runs_inline() {
        let caller = thread::current().id();
        let ran_on = hand_off(
            true,
            false,
            |_| panic!("the queue must not be used"),
            || thread::current().id(),
        );
        assert_eq!(ran_on, Some(caller));
    }

    /// GIVEN another thread and a running loop WHEN work is handed off THEN
    /// it runs on the loop thread and its result comes back.
    #[test]
    fn given_another_thread_then_work_runs_on_the_loop_and_returns() {
        let caller = thread::current().id();
        let (ran_on, value): (ThreadId, u32) =
            hand_off(false, true, other_thread, || (thread::current().id(), 42))
                .expect("the loop runs");
        assert_ne!(ran_on, caller);
        assert_eq!(value, 42);
    }

    /// GIVEN no main loop WHEN work is handed off from another thread THEN
    /// it is refused and never runs, so a prompt cannot wait forever.
    #[test]
    fn given_no_main_loop_then_work_is_refused() {
        let mut ran = false;
        let result = hand_off(
            false,
            false,
            |_| panic!("the queue must not be used"),
            || ran = true,
        );
        assert_eq!(result, None);
        assert!(!ran);
        // The real hand-off in a test process: no loop runs here.
        assert_eq!(main_thread(|| 1), None);
    }

    /// GIVEN work that panics on the loop thread THEN the loop thread
    /// survives and the panic reaches the caller.
    #[test]
    fn given_panicking_work_then_the_panic_reaches_the_caller() {
        let caught = panic::catch_unwind(|| {
            hand_off(false, true, other_thread, || -> u32 {
                panic!("synthetic prompt failure")
            })
        });
        let payload = caught.expect_err("the panic is resumed");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"synthetic prompt failure")
        );
    }
}

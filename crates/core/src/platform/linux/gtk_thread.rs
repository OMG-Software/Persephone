//! The `core-gtk` thread: the only thread in the core that uses GTK
//! (ADR-0010).
//!
//! GTK must stay on the thread that initialised it. That thread starts on
//! the first prompt, initialises GTK and then runs one job at a time from a
//! bounded queue. A job shows one modal window (its own nested main loop),
//! reads it, destroys it and returns. Between jobs no main loop runs and no
//! core window is open. A caller waits for its job's answer; prompts from
//! several threads are shown one after another.
//!
//! When GTK cannot start (no display in this session), every job is refused
//! at once with [`GtkError::NoDisplay`]; the thread does not try again. A
//! job that panics ends the thread, and every later job is refused with
//! [`GtkError::Stopped`]: the core never shows a window from a GTK state it
//! no longer trusts.

use std::{
    sync::{OnceLock, mpsc},
    thread,
};

/// Jobs that may wait for the thread. A sender beyond this waits for room.
const QUEUE: usize = 8;

type Job = Box<dyn FnOnce() + Send>;

/// Why a job did not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GtkError {
    /// GTK could not be initialised: no display, or no GTK, in this session.
    NoDisplay,
    /// The thread has stopped, or the job ended without an answer.
    Stopped,
}

/// The thread and its job queue.
pub struct GtkThread {
    /// `None` when initialisation failed.
    jobs: Option<mpsc::SyncSender<Job>>,
}

impl GtkThread {
    /// Starts the thread and waits for `init` to run on it first. `init`
    /// returns whether GTK is usable; the product passes [`init_gtk`].
    pub fn start(init: fn() -> bool) -> Self {
        let (ready_sender, ready) = mpsc::sync_channel(1);
        let (jobs, queue) = mpsc::sync_channel::<Job>(QUEUE);
        let spawned = thread::Builder::new()
            .name("core-gtk".into())
            .spawn(move || {
                let usable = init();
                let _ = ready_sender.send(usable);
                if usable {
                    for job in queue {
                        job();
                    }
                }
            });
        let usable = spawned.is_ok() && ready.recv().unwrap_or(false);
        Self {
            jobs: usable.then_some(jobs),
        }
    }

    /// Runs `work` on the thread and waits for its result.
    pub fn run<R: Send + 'static>(
        &self,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> Result<R, GtkError> {
        let jobs = self.jobs.as_ref().ok_or(GtkError::NoDisplay)?;
        let (answer, result) = mpsc::sync_channel(1);
        jobs.send(Box::new(move || {
            let _ = answer.send(work());
        }))
        .map_err(|_| GtkError::Stopped)?;
        result.recv().map_err(|_| GtkError::Stopped)
    }

    /// Queues `work` on the thread without waiting for it.
    pub fn post(&self, work: impl FnOnce() + Send + 'static) -> Result<(), GtkError> {
        let jobs = self.jobs.as_ref().ok_or(GtkError::NoDisplay)?;
        jobs.send(Box::new(work)).map_err(|_| GtkError::Stopped)
    }
}

/// The core's GTK thread, started on first use.
pub fn gtk_thread() -> &'static GtkThread {
    static THREAD: OnceLock<GtkThread> = OnceLock::new();
    THREAD.get_or_init(|| GtkThread::start(init_gtk))
}

/// Initialises GTK on the calling thread. Returns whether it is usable.
pub fn init_gtk() -> bool {
    // GTK would otherwise call setlocale(LC_ALL, "") here, changing the
    // locale under the core's other threads while they run. The prompts
    // have their own fixed words and need no translated GTK labels.
    gtk::disable_setlocale();
    if gtk::init().is_err() {
        return false;
    }
    // Like OFN_DONTADDTORECENT on Windows: a chosen file is not added to
    // the recent-files list.
    if let Some(settings) = gtk::Settings::default() {
        use gtk::prelude::GtkSettingsExt;
        settings.set_gtk_recent_files_enabled(false);
    }
    true
}

/// Lets GTK finish closing a window that was just destroyed: handles the
/// events already waiting, at most `MAX_EVENTS`, without blocking.
pub fn settle() {
    const MAX_EVENTS: usize = 256;
    for _ in 0..MAX_EVENTS {
        if !gtk::events_pending() {
            break;
        }
        gtk::main_iteration_do(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// GIVEN GTK that cannot start (no display) WHEN jobs are sent THEN each
    /// is refused at once and none runs.
    #[test]
    fn given_no_display_then_jobs_are_refused_without_running() {
        static RAN: AtomicUsize = AtomicUsize::new(0);
        let thread = GtkThread::start(|| false);
        assert_eq!(
            thread.run(|| RAN.fetch_add(1, Ordering::SeqCst)).err(),
            Some(GtkError::NoDisplay)
        );
        assert_eq!(
            thread.post(|| {
                RAN.fetch_add(1, Ordering::SeqCst);
            }),
            Err(GtkError::NoDisplay)
        );
        assert_eq!(RAN.load(Ordering::SeqCst), 0);
    }

    /// GIVEN a started thread WHEN jobs run THEN each runs on the one
    /// `core-gtk` thread, in order; GIVEN a job that panics THEN its caller
    /// gets `Stopped` and so does every later job.
    #[test]
    fn given_jobs_then_they_run_in_order_and_a_panic_stops_the_thread() {
        let thread = GtkThread::start(|| true);
        let name = thread
            .run(|| thread::current().name().map(str::to_owned))
            .unwrap();
        assert_eq!(name.as_deref(), Some("core-gtk"));
        let (sender, order) = mpsc::channel();
        for n in 0..3 {
            let sender = sender.clone();
            thread.post(move || sender.send(n).unwrap()).unwrap();
        }
        assert_eq!(thread.run(|| 7).unwrap(), 7);
        assert_eq!(order.try_iter().collect::<Vec<_>>(), [0, 1, 2]);

        let panicked: Result<(), _> = thread.run(|| panic!("synthetic job failure"));
        assert_eq!(panicked, Err(GtkError::Stopped));
        assert_eq!(thread.run(|| 1), Err(GtkError::Stopped));
    }
}

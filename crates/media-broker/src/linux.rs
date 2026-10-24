//! The Linux launcher (ADR-0010). Between fork and exec the worker gets a
//! new session, every descriptor above 2 closed at exec, a parent-death
//! SIGKILL, `no_new_privs` and rlimits: address space at the policy's memory
//! limit, CPU time at its CPU limit rounded up to whole seconds, no file
//! writes, no core dumps, no new processes and few descriptors. The worker
//! then makes itself non-dumpable, installs its seccomp allowlist and reports
//! `WorkerReady`. Before any input is sent the broker reads the worker's
//! `/proc/<pid>/status` and requires `NoNewPrivs: 1`, `Seccomp: 2` and more
//! seccomp filters than the broker's own thread has, so a filter the broker
//! itself runs under (a container's, say) does not count as the worker's.

use crate::{
    LaunchError, WorkerPolicy,
    launch::{Launcher, Spawned},
    unix::{self, Limits, UnixWorker},
};
use std::{ffi::c_uint, fs, io, os::unix::process::CommandExt, path::Path};

/// The Linux containment: nothing to create, since the worker applies its
/// own seccomp filter. It exists so that every OS has the same API.
pub struct SeccompContainment {
    _private: (),
}

impl SeccompContainment {
    /// Linux has no named containment object; `name` is ignored.
    pub fn create_or_open(_name: &str) -> Result<Self, LaunchError> {
        Ok(Self { _private: () })
    }

    /// The same as [`Self::create_or_open`]; there is nothing to delete.
    pub fn temporary(name: &str) -> Result<Self, LaunchError> {
        Self::create_or_open(name)
    }
}

impl Launcher for SeccompContainment {
    type Worker = UnixWorker;

    fn spawn(
        &self,
        worker: &Path,
        policy: &WorkerPolicy,
    ) -> Result<Spawned<UnixWorker>, LaunchError> {
        spawn_program(worker, &[], policy)
    }
}

fn spawn_program(
    program: &Path,
    args: &[&str],
    policy: &WorkerPolicy,
) -> Result<Spawned<UnixWorker>, LaunchError> {
    let mut command = unix::command(program)?;
    command.args(args);
    let limits = Limits::new(policy, true);
    // SAFETY: getpid takes no arguments.
    let broker = unsafe { libc::getpid() };
    // SAFETY: the closure runs between fork and exec and makes only system
    // calls; it allocates nothing.
    unsafe {
        command.pre_exec(move || child_setup(broker, limits));
    }
    unix::spawn(command, confirm)
}

/// An unused prctl argument, passed as a full word: the kernel refuses some
/// options unless the unused arguments are zero.
const NONE: libc::c_ulong = 0;

fn prctl(option: libc::c_int, value: libc::c_ulong) -> io::Result<()> {
    // SAFETY: prctl with integer arguments only.
    if unsafe { libc::prctl(option, value, NONE, NONE, NONE) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Runs in the child between fork and exec. Async-signal-safe.
fn child_setup(broker: libc::pid_t, limits: Limits) -> io::Result<()> {
    unix::new_session()?;
    cloexec_above_stderr()?;
    // The kernel sends this when the thread that started the worker ends;
    // that thread waits for the worker for the whole run.
    prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong)?;
    // The broker may have ended before the signal was armed.
    // SAFETY: getppid takes no arguments.
    if unsafe { libc::getppid() } != broker {
        return Err(io::Error::from_raw_os_error(libc::ESRCH));
    }
    prctl(libc::PR_SET_NO_NEW_PRIVS, 1)?;
    unix::set_limits(limits)
}

/// Marks every descriptor above 2 close-on-exec with one `close_range`
/// call (Linux 5.11), else one by one.
fn cloexec_above_stderr() -> io::Result<()> {
    // SAFETY: close_range with plain integer arguments.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3 as c_uint,
            c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    unix::cloexec_each_above_stderr()
}

/// The `/proc/<pid>/status` fields that show the worker's containment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProcStatus {
    no_new_privs: Option<u64>,
    seccomp: Option<u64>,
    seccomp_filters: Option<u64>,
}

impl ProcStatus {
    fn parse(text: &str) -> Self {
        let mut status = Self::default();
        for line in text.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().parse().ok();
            match name {
                "NoNewPrivs" => status.no_new_privs = value,
                "Seccomp" => status.seccomp = value,
                "Seccomp_filters" => status.seccomp_filters = value,
                _ => {}
            }
        }
        status
    }
}

/// `Seccomp: 2` is filter mode. Filters are inherited, so the worker must
/// have more than the broker's thread that started it. Kernels before 5.9
/// do not report the count; then filter mode counts only if the broker has
/// none of its own.
fn contained(worker: ProcStatus, broker: ProcStatus) -> bool {
    worker.no_new_privs == Some(1)
        && worker.seccomp == Some(2)
        && match (worker.seccomp_filters, broker.seccomp_filters) {
            (Some(worker), Some(broker)) => worker > broker,
            (None, None) => broker.seccomp == Some(0),
            _ => false,
        }
}

/// Reads the worker's status and this thread's (the one that started the
/// worker). Any read or parse failure is "not contained".
fn confirm(pid: libc::pid_t) -> bool {
    let (Ok(worker), Ok(broker)) = (
        fs::read_to_string(format!("/proc/{pid}/status")),
        fs::read_to_string("/proc/thread-self/status"),
    ) else {
        return false;
    };
    contained(ProcStatus::parse(&worker), ProcStatus::parse(&broker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RunError, launch};
    use persephone_media_protocol::{ResourceBudgets, WorkerReady};
    use std::time::Duration;

    fn status(no_new_privs: u64, seccomp: u64, filters: Option<u64>) -> ProcStatus {
        ProcStatus {
            no_new_privs: Some(no_new_privs),
            seccomp: Some(seccomp),
            seccomp_filters: filters,
        }
    }

    #[test]
    fn given_proc_status_text_then_containment_fields_are_read() {
        let text = "Name:\tpersephone-media\nNoNewPrivs:\t1\nSeccomp:\t2\nSeccomp_filters:\t3\n";
        assert_eq!(ProcStatus::parse(text), status(1, 2, Some(3)));
        assert_eq!(ProcStatus::parse("Seccomp:\tx\n"), ProcStatus::default());
    }

    /// GIVEN worker and broker status WHEN they are compared THEN only a
    /// worker with `no_new_privs`, filter mode and a filter of its own is
    /// contained.
    #[test]
    fn given_worker_status_then_only_its_own_filter_with_no_new_privs_counts() {
        let broker = status(0, 0, Some(0));
        assert!(contained(status(1, 2, Some(1)), broker));
        // No seccomp, strict mode, or no no_new_privs.
        assert!(!contained(status(1, 0, Some(0)), broker));
        assert!(!contained(status(1, 1, Some(0)), broker));
        assert!(!contained(status(0, 2, Some(1)), broker));
        // Inside a container that already filters: the inherited filter is
        // not the worker's.
        let filtered = status(0, 2, Some(1));
        assert!(!contained(status(1, 2, Some(1)), filtered));
        assert!(contained(status(1, 2, Some(2)), filtered));
        // Old kernels without the count.
        assert!(contained(status(1, 2, None), status(0, 0, None)));
        assert!(!contained(status(1, 2, None), status(0, 2, None)));
        // Missing fields.
        assert!(!contained(ProcStatus::default(), broker));
        assert!(!contained(status(1, 2, Some(1)), ProcStatus::default()));
    }

    /// A launcher that skips the worker's policy: `sh` reports
    /// `WorkerReady` without any seccomp filter, then sleeps. (No redirection:
    /// `dash` saves descriptors at 10 and above, beyond the worker's limit.)
    struct UnfilteredWorker;

    impl Launcher for UnfilteredWorker {
        type Worker = UnixWorker;

        fn spawn(
            &self,
            _worker: &Path,
            policy: &WorkerPolicy,
        ) -> Result<Spawned<UnixWorker>, LaunchError> {
            let mut ready = WorkerReady::header().encode().to_vec();
            ready.extend_from_slice(&WorkerReady.encode());
            let printf: String = ready.iter().map(|byte| format!("\\{byte:03o}")).collect();
            let script = format!("printf '{printf}'; exec /bin/sleep 30");
            spawn_program(Path::new("/bin/sh"), &["-c", &script], policy)
        }
    }

    /// GIVEN a worker that reports ready but has no seccomp filter WHEN the
    /// broker checks it THEN the run is refused before any input is sent
    /// and the worker is killed.
    #[test]
    fn given_ready_worker_without_seccomp_then_broker_refuses_before_sending_input() {
        let policy = WorkerPolicy {
            wall_deadline: Duration::from_secs(5),
            ..WorkerPolicy::default()
        };
        let budgets = ResourceBudgets::default();
        let run = launch::decode(
            &UnfilteredWorker,
            Path::new("/bin/sh"),
            b"not sent",
            &policy,
            &budgets,
        )
        .unwrap();
        assert!(
            matches!(run.outcome, Err(RunError::ContainmentNotConfirmed)),
            "{:?}",
            run.outcome.err()
        );
        assert_eq!(run.stats.exit_code, 128 + libc::SIGKILL as u32);
        assert!(run.stats.elapsed < Duration::from_secs(5));
        assert_eq!(budgets.aggregate_used(), 0);
    }

    #[test]
    fn given_relative_worker_path_then_launch_is_refused() {
        assert!(matches!(
            SeccompContainment::create_or_open("x").unwrap().spawn(
                Path::new("persephone-media-worker"),
                &WorkerPolicy::default()
            ),
            Err(LaunchError::Api {
                call: "worker path",
                ..
            })
        ));
    }
}

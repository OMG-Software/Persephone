//! What the Linux and macOS launchers share: the `Command` setup, the
//! resource limits set in the child before exec, and the started worker,
//! which the broker waits for, kills and reaps with `wait4`.
//!
//! The worker starts with an empty environment, `/` as its directory, a new
//! session (so it leads its own process group), stdin and stdout as pipes,
//! stderr on `/dev/null`, and every other descriptor closed at exec. It then
//! contains itself and reports `WorkerReady`; the OS launcher checks that
//! from outside before the broker sends any input.

use crate::{
    LaunchError, WorkerPolicy, WorkerStats,
    launch::{Readiness, Spawned, Wait, Worker},
};
use std::{
    cell::Cell,
    ffi::c_int,
    fs::File,
    io,
    os::fd::OwnedFd,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Most descriptors the worker may hold. The dynamic loader opens shared
/// libraries one at a time before `main`; afterwards the worker opens
/// nothing (seccomp or the sandbox refuses it anyway).
const WORKER_MAX_FILES: u64 = 8;
/// Longest pause between two checks for the worker's exit.
const MAX_POLL: Duration = Duration::from_millis(10);
/// Highest descriptor the fallback loop marks close-on-exec.
const MAX_FALLBACK_FD: u64 = 1 << 20;

fn error(call: &'static str, error: io::Error) -> LaunchError {
    LaunchError::Api {
        call,
        code: error.raw_os_error().unwrap_or(0) as u32,
    }
}

/// The base command for `program`: environment, directory and standard
/// streams. The OS launcher adds its `pre_exec` steps.
pub(crate) fn command(program: &Path) -> Result<Command, LaunchError> {
    // With `/` as the working directory a relative path would name another
    // file than the caller meant.
    if !program.is_absolute() {
        return Err(LaunchError::Api {
            call: "worker path",
            code: libc::EINVAL as u32,
        });
    }
    let mut command = Command::new(program);
    command
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    Ok(command)
}

/// The rlimits for one worker run.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// Address-space limit in bytes (`RLIMIT_AS`); `None` where the OS does
    /// not enforce it (macOS).
    pub address_space: Option<u64>,
    /// CPU time in whole seconds (`RLIMIT_CPU`), rounded up.
    pub cpu_seconds: u64,
}

impl Limits {
    pub(crate) fn new(policy: &WorkerPolicy, address_space: bool) -> Self {
        let cpu = policy.cpu_time_limit;
        let cpu_seconds = (cpu.as_secs() + u64::from(cpu.subsec_nanos() > 0)).max(1);
        Self {
            address_space: address_space.then_some(policy.memory_limit_bytes as u64),
            cpu_seconds,
        }
    }
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

/// Sets one rlimit, soft and hard. Runs in the child between fork and exec,
/// so it only makes system calls.
fn set_limit(resource: RlimitResource, value: u64) -> io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: `limit` is a valid rlimit.
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(last_error());
    }
    Ok(())
}

// glibc declares the resource argument as an enum type; macOS uses an int.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
type RlimitResource = libc::__rlimit_resource_t;
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
type RlimitResource = c_int;

/// The child's steps shared by Linux and macOS, after the OS's own: rlimits
/// for CPU time, file size, core dumps, processes, descriptors and (where
/// enforced) address space. Async-signal-safe: system calls only.
pub(crate) fn set_limits(limits: Limits) -> io::Result<()> {
    if let Some(bytes) = limits.address_space {
        set_limit(libc::RLIMIT_AS, bytes)?;
    }
    set_limit(libc::RLIMIT_CPU, limits.cpu_seconds)?;
    set_limit(libc::RLIMIT_FSIZE, 0)?;
    set_limit(libc::RLIMIT_CORE, 0)?;
    set_limit(libc::RLIMIT_NPROC, 0)?;
    set_limit(libc::RLIMIT_NOFILE, WORKER_MAX_FILES)
}

/// Starts a new session, so the worker leads its own process group and has
/// no controlling terminal. Async-signal-safe.
pub(crate) fn new_session() -> io::Result<()> {
    // SAFETY: setsid takes no arguments.
    if unsafe { libc::setsid() } < 0 {
        return Err(last_error());
    }
    Ok(())
}

/// Marks every descriptor above 2 close-on-exec, one by one, up to the soft
/// descriptor limit. Marking instead of closing keeps the descriptor that
/// `std` uses to report a failed exec working until the exec. Async-signal-
/// safe.
pub(crate) fn cloexec_each_above_stderr() -> io::Result<()> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid out-parameter.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(last_error());
    }
    let highest = limit.rlim_cur.min(MAX_FALLBACK_FD);
    for fd in 3..highest {
        // SAFETY: F_SETFD on a descriptor that may not exist; EBADF is fine.
        unsafe { libc::fcntl(fd as c_int, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok(())
}

/// Spawns `command` and takes the broker's ends of its pipes.
pub(crate) fn spawn(
    mut command: Command,
    confirm: fn(libc::pid_t) -> bool,
) -> Result<Spawned<UnixWorker>, LaunchError> {
    let mut child = command.spawn().map_err(|e| error("spawn", e))?;
    let pid = child.id() as libc::pid_t;
    let worker = UnixWorker {
        pid,
        reaped: Cell::new(None),
        confirm,
    };
    // Dropping `worker` on an error below kills and reaps the process.
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(LaunchError::Api {
            call: "spawn",
            code: libc::EBADF as u32,
        });
    };
    // The process is waited for and reaped through `worker`, not `child`;
    // dropping a `Child` neither waits nor kills.
    drop(child);
    Ok(Spawned {
        worker,
        stdin: File::from(OwnedFd::from(stdin)),
        stdout: File::from(OwnedFd::from(stdout)),
    })
}

/// A started worker, until the broker reaps it. Only the broker's run thread
/// waits, checks and kills it, so the process id cannot be reused while
/// those happen: the process stays a zombie until it is reaped here.
pub(crate) struct UnixWorker {
    pid: libc::pid_t,
    /// The wait status and resource usage, once reaped.
    reaped: Cell<Option<(c_int, libc::rusage)>>,
    /// The OS check that the worker is contained.
    confirm: fn(libc::pid_t) -> bool,
}

impl UnixWorker {
    /// Reaps the worker if it exited (or, with `block`, once it exits).
    /// Returns whether it is reaped.
    fn reap(&self, block: bool) -> io::Result<bool> {
        if self.reaped.get().is_some() {
            return Ok(true);
        }
        let flags = if block { 0 } else { libc::WNOHANG };
        let mut status: c_int = 0;
        // SAFETY: rusage is plain data; wait4 fills it.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: valid out-pointers; `pid` is this broker's child.
            let rc = unsafe { libc::wait4(self.pid, &mut status, flags, &mut usage) };
            if rc == self.pid {
                self.reaped.set(Some((status, usage)));
                return Ok(true);
            }
            if rc == 0 {
                return Ok(false);
            }
            let error = last_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Worker for UnixWorker {
    const READINESS: Readiness = Readiness::Announced;

    fn wait(&self, timeout: Duration) -> Wait {
        let start = Instant::now();
        let mut pause = Duration::from_millis(1);
        loop {
            match self.reap(false) {
                Ok(true) => return Wait::Exited,
                Ok(false) => {}
                Err(_) => return Wait::Failed,
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return Wait::TimedOut;
            }
            thread::sleep(pause.min(timeout - elapsed));
            pause = (pause * 2).min(MAX_POLL);
        }
    }

    fn kill(&self) {
        if self.reaped.get().is_none() {
            // The worker leads its own process group (setsid); the group
            // holds anything it could have started. An unreaped zombie keeps
            // the group id from being reused.
            // SAFETY: signals this broker's own, unreaped child's group.
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
                libc::kill(self.pid, libc::SIGKILL);
            }
        }
    }

    fn confirm_contained(&self) -> bool {
        self.reaped.get().is_none() && (self.confirm)(self.pid)
    }

    fn signalled(&self) -> bool {
        matches!(self.reaped.get(), Some((status, _)) if libc::WIFSIGNALED(status))
    }

    fn stats(&self, elapsed: Duration) -> WorkerStats {
        let Some((status, usage)) = self.reaped.get() else {
            return WorkerStats {
                elapsed,
                peak_process_memory: 0,
                user_cpu: Duration::ZERO,
                exit_code: u32::MAX,
            };
        };
        // ru_maxrss is in KiB on Linux and in bytes on macOS.
        let rss_unit = if cfg!(target_os = "linux") { 1024 } else { 1 };
        WorkerStats {
            elapsed,
            peak_process_memory: (usage.ru_maxrss.max(0) as usize).saturating_mul(rss_unit),
            user_cpu: Duration::from_secs(usage.ru_utime.tv_sec.max(0) as u64)
                + Duration::from_micros(usage.ru_utime.tv_usec.max(0) as u64),
            exit_code: exit_code(status),
        }
    }
}

/// The exit code, or 128 plus the signal number for a death by signal, as a
/// shell reports it; `u32::MAX` for anything else.
fn exit_code(status: c_int) -> u32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status) as u32
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status) as u32
    } else {
        u32::MAX
    }
}

impl Drop for UnixWorker {
    fn drop(&mut self) {
        if self.reaped.get().is_none() {
            self.kill();
            let _ = self.reap(true);
        }
    }
}

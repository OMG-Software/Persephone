//! The macOS launcher (ADR-0011). Between fork and exec the worker gets a
//! new session, every descriptor above 2 closed at exec, and rlimits: CPU
//! time at the policy's CPU limit rounded up to whole seconds, no file
//! writes, no core dumps, no new processes and few descriptors. The worker
//! then applies the pure-computation sandbox profile to itself and reports
//! `WorkerReady`; before any input is sent the broker asks the kernel
//! whether the worker is sandboxed (`sandbox_check`).
//!
//! macOS does not enforce `RLIMIT_AS`, so no address-space limit is set: the
//! memory bound is the decoder guest's linear-memory cap and the bounded
//! host code around it (ADR-0003). There is no parent-death signal or
//! `no_new_privs` on macOS; a worker whose broker ended reads end-of-file on
//! stdin and exits, and the CPU limit bounds a running decode.
//!
//! This module is not compiled on the development machine; CI builds and
//! tests it.

use crate::{
    LaunchError, WorkerPolicy,
    launch::{Launcher, Spawned},
    unix::{self, Limits, LongLivedWorker, StartedCallWorker, UnixWorker},
};
use std::{
    ffi::{c_char, c_int},
    os::unix::process::CommandExt,
    path::Path,
    ptr::null,
};

/// The macOS containment: nothing to create, since the worker applies its
/// own sandbox profile. It exists so that every OS has the same API.
pub struct SeatbeltContainment {
    _private: (),
}

impl SeatbeltContainment {
    /// macOS has no named containment object; `name` is ignored.
    pub fn create_or_open(_name: &str) -> Result<Self, LaunchError> {
        Ok(Self { _private: () })
    }

    /// The same as [`Self::create_or_open`]; there is nothing to delete.
    pub fn temporary(name: &str) -> Result<Self, LaunchError> {
        Self::create_or_open(name)
    }
}

impl Launcher for SeatbeltContainment {
    type Worker = UnixWorker;

    fn spawn(
        &self,
        worker: &Path,
        policy: &WorkerPolicy,
    ) -> Result<Spawned<UnixWorker>, LaunchError> {
        let mut command = unix::command(worker)?;
        let limits = Limits::new(policy, false);
        // SAFETY: the closure runs between fork and exec and makes only
        // system calls; it allocates nothing.
        unsafe {
            command.pre_exec(move || {
                unix::new_session()?;
                unix::cloexec_each_above_stderr()?;
                unix::set_limits(limits)
            });
        }
        unix::spawn(command, confirm)
    }
}

/// Starts the call worker with the macOS launch rules of ADR-0049: the media
/// worker's launch (new session, descriptors above 2 closed at exec, empty
/// environment) with the call limits (no file writes, no core dumps, 256
/// descriptors; macOS enforces no address-space limit). The worker applies
/// its call Seatbelt profile and reports `Ready`; the caller then calls
/// `confirm_contained` on the returned worker before it sends any input.
pub fn start_call_worker(
    _profile: &SeatbeltContainment,
    exe: &Path,
) -> Result<StartedCallWorker, LaunchError> {
    let mut command = unix::command(exe)?;
    // SAFETY: the closure runs between fork and exec and makes only system
    // calls; it allocates nothing.
    unsafe {
        command.pre_exec(|| {
            unix::new_session()?;
            unix::cloexec_each_above_stderr()?;
            // No file writes. The Linux bound (a memfd sound pool) was not
            // measured here; macOS keeps 0 until a Mac run needs more.
            unix::set_call_limits(None, 0)
        });
    }
    let spawned = unix::spawn(command, confirm)?;
    Ok(StartedCallWorker {
        worker: LongLivedWorker::new(spawned.worker),
        stdin: spawned.stdin,
        stdout: spawned.stdout,
        camera: None,
    })
}

#[link(name = "sandbox", kind = "dylib")]
unsafe extern "C" {
    /// Returns 1 when `pid` runs under a sandbox and `operation` is null
    /// (filter type 0, `SANDBOX_FILTER_NONE`).
    fn sandbox_check(pid: libc::pid_t, operation: *const c_char, filter: c_int, ...) -> c_int;
}

/// Whether the kernel reports the worker as sandboxed. Anything other than
/// 1, an error included, is "not contained".
fn confirm(pid: libc::pid_t) -> bool {
    // SAFETY: a null operation with no filter arguments asks only whether
    // the process is sandboxed.
    unsafe { sandbox_check(pid, null(), 0) == 1 }
}

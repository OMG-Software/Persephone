//! The macOS UI host: the UI in its own process group, ended with the core
//! (ADR-0011).
//!
//! macOS has no equivalent of the Windows Job, so the UI's memory is not
//! capped; the owner accepted that residual risk. Kill-on-exit has two
//! parts: the core kills the UI's process group when a run ends (dropping
//! [`ProcessGroup`]), and the UI exits by itself when the core's pipe
//! closes, which also covers a core that ends abnormally.
//!
//! The window watch (`ui_watch.rs`) counts the web view's processes. On
//! Windows they are the UI's children, in its Job. WKWebView's content and
//! network processes are XPC services that launchd starts; they are neither
//! children of the UI nor in its process group, and the core has no public
//! way to see them. So [`ProcessGroup::active_processes`] reports the count
//! as unreadable, and the watch judges the window by readiness only: a page
//! that never sends a request counts as failed. "The web view exited while
//! the UI kept running" cannot be observed on macOS; any count the core
//! could read (the UI alone, or the UI plus its own helpers) would either
//! never change or report a helper's exit as a failed window.

use super::credentials;
use crate::ui_host::{StartError, StartedUi, UiHost, UiTree, ui_command, ui_exe};
use std::{
    io,
    os::unix::process::CommandExt,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

/// `PROC_PGRP_ONLY` from `<libproc.h>`: list the processes of one process
/// group.
const PROC_PGRP_ONLY: u32 = 2;
/// Group members read at a time; a larger group still reads as not empty.
const MAX_GROUP_PIDS: usize = 64;
const POLL: Duration = Duration::from_millis(50);

/// Starts the UI in its own process group.
pub struct GroupHost;

impl UiHost for GroupHost {
    fn start(&self) -> Result<StartedUi, StartError> {
        let ui = ui_exe().map_err(|_| StartError::NoExe)?;
        let child =
            spawn_in_group(ui_command(&ui)).map_err(|error| StartError::Spawn { ui, error })?;
        let tree = Box::new(ProcessGroup::of(&child));
        Ok(StartedUi { child, tree })
    }

    fn ask_to_retry(&self) -> bool {
        credentials::ask_to_retry()
    }
}

/// Spawns `command` as the leader of a new process group. The group is set
/// in the spawn itself (`posix_spawn`), so no code of the child runs outside
/// it.
pub fn spawn_in_group(mut command: Command) -> io::Result<Child> {
    command.process_group(0).spawn()
}

/// The process group of one UI run, led by the UI. Dropping it kills
/// anything left in the group.
///
/// The core reaps the UI before it drops this (`main.rs`), so the group ID
/// may be free by then. A process ID is reused only after macOS has handed
/// out every other one, so the kill cannot plausibly reach a new group of
/// the same number in that moment.
pub struct ProcessGroup {
    leader: libc::pid_t,
}

impl ProcessGroup {
    /// The group that `child`, started by [`spawn_in_group`], leads.
    pub fn of(child: &Child) -> Self {
        Self {
            leader: child.id() as libc::pid_t,
        }
    }

    fn kill(&self) {
        // SAFETY: plain system call; a group that no longer exists gives
        // ESRCH, which needs no handling.
        unsafe { libc::killpg(self.leader, libc::SIGKILL) };
    }

    /// Processes still running in the group: the UI until it has exited
    /// (exited but not yet reaped counts as ended), and any other member.
    fn running(&self) -> io::Result<usize> {
        let leader = usize::from(!leader_exited(self.leader)?);
        let others = group_members(self.leader)?
            .into_iter()
            .filter(|pid| *pid != self.leader)
            .count();
        Ok(leader + others)
    }
}

impl UiTree for ProcessGroup {
    /// Unreadable on macOS: the web view's processes are not in the group
    /// (see the module documentation), so the watch judges readiness only.
    fn active_processes(&self) -> io::Result<u32> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the web view's processes are outside the UI's process group on macOS",
        ))
    }

    /// Kills the group and waits up to `timeout` for every member to exit.
    /// WebKit's XPC services end on their own when the UI is gone; they are
    /// not waited for.
    fn terminate(&self, timeout: Duration) -> bool {
        self.kill();
        let deadline = Instant::now() + timeout;
        loop {
            match self.running() {
                Ok(0) => return true,
                Ok(_) if Instant::now() < deadline => thread::sleep(POLL),
                _ => return false,
            }
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Whether the child `pid` has exited, without reaping it (`WNOWAIT`), so
/// the core's own wait still gets its status. A child already reaped has
/// exited.
fn leader_exited(pid: libc::pid_t) -> io::Result<bool> {
    // SAFETY: `info` is zeroed, which `waitid` leaves as is when the child
    // has not changed state (`WNOHANG`), so `si_pid` then reads 0.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: plain system call on a valid, writable `siginfo_t`.
    let status = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if status == -1 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ECHILD) => Ok(true),
            _ => Err(error),
        };
    }
    // SAFETY: `info` was filled by `waitid` or left zeroed.
    Ok(unsafe { info.si_pid() } == pid)
}

/// Process IDs in the group `pgid`, zombies included, at most
/// [`MAX_GROUP_PIDS`].
fn group_members(pgid: libc::pid_t) -> io::Result<Vec<libc::pid_t>> {
    let mut pids = [0 as libc::pid_t; MAX_GROUP_PIDS];
    // SAFETY: the buffer and its size in bytes describe `pids`.
    let filled = unsafe {
        libc::proc_listpids(
            PROC_PGRP_ONLY,
            pgid as u32,
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids) as libc::c_int,
        )
    };
    if filled < 0 {
        return Err(io::Error::last_os_error());
    }
    // Entries past those filled stay 0, whatever unit the count is in.
    Ok(pids.into_iter().filter(|pid| *pid > 0).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn sleeper(script: &str) -> Child {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        spawn_in_group(command).expect("start /bin/sh")
    }

    /// GIVEN a UI started in its own group THEN it leads that group, and its
    /// process count is unreadable, never a count the watch could take as
    /// "the web view exited".
    #[test]
    fn given_a_started_ui_then_it_leads_its_group_and_the_count_is_unreadable() {
        let mut child = sleeper("exec sleep 30");
        let group = ProcessGroup::of(&child);
        let pid = child.id() as libc::pid_t;
        // SAFETY: plain system calls.
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        assert_ne!(unsafe { libc::getpgid(0) }, pid, "not the test's group");
        let error = group.active_processes().expect_err("unreadable");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(group.running().unwrap(), 1);
        assert!(group.terminate(Duration::from_secs(10)));
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    /// GIVEN a UI with a helper in its group WHEN the tree is terminated
    /// THEN both end, the UI is left for the core to reap, and the wait
    /// reports the group empty.
    #[test]
    fn given_a_helper_in_the_group_then_terminate_ends_both() {
        let mut child = sleeper("sleep 30 & sleep 30");
        let group = ProcessGroup::of(&child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while group.running().unwrap() < 2 {
            assert!(Instant::now() < deadline, "the helper did not start");
            thread::sleep(POLL);
        }
        assert!(group.terminate(Duration::from_secs(10)));
        assert!(leader_exited(child.id() as libc::pid_t).unwrap());
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert!(leader_exited(child.id() as libc::pid_t).unwrap(), "reaped");
    }

    /// GIVEN a run that ended WHEN its group is dropped THEN anything left
    /// in the group is killed.
    #[test]
    fn given_a_dropped_group_then_its_members_are_killed() {
        let mut child = sleeper("exec sleep 30");
        drop(ProcessGroup::of(&child));
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }
}

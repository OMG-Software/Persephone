//! The Linux UI host (ADR-0010): the UI in its own process group, with a
//! parent-death signal and `no_new_privs`, placed in a systemd user scope
//! whose memory is capped like the Windows Job.
//!
//! * **Start.** The UI is spawned as the leader of a new process group. In
//!   the child, before `exec`, it asks the kernel for `SIGKILL` when the
//!   core goes away (`PR_SET_PDEATHSIG`) and gives up gaining privileges
//!   (`PR_SET_NO_NEW_PRIVS`).
//! * **Placed before it runs.** The new process holds itself before `exec`
//!   ([`spawn_gated`]): still running only the core's own code, it sends its
//!   process ID over a pipe and waits. A core thread places it in its scope
//!   and runs the checks below; only then does the process `exec` the UI.
//!   On any failure it exits without running the UI. So nothing of the UI
//!   runs, or starts another process, outside the scope.
//! * **Memory cap.** The core asks the systemd user manager, over the
//!   session bus, for a transient scope holding the UI (`StartTransientUnit`)
//!   with `MemoryMax` equal to the Windows Job's 2 GiB and no swap, and
//!   waits until the kernel shows the UI in that scope. The cap and the swap
//!   limit are read back from the scope's cgroup, and every live process of
//!   the window must be in it: each descendant of the UI, and each member of
//!   its process group, which also finds a grandchild whose parent exited
//!   and that was given to another parent. A process the UI started between
//!   `exec` would stay outside, so its presence ends the UI. With the hold
//!   before `exec` there is none; the check stays as a second line of
//!   defence. The web view's
//!   processes, started by the UI after it continues, are in the same scope.
//!   There is no per-process cap like the Job's 1 GiB: an address-space
//!   limit would stop the web view's large reservations.
//! * **No manager.** Without a session bus (no address, or no socket) or a
//!   systemd user manager on it, or when the scope's cgroup does not show
//!   the cap and the swap limit (no memory controller for the user manager),
//!   the UI runs anyway and the core shows a warning once that its memory is
//!   not capped. A session bus that refuses or fails the connection, any
//!   other refusal by the manager, a UI that does not appear in the scope in
//!   time, or a descendant outside it ends the UI: the core does not run a
//!   UI it could not contain as asked.
//! * **Watch and end.** With a scope, the window watch counts the processes
//!   in the scope's cgroup (the UI and its web view, as in the Job), and the
//!   tree is ended through `cgroup.kill` and the process group. Without
//!   one, the count is unreadable, so the watch judges readiness only, and
//!   the tree is ended through the process group (web view processes that
//!   leave the group end when the UI does, as on macOS).

use super::bus::{self, BusError};
use super::credentials;
use crate::ui_host::{StartError, StartedUi, UiHost, UiTree, ui_command, ui_exe};
use std::{
    fs, io,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    thread,
    time::{Duration, Instant},
};
use zbus::zvariant::Value;

/// Memory of the UI and all of its web view processes together: the
/// Windows Job's `UI_JOB_MEMORY_LIMIT` (`platform/windows/ui.rs`).
pub const UI_MEMORY_MAX: u64 = 2 * 1024 * 1024 * 1024;
/// Swap the scope may use: none, so the cap also bounds what the UI can
/// push out of memory, like the Job's limit on committed memory.
pub const UI_SWAP_MAX: u64 = 0;
/// Longest wait for the UI to appear in its scope.
const PLACE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);
/// Longest cgroup or `/proc` file read.
const MAX_FILE_BYTES: u64 = 64 * 1024;
/// Where the unified (version 2) cgroup hierarchy is mounted.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

const SYSTEMD: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER: &str = "org.freedesktop.systemd1.Manager";

/// The warning when the UI's memory is not capped (ADR-0010).
pub const UNCAPPED: (&str, &str) = (
    "Persephone's window memory is not limited",
    "This session has no systemd user manager that can limit memory, so \
     Persephone cannot cap the memory its window uses. A fault in the window \
     could use up this computer's memory.\n\n\
     Persephone works as usual otherwise.",
);

/// UI runs started by this core, for unique scope names.
static RUNS: AtomicU32 = AtomicU32::new(0);

/// Starts the UI with the controls in the module documentation.
#[derive(Default)]
pub struct ScopeHost {
    /// Whether the "not capped" warning was shown in this core.
    warned: AtomicBool,
}

impl UiHost for ScopeHost {
    fn start(&self) -> Result<StartedUi, StartError> {
        self.start_at(ui_exe().map_err(|_| StartError::NoExe)?)
    }

    fn ask_to_retry(&self) -> bool {
        credentials::ask_to_retry()
    }
}

impl ScopeHost {
    /// [`UiHost::start`] with the UI executable at `ui`.
    fn start_at(&self, ui: PathBuf) -> Result<StartedUi, StartError> {
        let unit = unit_name(std::process::id(), RUNS.fetch_add(1, Ordering::Relaxed));
        let (child, placement) = spawn_gated(ui_command(&ui), move |pid| place(pid, &unit))
            .map_err(|error| match error {
                GateError::Spawn(error) => StartError::Spawn { ui, error },
                GateError::Admit(error) => StartError::Contain(error),
            })?;
        if !placement.capped && !self.warned.swap(true, Ordering::AcqRel) {
            eprintln!("persephone: the window's memory is not capped");
            let (caption, message) = UNCAPPED;
            credentials::post(credentials::notice_alert(caption, message));
        }
        Ok(StartedUi {
            tree: Box::new(UiRun {
                group: ProcessGroup::of(&child),
                cgroup: placement.cgroup,
            }),
            child,
        })
    }
}

/// Spawns `command` as the leader of a new process group, with the
/// parent-death signal and `no_new_privs` set before `exec`.
///
/// The parent-death signal fires when the *thread* that spawned the child
/// ends, not the process. The core starts every UI from its main thread
/// (`run_ui` in `main.rs`), which lives as long as the core; spawning from a
/// shorter-lived thread would kill the UI when that thread ended.
pub fn spawn_contained(mut command: Command) -> io::Result<Child> {
    let core = std::process::id() as libc::pid_t;
    command.process_group(0);
    // SAFETY: `child_setup` makes only async-signal-safe system calls and
    // allocates nothing.
    unsafe { command.pre_exec(move || child_setup(core)) };
    command.spawn()
}

/// Why a held start did not run its command.
#[derive(Debug)]
pub enum GateError {
    /// The process could not be started or set up.
    Spawn(io::Error),
    /// `admit` refused it: the command never ran.
    Admit(io::Error),
}

/// Spawns `command` as [`spawn_contained`] does, but holds it before
/// `exec`. The new process, still running only the core's own code, sends
/// its process ID over a pipe and waits on a second one. `admit` runs on a
/// thread of its own with that ID; only when it returns `Ok` is the process
/// let through to `exec`. Otherwise its pipe closes, the process exits
/// without running `command`, and `admit`'s error comes back.
///
/// Both pipes are close-on-exec, so neither reaches the command. The
/// process closes the core's ends of them first, so it sees the end of the
/// pipe when `admit` fails.
pub fn spawn_gated<T: Send + 'static>(
    mut command: Command,
    admit: impl FnOnce(u32) -> io::Result<T> + Send + 'static,
) -> Result<(Child, T), GateError> {
    let core = std::process::id() as libc::pid_t;
    let (id_read, id_write) = cloexec_pipe().map_err(GateError::Spawn)?;
    let (go_read, go_write) = cloexec_pipe().map_err(GateError::Spawn)?;
    let core_ends = [id_read.as_raw_fd(), go_write.as_raw_fd()];
    let (child_id, child_go) = (id_write.as_raw_fd(), go_read.as_raw_fd());
    let gate = thread::Builder::new()
        .name("core-ui-gate".into())
        .spawn(move || -> io::Result<T> {
            let mut pid = [0u8; 4];
            fs::File::from(id_read).read_exact(&mut pid)?;
            let admitted = admit(u32::from_ne_bytes(pid))?;
            fs::File::from(go_write).write_all(&[GO])?;
            Ok(admitted)
        })
        .map_err(GateError::Spawn)?;
    command.process_group(0);
    // SAFETY: `child_setup` and `wait_at_gate` make only async-signal-safe
    // system calls and allocate nothing.
    unsafe {
        command.pre_exec(move || {
            child_setup(core)?;
            wait_at_gate(core_ends, child_id, child_go)
        })
    };
    let spawned = command.spawn();
    // The core's copies of the process's ends: without them, the gate
    // thread sees the end of the pipe if the process never wrote.
    drop(id_write);
    drop(go_read);
    let admitted = gate
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("the window's gate thread failed")));
    match (spawned, admitted) {
        (Ok(child), Ok(admitted)) => Ok((child, admitted)),
        (Ok(mut child), Err(error)) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(GateError::Admit(error))
        }
        (Err(error), Ok(_)) => Err(GateError::Spawn(error)),
        // The process exited at the gate because `admit` refused it, or
        // could not be set up; `admit`'s error says which when it ran.
        (Err(spawn), Err(admit)) => {
            if admit.kind() == io::ErrorKind::UnexpectedEof {
                Err(GateError::Spawn(spawn))
            } else {
                Err(GateError::Admit(admit))
            }
        }
    }
}

/// The byte that lets a held process through.
const GO: u8 = 1;

fn cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 writes.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 succeeded, so both are open descriptors owned by no one
    // else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Runs in the child between fork and exec. Async-signal-safe: sends this
/// process's ID, then waits for [`GO`]. The end of the pipe, or anything
/// else, refuses the `exec`.
fn wait_at_gate(core_ends: [RawFd; 2], id: RawFd, go: RawFd) -> io::Result<()> {
    for fd in core_ends {
        // SAFETY: closes this process's copy of a descriptor the core owns.
        unsafe { libc::close(fd) };
    }
    // SAFETY: getpid takes no arguments.
    let pid = unsafe { libc::getpid() }.to_ne_bytes();
    let mut sent = 0;
    while sent < pid.len() {
        // SAFETY: writes from a live buffer within its length.
        let n = unsafe { libc::write(id, pid[sent..].as_ptr().cast(), pid.len() - sent) };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        sent += n as usize;
    }
    // SAFETY: closes this process's own end; nothing else uses it.
    unsafe { libc::close(id) };
    let mut byte = 0u8;
    loop {
        // SAFETY: reads one byte into a live local.
        let n = unsafe { libc::read(go, (&raw mut byte).cast(), 1) };
        if n == 1 && byte == GO {
            return Ok(());
        }
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
}

fn prctl(option: libc::c_int, value: libc::c_ulong) -> io::Result<()> {
    const NONE: libc::c_ulong = 0;
    // SAFETY: prctl with integer arguments only.
    if unsafe { libc::prctl(option, value, NONE, NONE, NONE) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Runs in the child between fork and exec. Async-signal-safe.
fn child_setup(core: libc::pid_t) -> io::Result<()> {
    prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong)?;
    // The core may have ended before the signal was armed.
    // SAFETY: getppid takes no arguments.
    if unsafe { libc::getppid() } != core {
        return Err(io::Error::from_raw_os_error(libc::ESRCH));
    }
    prctl(libc::PR_SET_NO_NEW_PRIVS, 1)
}

/// The scope's unit name: unique for this core process and run.
pub fn unit_name(core: u32, run: u32) -> String {
    format!("persephone-ui-{core}-{run}.scope")
}

/// The scope's properties: the UI's process, the memory cap, no swap, and
/// removal once it has ended, even after a failure.
pub fn scope_properties(pid: u32) -> Vec<(&'static str, Value<'static>)> {
    vec![
        ("Description", Value::from("Persephone window")),
        ("PIDs", Value::from(vec![pid])),
        ("MemoryMax", Value::U64(UI_MEMORY_MAX)),
        ("MemorySwapMax", Value::U64(UI_SWAP_MAX)),
        ("CollectMode", Value::from("inactive-or-failed")),
    ]
}

/// The `StartTransientUnit` arguments: the name, mode "fail" (never replace
/// a unit of that name), the properties, and no auxiliary units.
#[allow(clippy::type_complexity)]
pub fn start_body(
    unit: &str,
    pid: u32,
) -> (
    &str,
    &'static str,
    Vec<(&'static str, Value<'static>)>,
    Vec<(&'static str, Vec<(&'static str, Value<'static>)>)>,
) {
    (unit, "fail", scope_properties(pid), Vec::new())
}

/// Where the UI ended up.
pub struct Placement {
    /// The scope's cgroup, when the UI is in one that can be read.
    pub cgroup: Option<Cgroup>,
    /// Whether the cgroup shows the memory cap.
    pub capped: bool,
}

/// Places the UI (`pid`) in a new scope called `unit`. No session bus or no
/// systemd user manager is an uncapped placement; any other failure is an
/// error, and the caller ends the UI.
fn place(pid: u32, unit: &str) -> io::Result<Placement> {
    let uncapped = Placement {
        cgroup: None,
        capped: false,
    };
    let connection = match bus::session() {
        Ok(connection) => connection,
        Err(BusError::Absent) => return Ok(uncapped),
        Err(BusError::Failed) => {
            return Err(io::Error::other(
                "the session bus refused or failed the connection",
            ));
        }
    };
    match connection.call_method(
        Some(SYSTEMD),
        SYSTEMD_PATH,
        Some(MANAGER),
        "StartTransientUnit",
        &start_body(unit, pid),
    ) {
        Ok(_) => {}
        Err(error) if bus::classify(&error) == BusError::Absent => return Ok(uncapped),
        Err(_) => {
            return Err(io::Error::other(
                "the systemd user manager refused the window's scope",
            ));
        }
    }
    let path = wait_in_scope(pid, unit)?;
    let cgroup = Cgroup::at(Path::new(CGROUP_ROOT), &path);
    if let Some(cgroup) = &cgroup {
        let inside = cgroup.processes()?;
        let window = window_processes(pid as libc::pid_t, &live_processes()?);
        let strays = outside(&window, &inside);
        if !strays.is_empty() {
            // The caller ends the UI's group and the UI; a process outside
            // both (a new session) would outlive them, so end it here.
            for stray in strays {
                // SAFETY: plain system call; ESRCH needs no handling.
                unsafe { libc::kill(stray, libc::SIGKILL) };
            }
            return Err(io::Error::other(
                "a process of the window is outside its scope",
            ));
        }
    }
    let capped = cgroup.as_ref().is_some_and(|cgroup| {
        cgroup.memory_max().ok() == Some(UI_MEMORY_MAX)
            && cgroup.swap_max().ok() == Some(UI_SWAP_MAX)
    });
    Ok(Placement { cgroup, capped })
}

/// One live process: its ID, parent and process group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessEntry {
    pub pid: libc::pid_t,
    pub parent: libc::pid_t,
    pub group: libc::pid_t,
}

/// Every live process, from `/proc`. Exited (zombie) processes are left
/// out: they run nothing and a cgroup does not list them.
fn live_processes() -> io::Result<Vec<ProcessEntry>> {
    let mut processes = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        // A process may exit while the list is read.
        let Ok(stat) = read_bounded(&PathBuf::from(format!("/proc/{pid}/stat"))) else {
            continue;
        };
        if let Some((state, parent, group)) = stat_fields(&stat)
            && state != 'Z'
            && state != 'X'
        {
            processes.push(ProcessEntry { pid, parent, group });
        }
    }
    Ok(processes)
}

/// The window's processes: `root` (the UI, which leads its process group),
/// every process descended from it, and every member of its group, which
/// includes a descendant whose parent exited and that was reparented.
pub fn window_processes(root: libc::pid_t, processes: &[ProcessEntry]) -> Vec<libc::pid_t> {
    let mut found = vec![root];
    for entry in processes {
        if entry.group == root && !found.contains(&entry.pid) {
            found.push(entry.pid);
        }
    }
    let mut next = 0;
    while let Some(&parent) = found.get(next) {
        next += 1;
        for entry in processes {
            if entry.parent == parent && !found.contains(&entry.pid) {
                found.push(entry.pid);
            }
        }
    }
    found
}

/// The processes in `tree` that are not in `inside` (a cgroup's list).
pub fn outside(tree: &[libc::pid_t], inside: &[libc::pid_t]) -> Vec<libc::pid_t> {
    tree.iter()
        .copied()
        .filter(|pid| !inside.contains(pid))
        .collect()
}

/// Waits until `/proc/<pid>/cgroup` shows the process in `unit`, and
/// returns its cgroup path.
fn wait_in_scope(pid: u32, unit: &str) -> io::Result<String> {
    let file = PathBuf::from(format!("/proc/{pid}/cgroup"));
    let deadline = Instant::now() + PLACE_TIMEOUT;
    loop {
        let text = read_bounded(&file)?;
        if let Some(path) = unified_path(&text).filter(|path| in_unit(path, unit)) {
            return Ok(path.to_owned());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the window did not appear in its scope",
            ));
        }
        thread::sleep(POLL);
    }
}

/// The unified-hierarchy path in a `/proc/<pid>/cgroup` text: the
/// `0::<path>` line.
pub fn unified_path(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .filter(|path| path.starts_with('/'))
}

/// Whether the cgroup `path` is the cgroup of `unit` itself.
pub fn in_unit(path: &str, unit: &str) -> bool {
    path.rsplit('/').next() == Some(unit)
}

/// Reads at most [`MAX_FILE_BYTES`] of a small kernel text file.
fn read_bounded(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    fs::File::open(path)?
        .take(MAX_FILE_BYTES)
        .read_to_string(&mut text)?;
    Ok(text)
}

/// The scope's cgroup directory.
pub struct Cgroup {
    dir: PathBuf,
}

impl Cgroup {
    /// The cgroup at `path` under `root`, when its process list can be
    /// read.
    pub fn at(root: &Path, path: &str) -> Option<Self> {
        let relative = path.trim_start_matches('/');
        if relative.split('/').any(|part| part == "..") {
            return None;
        }
        let dir = root.join(relative);
        read_bounded(&dir.join("cgroup.procs")).ok()?;
        Some(Self { dir })
    }

    /// The memory cap, in bytes; `None` for "max".
    pub fn memory_max(&self) -> io::Result<u64> {
        let text = read_bounded(&self.dir.join("memory.max"))?;
        text.trim()
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "memory.max"))
    }

    /// The swap limit, in bytes; an error for "max" (not limited) or an
    /// unreadable file (no swap accounting).
    pub fn swap_max(&self) -> io::Result<u64> {
        let text = read_bounded(&self.dir.join("memory.swap.max"))?;
        text.trim()
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "memory.swap.max"))
    }

    /// Processes in the cgroup. Zombies are not listed (cgroup v2), and a
    /// cgroup that systemd has removed, because it emptied, has none.
    pub fn processes(&self) -> io::Result<Vec<libc::pid_t>> {
        match read_bounded(&self.dir.join("cgroup.procs")) {
            Ok(text) => Ok(text
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// Kills every process in the cgroup: `cgroup.kill` (Linux 5.14), else
    /// each listed process. Without `cgroup.kill`, a process that exits
    /// between the read and the kill could in theory have its ID reused.
    pub fn kill(&self) {
        if fs::write(self.dir.join("cgroup.kill"), b"1").is_ok() {
            return;
        }
        for pid in self.processes().unwrap_or_default() {
            // SAFETY: plain system call; ESRCH needs no handling.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// The process group of one UI run, led by the UI.
///
/// The core reaps the UI before it drops the tree (`main.rs`), so the group
/// ID may be free by then. A new group with that ID would need a new
/// process with that ID to lead one in that moment; the kill cannot
/// plausibly reach it.
pub struct ProcessGroup {
    leader: libc::pid_t,
}

impl ProcessGroup {
    /// The group that `child`, started by [`spawn_contained`], leads.
    pub fn of(child: &Child) -> Self {
        Self {
            leader: child.id() as libc::pid_t,
        }
    }

    pub fn kill(&self) {
        // SAFETY: plain system call; a group that no longer exists gives
        // ESRCH, which needs no handling.
        unsafe { libc::killpg(self.leader, libc::SIGKILL) };
    }

    /// Processes still running in the group; an exited but unreaped UI
    /// counts as ended.
    pub fn running(&self) -> io::Result<usize> {
        let mut running = 0;
        for entry in fs::read_dir("/proc")? {
            let Ok(entry) = entry else { continue };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            // A process may exit while the list is read.
            let Ok(stat) = read_bounded(&PathBuf::from(format!("/proc/{pid}/stat"))) else {
                continue;
            };
            if let Some((state, group)) = state_and_group(&stat)
                && group == self.leader
                && state != 'Z'
                && state != 'X'
            {
                running += 1;
            }
        }
        Ok(running)
    }
}

/// The state, parent and process group in a `/proc/<pid>/stat` line, read
/// as for [`state_and_group`].
pub fn stat_fields(stat: &str) -> Option<(char, libc::pid_t, libc::pid_t)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let parent = fields.next()?.parse().ok()?;
    let group = fields.next()?.parse().ok()?;
    Some((state, parent, group))
}

/// The state and process group in a `/proc/<pid>/stat` line. The command
/// name may hold spaces and parentheses, so fields are read after the last
/// `)`: state, parent, group.
pub fn state_and_group(stat: &str) -> Option<(char, libc::pid_t)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let _parent = fields.next()?;
    let group = fields.next()?.parse().ok()?;
    Some((state, group))
}

/// One UI run: its process group, and its scope's cgroup when it has one.
/// Dropping it kills anything left in either.
pub struct UiRun {
    group: ProcessGroup,
    cgroup: Option<Cgroup>,
}

impl UiRun {
    fn kill(&self) {
        if let Some(cgroup) = &self.cgroup {
            cgroup.kill();
        }
        self.group.kill();
    }

    fn running(&self) -> io::Result<usize> {
        match &self.cgroup {
            Some(cgroup) => cgroup.processes().map(|pids| pids.len()),
            None => self.group.running(),
        }
    }
}

impl UiTree for UiRun {
    /// The processes in the scope: the UI and its web view. Unreadable
    /// without a scope, so the watch then judges readiness only.
    fn active_processes(&self) -> io::Result<u32> {
        match &self.cgroup {
            Some(cgroup) => cgroup
                .processes()
                .map(|pids| u32::try_from(pids.len()).unwrap_or(u32::MAX)),
            None => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the window has no scope to count its processes in",
            )),
        }
    }

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

impl Drop for UiRun {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use zbus::zvariant::DynamicType;

    /// GIVEN the scope request THEN it holds the UI's process, the Windows
    /// Job's 2 GiB with no swap, and removal after failure, in the
    /// signature systemd's `StartTransientUnit` takes, with mode "fail".
    #[test]
    fn given_a_ui_process_then_the_scope_request_caps_its_memory() {
        let body = start_body("persephone-ui-1-0.scope", 4242);
        assert_eq!(body.signature().to_string(), "(ssa(sv)a(sa(sv)))");
        assert_eq!(body.0, "persephone-ui-1-0.scope");
        assert_eq!(body.1, "fail");
        assert!(body.3.is_empty());
        let property = |name: &str| {
            body.2
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(property("MemoryMax"), Some(Value::U64(2_147_483_648)));
        assert_eq!(property("MemorySwapMax"), Some(Value::U64(0)));
        assert_eq!(property("PIDs"), Some(Value::from(vec![4242u32])));
        assert_eq!(
            property("PIDs").unwrap().value_signature().to_string(),
            "au"
        );
        assert_eq!(
            property("CollectMode"),
            Some(Value::from("inactive-or-failed"))
        );
        assert_eq!(body.2.len(), 5);
    }

    /// GIVEN core and run numbers THEN each scope name is a valid, distinct
    /// systemd scope name.
    #[test]
    fn given_runs_then_scope_names_are_valid_and_distinct() {
        let first = unit_name(4242, 0);
        assert_eq!(first, "persephone-ui-4242-0.scope");
        assert_ne!(first, unit_name(4242, 1));
        assert_ne!(first, unit_name(4243, 0));
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))
        );
        assert!(first.len() < 256);
    }

    /// GIVEN `/proc/<pid>/cgroup` texts THEN only the unified line's
    /// absolute path is read, and only a path ending in the scope itself
    /// counts as placed.
    #[test]
    fn given_cgroup_files_then_only_the_scope_itself_counts() {
        let unit = "persephone-ui-1-0.scope";
        let placed =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/persephone-ui-1-0.scope\n";
        assert!(unified_path(placed).is_some_and(|path| in_unit(path, unit)));
        let hybrid = "12:memory:/user.slice\n0::/user.slice/x/persephone-ui-1-0.scope\n";
        assert!(unified_path(hybrid).is_some_and(|path| in_unit(path, unit)));
        for not_yet in [
            "0::/user.slice/user-1000.slice/session-2.scope\n",
            "0::/init.scope\n",
            "0::/persephone-ui-1-0.scope/child\n",
            "0::/a/persephone-ui-1-0.scope.extra\n",
        ] {
            assert!(
                !unified_path(not_yet).is_some_and(|path| in_unit(path, unit)),
                "{not_yet}"
            );
        }
        assert_eq!(unified_path("1:name=systemd:/x\n"), None, "v1 only");
        assert_eq!(unified_path("0::relative\n"), None);
        assert_eq!(unified_path(""), None);
    }

    /// GIVEN a cgroup directory WHEN read THEN its processes and memory cap
    /// come back; GIVEN a removed one THEN it has no processes; GIVEN a
    /// path that climbs out of the root THEN it is refused.
    #[test]
    fn given_cgroup_directories_then_counts_and_caps_are_read() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("app.slice/persephone-ui-1-0.scope");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cgroup.procs"), "101\n102\n103\n").unwrap();
        fs::write(dir.join("memory.max"), "2147483648\n").unwrap();
        let cgroup = Cgroup::at(root.path(), "/app.slice/persephone-ui-1-0.scope").unwrap();
        assert_eq!(cgroup.processes().unwrap(), [101, 102, 103]);
        assert_eq!(cgroup.memory_max().unwrap(), UI_MEMORY_MAX);
        assert!(cgroup.swap_max().is_err(), "no swap accounting");
        fs::write(dir.join("memory.swap.max"), "max\n").unwrap();
        assert!(cgroup.swap_max().is_err(), "swap not limited");
        fs::write(dir.join("memory.swap.max"), "0\n").unwrap();
        assert_eq!(cgroup.swap_max().unwrap(), UI_SWAP_MAX);
        fs::write(dir.join("memory.max"), "max\n").unwrap();
        assert!(cgroup.memory_max().is_err(), "not capped");
        fs::remove_dir_all(&dir).unwrap();
        assert!(cgroup.processes().unwrap().is_empty(), "removed when empty");
        assert!(Cgroup::at(root.path(), "/app.slice/missing.scope").is_none());
        assert!(Cgroup::at(root.path(), "/../etc").is_none());
    }

    /// GIVEN `/proc/<pid>/stat` lines, with spaces and parentheses in the
    /// command name THEN the state and group are read after the last `)`.
    #[test]
    fn given_stat_lines_then_state_and_group_are_read() {
        assert_eq!(
            state_and_group("4242 (persephone-ui) S 1 4242 4242 0 -1"),
            Some(('S', 4242))
        );
        assert_eq!(
            state_and_group("77 (Web (Content) x) Z 4242 4242 4242 0"),
            Some(('Z', 4242))
        );
        assert_eq!(state_and_group("garbage"), None);
        assert_eq!(state_and_group("1 (x) S 1"), None);
        assert_eq!(
            stat_fields("77 (Web (Content) x) S 4242 77 77 0"),
            Some(('S', 4242, 77))
        );
        assert_eq!(stat_fields("1 (x) S 1"), None);
    }

    /// GIVEN a process table WHEN the window's processes are taken THEN
    /// they are the UI, every process below it and every member of its
    /// group, including a grandchild reparented to init and that one's own
    /// child, but not other processes; GIVEN a cgroup list THEN a window
    /// process missing from it is outside the scope.
    #[test]
    fn given_a_process_table_then_window_processes_outside_the_scope_are_found() {
        let entry = |pid, parent, group| ProcessEntry { pid, parent, group };
        let table = [
            entry(1, 0, 1),
            entry(10, 1, 10),
            entry(11, 10, 10),
            entry(12, 11, 12),
            entry(13, 10, 10),
            // A grandchild whose parent exited: reparented, still in the group.
            entry(14, 1, 10),
            entry(15, 14, 15),
            entry(20, 1, 20),
            entry(21, 20, 20),
        ];
        let mut window = window_processes(10, &table);
        window.sort_unstable();
        assert_eq!(window, [10, 11, 12, 13, 14, 15]);
        assert_eq!(window_processes(99, &table), [99], "no processes");
        assert!(outside(&window, &[10, 11, 12, 13, 14, 15, 21]).is_empty());
        assert_eq!(outside(&window, &[10, 11, 12, 13, 15]), [14]);
    }

    fn sleeper(script: &str) -> Child {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        spawn_contained(command).expect("start /bin/sh")
    }

    fn status_field(pid: u32, name: &str) -> String {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap()
            .trim()
            .to_owned()
    }

    /// GIVEN a contained start THEN the child leads its own group, cannot
    /// gain privileges, and is set to die with its spawning thread.
    #[test]
    fn given_a_contained_start_then_the_child_is_set_up() {
        let mut child = sleeper("exec sleep 30");
        let pid = child.id();
        // SAFETY: plain system calls.
        assert_eq!(unsafe { libc::getpgid(pid as libc::pid_t) }, pid as i32);
        assert_ne!(unsafe { libc::getpgid(0) }, pid as i32, "not ours");
        assert_eq!(status_field(pid, "NoNewPrivs:"), "1");
        let group = ProcessGroup::of(&child);
        assert_eq!(group.running().unwrap(), 1);
        let run = UiRun {
            group,
            cgroup: None,
        };
        assert_eq!(
            run.active_processes().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert!(run.terminate(Duration::from_secs(10)));
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    /// GIVEN a contained start from a thread WHEN that thread ends THEN the
    /// child is killed: the reason the core starts the UI from its main
    /// thread.
    #[test]
    fn given_the_spawning_thread_ends_then_the_child_is_killed() {
        let mut child = thread::spawn(|| sleeper("exec sleep 30")).join().unwrap();
        // Without the signal this wait would last the full 30 seconds and
        // then report a normal exit.
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    /// GIVEN a UI with a helper in its group and no scope WHEN the run is
    /// ended or dropped THEN both are killed and the group counts as empty.
    #[test]
    fn given_a_helper_in_the_group_then_terminate_ends_both() {
        let mut child = sleeper("sleep 30 & sleep 30");
        let run = UiRun {
            group: ProcessGroup::of(&child),
            cgroup: None,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while run.running().unwrap() < 2 {
            assert!(Instant::now() < deadline, "the helper did not start");
            thread::sleep(POLL);
        }
        assert!(run.terminate(Duration::from_secs(10)));
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));

        let mut other = sleeper("exec sleep 30");
        drop(UiRun {
            group: ProcessGroup::of(&other),
            cgroup: None,
        });
        assert_eq!(other.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    /// GIVEN a systemd user manager on the session bus (a desktop session,
    /// not CI) WHEN a process is placed THEN it is in its own scope with the
    /// cap read back, the cgroup counts it, and ending the run empties it.
    #[test]
    #[ignore = "needs a systemd user manager with the memory controller"]
    fn given_a_user_manager_then_the_process_is_capped_in_its_scope() {
        let mut child = sleeper("exec sleep 30");
        let unit = unit_name(std::process::id(), 1000);
        let placement = place(child.id(), &unit).expect("placed");
        assert!(
            placement.capped,
            "memory.max and memory.swap.max show the cap"
        );
        let run = UiRun {
            group: ProcessGroup::of(&child),
            cgroup: placement.cgroup,
        };
        assert_eq!(run.active_processes().unwrap(), 1);
        assert!(run.terminate(Duration::from_secs(10)));
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    /// GIVEN no UI executable at the path WHEN the host starts it THEN the
    /// start fails as a spawn failure naming that path, before any placement.
    #[test]
    fn given_a_missing_ui_then_the_start_fails_to_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("persephone-ui");
        match ScopeHost::default().start_at(missing.clone()) {
            Err(StartError::Spawn { ui, error }) => {
                assert_eq!(ui, missing);
                assert_eq!(error.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("expected a spawn failure, got {:?}", other.err()),
        }
    }

    /// GIVEN a user manager WHEN the host starts a UI THEN it runs capped in
    /// a scope of its own, and ending its tree ends it and empties the scope.
    #[test]
    #[ignore = "needs a systemd user manager with the memory controller"]
    fn given_a_user_manager_then_the_host_starts_the_ui_in_its_scope() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ui = dir.path().join("persephone-ui");
        fs::write(
            &ui,
            "#!/bin/sh
exec sleep 30
",
        )
        .unwrap();
        fs::set_permissions(&ui, fs::Permissions::from_mode(0o700)).unwrap();
        let host = ScopeHost::default();
        let StartedUi { mut child, tree } = host.start_at(ui).expect("started");
        let cgroup = read_bounded(&PathBuf::from(format!("/proc/{}/cgroup", child.id()))).unwrap();
        let unit = format!("persephone-ui-{}-", std::process::id());
        assert!(cgroup.contains(&unit), "in its own scope: {cgroup}");
        assert!(!host.warned.load(Ordering::Acquire), "capped: no warning");
        assert_eq!(tree.active_processes().unwrap(), 1);
        assert!(tree.terminate(Duration::from_secs(10)));
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert_eq!(tree.active_processes().unwrap(), 0);
    }

    /// GIVEN a held start WHEN `admit` runs THEN the process is still the
    /// core's own program, its command has not run, and it leads its group;
    /// after `Ok` the command runs as that same process.
    #[test]
    fn given_a_held_start_then_the_command_runs_only_after_admission() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "touch \"$0\"", marker.to_str().unwrap()]);
        let seen = marker.clone();
        let (mut child, held) = spawn_gated(command, move |pid| {
            let exe = fs::read_link(format!("/proc/{pid}/exe"))?;
            let stat = read_bounded(&PathBuf::from(format!("/proc/{pid}/stat")))?;
            Ok((pid, exe, seen.exists(), stat_fields(&stat).map(|f| f.2)))
        })
        .expect("admitted");
        let (pid, exe, ran_early, group) = held;
        assert_eq!(pid, child.id());
        assert_eq!(exe, std::env::current_exe().unwrap(), "still the core");
        assert!(!ran_early, "the command waited at the gate");
        assert_eq!(group, Some(pid as libc::pid_t), "leads its group");
        assert!(child.wait().unwrap().success());
        assert!(marker.exists(), "ran after admission");
    }

    /// GIVEN a held start WHEN `admit` refuses THEN its error comes back and
    /// the command never runs.
    #[test]
    fn given_a_refused_admission_then_the_command_never_runs() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "touch \"$0\"", marker.to_str().unwrap()]);
        let refused = spawn_gated(command, |_| -> io::Result<()> {
            Err(io::Error::other("not placed"))
        });
        match refused {
            Err(GateError::Admit(error)) => assert_eq!(error.to_string(), "not placed"),
            other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
        }
        thread::sleep(Duration::from_millis(200));
        assert!(!marker.exists(), "never ran");
    }

    /// GIVEN a user manager WHEN a held start runs a command that starts a
    /// process in a new session (`setsid`) whose parent then exits THEN that
    /// process is in the scope too: the command ran only after placement.
    #[test]
    #[ignore = "needs a systemd user manager with the memory controller"]
    fn given_a_held_start_then_even_a_new_session_starts_in_the_scope() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "setsid sh -c 'sleep 30 &'; exec sleep 30"]);
        let unit = unit_name(std::process::id(), 1003);
        let (mut child, placement) =
            spawn_gated(command, move |pid| place(pid, &unit)).expect("placed");
        assert!(placement.capped);
        let cgroup = placement.cgroup.expect("the scope's cgroup");
        // Parent and session in a `/proc/<pid>/stat` line.
        let parent_and_session = |pid: libc::pid_t| {
            let stat = read_bounded(&PathBuf::from(format!("/proc/{pid}/stat"))).ok()?;
            let rest = &stat[stat.rfind(')')? + 1..];
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let parent: libc::pid_t = fields.get(1)?.parse().ok()?;
            let session: libc::pid_t = fields.get(3)?.parse().ok()?;
            Some((parent, session))
        };
        let (_, ui_session) = parent_and_session(child.id() as libc::pid_t).unwrap();
        // The helper is `sleep`, started by the `setsid` shell, which leads a
        // new session and then exits. Wait for exactly that: a process in the
        // scope whose session is not the UI's, that does not lead it, and
        // whose session leader (its original parent) has exited, so it was
        // reparented. The test cannot pass before all of that has happened.
        let escaped = |pid: libc::pid_t| {
            let (parent, session) = parent_and_session(pid)?;
            let leader_gone = !Path::new(&format!("/proc/{session}")).exists();
            (session != ui_session && pid != session && parent != session && leader_gone)
                .then_some(pid)
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let helper = loop {
            if let Some(pid) = cgroup.processes().unwrap().into_iter().find_map(escaped) {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "no reparented process in a new session is in the scope"
            );
            thread::sleep(POLL);
        };
        assert!(cgroup.processes().unwrap().contains(&helper));
        let run = UiRun {
            group: ProcessGroup::of(&child),
            cgroup: Some(cgroup),
        };
        assert!(run.terminate(Duration::from_secs(10)));
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    /// GIVEN a process that started a grandchild before it was placed, whose
    /// parent then exited (the grandchild is reparented and stays in the old
    /// cgroup) WHEN it is placed THEN placement fails.
    #[test]
    #[ignore = "needs a systemd user manager with the memory controller"]
    fn given_a_reparented_grandchild_then_placement_fails() {
        let mut child = sleeper("(sleep 30 &); exec sleep 30");
        let group = ProcessGroup::of(&child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while group.running().unwrap() < 2 {
            assert!(Instant::now() < deadline, "the grandchild did not start");
            thread::sleep(POLL);
        }
        let unit = unit_name(std::process::id(), 1002);
        let error = place(child.id(), &unit).err().expect("refused");
        assert!(error.to_string().contains("outside its scope"), "{error}");
        group.kill();
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    /// GIVEN a process with a helper in a new session (`setsid`), started
    /// before placement while its parent lives WHEN it is placed THEN
    /// placement fails and the helper is ended too, though it is in neither
    /// the UI's group nor the scope.
    #[test]
    #[ignore = "needs a systemd user manager with the memory controller"]
    fn given_a_helper_in_a_new_session_then_placement_fails_and_ends_it() {
        let mut child = sleeper("setsid sleep 30 & exec sleep 30");
        let pid = child.id() as libc::pid_t;
        let helper = || {
            live_processes()
                .unwrap()
                .into_iter()
                .find(|entry| entry.parent == pid && entry.group != pid)
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let started = loop {
            if let Some(entry) = helper() {
                break entry;
            }
            assert!(Instant::now() < deadline, "the helper did not start");
            thread::sleep(POLL);
        };
        let unit = unit_name(std::process::id(), 1004);
        let error = place(child.id(), &unit).err().expect("refused");
        assert!(error.to_string().contains("outside its scope"), "{error}");
        let deadline = Instant::now() + Duration::from_secs(5);
        while live_processes()
            .unwrap()
            .iter()
            .any(|entry| entry.pid == started.pid)
        {
            assert!(Instant::now() < deadline, "the helper still runs");
            thread::sleep(POLL);
        }
        ProcessGroup::of(&child).kill();
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    /// GIVEN a process that forked before it was placed (its child stays in
    /// the old cgroup) WHEN it is placed THEN placement fails, so the caller
    /// ends the UI instead of running part of it outside the cap.
    #[test]
    #[ignore = "needs a systemd user manager with the memory controller"]
    fn given_a_child_forked_before_placement_then_placement_fails() {
        let mut child = sleeper("sleep 30 & exec sleep 30");
        let group = ProcessGroup::of(&child);
        // Let the shell fork its background child first.
        let deadline = Instant::now() + Duration::from_secs(5);
        while group.running().unwrap() < 2 {
            assert!(Instant::now() < deadline, "the child did not start");
            thread::sleep(POLL);
        }
        let unit = unit_name(std::process::id(), 1001);
        let error = place(child.id(), &unit).err().expect("refused");
        assert!(error.to_string().contains("outside its scope"), "{error}");
        group.kill();
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }
}

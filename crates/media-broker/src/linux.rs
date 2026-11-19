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
    unix::{self, Limits, LongLivedWorker, StartedCallWorker, UnixWorker},
};
use std::{
    env,
    ffi::{OsString, c_uint},
    fs, io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::Path,
};

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

/// The address-space limit of the call worker (ADR-0049): a bound, not the
/// 512 MiB of the Windows Job, because libwebrtc reserves much more address
/// space than it uses.
const CALL_ADDRESS_SPACE: u64 = 4 * 1024 * 1024 * 1024;

/// Which camera path the call worker may use (ADR-0049). The core decides it
/// before it starts the worker; the worker gets it as one argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallCamera {
    /// The PipeWire remote the core sends over the camera channel.
    Portal,
    /// The worker opens `/dev/video*` itself.
    V4l2,
    /// No camera.
    None,
}

impl CallCamera {
    /// The worker's one command-line argument.
    fn argument(self) -> &'static str {
        match self {
            Self::Portal => "--camera=portal",
            Self::V4l2 => "--camera=v4l2",
            Self::None => "--camera=none",
        }
    }
}

/// The environment of the call worker: the sound server's socket directory
/// and the home directory (its cookie), each only when it is an absolute
/// path, and `LANG=C`. There is no `DBUS_SESSION_BUS_ADDRESS`.
fn call_environment(
    mut get: impl FnMut(&str) -> Option<OsString>,
) -> Vec<(&'static str, OsString)> {
    let mut vars = Vec::new();
    for name in ["XDG_RUNTIME_DIR", "HOME"] {
        if let Some(value) = get(name).filter(|value| Path::new(value).is_absolute()) {
            vars.push((name, value));
        }
    }
    vars.push(("LANG", OsString::from("C")));
    vars
}

/// Starts the call worker with the Linux launch rules of ADR-0049: the
/// media worker's launch (new session, parent-death `SIGKILL`,
/// `no_new_privs`, descriptors above 2 closed at exec) with the call limits,
/// the environment of [`call_environment`], the `--camera` argument and the
/// camera channel as descriptor 3. The worker contains itself and reports
/// `Ready`; the caller then calls `confirm_contained` on the returned worker
/// before it sends any input.
pub fn start_call_worker(
    _profile: &SeccompContainment,
    exe: &Path,
    camera: CallCamera,
) -> Result<StartedCallWorker, LaunchError> {
    let vars = call_environment(|name| env::var_os(name));
    spawn_call(exe, &[camera.argument()], &vars)
}

fn spawn_call(
    program: &Path,
    args: &[&str],
    vars: &[(&'static str, OsString)],
) -> Result<StartedCallWorker, LaunchError> {
    let mut command = unix::command(program)?;
    command.args(args);
    for (name, value) in vars {
        command.env(name, value);
    }
    let (broker_end, child_end) = UnixStream::pair().map_err(|e| unix::error("socketpair", e))?;
    let child_fd = child_end.as_raw_fd();
    // SAFETY: getpid takes no arguments.
    let broker = unsafe { libc::getpid() };
    // SAFETY: the closure runs between fork and exec and makes only system
    // calls; it allocates nothing.
    unsafe {
        command.pre_exec(move || call_child_setup(broker, child_fd));
    }
    let spawned = unix::spawn(command, confirm)?;
    // The worker has its own copy; this one must not stay open here.
    drop(child_end);
    Ok(StartedCallWorker {
        worker: LongLivedWorker::new(spawned.worker),
        stdin: spawned.stdin,
        stdout: spawned.stdout,
        camera: Some(broker_end),
    })
}

/// Runs in the child between fork and exec. Async-signal-safe.
fn call_child_setup(broker: libc::pid_t, child_fd: libc::c_int) -> io::Result<()> {
    unix::new_session()?;
    cloexec_above_stderr()?;
    prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong)?;
    // SAFETY: getppid takes no arguments.
    if unsafe { libc::getppid() } != broker {
        return Err(io::Error::from_raw_os_error(libc::ESRCH));
    }
    prctl(libc::PR_SET_NO_NEW_PRIVS, 1)?;
    // After the close-on-exec marking, so that only descriptor 3 survives.
    unix::inherit_as_fd3(child_fd)?;
    unix::set_call_limits(Some(CALL_ADDRESS_SPACE))
}

/// The data byte of a camera descriptor on the camera channel (ADR-0053).
pub const KIND_CAMERA: u8 = b'c';
/// The data byte of a screen share descriptor on the camera channel.
pub const KIND_SCREEN: u8 = b's';

/// Sends the camera portal's remote `fd` to the worker over the camera
/// channel: one descriptor (`SCM_RIGHTS`) and one data byte, `c`. It never
/// blocks: a full channel is `WouldBlock`.
pub fn send_camera_fd(channel: &UnixStream, fd: BorrowedFd<'_>) -> io::Result<()> {
    send_fd(channel, fd, KIND_CAMERA)
}

/// Sends the screen cast portal's remote `fd` to the worker over the camera
/// channel with the data byte `s` (ADR-0053). The worker keeps at most four
/// of each kind; any other byte closes its channel.
pub fn send_screen_fd(channel: &UnixStream, fd: BorrowedFd<'_>) -> io::Result<()> {
    send_fd(channel, fd, KIND_SCREEN)
}

fn send_fd(channel: &UnixStream, fd: BorrowedFd<'_>, kind: u8) -> io::Result<()> {
    use std::mem::{size_of, zeroed};
    let byte = [kind; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_ptr() as *mut _,
        iov_len: byte.len(),
    };
    // An array of u64 aligns the buffer for `cmsghdr`; 32 bytes hold one
    // descriptor on every Linux ABI.
    let mut control = [0_u64; 4];
    let raw = fd.as_raw_fd();
    // SAFETY: msghdr is plain data; zero is a valid empty value.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    // SAFETY: CMSG_SPACE and CMSG_LEN are pure size computations.
    message.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) } as _;
    // SAFETY: the control buffer holds one `cmsghdr` and one descriptor.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<libc::c_int>(), raw);
    }
    loop {
        // SAFETY: `message` points at buffers that outlive the call.
        let sent = unsafe {
            libc::sendmsg(
                channel.as_fd().as_raw_fd(),
                &message,
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if sent == 1 {
            return Ok(());
        }
        let error = if sent < 0 {
            io::Error::last_os_error()
        } else {
            io::Error::from(io::ErrorKind::WriteZero)
        };
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
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
    fn given_camera_modes_then_each_has_its_one_argument() {
        assert_eq!(CallCamera::Portal.argument(), "--camera=portal");
        assert_eq!(CallCamera::V4l2.argument(), "--camera=v4l2");
        assert_eq!(CallCamera::None.argument(), "--camera=none");
    }

    /// GIVEN a broker environment WHEN the call worker's is built THEN only
    /// absolute `XDG_RUNTIME_DIR` and `HOME` and `LANG=C` pass.
    #[test]
    fn given_broker_environment_then_the_call_worker_gets_an_allowlist() {
        let vars = call_environment(|name| match name {
            "XDG_RUNTIME_DIR" => Some("/run/user/1000".into()),
            "HOME" => Some("/home/a".into()),
            _ => Some("leak".into()),
        });
        assert_eq!(
            vars,
            vec![
                ("XDG_RUNTIME_DIR", OsString::from("/run/user/1000")),
                ("HOME", OsString::from("/home/a")),
                ("LANG", OsString::from("C")),
            ]
        );
        // Relative or missing values are dropped.
        let vars = call_environment(|name| (name == "HOME").then(|| "relative".into()));
        assert_eq!(vars, vec![("LANG", OsString::from("C"))]);
    }

    fn run_sh(script: &str, vars: &[(&'static str, OsString)]) -> (String, StartedCallWorker) {
        use std::io::Read;
        let mut started = spawn_call(Path::new("/bin/sh"), &["-c", script], vars).unwrap();
        let mut out = String::new();
        started.stdout.read_to_string(&mut out).unwrap();
        (out, started)
    }

    /// GIVEN a launched call worker WHEN it lists itself THEN descriptor 3 is
    /// a socket, no other descriptor above 2 is open, only the allowlisted
    /// environment is set, and the limits are the call limits.
    #[test]
    fn given_a_launched_call_worker_then_it_has_only_the_channel_and_the_call_limits() {
        let script = "readlink /proc/self/fd/3; ls /proc/self/fd | tr '\n' ' '; echo;                       env | sort | tr '\n' ' '; echo;                       grep -E 'Max (address space|file size|core file size|open files|processes)'                       /proc/self/limits";
        let vars = [
            ("HOME", OsString::from("/home/x")),
            ("LANG", OsString::from("C")),
        ];
        let (out, started) = run_sh(script, &vars);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("socket:"), "{out}");
        // 0 1 2 3, plus the descriptor `ls` opens for /proc/self/fd.
        let fds: Vec<&str> = lines[1].split_whitespace().collect();
        assert!(fds.starts_with(&["0", "1", "2", "3"]), "{out}");
        assert!(fds.len() <= 5, "{out}");
        assert!(
            lines[2].contains("HOME=/home/x") && lines[2].contains("LANG=C"),
            "{out}"
        );
        assert!(!lines[2].contains("PATH="), "{out}");
        // "Max <name> <soft> <hard> <unit>": the soft limit is the first
        // number or word after the name.
        let soft = |name: &str| -> String {
            let line = lines[3..]
                .iter()
                .find(|line| line.contains(name))
                .expect(name);
            let words: Vec<&str> = line.split_whitespace().collect();
            let at = words
                .iter()
                .position(|word| word.chars().all(|c| c.is_ascii_digit()) || *word == "unlimited");
            words[at.expect(name)].to_owned()
        };
        assert_eq!(soft("address space"), "4294967296", "{out}");
        assert_eq!(soft("file size"), "0", "{out}");
        assert_eq!(soft("core file size"), "0", "{out}");
        assert_eq!(soft("open files"), "256", "{out}");
        // The process limit is the broker's, not 0: the call worker starts
        // threads.
        assert_ne!(soft("processes"), "0", "{out}");
        drop(started);
    }

    /// GIVEN a launched worker that is not sandboxed WHEN it is checked THEN
    /// it is not contained, and killing it ends it.
    #[test]
    fn given_a_call_worker_without_seccomp_then_it_is_not_contained_and_can_be_killed() {
        let mut started =
            spawn_call(Path::new("/bin/sh"), &["-c", "exec /bin/sleep 30"], &[]).unwrap();
        assert!(!started.worker.confirm_contained());
        started.worker.kill();
        assert!(started.worker.wait_exit(Duration::from_secs(5)));
        drop(started.camera.take());
    }

    /// GIVEN the broker's end of the channel WHEN a descriptor is sent THEN
    /// the worker's end receives one byte and one descriptor.
    #[test]
    fn given_a_descriptor_when_sent_then_the_other_end_gets_one_byte_and_one_descriptor() {
        let (broker, worker) = UnixStream::pair().unwrap();
        let file = fs::File::open("/dev/null").unwrap();
        send_camera_fd(&broker, file.as_fd()).unwrap();
        send_screen_fd(&broker, file.as_fd()).unwrap();
        // One byte arrives.
        let mut byte = [0_u8; 1];
        // SAFETY: recvmsg with a one-byte buffer and a control buffer.
        let mut control = [0_u64; 4];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = 32;
        let got = unsafe { libc::recvmsg(worker.as_raw_fd(), &mut message, 0) };
        assert_eq!(got, 1);
        assert_eq!(byte, [b'c'], "a camera descriptor carries the byte c");
        // SAFETY: the buffer was filled by recvmsg.
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        assert!(!header.is_null());
        assert_eq!(unsafe { (*header).cmsg_type }, libc::SCM_RIGHTS);
        // The screen descriptor came second, with the byte s.
        let mut second = [0_u8; 1];
        let mut control = [0_u64; 4];
        let mut iov = libc::iovec {
            iov_base: second.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = 32;
        let got = unsafe { libc::recvmsg(worker.as_raw_fd(), &mut message, 0) };
        assert_eq!(got, 1);
        assert_eq!(second, [b's'], "a screen descriptor carries the byte s");
        drop(worker);
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

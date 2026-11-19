//! The Linux containment of the call worker (ADR-0049). The worker applies it
//! itself, in two stages.
//!
//! * Stage 1 ([`missing_controls`]) runs before the worker reads any input and
//!   before it starts a thread: `PR_SET_DUMPABLE` 0, a check that the launcher
//!   set `no_new_privs`, Landlock (ABI 3 or newer) with the path table below,
//!   and a seccomp deny list. A control that cannot be applied sets its bit.
//! * Stage 2 ([`stage2`]) runs after the sound server connection and before
//!   `Room::connect`: no new `AF_UNIX` socket and no more Landlock calls.
//!
//! The filter is a deny list, not an allow list: libwebrtc, the sound server
//! client and PipeWire make too many system calls for an allow list that would
//! survive library updates. Landlock is the file boundary; the deny list
//! removes process, kernel and namespace access.
//!
//! The pure parts (the path table, the ABI rules and the filter tables) have
//! unit tests. The system calls need a Linux kernel with Landlock.

use std::collections::BTreeMap;
use std::ffi::{CString, OsStr, OsString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use super::{
    CHECK_FAILED, MISSING_DUMPABLE_OFF, MISSING_LANDLOCK, MISSING_NO_NEW_PRIVS, MISSING_SECCOMP,
};
use crate::args::CameraMode;

/// The checks of stage 1, for the camera mode the launcher chose.
pub fn missing_controls() -> u32 {
    stage1(crate::camera::mode())
}

/// Stage 2. Fails when the second filter cannot be installed for every thread.
pub fn stage2() -> Result<(), ()> {
    let filters = [
        compile(&stage2_kill_rules(), SeccompAction::KillProcess)?,
        compile(&stage2_unix_rules()?, STAGE2_UNIX_ACTION)?,
    ];
    install(&filters)
}

// ---------------------------------------------------------------------------
// Stage 1
// ---------------------------------------------------------------------------

/// An unused prctl argument, passed as a full word.
const NONE: libc::c_ulong = 0;

fn stage1(mode: CameraMode) -> u32 {
    let mut missing = 0;
    // SAFETY: prctl with integer arguments only.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, NONE, NONE, NONE, NONE) } != 0 {
        missing |= MISSING_DUMPABLE_OFF | CHECK_FAILED;
    }
    // SAFETY: as above; PR_GET_NO_NEW_PRIVS returns 0 or 1.
    match unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, NONE, NONE, NONE, NONE) } {
        1 => {}
        0 => {
            // Neither Landlock nor seccomp can be applied without it.
            return missing | MISSING_NO_NEW_PRIVS | MISSING_LANDLOCK | MISSING_SECCOMP;
        }
        _ => return missing | CHECK_FAILED | MISSING_LANDLOCK | MISSING_SECCOMP,
    }
    if landlock(mode).is_err() {
        missing |= MISSING_LANDLOCK;
    }
    if install_stage1().is_err() {
        missing |= MISSING_SECCOMP;
    }
    missing
}

// ---------------------------------------------------------------------------
// Landlock
// ---------------------------------------------------------------------------

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;

/// The oldest Landlock ABI the worker accepts (Linux 6.2: it has `TRUNCATE`).
const MIN_ABI: i64 = 3;

// File system rights, by ABI that introduced them.
const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_REG: u64 = 1 << 8;
/// ABI 1 has the rights of bits 0 to 12.
const FS_ABI1_ALL: u64 = (1 << 13) - 1;
const FS_REFER: u64 = 1 << 13; // ABI 2
const FS_TRUNCATE: u64 = 1 << 14; // ABI 3
const FS_IOCTL_DEV: u64 = 1 << 15; // ABI 5

// Scopes (ABI 6).
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

/// The newest ABI whose rights this code knows. A newer kernel may add
/// rights; they stay unhandled, so they restrict nothing and cannot block the
/// sound server's socket by surprise.
const KNOWN_ABI: i64 = 7;

/// The file system rights handled (denied unless a rule grants them) on this
/// ABI: every right this code knows up to the running ABI.
fn handled_fs(abi: i64) -> u64 {
    let abi = abi.min(KNOWN_ABI);
    let mut rights = FS_ABI1_ALL;
    if abi >= 2 {
        rights |= FS_REFER;
    }
    if abi >= 3 {
        rights |= FS_TRUNCATE;
    }
    if abi >= 5 {
        rights |= FS_IOCTL_DEV;
    }
    rights
}

/// The scopes on this ABI: abstract Unix sockets and signals stay inside the
/// worker (ABI 6 or newer), else none.
fn scoped(abi: i64) -> u64 {
    if abi >= 6 {
        SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
    } else {
        0
    }
}

/// The size of `landlock_ruleset_attr` that the ABI knows: 8, 16 or 24 bytes.
fn attr_size(abi: i64) -> usize {
    match abi {
        ..=3 => 8,
        4 | 5 => 16,
        _ => 24,
    }
}

/// Whether the worker may run with this ABI.
fn abi_ok(abi: i64) -> bool {
    abi >= MIN_ABI
}

/// What a rule on a directory grants.
const DIR_READ_EXEC: u64 = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR;
const DIR_READ: u64 = FS_READ_FILE | FS_READ_DIR;
const DIR_SHM: u64 =
    FS_READ_FILE | FS_READ_DIR | FS_WRITE_FILE | FS_TRUNCATE | FS_MAKE_REG | FS_REMOVE_FILE;
/// What a rule on a file grants. A rule on a file may hold only these.
const FILE_RIGHTS: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;
const FILE_READ: u64 = FS_READ_FILE;

/// One allowed path.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PathRule {
    path: PathBuf,
    access: u64,
}

/// The path table of ADR-0049 for a camera mode. `home` is `$HOME` and
/// `runtime` is `$XDG_RUNTIME_DIR`; each is used only when absolute.
/// `video_nodes` are the `/dev/video*` character devices found before the
/// rules apply (used with `--camera=v4l2` only). Rights are cut to the file
/// or directory kind and to the handled set when the rules are added.
fn path_table(
    mode: CameraMode,
    home: Option<&Path>,
    runtime: Option<&Path>,
    video_nodes: &[PathBuf],
) -> Vec<PathRule> {
    let rule = |path: &str, access| PathRule {
        path: PathBuf::from(path),
        access,
    };
    let mut table = vec![
        rule("/usr", DIR_READ_EXEC),
        rule("/lib", DIR_READ_EXEC),
        rule("/lib64", DIR_READ_EXEC),
        rule("/etc", DIR_READ_EXEC),
        rule("/run/systemd/resolve", DIR_READ_EXEC),
        rule("/proc/self", DIR_READ_EXEC),
        rule("/dev/urandom", FILE_READ),
        rule("/dev/null", FILE_READ),
        rule("/dev/shm", DIR_SHM),
    ];
    if let Some(home) = home.filter(|home| home.is_absolute()) {
        table.push(PathRule {
            path: home.join(".config/pulse"),
            access: DIR_READ,
        });
    }
    // The sound server's client library opens this directory and checks its
    // owner before it connects to the socket in it; without the rule the
    // check fails and no sound server is reached. Read only: the connect
    // itself needs no Landlock right on the ABIs this code handles.
    if let Some(runtime) = runtime.filter(|runtime| runtime.is_absolute()) {
        table.push(PathRule {
            path: runtime.join("pulse"),
            access: DIR_READ,
        });
    }
    if mode == CameraMode::V4l2 {
        for node in video_nodes {
            table.push(PathRule {
                path: node.clone(),
                access: FS_READ_FILE | FS_WRITE_FILE | FS_IOCTL_DEV,
            });
        }
    }
    table
}

/// The most `/dev/video<N>` nodes the worker allows (N below this) and probes.
pub const MAX_VIDEO_NODES: u32 = 64;

/// Whether a directory entry name is a V4L2 node name: `video` and the plain
/// decimal number of a node below [`MAX_VIDEO_NODES`] (no sign, no leading
/// zero).
fn is_video_node_name(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    let Some(digits) = bytes.strip_prefix(b"video") else {
        return false;
    };
    let plain = !digits.is_empty()
        && digits.len() <= 2
        && digits.iter().all(u8::is_ascii_digit)
        && (digits.len() == 1 || digits[0] != b'0');
    plain
        && std::str::from_utf8(digits).is_ok_and(|text| {
            text.parse::<u32>()
                .is_ok_and(|index| index < MAX_VIDEO_NODES)
        })
}

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The Landlock ABI of the running kernel; `None` when Landlock is missing or
/// turned off.
fn landlock_abi() -> Option<i64> {
    // SAFETY: a version query takes no attribute (null, size 0).
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0_usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    (abi > 0).then_some(abi)
}

/// The `/dev/video*` character devices. Read before Landlock applies, so only
/// devices present now can be opened later.
fn find_video_nodes() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut nodes: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| is_video_node_name(&entry.file_name()))
        .map(|entry| entry.path())
        .collect();
    nodes.sort();
    nodes
}

fn landlock(mode: CameraMode) -> Result<(), ()> {
    let abi = landlock_abi().ok_or(())?;
    if !abi_ok(abi) {
        return Err(());
    }
    let handled = handled_fs(abi);
    let attr = RulesetAttr {
        handled_access_fs: handled,
        handled_access_net: 0,
        scoped: scoped(abi),
    };
    // SAFETY: `attr` is a valid, initialised struct of at least `attr_size`
    // bytes; the kernel reads that many.
    let raw = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &raw const attr,
            attr_size(abi),
            0_u32,
        )
    };
    if raw < 0 {
        return Err(());
    }
    // SAFETY: the kernel returned a new descriptor that nothing else owns.
    let ruleset = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let nodes = if mode == CameraMode::V4l2 {
        find_video_nodes()
    } else {
        Vec::new()
    };
    for rule in path_table(mode, home.as_deref(), runtime.as_deref(), &nodes) {
        // A path that does not exist or cannot be opened gets no rule. Rules
        // only allow, so the process ends up with less access, never more.
        let Some((fd, is_dir)) = open_path(&rule.path) else {
            continue;
        };
        let kind = if is_dir { u64::MAX } else { FILE_RIGHTS };
        let beneath = PathBeneathAttr {
            allowed_access: rule.access & kind & handled,
            parent_fd: fd.as_raw_fd(),
        };
        if beneath.allowed_access == 0 {
            continue;
        }
        // SAFETY: `beneath` is a valid packed struct for the call; the
        // ruleset and parent descriptors are open.
        let added = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset.as_raw_fd(),
                LANDLOCK_RULE_PATH_BENEATH,
                &raw const beneath,
                0_u32,
            )
        };
        if added != 0 {
            return Err(());
        }
    }
    // SAFETY: the ruleset descriptor is open; no flags.
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0_u32) } != 0 {
        return Err(());
    }
    Ok(())
}

/// Opens `path` for use as a Landlock parent. Returns the descriptor and
/// whether it is a directory; `None` when it cannot be opened.
fn open_path(path: &Path) -> Option<(OwnedFd, bool)> {
    let path = CString::new(OsString::from(path).into_vec()).ok()?;
    // SAFETY: `path` is a NUL-terminated string.
    let raw = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if raw < 0 {
        return None;
    }
    // SAFETY: `open` returned a new descriptor that nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: `stat` is plain data; fstat writes it.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `stat` is a valid out-parameter for an open descriptor.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
        return None;
    }
    Some((fd, stat.st_mode & libc::S_IFMT == libc::S_IFDIR))
}

// ---------------------------------------------------------------------------
// seccomp
// ---------------------------------------------------------------------------

/// `AF_UNIX`, `AF_INET`, `AF_INET6` and `AF_NETLINK`: the only socket
/// families the worker may use.
const ALLOWED_FAMILIES: [libc::c_int; 4] = [
    libc::AF_UNIX,
    libc::AF_INET,
    libc::AF_INET6,
    libc::AF_NETLINK,
];

/// `CLONE_NEW*` flags that `clone` may not carry. (`CLONE_NEWTIME` is not a
/// `clone` flag: its bit is part of the exit signal.)
const CLONE_NEW_FLAGS: [u64; 7] = [
    libc::CLONE_NEWNS as u64,
    libc::CLONE_NEWCGROUP as u64,
    libc::CLONE_NEWUTS as u64,
    libc::CLONE_NEWIPC as u64,
    libc::CLONE_NEWUSER as u64,
    libc::CLONE_NEWPID as u64,
    libc::CLONE_NEWNET as u64,
];

/// What stage 2 does with a new `AF_UNIX` socket. ADR-0049 says kill. The
/// call fails with an error instead, because glibc's name lookup opens an
/// `AF_UNIX` socket to the name service cache daemon before every first
/// lookup and handles the failure; a kill would end the call at the SFU's
/// name lookup. The system call does not run in either case.
const STAGE2_UNIX_ACTION: SeccompAction = SeccompAction::Errno(libc::EACCES as u32);

/// Calls that kill the worker in both stages, whatever their arguments. The
/// names are the same on x86_64 and aarch64; `libc` supplies each number.
fn deny_calls() -> Vec<(&'static str, i64)> {
    macro_rules! calls {
        ($($name:ident),* $(,)?) => {
            vec![$((stringify!($name), libc::$name)),*]
        };
    }
    calls![
        // Run or inspect other processes.
        SYS_execve,
        SYS_execveat,
        SYS_ptrace,
        SYS_process_vm_readv,
        SYS_process_vm_writev,
        SYS_kill,
        SYS_pidfd_open,
        SYS_pidfd_getfd,
        SYS_pidfd_send_signal,
        // Mounts and namespaces.
        SYS_mount,
        SYS_umount2,
        SYS_move_mount,
        SYS_fsmount,
        SYS_fsopen,
        SYS_fsconfig,
        SYS_fspick,
        SYS_open_tree,
        SYS_mount_setattr,
        SYS_pivot_root,
        SYS_chroot,
        SYS_unshare,
        SYS_setns,
        // Kernel facilities that a media client never needs.
        SYS_bpf,
        SYS_perf_event_open,
        SYS_keyctl,
        SYS_add_key,
        SYS_request_key,
        SYS_userfaultfd,
        SYS_io_uring_setup,
        SYS_io_uring_enter,
        SYS_io_uring_register,
        SYS_open_by_handle_at,
        SYS_name_to_handle_at,
        SYS_init_module,
        SYS_finit_module,
        SYS_delete_module,
        SYS_kexec_load,
        SYS_kexec_file_load,
        SYS_reboot,
        SYS_swapon,
        SYS_swapoff,
        SYS_acct,
        SYS_quotactl,
        SYS_personality,
    ]
}

fn arg(index: u8, op: SeccompCmpOp, value: u64) -> Result<SeccompCondition, ()> {
    SeccompCondition::new(index, SeccompCmpArgLen::Dword, op, value).map_err(|_| ())
}

fn rule(conditions: Vec<SeccompCondition>) -> Result<SeccompRule, ()> {
    SeccompRule::new(conditions).map_err(|_| ())
}

/// A rule that matches a socket family other than the four allowed ones.
fn other_family_rule() -> Result<SeccompRule, ()> {
    let mut conditions = Vec::new();
    for family in ALLOWED_FAMILIES {
        conditions.push(arg(0, SeccompCmpOp::Ne, family as u64)?);
    }
    rule(conditions)
}

/// The signal calls that name the target thread group in argument 0. The
/// worker may signal its own threads only. `kill` and `pidfd_send_signal` are
/// in the deny list. `tkill` has no thread group, so a rule cannot tie it to
/// this process; see [`stage1_errno_rules`]. Landlock ABI 3 to 5 has no signal
/// scope and the worker has the core's UID, so this rule holds for every ABI.
const OWN_GROUP_SIGNAL_CALLS: [i64; 3] = [
    libc::SYS_tgkill,
    libc::SYS_rt_tgsigqueueinfo,
    libc::SYS_rt_sigqueueinfo,
];

/// The rules that kill in stage 1: the deny list, `clone` with a namespace
/// flag, a socket of another family and a signal to another process.
/// `pid` is the worker's own process ID. `clone3` and `tkill` are not here
/// (see [`stage1_errno_rules`]).
fn stage1_kill_rules(pid: u32) -> Result<BTreeMap<i64, Vec<SeccompRule>>, ()> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for (_, number) in deny_calls() {
        rules.insert(number, Vec::new());
    }
    let mut clone = Vec::new();
    for flag in CLONE_NEW_FLAGS {
        // One rule per flag; the rules of one call are alternatives.
        clone.push(rule(vec![arg(0, SeccompCmpOp::MaskedEq(flag), flag)?])?);
    }
    rules.insert(libc::SYS_clone, clone);
    rules.insert(libc::SYS_socket, vec![other_family_rule()?]);
    rules.insert(libc::SYS_socketpair, vec![other_family_rule()?]);
    for number in OWN_GROUP_SIGNAL_CALLS {
        // The kernel reads the thread group ID as 32 bits; so does the rule.
        rules.insert(
            number,
            vec![rule(vec![arg(0, SeccompCmpOp::Ne, u64::from(pid))?])?],
        );
    }
    Ok(rules)
}

/// `clone3` fails with `ENOSYS`, so that the C library falls back to `clone`,
/// whose flags the filter can read (`clone3` passes them in memory). `tkill`
/// fails with `ENOSYS` too: the C library and Rust use `tgkill`, and a rule
/// cannot tie a bare thread ID to this process.
fn stage1_errno_rules() -> BTreeMap<i64, Vec<SeccompRule>> {
    BTreeMap::from([
        (libc::SYS_clone3, Vec::new()),
        (libc::SYS_tkill, Vec::new()),
    ])
}

/// The worker's own process ID, for the signal rules.
fn own_pid() -> u32 {
    // SAFETY: `getpid` has no arguments and cannot fail.
    unsafe { libc::getpid() as u32 }
}

/// What stage 2 kills: the Landlock calls.
fn stage2_kill_rules() -> BTreeMap<i64, Vec<SeccompRule>> {
    BTreeMap::from([
        (libc::SYS_landlock_create_ruleset, Vec::new()),
        (libc::SYS_landlock_add_rule, Vec::new()),
        (libc::SYS_landlock_restrict_self, Vec::new()),
    ])
}

/// What stage 2 refuses: a new `AF_UNIX` socket, by `socket` or `socketpair`.
fn stage2_unix_rules() -> Result<BTreeMap<i64, Vec<SeccompRule>>, ()> {
    let unix = || rule(vec![arg(0, SeccompCmpOp::Eq, libc::AF_UNIX as u64)?]);
    Ok(BTreeMap::from([
        (libc::SYS_socket, vec![unix()?]),
        (libc::SYS_socketpair, vec![unix()?]),
    ]))
}

/// Compiles rules into a program: a call with a matching rule gets `action`,
/// every other call is allowed.
fn compile(
    rules: &BTreeMap<i64, Vec<SeccompRule>>,
    action: SeccompAction,
) -> Result<BpfProgram, ()> {
    compile_for(rules, action, target_arch()?)
}

fn compile_for(
    rules: &BTreeMap<i64, Vec<SeccompRule>>,
    action: SeccompAction,
    arch: TargetArch,
) -> Result<BpfProgram, ()> {
    SeccompFilter::new(rules.clone(), SeccompAction::Allow, action, arch)
        .map_err(|_| ())?
        .try_into()
        .map_err(|_| ())
}

fn target_arch() -> Result<TargetArch, ()> {
    TargetArch::try_from(std::env::consts::ARCH).map_err(|_| ())
}

/// Installs every program on every thread (TSYNC). The kernel runs all of
/// them and takes the strictest verdict.
fn install(programs: &[BpfProgram]) -> Result<(), ()> {
    for program in programs {
        seccompiler::apply_filter_all_threads(program).map_err(|_| ())?;
    }
    Ok(())
}

fn install_stage1() -> Result<(), ()> {
    let kill = compile(&stage1_kill_rules(own_pid())?, SeccompAction::KillProcess)?;
    let errno = compile(
        &stage1_errno_rules(),
        SeccompAction::Errno(libc::ENOSYS as u32),
    )?;
    // No call is in both programs. The order does not matter.
    install(&[errno, kill])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set in the child run of the signal test.
    const SIGNAL_PROBE: &str = "PERSEPHONE_SIGNAL_PROBE";

    /// GIVEN the stage 1 filters installed on the running kernel WHEN the
    /// process signals its own thread, calls `tkill`, then signals its
    /// parent THEN the own signal works, `tkill` fails with ENOSYS, and the
    /// signal to the parent kills the process with SIGSYS. The filters run
    /// in a child process (this test, started again), because they cannot
    /// be removed.
    #[test]
    fn given_stage1_installed_then_only_own_signals_pass() {
        if std::env::var_os(SIGNAL_PROBE).is_some() {
            probe_signals();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "given_stage1_installed_then_only_own_signals_pass",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(SIGNAL_PROBE, "1")
            .output()
            .expect("child run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("own signal: ok"), "{stdout}");
        assert!(stdout.contains("tkill: ENOSYS"), "{stdout}");
        assert!(!stdout.contains("parent signal: not killed"), "{stdout}");
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(output.status.signal(), Some(libc::SIGSYS), "{stdout}");
    }

    /// The child side: installs stage 1 and tries each signal. Signal 0
    /// checks the target and delivers nothing.
    fn probe_signals() {
        use std::io::Write;
        // SAFETY: prctl with integer arguments only.
        let no_new_privs = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, NONE, NONE, NONE) };
        assert_eq!(no_new_privs, 0);
        install_stage1().expect("stage 1 installs");
        // SAFETY: getpid, gettid and getppid take no arguments; tgkill and
        // tkill with signal 0 only check their target.
        let (pid, tid, parent) = unsafe { (libc::getpid(), libc::gettid(), libc::getppid()) };
        let own = unsafe { libc::syscall(libc::SYS_tgkill, pid, tid, 0) };
        println!("own signal: {}", if own == 0 { "ok" } else { "failed" });
        let tkill = unsafe { libc::syscall(libc::SYS_tkill, tid, 0) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        let refused = tkill == -1 && errno == Some(libc::ENOSYS);
        println!("tkill: {}", if refused { "ENOSYS" } else { "allowed" });
        let _ = std::io::stdout().flush();
        // SAFETY: as above; the filter kills the process here.
        unsafe { libc::syscall(libc::SYS_tgkill, parent, parent, 0) };
        println!("parent signal: not killed");
    }

    /// A fixed process ID for the rule builder.
    const TEST_PID: u32 = 4242;

    fn names() -> Vec<&'static str> {
        deny_calls().into_iter().map(|(name, _)| name).collect()
    }

    /// GIVEN the deny list THEN it holds the calls of ADR-0049 and compiles
    /// for both supported architectures.
    #[test]
    fn given_deny_list_then_it_has_the_adr_calls_and_compiles() {
        let names = names();
        for wanted in [
            "SYS_execve",
            "SYS_execveat",
            "SYS_ptrace",
            "SYS_process_vm_readv",
            "SYS_process_vm_writev",
            "SYS_kill",
            "SYS_pidfd_open",
            "SYS_pidfd_getfd",
            "SYS_pidfd_send_signal",
            "SYS_mount",
            "SYS_umount2",
            "SYS_move_mount",
            "SYS_fsmount",
            "SYS_fsopen",
            "SYS_open_tree",
            "SYS_pivot_root",
            "SYS_chroot",
            "SYS_unshare",
            "SYS_setns",
            "SYS_bpf",
            "SYS_perf_event_open",
            "SYS_keyctl",
            "SYS_add_key",
            "SYS_request_key",
            "SYS_userfaultfd",
            "SYS_io_uring_setup",
            "SYS_io_uring_enter",
            "SYS_io_uring_register",
            "SYS_open_by_handle_at",
            "SYS_name_to_handle_at",
            "SYS_init_module",
            "SYS_finit_module",
            "SYS_delete_module",
            "SYS_kexec_load",
            "SYS_reboot",
            "SYS_swapon",
            "SYS_swapoff",
            "SYS_acct",
            "SYS_quotactl",
            "SYS_personality",
        ] {
            assert!(names.contains(&wanted), "{wanted} is not denied");
        }
        let rules = stage1_kill_rules(TEST_PID).expect("rules");
        for arch in [TargetArch::x86_64, TargetArch::aarch64] {
            let program = compile_for(&rules, SeccompAction::KillProcess, arch).expect("compiles");
            assert!(!program.is_empty());
            let errno = compile_for(
                &stage1_errno_rules(),
                SeccompAction::Errno(libc::ENOSYS as u32),
                arch,
            )
            .expect("compiles");
            assert!(!errno.is_empty());
        }
    }

    /// GIVEN the deny list THEN no call the worker needs is in it.
    #[test]
    fn given_deny_list_then_the_calls_the_worker_needs_are_allowed() {
        let rules = stage1_kill_rules(TEST_PID).expect("rules");
        for needed in [
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_openat,
            libc::SYS_mmap,
            libc::SYS_mprotect,
            libc::SYS_futex,
            libc::SYS_connect,
            libc::SYS_sendmsg,
            libc::SYS_recvmsg,
            libc::SYS_ioctl,
            libc::SYS_prctl,
            libc::SYS_memfd_create,
            libc::SYS_landlock_restrict_self,
        ] {
            assert!(!rules.contains_key(&needed), "{needed} is denied");
        }
        // `tgkill` stays allowed for the worker's own thread group only.
        assert_eq!(rules[&libc::SYS_tgkill].len(), 1);
    }

    /// GIVEN the stage 1 rules THEN the four calls that signal another
    /// process are covered: three conditional kills and `tkill` as ENOSYS.
    /// The programs compile for both architectures.
    #[test]
    fn given_stage1_rules_then_every_signal_call_is_covered() {
        let kill = stage1_kill_rules(TEST_PID).expect("rules");
        for number in [
            libc::SYS_tgkill,
            libc::SYS_rt_tgsigqueueinfo,
            libc::SYS_rt_sigqueueinfo,
        ] {
            assert_eq!(kill[&number].len(), 1, "{number} has one argument rule");
        }
        assert!(!kill.contains_key(&libc::SYS_tkill));
        assert!(stage1_errno_rules().contains_key(&libc::SYS_tkill));
        // `kill` and the pidfd call stay denied outright.
        assert!(kill[&libc::SYS_kill].is_empty());
        assert!(kill[&libc::SYS_pidfd_send_signal].is_empty());
        for pid in [1, TEST_PID, u32::MAX] {
            let rules = stage1_kill_rules(pid).expect("rules");
            for arch in [TargetArch::x86_64, TargetArch::aarch64] {
                let program =
                    compile_for(&rules, SeccompAction::KillProcess, arch).expect("compiles");
                assert!(!program.is_empty());
            }
        }
        assert!(own_pid() > 0);
    }

    /// GIVEN the stage 1 rules THEN `clone3` is not killed but fails with
    /// ENOSYS, and `clone`, `socket` and `socketpair` have argument rules.
    #[test]
    fn given_stage1_rules_then_clone3_is_enosys_and_clone_and_socket_are_conditional() {
        let kill = stage1_kill_rules(TEST_PID).expect("rules");
        assert!(!kill.contains_key(&libc::SYS_clone3));
        assert!(stage1_errno_rules().contains_key(&libc::SYS_clone3));
        assert_eq!(kill[&libc::SYS_clone].len(), CLONE_NEW_FLAGS.len());
        assert_eq!(kill[&libc::SYS_socket].len(), 1);
        assert_eq!(kill[&libc::SYS_socketpair].len(), 1);
        // The unconditional kills have no rules.
        assert!(kill[&libc::SYS_execve].is_empty());
    }

    /// GIVEN the clone flag list THEN it holds each namespace flag of the
    /// kernel ABI.
    #[test]
    fn given_clone_flags_then_each_namespace_flag_is_listed() {
        let flags = CLONE_NEW_FLAGS;
        for value in [
            0x0002_0000_u64, // NEWNS
            0x0200_0000,     // NEWCGROUP
            0x0400_0000,     // NEWUTS
            0x0800_0000,     // NEWIPC
            0x1000_0000,     // NEWUSER
            0x2000_0000,     // NEWPID
            0x4000_0000,     // NEWNET
        ] {
            assert!(flags.contains(&value), "{value:#x}");
        }
    }

    /// GIVEN the socket families THEN exactly AF_UNIX, AF_INET, AF_INET6 and
    /// AF_NETLINK are allowed.
    #[test]
    fn given_socket_families_then_only_four_are_allowed() {
        assert_eq!(ALLOWED_FAMILIES, [1, 2, 10, 16]);
    }

    /// GIVEN the stage 2 rules THEN they cover socket and socketpair for
    /// AF_UNIX only, kill the Landlock calls, and compile.
    #[test]
    fn given_stage2_rules_then_they_cover_unix_sockets_and_landlock() {
        let unix = stage2_unix_rules().expect("rules");
        assert_eq!(unix.len(), 2);
        assert!(unix.contains_key(&libc::SYS_socket));
        assert!(unix.contains_key(&libc::SYS_socketpair));
        assert!(unix.values().all(|rules| rules.len() == 1));
        let kill = stage2_kill_rules();
        for number in [
            libc::SYS_landlock_create_ruleset,
            libc::SYS_landlock_add_rule,
            libc::SYS_landlock_restrict_self,
        ] {
            assert!(kill.contains_key(&number));
        }
        for arch in [TargetArch::x86_64, TargetArch::aarch64] {
            assert!(
                !compile_for(&kill, SeccompAction::KillProcess, arch)
                    .expect("compiles")
                    .is_empty()
            );
            assert!(
                !compile_for(&unix, STAGE2_UNIX_ACTION, arch)
                    .expect("compiles")
                    .is_empty()
            );
        }
    }

    fn rule_for<'a>(table: &'a [PathRule], path: &str) -> Option<&'a PathRule> {
        table.iter().find(|rule| rule.path == Path::new(path))
    }

    /// GIVEN the portal camera mode WHEN the path table is built THEN it
    /// holds the ADR paths and no camera device.
    #[test]
    fn given_portal_mode_then_the_table_has_the_adr_paths_and_no_camera() {
        let nodes = [PathBuf::from("/dev/video0")];
        let table = path_table(
            CameraMode::Portal,
            Some(Path::new("/home/user")),
            Some(Path::new("/run/user/1000")),
            &nodes,
        );
        for dir in [
            "/usr",
            "/lib",
            "/lib64",
            "/etc",
            "/run/systemd/resolve",
            "/proc/self",
        ] {
            let rule = rule_for(&table, dir).expect(dir);
            assert_eq!(rule.access, DIR_READ_EXEC, "{dir}");
        }
        assert_eq!(
            rule_for(&table, "/home/user/.config/pulse")
                .expect("pulse")
                .access,
            DIR_READ
        );
        assert_eq!(
            rule_for(&table, "/run/user/1000/pulse")
                .expect("sound server directory")
                .access,
            DIR_READ
        );
        // Only the sound server's directory, not the rest of the runtime
        // directory (the session bus, PipeWire and other sockets live there).
        assert!(rule_for(&table, "/run/user/1000").is_none());
        assert_eq!(
            rule_for(&table, "/dev/urandom").expect("urandom").access,
            FILE_READ
        );
        assert_eq!(
            rule_for(&table, "/dev/null").expect("null").access,
            FILE_READ
        );
        let shm = rule_for(&table, "/dev/shm").expect("shm");
        assert_ne!(shm.access & FS_WRITE_FILE, 0);
        assert_eq!(shm.access & FS_EXECUTE, 0);
        assert!(rule_for(&table, "/dev/video0").is_none());
        // No writable path other than /dev/shm, and no execute right outside
        // the library and system directories.
        for rule in &table {
            if rule.path != Path::new("/dev/shm") {
                assert_eq!(rule.access & FS_WRITE_FILE, 0, "{:?}", rule.path);
            }
        }
    }

    /// GIVEN the V4L2 mode WHEN the table is built THEN each camera node gets
    /// read, write and device ioctl rights, and the none mode has no camera.
    #[test]
    fn given_v4l2_mode_then_camera_nodes_get_device_rights() {
        let nodes = [PathBuf::from("/dev/video0"), PathBuf::from("/dev/video2")];
        let table = path_table(CameraMode::V4l2, None, None, &nodes);
        for node in ["/dev/video0", "/dev/video2"] {
            assert_eq!(
                rule_for(&table, node).expect(node).access,
                FS_READ_FILE | FS_WRITE_FILE | FS_IOCTL_DEV
            );
        }
        let none = path_table(CameraMode::None, None, None, &nodes);
        assert!(rule_for(&none, "/dev/video0").is_none());
    }

    /// GIVEN a missing or relative HOME or XDG_RUNTIME_DIR THEN the sound
    /// server's cookie and socket directory rules are left out.
    #[test]
    fn given_bad_home_or_runtime_then_the_pulse_rules_are_left_out() {
        for dir in [None, Some(Path::new("relative/dir")), Some(Path::new(""))] {
            let table = path_table(CameraMode::None, dir, dir, &[]);
            assert!(
                table.iter().all(|rule| !rule.path.ends_with("pulse")),
                "{dir:?}"
            );
        }
    }

    /// GIVEN entry names WHEN checked THEN only `video<digits>` is a camera
    /// node.
    #[test]
    fn given_dev_names_then_only_video_digits_are_camera_nodes() {
        for good in ["video0", "video9", "video12", "video63"] {
            assert!(is_video_node_name(OsStr::new(good)), "{good}");
        }
        for bad in [
            "video", "video0x", "videox", "Video0", "v4l", "video-1", "video00", "video01",
            "video64", "video100", "xvideo0", "media0", "",
        ] {
            assert!(!is_video_node_name(OsStr::new(bad)), "{bad}");
        }
    }

    /// GIVEN each Landlock ABI THEN the handled rights grow with the ABI,
    /// ABI 3 is the minimum, scopes start at ABI 6 and the attribute size
    /// follows the ABI.
    #[test]
    fn given_abi_then_rights_scopes_and_size_follow_it() {
        assert!(!abi_ok(1) && !abi_ok(2) && !abi_ok(0) && !abi_ok(-1));
        assert!(abi_ok(3) && abi_ok(6) && abi_ok(9));
        assert_eq!(handled_fs(1), FS_ABI1_ALL);
        assert_eq!(handled_fs(2) & FS_REFER, FS_REFER);
        assert_eq!(handled_fs(3) & FS_TRUNCATE, FS_TRUNCATE);
        assert_eq!(handled_fs(3) & FS_IOCTL_DEV, 0);
        assert_eq!(handled_fs(4) & FS_IOCTL_DEV, 0);
        assert_eq!(handled_fs(5) & FS_IOCTL_DEV, FS_IOCTL_DEV);
        // A newer kernel handles no more than this code knows.
        assert_eq!(handled_fs(KNOWN_ABI + 5), handled_fs(KNOWN_ABI));
        assert_eq!(handled_fs(KNOWN_ABI), (1 << 16) - 1);
        assert_eq!(scoped(5), 0);
        assert_eq!(scoped(6), SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL);
        assert_eq!(
            (
                attr_size(3),
                attr_size(4),
                attr_size(5),
                attr_size(6),
                attr_size(7)
            ),
            (8, 16, 16, 24, 24)
        );
    }

    /// GIVEN the rights a path rule may hold THEN a file rule has no directory
    /// right, and every table entry is inside the handled set of ABI 5.
    #[test]
    fn given_rights_then_file_rules_hold_only_file_rights() {
        assert_eq!(
            FILE_RIGHTS & (FS_READ_DIR | FS_MAKE_REG | FS_REMOVE_FILE),
            0
        );
        let nodes = [PathBuf::from("/dev/video0")];
        for rule in path_table(
            CameraMode::V4l2,
            Some(Path::new("/h")),
            Some(Path::new("/run/user/1")),
            &nodes,
        ) {
            assert_eq!(rule.access & !handled_fs(5), 0, "{:?}", rule.path);
        }
    }

    /// GIVEN the real kernel WHEN the Landlock ABI is read THEN it is either
    /// absent or a small positive number. (The hosts differ; nothing is
    /// applied here.)
    #[test]
    fn given_this_kernel_when_abi_read_then_it_is_sane() {
        if let Some(abi) = landlock_abi() {
            assert!((1..64).contains(&abi));
        }
    }
}

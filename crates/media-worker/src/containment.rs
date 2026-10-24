//! The worker's own part of its containment, and its self-check of the part
//! the broker must apply. Each failed check sets one bit; the worker decodes
//! nothing unless the result is zero.
//!
//! Each OS has its own checks and bits. Bits 0 to 4 are Windows (unchanged
//! since ADR-0003), bits 5 and 6 are Linux (ADR-0010) and bit 7 is macOS
//! (ADR-0011). Bits 8 to 30 are free. Bit 31 means the OS has no implemented
//! containment.

/// The process token is not an AppContainer token.
pub const MISSING_APPCONTAINER: u32 = 1 << 0;
/// The process is not inside a Job object.
pub const MISSING_JOB: u32 = 1 << 1;
/// Executable memory can still be allocated (dynamic code not prohibited).
pub const MISSING_DYNAMIC_CODE_BLOCK: u32 = 1 << 2;
/// Win32k system calls are not disabled.
pub const MISSING_WIN32K_LOCKDOWN: u32 = 1 << 3;
/// A check could not be evaluated; treated as missing.
pub const CHECK_FAILED: u32 = 1 << 4;
/// Linux: the launcher did not set `no_new_privs`.
pub const MISSING_NO_NEW_PRIVS: u32 = 1 << 5;
/// Linux: the seccomp allowlist could not be installed.
pub const MISSING_SECCOMP: u32 = 1 << 6;
/// macOS: the pure-computation sandbox profile could not be applied.
pub const MISSING_SANDBOX: u32 = 1 << 7;
/// Containment is not implemented for this operating system.
pub const UNSUPPORTED_PLATFORM: u32 = 1 << 31;

/// Applies the worker's own part of its containment and checks the part its
/// launcher must apply. Returns the bits of the missing controls; zero only
/// when every one is in place. Call it before reading any input, and do not
/// read input unless it returns zero.
///
/// * Windows: checks only; the broker applies everything before the worker
///   runs.
/// * Linux: makes the process non-dumpable, checks that the launcher set
///   `no_new_privs`, and installs the seccomp allowlist for every thread.
///   After a zero result the process can read fd 0, write fds 1 and 2,
///   manage non-executable memory and exit; any other system call ends it.
/// * macOS: applies the pure-computation sandbox profile (no files, network
///   or IPC).
pub fn contain() -> u32 {
    #[cfg(windows)]
    return windows::missing_controls();
    #[cfg(target_os = "linux")]
    return linux::contain();
    #[cfg(target_os = "macos")]
    return macos::contain();
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    return UNSUPPORTED_PLATFORM;
}

/// The seccomp allowlist (ADR-0010). The set was measured by running the
/// worker's operations under the filter with the `syscall-trap` feature,
/// which reports the first call outside the list instead of being killed
/// silently.
#[cfg(target_os = "linux")]
mod linux {
    use super::{CHECK_FAILED, MISSING_NO_NEW_PRIVS, MISSING_SECCOMP};
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };
    use std::collections::BTreeMap;

    /// An unused prctl argument: passed as a full word, since the kernel
    /// refuses some options unless the unused arguments are zero.
    const NONE: libc::c_ulong = 0;

    pub(super) fn contain() -> u32 {
        // SAFETY: prctl with integer arguments only.
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, NONE, NONE, NONE, NONE) } != 0 {
            return CHECK_FAILED;
        }
        // SAFETY: as above; PR_GET_NO_NEW_PRIVS returns 0 or 1.
        match unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, NONE, NONE, NONE, NONE) } {
            1 => {}
            0 => return MISSING_NO_NEW_PRIVS,
            _ => return CHECK_FAILED,
        }
        match install() {
            Ok(()) => 0,
            Err(()) => MISSING_SECCOMP,
        }
    }

    fn install() -> Result<(), ()> {
        let program = program()?;
        #[cfg(feature = "syscall-trap")]
        trap::install()?;
        // TSYNC: every thread of the process gets the filter.
        seccompiler::apply_filter_all_threads(&program).map_err(|_| ())
    }

    /// What happens to a system call outside the allowlist.
    fn denied() -> SeccompAction {
        if cfg!(feature = "syscall-trap") {
            SeccompAction::Trap
        } else {
            SeccompAction::KillProcess
        }
    }

    /// The compiled filter for this process's architecture.
    fn program() -> Result<BpfProgram, ()> {
        let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|_| ())?;
        let filter = SeccompFilter::new(allowlist()?, denied(), SeccompAction::Allow, arch)
            .map_err(|_| ())?;
        filter.try_into().map_err(|_| ())
    }

    fn arg(index: u8, op: SeccompCmpOp, value: u64) -> Result<SeccompCondition, ()> {
        SeccompCondition::new(index, SeccompCmpArgLen::Dword, op, value).map_err(|_| ())
    }

    fn rule(conditions: Vec<SeccompCondition>) -> Result<SeccompRule, ()> {
        SeccompRule::new(conditions).map_err(|_| ())
    }

    /// Argument `index` (a protection word) has no `PROT_EXEC` bit.
    fn not_executable(index: u8) -> Result<SeccompCondition, ()> {
        arg(index, SeccompCmpOp::MaskedEq(libc::PROT_EXEC as u64), 0)
    }

    /// The allowed system calls with their argument conditions; an empty
    /// rule list allows the call with any arguments. The names are the
    /// same on x86_64 and aarch64; `libc` supplies each one's number.
    ///
    /// Measured on x86_64 (debug and release worker; PNG and JPEG decodes up
    /// to 16 megapixels, strips, guest rejections, malformed requests, and
    /// runs under an address-space limit too small for the guest): `read`,
    /// `write`, `brk`, `mmap`, `munmap` and `exit_group`. Two more are
    /// allowed without being seen: `mremap`, which glibc's `realloc` uses for
    /// a chunk it mapped, and `exit`, which ends one thread. Nothing else is
    /// needed: the worker has one thread, so locks never wait (`futex`), and
    /// a worker that faults or aborts is killed by the filter (SIGSYS)
    /// instead of by its own signal, which the broker treats the same.
    fn allowlist() -> Result<BTreeMap<i64, Vec<SeccompRule>>, ()> {
        let any = Vec::new;
        Ok(BTreeMap::from([
            // The request on stdin, the reply on stdout, a panic message on
            // stderr (which the broker points at /dev/null).
            (
                libc::SYS_read,
                vec![rule(vec![arg(0, SeccompCmpOp::Eq, 0)?])?],
            ),
            (
                libc::SYS_write,
                vec![
                    rule(vec![arg(0, SeccompCmpOp::Eq, 1)?])?,
                    rule(vec![arg(0, SeccompCmpOp::Eq, 2)?])?,
                ],
            ),
            // Heap and the guest's linear memory: anonymous and never
            // executable. Protections cannot be changed (no `mprotect`), and
            // the worker opens no files, so there is nothing to map from one.
            (libc::SYS_brk, any()),
            (
                libc::SYS_mmap,
                vec![rule(vec![
                    not_executable(2)?,
                    arg(
                        3,
                        SeccompCmpOp::MaskedEq(libc::MAP_ANONYMOUS as u64),
                        libc::MAP_ANONYMOUS as u64,
                    )?,
                ])?],
            ),
            (libc::SYS_munmap, any()),
            (libc::SYS_mremap, any()),
            (libc::SYS_exit, any()),
            (libc::SYS_exit_group, any()),
        ]))
    }

    /// Development only (`syscall-trap`): a call outside the allowlist
    /// raises SIGSYS instead of killing the process. The handler writes the
    /// call's number to stderr and exits with 159 (128 + SIGSYS), so the call
    /// is still never run.
    #[cfg(feature = "syscall-trap")]
    mod trap {
        use std::ffi::c_void;

        pub(super) fn install() -> Result<(), ()> {
            // SAFETY: a zeroed sigaction with a SA_SIGINFO handler.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = report as *const () as usize;
                action.sa_flags = libc::SA_SIGINFO;
                if libc::sigaction(libc::SIGSYS, &action, std::ptr::null_mut()) != 0 {
                    return Err(());
                }
            }
            Ok(())
        }

        extern "C" fn report(_: libc::c_int, info: *mut libc::siginfo_t, _: *mut c_void) {
            // For SIGSYS the kernel stores the call's number as an int at
            // byte 24 of siginfo on 64-bit Linux (`si_syscall`).
            // SAFETY: the kernel passes a valid siginfo for SA_SIGINFO.
            let number = unsafe { info.cast::<u8>().add(24).cast::<i32>().read() };
            let mut text = *b"media worker: system call outside the allowlist:           \n";
            let mut at = text.len() - 2;
            let mut rest = number.unsigned_abs();
            loop {
                text[at] = b'0' + (rest % 10) as u8;
                rest /= 10;
                if rest == 0 || at == 0 {
                    break;
                }
                at -= 1;
            }
            // SAFETY: write to stderr and exit are allowed by the filter.
            unsafe {
                libc::write(2, text.as_ptr().cast(), text.len());
                libc::syscall(libc::SYS_exit_group, 159);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// GIVEN the allowlist THEN no call that opens files, makes sockets,
        /// starts processes or signals other processes is in it, and it
        /// compiles for both supported architectures.
        #[test]
        fn given_allowlist_then_it_has_no_file_network_or_process_calls_and_compiles() {
            let rules = allowlist().unwrap();
            for denied in [
                libc::SYS_openat,
                libc::SYS_socket,
                libc::SYS_connect,
                libc::SYS_clone,
                libc::SYS_clone3,
                libc::SYS_execve,
                libc::SYS_kill,
                libc::SYS_tgkill,
                libc::SYS_ptrace,
                libc::SYS_prctl,
                libc::SYS_seccomp,
                libc::SYS_ioctl,
                libc::SYS_mprotect,
                libc::SYS_pkey_mprotect,
            ] {
                assert!(!rules.contains_key(&denied), "{denied} is allowed");
            }
            assert!(!program().unwrap().is_empty());
            for arch in [TargetArch::x86_64, TargetArch::aarch64] {
                let filter =
                    SeccompFilter::new(rules.clone(), denied(), SeccompAction::Allow, arch)
                        .unwrap();
                let program: BpfProgram = filter.try_into().unwrap();
                assert!(!program.is_empty());
            }
        }
    }
}

/// The pure-computation Seatbelt profile (ADR-0011). macOS code is not
/// compiled on the development machine; CI builds and tests it.
#[cfg(target_os = "macos")]
mod macos {
    use super::MISSING_SANDBOX;
    use std::{
        ffi::{c_char, c_int},
        ptr::null_mut,
    };

    /// `SANDBOX_NAMED` from `<sandbox.h>`: the profile is a built-in name.
    const SANDBOX_NAMED: u64 = 0x0001;

    #[link(name = "sandbox", kind = "dylib")]
    unsafe extern "C" {
        /// `extern const char kSBXProfilePureComputation[]`; only its
        /// address is used.
        static kSBXProfilePureComputation: c_char;
        fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
        fn sandbox_free_error(errorbuf: *mut c_char);
    }

    pub(super) fn contain() -> u32 {
        let mut error: *mut c_char = null_mut();
        // SAFETY: the profile name is the library's own NUL-terminated
        // constant; `error` receives a library-allocated message or null,
        // which is freed below.
        let status = unsafe {
            let status = sandbox_init(
                &raw const kSBXProfilePureComputation,
                SANDBOX_NAMED,
                &mut error,
            );
            if !error.is_null() {
                sandbox_free_error(error);
            }
            status
        };
        if status == 0 { 0 } else { MISSING_SANDBOX }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_DYNAMIC_CODE_BLOCKED, GetLastError, HANDLE},
        Security::{GetTokenInformation, TOKEN_QUERY, TokenIsAppContainer},
        System::{
            JobObjects::IsProcessInJob,
            Memory::{
                MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc,
                VirtualFree,
            },
            SystemServices::PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY,
            Threading::{
                GetCurrentProcess, GetProcessMitigationPolicy, OpenProcessToken,
                ProcessSystemCallDisablePolicy,
            },
        },
    };

    pub fn missing_controls() -> u32 {
        appcontainer() | job() | dynamic_code_blocked() | win32k_disabled()
    }

    fn appcontainer() -> u32 {
        let mut token: HANDLE = null_mut();
        // SAFETY: OpenProcessToken writes one handle on success.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return CHECK_FAILED;
        }
        let mut value = 0u32;
        let mut returned = 0u32;
        // SAFETY: TokenIsAppContainer returns one DWORD into `value`.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenIsAppContainer,
                (&mut value as *mut u32).cast(),
                4,
                &mut returned,
            )
        };
        // SAFETY: `token` was opened above and is closed once.
        unsafe { CloseHandle(token) };
        match (ok != 0, value) {
            (false, _) => CHECK_FAILED,
            (true, 1) => 0,
            (true, _) => MISSING_APPCONTAINER,
        }
    }

    fn job() -> u32 {
        let mut in_job = 0;
        // SAFETY: a null job handle asks about any job; one BOOL is written.
        if unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &mut in_job) } == 0 {
            return CHECK_FAILED;
        }
        if in_job != 0 { 0 } else { MISSING_JOB }
    }

    /// Asks for one page of read-write-execute memory. Under the dynamic code
    /// policy this must fail with ERROR_DYNAMIC_CODE_BLOCKED.
    fn dynamic_code_blocked() -> u32 {
        // SAFETY: a fresh anonymous allocation; freed below if it succeeds.
        let page = unsafe {
            VirtualAlloc(
                null(),
                4096,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        };
        if !page.is_null() {
            // SAFETY: `page` came from VirtualAlloc above.
            unsafe { VirtualFree(page, 0, MEM_RELEASE) };
            return MISSING_DYNAMIC_CODE_BLOCK;
        }
        // SAFETY: reads this thread's last error code.
        if unsafe { GetLastError() } == ERROR_DYNAMIC_CODE_BLOCKED {
            0
        } else {
            CHECK_FAILED
        }
    }

    fn win32k_disabled() -> u32 {
        let mut policy = PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY::default();
        // SAFETY: the buffer is the documented structure for this policy.
        let ok = unsafe {
            GetProcessMitigationPolicy(
                GetCurrentProcess(),
                ProcessSystemCallDisablePolicy,
                (&mut policy as *mut PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY).cast(),
                std::mem::size_of_val(&policy),
            )
        };
        if ok == 0 {
            return CHECK_FAILED;
        }
        // SAFETY: the union's flags view is always initialized by the call.
        let flags = unsafe { policy.Anonymous.Flags };
        // Bit 0 is DisallowWin32kSystemCalls.
        if flags & 1 != 0 {
            0
        } else {
            MISSING_WIN32K_LOCKDOWN
        }
    }
}

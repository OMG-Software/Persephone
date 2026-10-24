//! Self-check of the containment the broker must apply. Each failed check sets
//! one bit; the worker decodes nothing unless the result is zero.
//!
//! Each OS has its own checks and bits. Bits 0 to 4 are Windows (unchanged
//! since ADR-0003); bits 5 and 6 are reserved for Linux and bit 7 for macOS,
//! for the plan's "Linux" and "macOS" media-worker controls. Bits 8 to 30 are
//! free. Bit 31 means the OS has no implemented containment.

/// The process token is not an AppContainer token.
#[cfg_attr(not(windows), allow(dead_code))]
pub const MISSING_APPCONTAINER: u32 = 1 << 0;
/// The process is not inside a Job object.
#[cfg_attr(not(windows), allow(dead_code))]
pub const MISSING_JOB: u32 = 1 << 1;
/// Executable memory can still be allocated (dynamic code not prohibited).
#[cfg_attr(not(windows), allow(dead_code))]
pub const MISSING_DYNAMIC_CODE_BLOCK: u32 = 1 << 2;
/// Win32k system calls are not disabled.
#[cfg_attr(not(windows), allow(dead_code))]
pub const MISSING_WIN32K_LOCKDOWN: u32 = 1 << 3;
/// A check could not be evaluated; treated as missing.
#[cfg_attr(not(windows), allow(dead_code))]
pub const CHECK_FAILED: u32 = 1 << 4;
/// Linux, reserved: `no_new_privs` is not set (`NoNewPrivs` in
/// `/proc/self/status`). Not checked yet.
#[allow(dead_code)]
pub const MISSING_NO_NEW_PRIVS: u32 = 1 << 5;
/// Linux, reserved: the seccomp allowlist is not installed (`Seccomp: 2`).
/// Not checked yet.
#[allow(dead_code)]
pub const MISSING_SECCOMP: u32 = 1 << 6;
/// macOS, reserved: the `sandbox_init` pure-computation profile is not
/// applied. Not checked yet.
#[allow(dead_code)]
pub const MISSING_SANDBOX: u32 = 1 << 7;
/// Containment is not implemented for this operating system.
#[cfg_attr(windows, allow(dead_code))]
pub const UNSUPPORTED_PLATFORM: u32 = 1 << 31;

/// The bits of the controls missing on this OS; zero only when every one is
/// in place.
pub fn missing_controls() -> u32 {
    #[cfg(windows)]
    return windows::missing_controls();
    #[cfg(target_os = "linux")]
    return linux::missing_controls();
    #[cfg(target_os = "macos")]
    return macos::missing_controls();
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    return UNSUPPORTED_PLATFORM;
}

/// Plan "Linux", Media worker: will check `MISSING_NO_NEW_PRIVS` and
/// `MISSING_SECCOMP`. Until then nothing is decoded.
#[cfg(target_os = "linux")]
mod linux {
    use super::UNSUPPORTED_PLATFORM;

    pub fn missing_controls() -> u32 {
        UNSUPPORTED_PLATFORM
    }
}

/// Plan "macOS", Media worker: will check `MISSING_SANDBOX`. Until then
/// nothing is decoded.
#[cfg(target_os = "macos")]
mod macos {
    use super::UNSUPPORTED_PLATFORM;

    pub fn missing_controls() -> u32 {
        UNSUPPORTED_PLATFORM
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

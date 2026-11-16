//! The call worker's self-check of the containment its launcher applies
//! (ADR-0043). Each missing control sets one bit; the core refuses a call
//! unless the result is zero.
//!
//! The bits match the media worker's where the control is the same. The call
//! worker does not require the dynamic-code prohibition or the Win32k
//! lockdown, because the audio stack needs both turned off. Bit 31 means the
//! OS has no reviewed call containment, so calls are refused there.

/// The process token is not an AppContainer token.
#[cfg(windows)]
pub const MISSING_APPCONTAINER: u32 = 1 << 0;
/// The process is not inside a Job object.
#[cfg(windows)]
pub const MISSING_JOB: u32 = 1 << 1;
/// A check could not be evaluated; treated as missing.
#[cfg(windows)]
pub const CHECK_FAILED: u32 = 1 << 4;
/// Call containment is not implemented for this operating system.
#[cfg(not(windows))]
pub const UNSUPPORTED_PLATFORM: u32 = 1 << 31;

/// Returns the bits of the missing controls; zero only when all are present.
pub fn missing_controls() -> u32 {
    #[cfg(windows)]
    return windows::missing_controls();
    #[cfg(not(windows))]
    return UNSUPPORTED_PLATFORM;
}

#[cfg(windows)]
mod windows {
    use super::{CHECK_FAILED, MISSING_APPCONTAINER, MISSING_JOB};
    use std::ptr::null_mut;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{GetTokenInformation, TOKEN_QUERY, TokenIsAppContainer},
        System::{
            JobObjects::IsProcessInJob,
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };

    pub fn missing_controls() -> u32 {
        appcontainer() | job()
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
}

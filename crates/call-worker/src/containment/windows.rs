//! The Windows checks: the AppContainer token and the Job object.

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

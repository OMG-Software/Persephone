//! Windows Job containing the UI process and its web view processes.
//!
//! The web view can make the UI allocate without a bound the application can
//! enforce (docs/evidence/ipc-transport.md). The Job caps that memory so a flood
//! ends the UI instead of pressuring the whole machine, and kills the UI tree
//! when the core exits.
//!
//! The Job also shows whether the web view still runs (its processes are the
//! UI's children, so they are in the Job), and ends the whole tree when a
//! failed window is replaced.

use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::Child,
    ptr::{null, null_mut},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::HANDLE,
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject,
    },
    UI::WindowsAndMessaging::{
        IDRETRY, MB_ICONWARNING, MB_RETRYCANCEL, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
    },
};

/// Committed memory for the UI and all of its web view processes together.
pub const UI_JOB_MEMORY_LIMIT: usize = 2 * 1024 * 1024 * 1024;
/// Committed memory for any single process in the UI tree.
pub const UI_PROCESS_MEMORY_LIMIT: usize = 1024 * 1024 * 1024;

pub fn contain(child: &Child) -> io::Result<OwnedHandle> {
    // SAFETY: an anonymous Job with default security; owned below.
    let raw = unsafe { CreateJobObjectW(null(), null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` was just created and is owned only here.
    let job = unsafe { OwnedHandle::from_raw_handle(raw as _) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    limits.JobMemoryLimit = UI_JOB_MEMORY_LIMIT;
    limits.ProcessMemoryLimit = UI_PROCESS_MEMORY_LIMIT;
    // SAFETY: `limits` is the documented structure for this class.
    let ok = unsafe {
        SetInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of_val(&limits) as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both handles are valid for the duration of the call.
    if unsafe {
        AssignProcessToJobObject(
            job.as_raw_handle() as HANDLE,
            child.as_raw_handle() as HANDLE,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

/// Number of processes running in the Job: the UI and its web view.
pub fn active_processes(job: &OwnedHandle) -> io::Result<u32> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: `info` is the documented structure for this class and lives
    // for the call; the returned length is not needed.
    let ok = unsafe {
        QueryInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            size_of_val(&info) as u32,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.ActiveProcesses)
}

/// Ends every process in the Job, then waits up to `timeout` for them to
/// exit, so a new web view does not start while the old one still holds its
/// data folder. Returns whether the Job emptied in time.
pub fn terminate(job: &OwnedHandle, timeout: Duration) -> bool {
    // SAFETY: the handle is valid; exit code 1 marks a forced end.
    unsafe { TerminateJobObject(job.as_raw_handle() as HANDLE, 1) };
    let deadline = Instant::now() + timeout;
    loop {
        match active_processes(job) {
            Ok(0) => return true,
            Ok(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            _ => return false,
        }
    }
}

/// Asks, in a native dialog owned by the core, whether to start the window
/// again. Blocks until answered; `true` means Retry.
pub fn ask_to_retry() -> bool {
    let text = wide(concat!(
        "Persephone's window did not start correctly. The Windows web view it ",
        "uses sometimes fails to start.\n\n",
        "Retry opens the window again; you stay signed in. ",
        "Cancel closes Persephone.",
    ));
    let caption = wide("Persephone");
    // SAFETY: both strings are NUL-terminated and live for the call; no
    // owner window, since the UI's window is the one that failed.
    let answer = unsafe {
        MessageBoxW(
            null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_RETRYCANCEL | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
        )
    };
    answer == IDRETRY
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

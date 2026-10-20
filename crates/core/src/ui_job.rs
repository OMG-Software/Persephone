//! Windows Job containing the UI process and its web view processes.
//!
//! The web view can make the UI allocate without a bound the application can
//! enforce (docs/evidence/ipc-transport.md). The Job caps that memory so a flood
//! ends the UI instead of pressuring the whole machine, and kills the UI tree
//! when the core exits.

use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::Child,
    ptr::null,
};
use windows_sys::Win32::{
    Foundation::HANDLE,
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
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

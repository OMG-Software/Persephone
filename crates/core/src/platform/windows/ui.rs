//! The Windows UI host: a Job containing the UI process and its web view
//! processes.
//!
//! The web view can make the UI allocate without a bound the application can
//! enforce (docs/evidence/ipc-transport.md). The Job caps that memory so a flood
//! ends the UI instead of pressuring the whole machine, and kills the UI tree
//! when the core exits.
//!
//! The Job also shows whether the web view still runs (its processes are the
//! UI's children, so they are in the Job), and ends the whole tree when a
//! failed window is replaced.

use crate::ui_host::{ProcessIds, StartError, StartedUi, UiHost, UiTree, ui_command, ui_exe};
use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::Child,
    ptr::{null, null_mut},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_MORE_DATA, HANDLE},
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
        JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
        SetInformationJobObject, TerminateJobObject,
    },
    UI::WindowsAndMessaging::{
        IDRETRY, MB_ICONWARNING, MB_RETRYCANCEL, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
    },
};

/// Committed memory for the UI and all of its web view processes together.
pub const UI_JOB_MEMORY_LIMIT: usize = 2 * 1024 * 1024 * 1024;
/// Committed memory for any single process in the UI tree.
pub const UI_PROCESS_MEMORY_LIMIT: usize = 1024 * 1024 * 1024;

/// Starts the UI and places it in a [`UiJob`].
pub struct JobHost;

impl UiHost for JobHost {
    fn start(&self) -> Result<StartedUi, StartError> {
        let ui = ui_exe().map_err(|_| StartError::NoExe)?;
        // The media broker's process-start lock: the UI cannot inherit a
        // call worker's or screen helper's pipe end while one launches.
        let start = persephone_media_broker::process_start_lock();
        let spawned = ui_command(&ui).spawn();
        drop(start);
        let mut child = spawned.map_err(|error| StartError::Spawn { ui, error })?;
        // Contain the UI before it creates its web view. The Job holds the UI
        // and its web view processes; closing it on exit ends anything left.
        match contain(&child) {
            Ok(job) => Ok(StartedUi {
                child,
                tree: Box::new(UiJob(job)),
            }),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(StartError::Contain(error))
            }
        }
    }

    fn ask_to_retry(&self) -> bool {
        ask_to_retry()
    }
}

/// The Job holding one UI run. Dropping it closes the Job, and
/// KILL_ON_JOB_CLOSE ends anything left.
pub struct UiJob(OwnedHandle);

impl UiTree for UiJob {
    fn active_processes(&self) -> io::Result<u32> {
        active_processes(&self.0)
    }

    fn terminate(&self, timeout: Duration) -> bool {
        terminate(&self.0, timeout)
    }

    fn process_ids(&self) -> io::Result<ProcessIds> {
        process_ids(&self.0)
    }
}

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

/// The most process IDs read from the Job.
const MAX_PROCESS_IDS: usize = 256;

/// The documented `JOBOBJECT_BASIC_PROCESS_ID_LIST` with room for
/// [`MAX_PROCESS_IDS`] entries.
#[repr(C)]
struct ProcessIdList<const N: usize> {
    assigned: u32,
    listed: u32,
    ids: [usize; N],
}

/// The IDs of the processes running in the Job, at most [`MAX_PROCESS_IDS`],
/// and whether that is all of them.
pub fn process_ids(job: &OwnedHandle) -> io::Result<ProcessIds> {
    list_ids::<MAX_PROCESS_IDS>(job)
}

/// [`process_ids`] with room for `N` IDs.
fn list_ids<const N: usize>(job: &OwnedHandle) -> io::Result<ProcessIds> {
    let mut list = ProcessIdList::<N> {
        assigned: 0,
        listed: 0,
        ids: [0; N],
    };
    // SAFETY: `list` has the documented layout with room for its length and
    // lives for the call; the returned length is not needed.
    let ok = unsafe {
        QueryInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectBasicProcessIdList,
            (&raw mut list).cast(),
            u32::try_from(size_of::<ProcessIdList<N>>()).map_err(io::Error::other)?,
            null_mut(),
        )
    };
    // ERROR_MORE_DATA still fills the buffer with the IDs that fit.
    if ok == 0 && io::Error::last_os_error().raw_os_error() != Some(ERROR_MORE_DATA as i32) {
        return Err(io::Error::last_os_error());
    }
    let count = (list.listed as usize).min(N);
    let ids: Vec<u32> = list.ids[..count]
        .iter()
        .filter_map(|id| u32::try_from(*id).ok())
        .collect();
    // More assigned than listed: the buffer was too small (ERROR_MORE_DATA).
    let complete = ok != 0 && list.assigned as usize <= count && ids.len() == count;
    Ok(ProcessIds { ids, complete })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::windows::spawn_for_test;
    use std::process::{Command, Stdio};

    /// GIVEN a Job holding a running child process
    /// WHEN the process IDs are listed
    /// THEN the child's ID is in the list, and none is left after it ends
    #[test]
    fn given_a_job_with_a_child_then_its_pid_is_listed() {
        let mut child = spawn_for_test(
            Command::new("cmd")
                .args(["/c", "ping -n 30 127.0.0.1 >nul"])
                .stdin(Stdio::null())
                .stdout(Stdio::null()),
        )
        .expect("start child");
        let job = contain(&child).expect("contain child");
        let listed = process_ids(&job).expect("list ids");
        assert!(listed.complete);
        assert!(
            listed.ids.contains(&child.id()),
            "child missing from {:?}",
            listed.ids
        );
        assert!(terminate(&job, Duration::from_secs(10)));
        let empty = process_ids(&job).expect("list ids");
        assert!(empty.ids.is_empty() && empty.complete);
        let _ = child.wait();
    }

    /// GIVEN a Job holding cmd and its ping child
    /// WHEN the IDs are listed with room for one
    /// THEN one ID is listed and the list says it is not complete; with room
    /// for all it is complete (the control)
    #[test]
    fn given_more_processes_than_room_then_the_list_is_incomplete() {
        let mut child = spawn_for_test(
            Command::new("cmd")
                .args(["/c", "ping -n 30 127.0.0.1 >nul"])
                .stdin(Stdio::null())
                .stdout(Stdio::null()),
        )
        .expect("start child");
        let job = contain(&child).expect("contain child");
        // Wait until cmd has started ping inside the Job.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while active_processes(&job).expect("count") < 2 {
            assert!(std::time::Instant::now() < deadline, "ping did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        let short = list_ids::<1>(&job).expect("list ids");
        assert_eq!(short.ids.len(), 1);
        assert!(!short.complete);
        assert!(process_ids(&job).expect("list ids").complete);
        assert!(terminate(&job, Duration::from_secs(10)));
        let _ = child.wait();
    }
}

use crate::{
    DecodedImage, LaunchError, Mitigations, RunError, WorkerPolicy, WorkerRun, WorkerStats,
};
use persephone_media_protocol::{
    FrameError, FrameHeader, FrameKind, PixelInfo, ReceivedFrame, ResourceBudgets, WorkerFailure,
    WorkerOperation, read_frame, write_frame,
};
use std::{
    env,
    ffi::{OsStr, c_void},
    fs::File,
    os::windows::{
        ffi::OsStrExt,
        io::{FromRawHandle, OwnedHandle, RawHandle},
    },
    path::Path,
    ptr::{null, null_mut},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        GetLastError, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    },
    Security::{
        FreeSid, Isolation::CreateAppContainerProfile, Isolation::DeleteAppContainerProfile,
        Isolation::DeriveAppContainerSidFromAppContainerName, PSID, SECURITY_ATTRIBUTES,
        SECURITY_CAPABILITIES,
    },
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        Pipes::CreatePipe,
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
            InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ResumeThread,
            STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
            WaitForSingleObject,
        },
    },
};

// PROCESS_CREATION_MITIGATION_POLICY_* from winnt.h; most are not exported by
// windows-sys. Each policy is a two-bit field; 01 at its position is ALWAYS_ON.
const DEP_ENABLE: u64 = 0x1;
const HEAP_TERMINATE_ALWAYS_ON: u64 = 1 << 12;
const BOTTOM_UP_ASLR_ALWAYS_ON: u64 = 1 << 16;
const HIGH_ENTROPY_ASLR_ALWAYS_ON: u64 = 1 << 20;
const STRICT_HANDLE_CHECKS_ALWAYS_ON: u64 = 1 << 24;
const WIN32K_SYSTEM_CALL_DISABLE_ALWAYS_ON: u64 = 1 << 28;
const EXTENSION_POINT_DISABLE_ALWAYS_ON: u64 = 1 << 32;
const PROHIBIT_DYNAMIC_CODE_ALWAYS_ON: u64 = 1 << 36;
const FONT_DISABLE_ALWAYS_ON: u64 = 1 << 48;
const IMAGE_LOAD_NO_REMOTE_ALWAYS_ON: u64 = 1 << 52;
const IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON: u64 = 1 << 56;
const IMAGE_LOAD_PREFER_SYSTEM32_ALWAYS_ON: u64 = 1 << 60;
const HRESULT_ALREADY_EXISTS: i32 = 0x800700B7_u32 as i32;
const PIPE_BUFFER_BYTES: u32 = 1024 * 1024;
const EXIT_WAIT_AFTER_TERMINATE_MS: u32 = 5000;

fn mitigation_flags(mitigations: Mitigations) -> u64 {
    let mut flags = DEP_ENABLE
        | HEAP_TERMINATE_ALWAYS_ON
        | BOTTOM_UP_ASLR_ALWAYS_ON
        | HIGH_ENTROPY_ASLR_ALWAYS_ON
        | STRICT_HANDLE_CHECKS_ALWAYS_ON
        | EXTENSION_POINT_DISABLE_ALWAYS_ON
        | FONT_DISABLE_ALWAYS_ON
        | IMAGE_LOAD_NO_REMOTE_ALWAYS_ON
        | IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON
        | IMAGE_LOAD_PREFER_SYSTEM32_ALWAYS_ON;
    if mitigations.prohibit_dynamic_code {
        flags |= PROHIBIT_DYNAMIC_CODE_ALWAYS_ON;
    }
    if mitigations.disable_win32k {
        flags |= WIN32K_SYSTEM_CALL_DISABLE_ALWAYS_ON;
    }
    flags
}

fn api(call: &'static str) -> LaunchError {
    // SAFETY: reads this thread's last error code.
    LaunchError::Api {
        call,
        code: unsafe { GetLastError() },
    }
}

fn owned(handle: HANDLE) -> OwnedHandle {
    // SAFETY: called only with a handle this module just created and owns.
    unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) }
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    use std::os::windows::io::AsRawHandle;
    handle.as_raw_handle() as HANDLE
}

/// A named AppContainer profile. The worker runs under its SID with no
/// capabilities.
pub struct AppContainerProfile {
    name: Vec<u16>,
    delete_on_drop: bool,
}

impl AppContainerProfile {
    /// Creates the profile, or reuses it if it already exists.
    pub fn create_or_open(name: &str) -> Result<Self, LaunchError> {
        let wide: Vec<u16> = OsStr::new(name).encode_wide().chain(Some(0)).collect();
        let mut sid: PSID = null_mut();
        // SAFETY: all strings are NUL-terminated; no capabilities are passed.
        let hr = unsafe {
            CreateAppContainerProfile(
                wide.as_ptr(),
                wide.as_ptr(),
                wide.as_ptr(),
                null(),
                0,
                &mut sid,
            )
        };
        if hr >= 0 {
            // SAFETY: the SID was allocated by CreateAppContainerProfile.
            unsafe { FreeSid(sid) };
        } else if hr != HRESULT_ALREADY_EXISTS {
            return Err(LaunchError::Api {
                call: "CreateAppContainerProfile",
                code: hr as u32,
            });
        }
        Ok(Self {
            name: wide,
            delete_on_drop: false,
        })
    }

    /// A profile deleted again when dropped, for tests and one-off runs.
    pub fn temporary(name: &str) -> Result<Self, LaunchError> {
        let mut profile = Self::create_or_open(name)?;
        profile.delete_on_drop = true;
        Ok(profile)
    }

    fn sid(&self) -> Result<Sid, LaunchError> {
        let mut sid: PSID = null_mut();
        // SAFETY: `name` is NUL-terminated; the SID is freed by `Sid`.
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(self.name.as_ptr(), &mut sid) };
        if hr < 0 {
            return Err(LaunchError::Api {
                call: "DeriveAppContainerSidFromAppContainerName",
                code: hr as u32,
            });
        }
        Ok(Sid(sid))
    }
}

impl Drop for AppContainerProfile {
    fn drop(&mut self) {
        if self.delete_on_drop {
            // SAFETY: `name` is NUL-terminated.
            unsafe { DeleteAppContainerProfile(self.name.as_ptr()) };
        }
    }
}

struct Sid(PSID);
impl Drop for Sid {
    fn drop(&mut self) {
        // SAFETY: the SID came from DeriveAppContainerSidFromAppContainerName.
        unsafe { FreeSid(self.0) };
    }
}

struct AttributeList(Vec<usize>);
impl AttributeList {
    fn new(count: u32) -> Result<Self, LaunchError> {
        let mut bytes = 0usize;
        // SAFETY: a null list asks only for the required size.
        unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut bytes) };
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        // SAFETY: `storage` is at least `bytes` long and suitably aligned.
        if unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), count, 0, &mut bytes)
        } == 0
        {
            return Err(api("InitializeProcThreadAttributeList"));
        }
        Ok(Self(storage))
    }

    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    /// SAFETY: `value` must stay valid until the process is created.
    unsafe fn set(
        &mut self,
        attribute: usize,
        value: *const c_void,
        size: usize,
    ) -> Result<(), LaunchError> {
        // SAFETY: the list was initialized; the caller keeps `value` alive.
        if unsafe {
            UpdateProcThreadAttribute(self.as_ptr(), 0, attribute, value, size, null_mut(), null())
        } == 0
        {
            return Err(api("UpdateProcThreadAttribute"));
        }
        Ok(())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: the list was initialized in `new`.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

/// A pipe whose child end is inheritable and whose broker end is not.
fn pipe(child_reads: bool) -> Result<(OwnedHandle, OwnedHandle), LaunchError> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let (mut read, mut write): (HANDLE, HANDLE) = (null_mut(), null_mut());
    // SAFETY: both out-pointers are valid; the handles are owned below.
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, PIPE_BUFFER_BYTES) } == 0 {
        return Err(api("CreatePipe"));
    }
    let (read, write) = (owned(read), owned(write));
    let (child, broker) = if child_reads {
        (read, write)
    } else {
        (write, read)
    };
    // SAFETY: clears inheritance on the broker's own end.
    if unsafe { SetHandleInformation(raw(&broker), HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(api("SetHandleInformation"));
    }
    Ok((child, broker))
}

fn job(policy: &WorkerPolicy) -> Result<OwnedHandle, LaunchError> {
    // SAFETY: an anonymous Job with default security.
    let job = unsafe { CreateJobObjectW(null(), null()) };
    if job.is_null() {
        return Err(api("CreateJobObjectW"));
    }
    let job = owned(job);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_PROCESS_TIME;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    limits.BasicLimitInformation.PerProcessUserTimeLimit =
        i64::try_from(policy.cpu_time_limit.as_nanos() / 100).unwrap_or(i64::MAX);
    limits.ProcessMemoryLimit = policy.memory_limit_bytes;
    // SAFETY: `limits` is the documented structure for this class.
    let ok = unsafe {
        SetInformationJobObject(
            raw(&job),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of_val(&limits) as u32,
        )
    };
    if ok == 0 {
        return Err(api("SetInformationJobObject"));
    }
    Ok(job)
}

/// Runtime variables the AppContainer process needs to start; nothing else
/// from the broker's environment is passed.
fn environment_block() -> Vec<u16> {
    let mut entries: Vec<_> = ["LOCALAPPDATA", "SystemDrive", "SystemRoot", "windir"]
        .into_iter()
        .filter_map(|name| env::var_os(name).map(|value| (name, value)))
        .collect();
    entries.sort_by_key(|(name, _)| name.to_ascii_lowercase());
    let mut block = Vec::new();
    for (name, value) in entries {
        block.extend(OsStr::new(name).encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

struct Launched {
    process: OwnedHandle,
    job: OwnedHandle,
    stdin: File,
    stdout: File,
}

fn launch(
    profile: &AppContainerProfile,
    worker: &Path,
    policy: &WorkerPolicy,
) -> Result<Launched, LaunchError> {
    let sid = profile.sid()?;
    let (child_stdin, broker_stdin) = pipe(true)?;
    let (child_stdout, broker_stdout) = pipe(false)?;
    let job = job(policy)?;

    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: sid.0,
        Capabilities: null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let inherited = [raw(&child_stdin), raw(&child_stdout)];
    let mitigations = mitigation_flags(policy.mitigations);
    let mut attributes = AttributeList::new(3)?;
    // SAFETY: each value outlives CreateProcessW below.
    unsafe {
        attributes.set(
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            (&capabilities as *const SECURITY_CAPABILITIES).cast(),
            size_of_val(&capabilities),
        )?;
        attributes.set(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            inherited.as_ptr().cast(),
            size_of_val(&inherited),
        )?;
        attributes.set(
            PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY as usize,
            (&mitigations as *const u64).cast(),
            size_of_val(&mitigations),
        )?;
    }

    let executable: Vec<u16> = worker.as_os_str().encode_wide().chain(Some(0)).collect();
    // The worker takes no arguments; a path containing `"` is not valid.
    let mut command: Vec<u16> = OsStr::new("\"")
        .encode_wide()
        .chain(worker.as_os_str().encode_wide())
        .chain(OsStr::new("\"").encode_wide())
        .chain(Some(0))
        .collect();
    let directory: Vec<u16> = env::var_os("SystemRoot")
        .unwrap_or_else(|| "C:\\Windows".into())
        .encode_wide()
        .chain(Some(0))
        .collect();
    let environment = environment_block();

    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inherited[0];
    startup.StartupInfo.hStdOutput = inherited[1];
    startup.StartupInfo.hStdError = null_mut();
    startup.lpAttributeList = attributes.as_ptr();
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: all buffers are NUL-terminated and live through the call; with
    // a handle list, only the two listed handles are inherited.
    let created = unsafe {
        CreateProcessW(
            executable.as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_SUSPENDED
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            directory.as_ptr(),
            &startup.StartupInfo,
            &mut info,
        )
    };
    if created == 0 {
        return Err(api("CreateProcessW"));
    }
    let process = owned(info.hProcess);
    let thread = owned(info.hThread);
    // The worker must not run a single instruction outside the Job.
    // SAFETY: both handles are valid; on failure the suspended process is killed.
    if unsafe { AssignProcessToJobObject(raw(&job), raw(&process)) } == 0 {
        let error = api("AssignProcessToJobObject");
        unsafe { TerminateProcess(raw(&process), 1) };
        return Err(error);
    }
    drop(child_stdin);
    drop(child_stdout);
    // SAFETY: resumes the worker's primary thread.
    if unsafe { ResumeThread(raw(&thread)) } == u32::MAX {
        let error = api("ResumeThread");
        unsafe { TerminateJobObject(raw(&job), 1) };
        return Err(error);
    }
    Ok(Launched {
        process,
        job,
        stdin: File::from(broker_stdin),
        stdout: File::from(broker_stdout),
    })
}

fn job_stats(job: &OwnedHandle, process: &OwnedHandle, elapsed: Duration) -> WorkerStats {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    let mut exit_code = u32::MAX;
    // SAFETY: each buffer is the documented structure for its class.
    unsafe {
        QueryInformationJobObject(
            raw(job),
            JobObjectExtendedLimitInformation,
            (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of_val(&limits) as u32,
            null_mut(),
        );
        QueryInformationJobObject(
            raw(job),
            JobObjectBasicAccountingInformation,
            (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            size_of_val(&accounting) as u32,
            null_mut(),
        );
        GetExitCodeProcess(raw(process), &mut exit_code);
    }
    WorkerStats {
        elapsed,
        peak_process_memory: limits.PeakProcessMemoryUsed,
        user_cpu: Duration::from_nanos(u64::try_from(accounting.TotalUserTime).unwrap_or(0) * 100),
        exit_code,
    }
}

/// Launches a fresh contained worker, decodes `encoded`, and returns the
/// validated RGBA pixel frame with its EXIF orientation. The pixel frame keeps
/// its `WorkerOutput` budget reservation.
pub fn run_decode(
    profile: &AppContainerProfile,
    worker: &Path,
    encoded: &[u8],
    policy: &WorkerPolicy,
    budgets: &ResourceBudgets,
) -> Result<WorkerRun<DecodedImage>, LaunchError> {
    let (raw, stats) = run_worker(
        profile,
        worker,
        WorkerOperation::Decode,
        encoded,
        policy,
        budgets,
    )?;
    let outcome = raw.and_then(|reply| classify_decode(reply, stats.exit_code));
    Ok(WorkerRun { outcome, stats })
}

/// Launches a fresh contained worker, strips the metadata from `encoded`, and
/// returns the stripped file as an `EncodedImage` frame. The frame keeps its
/// `EncodedInFlight` budget reservation and is never longer than `encoded`.
pub fn run_strip(
    profile: &AppContainerProfile,
    worker: &Path,
    encoded: &[u8],
    policy: &WorkerPolicy,
    budgets: &ResourceBudgets,
) -> Result<WorkerRun<ReceivedFrame>, LaunchError> {
    let (raw, stats) = run_worker(
        profile,
        worker,
        WorkerOperation::Strip,
        encoded,
        policy,
        budgets,
    )?;
    let outcome = raw.and_then(|reply| classify_strip(reply, stats.exit_code, encoded.len()));
    Ok(WorkerRun { outcome, stats })
}

/// The frames read back from one worker. `second` is read only after a
/// decode's pixel frame, which must be followed by its pixel information.
struct Reply {
    first: Result<ReceivedFrame, FrameError>,
    second: Option<Result<ReceivedFrame, FrameError>>,
}

/// Launches a fresh contained worker, sends the operation and `encoded`, and
/// reads its reply. Every path out of this function leaves the worker
/// terminated: the Job is killed on deadline and closed (KILL_ON_JOB_CLOSE) on
/// return. The reply is classified by the caller after the worker has exited.
fn run_worker(
    profile: &AppContainerProfile,
    worker: &Path,
    operation: WorkerOperation,
    encoded: &[u8],
    policy: &WorkerPolicy,
    budgets: &ResourceBudgets,
) -> Result<(Result<Reply, RunError>, WorkerStats), LaunchError> {
    let request = FrameHeader {
        kind: FrameKind::EncodedImage,
        length: encoded.len() as u64,
        width: 0,
        height: 0,
    };
    if FrameHeader::decode(request.encode()).is_err() {
        return Err(LaunchError::InputTooLarge);
    }
    let Launched {
        process,
        job,
        mut stdin,
        mut stdout,
    } = launch(profile, worker, policy)?;
    let started = Instant::now();

    let (wait, written, reply) = thread::scope(|scope| {
        let writer = scope.spawn(move || {
            let written = write_frame(&mut stdin, WorkerOperation::header(), &operation.encode())
                .and_then(|()| write_frame(&mut stdin, request, encoded))
                .is_ok();
            drop(stdin);
            written
        });
        let reader = scope.spawn(move || {
            let first = read_frame(&mut stdout, budgets);
            let pixels =
                matches!(&first, Ok(frame) if frame.header().kind == FrameKind::RgbaPixels);
            let second = (operation == WorkerOperation::Decode && pixels)
                .then(|| read_frame(&mut stdout, budgets));
            Reply { first, second }
        });
        let deadline_ms = u32::try_from(policy.wall_deadline.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: waits on the worker's process handle.
        let mut wait = unsafe { WaitForSingleObject(raw(&process), deadline_ms) };
        if wait == WAIT_TIMEOUT {
            // SAFETY: terminates every process in the worker's Job.
            unsafe {
                TerminateJobObject(raw(&job), 1);
                WaitForSingleObject(raw(&process), EXIT_WAIT_AFTER_TERMINATE_MS);
            }
        } else if wait != WAIT_OBJECT_0 {
            unsafe { TerminateJobObject(raw(&job), 1) };
            wait = WAIT_TIMEOUT;
        }
        // Both threads finish once the worker's pipe ends are closed.
        (wait, writer.join().unwrap_or(false), reader.join())
    });
    let stats = job_stats(&job, &process, started.elapsed());
    // Dropping `job` closes it; KILL_ON_JOB_CLOSE ends anything left.
    drop(job);

    let outcome = if wait == WAIT_TIMEOUT {
        Err(RunError::DeadlineExceeded)
    } else {
        check_reply(reply, written, stats.exit_code)
    };
    Ok((outcome, stats))
}

/// Maps a missing or unreadable first frame to its error; the reply is kept
/// otherwise.
fn check_reply(
    reply: thread::Result<Reply>,
    written: bool,
    exit_code: u32,
) -> Result<Reply, RunError> {
    match reply {
        Ok(reply @ Reply { first: Ok(_), .. }) => Ok(reply),
        Ok(Reply {
            first: Err(FrameError::Io(_)),
            ..
        })
        | Err(_)
            if !written =>
        {
            Err(RunError::RequestWrite)
        }
        Ok(Reply {
            first: Err(FrameError::Io(_)),
            ..
        })
        | Err(_) => Err(RunError::AbnormalExit(exit_code)),
        Ok(Reply {
            first: Err(error), ..
        }) => Err(RunError::Protocol(error)),
    }
}

/// A control frame in first position must be a worker failure message.
fn failure(frame: &ReceivedFrame) -> RunError {
    match WorkerFailure::decode(frame.body()) {
        Some(failure) => RunError::Worker(failure),
        None => RunError::UnexpectedReply,
    }
}

fn classify_decode(reply: Reply, exit_code: u32) -> Result<DecodedImage, RunError> {
    let Reply { first, second } = reply;
    let pixels = first.map_err(RunError::Protocol)?;
    match pixels.header().kind {
        FrameKind::RgbaPixels if exit_code == 0 => {
            let info = match second {
                Some(Ok(info)) if info.header().kind == FrameKind::Control => {
                    PixelInfo::decode(info.body()).ok_or(RunError::UnexpectedReply)?
                }
                Some(Err(FrameError::Io(_))) | None | Some(Ok(_)) => {
                    return Err(RunError::UnexpectedReply);
                }
                Some(Err(error)) => return Err(RunError::Protocol(error)),
            };
            Ok(DecodedImage {
                pixels,
                orientation: info.orientation(),
            })
        }
        FrameKind::Control => Err(failure(&pixels)),
        _ => Err(RunError::UnexpectedReply),
    }
}

fn classify_strip(
    reply: Reply,
    exit_code: u32,
    input_len: usize,
) -> Result<ReceivedFrame, RunError> {
    let stripped = reply.first.map_err(RunError::Protocol)?;
    match stripped.header().kind {
        FrameKind::EncodedImage if exit_code == 0 && stripped.body().len() <= input_len => {
            Ok(stripped)
        }
        FrameKind::Control => Err(failure(&stripped)),
        _ => Err(RunError::UnexpectedReply),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use persephone_media_protocol::BudgetClass;
    use std::io::Cursor;

    fn frame(budgets: &ResourceBudgets, header: FrameHeader, body: &[u8]) -> ReceivedFrame {
        let mut wire = Vec::new();
        write_frame(&mut wire, header, body).unwrap();
        read_frame(&mut Cursor::new(wire), budgets).unwrap()
    }

    fn pixels(budgets: &ResourceBudgets) -> ReceivedFrame {
        let header = FrameHeader {
            kind: FrameKind::RgbaPixels,
            length: 8,
            width: 1,
            height: 2,
        };
        frame(budgets, header, &[9; 8])
    }

    fn encoded(budgets: &ResourceBudgets, length: usize) -> ReceivedFrame {
        let header = FrameHeader {
            kind: FrameKind::EncodedImage,
            length: length as u64,
            width: 0,
            height: 0,
        };
        frame(budgets, header, &vec![1; length])
    }

    fn control(budgets: &ResourceBudgets, header: FrameHeader, body: &[u8]) -> ReceivedFrame {
        frame(budgets, header, body)
    }

    fn reply(first: ReceivedFrame, second: Option<Result<ReceivedFrame, FrameError>>) -> Reply {
        Reply {
            first: Ok(first),
            second,
        }
    }

    #[test]
    fn given_pixels_and_pixel_info_then_decode_reply_carries_orientation_and_reservation() {
        let budgets = ResourceBudgets::default();
        let info = control(
            &budgets,
            PixelInfo::header(),
            &PixelInfo::new(6).unwrap().encode(),
        );
        let decoded = classify_decode(reply(pixels(&budgets), Some(Ok(info))), 0).unwrap();
        assert_eq!(decoded.orientation, 6);
        assert_eq!(decoded.pixels.body(), &[9; 8]);
        assert_eq!(budgets.class_used(BudgetClass::WorkerOutput), 8);
        // The pixel information frame is released once read.
        assert_eq!(budgets.class_used(BudgetClass::ControlQueue), 0);
        drop(decoded);
        assert_eq!(budgets.aggregate_used(), 0);
    }

    #[test]
    fn given_pixels_without_valid_pixel_info_then_decode_reply_is_refused() {
        let budgets = ResourceBudgets::default();
        let bad_info = [
            None,
            Some(Err(FrameError::Io(std::io::ErrorKind::UnexpectedEof))),
            Some(Ok(control(&budgets, PixelInfo::header(), &[1, 9]))),
            Some(Ok(control(
                &budgets,
                WorkerFailure::header(),
                &WorkerFailure::Trap.encode(),
            ))),
            Some(Ok(pixels(&budgets))),
        ];
        for second in bad_info {
            assert!(matches!(
                classify_decode(reply(pixels(&budgets), second), 0),
                Err(RunError::UnexpectedReply)
            ));
        }
        let info = control(
            &budgets,
            PixelInfo::header(),
            &PixelInfo::new(1).unwrap().encode(),
        );
        assert!(matches!(
            classify_decode(reply(pixels(&budgets), Some(Ok(info))), 1),
            Err(RunError::UnexpectedReply)
        ));
        assert_eq!(budgets.aggregate_used(), 0);
    }

    #[test]
    fn given_failure_or_wrong_kind_then_each_operation_reports_it() {
        let budgets = ResourceBudgets::default();
        let failed = || {
            control(
                &budgets,
                WorkerFailure::header(),
                &WorkerFailure::GuestRejected(8).encode(),
            )
        };
        assert!(matches!(
            classify_decode(reply(failed(), None), 1),
            Err(RunError::Worker(WorkerFailure::GuestRejected(8)))
        ));
        assert!(matches!(
            classify_strip(reply(failed(), None), 1, 100),
            Err(RunError::Worker(WorkerFailure::GuestRejected(8)))
        ));
        assert!(matches!(
            classify_decode(reply(encoded(&budgets, 4), None), 0),
            Err(RunError::UnexpectedReply)
        ));
        assert!(matches!(
            classify_strip(reply(pixels(&budgets), None), 0, 100),
            Err(RunError::UnexpectedReply)
        ));
        let unknown = control(&budgets, PixelInfo::header(), &[1, 1]);
        assert!(matches!(
            classify_strip(reply(unknown, None), 0, 100),
            Err(RunError::UnexpectedReply)
        ));
    }

    #[test]
    fn given_stripped_reply_then_it_must_not_be_longer_than_input_or_follow_a_failed_exit() {
        let budgets = ResourceBudgets::default();
        let stripped = classify_strip(reply(encoded(&budgets, 10), None), 0, 10).unwrap();
        assert_eq!(stripped.body().len(), 10);
        assert_eq!(budgets.class_used(BudgetClass::EncodedInFlight), 10);
        drop(stripped);
        assert!(matches!(
            classify_strip(reply(encoded(&budgets, 11), None), 0, 10),
            Err(RunError::UnexpectedReply)
        ));
        assert!(matches!(
            classify_strip(reply(encoded(&budgets, 4), None), 1, 10),
            Err(RunError::UnexpectedReply)
        ));
        assert_eq!(budgets.aggregate_used(), 0);
    }

    #[test]
    fn given_unreadable_first_frame_then_request_write_exit_and_protocol_errors_stay_distinct() {
        let io = || Reply {
            first: Err(FrameError::Io(std::io::ErrorKind::UnexpectedEof)),
            second: None,
        };
        assert!(matches!(
            check_reply(Ok(io()), false, 0),
            Err(RunError::RequestWrite)
        ));
        assert!(matches!(
            check_reply(Ok(io()), true, 5),
            Err(RunError::AbnormalExit(5))
        ));
        let protocol = Reply {
            first: Err(FrameError::TooLarge),
            second: None,
        };
        assert!(matches!(
            check_reply(Ok(protocol), true, 0),
            Err(RunError::Protocol(FrameError::TooLarge))
        ));
    }

    #[test]
    fn given_default_mitigations_then_flags_set_every_required_policy_bit() {
        let flags = mitigation_flags(Mitigations::default());
        for bit in [
            PROHIBIT_DYNAMIC_CODE_ALWAYS_ON,
            WIN32K_SYSTEM_CALL_DISABLE_ALWAYS_ON,
            STRICT_HANDLE_CHECKS_ALWAYS_ON,
            IMAGE_LOAD_NO_REMOTE_ALWAYS_ON,
            IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON,
            EXTENSION_POINT_DISABLE_ALWAYS_ON,
        ] {
            assert_eq!(flags & bit, bit);
            // ALWAYS_OFF is the next bit up; it must never be set.
            assert_eq!(flags & (bit << 1), 0);
        }
        let relaxed = mitigation_flags(Mitigations {
            prohibit_dynamic_code: false,
            disable_win32k: false,
        });
        assert_eq!(
            relaxed & (PROHIBIT_DYNAMIC_CODE_ALWAYS_ON | WIN32K_SYSTEM_CALL_DISABLE_ALWAYS_ON),
            0
        );
    }

    #[test]
    fn given_environment_block_then_only_runtime_variables_are_passed() {
        let block = String::from_utf16(&environment_block()).unwrap();
        for entry in block.split('\0').filter(|entry| !entry.is_empty()) {
            let name = entry.split('=').next().unwrap();
            assert!(
                ["LOCALAPPDATA", "SystemDrive", "SystemRoot", "windir"].contains(&name),
                "unexpected variable {name}"
            );
        }
    }
}

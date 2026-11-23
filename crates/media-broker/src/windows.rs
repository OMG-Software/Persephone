//! The Windows launcher: the worker starts suspended in a zero-capability
//! AppContainer with process mitigations set at creation, inherits only its
//! two pipe handles, and is placed in a Job before it runs. The deadline is
//! enforced by terminating the Job.
//!
//! The call worker (ADR-0043) uses the same launcher with a closed set of two
//! capabilities, relaxed mitigations where audio needs them, and a Job without
//! a time limit, because a call is long-lived.

use crate::{
    LaunchError, Mitigations, WorkerPolicy, WorkerStats,
    launch::{Launcher, Readiness, Spawned, Wait, Worker},
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
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{
        GetLastError, HANDLE, HANDLE_FLAG_INHERIT, LocalFree, STILL_ACTIVE, SetHandleInformation,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        DeriveCapabilitySidsFromName, FreeSid, Isolation::CreateAppContainerProfile,
        Isolation::DeleteAppContainerProfile, Isolation::DeriveAppContainerSidFromAppContainerName,
        PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
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
// SE_GROUP_ENABLED from winnt.h, as the unsigned attribute value.
const SE_GROUP_ENABLED: u32 = 0x4;

/// The only capabilities a call worker gets: network access for LiveKit and
/// the microphone and the camera (`webcam`, ADR-0048). A closed set; callers
/// cannot add names.
#[cfg(not(feature = "dev-private-network"))]
const CALL_CAPABILITIES: &[&str] = &["internetClient", "microphone", "webcam"];
/// Development builds for the live call test only: also private networks, so
/// the worker reaches a disposable SFU on the WSL address. Never shipped.
#[cfg(feature = "dev-private-network")]
const CALL_CAPABILITIES: &[&str] = &[
    "internetClient",
    "microphone",
    "webcam",
    "privateNetworkClientServer",
];
/// Committed memory for the call worker process.
const CALL_MEMORY_LIMIT_BYTES: usize = 512 * 1024 * 1024;

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

/// What differs between workers that share this launcher.
struct LaunchSpec {
    /// Capability names; empty for the media worker.
    capabilities: &'static [&'static str],
    mitigations: Mitigations,
    memory_limit_bytes: usize,
    /// User-mode CPU limit; `None` for a long-lived worker.
    cpu_time_limit: Option<Duration>,
}

impl LaunchSpec {
    /// The media worker: zero capabilities and the policy's limits.
    fn media(policy: &WorkerPolicy) -> Self {
        Self {
            capabilities: &[],
            mitigations: policy.mitigations,
            memory_limit_bytes: policy.memory_limit_bytes,
            cpu_time_limit: Some(policy.cpu_time_limit),
        }
    }

    /// The call worker: two capabilities, no dynamic-code or Win32k
    /// mitigation (the audio stack needs both), no CPU limit.
    fn call() -> Self {
        Self {
            capabilities: CALL_CAPABILITIES,
            mitigations: Mitigations {
                prohibit_dynamic_code: false,
                disable_win32k: false,
            },
            memory_limit_bytes: CALL_MEMORY_LIMIT_BYTES,
            cpu_time_limit: None,
        }
    }
}

/// Capability SIDs derived from names, freed on drop.
struct CapabilitySids {
    sids: Vec<PSID>,
}

impl CapabilitySids {
    fn derive(names: &[&str]) -> Result<Self, LaunchError> {
        // Built first, so the SIDs derived so far are freed on an error.
        let mut result = Self { sids: Vec::new() };
        for name in names {
            let wide: Vec<u16> = OsStr::new(name).encode_wide().chain(Some(0)).collect();
            let (mut group_sids, mut group_count): (*mut PSID, u32) = (null_mut(), 0);
            let (mut capability_sids, mut capability_count): (*mut PSID, u32) = (null_mut(), 0);
            // SAFETY: the name is NUL-terminated; all four out-pointers are valid.
            let ok = unsafe {
                DeriveCapabilitySidsFromName(
                    wide.as_ptr(),
                    &mut group_sids,
                    &mut group_count,
                    &mut capability_sids,
                    &mut capability_count,
                )
            };
            if ok == 0 {
                return Err(api("DeriveCapabilitySidsFromName"));
            }
            // SAFETY: on success each array holds that many SIDs, and the
            // arrays and the SIDs were allocated with LocalAlloc.
            unsafe {
                for index in 0..group_count as usize {
                    LocalFree((*group_sids.add(index)).cast());
                }
                LocalFree(group_sids.cast());
                let derived =
                    std::slice::from_raw_parts(capability_sids, capability_count as usize);
                // The capability SID, not the group SID, goes in the token.
                result.sids.extend_from_slice(derived);
                LocalFree(capability_sids.cast());
            }
            if capability_count != 1 {
                // A name that maps to no single capability is refused.
                return Err(LaunchError::Api {
                    call: "DeriveCapabilitySidsFromName",
                    code: capability_count,
                });
            }
        }
        Ok(result)
    }

    fn attributes(&self) -> Vec<SID_AND_ATTRIBUTES> {
        self.sids
            .iter()
            .map(|&sid| SID_AND_ATTRIBUTES {
                Sid: sid,
                Attributes: SE_GROUP_ENABLED,
            })
            .collect()
    }
}

impl Drop for CapabilitySids {
    fn drop(&mut self) {
        for &sid in &self.sids {
            // SAFETY: each SID was allocated by DeriveCapabilitySidsFromName.
            unsafe { LocalFree(sid.cast()) };
        }
    }
}

fn job(spec: &LaunchSpec) -> Result<OwnedHandle, LaunchError> {
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
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    if let Some(cpu_time_limit) = spec.cpu_time_limit {
        limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_TIME;
        limits.BasicLimitInformation.PerProcessUserTimeLimit =
            i64::try_from(cpu_time_limit.as_nanos() / 100).unwrap_or(i64::MAX);
    }
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    limits.ProcessMemoryLimit = spec.memory_limit_bytes;
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

/// A worker process in its Job. Dropping it closes the Job, and
/// KILL_ON_JOB_CLOSE ends anything left.
pub(crate) struct JobWorker {
    // Declared first, so the Job closes before the process handle, as the
    // run did before the launcher split.
    job: OwnedHandle,
    process: OwnedHandle,
}

impl Launcher for AppContainerProfile {
    type Worker = JobWorker;

    fn spawn(
        &self,
        worker: &Path,
        policy: &WorkerPolicy,
    ) -> Result<Spawned<JobWorker>, LaunchError> {
        let started = launch(self, worker, &LaunchSpec::media(policy))?;
        Ok(Spawned {
            worker: started.worker,
            stdin: started.stdin,
            stdout: started.stdout,
        })
    }
}

impl Worker for JobWorker {
    const READINESS: Readiness = Readiness::BeforeStart;

    fn wait(&self, timeout: Duration) -> Wait {
        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: waits on the worker's process handle.
        match unsafe { WaitForSingleObject(raw(&self.process), timeout_ms) } {
            WAIT_OBJECT_0 => Wait::Exited,
            WAIT_TIMEOUT => Wait::TimedOut,
            _ => Wait::Failed,
        }
    }

    fn kill(&self) {
        // SAFETY: terminates every process in the worker's Job.
        unsafe { TerminateJobObject(raw(&self.job), 1) };
    }

    /// The AppContainer, mitigations and Job are in place before the worker
    /// runs its first instruction.
    fn confirm_contained(&self) -> bool {
        true
    }

    /// Windows has no signals; the Job's limits end a worker with an exit
    /// code.
    fn signalled(&self) -> bool {
        false
    }

    fn stats(&self, elapsed: Duration) -> WorkerStats {
        job_stats(&self.job, &self.process, elapsed)
    }
}

/// A call worker in its Job. It has no deadline: it runs until it exits or is
/// killed. Dropping it closes the Job, and KILL_ON_JOB_CLOSE ends the process.
pub struct LongLivedWorker(JobWorker);

impl LongLivedWorker {
    /// Ends the worker now by terminating its Job.
    pub fn kill(&self) {
        self.0.kill();
    }

    /// Waits up to `timeout` for the process to exit. Returns whether it did.
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        self.0.wait(timeout) == Wait::Exited
    }

    /// How the process ended. `None` while it runs or when the OS cannot say.
    pub fn exit_status(&self) -> Option<crate::WorkerExit> {
        let mut code = 0u32;
        // SAFETY: reads the exit code of the worker's own process handle.
        let ok = unsafe { GetExitCodeProcess(raw(&self.0.process), &mut code) };
        (ok != 0 && code != STILL_ACTIVE as u32).then_some(crate::WorkerExit::Code(code))
    }
}

/// A started call worker: its handle and the broker's ends of its pipes.
pub struct StartedCallWorker {
    pub worker: LongLivedWorker,
    /// Writes to the worker's standard input.
    pub stdin: File,
    /// Reads from the worker's standard output.
    pub stdout: File,
}

/// Starts the call worker in `profile` with the call containment: the
/// `internetClient` and `microphone` capabilities only, the media worker's
/// mitigations except dynamic-code prohibition and Win32k disable, and a Job
/// with kill-on-close and a memory limit. The worker is in the Job before it
/// runs.
pub fn start_call_worker(
    profile: &AppContainerProfile,
    exe: &Path,
) -> Result<StartedCallWorker, LaunchError> {
    let started = launch(profile, exe, &LaunchSpec::call())?;
    Ok(StartedCallWorker {
        worker: LongLivedWorker(started.worker),
        stdin: started.stdin,
        stdout: started.stdout,
    })
}

struct Started {
    worker: JobWorker,
    stdin: File,
    stdout: File,
}

fn launch(
    profile: &AppContainerProfile,
    worker: &Path,
    spec: &LaunchSpec,
) -> Result<Started, LaunchError> {
    let sid = profile.sid()?;
    let (child_stdin, broker_stdin) = pipe(true)?;
    let (child_stdout, broker_stdout) = pipe(false)?;
    let job = job(spec)?;

    let capability_sids = CapabilitySids::derive(spec.capabilities)?;
    let mut capability_list = capability_sids.attributes();
    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: sid.0,
        Capabilities: if capability_list.is_empty() {
            null_mut()
        } else {
            capability_list.as_mut_ptr()
        },
        CapabilityCount: capability_list.len() as u32,
        Reserved: 0,
    };
    let inherited = [raw(&child_stdin), raw(&child_stdout)];
    let mitigations = mitigation_flags(spec.mitigations);
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
    Ok(Started {
        worker: JobWorker { process, job },
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// GIVEN the call worker's launch spec in a build without the dev feature
    /// WHEN read THEN it has exactly the three capabilities, no CPU limit, and
    /// the 512 MiB memory limit.
    #[cfg(not(feature = "dev-private-network"))]
    #[test]
    fn given_call_spec_then_capabilities_are_exactly_network_microphone_and_camera() {
        let spec = LaunchSpec::call();
        assert_eq!(
            spec.capabilities,
            ["internetClient", "microphone", "webcam"]
        );
        assert_eq!(spec.cpu_time_limit, None);
        assert_eq!(spec.memory_limit_bytes, 512 * 1024 * 1024);
    }

    /// GIVEN the dev-private-network feature WHEN the call spec is read THEN
    /// private networks are the only addition to the release capabilities.
    #[cfg(feature = "dev-private-network")]
    #[test]
    fn given_dev_private_network_then_only_private_networks_are_added() {
        let spec = LaunchSpec::call();
        assert_eq!(
            spec.capabilities,
            [
                "internetClient",
                "microphone",
                "webcam",
                "privateNetworkClientServer"
            ]
        );
    }

    /// GIVEN the call worker's mitigations WHEN flags are built THEN every
    /// bit of the media worker's set stays except dynamic-code prohibition
    /// and Win32k disable.
    #[test]
    fn given_call_spec_then_mitigations_keep_every_bit_but_two() {
        let call = mitigation_flags(LaunchSpec::call().mitigations);
        let media = mitigation_flags(Mitigations::default());
        let dropped = PROHIBIT_DYNAMIC_CODE_ALWAYS_ON | WIN32K_SYSTEM_CALL_DISABLE_ALWAYS_ON;
        assert_eq!(media & dropped, dropped);
        assert_eq!(call, media & !dropped);
        assert_ne!(call, 0);
    }

    /// GIVEN the media worker's policy WHEN its launch spec is built THEN it
    /// has zero capabilities, the policy's mitigations and limits.
    #[test]
    fn given_media_policy_then_launch_spec_is_unchanged() {
        let policy = WorkerPolicy::default();
        let spec = LaunchSpec::media(&policy);
        assert!(spec.capabilities.is_empty());
        assert_eq!(
            mitigation_flags(spec.mitigations),
            mitigation_flags(Mitigations::default())
        );
        assert_eq!(spec.memory_limit_bytes, policy.memory_limit_bytes);
        assert_eq!(spec.cpu_time_limit, Some(policy.cpu_time_limit));
    }

    /// GIVEN the call capability names WHEN the OS derives their SIDs THEN
    /// each yields one capability SID and the SIDs differ.
    #[test]
    fn given_call_capabilities_then_the_os_derives_distinct_sids() {
        let sids = CapabilitySids::derive(CALL_CAPABILITIES).unwrap();
        assert_eq!(sids.sids.len(), CALL_CAPABILITIES.len());
        for (i, sid) in sids.sids.iter().enumerate() {
            assert!(sids.sids[i + 1..].iter().all(|other| other != sid));
        }
        assert!(sids.attributes().iter().all(|a| a.Attributes == 4));
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

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
    sync::{Mutex, MutexGuard, PoisonError},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT,
        LocalFree, STILL_ACTIVE, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        AllocateAndInitializeSid, DeriveCapabilitySidsFromName, FreeSid,
        Isolation::CreateAppContainerProfile, Isolation::DeleteAppContainerProfile,
        Isolation::DeriveAppContainerSidFromAppContainerName, PSID, SECURITY_APP_PACKAGE_AUTHORITY,
        SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
    },
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
            JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS,
            JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_GLOBALATOMS,
            JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
            JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_UI_RESTRICTIONS,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
            JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        Pipes::CreatePipe,
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DETACHED_PROCESS, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
            GetCurrentProcess, GetExitCodeProcess, InitializeProcThreadAttributeList,
            LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY,
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
// SECURITY_CAPABILITY_BASE_RID from winnt.h: the first RID of a capability
// SID (`S-1-15-3-...`).
const SECURITY_CAPABILITY_BASE_RID: u32 = 3;

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
/// The device capabilities a call worker gets, as the four RIDs of each
/// device class GUID. A packaged app's token carries a device capability as
/// the SID `S-1-15-3-<GUID as four RIDs>`, and the camera device interfaces
/// grant access to that SID. The SID that `DeriveCapabilitySidsFromName`
/// gives for `webcam` is not enough: with it alone, Media Foundation lists
/// the camera inside the container but `ActivateObject` fails with
/// `E_ACCESSDENIED` (ADR-0048; `docs/evidence/camera-windows.md`). Only
/// `webcam`. A closed set; callers cannot add GUIDs.
const CALL_DEVICE_CAPABILITIES: &[[u32; 4]] = &[WEBCAM_DEVICE_CAPABILITY];
/// `webcam`: the device class `{E5323777-F976-4F5B-9B55-B94699C46E44}`.
const WEBCAM_DEVICE_CAPABILITY: [u32; 4] = guid_rids(
    0xE532_3777,
    0xF976,
    0x4F5B,
    [0x9B, 0x55, 0xB9, 0x46, 0x99, 0xC4, 0x6E, 0x44],
);

/// The four RIDs of a GUID in a device capability SID: the GUID's 16 bytes
/// in memory order, read as four little-endian numbers.
const fn guid_rids(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> [u32; 4] {
    [
        data1,
        ((data3 as u32) << 16) | data2 as u32,
        u32::from_le_bytes([data4[0], data4[1], data4[2], data4[3]]),
        u32::from_le_bytes([data4[4], data4[5], data4[6], data4[7]]),
    ]
}

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

/// Every process start of this process holds this lock: the broker's
/// launches and the core's own `std::process::Command` starts. A handle is
/// inheritable only while a start holds the lock, and it is closed before
/// the lock is released, so no other start can inherit it. Windows lets a
/// start without a handle list inherit every inheritable handle, and the
/// standard library starts with no handle list.
static PROCESS_START: Mutex<()> = Mutex::new(());

/// Takes the process-start lock. Hold it across a whole process start
/// (`Command::spawn`, or a launch here) and take it before nothing else.
pub fn process_start_lock() -> MutexGuard<'static, ()> {
    PROCESS_START.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Makes `handles` inheritable for the process start that holds `_start`.
/// The caller closes them before it releases the lock.
fn inheritable_for(
    _start: &MutexGuard<'static, ()>,
    handles: &[HANDLE],
) -> Result<(), LaunchError> {
    for &handle in handles {
        // SAFETY: each handle is a valid handle of this process.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(api("SetHandleInformation"));
        }
    }
    Ok(())
}

/// A pipe with no inheritable end. A launch makes the child's end
/// inheritable with [`inheritable_for`] under the process-start lock.
fn pipe(child_reads: bool) -> Result<(OwnedHandle, OwnedHandle), LaunchError> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 0,
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
    Ok((child, broker))
}

/// What differs between workers that share this launcher.
struct LaunchSpec {
    /// Capability names; empty for the media worker.
    capabilities: &'static [&'static str],
    /// Device capability GUIDs as RIDs; empty for the media worker.
    device_capabilities: &'static [[u32; 4]],
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
            device_capabilities: &[],
            mitigations: policy.mitigations,
            memory_limit_bytes: policy.memory_limit_bytes,
            cpu_time_limit: Some(policy.cpu_time_limit),
        }
    }

    /// The call worker: the call capabilities and the camera's device
    /// capability, no dynamic-code or Win32k mitigation (the audio stack
    /// needs both), no CPU limit.
    fn call() -> Self {
        Self {
            capabilities: CALL_CAPABILITIES,
            device_capabilities: CALL_DEVICE_CAPABILITIES,
            mitigations: Mitigations {
                prohibit_dynamic_code: false,
                disable_win32k: false,
            },
            memory_limit_bytes: CALL_MEMORY_LIMIT_BYTES,
            cpu_time_limit: None,
        }
    }
}

/// Capability SIDs derived from names and device capability GUIDs, freed on
/// drop.
struct CapabilitySids {
    /// From `DeriveCapabilitySidsFromName` (LocalAlloc).
    sids: Vec<PSID>,
    /// From `AllocateAndInitializeSid` (FreeSid).
    device: Vec<PSID>,
}

impl CapabilitySids {
    fn derive(names: &[&str], devices: &[[u32; 4]]) -> Result<Self, LaunchError> {
        // Built first, so the SIDs derived so far are freed on an error.
        let mut result = Self {
            sids: Vec::new(),
            device: Vec::new(),
        };
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
        for rids in devices {
            let mut sid: PSID = null_mut();
            // SAFETY: the authority outlives the call and `sid` is a valid out
            // pointer. Five RIDs are used: the capability base RID and the
            // GUID's four.
            let ok = unsafe {
                AllocateAndInitializeSid(
                    &SECURITY_APP_PACKAGE_AUTHORITY,
                    5,
                    SECURITY_CAPABILITY_BASE_RID,
                    rids[0],
                    rids[1],
                    rids[2],
                    rids[3],
                    0,
                    0,
                    0,
                    &mut sid,
                )
            };
            if ok == 0 {
                return Err(api("AllocateAndInitializeSid"));
            }
            result.device.push(sid);
        }
        Ok(result)
    }

    fn attributes(&self) -> Vec<SID_AND_ATTRIBUTES> {
        self.sids
            .iter()
            .chain(&self.device)
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
        for &sid in &self.device {
            // SAFETY: each SID was allocated by AllocateAndInitializeSid.
            unsafe { FreeSid(sid) };
        }
    }
}

fn job(spec: &LaunchSpec) -> Result<OwnedHandle, LaunchError> {
    job_with(spec.memory_limit_bytes, spec.cpu_time_limit, false)
}

/// A Job that holds one process, kills it when the Job closes, and caps its
/// committed memory and, when asked, its CPU time. With `ui_limits` the
/// process also cannot use the clipboard, change system parameters or
/// display settings, use global atoms, switch or create desktops, log the
/// user off, or touch the USER handles of processes outside the Job.
fn job_with(
    memory_limit_bytes: usize,
    cpu_time_limit: Option<Duration>,
    ui_limits: bool,
) -> Result<OwnedHandle, LaunchError> {
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
    if let Some(cpu_time_limit) = cpu_time_limit {
        limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_TIME;
        limits.BasicLimitInformation.PerProcessUserTimeLimit =
            i64::try_from(cpu_time_limit.as_nanos() / 100).unwrap_or(i64::MAX);
    }
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    limits.ProcessMemoryLimit = memory_limit_bytes;
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
    if ui_limits {
        let restrictions = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: HELPER_UI_LIMITS,
        };
        // SAFETY: `restrictions` is the documented structure for this class.
        let ok = unsafe {
            SetInformationJobObject(
                raw(&job),
                JobObjectBasicUIRestrictions,
                (&restrictions as *const JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
                size_of_val(&restrictions) as u32,
            )
        };
        if ok == 0 {
            return Err(api("SetInformationJobObject"));
        }
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
/// `internetClient`, `microphone` and `webcam` capabilities only (`webcam`
/// also as its device capability SID), the media worker's
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
    // Taken first, so it is released last: the child's pipe ends close
    // before any other start can run (`PROCESS_START`).
    let start = process_start_lock();
    let sid = profile.sid()?;
    let (child_stdin, broker_stdin) = pipe(true)?;
    let (child_stdout, broker_stdout) = pipe(false)?;
    inheritable_for(&start, &[raw(&child_stdin), raw(&child_stdout)])?;
    let job = job(spec)?;

    let capability_sids = CapabilitySids::derive(spec.capabilities, spec.device_capabilities)?;
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

// ---------------------------------------------------------------------------
// The screen helper (ADR-0067)
// ---------------------------------------------------------------------------

/// Committed memory for the screen helper: two 3840x2160 BGRA pictures (the
/// staging copy and the repacked copy, about 64 MiB) and the Direct3D and
/// WinRT runtime. The peak measured on 25 November 2026 was 87 MiB for one
/// monitor on the development machine; the limit leaves room for a 4K
/// monitor and for a graphics driver that commits more.
const SCREEN_HELPER_MEMORY_LIMIT_BYTES: usize = 512 * 1024 * 1024;

/// What the screen helper's Job forbids the process to do with the desktop.
/// None of it is needed for Windows.Graphics.Capture of one monitor.
const HELPER_UI_LIMITS: u32 = JOB_OBJECT_UILIMIT_HANDLES
    | JOB_OBJECT_UILIMIT_READCLIPBOARD
    | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
    | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
    | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
    | JOB_OBJECT_UILIMIT_GLOBALATOMS
    | JOB_OBJECT_UILIMIT_DESKTOP
    | JOB_OBJECT_UILIMIT_EXITWINDOWS;

/// `PROCESS_CREATION_CHILD_PROCESS_RESTRICTED` from winnt.h.
const CHILD_PROCESS_RESTRICTED: u32 = 0x1;

/// The creation-time mitigations of the screen helper: the same flags as the
/// call worker's. Like the call worker, the helper has no dynamic-code
/// prohibition (the graphics driver generates code in the process) and no
/// Win32k lockdown (WinRT capture and Direct3D need Win32k system calls;
/// the call worker's audio stack needs them too).
fn helper_mitigation_flags() -> u64 {
    mitigation_flags(Mitigations {
        prohibit_dynamic_code: false,
        disable_win32k: false,
    })
}

/// The two pipes of one screen share, made before the helper starts. The
/// helper gets the control pipe's read end and the frame pipe's write end;
/// the core keeps the other two. No end is inheritable: the launch makes the
/// helper's two ends inheritable under the process-start lock and closes
/// them before it releases the lock.
pub struct ScreenHelperPipes {
    control_for_helper: OwnedHandle,
    control_for_core: OwnedHandle,
    frames_for_helper: OwnedHandle,
    frames_for_core: OwnedHandle,
}

impl ScreenHelperPipes {
    pub fn new() -> Result<Self, LaunchError> {
        let (control_for_helper, control_for_core) = pipe(true)?;
        let (frames_for_helper, frames_for_core) = pipe(false)?;
        Ok(Self {
            control_for_helper,
            control_for_core,
            frames_for_helper,
            frames_for_core,
        })
    }

    /// The handle value of the helper's control pipe end. The helper gets
    /// the same value, because it inherits the handle.
    pub fn control_value(&self) -> u32 {
        handle_value(&self.control_for_helper)
    }

    /// The handle value of the helper's frame pipe end.
    pub fn frames_value(&self) -> u32 {
        handle_value(&self.frames_for_helper)
    }
}

/// The 32-bit significant value of a kernel handle. Windows handles fit in
/// 32 bits on every supported system, so the value goes into a `u32` field
/// of the call protocol.
fn handle_value(handle: &OwnedHandle) -> u32 {
    use std::os::windows::io::AsRawHandle;
    handle.as_raw_handle() as usize as u32
}

/// A screen helper in its Job. Dropping it closes the Job, and
/// KILL_ON_JOB_CLOSE ends the process.
pub struct ScreenHelperProcess(JobWorker);

impl ScreenHelperProcess {
    /// Waits up to `timeout` for the process to exit. Returns whether it did.
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        self.0.wait(timeout) == Wait::Exited
    }

    /// Ends the helper now by terminating its Job.
    pub fn terminate(&self) {
        self.0.kill();
    }

    /// The exit code. `None` while the process runs or when the OS cannot
    /// say.
    pub fn exit_code(&self) -> Option<u32> {
        let mut code = 0u32;
        // SAFETY: reads the exit code of the helper's own process handle.
        let ok = unsafe { GetExitCodeProcess(raw(&self.0.process), &mut code) };
        (ok != 0 && code != STILL_ACTIVE as u32).then_some(code)
    }

    /// The peak committed memory of the process, from its Job. Zero when the
    /// OS does not say.
    pub fn peak_memory(&self) -> usize {
        job_peak_memory(&self.0.job)
    }
}

/// A started screen helper and the core's ends of its pipes.
pub struct StartedScreenHelper {
    pub process: ScreenHelperProcess,
    /// The write end of the control pipe. Dropping it asks the helper to
    /// end. The core never writes to it.
    pub control: File,
    /// The read end of the frame pipe. It goes to the call worker
    /// ([`LongLivedWorker::give_handle`]); the core never reads it.
    pub frames: OwnedHandle,
}

/// The characters a helper argument may hold: ASCII letters and digits and
/// `-`, `=`, `,`, `.`. No quote, space or backslash, so the command line
/// needs no escaping. The helper's arguments (`HelperStart::to_args` of the
/// call protocol) use only these.
fn helper_argument_ok(argument: &str) -> bool {
    !argument.is_empty()
        && argument
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-=,.".contains(&byte))
}

/// Starts the screen helper at `exe` with `args` and its two pipes.
///
/// The helper runs with the user's normal token, because Windows.Graphics.
/// Capture is refused in an AppContainer. It starts suspended, in a Job with
/// one active process, kill-on-close and a memory limit, with the creation-
/// time mitigations of [`helper_mitigation_flags`], with child process
/// creation blocked, inheriting only the two pipe handles of `pipes` (an
/// explicit handle list), with null standard handles, no console and a
/// minimal environment. It is in the Job before it runs.
///
/// The helper has no console (`DETACHED_PROCESS`): a console program would
/// make the system start `conhost.exe` for it, and the child process policy
/// stops that (the process then fails to start with `STATUS_DLL_INIT_FAILED`,
/// measured on 25 November 2026).
pub fn start_screen_helper(
    exe: &Path,
    args: &[String],
    pipes: ScreenHelperPipes,
) -> Result<StartedScreenHelper, LaunchError> {
    if args.len() > 16 || !args.iter().all(|argument| helper_argument_ok(argument)) {
        return Err(LaunchError::Api {
            call: "helper argument",
            code: 0,
        });
    }
    launch_helper(exe, args, pipes)
}

/// The handles the helper inherits: exactly its two pipe ends, in this
/// order. Nothing else is inherited.
fn helper_inherit_list(pipes: &ScreenHelperPipes) -> [HANDLE; 2] {
    [
        raw(&pipes.control_for_helper),
        raw(&pipes.frames_for_helper),
    ]
}

/// [`start_screen_helper`] without the argument check, so a test can start
/// a probe program.
fn launch_helper(
    exe: &Path,
    args: &[String],
    pipes: ScreenHelperPipes,
) -> Result<StartedScreenHelper, LaunchError> {
    // Taken first, so it is released last: the helper's pipe ends close
    // before any other start can run (`PROCESS_START`).
    let start = process_start_lock();
    let inherited = helper_inherit_list(&pipes);
    let ScreenHelperPipes {
        control_for_helper,
        control_for_core,
        frames_for_helper,
        frames_for_core,
    } = pipes;
    inheritable_for(&start, &inherited)?;
    let job = job_with(SCREEN_HELPER_MEMORY_LIMIT_BYTES, None, true)?;
    let mitigations = helper_mitigation_flags();
    let child_policy = CHILD_PROCESS_RESTRICTED;
    let mut attributes = AttributeList::new(3)?;
    // SAFETY: each value outlives CreateProcessW below.
    unsafe {
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
        attributes.set(
            PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY as usize,
            (&child_policy as *const u32).cast(),
            size_of_val(&child_policy),
        )?;
    }
    let executable: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut command: Vec<u16> = OsStr::new("\"")
        .encode_wide()
        .chain(exe.as_os_str().encode_wide())
        .chain(OsStr::new("\"").encode_wide())
        .collect();
    for argument in args {
        command.push(u16::from(b' '));
        command.extend(OsStr::new(argument).encode_wide());
    }
    command.push(0);
    let directory: Vec<u16> = env::var_os("SystemRoot")
        .unwrap_or_else(|| "C:\\Windows".into())
        .encode_wide()
        .chain(Some(0))
        .collect();
    let environment = environment_block();
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    // Null standard handles: the helper has no console and no input.
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
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
                | DETACHED_PROCESS
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
    // The helper must not run a single instruction outside the Job.
    // SAFETY: both handles are valid; on failure the suspended process is killed.
    if unsafe { AssignProcessToJobObject(raw(&job), raw(&process)) } == 0 {
        let error = api("AssignProcessToJobObject");
        unsafe { TerminateProcess(raw(&process), 1) };
        return Err(error);
    }
    // The helper has its own copies now.
    drop(control_for_helper);
    drop(frames_for_helper);
    // SAFETY: resumes the helper's primary thread.
    if unsafe { ResumeThread(raw(&thread)) } == u32::MAX {
        let error = api("ResumeThread");
        unsafe { TerminateJobObject(raw(&job), 1) };
        return Err(error);
    }
    Ok(StartedScreenHelper {
        process: ScreenHelperProcess(JobWorker { process, job }),
        control: File::from(control_for_core),
        frames: frames_for_core,
    })
}

/// Duplicates `source` into the process `target` and gives the value of the
/// new handle there. The new handle has the access of the old one and is not
/// inheritable.
fn duplicate_into(target: HANDLE, source: &OwnedHandle) -> Result<u32, LaunchError> {
    let mut duplicate: HANDLE = null_mut();
    // SAFETY: `source` is a handle of this process; `target` is a process
    // handle with PROCESS_DUP_HANDLE access; `duplicate` is a valid out
    // value.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            raw(source),
            target,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        return Err(api("DuplicateHandle"));
    }
    // A value that does not fit the call protocol's field is never sent. It
    // cannot be closed from here, and the worker ends with the call.
    u32::try_from(duplicate as usize).map_err(|_| LaunchError::Api {
        call: "DuplicateHandle",
        code: 0,
    })
}

impl LongLivedWorker {
    /// Duplicates `handle` into the worker process and gives its value
    /// there. The core sends the value to the worker (`ScreenFrames`); the
    /// worker checks that it is a pipe before it reads. The caller keeps its
    /// own handle and closes it.
    pub fn give_handle(&self, handle: &OwnedHandle) -> Result<u32, LaunchError> {
        duplicate_into(raw(&self.0.process), handle)
    }
}

/// The peak committed memory of the Job's process in bytes, from the Job's
/// extended limit information. Zero when the OS does not say.
fn job_peak_memory(job: &OwnedHandle) -> usize {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: `limits` is the documented structure for this class.
    let ok = unsafe {
        QueryInformationJobObject(
            raw(job),
            JobObjectExtendedLimitInformation,
            (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of_val(&limits) as u32,
            null_mut(),
        )
    };
    if ok == 0 {
        0
    } else {
        limits.PeakProcessMemoryUsed
    }
}

fn job_stats(job: &OwnedHandle, process: &OwnedHandle, elapsed: Duration) -> WorkerStats {
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    let mut exit_code = u32::MAX;
    let peak_process_memory = job_peak_memory(job);
    // SAFETY: each buffer is the documented structure for its class.
    unsafe {
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
        peak_process_memory,
        user_cpu: Duration::from_nanos(u64::try_from(accounting.TotalUserTime).unwrap_or(0) * 100),
        exit_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::{
        EqualSid, GetSidIdentifierAuthority, GetSidSubAuthority, GetSidSubAuthorityCount,
    };

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
        let sids = CapabilitySids::derive(CALL_CAPABILITIES, &[]).unwrap();
        assert_eq!(sids.sids.len(), CALL_CAPABILITIES.len());
        for (i, sid) in sids.sids.iter().enumerate() {
            assert!(sids.sids[i + 1..].iter().all(|other| other != sid));
        }
        assert!(sids.attributes().iter().all(|a| a.Attributes == 4));
    }

    /// GIVEN the `webcam` device class GUID WHEN turned into RIDs THEN they
    /// are the four numbers of the device capability SID that packaged apps
    /// carry, `S-1-15-3-3845273463-1331427702-1186551195-1148109977`.
    #[test]
    fn given_webcam_guid_then_the_rids_are_those_of_its_device_capability_sid() {
        assert_eq!(
            WEBCAM_DEVICE_CAPABILITY,
            [3_845_273_463, 1_331_427_702, 1_186_551_195, 1_148_109_977]
        );
    }

    /// GIVEN the call and media launch specs THEN only the call worker gets a
    /// device capability, and that is the camera only.
    #[test]
    fn given_launch_specs_then_only_the_call_worker_gets_the_camera_device_capability() {
        assert_eq!(
            LaunchSpec::call().device_capabilities,
            [WEBCAM_DEVICE_CAPABILITY]
        );
        let media = LaunchSpec::media(&WorkerPolicy::default());
        assert!(media.device_capabilities.is_empty());
    }

    /// GIVEN the call capabilities and the camera device capability WHEN
    /// the SIDs are built THEN the token list has the named SIDs and then
    /// `S-1-15-3-<RIDs>`, each enabled, and nothing else.
    #[test]
    fn given_device_capability_then_its_sid_is_app_package_authority_base_rid_and_guid() {
        let sids = CapabilitySids::derive(CALL_CAPABILITIES, CALL_DEVICE_CAPABILITIES).unwrap();
        let list = sids.attributes();
        assert_eq!(list.len(), CALL_CAPABILITIES.len() + 1);
        assert!(list.iter().all(|a| a.Attributes == SE_GROUP_ENABLED));
        let device = list.last().unwrap().Sid;
        // SAFETY: `device` is a valid SID owned by `sids`, which lives on.
        unsafe {
            assert_eq!(
                (*GetSidIdentifierAuthority(device)).Value,
                SECURITY_APP_PACKAGE_AUTHORITY.Value
            );
            assert_eq!(*GetSidSubAuthorityCount(device), 5);
            let rids: Vec<u32> = (0..5).map(|i| *GetSidSubAuthority(device, i)).collect();
            assert_eq!(rids[0], SECURITY_CAPABILITY_BASE_RID);
            assert_eq!(rids[1..], WEBCAM_DEVICE_CAPABILITY);
        }
        // The named `webcam` SID is a different SID; both are kept.
        assert!(
            sids.sids
                .iter()
                .all(|&named| unsafe { EqualSid(named, device) } == 0)
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

    // --- The screen helper (ADR-0067) ---

    /// GIVEN the screen helper's mitigations WHEN compared with the call
    /// worker's THEN they are the same set: every bit of the media worker's
    /// except dynamic-code prohibition and Win32k disable, which Direct3D
    /// and the capture runtime need. Strict handle checks, DEP, ASLR,
    /// extension points off, font loading off and the image-load limits stay.
    #[test]
    fn given_the_helper_spec_then_mitigations_are_the_call_workers() {
        let flags = helper_mitigation_flags();
        assert_eq!(flags, mitigation_flags(LaunchSpec::call().mitigations));
        for bit in [
            STRICT_HANDLE_CHECKS_ALWAYS_ON,
            EXTENSION_POINT_DISABLE_ALWAYS_ON,
            FONT_DISABLE_ALWAYS_ON,
            IMAGE_LOAD_NO_REMOTE_ALWAYS_ON,
            IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON,
            IMAGE_LOAD_PREFER_SYSTEM32_ALWAYS_ON,
            BOTTOM_UP_ASLR_ALWAYS_ON,
            HIGH_ENTROPY_ASLR_ALWAYS_ON,
            HEAP_TERMINATE_ALWAYS_ON,
        ] {
            assert_eq!(flags & bit, bit);
        }
        assert_eq!(flags & DEP_ENABLE, DEP_ENABLE);
        assert_eq!(
            flags & (PROHIBIT_DYNAMIC_CODE_ALWAYS_ON | WIN32K_SYSTEM_CALL_DISABLE_ALWAYS_ON),
            0
        );
    }

    /// GIVEN the pipes of one share WHEN the helper's inherit list is built
    /// THEN it holds exactly two handles: the control pipe's read end and the
    /// frame pipe's write end, in the order of the helper's arguments, with
    /// the values the core puts on the command line. The core's ends are not
    /// in the list and are not inheritable.
    #[test]
    fn given_helper_pipes_then_it_inherits_only_its_two_ends() {
        use windows_sys::Win32::Foundation::GetHandleInformation;
        let pipes = ScreenHelperPipes::new().expect("pipes");
        let list = helper_inherit_list(&pipes);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], raw(&pipes.control_for_helper));
        assert_eq!(list[1], raw(&pipes.frames_for_helper));
        assert_ne!(list[0], list[1]);
        // That the values on the command line name the handles the helper
        // inherits is seen in the child: `helper_probe_child` calls
        // `GetFileType` on them.
        let flags = |handle: &OwnedHandle| {
            let mut flags = 0u32;
            // SAFETY: reads the flags of a handle this test owns.
            assert_ne!(unsafe { GetHandleInformation(raw(handle), &mut flags) }, 0);
            flags
        };
        // No end is inheritable until a launch holds the process-start lock.
        assert_eq!(flags(&pipes.control_for_helper) & HANDLE_FLAG_INHERIT, 0);
        assert_eq!(flags(&pipes.frames_for_helper) & HANDLE_FLAG_INHERIT, 0);
        assert_eq!(flags(&pipes.control_for_core) & HANDLE_FLAG_INHERIT, 0);
        assert_eq!(flags(&pipes.frames_for_core) & HANDLE_FLAG_INHERIT, 0);
    }

    /// GIVEN the helper's Job WHEN its limits are read THEN it holds one
    /// active process, kills it on close, caps memory at the helper's limit
    /// and has no CPU limit, and its UI limits stop the clipboard, global
    /// atoms, other processes' USER handles and desktop changes.
    #[test]
    fn given_the_helper_job_then_it_holds_one_process_with_a_memory_cap_and_ui_limits() {
        let job = job_with(SCREEN_HELPER_MEMORY_LIMIT_BYTES, None, true).expect("a job");
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let mut ui = JOBOBJECT_BASIC_UI_RESTRICTIONS::default();
        // SAFETY: each buffer is the documented structure for its class.
        unsafe {
            assert_ne!(
                QueryInformationJobObject(
                    raw(&job),
                    JobObjectExtendedLimitInformation,
                    (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of_val(&limits) as u32,
                    null_mut(),
                ),
                0
            );
            assert_ne!(
                QueryInformationJobObject(
                    raw(&job),
                    JobObjectBasicUIRestrictions,
                    (&mut ui as *mut JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
                    size_of_val(&ui) as u32,
                    null_mut(),
                ),
                0
            );
        }
        let flags = limits.BasicLimitInformation.LimitFlags;
        for flag in [
            JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
        ] {
            assert_eq!(flags & flag, flag);
        }
        assert_eq!(flags & JOB_OBJECT_LIMIT_PROCESS_TIME, 0);
        assert_eq!(limits.BasicLimitInformation.ActiveProcessLimit, 1);
        assert_eq!(limits.ProcessMemoryLimit, 512 * 1024 * 1024);
        assert_eq!(ui.UIRestrictionsClass, HELPER_UI_LIMITS);
        assert_ne!(HELPER_UI_LIMITS & JOB_OBJECT_UILIMIT_HANDLES, 0);
        assert_ne!(HELPER_UI_LIMITS & JOB_OBJECT_UILIMIT_READCLIPBOARD, 0);
    }

    /// GIVEN arguments with a quote, a space, a backslash, a NUL, a letter
    /// outside ASCII or none at all WHEN the helper is started THEN the
    /// start is refused before any process exists; the arguments the call
    /// protocol builds pass.
    #[test]
    fn given_unsafe_helper_arguments_when_started_then_nothing_is_launched() {
        for bad in [
            "",
            "a b",
            "a\"b",
            r"a\b",
            "a\0b",
            "caf\u{e9}",
            "--x;",
            "a&b",
        ] {
            assert!(!helper_argument_ok(bad), "{bad:?}");
            let pipes = ScreenHelperPipes::new().expect("pipes");
            let result = start_screen_helper(
                Path::new(r"C:\Windows\System32\cmd.exe"),
                &[bad.to_owned()],
                pipes,
            );
            assert!(
                matches!(
                    result,
                    Err(LaunchError::Api {
                        call: "helper argument",
                        ..
                    })
                ),
                "{bad:?}"
            );
        }
        for good in [
            "--share=7",
            "--name=5c5c2e5c444953504c415931",
            "--rect=-1920,-100,0,980",
        ] {
            assert!(helper_argument_ok(good), "{good}");
        }
        let pipes = ScreenHelperPipes::new().expect("pipes");
        let many = vec!["--a=1".to_owned(); 17];
        assert!(start_screen_helper(Path::new("x.exe"), &many, pipes).is_err());
    }

    /// GIVEN a pipe WHEN it is duplicated into a process THEN the new handle
    /// is another value for the same pipe, and a bad source is refused. The
    /// process here is this one, which has the access that the core has to
    /// the call worker it started.
    #[test]
    fn given_a_pipe_when_duplicated_into_a_process_then_the_copy_is_the_same_pipe() {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
        let pipes = ScreenHelperPipes::new().expect("pipes");
        // SAFETY: the pseudo handle of this process, valid for the call.
        let me = unsafe { GetCurrentProcess() };
        let value = duplicate_into(me, &pipes.frames_for_core).expect("a duplicate");
        let copy = value as usize as HANDLE;
        assert_ne!(copy, raw(&pipes.frames_for_core));
        // SAFETY: `copy` is a handle this test just made in this process.
        unsafe {
            assert_eq!(GetFileType(copy), FILE_TYPE_PIPE);
            assert_ne!(CloseHandle(copy), 0);
        }
        // A handle value that is not open is refused, and never closed here.
        // SAFETY: the value is not a live handle; `ManuallyDrop` keeps it
        // from being closed.
        let stale = std::mem::ManuallyDrop::new(unsafe {
            OwnedHandle::from_raw_handle(0x00ff_fff0_usize as RawHandle)
        });
        assert!(matches!(
            duplicate_into(me, &stale),
            Err(LaunchError::Api {
                call: "DuplicateHandle",
                ..
            })
        ));
    }

    /// The probe that `given_a_started_helper_...` launches. It runs only
    /// inside that test's child process (it is ignored otherwise): it
    /// checks what the helper's launch gave it, then waits for the control
    /// pipe to close.
    #[test]
    #[ignore = "runs as the child of the helper launch test"]
    fn helper_probe_child() {
        use std::io::Write;
        use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;
        let numbers: Vec<usize> = env::args()
            .filter_map(|argument| argument.parse::<usize>().ok())
            .collect();
        let [control, frames] = numbers[..] else {
            std::process::exit(21)
        };
        let (control, frames) = (control as HANDLE, frames as HANDLE);
        // SAFETY: asks the kernel about the two inherited handles.
        let (control_type, frames_type) = unsafe { (GetFileType(control), GetFileType(frames)) };
        if control_type != FILE_TYPE_PIPE || frames_type != FILE_TYPE_PIPE {
            std::process::exit(22);
        }
        // A child process cannot be made: one active process in the Job, and
        // the child process policy.
        if std::process::Command::new(r"C:\Windows\System32\cmd.exe")
            .args(["/c", "exit"])
            .status()
            .is_ok()
        {
            std::process::exit(23);
        }
        // SAFETY: the frame pipe's write end is inherited and owned here.
        let mut out = unsafe { File::from_raw_handle(frames as RawHandle) };
        if out.write_all(b"k").is_err() {
            std::process::exit(24);
        }
        // Wait for the core to close its end of the control pipe.
        for _ in 0..1000 {
            let mut available = 0u32;
            // SAFETY: peeks the control pipe; nothing is read.
            let peeked = unsafe {
                PeekNamedPipe(
                    control,
                    null_mut(),
                    0,
                    null_mut(),
                    &mut available,
                    null_mut(),
                )
            };
            if peeked == 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::process::exit(25);
    }

    /// GIVEN this test program started as a helper WHEN it runs THEN it has
    /// its two pipes and nothing else to wait on, it cannot start a child
    /// process, the frame pipe carries its byte to the core, closing the
    /// control pipe ends it, and its exit code is read.
    #[test]
    fn given_a_started_helper_then_it_has_only_its_pipes_and_ends_when_control_closes() {
        use std::io::Read;
        let pipes = ScreenHelperPipes::new().expect("pipes");
        let (control, frames) = (pipes.control_value(), pipes.frames_value());
        let exe = env::current_exe().expect("this test program");
        let args: Vec<String> = [
            "--ignored",
            "--exact",
            "windows::tests::helper_probe_child",
            "--nocapture",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain([control.to_string(), frames.to_string()])
        .collect();
        let started = launch_helper(&exe, &args, pipes).expect("the probe starts");
        let mut frames = File::from(started.frames);
        let mut byte = [0u8; 1];
        if frames.read_exact(&mut byte).is_err() {
            started.process.wait_exit(Duration::from_secs(5));
            panic!("no byte; exit code {:?}", started.process.exit_code());
        }
        assert_eq!(&byte, b"k");
        assert!(!started.process.wait_exit(Duration::from_millis(50)));
        assert_eq!(started.process.exit_code(), None);
        drop(started.control);
        assert!(started.process.wait_exit(Duration::from_secs(20)));
        assert_eq!(started.process.exit_code(), Some(0));
        assert!(started.process.peak_memory() > 0);
        // The frame pipe is closed at the helper's end now.
        assert_eq!(frames.read(&mut byte).expect("end of pipe"), 0);
    }

    /// A probe that ignores the control pipe and sleeps. It runs only as the
    /// child of the terminate test.
    #[test]
    #[ignore = "runs as the child of the helper terminate test"]
    fn helper_probe_sleep() {
        std::thread::sleep(Duration::from_secs(120));
    }

    /// GIVEN a started helper that ignores the control pipe WHEN the core
    /// terminates its Job THEN it ends at once and has an exit code.
    #[test]
    fn given_a_started_helper_when_terminated_then_it_ends_with_a_code() {
        let pipes = ScreenHelperPipes::new().expect("pipes");
        let exe = env::current_exe().expect("this test program");
        let args: Vec<String> = ["--ignored", "--exact", "windows::tests::helper_probe_sleep"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let helper = launch_helper(&exe, &args, pipes).expect("the probe starts");
        assert!(!helper.process.wait_exit(Duration::from_millis(200)));
        assert_eq!(helper.process.exit_code(), None);
        helper.process.terminate();
        assert!(helper.process.wait_exit(Duration::from_secs(5)));
        assert_eq!(helper.process.exit_code(), Some(1));
    }
}

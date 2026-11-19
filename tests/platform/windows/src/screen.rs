//! Screen capture probe (ADR-0053): does Windows.Graphics.Capture run inside
//! the call worker's AppContainer?
//!
//! `persephone-test-windows --screen-probe` first runs the capture in this
//! process as a positive control (the same user session, no container). It
//! then launches the same executable in an AppContainer with the call
//! worker's capabilities and process mitigations, once for each capability
//! set below, and reports how far `CreateForMonitor` and one frame got. The
//! capture code is the worker's own `screen/wgc.rs`, compiled into this
//! executable, so the measurement uses the code that ships.
//!
//! The launch below repeats `LaunchSpec::call()` of
//! `crates/media-broker/src/windows.rs` (capabilities, mitigations, Job
//! limits) plus one candidate capability. Keep the two in step.
//!
//! Exit status of the parent: 0 when the control captured a frame and every
//! container run finished (whatever it measured); 1 when the control failed
//! or a run could not be launched; the result lines say what each set got.
//! A set that cannot capture is a measurement, not a gate failure: the
//! worker is granted only a capability that a set below shows to work.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, LocalFree, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    DeriveCapabilitySidsFromName, FreeSid, PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
    InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ResumeThread, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

#[allow(dead_code)] // the worker uses the parts the probe does not
#[path = "../../../../crates/call-worker/src/screen/wgc.rs"]
mod wgc;

const PARENT_ARG: &str = "--screen-probe";
const CHILD_ARG: &str = "--screen-probe-child";
/// The capabilities the call worker has today (`CALL_CAPABILITIES`).
const CALL_CAPABILITIES: [&str; 3] = ["internetClient", "microphone", "webcam"];
/// The candidates tried on top of them.
const CANDIDATES: [(&str, &[&str]); 3] = [
    (
        "with graphicsCaptureProgrammatic",
        &["graphicsCaptureProgrammatic"],
    ),
    ("with graphicsCapture", &["graphicsCapture"]),
    (
        "with both",
        &["graphicsCaptureProgrammatic", "graphicsCapture"],
    ),
];
const RUN_TIMEOUT: Duration = Duration::from_secs(30);
/// How many more `RUN_TIMEOUT` waits the probe gives a child whose exit was
/// not confirmed, ending it again before each.
const EXIT_ROUNDS: u32 = 10;
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const MEMORY_LIMIT_BYTES: usize = 512 * 1024 * 1024;
// PROCESS_CREATION_MITIGATION_POLICY_* of the call worker: everything the
// media launcher sets except prohibit-dynamic-code and Win32k lockdown.
const DEP_ENABLE: u64 = 0x1;
const HEAP_TERMINATE_ALWAYS_ON: u64 = 1 << 12;
const BOTTOM_UP_ASLR_ALWAYS_ON: u64 = 1 << 16;
const HIGH_ENTROPY_ASLR_ALWAYS_ON: u64 = 1 << 20;
const STRICT_HANDLE_CHECKS_ALWAYS_ON: u64 = 1 << 24;
const EXTENSION_POINT_DISABLE_ALWAYS_ON: u64 = 1 << 32;
const FONT_DISABLE_ALWAYS_ON: u64 = 1 << 48;
const IMAGE_LOAD_NO_REMOTE_ALWAYS_ON: u64 = 1 << 52;
const IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON: u64 = 1 << 56;
const IMAGE_LOAD_PREFER_SYSTEM32_ALWAYS_ON: u64 = 1 << 60;
const SE_GROUP_ENABLED: u32 = 0x4;
const HRESULT_ALREADY_EXISTS: i32 = 0x800700B7_u32 as i32;
/// Exit code of a child that did not reach the capture code at all.
const NOT_STARTED: u32 = 0xFFFF_FFFF;

/// Runs the probe when the arguments ask for it. `None`: not a screen probe
/// invocation.
pub fn dispatch() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next()?;
    if first == OsStr::new(CHILD_ARG) {
        let index = args
            .next()
            .and_then(|arg| arg.to_str().and_then(|text| text.parse::<usize>().ok()))
            .unwrap_or(0);
        let part = args
            .next()
            .and_then(|arg| arg.to_str().and_then(|text| text.parse::<u32>().ok()))
            .unwrap_or(0);
        let stage = child(index);
        // Exit codes are the only channel out of the container, and 32 bits
        // cannot hold a stage and a whole HRESULT. Part 1 and 2 return the
        // stage of this run with one half of its last HRESULT.
        let hresult = wgc::last_error() as u32;
        return Some(encode(stage, hresult, part) as i32);
    }
    if first == OsStr::new(PARENT_ARG) {
        return Some(parent());
    }
    None
}

/// The exit code of a child run. Part 0 is the stage alone. Part 1 and 2 hold
/// the stage in bits 16 and up and the high or low half of the HRESULT below.
fn encode(stage: u32, hresult: u32, part: u32) -> u32 {
    match part {
        1 => (stage << 16) | (hresult >> 16),
        2 => (stage << 16) | (hresult & 0xFFFF),
        _ => stage,
    }
}

/// The stage and the HRESULT half of a part 1 or 2 exit code. `None` when the
/// code cannot come from `encode` (for example `NOT_STARTED`).
fn decode(code: u32) -> Option<(u32, u32)> {
    let stage = code >> 16;
    (stage <= 0xFF).then_some((stage, code & 0xFFFF))
}

/// The HRESULT of one failure, built only from two runs that report the same
/// stage as the first run. `None` when the failure changed between the runs.
fn combine(stage: u32, high: u32, low: u32) -> Option<u32> {
    let (high_stage, high) = decode(high)?;
    let (low_stage, low) = decode(low)?;
    (high_stage == stage && low_stage == stage).then_some((high << 16) | low)
}

/// The capture attempt: 0 when a frame arrived, else the stage that failed
/// (`wgc::Stage` as a number).
fn child(index: usize) -> u32 {
    let Ok(_com) = wgc::ComGuard::start() else {
        return wgc::Stage::Com as u32;
    };
    let mut capture = match wgc::open(index, None) {
        Ok(capture) => capture,
        Err(stage) => return stage as u32,
    };
    let until = Instant::now() + FRAME_TIMEOUT;
    let mut got = false;
    while !got && Instant::now() < until {
        let mut admit = || true;
        let mut deliver = |width: u32, height: u32, _stride: u32, bytes: &[u8]| {
            got = width > 0 && height > 0 && !bytes.is_empty();
        };
        match capture.poll(&mut admit, &mut deliver) {
            Ok(true) => {}
            Ok(false) => std::thread::sleep(Duration::from_millis(30)),
            Err(stage) => return stage as u32,
        }
    }
    if got { 0 } else { wgc::Stage::NoFrame as u32 }
}

fn describe(code: u32) -> String {
    let name = match code {
        0 => "captured a frame",
        1 => "COM did not start",
        2 => "no monitor listed",
        3 => "monitor size differs",
        4 => "capture item factory missing",
        5 => "CreateForMonitor failed",
        6 => "Direct3D 11 device failed",
        7 => "WinRT device failed",
        8 => "frame pool failed",
        9 => "capture session failed",
        10 => "StartCapture failed",
        11 => "no frame arrived",
        12 => "frame texture unreadable",
        13 => "frame map failed",
        14 => "bad capture size",
        NOT_STARTED => "the process did not run",
        _ => "the process ended with another code",
    };
    format!("exit {code:#x}: {name}")
}

fn parent() -> i32 {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            println!("screen-probe: no executable path: {error}");
            return 1;
        }
    };
    let monitors = {
        let Ok(_com) = wgc::ComGuard::start() else {
            println!("screen-probe: COM did not start");
            return 1;
        };
        wgc::monitors()
    };
    println!("screen-probe: {} monitor(s) listed", monitors.len());
    for (index, monitor) in monitors.iter().enumerate() {
        println!(
            "screen-probe: monitor {index} {}x{}",
            monitor.width, monitor.height
        );
    }
    let control = child(0);
    println!(
        "screen-probe: control (this process, no container): {}",
        describe(control)
    );
    if control != 0 {
        println!("screen-probe: INCONCLUSIVE, the control did not capture a frame");
        return 1;
    }
    let profile = format!("Persephone.Screen.Probe.{}", std::process::id());
    let Some(container) = Container::create(&profile) else {
        println!("screen-probe: could not create the AppContainer profile");
        return 1;
    };
    let mut failed = false;
    let mut sets: Vec<(String, Vec<&str>)> =
        vec![("call worker today".to_owned(), CALL_CAPABILITIES.to_vec())];
    for (name, extra) in CANDIDATES {
        let mut caps = CALL_CAPABILITIES.to_vec();
        caps.extend_from_slice(extra);
        sets.push((name.to_owned(), caps));
    }
    for (name, caps) in &sets {
        let run = |part| run_in_container(&container, &exe, caps, part);
        match run(0) {
            Ok(code) => {
                let mut line = describe(code);
                if code != 0 {
                    // The error code of the failed call, in two more runs. Each
                    // run reports its own stage. Use the halves only when both
                    // runs failed at the stage of the first run.
                    let high = match run(1) {
                        Err(RunError::Unconfirmed(error, child)) => {
                            return abandon(container, name, &error, child);
                        }
                        other => other,
                    };
                    let low = match run(2) {
                        Err(RunError::Unconfirmed(error, child)) => {
                            return abandon(container, name, &error, child);
                        }
                        other => other,
                    };
                    match (high, low) {
                        (Ok(high), Ok(low)) => match combine(code, high, low) {
                            Some(hresult) => line.push_str(&format!(
                                ", HRESULT {hresult:#010x} (halves of two repeat runs, same stage)"
                            )),
                            None => line.push_str(", HRESULT unknown: the failure changed"),
                        },
                        // A capture failure is a result; a repeat that did
                        // not launch is a failed measurement.
                        _ => {
                            line.push_str(", HRESULT unknown: a repeat run did not launch");
                            failed = true;
                        }
                    }
                }
                println!(
                    "screen-probe: AppContainer [{}] ({name}): {line}",
                    caps.join(", ")
                );
            }
            Err(RunError::Failed(error)) => {
                println!("screen-probe: AppContainer ({name}): launch failed: {error}");
                failed = true;
            }
            Err(RunError::Unconfirmed(error, child)) => {
                return abandon(container, name, &error, child);
            }
        }
    }
    i32::from(failed)
}

/// Stops the probe after a run whose child may still be running. The probe
/// keeps owning the child and waits longer for its exit. Only a confirmed
/// exit releases the handles and deletes the AppContainer profile. Otherwise
/// the profile is kept; the handles close when the probe exits, and the
/// Job's kill-on-close limit ends a child that is in the Job.
fn abandon(container: Container, name: &str, error: &str, child: Unexited) -> i32 {
    println!("screen-probe: AppContainer ({name}): {error}; the probe stops");
    if child.end_and_wait_longer() {
        println!("screen-probe: the child exited; the AppContainer profile is deleted");
    } else {
        println!(
            "screen-probe: the child's exit was not confirmed; the AppContainer profile is kept"
        );
        container.keep();
    }
    1
}

/// Why a container run gave no exit code.
enum RunError {
    /// The run failed, and its child has exited or never started.
    Failed(String),
    /// The child may still run. The caller keeps its handles until it
    /// confirms the exit, and must not delete the profile before that.
    Unconfirmed(String, Unexited),
}

impl From<&str> for RunError {
    fn from(error: &str) -> Self {
        Self::Failed(error.to_owned())
    }
}

impl From<String> for RunError {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

/// The child (its Job, process and thread) did not confirm its exit. The
/// handles go to the caller, so the process cannot be mistaken for another
/// and the Job keeps its limits while the caller waits.
fn unconfirmed(what: &str, job: Owned, process: Owned, thread: Owned) -> RunError {
    RunError::Unconfirmed(
        format!("{what}, and the probe's exit was not confirmed"),
        Unexited {
            job,
            process,
            thread,
        },
    )
}

/// A child whose exit is not confirmed, with the handles that own it.
struct Unexited {
    job: Owned,
    process: Owned,
    // Held, not used: kept open until the exit is confirmed.
    #[allow(dead_code)]
    thread: Owned,
}

impl Unexited {
    /// Ends the child again and waits, up to `EXIT_ROUNDS` more times. True
    /// once the process has exited; false if it did not, or if the wait
    /// cannot observe it.
    fn end_and_wait_longer(&self) -> bool {
        for round in 1..=EXIT_ROUNDS {
            // SAFETY: the handles are owned and valid. The Job ends every
            // process in it; the process may not be in the Job.
            unsafe {
                TerminateJobObject(self.job.0, 1);
                TerminateProcess(self.process.0, 1);
            }
            match wait_once(self.process.0) {
                WAIT_OBJECT_0 => return true,
                WAIT_TIMEOUT => println!(
                    "screen-probe: the child has not exited ({round} of {EXIT_ROUNDS} more waits)"
                ),
                _ => return false,
            }
        }
        false
    }
}

/// A named AppContainer profile, deleted when dropped unless kept.
struct Container {
    name: Vec<u16>,
    keep: bool,
}

impl Container {
    fn create(name: &str) -> Option<Self> {
        let wide: Vec<u16> = OsStr::new(name).encode_wide().chain(Some(0)).collect();
        let mut sid: PSID = null_mut();
        // SAFETY: the strings are NUL-terminated; no capabilities are passed.
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
        if hr == 0 {
            // SAFETY: the SID came from CreateAppContainerProfile.
            unsafe { FreeSid(sid) };
        } else if hr != HRESULT_ALREADY_EXISTS {
            return None;
        }
        Some(Self {
            name: wide,
            keep: false,
        })
    }

    /// Keeps the profile: a child that may still run uses it.
    fn keep(mut self) {
        self.keep = true;
    }

    fn sid(&self) -> Option<PSID> {
        let mut sid: PSID = null_mut();
        // SAFETY: the name is NUL-terminated.
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(self.name.as_ptr(), &mut sid) };
        (hr >= 0).then_some(sid)
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        // SAFETY: the name is NUL-terminated.
        unsafe { DeleteAppContainerProfile(self.name.as_ptr()) };
    }
}

/// Capability SIDs derived from names, freed on drop.
struct CapabilitySids(Vec<PSID>);

impl CapabilitySids {
    fn derive(names: &[&str]) -> Option<Self> {
        let mut result = Self(Vec::new());
        for name in names {
            let wide: Vec<u16> = OsStr::new(name).encode_wide().chain(Some(0)).collect();
            let (mut groups, mut group_count): (*mut PSID, u32) = (null_mut(), 0);
            let (mut capabilities, mut capability_count): (*mut PSID, u32) = (null_mut(), 0);
            // SAFETY: the name is NUL-terminated; all out pointers are valid.
            let ok = unsafe {
                DeriveCapabilitySidsFromName(
                    wide.as_ptr(),
                    &mut groups,
                    &mut group_count,
                    &mut capabilities,
                    &mut capability_count,
                )
            };
            if ok == 0 {
                return None;
            }
            // SAFETY: on success each array holds that many SIDs, allocated
            // with LocalAlloc, as does each SID.
            unsafe {
                for index in 0..group_count as usize {
                    LocalFree((*groups.add(index)).cast());
                }
                LocalFree(groups.cast());
                result.0.extend_from_slice(std::slice::from_raw_parts(
                    capabilities,
                    capability_count as usize,
                ));
                LocalFree(capabilities.cast());
            }
            if capability_count != 1 {
                return None;
            }
        }
        Some(result)
    }
}

impl Drop for CapabilitySids {
    fn drop(&mut self) {
        for &sid in &self.0 {
            // SAFETY: each SID was allocated by DeriveCapabilitySidsFromName.
            unsafe { LocalFree(sid.cast()) };
        }
    }
}

/// A handle closed when dropped.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and valid.
        unsafe { CloseHandle(self.0) };
    }
}

/// Ends the Job's process and waits for it to exit, so that the AppContainer
/// profile is not deleted under a running child. Termination is asynchronous.
fn end_and_wait(job: HANDLE, process: HANDLE) -> bool {
    // SAFETY: both handles are valid. The Job ends every process it holds.
    unsafe { TerminateJobObject(job, 1) };
    wait_for_exit(process)
}

/// Waits, bounded, for a process to exit.
fn wait_for_exit(process: HANDLE) -> bool {
    wait_once(process) == WAIT_OBJECT_0
}

/// One wait of `RUN_TIMEOUT` on a process handle.
fn wait_once(process: HANDLE) -> WAIT_EVENT {
    // SAFETY: waits on a process handle the caller owns.
    unsafe { WaitForSingleObject(process, RUN_TIMEOUT.as_millis() as u32) }
}

/// The capture attempt in the container. `part` 0 returns the stage, 1 and 2
/// the stage with one half of the failure's HRESULT (see `encode`).
fn run_in_container(
    container: &Container,
    exe: &Path,
    capabilities: &[&str],
    part: u32,
) -> Result<u32, RunError> {
    let sid = container
        .sid()
        .ok_or("DeriveAppContainerSidFromAppContainerName")?;
    let sids = CapabilitySids::derive(capabilities).ok_or("DeriveCapabilitySidsFromName")?;
    let mut list: Vec<SID_AND_ATTRIBUTES> = sids
        .0
        .iter()
        .map(|&sid| SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: SE_GROUP_ENABLED,
        })
        .collect();
    let security = SECURITY_CAPABILITIES {
        AppContainerSid: sid,
        Capabilities: list.as_mut_ptr(),
        CapabilityCount: list.len() as u32,
        Reserved: 0,
    };
    let mitigations = DEP_ENABLE
        | HEAP_TERMINATE_ALWAYS_ON
        | BOTTOM_UP_ASLR_ALWAYS_ON
        | HIGH_ENTROPY_ASLR_ALWAYS_ON
        | STRICT_HANDLE_CHECKS_ALWAYS_ON
        | EXTENSION_POINT_DISABLE_ALWAYS_ON
        | FONT_DISABLE_ALWAYS_ON
        | IMAGE_LOAD_NO_REMOTE_ALWAYS_ON
        | IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON
        | IMAGE_LOAD_PREFER_SYSTEM32_ALWAYS_ON;

    // SAFETY: an anonymous Job with default security.
    let job = unsafe { CreateJobObjectW(null(), null()) };
    if job.is_null() {
        return Err("CreateJobObjectW".into());
    }
    let job = Owned(job);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    limits.ProcessMemoryLimit = MEMORY_LIMIT_BYTES;
    // SAFETY: `limits` is the documented structure for this class.
    if unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err("SetInformationJobObject".into());
    }

    let mut attribute_bytes = 0_usize;
    // SAFETY: the first call only asks for the size.
    unsafe { InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut attribute_bytes) };
    let mut storage = vec![0_usize; attribute_bytes.div_ceil(size_of::<usize>())];
    let attributes = storage.as_mut_ptr().cast();
    // SAFETY: `storage` has the size the first call asked for.
    if unsafe { InitializeProcThreadAttributeList(attributes, 2, 0, &mut attribute_bytes) } == 0 {
        return Err("InitializeProcThreadAttributeList".into());
    }
    struct Attributes(*mut std::ffi::c_void);
    impl Drop for Attributes {
        fn drop(&mut self) {
            // SAFETY: the list was initialised.
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
    let _attributes = Attributes(attributes);
    // SAFETY: each value outlives CreateProcessW below.
    unsafe {
        if UpdateProcThreadAttribute(
            attributes,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            (&raw const security).cast(),
            size_of_val(&security),
            null_mut(),
            null(),
        ) == 0
            || UpdateProcThreadAttribute(
                attributes,
                0,
                PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY as usize,
                (&raw const mitigations).cast(),
                size_of_val(&mitigations),
                null_mut(),
                null(),
            ) == 0
        {
            return Err("UpdateProcThreadAttribute".into());
        }
    }

    let executable: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut command: Vec<u16> = OsStr::new(&format!("\"{}\" {CHILD_ARG} 0 {part}", exe.display()))
        .encode_wide()
        .chain(Some(0))
        .collect();
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let directory: Vec<u16> = system_root.encode_wide().chain(Some(0)).collect();
    // Only the OS runtime variables the AppContainer needs to start, sorted
    // as the launcher sorts them.
    let mut entries: Vec<_> = ["LOCALAPPDATA", "SystemDrive", "SystemRoot", "windir"]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
        .collect();
    entries.sort_by_key(|(name, _)| name.to_ascii_lowercase());
    let mut environment: Vec<u16> = Vec::new();
    for (name, value) in entries {
        environment.extend(OsStr::new(name).encode_wide());
        environment.push(u16::from(b'='));
        environment.extend(value.encode_wide());
        environment.push(0);
    }
    environment.push(0);

    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attributes;
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: all buffers are NUL-terminated and live through the call; no
    // handle is inherited.
    let created = unsafe {
        CreateProcessW(
            executable.as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            0,
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
        // SAFETY: reads this thread's last error.
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(format!("CreateProcessW error {code}").into());
    }
    let process = Owned(info.hProcess);
    let thread = Owned(info.hThread);
    // The probe must not run a single instruction outside the Job.
    // SAFETY: both handles are valid.
    if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
        // The process is not in the Job, so end it directly.
        unsafe { TerminateProcess(process.0, 1) };
        if !wait_for_exit(process.0) {
            return Err(unconfirmed(
                "AssignProcessToJobObject",
                job,
                process,
                thread,
            ));
        }
        return Err("AssignProcessToJobObject".into());
    }
    // SAFETY: resumes the probe's primary thread.
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        if !end_and_wait(job.0, process.0) {
            return Err(unconfirmed("ResumeThread", job, process, thread));
        }
        return Err("ResumeThread".into());
    }
    let waited = wait_once(process.0);
    if waited != WAIT_OBJECT_0 {
        let what = if waited == WAIT_TIMEOUT {
            "the probe did not finish in time"
        } else {
            "WaitForSingleObject failed"
        };
        if !end_and_wait(job.0, process.0) {
            return Err(unconfirmed(what, job, process, thread));
        }
        return Err(what.into());
    }
    let mut code = NOT_STARTED;
    // SAFETY: the process has ended.
    unsafe { GetExitCodeProcess(process.0, &mut code) };
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN every exit code the capture stages can return WHEN described
    /// THEN each has its own name and success is code 0.
    #[test]
    fn given_stage_exit_codes_when_described_then_each_has_a_name() {
        assert!(describe(0).contains("captured a frame"));
        let mut names = std::collections::HashSet::new();
        for code in 1..=14 {
            assert!(names.insert(describe(code)), "{code}");
        }
        assert!(describe(0xC000_0005).contains("another code"));
        assert!(describe(NOT_STARTED).contains("did not run"));
    }

    /// GIVEN two runs that failed at the stage of the first run WHEN the
    /// halves are combined THEN the HRESULT is the one the runs reported.
    #[test]
    fn given_runs_with_the_same_stage_when_combined_then_the_hresult_is_whole() {
        let hresult = 0x8007_0005_u32;
        let high = encode(10, hresult, 1);
        let low = encode(10, hresult, 2);
        assert_eq!(combine(10, high, low), Some(hresult));
    }

    /// GIVEN runs where the failure changed WHEN the halves are combined THEN
    /// no HRESULT is reported.
    #[test]
    fn given_runs_with_another_stage_when_combined_then_no_hresult() {
        let high = encode(10, 0x8007_0005, 1);
        let low = encode(5, 0x8007_0005, 2);
        assert_eq!(combine(10, high, low), None);
        assert_eq!(combine(10, NOT_STARTED, low), None);
    }

    /// GIVEN the call worker's capabilities and the candidates WHEN the
    /// sets are built THEN every candidate keeps the three current
    /// capabilities and adds only graphics capture names.
    #[test]
    fn given_candidates_then_they_only_add_graphics_capture_capabilities() {
        assert_eq!(
            CALL_CAPABILITIES,
            ["internetClient", "microphone", "webcam"]
        );
        for (_, extra) in CANDIDATES {
            assert!(!extra.is_empty());
            assert!(extra.iter().all(|name| name.starts_with("graphicsCapture")));
        }
    }
}

#![cfg(windows)]

use crate::command_line::quote;
use std::{
    env,
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, UdpSocket},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::Command,
    ptr::{null, null_mut},
    slice,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    NetworkManagement::WindowsFirewall::NetworkIsolationDiagnoseConnectFailureAndGetInfo,
    Networking::WinSock::{WSADATA, WSAStartup},
    Security::Isolation::DeriveAppContainerSidFromAppContainerName,
    Security::{
        Authorization::ConvertSidToStringSidW, CreateRestrictedToken, CreateWellKnownSid,
        DISABLE_MAX_PRIVILEGE, GetTokenInformation, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
        SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel, TokenIsAppContainer,
        WRITE_RESTRICTED, WinRestrictedCodeSid, WinUntrustedLabelSid,
    },
    Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE},
    System::{
        Com::CoTaskMemFree,
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            JOB_OBJECT_LIMIT_PROCESS_TIME, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
        },
        Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc},
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
            CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
            GetCurrentProcess, GetExitCodeProcess, InitializeProcThreadAttributeList,
            OpenProcessToken, PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ResumeThread,
            STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
        },
    },
};

#[link(name = "userenv")]
unsafe extern "system" {
    fn GetAppContainerFolderPath(app_container_sid: *const u16, path: *mut *mut u16) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: HANDLE,
    object_name: *const UnicodeString,
    attributes: u32,
    security_descriptor: *const std::ffi::c_void,
    security_quality_of_service: *const std::ffi::c_void,
}

#[repr(C)]
struct IoStatusBlock {
    // Union of NTSTATUS and PVOID; pointer-sized either way.
    status: isize,
    information: usize,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    #[allow(clippy::too_many_arguments)]
    fn NtCreateFile(
        file_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *const ObjectAttributes,
        io_status_block: *mut IoStatusBlock,
        allocation_size: *const i64,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
        ea_buffer: *const std::ffi::c_void,
        ea_length: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

const APP_LIMIT_BYTES: usize = 64 * 1024 * 1024;
const JOB_USER_TIME_100NS: i64 = 10_000_000; // 1 second
const WORKER_EXIT_MARKER_ENV_VERIFIED: u32 = 0x6000_0000;
const WORKER_EXIT_MARKER_ENV_REJECTED: u32 = 0x7000_0000;
const NETWORK_POLICY_MARKER: u32 = 0x4000_0000;
const NET_PROBE_MARKER: u32 = 0x5000_0000;
// UpdateProcThreadAttribute: opts the process out of ALL APPLICATION PACKAGES,
// producing a Less Privileged AppContainer (LPAC). Not exported by windows-sys.
const PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT: u32 = 0x01;
const AFD_PROBE_MARKER: u32 = 0x3000_0000;
// Kernel socket provider, by native NT path. Winsock opens a file on this
// device for every socket, so an access check failure here denies sockets
// beneath any user-mode library. The trailing component names the open only.
const AFD_DEVICE_PATH: &str = r"\Device\Afd\PersephoneProbe";
const ERROR_ACCESS_DENIED: u16 = 5;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;
const FILE_OPEN: u32 = 1;
const SYNCHRONIZE: u32 = 0x0010_0000;
// Windows retries a refused SYN before reporting WSAECONNREFUSED; the host
// closed-port control measured about two seconds. Worker attempts use the same
// window so a silent drop and a refusal are compared like for like.
const NET_PROBE_TIMEOUT: Duration = Duration::from_millis(3000);
const NET_PROBE_ELAPSED_UNIT_MS: u32 = 100;
const WSAEACCES: u16 = 10013;
const WSAETIMEDOUT: u16 = 10060;
const WSAECONNREFUSED: u16 = 10061;
const NET_STAGE_EXCHANGED: u32 = 0;
const NET_STAGE_CONNECT_FAILED: u32 = 1;
/// WSAStartup failed. Rust std asserts on that failure, so the probe calls it
/// first to report the code instead of exiting with a panic.
const NET_STAGE_SOCKET_INIT_FAILED: u32 = 8;
const NET_STAGE_DATAGRAM_SENT: u32 = 6;
const NET_STAGE_DATAGRAM_FAILED: u32 = 7;
const NET_KIND_TIMED_OUT: u32 = 1;
const NET_KIND_PERMISSION_DENIED: u32 = 2;
const LOOPBACK_CHALLENGE: &[u8] = b"PERSEPHONE-P0-SYNTHETIC-CHALLENGE";
const LOOPBACK_ACK: &[u8] = b"PERSEPHONE-P0-SYNTHETIC-ACK";
const NET_CODE_NONE: u16 = 0;
const NET_CODE_NO_OS_ERROR: u16 = 0xfffd;
const NET_CODE_PROTOCOL_MISMATCH: u16 = 0xfffe;
const READ_STATUS_DENIED: u32 = 0;
const READ_STATUS_ALLOWED: u32 = 1;
const READ_STATUS_UNEXPECTED: u32 = 2;
const ENV_FLAG_SECRET_CANARY: u32 = 1 << 0;
const ENV_FLAG_COMSPEC: u32 = 1 << 1;
const ENV_FLAG_TEMP: u32 = 1 << 2;
const ENV_FLAG_TMP: u32 = 1 << 3;
const ENV_FLAG_USERPROFILE: u32 = 1 << 4;
const ENV_FLAG_APPDATA: u32 = 1 << 5;
const ENV_FLAG_PROGRAMDATA: u32 = 1 << 6;
const ENV_FLAG_DRIVE_CURRENT_DIRECTORY: u32 = 1 << 7;
const ENV_FLAG_OTHER: u32 = 1 << 8;
const ENV_OTHER_COUNT_SHIFT: u32 = 9;
const ENV_FLAG_LOCALAPPDATA_MATCHES_EXPECTED: u32 = 1 << 13;
const ENV_FLAG_TEMP_MATCHES_EXPECTED: u32 = 1 << 14;
const ENV_FLAG_TMP_MATCHES_EXPECTED: u32 = 1 << 15;
const ENV_FLAG_LOCALAPPDATA_MATCHES_BROKER: u32 = 1 << 16;
const ENV_FLAG_TEMP_MATCHES_BROKER: u32 = 1 << 17;
const ENV_FLAG_TMP_MATCHES_BROKER: u32 = 1 << 18;
const PARENT_SECRET_CANARY_NAME: &str = "PERSEPHONE_PROBE_SYNTHETIC_SECRET_CANARY";
const PARENT_SECRET_CANARY_VALUE: &str = "synthetic-parent-secret-canary-do-not-inherit";
const EXTRA_OS_ENVIRONMENT: [&str; 7] = [
    "ComSpec",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "LOCALAPPDATA",
    "APPDATA",
    "ProgramData",
];

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct OwnedSid(*mut std::ffi::c_void);
impl Drop for OwnedSid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Security::FreeSid(self.0);
            }
        }
    }
}

struct OwnedLocalString(*mut u16);
impl Drop for OwnedLocalString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0.cast());
            }
        }
    }
}

struct OwnedCoTaskString(*mut u16);
impl Drop for OwnedCoTaskString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CoTaskMemFree(self.0.cast());
            }
        }
    }
}

struct OwnedAttributes(*mut std::ffi::c_void);
impl Drop for OwnedAttributes {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                DeleteProcThreadAttributeList(self.0);
            }
        }
    }
}

struct ListenerGuard {
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl ListenerGuard {
    fn stop_and_join(&mut self) -> bool {
        self.stopped.store(true, Ordering::Relaxed);
        self.thread
            .take()
            .is_none_or(|thread| thread.join().is_ok())
    }
}
impl Drop for ListenerGuard {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

/// Controlled TCP endpoint. Every accepted connection is counted, whether or
/// not the challenge completes, so endpoint-side observation is the ground
/// truth for whether any worker connection was established.
struct ChallengeListener {
    address: SocketAddr,
    accepts: Arc<AtomicUsize>,
    exchanges: Arc<AtomicUsize>,
    guard: ListenerGuard,
}
impl ChallengeListener {
    fn bind(address: SocketAddr) -> std::io::Result<Self> {
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stopped = Arc::new(AtomicBool::new(false));
        let accepts = Arc::new(AtomicUsize::new(0));
        let exchanges = Arc::new(AtomicUsize::new(0));
        let (stop, thread_accepts, thread_exchanges) = (
            Arc::clone(&stopped),
            Arc::clone(&accepts),
            Arc::clone(&exchanges),
        );
        let thread = thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        thread_accepts.fetch_add(1, Ordering::Relaxed);
                        // Accepted sockets inherit non-blocking mode on Windows.
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                        let mut challenge = vec![0; LOOPBACK_CHALLENGE.len()];
                        if stream.read_exact(&mut challenge).is_ok()
                            && challenge == LOOPBACK_CHALLENGE
                            && stream.write_all(LOOPBACK_ACK).is_ok()
                        {
                            thread_exchanges.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Ok(Self {
            address,
            accepts,
            exchanges,
            guard: ListenerGuard {
                stopped,
                thread: Some(thread),
            },
        })
    }
}

/// Controlled UDP endpoint counting every received datagram.
struct DatagramReceiver {
    address: SocketAddr,
    datagrams: Arc<AtomicUsize>,
    guard: ListenerGuard,
}
impl DatagramReceiver {
    fn bind(address: SocketAddr) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(address)?;
        socket.set_read_timeout(Some(Duration::from_millis(10)))?;
        let address = socket.local_addr()?;
        let stopped = Arc::new(AtomicBool::new(false));
        let datagrams = Arc::new(AtomicUsize::new(0));
        let (stop, thread_datagrams) = (Arc::clone(&stopped), Arc::clone(&datagrams));
        let thread = thread::spawn(move || {
            let mut buffer = [0u8; 512];
            while !stop.load(Ordering::Relaxed) {
                // Any datagram, including a malformed one, counts as reaching
                // the endpoint. Errors such as ICMP-reset reports are ignored.
                if socket.recv_from(&mut buffer).is_ok() {
                    thread_datagrams.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        Ok(Self {
            address,
            datagrams,
            guard: ListenerGuard {
                stopped,
                thread: Some(thread),
            },
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContainerPolicy {
    AppContainer,
    /// Also opted out of ALL APPLICATION PACKAGES access grants.
    LessPrivileged,
    /// AppContainer launch whose base is a restricted version of the broker
    /// token (CreateRestrictedToken + untrusted integrity), launched with
    /// CreateProcessAsUserW. The restricting-SID set does not grant the AFD
    /// device, so a write-restricted access check should fail there.
    RestrictedToken,
}

// SID_AND_ATTRIBUTES.Attributes value marking a mandatory-label SID. Lives in
// Win32_System_SystemServices, a feature this crate does not otherwise need.
const SE_GROUP_INTEGRITY: u32 = 0x20;
// RESTRICTED (S-1-5-12) is the sole restricting SID. It appears on no device
// DACL that grants this worker write access, so under WRITE_RESTRICTED the
// GENERIC_WRITE open of \Device\Afd must satisfy a SID the worker lacks.
// Whether the restricted-token policy also drops integrity to Untrusted. The
// LowBox worker fails DLL initialization (STATUS_DLL_INIT_FAILED, 0xC0000142)
// before reaching any probe both with and without this drop, so the restricting
// SID set alone is the decisive cause; the drop is retained to represent the
// full restricted policy the gate was asked to test. See
// docs/evidence/windows-sandbox.md.
const RESTRICTED_TOKEN_UNTRUSTED_INTEGRITY: bool = true;

struct ProbeLauncher<'a> {
    profile: &'a str,
    exe: &'a Path,
    extra_environment: &'a [&'a str],
    policy: ContainerPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProbeOutcome {
    appcontainer: bool,
    stage: u32,
    code: u16,
    kind: u32,
    elapsed_ms: u32,
}

/// Result of opening the AFD device with read/write access from the worker,
/// relative to the same open from the unrestricted broker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceAccess {
    /// Worker open failed with ERROR_ACCESS_DENIED; the broker's did not.
    Denied,
    /// Worker obtained a handle.
    Opened,
    /// Neither result above, or the broker control was itself denied.
    Inconclusive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndpointShape {
    OpenStream,
    ClosedStream,
    Datagram,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndpointVerdict {
    /// The OS refused the operation with an access error.
    DeniedExplicit,
    /// The attempt failed silently and the endpoint observed nothing.
    Dropped,
    /// Worker traffic crossed the boundary: a connection, a datagram, or a
    /// closed-port refusal that only the endpoint's TCP stack could send.
    Reached,
    /// The worker result does not distinguish denial from other failure.
    Equivocal,
    /// A host positive control failed, so the endpoint proves nothing.
    ControlFailed,
    /// The probe process was not running with an AppContainer token.
    NotAContainer,
}

struct EndpointEvidence {
    name: &'static str,
    shape: EndpointShape,
    required: bool,
    controls_passed: bool,
    control_detail: String,
    endpoint_worker_observations: usize,
    worker: ProbeOutcome,
}

struct CanaryFiles(Vec<PathBuf>);
impl CanaryFiles {
    fn create(&mut self, path: &Path, contents: &str) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| format!("create synthetic canary failed: {error}"))?;
        self.0.push(path.to_path_buf());
        file.write_all(contents.as_bytes())
            .map_err(|error| format!("write synthetic canary failed: {error}"))
    }
}

struct BrokerEnvironment<'a> {
    expected_local_data: &'a Path,
    broker_local_data: &'a Path,
    broker_temp: &'a Path,
    broker_tmp: &'a Path,
}
impl Drop for CanaryFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

struct SyntheticEnvironmentCanary;
impl Drop for SyntheticEnvironmentCanary {
    fn drop(&mut self) {
        unsafe {
            env::remove_var(PARENT_SECRET_CANARY_NAME);
        }
    }
}

pub(crate) fn run() {
    let result = if env::args_os().nth(1).as_deref() == Some(OsStr::new("--worker")) {
        worker_main()
    } else if env::args_os().nth(1).as_deref() == Some(OsStr::new("--hold")) {
        loop {
            std::hint::spin_loop();
        }
    } else if env::args_os().nth(1).as_deref() == Some(OsStr::new("--noop")) {
        Ok(())
    } else if env::args_os().nth(1).as_deref() == Some(OsStr::new("--env-report")) {
        std::process::exit(unexpected_environment_flags() as i32)
    } else if env::args_os().nth(1).as_deref() == Some(OsStr::new("--network-policy")) {
        std::process::exit(network_policy_status() as i32)
    } else if env::args_os().nth(1).as_deref() == Some(OsStr::new("--net-probe")) {
        std::process::exit(net_probe_main() as i32)
    } else if env::args_os().nth(1).as_deref() == Some(OsStr::new("--afd-probe")) {
        std::process::exit(afd_probe_exit() as i32)
    } else if env::args_os().nth(1).as_deref() == Some(OsStr::new("--diagnose-env")) {
        diagnose_environment()
    } else {
        parent_main()
    };
    if let Err(error) = result {
        eprintln!("probe error: {error}");
        std::process::exit(1);
    }
}

fn parent_main() -> Result<(), String> {
    if env::var_os(PARENT_SECRET_CANARY_NAME).is_some() {
        return Err(
            "synthetic secret canary variable already exists; refusing to overwrite".into(),
        );
    }
    unsafe {
        env::set_var(PARENT_SECRET_CANARY_NAME, PARENT_SECRET_CANARY_VALUE);
    }
    let _secret_canary_guard = SyntheticEnvironmentCanary;
    let root = PathBuf::from(env::var_os("PERSEPHONE_PROBE_DIR").unwrap_or_else(|| {
        env::temp_dir()
            .join("persephone-p0-platform")
            .into_os_string()
    }));
    fs::create_dir_all(&root).map_err(err)?;
    let canary = root.join("synthetic-canary.txt");
    let store_canary = root.join("synthetic-store-canary.txt");
    let mut canary_files = CanaryFiles(Vec::new());
    canary_files.create(&canary, "synthetic canary: worker must not read this\n")?;
    canary_files.create(
        &store_canary,
        "synthetic store canary: worker must not read this\n",
    )?;

    // Positive controls: the same host identity can read both generated canaries,
    // reach a loopback listener, and create a harmless probe child.
    let canary_control = fs::read(&canary).is_ok();
    let store_control = fs::read(&store_canary).is_ok();
    let mut listener =
        ChallengeListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).map_err(err)?;
    let address = listener.address;
    let accepted_connections = Arc::clone(&listener.accepts);
    let completed_exchanges = Arc::clone(&listener.exchanges);
    let network_control_before = exchange_challenge(&address).is_ok();
    if network_control_before {
        wait_for_connections(&completed_exchanges, 1)?;
    }
    let child_control = Command::new(env::current_exe().map_err(err)?)
        .arg("--noop")
        .spawn()
        .and_then(|mut c| c.wait())
        .map(|s| s.success())
        .unwrap_or(false);

    if !(canary_control && store_control && network_control_before && child_control) {
        return Err(format!(
            "positive control failed: canary={canary_control}, store={store_control}, network_before={network_control_before}, child={child_control}"
        ));
    }

    let extra_environment = if env::var_os("LOCALAPPDATA").is_some() {
        vec!["LOCALAPPDATA"]
    } else {
        return Err("required OS runtime variable LOCALAPPDATA is unavailable".into());
    };
    let broker_local_data = PathBuf::from(env::var_os("LOCALAPPDATA").unwrap());
    let broker_temp = env::var_os("TEMP")
        .map(PathBuf::from)
        .ok_or("required OS runtime variable TEMP is unavailable")?;
    let broker_tmp = env::var_os("TMP")
        .map(PathBuf::from)
        .ok_or("required OS runtime variable TMP is unavailable")?;
    let profile = format!("Persephone.P0.Probe.{}", std::process::id());
    create_temporary_profile(&profile)?;
    let run = (|| {
        let worker_exe = env::current_exe().map_err(err)?;
        let expected_local_data = appcontainer_local_data_path(&profile)?;
        let report = launch_worker(
            &profile,
            &worker_exe,
            &canary,
            &store_canary,
            address.port(),
            &extra_environment,
            &BrokerEnvironment {
                expected_local_data: &expected_local_data,
                broker_local_data: &broker_local_data,
                broker_temp: &broker_temp,
                broker_tmp: &broker_tmp,
            },
        )?;
        // Let the listener finish any worker challenge exchange before the
        // second host positive control is counted.
        thread::sleep(Duration::from_millis(200));
        let network_control_after = exchange_challenge(&address).is_ok();
        if network_control_after {
            wait_for_connections(&completed_exchanges, 2)?;
        }
        println!("Windows AppContainer probe (synthetic data only)");
        println!(
            "positive_controls: read_canary={canary_control}, read_store_canary={store_control}, loopback_challenge_before={network_control_before}, loopback_challenge_after_same_listener={network_control_after}, accepts={}, completed_challenges={}, child_process={child_control}",
            accepted_connections.load(Ordering::Relaxed),
            completed_exchanges.load(Ordering::Relaxed)
        );
        println!("worker_report: {report}");

        // Read-only capability diagnosis supplements the actual socket attempt.
        // It does not turn an inconclusive connect timeout into a denial pass.
        println!(
            "broker_network_policy: {}",
            format_network_policy(network_policy_status())?
        );
        let policy = launch(
            &profile,
            &worker_exe,
            &[OsStr::new("--network-policy")],
            false,
            &extra_environment,
        )?;
        println!("worker_network_policy: {policy}");

        // The combined worker's loopback field is the client-side view only. A
        // silent timeout there is never a pass by itself; denial is decided by
        // the endpoint-observed differential suite below.
        // The gate uses the AppContainer policy that every other check above ran
        // under. The LPAC suite is comparative evidence for the worker design;
        // it cannot pass this gate for a configuration the other checks skipped.
        // AppContainer runs first and is shown below to create sockets, so the
        // device probe must report Opened there. Otherwise a later Denied
        // result could reflect the probe path rather than the device policy.
        let mut device_probe_validated = false;
        let probe_launcher = |policy| ProbeLauncher {
            profile: &profile,
            exe: &worker_exe,
            extra_environment: &extra_environment,
            policy,
        };
        // The gate uses the AppContainer policy that every other check above ran
        // under. The LPAC and restricted-token suites are comparative evidence
        // for the worker design; they cannot pass this gate for a configuration
        // the other checks skipped.
        let network_denied = evaluate_policy(
            "appcontainer",
            &probe_launcher(ContainerPolicy::AppContainer),
            &mut device_probe_validated,
        )?;
        evaluate_policy(
            "lpac",
            &probe_launcher(ContainerPolicy::LessPrivileged),
            &mut device_probe_validated,
        )?;
        // A restricted-token launch may fail outright (the AppContainer base and
        // a restricted token can be mutually exclusive). Report that as evidence
        // rather than aborting the AppContainer gate already printed above.
        let restricted_denied = match evaluate_policy(
            "restricted_token",
            &probe_launcher(ContainerPolicy::RestrictedToken),
            &mut device_probe_validated,
        ) {
            Ok(denied) => denied,
            Err(error) => {
                println!("socket_device[restricted_token]: POLICY_NO_WORKER_RESULT({error})");
                println!(
                    "network_denial[restricted_token]: NOT_ESTABLISHED(no_running_worker_under_restricted_token)"
                );
                false
            }
        };

        let time_limit = launch_time_limit_probe(&profile, &worker_exe, &extra_environment)?;
        println!("job_cpu_time_limit: {time_limit}");
        println!(
            "listener_after_deadline_probe: accepts={},completed_challenges={}",
            accepted_connections.load(Ordering::Relaxed),
            completed_exchanges.load(Ordering::Relaxed)
        );

        // If, and only if, the restricted-token policy denied networking, run
        // the full combined worker and deadline checks under it and report
        // whether the complete gate could pass with that policy. This never
        // relaxes the AppContainer gate below.
        if restricted_denied {
            match restricted_token_full_gate(
                &profile,
                &worker_exe,
                &canary,
                &store_canary,
                address.port(),
                &extra_environment,
                &BrokerEnvironment {
                    expected_local_data: &expected_local_data,
                    broker_local_data: &broker_local_data,
                    broker_temp: &broker_temp,
                    broker_tmp: &broker_tmp,
                },
            ) {
                Ok((report, time_limit)) => {
                    println!("restricted_token_combined_worker: {report}");
                    println!("restricted_token_time_limit: {time_limit}");
                    println!(
                        "restricted_token_full_gate: {}",
                        if worker_gate_assertions_pass(&report, restricted_denied, &time_limit) {
                            "WOULD_PASS"
                        } else {
                            "WOULD_FAIL"
                        }
                    );
                }
                Err(error) => {
                    println!("restricted_token_full_gate: NOT_EVALUATED({error})");
                }
            }
        } else {
            println!(
                "restricted_token_full_gate: NOT_RUN(network_denial_not_established_under_restricted_token)"
            );
        }

        if worker_gate_assertions_pass(&report, network_denied, &time_limit)
            && network_control_after
            && accepted_connections.load(Ordering::Relaxed) == 2
            && completed_exchanges.load(Ordering::Relaxed) == 2
        {
            Ok(())
        } else {
            Err("one or more sandbox assertions failed; see worker_report".into())
        }
    })();
    let listener_stopped_cleanly = listener.guard.stop_and_join();
    let deleted = delete_temporary_profile(&profile);
    if !deleted {
        return Err("temporary AppContainer profile cleanup failed".into());
    }
    if !listener_stopped_cleanly {
        return Err("loopback listener thread did not stop cleanly".into());
    }
    run
}

/// Runs the AFD device probe and the network-denial suite under one policy,
/// prints the per-policy evidence lines, and returns whether network denial was
/// established. The AppContainer run must set `device_probe_validated`; a later
/// policy's device-access `Denied` is credited only once that control opened
/// the device, so a probe-path artifact cannot masquerade as a denial.
fn evaluate_policy(
    label: &str,
    launcher: &ProbeLauncher<'_>,
    device_probe_validated: &mut bool,
) -> Result<bool, String> {
    let (mut device, device_detail) = afd_device_access(launcher)?;
    let network_evidence = network_denial_suite(launcher)?;
    if launcher.policy == ContainerPolicy::AppContainer {
        *device_probe_validated = device == DeviceAccess::Opened
            && network_evidence
                .iter()
                .any(|item| classify_endpoint(item, device) == EndpointVerdict::Reached);
    } else if device == DeviceAccess::Denied && !*device_probe_validated {
        device = DeviceAccess::Inconclusive;
    }
    println!(
        "socket_device[{label}]: {device_detail},probe_validated_by_appcontainer_socket_control={},effective={device:?}",
        *device_probe_validated
    );
    for evidence in &network_evidence {
        println!(
            "network_endpoint[{label}]: {}",
            format_endpoint_evidence(evidence, device)
        );
    }
    let denied = network_denial_established(&network_evidence, device);
    println!(
        "network_denial[{label}]: {}",
        if denied {
            "DENIED(endpoint_observed_no_worker_traffic,closed_port_differential=CONFIRMED)"
        } else {
            "NOT_ESTABLISHED"
        }
    );
    Ok(denied)
}

/// Runs the combined worker and the deadline probe under the restricted-token
/// policy and returns their reports, so the broker can decide whether the whole
/// gate would pass there. Used only after that policy denied networking.
#[allow(clippy::too_many_arguments)]
fn restricted_token_full_gate(
    profile: &str,
    exe: &Path,
    canary: &Path,
    store_canary: &Path,
    port: u16,
    extra_environment: &[&str],
    broker_environment: &BrokerEnvironment<'_>,
) -> Result<(String, String), String> {
    let report = launch_with_policy(
        profile,
        exe,
        &[
            OsStr::new("--worker"),
            canary.as_os_str(),
            store_canary.as_os_str(),
            OsStr::new(&port.to_string()),
            broker_environment.expected_local_data.as_os_str(),
            broker_environment.broker_local_data.as_os_str(),
            broker_environment.broker_temp.as_os_str(),
            broker_environment.broker_tmp.as_os_str(),
        ],
        false,
        extra_environment,
        ContainerPolicy::RestrictedToken,
    )?;
    let time_limit = launch_with_policy(
        profile,
        exe,
        &[OsStr::new("--hold")],
        true,
        extra_environment,
        ContainerPolicy::RestrictedToken,
    )?;
    Ok((report, time_limit))
}

/// The worker-intrinsic half of the gate: environment, storage, file, child,
/// token, memory, network-denial and deadline assertions that must all hold.
/// The broker adds the host-side listener/control counts separately. Factoring
/// this out lets the same bar judge the AppContainer and restricted-token runs.
fn worker_gate_assertions_pass(report: &str, network_denied: bool, time_limit: &str) -> bool {
    report.contains("canary_read=DENIED_ACCESS(5)")
        && report.contains("store_read=DENIED_ACCESS(5)")
        && report.contains("parent_secret_env=ABSENT")
        && report.contains("appcontainer_storage_env=VERIFIED")
        && network_denied
        // Only connect-stage failures report timeout_setup=NOT_ATTEMPTED;
        // any later stage means a connection was established.
        && (report.contains("loopback=DENIED") || report.contains("loopback=EQUIVOCAL"))
        && report.contains("timeout_setup=NOT_ATTEMPTED")
        && report.contains("child=DENIED,child_error=ERROR_NOT_ENOUGH_QUOTA(1816)")
        && report.contains("token_appcontainer=true")
        && report.contains("memory_limit=ENFORCED")
        && (time_limit.contains("job_cpu_time_limit=ENFORCED")
            || time_limit.contains("broker_wall_deadline=ENFORCED"))
}

fn wait_for_connections(count: &AtomicUsize, expected: usize) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if count.load(Ordering::Relaxed) >= expected {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(format!(
        "loopback listener did not accept control connection {expected}"
    ))
}

/// Runs one contained worker per controlled endpoint. Each endpoint has host
/// positive controls before and after the worker attempt, and counts what it
/// actually receives, so the verdict does not rest on the worker's error alone.
fn network_denial_suite(launcher: &ProbeLauncher<'_>) -> Result<Vec<EndpointEvidence>, String> {
    let mut evidence = Vec::new();

    let mut open_v4 =
        ChallengeListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).map_err(err)?;
    evidence.push(probe_open_stream(
        "loopback_tcp_v4_open",
        true,
        &open_v4,
        launcher,
    )?);
    if !open_v4.guard.stop_and_join() {
        return Err("IPv4 endpoint listener did not stop cleanly".into());
    }

    // Reserve an ephemeral port, then release it so connections must be
    // refused by the TCP stack. Host controls before and after confirm that.
    let closed_port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .map_err(err)?
        .port();
    evidence.push(probe_closed_stream(
        "loopback_tcp_v4_closed",
        true,
        SocketAddr::from((Ipv4Addr::LOCALHOST, closed_port)),
        launcher,
    )?);

    let mut datagram_v4 =
        DatagramReceiver::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).map_err(err)?;
    evidence.push(probe_datagram(
        "loopback_udp_v4",
        true,
        &datagram_v4,
        launcher,
    )?);
    if !datagram_v4.guard.stop_and_join() {
        return Err("UDP endpoint receiver did not stop cleanly".into());
    }

    match ChallengeListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, 0))) {
        Ok(mut open_v6) => {
            evidence.push(probe_open_stream(
                "loopback_tcp_v6_open",
                false,
                &open_v6,
                launcher,
            )?);
            if !open_v6.guard.stop_and_join() {
                return Err("IPv6 endpoint listener did not stop cleanly".into());
            }
        }
        Err(_) => println!("network_endpoint: name=loopback_tcp_v6_open,NOT_AVAILABLE"),
    }

    // A closed port on this host's own routed address covers the non-loopback
    // path without opening a listener there, which could invoke firewall UI.
    match local_interface_ipv4() {
        Some(ip) => evidence.push(probe_closed_stream(
            "local_interface_tcp_v4_closed",
            false,
            SocketAddr::from((ip, closed_port)),
            launcher,
        )?),
        None => println!("network_endpoint: name=local_interface_tcp_v4_closed,NOT_AVAILABLE"),
    }
    Ok(evidence)
}

fn probe_open_stream(
    name: &'static str,
    required: bool,
    listener: &ChallengeListener,
    launcher: &ProbeLauncher<'_>,
) -> Result<EndpointEvidence, String> {
    let before = exchange_challenge(&listener.address).is_ok()
        && wait_for_connections(&listener.exchanges, 1).is_ok();
    let worker = launch_net_probe(launcher, "tcp", listener.address)?;
    thread::sleep(Duration::from_millis(200));
    let after = exchange_challenge(&listener.address).is_ok()
        && wait_for_connections(&listener.exchanges, 2).is_ok();
    let host_accepts = usize::from(before) + usize::from(after);
    Ok(EndpointEvidence {
        name,
        shape: EndpointShape::OpenStream,
        required,
        controls_passed: before && after,
        control_detail: format!("host_challenge_before={before},host_challenge_after={after}"),
        endpoint_worker_observations: listener
            .accepts
            .load(Ordering::Relaxed)
            .saturating_sub(host_accepts),
        worker,
    })
}

fn probe_closed_stream(
    name: &'static str,
    required: bool,
    address: SocketAddr,
    launcher: &ProbeLauncher<'_>,
) -> Result<EndpointEvidence, String> {
    let (before, before_ms) = host_connect_refused(&address);
    let worker = launch_net_probe(launcher, "tcp", address)?;
    let (after, after_ms) = host_connect_refused(&address);
    Ok(EndpointEvidence {
        name,
        shape: EndpointShape::ClosedStream,
        required,
        controls_passed: before && after,
        control_detail: format!(
            "host_refused_before={before}({before_ms}ms),host_refused_after={after}({after_ms}ms)"
        ),
        // Nothing listens here; a refusal reaching the worker is the signal.
        endpoint_worker_observations: 0,
        worker,
    })
}

fn probe_datagram(
    name: &'static str,
    required: bool,
    receiver: &DatagramReceiver,
    launcher: &ProbeLauncher<'_>,
) -> Result<EndpointEvidence, String> {
    let before = host_send_datagram(&receiver.address)
        && wait_for_connections(&receiver.datagrams, 1).is_ok();
    let worker = launch_net_probe(launcher, "udp", receiver.address)?;
    thread::sleep(Duration::from_millis(200));
    let after = host_send_datagram(&receiver.address)
        && wait_for_connections(&receiver.datagrams, 2).is_ok();
    let host_datagrams = usize::from(before) + usize::from(after);
    Ok(EndpointEvidence {
        name,
        shape: EndpointShape::Datagram,
        required,
        controls_passed: before && after,
        control_detail: format!("host_datagram_before={before},host_datagram_after={after}"),
        endpoint_worker_observations: receiver
            .datagrams
            .load(Ordering::Relaxed)
            .saturating_sub(host_datagrams),
        worker,
    })
}

fn host_connect_refused(address: &SocketAddr) -> (bool, u128) {
    let started = Instant::now();
    let refused = matches!(
        TcpStream::connect_timeout(address, NET_PROBE_TIMEOUT),
        Err(error) if error.raw_os_error() == Some(WSAECONNREFUSED.into())
    );
    (refused, started.elapsed().as_millis())
}

fn host_send_datagram(address: &SocketAddr) -> bool {
    let local: SocketAddr = if address.is_ipv4() {
        (Ipv4Addr::LOCALHOST, 0).into()
    } else {
        (Ipv6Addr::LOCALHOST, 0).into()
    };
    UdpSocket::bind(local)
        .and_then(|socket| socket.send_to(LOOPBACK_CHALLENGE, address))
        .is_ok_and(|sent| sent == LOOPBACK_CHALLENGE.len())
}

/// Returns this host's routed IPv4 source address. UDP connect selects a route
/// without sending a datagram; 192.0.2.1 is TEST-NET-1 (RFC 5737).
fn local_interface_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

fn launch_net_probe(
    launcher: &ProbeLauncher<'_>,
    protocol: &str,
    address: SocketAddr,
) -> Result<ProbeOutcome, String> {
    let address = address.to_string();
    let exit = launch_with_policy(
        launcher.profile,
        launcher.exe,
        &[
            OsStr::new("--net-probe"),
            OsStr::new(protocol),
            OsStr::new(&address),
        ],
        false,
        launcher.extra_environment,
        launcher.policy,
    )?;
    decode_probe(exit.parse().map_err(err)?)
}

fn classify_endpoint(evidence: &EndpointEvidence, device: DeviceAccess) -> EndpointVerdict {
    let worker = &evidence.worker;
    if !worker.appcontainer {
        return EndpointVerdict::NotAContainer;
    }
    if !evidence.controls_passed {
        return EndpointVerdict::ControlFailed;
    }
    if evidence.endpoint_worker_observations > 0 {
        return EndpointVerdict::Reached;
    }
    if worker.stage == NET_STAGE_SOCKET_INIT_FAILED {
        // A user-mode Winsock failure alone could be a catalog or registry
        // problem that direct device I/O bypasses. It is an explicit denial
        // only when the kernel socket device itself refused this worker.
        return if device == DeviceAccess::Denied {
            EndpointVerdict::DeniedExplicit
        } else {
            EndpointVerdict::Equivocal
        };
    }
    let explicit = worker.code == WSAEACCES || worker.kind == NET_KIND_PERMISSION_DENIED;
    let timed_out = worker.code == WSAETIMEDOUT || worker.kind == NET_KIND_TIMED_OUT;
    match (evidence.shape, worker.stage) {
        (EndpointShape::OpenStream | EndpointShape::ClosedStream, NET_STAGE_CONNECT_FAILED) => {
            if explicit {
                EndpointVerdict::DeniedExplicit
            } else if timed_out {
                EndpointVerdict::Dropped
            } else if evidence.shape == EndpointShape::ClosedStream
                && worker.code == WSAECONNREFUSED
            {
                // Only the endpoint's TCP stack sends this reset.
                EndpointVerdict::Reached
            } else {
                EndpointVerdict::Equivocal
            }
        }
        // Stages 0 and 2–5 all follow an established connection.
        (EndpointShape::OpenStream | EndpointShape::ClosedStream, NET_STAGE_EXCHANGED..=5) => {
            EndpointVerdict::Reached
        }
        (EndpointShape::Datagram, NET_STAGE_DATAGRAM_FAILED) if explicit => {
            EndpointVerdict::DeniedExplicit
        }
        // UDP gives the sender no delivery signal; the receiver count above
        // is the evidence that the datagram was dropped.
        (EndpointShape::Datagram, NET_STAGE_DATAGRAM_SENT) => EndpointVerdict::Dropped,
        _ => EndpointVerdict::Equivocal,
    }
}

/// Network denial passes only when every required endpoint, and every optional
/// endpoint whose controls ran, shows explicit denial or a silent drop with no
/// endpoint-side observation. A required closed-port discriminator must also
/// pass: a host connection there is refused, so a worker that sees no refusal
/// had its packets filtered before they reached the TCP stack.
fn network_denial_established(evidence: &[EndpointEvidence], device: DeviceAccess) -> bool {
    let denied = |verdict| {
        matches!(
            verdict,
            EndpointVerdict::DeniedExplicit | EndpointVerdict::Dropped
        )
    };
    let required_shapes = [
        EndpointShape::OpenStream,
        EndpointShape::ClosedStream,
        EndpointShape::Datagram,
    ];
    required_shapes.iter().all(|shape| {
        evidence.iter().any(|item| {
            item.required && item.shape == *shape && denied(classify_endpoint(item, device))
        })
    }) && evidence.iter().all(|item| {
        let verdict = classify_endpoint(item, device);
        denied(verdict) || (!item.required && verdict == EndpointVerdict::ControlFailed)
    })
}

fn format_endpoint_evidence(evidence: &EndpointEvidence, device: DeviceAccess) -> String {
    let worker = &evidence.worker;
    format!(
        "name={},required={},{},endpoint_worker_observations={},worker_appcontainer={},worker_stage={},worker_error={},worker_elapsed_ms~{},verdict={:?}",
        evidence.name,
        evidence.required,
        evidence.control_detail,
        evidence.endpoint_worker_observations,
        worker.appcontainer,
        worker.stage,
        format_network_error(worker.code.into(), worker.kind),
        worker.elapsed_ms,
        classify_endpoint(evidence, device)
    )
}

// Layout below the marker nibble: bit 0 non-container token, bits 1–4 stage,
// bits 5–20 error code, bits 21–22 error kind, bits 23–27 elapsed units.
fn encode_probe(outcome: ProbeOutcome) -> u32 {
    NET_PROBE_MARKER
        | u32::from(!outcome.appcontainer)
        | ((outcome.stage & 0xf) << 1)
        | (u32::from(outcome.code) << 5)
        | ((outcome.kind & 0x3) << 21)
        | ((outcome.elapsed_ms / NET_PROBE_ELAPSED_UNIT_MS).min(0x1f) << 23)
}

fn decode_probe(exit: u32) -> Result<ProbeOutcome, String> {
    if exit & 0xf000_0000 != NET_PROBE_MARKER {
        return Err(format!(
            "network probe marker missing: exit={exit}{}",
            describe_unexpected_exit(exit)
        ));
    }
    Ok(ProbeOutcome {
        appcontainer: exit & 1 == 0,
        stage: (exit >> 1) & 0xf,
        code: ((exit >> 5) & 0xffff) as u16,
        kind: (exit >> 21) & 0x3,
        elapsed_ms: ((exit >> 23) & 0x1f) * NET_PROBE_ELAPSED_UNIT_MS,
    })
}

/// Opens the AFD device requesting read/write access and returns the marker,
/// bit 0 set for a non-container token, bit 1 set if a handle was obtained,
/// and the Win32 error in bits 2–17.
fn afd_probe_exit() -> u32 {
    let appcontainer = token_is_appcontainer().unwrap_or(false);
    let (opened, error) = open_afd_device();
    AFD_PROBE_MARKER | u32::from(!appcontainer) | (u32::from(opened) << 1) | (u32::from(error) << 2)
}

/// Opens an AFD handle through the native NT path, as Winsock does, rather
/// than through Win32 device-path translation, which AppContainers cannot use
/// for this device. Returns a Win32 error code converted from the NTSTATUS.
fn open_afd_device() -> (bool, u16) {
    let mut name: Vec<u16> = AFD_DEVICE_PATH.encode_utf16().collect();
    let bytes = (name.len() * 2) as u16;
    let unicode_name = UnicodeString {
        length: bytes,
        maximum_length: bytes,
        buffer: name.as_mut_ptr(),
    };
    let attributes = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: null_mut(),
        object_name: &unicode_name,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: null(),
        security_quality_of_service: null(),
    };
    let mut handle: HANDLE = null_mut();
    let mut io_status = IoStatusBlock {
        status: 0,
        information: 0,
    };
    // Desired access must be nonzero: an open requesting no access skips the
    // device's discretionary access check. These are a socket's open rights.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE,
            &attributes,
            &mut io_status,
            null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
            0,
            null(),
            0,
        )
    };
    if status < 0 {
        let error = unsafe { RtlNtStatusToDosError(status) };
        return (false, u16::try_from(error).unwrap_or(u16::MAX));
    }
    unsafe {
        CloseHandle(handle);
    }
    (true, 0)
}

fn decode_afd_probe(exit: u32) -> Result<(bool, bool, u16), String> {
    if exit & 0xf000_0000 != AFD_PROBE_MARKER {
        return Err(format!(
            "AFD probe marker missing: exit={exit}{}",
            describe_unexpected_exit(exit)
        ));
    }
    Ok((exit & 1 == 0, exit & 2 != 0, ((exit >> 2) & 0xffff) as u16))
}

/// Names the common process-crash NTSTATUS values that appear as an exit code
/// when a worker never reaches its encoded result, so a marker-missing error is
/// self-explanatory. The high nibble 0xC marks an NTSTATUS error facility.
fn describe_unexpected_exit(exit: u32) -> String {
    let name = match exit {
        0xC000_0142 => Some("STATUS_DLL_INIT_FAILED; worker process failed to initialize"),
        0xC000_0135 => Some("STATUS_DLL_NOT_FOUND"),
        0xC000_0022 => Some("STATUS_ACCESS_DENIED"),
        0xC000_0017 => Some("STATUS_NO_MEMORY"),
        _ => None,
    };
    match name {
        Some(name) => format!(" ({exit:#010x} {name})"),
        None if exit & 0xC000_0000 == 0xC000_0000 => {
            format!(" ({exit:#010x} NTSTATUS error; worker did not run to completion)")
        }
        None => String::new(),
    }
}

fn classify_device_access(
    host: (bool, u16),
    worker_appcontainer: bool,
    worker: (bool, u16),
) -> DeviceAccess {
    let host_denied = !host.0 && host.1 == ERROR_ACCESS_DENIED;
    if !worker_appcontainer || host_denied {
        DeviceAccess::Inconclusive
    } else if worker.0 {
        DeviceAccess::Opened
    } else if worker.1 == ERROR_ACCESS_DENIED {
        DeviceAccess::Denied
    } else {
        DeviceAccess::Inconclusive
    }
}

fn afd_device_access(launcher: &ProbeLauncher<'_>) -> Result<(DeviceAccess, String), String> {
    let host = open_afd_device();
    let exit = launch_with_policy(
        launcher.profile,
        launcher.exe,
        &[OsStr::new("--afd-probe")],
        false,
        launcher.extra_environment,
        launcher.policy,
    )?;
    let (appcontainer, opened, error) = decode_afd_probe(exit.parse().map_err(err)?)?;
    let access = classify_device_access(host, appcontainer, (opened, error));
    Ok((
        access,
        format!(
            "host_opened={},host_error={},worker_appcontainer={appcontainer},worker_opened={opened},worker_error={error},access={access:?}",
            host.0, host.1
        ),
    ))
}

/// Contained single-endpoint probe. Returns an encoded outcome; malformed
/// arguments return 1, which the broker rejects as a missing marker.
fn net_probe_main() -> u32 {
    let mut args = env::args_os().skip(2);
    let (Some(protocol), Some(address)) = (args.next(), args.next()) else {
        return 1;
    };
    let Some(address) = address.to_str().and_then(|value| value.parse().ok()) else {
        return 1;
    };
    let appcontainer = token_is_appcontainer().unwrap_or(false);
    let started = Instant::now();
    let mut winsock: WSADATA = unsafe { std::mem::zeroed() };
    let startup = unsafe { WSAStartup(0x0202, &mut winsock) };
    if startup != 0 {
        let code = u16::try_from(startup).unwrap_or(NET_CODE_NO_OS_ERROR);
        return encode_probe(ProbeOutcome {
            appcontainer,
            stage: NET_STAGE_SOCKET_INIT_FAILED,
            code,
            kind: if code == WSAEACCES {
                NET_KIND_PERMISSION_DENIED
            } else {
                3
            },
            elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        });
    }
    let (stage, code, kind) = match protocol.to_str() {
        Some("tcp") => attempt_challenge(&address, NET_PROBE_TIMEOUT),
        Some("udp") => attempt_datagram(&address),
        _ => return 1,
    };
    encode_probe(ProbeOutcome {
        appcontainer,
        stage,
        code,
        kind,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    })
}

fn attempt_challenge(address: &SocketAddr, timeout: Duration) -> (u32, u16, u32) {
    match TcpStream::connect_timeout(address, timeout) {
        Err(error) => {
            let (code, kind) = encode_io_error(&error);
            (NET_STAGE_CONNECT_FAILED, code, kind)
        }
        Ok(mut stream) => {
            let setup = stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(1))));
            if let Err(error) = setup {
                let (code, kind) = encode_io_error(&error);
                (4, code, kind)
            } else if let Err(error) = stream.write_all(LOOPBACK_CHALLENGE) {
                let (code, kind) = encode_io_error(&error);
                (2, code, kind)
            } else {
                let mut ack = vec![0; LOOPBACK_ACK.len()];
                match stream.read_exact(&mut ack) {
                    Err(error) => {
                        let (code, kind) = encode_io_error(&error);
                        (3, code, kind)
                    }
                    Ok(()) if ack == LOOPBACK_ACK => (NET_STAGE_EXCHANGED, NET_CODE_NONE, 0),
                    Ok(()) => (5, NET_CODE_PROTOCOL_MISMATCH, 0),
                }
            }
        }
    }
}

fn attempt_datagram(address: &SocketAddr) -> (u32, u16, u32) {
    let local: SocketAddr = if address.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    };
    match UdpSocket::bind(local).and_then(|socket| socket.send_to(LOOPBACK_CHALLENGE, address)) {
        Ok(_) => (NET_STAGE_DATAGRAM_SENT, NET_CODE_NONE, 0),
        Err(error) => {
            let (code, kind) = encode_io_error(&error);
            (NET_STAGE_DATAGRAM_FAILED, code, kind)
        }
    }
}

fn diagnose_environment() -> Result<(), String> {
    let executable = env::current_exe().map_err(err)?;
    let base = ["SystemDrive", "SystemRoot", "windir"]
        .into_iter()
        .filter(|name| env::var_os(name).is_some())
        .collect::<Vec<_>>();
    println!("environment_diagnostic: variable names only; values are not displayed");
    println!(
        "plain_child base_minimal: {}",
        plain_noop_with_environment(&executable, &base)
    );
    println!(
        "plain_child explicit_base_environment_report: {}",
        plain_environment_report(&executable, &base)
    );

    let mut present = Vec::new();
    for name in EXTRA_OS_ENVIRONMENT {
        if env::var_os(name).is_some() {
            let names = [base.clone(), vec![name]].concat();
            println!(
                "plain_child base_plus_{name}: {}",
                plain_noop_with_environment(&executable, &names)
            );
            present.push(name);
        } else {
            println!("plain_child base_plus_{name}: NOT_PRESENT");
        }
    }
    let cumulative = [base.clone(), present.to_vec()].concat();
    println!(
        "plain_child base_plus_all_present_OS_vars: {}",
        plain_noop_with_environment(&executable, &cumulative)
    );

    let profile = format!("Persephone.P0.EnvProbe.{}", std::process::id());
    create_temporary_profile(&profile)?;
    let appcontainer_base = launch(&profile, &executable, &[OsStr::new("--noop")], false, &[]);
    println!(
        "appcontainer_noop base_minimal: {}",
        appcontainer_base.unwrap_or_else(|error| error)
    );
    for name in &present {
        let appcontainer_single = launch(
            &profile,
            &executable,
            &[OsStr::new("--noop")],
            false,
            &[*name],
        );
        println!(
            "appcontainer_noop base_plus_{name}: {}",
            appcontainer_single.unwrap_or_else(|error| error)
        );
    }
    let without_local_app_data = present
        .iter()
        .copied()
        .filter(|name| *name != "LOCALAPPDATA")
        .collect::<Vec<_>>();
    for left in 0..without_local_app_data.len() {
        for right in left + 1..without_local_app_data.len() {
            let pair = [without_local_app_data[left], without_local_app_data[right]];
            let result = launch(&profile, &executable, &[OsStr::new("--noop")], false, &pair);
            println!(
                "appcontainer_noop base_plus_{}_{}: {}",
                pair[0],
                pair[1],
                result.unwrap_or_else(|error| error)
            );
        }
    }
    let appcontainer_cumulative = launch(
        &profile,
        &executable,
        &[OsStr::new("--noop")],
        false,
        &present,
    );
    println!(
        "appcontainer_noop base_plus_all_present_OS_vars: {}",
        appcontainer_cumulative.unwrap_or_else(|error| error)
    );
    if !delete_temporary_profile(&profile) {
        return Err("temporary diagnostic AppContainer profile cleanup failed".into());
    }
    Ok(())
}

fn plain_noop_with_environment(exe: &Path, names: &[&str]) -> String {
    plain_child_with_environment(exe, names, "--noop")
}

fn plain_environment_report(exe: &Path, names: &[&str]) -> String {
    let result = plain_child_with_environment(exe, names, "--env-report");
    match result
        .strip_prefix("EXIT_")
        .and_then(|code| code.parse::<u32>().ok())
    {
        Some(flags) => format_environment_diagnostic(flags),
        None => result,
    }
}

fn plain_child_with_environment(exe: &Path, names: &[&str], argument: &str) -> String {
    let mut command_w = quote(exe.as_os_str());
    command_w.push(b' ' as u16);
    command_w.extend(quote(OsStr::new(argument)));
    command_w.push(0);
    let environment = environment_block(names);
    let directory: Vec<u16> = match env::var_os("SystemRoot") {
        Some(root) => root.encode_wide().chain(Some(0)).collect(),
        None => return "NOT_RUN(SystemRoot_unavailable)".into(),
    };
    let executable: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let created = unsafe {
        CreateProcessW(
            executable.as_ptr(),
            command_w.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            directory.as_ptr(),
            &startup.StartupInfo,
            &mut process,
        )
    };
    if created == 0 {
        return format!("CREATE_FAILED_WIN32_{}", unsafe { GetLastError() });
    }
    let process_handle = OwnedHandle(process.hProcess);
    let _thread_handle = OwnedHandle(process.hThread);
    let wait = unsafe { WaitForSingleObject(process_handle.0, 5000) };
    if wait != WAIT_OBJECT_0 {
        return format!("WAIT_FAILED_{wait}");
    }
    let mut exit = 0u32;
    if unsafe { GetExitCodeProcess(process_handle.0, &mut exit) } == 0 {
        return format!("EXIT_QUERY_FAILED_WIN32_{}", unsafe { GetLastError() });
    }
    format!("EXIT_{exit}")
}

fn environment_block(names: &[&str]) -> Vec<u16> {
    let mut entries = names
        .iter()
        .filter_map(|name| env::var_os(name).map(|value| (name.to_string(), value)))
        .collect::<Vec<_>>();
    entries.sort_by_key(|(name, _)| name.to_ascii_lowercase());
    let mut block = Vec::new();
    for (name, value) in entries {
        block.extend(OsStr::new(&format!("{name}=")).encode_wide());
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

fn exchange_challenge(address: &std::net::SocketAddr) -> Result<(), String> {
    let mut stream = TcpStream::connect_timeout(address, Duration::from_secs(2)).map_err(err)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .map_err(err)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(err)?;
    stream.write_all(LOOPBACK_CHALLENGE).map_err(err)?;
    let mut ack = vec![0; LOOPBACK_ACK.len()];
    stream.read_exact(&mut ack).map_err(err)?;
    if ack != LOOPBACK_ACK {
        return Err("loopback listener returned an unexpected ACK".into());
    }
    Ok(())
}

fn create_temporary_profile(profile: &str) -> Result<(), String> {
    let name: Vec<u16> = OsStr::new(profile).encode_wide().chain(Some(0)).collect();
    let display = name.clone();
    let description = name.clone();
    let mut sid = null_mut();
    let hr = unsafe {
        windows_sys::Win32::Security::Isolation::CreateAppContainerProfile(
            name.as_ptr(),
            display.as_ptr(),
            description.as_ptr(),
            null(),
            0,
            &mut sid,
        )
    };
    if hr < 0 {
        return Err(format!("CreateAppContainerProfile HRESULT={hr:#x}"));
    }
    unsafe {
        windows_sys::Win32::Security::FreeSid(sid);
    }
    Ok(())
}

fn delete_temporary_profile(profile: &str) -> bool {
    let name: Vec<u16> = OsStr::new(profile).encode_wide().chain(Some(0)).collect();
    unsafe {
        windows_sys::Win32::Security::Isolation::DeleteAppContainerProfile(name.as_ptr()) >= 0
    }
}

fn appcontainer_local_data_path(profile: &str) -> Result<PathBuf, String> {
    let name: Vec<u16> = OsStr::new(profile).encode_wide().chain(Some(0)).collect();
    let mut sid = null_mut();
    let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
    if hr < 0 {
        return Err(format!(
            "DeriveAppContainerSidFromAppContainerName for storage path HRESULT={hr:#x}"
        ));
    }
    let _sid = OwnedSid(sid);
    let mut sid_string = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut sid_string) } == 0 {
        return Err(winerr("ConvertSidToStringSidW"));
    }
    let _sid_string = OwnedLocalString(sid_string);
    let mut folder = null_mut();
    let hr = unsafe { GetAppContainerFolderPath(sid_string, &mut folder) };
    if hr < 0 {
        return Err(format!("GetAppContainerFolderPath HRESULT={hr:#x}"));
    }
    let _folder = OwnedCoTaskString(folder);
    let length = unsafe { (0..).take_while(|&index| *folder.add(index) != 0).count() };
    let wide = unsafe { slice::from_raw_parts(folder, length) };
    Ok(PathBuf::from(std::ffi::OsString::from_wide(wide)))
}

fn worker_main() -> Result<(), String> {
    let mut args = env::args_os().skip(2);
    let canary = PathBuf::from(args.next().ok_or("missing canary path")?);
    let store_canary = PathBuf::from(args.next().ok_or("missing store canary path")?);
    let port: u16 = args
        .next()
        .ok_or("missing port")?
        .into_string()
        .map_err(|_| "port is not Unicode")?
        .parse()
        .map_err(err)?;
    let expected_local_data = PathBuf::from(args.next().ok_or("missing expected local data path")?);
    let broker_local_data = PathBuf::from(args.next().ok_or("missing broker local data path")?);
    let broker_temp = PathBuf::from(args.next().ok_or("missing broker TEMP path")?);
    let broker_tmp = PathBuf::from(args.next().ok_or("missing broker TMP path")?);
    let expected_temp = expected_local_data.join("Temp");
    let local_data_matches_expected = env::var_os("LOCALAPPDATA")
        .is_some_and(|value| windows_path_eq(Path::new(&value), &expected_local_data));
    let temp_matches_expected =
        env::var_os("TEMP").is_some_and(|value| windows_path_eq(Path::new(&value), &expected_temp));
    let tmp_matches_expected =
        env::var_os("TMP").is_some_and(|value| windows_path_eq(Path::new(&value), &expected_temp));
    let local_data_matches_broker = env::var_os("LOCALAPPDATA")
        .is_some_and(|value| windows_path_eq(Path::new(&value), &broker_local_data));
    let temp_matches_broker =
        env::var_os("TEMP").is_some_and(|value| windows_path_eq(Path::new(&value), &broker_temp));
    let tmp_matches_broker =
        env::var_os("TMP").is_some_and(|value| windows_path_eq(Path::new(&value), &broker_tmp));
    let path_flags = (if local_data_matches_expected {
        ENV_FLAG_LOCALAPPDATA_MATCHES_EXPECTED
    } else {
        0
    }) | (if temp_matches_expected {
        ENV_FLAG_TEMP_MATCHES_EXPECTED
    } else {
        0
    }) | (if tmp_matches_expected {
        ENV_FLAG_TMP_MATCHES_EXPECTED
    } else {
        0
    }) | (if local_data_matches_broker {
        ENV_FLAG_LOCALAPPDATA_MATCHES_BROKER
    } else {
        0
    }) | (if temp_matches_broker {
        ENV_FLAG_TEMP_MATCHES_BROKER
    } else {
        0
    }) | (if tmp_matches_broker {
        ENV_FLAG_TMP_MATCHES_BROKER
    } else {
        0
    });
    let environment_violations = unexpected_environment_flags();
    let allowed_os_temp_names = environment_violations & (ENV_FLAG_TEMP | ENV_FLAG_TMP);
    let unexpected_names = environment_violations & !(ENV_FLAG_TEMP | ENV_FLAG_TMP);
    let os_temp_verified = allowed_os_temp_names == (ENV_FLAG_TEMP | ENV_FLAG_TMP)
        && local_data_matches_expected
        && temp_matches_expected
        && tmp_matches_expected
        && !local_data_matches_broker
        && !temp_matches_broker
        && !tmp_matches_broker;
    if unexpected_names != 0 || !os_temp_verified {
        std::process::exit(
            (WORKER_EXIT_MARKER_ENV_REJECTED | environment_violations | path_flags) as i32,
        );
    }
    let token_appcontainer = token_is_appcontainer()?;
    let canary_read_status = read_canary_status(&canary);
    let store_read_status = read_canary_status(&store_canary);
    let address = format!("127.0.0.1:{port}").parse().map_err(err)?;
    let (network_stage, network_error_code, network_error_kind) =
        attempt_challenge(&address, Duration::from_millis(750));
    let child_command =
        PathBuf::from(env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into()))
            .join("System32\\cmd.exe");
    let (child_allowed, child_error) = match Command::new(child_command)
        .args(["/c", "exit", "0"])
        .spawn()
    {
        Ok(mut child) => {
            let _ = child.wait();
            (true, 0u32)
        }
        Err(error) => (false, error.raw_os_error().unwrap_or(-1) as u32),
    };
    let memory_limited = memory_limit_is_enforced();
    let code = (if !token_appcontainer { 1 } else { 0 })
        | (canary_read_status << 1)
        | (store_read_status << 3)
        | (if child_allowed || (child_error != 1816) {
            1 << 5
        } else {
            0
        })
        | (if !memory_limited { 1 << 6 } else { 0 })
        | (network_stage << 7)
        | ((network_error_code as u32) << 10)
        | ((network_error_kind & 0x3) << 26);
    std::process::exit((WORKER_EXIT_MARKER_ENV_VERIFIED | code) as i32)
}

fn windows_path_eq(left: &Path, right: &Path) -> bool {
    fn normalized(path: &Path) -> Vec<u16> {
        let mut units: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .map(|unit| {
                if (b'A' as u16..=b'Z' as u16).contains(&unit) {
                    unit + (b'a' - b'A') as u16
                } else {
                    unit
                }
            })
            .collect();
        while units
            .last()
            .is_some_and(|unit| *unit == b'\\' as u16 || *unit == b'/' as u16)
        {
            units.pop();
        }
        units
    }
    left.is_absolute() && right.is_absolute() && normalized(left) == normalized(right)
}

fn unexpected_environment_flags() -> u32 {
    let mut flags = 0u32;
    let mut other_count = 0u32;
    for (name, _) in env::vars_os() {
        if ["SystemDrive", "SystemRoot", "windir", "LOCALAPPDATA"]
            .iter()
            .any(|allowed| name == OsStr::new(allowed))
        {
            continue;
        }
        if name == OsStr::new(PARENT_SECRET_CANARY_NAME) {
            flags |= ENV_FLAG_SECRET_CANARY;
            continue;
        }
        let lower = name.to_string_lossy().to_ascii_lowercase();
        match lower.as_str() {
            "comspec" => flags |= ENV_FLAG_COMSPEC,
            "temp" => flags |= ENV_FLAG_TEMP,
            "tmp" => flags |= ENV_FLAG_TMP,
            "userprofile" => flags |= ENV_FLAG_USERPROFILE,
            "appdata" => flags |= ENV_FLAG_APPDATA,
            "programdata" => flags |= ENV_FLAG_PROGRAMDATA,
            _ if is_drive_current_directory_name(&lower) => {
                flags |= ENV_FLAG_DRIVE_CURRENT_DIRECTORY;
            }
            _ => {
                other_count = other_count.saturating_add(1);
                flags |= ENV_FLAG_OTHER;
            }
        }
    }
    flags | (other_count.min(0xf) << ENV_OTHER_COUNT_SHIFT)
}

fn is_drive_current_directory_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 3 && bytes[0] == b'=' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':'
}

fn encode_io_error(error: &std::io::Error) -> (u16, u32) {
    let code = error
        .raw_os_error()
        .map(|code| code as u16)
        .unwrap_or(NET_CODE_NO_OS_ERROR);
    let kind = match error.kind() {
        std::io::ErrorKind::TimedOut => 1,
        std::io::ErrorKind::PermissionDenied => 2,
        _ => 3,
    };
    (code, kind)
}

fn read_canary_status(path: &Path) -> u32 {
    match fs::read(path) {
        Ok(_) => READ_STATUS_ALLOWED,
        Err(error) if error.raw_os_error() == Some(5) => READ_STATUS_DENIED,
        Err(_) => READ_STATUS_UNEXPECTED,
    }
}

fn token_is_appcontainer() -> Result<bool, String> {
    let mut token: HANDLE = null_mut();
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(winerr("OpenProcessToken"));
        }
    }
    let mut value: u32 = 0;
    let mut returned = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenIsAppContainer,
            (&mut value as *mut u32).cast(),
            4,
            &mut returned,
        )
    };
    unsafe {
        CloseHandle(token);
    }
    if ok == 0 {
        return Err(winerr("GetTokenInformation(TokenIsAppContainer)"));
    }
    Ok(value == 1)
}

fn memory_limit_is_enforced() -> bool {
    let mut committed = 0usize;
    loop {
        if committed >= APP_LIMIT_BYTES {
            return false;
        }
        let block = unsafe {
            VirtualAlloc(
                null(),
                4 * 1024 * 1024,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        if block.is_null() {
            return (16 * 1024 * 1024..=APP_LIMIT_BYTES).contains(&committed);
        }
        unsafe {
            (block as *mut u8).write_volatile(1);
        }
        committed += 4 * 1024 * 1024;
    }
}

fn launch_worker(
    profile: &str,
    exe: &Path,
    canary: &Path,
    store_canary: &Path,
    port: u16,
    extra_environment: &[&str],
    broker_environment: &BrokerEnvironment<'_>,
) -> Result<String, String> {
    launch(
        profile,
        exe,
        &[
            OsStr::new("--worker"),
            canary.as_os_str(),
            store_canary.as_os_str(),
            OsStr::new(&port.to_string()),
            broker_environment.expected_local_data.as_os_str(),
            broker_environment.broker_local_data.as_os_str(),
            broker_environment.broker_temp.as_os_str(),
            broker_environment.broker_tmp.as_os_str(),
        ],
        false,
        extra_environment,
    )
}

fn launch_time_limit_probe(
    profile: &str,
    exe: &Path,
    extra_environment: &[&str],
) -> Result<String, String> {
    launch(
        profile,
        exe,
        &[OsStr::new("--hold")],
        true,
        extra_environment,
    )
}

fn launch(
    profile: &str,
    exe: &Path,
    args: &[&OsStr],
    hold: bool,
    extra_environment: &[&str],
) -> Result<String, String> {
    launch_with_policy(
        profile,
        exe,
        args,
        hold,
        extra_environment,
        ContainerPolicy::AppContainer,
    )
}

/// Builds a well-known SID into an owned buffer. CreateWellKnownSid is called
/// twice: first to size the SID, then to fill the allocation.
fn well_known_sid(kind: i32) -> Result<Vec<u8>, String> {
    let mut size = 0u32;
    unsafe {
        CreateWellKnownSid(kind, null_mut(), null_mut(), &mut size);
    }
    if size == 0 {
        return Err(winerr("CreateWellKnownSid(size)"));
    }
    let mut buffer = vec![0u8; size as usize];
    if unsafe { CreateWellKnownSid(kind, null_mut(), buffer.as_mut_ptr().cast(), &mut size) } == 0 {
        return Err(winerr("CreateWellKnownSid"));
    }
    Ok(buffer)
}

/// Returns a primary token that is a restricted version of the broker's own
/// token: write-restricted to the RESTRICTED SID only, with maximum privileges
/// disabled and integrity lowered to Untrusted. Because the token is derived
/// from the caller, CreateProcessAsUserW needs no assign-primary-token
/// privilege. The AppContainer capabilities are applied separately, at launch.
fn restricted_primary_token() -> Result<OwnedHandle, String> {
    let mut source: HANDLE = null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
            &mut source,
        )
    } == 0
    {
        return Err(winerr("OpenProcessToken(restricted)"));
    }
    let source = OwnedHandle(source);

    let restricted_sid = well_known_sid(WinRestrictedCodeSid)?;
    let restricting = [SID_AND_ATTRIBUTES {
        Sid: restricted_sid.as_ptr() as *mut _,
        Attributes: 0,
    }];
    let mut restricted: HANDLE = null_mut();
    if unsafe {
        CreateRestrictedToken(
            source.0,
            WRITE_RESTRICTED | DISABLE_MAX_PRIVILEGE,
            0,
            null(),
            0,
            null(),
            restricting.len() as u32,
            restricting.as_ptr(),
            &mut restricted,
        )
    } == 0
    {
        return Err(winerr("CreateRestrictedToken"));
    }
    let restricted = OwnedHandle(restricted);

    // Lower integrity to Untrusted. Only a lower level is permitted, so this
    // always succeeds from the broker's medium/high token. The token keeps its
    // own copy of the SID, so the buffer need outlive only this call.
    if RESTRICTED_TOKEN_UNTRUSTED_INTEGRITY {
        let untrusted_sid = well_known_sid(WinUntrustedLabelSid)?;
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: untrusted_sid.as_ptr() as *mut _,
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        if unsafe {
            SetTokenInformation(
                restricted.0,
                TokenIntegrityLevel,
                (&label as *const TOKEN_MANDATORY_LABEL).cast(),
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32,
            )
        } == 0
        {
            return Err(winerr("SetTokenInformation(TokenIntegrityLevel)"));
        }
    }
    Ok(restricted)
}

fn launch_with_policy(
    profile: &str,
    exe: &Path,
    args: &[&OsStr],
    hold: bool,
    extra_environment: &[&str],
    policy: ContainerPolicy,
) -> Result<String, String> {
    let name: Vec<u16> = OsStr::new(profile).encode_wide().chain(Some(0)).collect();
    let mut app_sid = null_mut();
    let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut app_sid) };
    if hr < 0 {
        return Err(format!(
            "DeriveAppContainerSidFromAppContainerName HRESULT={hr:#x}"
        ));
    }
    let app_sid = OwnedSid(app_sid);
    let job = OwnedHandle(unsafe { CreateJobObjectW(null(), null()) });
    if job.0.is_null() {
        return Err(winerr("CreateJobObjectW"));
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_PROCESS_TIME;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    // Keep this above the two-second broker deadline so the deadline test has
    // one cause. CPU-accounting enforcement requires a separate probe.
    limits.BasicLimitInformation.PerProcessUserTimeLimit = 30 * JOB_USER_TIME_100NS;
    limits.ProcessMemoryLimit = APP_LIMIT_BYTES;
    let set_limits = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    };
    if set_limits == 0 {
        return Err(winerr("SetInformationJobObject"));
    }

    let caps = SECURITY_CAPABILITIES {
        AppContainerSid: app_sid.0,
        Capabilities: null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let all_packages_opt_out = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT;
    let mut attrs = vec![(
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
        (&caps as *const SECURITY_CAPABILITIES).cast::<std::ffi::c_void>(),
        std::mem::size_of_val(&caps),
    )];
    if policy == ContainerPolicy::LessPrivileged {
        attrs.push((
            PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY as usize,
            (&all_packages_opt_out as *const u32).cast(),
            std::mem::size_of_val(&all_packages_opt_out),
        ));
    }
    let attr_count = attrs.len() as u32;
    let mut attr_bytes = 0usize;
    unsafe {
        InitializeProcThreadAttributeList(null_mut(), attr_count, 0, &mut attr_bytes);
    }
    let mut attr_storage = vec![0usize; attr_bytes.div_ceil(std::mem::size_of::<usize>())];
    let attr_list = attr_storage.as_mut_ptr().cast();
    if unsafe { InitializeProcThreadAttributeList(attr_list, attr_count, 0, &mut attr_bytes) } == 0
    {
        return Err(winerr("InitializeProcThreadAttributeList"));
    }
    let _attrs_guard = OwnedAttributes(attr_list);
    for (key, value, size) in attrs {
        if unsafe {
            UpdateProcThreadAttribute(attr_list, 0, key, value, size, null_mut(), null_mut())
        } == 0
        {
            return Err(winerr("UpdateProcThreadAttribute"));
        }
    }

    let mut command_w = quote(exe.as_os_str());
    for arg in args {
        command_w.push(b' ' as u16);
        command_w.extend(quote(arg));
    }
    command_w.push(0);
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attr_list;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // Windows redirects the broker's base LOCALAPPDATA into this AppContainer.
    // Supplying the already redirected folder would cause a second redirection.
    // Copy only these OS runtime variables, never the ambient environment.
    let base_names = ["SystemDrive", "SystemRoot", "windir"];
    let names = [base_names.as_slice(), extra_environment].concat();
    let environment = environment_block(&names);
    let executable: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let directory: Vec<u16> = env::var_os("SystemRoot")
        .ok_or("SystemRoot is unavailable")?
        .encode_wide()
        .chain(Some(0))
        .collect();
    let creation_flags = EXTENDED_STARTUPINFO_PRESENT
        | CREATE_NO_WINDOW
        | CREATE_SUSPENDED
        | CREATE_UNICODE_ENVIRONMENT;
    // The restricted-token policy supplies a primary token to CreateProcessAsUserW
    // while still carrying the same AppContainer SECURITY_CAPABILITIES attribute.
    // The token handle must outlive the call, so it is bound here.
    let (created, api) = if policy == ContainerPolicy::RestrictedToken {
        let token = restricted_primary_token()?;
        let created = unsafe {
            CreateProcessAsUserW(
                token.0,
                executable.as_ptr(),
                command_w.as_mut_ptr(),
                null(),
                null(),
                0,
                creation_flags,
                environment.as_ptr().cast(),
                directory.as_ptr(),
                &startup.StartupInfo,
                &mut pi,
            )
        };
        (
            created,
            "CreateProcessAsUserW(RestrictedToken+AppContainer)",
        )
    } else {
        let created = unsafe {
            CreateProcessW(
                executable.as_ptr(),
                command_w.as_mut_ptr(),
                null(),
                null(),
                0,
                creation_flags,
                environment.as_ptr().cast(),
                directory.as_ptr(),
                &startup.StartupInfo,
                &mut pi,
            )
        };
        (created, "CreateProcessW(AppContainer)")
    };
    if created == 0 {
        return Err(winerr(api));
    }
    let process = OwnedHandle(pi.hProcess);
    let thread = OwnedHandle(pi.hThread);
    if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
        unsafe {
            windows_sys::Win32::System::Threading::TerminateProcess(process.0, 1);
            WaitForSingleObject(process.0, 5000);
        }
        return Err(winerr("AssignProcessToJobObject"));
    }
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        unsafe {
            windows_sys::Win32::System::Threading::TerminateProcess(process.0, 1);
            WaitForSingleObject(process.0, 5000);
        }
        return Err(winerr("ResumeThread"));
    }

    if hold {
        let wait = unsafe { WaitForSingleObject(process.0, 2000) };
        if wait == WAIT_TIMEOUT {
            let terminated = unsafe { TerminateJobObject(job.0, 0xE0000001) };
            let confirmed = unsafe { WaitForSingleObject(process.0, 5000) };
            let mut exit_code = 0u32;
            unsafe {
                GetExitCodeProcess(process.0, &mut exit_code);
            }
            return Ok(format!(
                "job_cpu_time_limit=NOT_DEMONSTRATED,broker_wall_deadline={},job_termination_confirmed={},exit={exit_code}",
                if terminated != 0 && confirmed == WAIT_OBJECT_0 {
                    "ENFORCED"
                } else {
                    "FAILED"
                },
                if confirmed == WAIT_OBJECT_0 {
                    "true"
                } else {
                    "false"
                }
            ));
        }
        let mut exit_code = 0u32;
        unsafe {
            GetExitCodeProcess(process.0, &mut exit_code);
        }
        if wait == WAIT_OBJECT_0 && exit_code != 0 {
            Ok(format!(
                "job_cpu_time_limit=NOT_DEMONSTRATED,broker_wall_deadline=FAILED_EARLY_EXIT,exit={exit_code}"
            ))
        } else {
            Ok(format!(
                "job_cpu_time_limit=NOT_DEMONSTRATED,broker_wall_deadline=FAILED(wait={wait},exit={exit_code})"
            ))
        }
    } else {
        let wait = unsafe { WaitForSingleObject(process.0, 5000) };
        let mut exit_code = 0u32;
        unsafe {
            GetExitCodeProcess(process.0, &mut exit_code);
        }
        if wait != WAIT_OBJECT_0 {
            return Err(format!(
                "worker completion failed wait={wait}, exit={exit_code}"
            ));
        }
        if args.first().is_some_and(|arg| *arg == OsStr::new("--noop")) {
            if exit_code == 0 {
                return Ok("noop_exit=0".into());
            }
            return Err(format!("noop child exited with {exit_code}"));
        }
        if args
            .first()
            .is_some_and(|arg| *arg == OsStr::new("--network-policy"))
        {
            return format_network_policy(exit_code);
        }
        if args
            .first()
            .is_some_and(|arg| *arg == OsStr::new("--net-probe"))
        {
            // launch_net_probe decodes and marker-checks this raw value.
            return Ok(exit_code.to_string());
        }
        if args
            .first()
            .is_some_and(|arg| *arg == OsStr::new("--afd-probe"))
        {
            return Ok(exit_code.to_string());
        }
        worker_report(exit_code)
    }
}

fn worker_report(exit_code: u32) -> Result<String, String> {
    let worker_marker = exit_code & 0xf000_0000;
    if worker_marker == WORKER_EXIT_MARKER_ENV_REJECTED {
        return Ok(format!(
            "worker_environment=REJECTED,{},appcontainer_storage_comparisons: localappdata_expected={},temp_expected={},tmp_expected={},localappdata_matches_broker={},temp_matches_broker={},tmp_matches_broker={},worker_checks=NOT_RUN",
            format_environment_diagnostic(exit_code & 0x0fff_ffff),
            exit_code & ENV_FLAG_LOCALAPPDATA_MATCHES_EXPECTED != 0,
            exit_code & ENV_FLAG_TEMP_MATCHES_EXPECTED != 0,
            exit_code & ENV_FLAG_TMP_MATCHES_EXPECTED != 0,
            exit_code & ENV_FLAG_LOCALAPPDATA_MATCHES_BROKER != 0,
            exit_code & ENV_FLAG_TEMP_MATCHES_BROKER != 0,
            exit_code & ENV_FLAG_TMP_MATCHES_BROKER != 0,
        ));
    }
    if worker_marker != WORKER_EXIT_MARKER_ENV_VERIFIED {
        return Err(format!("worker entry marker missing: exit={exit_code}"));
    }
    let network_stage = (exit_code >> 7) & 0x7;
    let network_error_code = (exit_code >> 10) & 0xffff;
    let network_error_kind = (exit_code >> 26) & 0x3;
    let network_error = format_network_error(network_error_code, network_error_kind);
    let connect_status = if network_stage == 1 {
        network_error.clone()
    } else {
        "OK".into()
    };
    let timeout_status = match network_stage {
        1 => "NOT_ATTEMPTED".into(),
        4 => network_error.clone(),
        _ => "OK".into(),
    };
    let write_status = if network_stage == 1 || network_stage == 4 {
        "NOT_ATTEMPTED".into()
    } else if network_stage == 2 {
        network_error.clone()
    } else {
        "OK".into()
    };
    let read_status = match network_stage {
        1 | 2 | 4 => "NOT_ATTEMPTED".into(),
        3 => network_error.clone(),
        5 => "PROTOCOL_MISMATCH".into(),
        _ => "ACK_OK".into(),
    };
    let network_result =
        if network_stage == 1 && (network_error_code == 10013 || network_error_kind == 2) {
            "DENIED"
        } else if network_stage == 0 {
            "CHALLENGE_ACK"
        } else {
            "EQUIVOCAL"
        };
    // This marker is emitted only after every environment/path check passes.
    // Its remaining payload bits hold socket results, not path booleans.
    Ok(format!(
        "token_appcontainer={},parent_secret_env=ABSENT,appcontainer_storage_env=VERIFIED(localappdata_matches_expected=true,temp_and_tmp_match_expected=true,all_differ_from_broker=true),canary_read={},store_read={},loopback={},connect={},timeout_setup={},write={},read={},child={},child_error={},memory_limit={},worker_exit={exit_code}",
        if exit_code & 1 == 0 { "true" } else { "false" },
        format_canary_status((exit_code >> 1) & 0x3),
        format_canary_status((exit_code >> 3) & 0x3),
        network_result,
        connect_status,
        timeout_status,
        write_status,
        read_status,
        if exit_code & (1 << 5) == 0 {
            "DENIED"
        } else {
            "ALLOWED_OR_WRONG_ERROR"
        },
        if exit_code & (1 << 5) == 0 {
            "ERROR_NOT_ENOUGH_QUOTA(1816)"
        } else {
            "UNEXPECTED_OR_OTHER"
        },
        if exit_code & (1 << 6) == 0 {
            "ENFORCED"
        } else {
            "NOT_ENFORCED"
        }
    ))
}

fn winerr(api: &str) -> String {
    format!("{api} failed: Win32 {}", unsafe { GetLastError() })
}

fn network_policy_status() -> u32 {
    let server: Vec<u16> = "127.0.0.1".encode_utf16().chain(Some(0)).collect();
    let mut missing_capability = -1;
    let status = unsafe {
        NetworkIsolationDiagnoseConnectFailureAndGetInfo(server.as_ptr(), &mut missing_capability)
    };
    if status > u16::MAX as u32 || (status == 0 && !(0..=3).contains(&missing_capability)) {
        return 1; // Unknown results must not be decoded as a successful diagnosis.
    }
    let missing = if status == 0 {
        missing_capability as u32
    } else {
        15
    };
    NETWORK_POLICY_MARKER | (status << 4) | missing
}

fn format_network_policy(code: u32) -> Result<String, String> {
    if code & 0xfff0_0000 != NETWORK_POLICY_MARKER {
        return Err(format!(
            "network policy diagnostic marker missing: exit={code}"
        ));
    }
    let status = (code >> 4) & 0xffff;
    let missing = match code & 15 {
        0 => "NONE",
        1 => "PRIVATE_NETWORK",
        2 => "INTERNET_CLIENT",
        3 => "INTERNET_CLIENT_SERVER",
        _ => "UNAVAILABLE",
    };
    Ok(format!("api_status={status},missing_capability={missing}"))
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn format_network_error(code: u32, kind: u32) -> String {
    let kind_name = match kind {
        1 => "TimedOut",
        2 => "PermissionDenied",
        3 => "Other",
        _ => "None",
    };
    match code as u16 {
        NET_CODE_NONE => "NO_ERROR".into(),
        NET_CODE_NO_OS_ERROR => format!("NO_RAW_OS_ERROR(kind={kind_name})"),
        NET_CODE_PROTOCOL_MISMATCH => "PROTOCOL_MISMATCH".into(),
        raw => format!("OS_ERROR({raw},kind={kind_name})"),
    }
}

fn format_canary_status(status: u32) -> &'static str {
    match status {
        READ_STATUS_DENIED => "DENIED_ACCESS(5)",
        READ_STATUS_ALLOWED => "ALLOWED",
        READ_STATUS_UNEXPECTED => "UNEXPECTED_ERROR",
        _ => "INVALID_STATUS",
    }
}

fn format_environment_diagnostic(flags: u32) -> String {
    let mut names = Vec::new();
    for (flag, name) in [
        (ENV_FLAG_SECRET_CANARY, "synthetic_parent_secret_canary"),
        (ENV_FLAG_COMSPEC, "ComSpec"),
        (ENV_FLAG_TEMP, "TEMP"),
        (ENV_FLAG_TMP, "TMP"),
        (ENV_FLAG_USERPROFILE, "USERPROFILE"),
        (ENV_FLAG_APPDATA, "APPDATA"),
        (ENV_FLAG_PROGRAMDATA, "ProgramData"),
        (ENV_FLAG_DRIVE_CURRENT_DIRECTORY, "drive_current_directory"),
    ] {
        if flags & flag != 0 {
            names.push(name.to_string());
        }
    }
    let other_count = (flags >> ENV_OTHER_COUNT_SHIFT) & 0xf;
    if other_count > 0 {
        names.push(format!("other_unexpected_names={other_count}"));
    }
    if names.is_empty() {
        names.push("unknown_env_violation".into());
    }
    format!(
        "unexpected_names=[{}],synthetic_parent_secret_canary={}",
        names.join(","),
        if flags & ENV_FLAG_SECRET_CANARY != 0 {
            "PRESENT"
        } else {
            "ABSENT"
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_verified_environment_when_socket_bits_change_then_path_report_stays_verified() {
        for stage in 0..=5u32 {
            for raw_error in [0, 10013, 10061, NET_CODE_NO_OS_ERROR as u32] {
                for kind in 0..=3u32 {
                    let code = WORKER_EXIT_MARKER_ENV_VERIFIED
                        | (stage << 7)
                        | (raw_error << 10)
                        | (kind << 26);
                    let report = worker_report(code).unwrap();
                    assert!(report.contains("appcontainer_storage_env=VERIFIED(localappdata_matches_expected=true,temp_and_tmp_match_expected=true,all_differ_from_broker=true)"));
                }
            }
        }
    }

    #[test]
    fn given_environment_rejection_then_no_worker_check_is_reported_as_executed() {
        let report = worker_report(
            WORKER_EXIT_MARKER_ENV_REJECTED | ENV_FLAG_TEMP | ENV_FLAG_TMP | ENV_FLAG_SECRET_CANARY,
        )
        .unwrap();
        assert!(report.contains("worker_environment=REJECTED"));
        assert!(report.contains("synthetic_parent_secret_canary=PRESENT"));
        assert!(
            report.contains("localappdata_expected=false,temp_expected=false,tmp_expected=false")
        );
        assert!(report.contains("worker_checks=NOT_RUN"));
        assert!(!report.contains("VERIFIED"));
    }

    #[test]
    fn given_nonworker_exit_then_decoder_rejects_it() {
        for code in [
            0,
            1,
            259,
            0xc000_0005,
            NETWORK_POLICY_MARKER,
            NET_PROBE_MARKER,
        ] {
            assert!(worker_report(code).is_err());
        }
    }

    fn silent_timeout() -> ProbeOutcome {
        ProbeOutcome {
            appcontainer: true,
            stage: NET_STAGE_CONNECT_FAILED,
            code: NET_CODE_NO_OS_ERROR,
            kind: NET_KIND_TIMED_OUT,
            elapsed_ms: 3000,
        }
    }

    fn endpoint(
        name: &'static str,
        shape: EndpointShape,
        worker: ProbeOutcome,
    ) -> EndpointEvidence {
        EndpointEvidence {
            name,
            shape,
            required: true,
            controls_passed: true,
            control_detail: String::new(),
            endpoint_worker_observations: 0,
            worker,
        }
    }

    fn sent_datagram() -> ProbeOutcome {
        ProbeOutcome {
            stage: NET_STAGE_DATAGRAM_SENT,
            code: NET_CODE_NONE,
            kind: 0,
            ..silent_timeout()
        }
    }

    fn silent_suite() -> Vec<EndpointEvidence> {
        vec![
            endpoint("open", EndpointShape::OpenStream, silent_timeout()),
            endpoint("closed", EndpointShape::ClosedStream, silent_timeout()),
            endpoint("udp", EndpointShape::Datagram, sent_datagram()),
        ]
    }

    #[test]
    fn given_probe_outcomes_when_encoded_then_decoding_round_trips_and_caps_elapsed() {
        for outcome in [
            silent_timeout(),
            sent_datagram(),
            ProbeOutcome {
                appcontainer: false,
                stage: 5,
                code: WSAECONNREFUSED,
                kind: 3,
                elapsed_ms: 900,
            },
        ] {
            assert_eq!(decode_probe(encode_probe(outcome)).unwrap(), outcome);
        }
        let capped = decode_probe(encode_probe(ProbeOutcome {
            elapsed_ms: u32::MAX,
            ..silent_timeout()
        }))
        .unwrap();
        assert_eq!(capped.elapsed_ms, 31 * NET_PROBE_ELAPSED_UNIT_MS);
        let init_failure = ProbeOutcome {
            stage: NET_STAGE_SOCKET_INIT_FAILED,
            code: 10106,
            kind: 3,
            ..silent_timeout()
        };
        assert_eq!(
            decode_probe(encode_probe(init_failure)).unwrap(),
            init_failure
        );
        for exit in [
            0,
            1,
            NETWORK_POLICY_MARKER,
            WORKER_EXIT_MARKER_ENV_VERIFIED,
            0xE000_0001,
        ] {
            assert!(decode_probe(exit).is_err());
        }
    }

    #[test]
    fn given_silent_drops_with_closed_port_differential_then_denial_is_established() {
        let suite = silent_suite();
        assert!(
            suite
                .iter()
                .all(|item| classify_endpoint(item, DeviceAccess::Opened)
                    == EndpointVerdict::Dropped)
        );
        assert!(network_denial_established(&suite, DeviceAccess::Opened));
    }

    #[test]
    fn given_explicit_access_errors_then_endpoints_are_denied_explicitly() {
        let refused_access = ProbeOutcome {
            code: WSAEACCES,
            kind: NET_KIND_PERMISSION_DENIED,
            ..silent_timeout()
        };
        let datagram_access = ProbeOutcome {
            stage: NET_STAGE_DATAGRAM_FAILED,
            ..refused_access
        };
        let suite = vec![
            endpoint("open", EndpointShape::OpenStream, refused_access),
            endpoint("closed", EndpointShape::ClosedStream, refused_access),
            endpoint("udp", EndpointShape::Datagram, datagram_access),
        ];
        assert!(
            suite
                .iter()
                .all(|item| classify_endpoint(item, DeviceAccess::Opened)
                    == EndpointVerdict::DeniedExplicit)
        );
        assert!(network_denial_established(&suite, DeviceAccess::Opened));
    }

    #[test]
    fn given_worker_refused_on_closed_port_then_boundary_was_crossed() {
        let mut suite = silent_suite();
        suite[1].worker = ProbeOutcome {
            code: WSAECONNREFUSED,
            kind: 3,
            ..silent_timeout()
        };
        assert_eq!(
            classify_endpoint(&suite[1], DeviceAccess::Opened),
            EndpointVerdict::Reached
        );
        assert!(!network_denial_established(&suite, DeviceAccess::Opened));
    }

    #[test]
    fn given_any_endpoint_observation_then_silent_worker_failure_is_not_denial() {
        for index in 0..3 {
            let mut suite = silent_suite();
            suite[index].endpoint_worker_observations = 1;
            assert_eq!(
                classify_endpoint(&suite[index], DeviceAccess::Opened),
                EndpointVerdict::Reached
            );
            assert!(!network_denial_established(&suite, DeviceAccess::Opened));
        }
    }

    #[test]
    fn given_established_stream_at_any_stage_then_endpoint_was_reached() {
        for stage in [NET_STAGE_EXCHANGED, 2, 3, 4, 5] {
            let mut suite = silent_suite();
            suite[0].worker = ProbeOutcome {
                stage,
                ..silent_timeout()
            };
            assert_eq!(
                classify_endpoint(&suite[0], DeviceAccess::Opened),
                EndpointVerdict::Reached
            );
            assert!(!network_denial_established(&suite, DeviceAccess::Opened));
        }
    }

    #[test]
    fn given_failed_required_control_or_noncontainer_token_then_denial_is_not_established() {
        let mut suite = silent_suite();
        suite[1].controls_passed = false;
        assert_eq!(
            classify_endpoint(&suite[1], DeviceAccess::Opened),
            EndpointVerdict::ControlFailed
        );
        assert!(!network_denial_established(&suite, DeviceAccess::Opened));

        let mut suite = silent_suite();
        suite[2].worker.appcontainer = false;
        assert_eq!(
            classify_endpoint(&suite[2], DeviceAccess::Opened),
            EndpointVerdict::NotAContainer
        );
        assert!(!network_denial_established(&suite, DeviceAccess::Opened));
    }

    #[test]
    fn given_missing_closed_port_discriminator_then_denial_is_not_established() {
        let mut suite = silent_suite();
        suite.remove(1);
        assert!(!network_denial_established(&suite, DeviceAccess::Opened));
        let mut suite = silent_suite();
        suite[1].required = false;
        assert!(!network_denial_established(&suite, DeviceAccess::Opened));
        assert!(!network_denial_established(&[], DeviceAccess::Opened));
    }

    #[test]
    fn given_unexplained_connect_error_then_endpoint_is_equivocal() {
        let mut suite = silent_suite();
        suite[0].worker = ProbeOutcome {
            code: 10051,
            kind: 3,
            ..silent_timeout()
        };
        assert_eq!(
            classify_endpoint(&suite[0], DeviceAccess::Opened),
            EndpointVerdict::Equivocal
        );
        assert!(!network_denial_established(&suite, DeviceAccess::Opened));
    }

    #[test]
    fn given_optional_endpoint_then_only_control_unavailability_is_tolerated() {
        let mut suite = silent_suite();
        let mut optional = endpoint("optional", EndpointShape::ClosedStream, silent_timeout());
        optional.required = false;
        optional.controls_passed = false;
        suite.push(optional);
        assert!(network_denial_established(&suite, DeviceAccess::Opened));

        let last = suite.len() - 1;
        suite[last].controls_passed = true;
        suite[last].worker.code = WSAECONNREFUSED;
        suite[last].worker.kind = 3;
        assert_eq!(
            classify_endpoint(&suite[last], DeviceAccess::Opened),
            EndpointVerdict::Reached
        );
        assert!(!network_denial_established(&suite, DeviceAccess::Opened));
    }

    #[test]
    fn given_distinct_utf16_paths_then_comparison_does_not_collapse_replacement_characters() {
        let prefix: Vec<u16> = r"C:\synthetic\".encode_utf16().collect();
        let first = [prefix.clone(), vec![0xd800]].concat();
        let second = [prefix, vec![0xd801]].concat();
        let first = PathBuf::from(std::ffi::OsString::from_wide(&first));
        let second = PathBuf::from(std::ffi::OsString::from_wide(&second));
        assert!(!windows_path_eq(&first, &second));
        assert!(windows_path_eq(&first, &first));
        assert!(!windows_path_eq(Path::new(r"C:"), Path::new(r"C:\")));
        assert!(windows_path_eq(
            Path::new(r"C:\Synthetic\Temp\"),
            Path::new(r"c:\synthetic\temp")
        ));
    }

    fn socket_init_failed() -> ProbeOutcome {
        ProbeOutcome {
            stage: NET_STAGE_SOCKET_INIT_FAILED,
            code: 10106,
            kind: 3,
            ..silent_timeout()
        }
    }

    #[test]
    fn given_socket_init_failure_then_only_a_denied_device_makes_it_explicit_denial() {
        let suite = vec![
            endpoint("open", EndpointShape::OpenStream, socket_init_failed()),
            endpoint("closed", EndpointShape::ClosedStream, socket_init_failed()),
            endpoint("udp", EndpointShape::Datagram, socket_init_failed()),
        ];
        for item in &suite {
            assert_eq!(
                classify_endpoint(item, DeviceAccess::Denied),
                EndpointVerdict::DeniedExplicit
            );
            for device in [DeviceAccess::Opened, DeviceAccess::Inconclusive] {
                assert_eq!(classify_endpoint(item, device), EndpointVerdict::Equivocal);
            }
        }
        assert!(network_denial_established(&suite, DeviceAccess::Denied));
        assert!(!network_denial_established(&suite, DeviceAccess::Opened));
        assert!(!network_denial_established(
            &suite,
            DeviceAccess::Inconclusive
        ));
    }

    #[test]
    fn given_denied_device_then_endpoint_observations_and_refusals_still_fail() {
        let mut suite = silent_suite();
        suite[0].endpoint_worker_observations = 1;
        assert_eq!(
            classify_endpoint(&suite[0], DeviceAccess::Denied),
            EndpointVerdict::Reached
        );
        assert!(!network_denial_established(&suite, DeviceAccess::Denied));

        let mut suite = silent_suite();
        suite[1].worker.code = WSAECONNREFUSED;
        suite[1].worker.kind = 3;
        assert!(!network_denial_established(&suite, DeviceAccess::Denied));
    }

    #[test]
    fn given_afd_probe_exits_then_decoding_and_device_classification_are_strict() {
        for (appcontainer, opened, error) in [(true, false, 5), (false, true, 0), (true, false, 2)]
        {
            let exit = AFD_PROBE_MARKER
                | u32::from(!appcontainer)
                | (u32::from(opened) << 1)
                | (u32::from(error) << 2);
            assert_eq!(
                decode_afd_probe(exit).unwrap(),
                (appcontainer, opened, error)
            );
        }
        for exit in [0, 101, NET_PROBE_MARKER, NETWORK_POLICY_MARKER] {
            assert!(decode_afd_probe(exit).is_err());
        }

        let host_ok = (true, 0);
        let host_other_error = (false, 87);
        let host_denied = (false, ERROR_ACCESS_DENIED);
        assert_eq!(
            classify_device_access(host_ok, true, (false, ERROR_ACCESS_DENIED)),
            DeviceAccess::Denied
        );
        assert_eq!(
            classify_device_access(host_other_error, true, (false, ERROR_ACCESS_DENIED)),
            DeviceAccess::Denied
        );
        assert_eq!(
            classify_device_access(host_ok, true, (true, 0)),
            DeviceAccess::Opened
        );
        assert_eq!(
            classify_device_access(host_denied, true, (false, ERROR_ACCESS_DENIED)),
            DeviceAccess::Inconclusive
        );
        assert_eq!(
            classify_device_access(host_ok, false, (false, ERROR_ACCESS_DENIED)),
            DeviceAccess::Inconclusive
        );
        assert_eq!(
            classify_device_access(host_ok, true, (false, 2)),
            DeviceAccess::Inconclusive
        );
    }

    const PASSING_WORKER_REPORT: &str = "token_appcontainer=true,parent_secret_env=ABSENT,appcontainer_storage_env=VERIFIED(localappdata_matches_expected=true,temp_and_tmp_match_expected=true,all_differ_from_broker=true),canary_read=DENIED_ACCESS(5),store_read=DENIED_ACCESS(5),loopback=DENIED,connect=OK,timeout_setup=NOT_ATTEMPTED,write=NOT_ATTEMPTED,read=NOT_ATTEMPTED,child=DENIED,child_error=ERROR_NOT_ENOUGH_QUOTA(1816),memory_limit=ENFORCED,worker_exit=1744827520";
    const PASSING_TIME_LIMIT: &str = "job_cpu_time_limit=NOT_DEMONSTRATED,broker_wall_deadline=ENFORCED,job_termination_confirmed=true,exit=3758096385";

    #[test]
    fn given_complete_worker_report_and_denial_when_judged_then_gate_assertions_pass() {
        assert!(worker_gate_assertions_pass(
            PASSING_WORKER_REPORT,
            true,
            PASSING_TIME_LIMIT
        ));
        // An EQUIVOCAL client-side loopback is accepted exactly like DENIED,
        // because the endpoint differential, not the worker view, decides denial.
        let equivocal = PASSING_WORKER_REPORT.replace("loopback=DENIED", "loopback=EQUIVOCAL");
        assert!(worker_gate_assertions_pass(
            &equivocal,
            true,
            PASSING_TIME_LIMIT
        ));
        // The CPU-time branch may stand in for the broker-deadline branch.
        let cpu_time = PASSING_TIME_LIMIT.replace(
            "job_cpu_time_limit=NOT_DEMONSTRATED,broker_wall_deadline=ENFORCED",
            "job_cpu_time_limit=ENFORCED,broker_wall_deadline=NOT_DEMONSTRATED",
        );
        assert!(worker_gate_assertions_pass(
            PASSING_WORKER_REPORT,
            true,
            &cpu_time
        ));
    }

    #[test]
    fn given_network_not_denied_when_judged_then_gate_assertions_fail() {
        assert!(!worker_gate_assertions_pass(
            PASSING_WORKER_REPORT,
            false,
            PASSING_TIME_LIMIT
        ));
    }

    #[test]
    fn given_each_required_clause_missing_then_gate_assertions_fail() {
        // Removing any single required substring must flip the verdict, so a
        // restricted-token worker that regresses even one guarantee cannot pass.
        for fragment in [
            "canary_read=DENIED_ACCESS(5)",
            "store_read=DENIED_ACCESS(5)",
            "parent_secret_env=ABSENT",
            "appcontainer_storage_env=VERIFIED",
            "timeout_setup=NOT_ATTEMPTED",
            "child=DENIED,child_error=ERROR_NOT_ENOUGH_QUOTA(1816)",
            "token_appcontainer=true",
            "memory_limit=ENFORCED",
        ] {
            let broken = PASSING_WORKER_REPORT.replace(fragment, "");
            assert!(
                !worker_gate_assertions_pass(&broken, true, PASSING_TIME_LIMIT),
                "removing {fragment} should fail the gate"
            );
        }
        // Neither deadline branch present.
        let no_deadline = PASSING_TIME_LIMIT.replace(
            "broker_wall_deadline=ENFORCED",
            "broker_wall_deadline=FAILED",
        );
        assert!(!worker_gate_assertions_pass(
            PASSING_WORKER_REPORT,
            true,
            &no_deadline
        ));
    }

    #[test]
    fn given_worker_init_crash_exit_then_description_names_the_ntstatus() {
        // The restricted-token LowBox worker exits with this status before it
        // can emit a probe marker; the decoder must make that self-explanatory.
        assert!(describe_unexpected_exit(0xC000_0142).contains("STATUS_DLL_INIT_FAILED"));
        assert!(describe_unexpected_exit(0xC000_0142).contains("0xc0000142"));
        // Any other NTSTATUS error facility code is still flagged as a crash.
        let other = describe_unexpected_exit(0xC000_0001);
        assert!(other.contains("NTSTATUS error"));
        assert!(other.contains("0xc0000001"));
        // Ordinary small exit codes and our own markers carry no annotation.
        assert_eq!(describe_unexpected_exit(1), "");
        assert_eq!(describe_unexpected_exit(0), "");
        assert_eq!(describe_unexpected_exit(NET_PROBE_MARKER), "");
    }

    #[test]
    fn given_init_crash_exit_then_probe_decoders_reject_with_named_status() {
        // Both probe decoders surface the decoded status in their error text.
        let afd = decode_afd_probe(0xC000_0142).unwrap_err();
        assert!(afd.contains("AFD probe marker missing"));
        assert!(afd.contains("STATUS_DLL_INIT_FAILED"));
        let net = decode_probe(0xC000_0142).unwrap_err();
        assert!(net.contains("network probe marker missing"));
        assert!(net.contains("STATUS_DLL_INIT_FAILED"));
    }
}

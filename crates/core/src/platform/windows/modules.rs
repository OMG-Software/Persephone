//! A report of the images mapped into Persephone's processes.
//!
//! Windows and security software can load code into any process. The
//! mitigation policies in `harden.rs` stop hook-based code and images not
//! signed through Microsoft in the core. This scanner lists the other
//! modules, so the user can see them. It makes no network request, shows no
//! UI and never logs a path, because a path can hold the user name.

use persephone_contracts::{ModuleProcess, ModuleSignature};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs::File,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    time::SystemTime,
};
use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_INVALID_PARAMETER, GetLastError, HANDLE, HMODULE, STILL_ACTIVE,
            TRUST_E_NOSIGNATURE, TRUST_E_PROVIDER_UNKNOWN, TRUST_E_SUBJECT_FORM_UNKNOWN,
        },
        Security::{
            Cryptography::{
                BCRYPT_SHA1_ALGORITHM, BCRYPT_SHA256_ALGORITHM, CERT_NAME_SIMPLE_DISPLAY_TYPE,
                Catalog::{
                    CATALOG_INFO, CryptCATAdminAcquireContext2,
                    CryptCATAdminCalcHashFromFileHandle2, CryptCATAdminEnumCatalogFromHash,
                    CryptCATAdminReleaseCatalogContext, CryptCATAdminReleaseContext,
                    CryptCATCatalogInfoFromContext,
                },
                CertGetNameStringW,
            },
            WinTrust::{
                WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_CATALOG_INFO, WINTRUST_DATA,
                WINTRUST_DATA_0, WINTRUST_FILE_INFO, WTD_CACHE_ONLY_URL_RETRIEVAL,
                WTD_CHOICE_CATALOG, WTD_CHOICE_FILE, WTD_REVOCATION_CHECK_NONE, WTD_REVOKE_NONE,
                WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
                WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
            },
        },
        System::{
            ProcessStatus::{K32EnumProcessModulesEx, K32GetModuleFileNameExW, LIST_MODULES_ALL},
            SystemInformation::GetSystemWindowsDirectoryW,
            Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
            },
        },
    },
    core::GUID,
};

/// Module handles asked for first in one process.
const FIRST_MODULES: usize = 1024;
/// The most modules read from one process. A process with more is reported
/// as incomplete.
const MAX_MODULES: usize = 8192;
/// Tries at listing one process's modules while it keeps loading more.
const LIST_ATTEMPTS: usize = 3;
/// Characters in a module path buffer: the longest Windows path.
const PATH_CHARS: usize = 32_768;
/// Characters in a signer name buffer, with the NUL.
const SIGNER_CHARS: usize = 256;
/// The most entries in the signature cache.
const MAX_CACHE: usize = 2048;
/// The most catalogs tried for one file.
const MAX_CATALOGS: usize = 8;
/// Room for the largest hash used, SHA-256.
const HASH_BYTES: usize = 64;

/// One process to scan.
pub struct ScanTarget {
    pub pid: u32,
    pub process: ModuleProcess,
}

/// One module as found, before merging with acknowledgement state.
pub struct FoundModule {
    /// The full path, as the OS reported it.
    pub path: PathBuf,
    pub process: ModuleProcess,
    pub signature: ModuleSignature,
    /// The signer's simple display name, raw. Only for a valid signature.
    pub signer: Option<String>,
}

/// The result of one scan.
pub struct ScanResult {
    pub modules: Vec<FoundModule>,
    /// A target process, or some of its modules, could not be read. A
    /// process that has exited is not counted.
    pub incomplete: bool,
}

/// The modules read from one process.
struct Listed {
    paths: Vec<PathBuf>,
    /// Every module of a running process was read, or the process has gone.
    complete: bool,
}

impl Listed {
    fn gone() -> Self {
        Self {
            paths: Vec::new(),
            complete: true,
        }
    }

    fn unreadable() -> Self {
        Self {
            paths: Vec::new(),
            complete: false,
        }
    }
}

type Cache = HashMap<(String, u64, Option<SystemTime>), (ModuleSignature, Option<String>)>;

/// Lists third-party modules. It keeps signature results between scans.
pub struct Scanner {
    cache: Cache,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    /// Lists third-party modules in the targets. Expected modules are left
    /// out. A process that cannot be opened or fully read makes the result
    /// incomplete; one that has exited does not. Never panics.
    pub fn scan(&mut self, targets: &[ScanTarget]) -> ScanResult {
        let mut incomplete = false;
        let listed: Vec<(ModuleProcess, Vec<PathBuf>)> = targets
            .iter()
            .map(|target| {
                let listed = list_modules(target.pid);
                incomplete |= !listed.complete;
                (target.process, listed.paths)
            })
            .collect();
        let webview_dir = listed
            .iter()
            .filter(|(process, _)| *process == ModuleProcess::WebView)
            .flat_map(|(_, paths)| paths)
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("msedgewebview2.exe"))
            })
            .and_then(|path| path.parent().map(Path::to_path_buf));
        // Without the Windows folder, no module counts as expected under it.
        let system_root = windows_dir().unwrap_or_default();
        let own_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        let mut seen = HashSet::new();
        let mut found = Vec::new();
        for (process, paths) in listed {
            for path in paths {
                if !seen.insert((normalize(&path), process)) {
                    continue;
                }
                let (signature, signer) = self.signature_of(&path);
                if is_expected(
                    &path,
                    signature,
                    signer.as_deref(),
                    &system_root,
                    webview_dir.as_deref(),
                    &own_dir,
                ) {
                    continue;
                }
                found.push(FoundModule {
                    path,
                    process,
                    signature,
                    signer,
                });
            }
        }
        ScanResult {
            modules: found,
            incomplete,
        }
    }

    /// The signature result for the file, from the cache when the file is the
    /// same size and has the same write time.
    fn signature_of(&mut self, path: &Path) -> (ModuleSignature, Option<String>) {
        let Ok(meta) = std::fs::metadata(path) else {
            return (ModuleSignature::Invalid, None);
        };
        let key = (normalize(path), meta.len(), meta.modified().ok());
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }
        let result = check_signature(path);
        if self.cache.len() >= MAX_CACHE {
            self.cache.clear();
        }
        self.cache.insert(key, result.clone());
        result
    }
}

/// The Windows folder from the OS. Not `%SystemRoot%`: whoever starts the
/// core sets its environment, and could make another folder count as
/// Windows.
fn windows_dir() -> Option<PathBuf> {
    let mut buffer = [0u16; 260];
    // SAFETY: the buffer is writable for the stated number of characters.
    let length = unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    let length = usize::try_from(length).ok()?;
    (length > 0 && length < buffer.len())
        .then(|| PathBuf::from(OsString::from_wide(&buffer[..length])))
}

/// Lower case, with `\\?\` removed and `/` as `\`, for comparisons.
fn normalize(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.trim_end_matches('\\').to_lowercase()
}

/// Whether `path` is the folder `dir` or inside it.
fn is_under(path: &str, dir: &str) -> bool {
    !dir.is_empty()
        && path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('\\'))
}

/// Whether a module is expected, so it is not reported. See the report's
/// rules: Microsoft Windows signers, Microsoft Corporation in the system
/// folder or the WebView2 folder, and Persephone's own executables.
pub fn is_expected(
    path: &Path,
    signature: ModuleSignature,
    signer: Option<&str>,
    system_root: &Path,
    webview_dir: Option<&Path>,
    own_dir: &Path,
) -> bool {
    let norm = normalize(path);
    if signature == ModuleSignature::Valid {
        match signer {
            Some("Microsoft Windows" | "Microsoft Windows Publisher") => return true,
            Some("Microsoft Corporation")
                if is_under(&norm, &normalize(system_root))
                    || webview_dir.is_some_and(|dir| is_under(&norm, &normalize(dir))) =>
            {
                return true;
            }
            _ => {}
        }
    }
    let own = normalize(own_dir);
    let own_name = path.file_name().is_some_and(|name| {
        [
            "persephone.exe",
            "persephone-ui.exe",
            "persephone-media-worker.exe",
        ]
        .iter()
        .any(|own| name.eq_ignore_ascii_case(own))
    });
    own_name && is_under(&norm, &own) && norm.rfind('\\').is_some_and(|at| norm[..at] == own)
}

/// The paths of the modules in the process. A process that cannot be
/// opened or listed, has more than [`MAX_MODULES`] modules, or has a module
/// path that does not fit gives an incomplete result, unless it has exited.
fn list_modules(pid: u32) -> Listed {
    // SAFETY: plain call; the result is checked and owned below.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if raw.is_null() {
        // SAFETY: reads the calling thread's last error.
        let error = unsafe { GetLastError() };
        // No such process: it exited after the process list was read.
        return if error == ERROR_INVALID_PARAMETER {
            Listed::gone()
        } else {
            Listed::unreadable()
        };
    }
    // SAFETY: `raw` is a new handle that only this value owns.
    let process = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    let handle = process.as_raw_handle() as HANDLE;
    let Some((modules, all_listed)) = module_handles(handle) else {
        return if has_exited(handle) {
            Listed::gone()
        } else {
            Listed::unreadable()
        };
    };
    let mut listed = read_paths(&modules, |module, buffer| {
        // SAFETY: the buffer holds `buffer.len()` characters, as passed.
        unsafe { K32GetModuleFileNameExW(handle, module, buffer.as_mut_ptr(), buffer.len() as u32) }
    });
    listed.complete &= all_listed;
    listed
}

/// Reads each module's path with `read`, which fills the buffer and returns
/// the length, 0 on failure, like `K32GetModuleFileNameExW`. A failed or
/// full read leaves that path out and makes the result incomplete.
fn read_paths(modules: &[HMODULE], mut read: impl FnMut(HMODULE, &mut [u16]) -> u32) -> Listed {
    let mut buffer = vec![0u16; PATH_CHARS];
    let mut paths = Vec::with_capacity(modules.len());
    let mut complete = true;
    for &module in modules {
        let length = read(module, &mut buffer) as usize;
        if length == 0 || length >= PATH_CHARS {
            // Zero: unloaded after the list was read, or not readable; the
            // two cannot be told apart. Full: the path was cut. Neither path
            // is shown, so the result is not complete.
            complete = false;
            continue;
        }
        paths.push(PathBuf::from(OsString::from_wide(&buffer[..length])));
    }
    Listed { paths, complete }
}

/// The process's module handles, and whether they are all of them. The
/// buffer grows from what the call says it needs, up to [`MAX_MODULES`] and
/// [`LIST_ATTEMPTS`] tries. `None` when the call fails.
fn module_handles(handle: HANDLE) -> Option<(Vec<HMODULE>, bool)> {
    let mut capacity = FIRST_MODULES;
    for attempt in 1..=LIST_ATTEMPTS {
        let mut modules: Vec<HMODULE> = vec![null_mut(); capacity];
        let mut needed = 0u32;
        // SAFETY: the buffer holds `capacity` entries, and its byte size is
        // passed; `needed` lives for the call.
        let ok = unsafe {
            K32EnumProcessModulesEx(
                handle,
                modules.as_mut_ptr(),
                u32::try_from(capacity * size_of::<HMODULE>()).ok()?,
                &mut needed,
                LIST_MODULES_ALL,
            )
        };
        if ok == 0 {
            return None;
        }
        let wanted = needed as usize / size_of::<HMODULE>();
        if wanted <= capacity {
            modules.truncate(wanted);
            return Some((modules, true));
        }
        if capacity == MAX_MODULES || attempt == LIST_ATTEMPTS {
            // The buffer is full: list what was read, as incomplete.
            return Some((modules, false));
        }
        // Room for a few more, in case it loads more before the next call.
        capacity = (wanted + 64).min(MAX_MODULES);
    }
    None
}

/// Whether the process has exited.
fn has_exited(handle: HANDLE) -> bool {
    let mut code = 0u32;
    // SAFETY: `handle` is open with query rights; `code` lives for the call.
    let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
    ok != 0 && code != STILL_ACTIVE as u32
}

/// A WinVerifyTrust state, closed on drop so no path leaks it.
struct Verification {
    data: WINTRUST_DATA,
    action: GUID,
    open: bool,
}

impl Verification {
    /// Verifies the file in `data` offline. Returns the status, and the signer
    /// name when the status is 0.
    fn run(data: WINTRUST_DATA) -> (i32, Option<String>) {
        let mut this = Self {
            data,
            action: WINTRUST_ACTION_GENERIC_VERIFY_V2,
            open: false,
        };
        this.data.cbStruct = size_of::<WINTRUST_DATA>() as u32;
        this.data.dwUIChoice = WTD_UI_NONE;
        this.data.fdwRevocationChecks = WTD_REVOKE_NONE;
        this.data.dwProvFlags = WTD_REVOCATION_CHECK_NONE | WTD_CACHE_ONLY_URL_RETRIEVAL;
        this.data.dwStateAction = WTD_STATEACTION_VERIFY;
        this.open = true;
        // SAFETY: `data` is a complete WINTRUST_DATA whose file or catalog
        // record outlives this call (the caller holds it); -1 means no window.
        let status = unsafe {
            WinVerifyTrust(
                usize::MAX as _,
                &raw mut this.action,
                (&raw mut this.data).cast(),
            )
        };
        let signer = (status == 0)
            .then(|| signer_name(this.data.hWVTStateData))
            .flatten();
        (status, signer)
    }
}

impl Drop for Verification {
    fn drop(&mut self) {
        if self.open {
            self.data.dwStateAction = WTD_STATEACTION_CLOSE;
            // SAFETY: closes the state opened by the verify call on this data.
            unsafe {
                WinVerifyTrust(
                    usize::MAX as _,
                    &raw mut self.action,
                    (&raw mut self.data).cast(),
                )
            };
        }
    }
}

/// The leaf signer's simple display name from an open verification state.
fn signer_name(state: HANDLE) -> Option<String> {
    // SAFETY: `state` is the open state of a finished verify call; every
    // pointer below is checked for null before use, and the chain data stays
    // valid until the state is closed, which the caller does afterwards.
    unsafe {
        let provider = WTHelperProvDataFromStateData(state);
        if provider.is_null() {
            return None;
        }
        let signer = WTHelperGetProvSignerFromChain(provider, 0, 0, 0);
        if signer.is_null() || (*signer).csCertChain == 0 || (*signer).pasCertChain.is_null() {
            return None;
        }
        let cert = (*(*signer).pasCertChain).pCert;
        if cert.is_null() {
            return None;
        }
        let mut name = [0u16; SIGNER_CHARS];
        let length = CertGetNameStringW(
            cert,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            null(),
            name.as_mut_ptr(),
            SIGNER_CHARS as u32,
        ) as usize;
        // The count includes the NUL; 1 means an empty name.
        (length > 1 && length <= SIGNER_CHARS)
            .then(|| String::from_utf16_lossy(&name[..length - 1]))
    }
}

/// Checks the file's signature with no network and no UI: the embedded
/// signature first, then the system catalogs.
fn check_signature(path: &Path) -> (ModuleSignature, Option<String>) {
    let Ok(file) = File::open(path) else {
        return (ModuleSignature::Invalid, None);
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: wide.as_ptr(),
        hFile: file.as_raw_handle() as HANDLE,
        pgKnownSubject: null_mut(),
    };
    let data = WINTRUST_DATA {
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &raw mut info,
        },
        ..Default::default()
    };
    let (status, signer) = Verification::run(data);
    if status == 0 {
        return (ModuleSignature::Valid, signer);
    }
    if !matches!(
        status,
        TRUST_E_NOSIGNATURE | TRUST_E_PROVIDER_UNKNOWN | TRUST_E_SUBJECT_FORM_UNKNOWN
    ) {
        return (ModuleSignature::Invalid, None);
    }
    for algorithm in [BCRYPT_SHA256_ALGORITHM, BCRYPT_SHA1_ALGORITHM] {
        if let Some(result) = check_catalogs(&file, &wide, algorithm) {
            return result;
        }
    }
    (ModuleSignature::Unsigned, None)
}

/// A catalog admin context, released on drop.
struct CatAdmin(isize);

impl Drop for CatAdmin {
    fn drop(&mut self) {
        // SAFETY: the context came from a successful acquire and is released
        // once.
        unsafe { CryptCATAdminReleaseContext(self.0, 0) };
    }
}

/// Looks the file up in the system catalogs by its hash. `None` means no
/// catalog lists it. Otherwise the result is the catalog signature's.
fn check_catalogs(
    file: &File,
    path: &[u16],
    algorithm: windows_sys::core::PCWSTR,
) -> Option<(ModuleSignature, Option<String>)> {
    let mut raw = 0isize;
    // SAFETY: `raw` lives for the call; the algorithm is a constant string.
    let ok = unsafe { CryptCATAdminAcquireContext2(&mut raw, null(), algorithm, null(), 0) };
    if ok == 0 || raw == 0 {
        return None;
    }
    let admin = CatAdmin(raw);
    let mut hash = [0u8; HASH_BYTES];
    let mut length = HASH_BYTES as u32;
    // SAFETY: the buffer holds `length` bytes; the file handle is open.
    let ok = unsafe {
        CryptCATAdminCalcHashFromFileHandle2(
            admin.0,
            file.as_raw_handle() as HANDLE,
            &mut length,
            hash.as_mut_ptr(),
            0,
        )
    };
    if ok == 0 || length == 0 || length as usize > HASH_BYTES {
        return None;
    }
    let hash = &mut hash[..length as usize];
    let tag: Vec<u16> = hash
        .iter()
        .flat_map(|byte| format!("{byte:02X}").encode_utf16().collect::<Vec<_>>())
        .chain([0])
        .collect();
    let mut previous = 0isize;
    let mut outcome = None;
    for _ in 0..MAX_CATALOGS {
        let previous_ptr = if previous == 0 {
            null_mut()
        } else {
            &raw mut previous
        };
        // SAFETY: the hash slice is valid for its length; a non-null previous
        // context is one this loop got, and the call releases it.
        let catalog = unsafe {
            CryptCATAdminEnumCatalogFromHash(admin.0, hash.as_ptr(), length, 0, previous_ptr)
        };
        if catalog == 0 {
            previous = 0;
            break;
        }
        previous = catalog;
        let mut catalog_info = CATALOG_INFO {
            cbStruct: size_of::<CATALOG_INFO>() as u32,
            wszCatalogFile: [0; 260],
        };
        // SAFETY: `catalog` is a live context; the structure is sized.
        if unsafe { CryptCATCatalogInfoFromContext(catalog, &mut catalog_info, 0) } == 0 {
            continue;
        }
        let mut member = WINTRUST_CATALOG_INFO {
            cbStruct: size_of::<WINTRUST_CATALOG_INFO>() as u32,
            dwCatalogVersion: 0,
            pcwszCatalogFilePath: catalog_info.wszCatalogFile.as_ptr(),
            pcwszMemberTag: tag.as_ptr(),
            pcwszMemberFilePath: path.as_ptr(),
            hMemberFile: file.as_raw_handle() as HANDLE,
            pbCalculatedFileHash: hash.as_mut_ptr(),
            cbCalculatedFileHash: length,
            pcCatalogContext: null_mut(),
            hCatAdmin: admin.0,
        };
        let data = WINTRUST_DATA {
            dwUnionChoice: WTD_CHOICE_CATALOG,
            Anonymous: WINTRUST_DATA_0 {
                pCatalog: &raw mut member,
            },
            ..Default::default()
        };
        let (status, signer) = Verification::run(data);
        if status == 0 {
            outcome = Some((ModuleSignature::Valid, signer));
            break;
        }
        outcome = Some((ModuleSignature::Invalid, None));
    }
    if previous != 0 {
        // SAFETY: the last context is still held; it is released once.
        unsafe { CryptCATAdminReleaseCatalogContext(admin.0, previous, 0) };
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::c_void, fs};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
    }

    const SYSTEM_ROOT: &str = r"C:\Windows";
    const OWN_DIR: &str = r"C:\Apps\Persephone";

    fn expected(
        path: &str,
        signature: ModuleSignature,
        signer: Option<&str>,
        webview: Option<&str>,
    ) -> bool {
        is_expected(
            Path::new(path),
            signature,
            signer,
            Path::new(SYSTEM_ROOT),
            webview.map(Path::new),
            Path::new(OWN_DIR),
        )
    }

    /// GIVEN a list of module descriptions
    /// WHEN each one is classified
    /// THEN only Windows signers, Microsoft files in the system or WebView2
    /// folder, and Persephone's own executables are expected
    #[test]
    fn expected_modules_follow_the_rules() {
        use ModuleSignature::{Invalid, Unsigned, Valid};
        let webview = Some(r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application\1.0");
        let cases = [
            (r"C:\x\a.dll", Valid, Some("Microsoft Windows"), None, true),
            (
                r"C:\x\a.dll",
                Valid,
                Some("Microsoft Windows Publisher"),
                None,
                true,
            ),
            (
                r"C:\x\a.dll",
                Unsigned,
                Some("Microsoft Windows"),
                None,
                false,
            ),
            (r"C:\x\a.dll", Invalid, None, None, false),
            (
                r"C:\Windows\System32\v.dll",
                Valid,
                Some("Microsoft Corporation"),
                None,
                true,
            ),
            (
                r"\\?\c:\windows\system32\v.dll",
                Valid,
                Some("Microsoft Corporation"),
                None,
                true,
            ),
            (
                r"C:\Windows\System32\hmpalert.dll",
                Valid,
                Some("Sophos Ltd"),
                None,
                false,
            ),
            (r"C:\Windows\System32\x.dll", Unsigned, None, None, false),
            (
                r"C:\Windowsx\a.dll",
                Valid,
                Some("Microsoft Corporation"),
                None,
                false,
            ),
            (
                r"C:\x\a.dll",
                Valid,
                Some("Microsoft Corporation"),
                None,
                false,
            ),
            (
                r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application\1.0\a.dll",
                Valid,
                Some("Microsoft Corporation"),
                webview,
                true,
            ),
            (
                r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application\1.0\a.dll",
                Valid,
                Some("Other Corp"),
                webview,
                false,
            ),
            (
                r"C:\Apps\Persephone\persephone.exe",
                Unsigned,
                None,
                None,
                true,
            ),
            (
                r"c:\apps\persephone\PERSEPHONE-UI.EXE",
                Unsigned,
                None,
                None,
                true,
            ),
            (
                r"C:\Apps\Persephone\persephone-media-worker.exe",
                Invalid,
                None,
                None,
                true,
            ),
            (r"C:\Apps\Persephone\other.dll", Unsigned, None, None, false),
            (r"C:\Elsewhere\persephone.exe", Unsigned, None, None, false),
            (
                r"C:\Apps\Persephone\sub\persephone.exe",
                Unsigned,
                None,
                None,
                false,
            ),
        ];
        for (path, signature, signer, webview, want) in cases {
            assert_eq!(
                expected(path, signature, signer, webview),
                want,
                "{path} {signature:?} {signer:?}"
            );
        }
    }

    fn self_target() -> [ScanTarget; 1] {
        [ScanTarget {
            pid: std::process::id(),
            process: ModuleProcess::Core,
        }]
    }

    /// GIVEN the test process, which has Microsoft system DLLs mapped
    /// WHEN it is scanned
    /// THEN kernel32.dll and ntdll.dll are not reported
    #[test]
    fn a_self_scan_omits_microsoft_system_modules() {
        let result = Scanner::new().scan(&self_target());
        assert!(!result.incomplete, "the test process was not fully read");
        for module in &result.modules {
            let name = module.path.file_name().expect("file name");
            assert!(
                !name.eq_ignore_ascii_case("kernel32.dll")
                    && !name.eq_ignore_ascii_case("ntdll.dll"),
                "system module reported"
            );
            assert_eq!(module.process, ModuleProcess::Core);
        }
    }

    /// GIVEN a process that does not exist, as one that exited after the
    /// list was read
    /// WHEN it is scanned
    /// THEN nothing is reported, nothing fails and the scan is complete
    #[test]
    fn a_process_that_has_gone_is_skipped() {
        let target = ScanTarget {
            pid: u32::MAX - 1,
            process: ModuleProcess::Ui,
        };
        let result = Scanner::new().scan(&[target]);
        assert!(result.modules.is_empty());
        assert!(!result.incomplete);
    }

    /// GIVEN three modules WHEN one path read fails, or one fills the buffer
    /// THEN that path is left out and the result is incomplete; WHEN every
    /// read works THEN all paths are listed and it is complete (the control)
    #[test]
    fn a_failed_or_cut_path_read_makes_the_result_incomplete() {
        let modules: Vec<HMODULE> = (1..=3usize).map(|n| n as HMODULE).collect();
        let reader = |bad: Option<(usize, u32)>| {
            move |module: HMODULE, buffer: &mut [u16]| -> u32 {
                let n = module as usize;
                if let Some((which, length)) = bad
                    && which == n
                {
                    return length;
                }
                let path: Vec<u16> = format!(r"C:\x\m{n}.dll").encode_utf16().collect();
                buffer[..path.len()].copy_from_slice(&path);
                path.len() as u32
            }
        };
        let all = read_paths(&modules, reader(None));
        assert!(all.complete);
        assert_eq!(all.paths.len(), 3);
        for (length, case) in [(0, "failed"), (PATH_CHARS as u32, "cut")] {
            let listed = read_paths(&modules, reader(Some((2, length))));
            assert!(!listed.complete, "{case}");
            assert_eq!(
                listed.paths,
                [PathBuf::from(r"C:\x\m1.dll"), PathBuf::from(r"C:\x\m3.dll")],
                "{case}"
            );
        }
    }

    /// GIVEN the System process (pid 4), which a user process cannot open
    /// for reading, and the test process
    /// WHEN both are scanned
    /// THEN the scan is incomplete, and the readable process is still listed
    #[test]
    fn an_unreadable_process_makes_the_scan_incomplete() {
        let targets = [
            ScanTarget {
                pid: 4,
                process: ModuleProcess::WebView,
            },
            ScanTarget {
                pid: std::process::id(),
                process: ModuleProcess::Core,
            },
        ];
        let result = Scanner::new().scan(&targets);
        assert!(result.incomplete);
        assert!(
            result
                .modules
                .iter()
                .all(|module| module.process == ModuleProcess::Core)
        );
        // The control: the same process alone reads fully.
        assert!(!Scanner::new().scan(&targets[1..]).incomplete);
    }

    /// GIVEN Microsoft's kernel32.dll, signed through a catalog
    /// WHEN its signature is checked
    /// THEN it is valid, with the signer "Microsoft Windows"
    #[test]
    fn kernel32_has_a_valid_microsoft_signature() {
        let (signature, signer) = check_signature(Path::new(r"C:\Windows\System32\kernel32.dll"));
        assert_eq!(signature, ModuleSignature::Valid);
        assert_eq!(signer.as_deref(), Some("Microsoft Windows"));
    }

    /// GIVEN a copy of version.dll with one byte of its DOS stub changed,
    /// loaded into the test process (the negative control)
    /// WHEN the process is scanned
    /// THEN the copy is reported as unsigned or invalid, with no signer
    #[test]
    fn a_tampered_microsoft_dll_is_reported() {
        let dir = std::env::temp_dir().join(format!("persephone-modules-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        let mut bytes = fs::read(r"C:\Windows\System32\version.dll").expect("read version.dll");
        let marker = b"This program cannot be run";
        let at = bytes
            .windows(marker.len())
            .position(|window| window == marker)
            .expect("DOS stub text");
        bytes[at] = b't';
        let copy = dir.join("version.dll");
        fs::write(&copy, &bytes).expect("write copy");
        let wide: Vec<u16> = copy.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; the file
        // handle is reserved and must be null.
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), null_mut(), 0) };
        assert!(!module.is_null(), "copy did not load");
        let found = Scanner::new().scan(&self_target()).modules;
        // SAFETY: `module` came from the successful load above.
        unsafe { FreeLibrary(module) };
        let _ = fs::remove_dir_all(&dir);
        let reported = found
            .iter()
            .find(|module| normalize(&module.path) == normalize(&copy))
            .expect("tampered copy reported");
        assert!(matches!(
            reported.signature,
            ModuleSignature::Unsigned | ModuleSignature::Invalid
        ));
        assert_eq!(reported.signer, None);
    }
}

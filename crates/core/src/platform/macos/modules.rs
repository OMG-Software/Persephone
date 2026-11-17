//! A report of the code loaded in Persephone's core on macOS (ADR-0047).
//!
//! The core lists its own loaded images through dyld and checks each one
//! that is not expected with the code-signing API:
//!
//! * Expected, not reported: an image in the dyld shared cache; Persephone's
//!   own executables in the core's folder; Apple's own code (the requirement
//!   `anchor apple`); and code with a valid signature inside the `.app`
//!   bundle the core runs from.
//! * Reported: any other image, with its signature state and, for a valid
//!   signature, the signer's name from the leaf certificate.
//!
//! The UI and web view processes cannot be read without the debugging
//! rights the release build does not hold, so every report is incomplete.
//! The release build is signed with the hardened runtime, which already
//! refuses a library that is neither Apple's nor signed by the same team;
//! this report shows what is loaded, it does not enforce anything.
//!
//! The scanner makes no network request, shows no UI and never logs a path,
//! because a path can hold the user name. A signature check reads the whole
//! file, so its result is kept while the file's identity is unchanged.

use crate::modules::Found;
use core_foundation::{
    array::{CFArrayGetCount, CFArrayGetTypeID, CFArrayGetValueAtIndex, CFArrayRef},
    base::{CFGetTypeID, CFRelease, OSStatus, TCFType},
    dictionary::{CFDictionaryGetValue, CFDictionaryRef},
    string::CFStringRef,
    url::CFURL,
};
use persephone_contracts::{ModuleProcess, ModuleReason, ModuleSignature};
use security_framework::{
    certificate::SecCertificate,
    os::macos::code_signing::{Flags, SecRequirement, SecStaticCode},
};
use security_framework_sys::{
    certificate::SecCertificateGetTypeID, code_signing::SecStaticCodeRef,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CStr, CString, OsStr, c_char},
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    ptr,
    str::FromStr,
};

/// The most images listed in one scan. More is incomplete.
const MAX_IMAGES: u32 = 8192;
/// Persephone's own executables, expected in the core's folder.
const OWN_NAMES: [&str; 3] = ["persephone", "persephone-ui", "persephone-media-worker"];
/// `errSecCSUnsigned` in `CSCommon.h`: the code has no signature.
const ERR_SEC_CS_UNSIGNED: OSStatus = -67062;
/// `kSecCSSigningInformation` in `SecCode.h`: include the certificates.
const SIGNING_INFORMATION: u32 = 1 << 1;

// dyld's image list is part of libSystem, which every program links.
unsafe extern "C" {
    /// `uint32_t _dyld_image_count(void)`.
    fn _dyld_image_count() -> u32;
    /// `const char* _dyld_get_image_name(uint32_t image_index)`: null when
    /// the index is out of range.
    fn _dyld_get_image_name(image_index: u32) -> *const c_char;
    /// `bool _dyld_shared_cache_contains_path(const char* path)` (macOS 11).
    fn _dyld_shared_cache_contains_path(path: *const c_char) -> bool;
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    /// The key of the certificate chain, leaf first, in the signing
    /// information.
    static kSecCodeInfoCertificates: CFStringRef;
    /// `OSStatus SecCodeCopySigningInformation(SecStaticCodeRef code,
    /// SecCSFlags flags, CFDictionaryRef *information)`.
    fn SecCodeCopySigningInformation(
        code: SecStaticCodeRef,
        flags: u32,
        information: *mut CFDictionaryRef,
    ) -> OSStatus;
}

/// One process to scan.
pub struct ScanTarget {
    pub pid: u32,
    pub process: ModuleProcess,
}

/// The result of one scan.
pub struct ScanResult {
    pub modules: Vec<Found>,
    /// Always true on macOS: only the core can be read.
    pub incomplete: bool,
}

/// The state of a file that a signature check depends on. A change to any
/// part checks the file again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Identity {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

/// What a check found: `None` is expected code.
type Verdict = Option<(ModuleSignature, Option<String>)>;

/// Where the core runs from.
struct Own {
    /// The folder of the core's executable.
    dir: Option<PathBuf>,
    /// The `.app` bundle around the core's executable, if any.
    bundle: Option<PathBuf>,
}

impl Own {
    fn read() -> Self {
        let exe = std::env::current_exe().and_then(fs::canonicalize).ok();
        let dir = exe.as_deref().and_then(Path::parent).map(Path::to_path_buf);
        let bundle = exe
            .as_deref()
            .and_then(|exe| {
                exe.ancestors()
                    .find(|folder| folder.extension().is_some_and(|ext| ext == "app"))
            })
            .map(Path::to_path_buf);
        Self { dir, bundle }
    }

    fn is_own_executable(&self, path: &Path) -> bool {
        self.dir.is_some()
            && path.parent() == self.dir.as_deref()
            && path
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| OWN_NAMES.contains(&name))
    }

    fn in_bundle(&self, path: &Path) -> bool {
        self.bundle
            .as_deref()
            .is_some_and(|bundle| path.starts_with(bundle))
    }
}

/// The paths of the images dyld has loaded in this process, at most
/// [`MAX_IMAGES`]. dyld's list is not locked while it is read, so an
/// image unloaded meanwhile can be missed; the core does not unload images.
fn loaded_images() -> BTreeSet<CString> {
    // SAFETY: no arguments; returns a count.
    let count = unsafe { _dyld_image_count() };
    let mut images = BTreeSet::new();
    for index in 0..count.min(MAX_IMAGES) {
        // SAFETY: any index is allowed; an index out of range gives null.
        let name = unsafe { _dyld_get_image_name(index) };
        if name.is_null() {
            continue;
        }
        // SAFETY: dyld returns a NUL-terminated path that stays valid while
        // the image is loaded; it is copied at once.
        images.insert(unsafe { CStr::from_ptr(name) }.to_owned());
    }
    images
}

/// True when the path names an image in the dyld shared cache: Apple's own
/// libraries, which have no file of their own on disk.
pub fn in_shared_cache(path: &CStr) -> bool {
    // SAFETY: a valid NUL-terminated string that lives across the call.
    unsafe { _dyld_shared_cache_contains_path(path.as_ptr()) }
}

/// The common name of the leaf certificate of signed code.
fn signer_name(code: &SecStaticCode) -> Option<String> {
    let mut information: CFDictionaryRef = ptr::null();
    // SAFETY: a valid static code object and an out pointer; on success the
    // dictionary is owned by the caller and released below.
    let status = unsafe {
        SecCodeCopySigningInformation(
            code.as_concrete_TypeRef(),
            SIGNING_INFORMATION,
            &mut information,
        )
    };
    if status != 0 || information.is_null() {
        return None;
    }
    // SAFETY: the dictionary is valid until it is released at the end; the
    // values read from it are checked for their type before use, and the
    // certificate is retained by `wrap_under_get_rule` before the release.
    let name = unsafe {
        let chain = CFDictionaryGetValue(information, kSecCodeInfoCertificates.cast());
        let leaf = if chain.is_null() || CFGetTypeID(chain) != CFArrayGetTypeID() {
            None
        } else {
            let chain: CFArrayRef = chain.cast();
            (CFArrayGetCount(chain) > 0)
                .then(|| CFArrayGetValueAtIndex(chain, 0))
                .filter(|leaf| !leaf.is_null() && CFGetTypeID(*leaf) == SecCertificateGetTypeID())
                .map(|leaf| SecCertificate::wrap_under_get_rule(leaf.cast_mut().cast()))
        };
        let name = leaf.map(|leaf| leaf.subject_summary());
        CFRelease(information.cast());
        name
    };
    name.filter(|name| !name.is_empty())
}

/// Checks the signature of one file.
fn check(own: &Own, path: &Path, apple: &SecRequirement, any: &SecRequirement) -> Verdict {
    let Some(url) = CFURL::from_path(path, false) else {
        return Some((ModuleSignature::Invalid, None));
    };
    let Ok(code) = SecStaticCode::from_path(&url, Flags::NONE) else {
        return Some((ModuleSignature::Invalid, None));
    };
    if code.check_validity(Flags::NONE, apple).is_ok() {
        return None;
    }
    match code.check_validity(Flags::NONE, any) {
        Ok(()) if own.in_bundle(path) => None,
        Ok(()) => Some((ModuleSignature::Valid, signer_name(&code))),
        Err(error) if error.code() == ERR_SEC_CS_UNSIGNED => {
            Some((ModuleSignature::Unsigned, None))
        }
        Err(_) => Some((ModuleSignature::Invalid, None)),
    }
}

/// Lists the code in Persephone's core that is not expected.
pub struct Scanner {
    own: Own,
    /// The last verdict for each path, kept while its identity holds.
    verdicts: BTreeMap<PathBuf, (Identity, Verdict)>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    pub fn new() -> Self {
        Self {
            own: Own::read(),
            verdicts: BTreeMap::new(),
        }
    }

    /// Lists the unexpected code in the core, checking `stop` between
    /// files. Only a target that is this process is read; the report is
    /// always incomplete. `None` when it stopped: the partial result is
    /// dropped. Never panics on what it reads.
    pub fn scan_until(
        &mut self,
        targets: &[ScanTarget],
        stop: &dyn Fn() -> bool,
    ) -> Option<ScanResult> {
        let own_pid = std::process::id();
        let mut modules = Vec::new();
        for target in targets.iter().filter(|target| target.pid == own_pid) {
            if stop() {
                return None;
            }
            modules.extend(self.list(target.process, stop)?);
        }
        Some(ScanResult {
            modules,
            incomplete: true,
        })
    }

    fn list(&mut self, process: ModuleProcess, stop: &dyn Fn() -> bool) -> Option<Vec<Found>> {
        // The requirements are created per scan: Core Foundation objects are
        // not `Send`, and the scanner moves to the scan thread.
        let (Ok(apple), Ok(any)) = (
            SecRequirement::from_str("anchor apple"),
            SecRequirement::from_str("always"),
        ) else {
            return Some(Vec::new());
        };
        let images = loaded_images();
        let mut seen = BTreeSet::new();
        let mut modules = Vec::new();
        for image in images {
            if stop() {
                return None;
            }
            if in_shared_cache(&image) {
                continue;
            }
            let raw = PathBuf::from(OsStr::from_bytes(image.to_bytes()));
            let path = fs::canonicalize(&raw).unwrap_or(raw);
            if !seen.insert(path.clone()) || self.own.is_own_executable(&path) {
                continue;
            }
            let verdict = match fs::metadata(&path) {
                Ok(metadata) => {
                    let identity = Identity::of(&metadata);
                    match self.verdicts.get(&path) {
                        Some((known, verdict)) if *known == identity => verdict.clone(),
                        _ => {
                            let verdict = check(&self.own, &path, &apple, &any);
                            self.verdicts
                                .insert(path.clone(), (identity, verdict.clone()));
                            verdict
                        }
                    }
                }
                // Loaded, but its file is gone or unreadable: it cannot be
                // checked, so it is not trusted.
                Err(_) => Some((ModuleSignature::Invalid, None)),
            };
            if let Some((signature, signer)) = verdict {
                modules.push(Found {
                    path,
                    process,
                    signature,
                    signer,
                    reason: ModuleReason::Signer,
                });
            }
        }
        // Verdicts of images no longer loaded are dropped, so the cache
        // stays bounded by the image list.
        self.verdicts.retain(|path, _| seen.contains(path));
        Some(modules)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core_target() -> ScanTarget {
        ScanTarget {
            pid: std::process::id(),
            process: ModuleProcess::Core,
        }
    }

    /// GIVEN libSystem THEN dyld names it as part of the shared cache.
    #[test]
    fn given_libsystem_then_it_is_in_the_shared_cache() {
        assert!(in_shared_cache(c"/usr/lib/libSystem.B.dylib"));
        assert!(!in_shared_cache(c"/nonexistent/libnothing.dylib"));
    }

    /// GIVEN this test process WHEN it is scanned as the core THEN the
    /// shared cache is not listed, the test executable (not one of
    /// Persephone's own names) is listed, and the report is incomplete
    /// because the other processes cannot be read.
    #[test]
    fn given_this_process_then_only_unexpected_images_are_listed() {
        let mut scanner = Scanner::new();
        let result = scanner
            .scan_until(&[core_target()], &|| false)
            .expect("not stopped");
        assert!(result.incomplete);
        let exe = fs::canonicalize(std::env::current_exe().expect("exe")).expect("canonical");
        assert!(
            result
                .modules
                .iter()
                .any(|module| module.path == exe && module.process == ModuleProcess::Core),
            "the test executable is listed"
        );
        assert!(
            result
                .modules
                .iter()
                .all(|module| !module.path.starts_with("/usr/lib/libSystem")),
            "the shared cache is expected"
        );
        assert!(
            result
                .modules
                .iter()
                .all(|module| module.reason == ModuleReason::Signer)
        );
        // A second scan reuses the verdicts and gives the same rows.
        let again = scanner
            .scan_until(&[core_target()], &|| false)
            .expect("not stopped");
        assert_eq!(again.modules, result.modules);
    }

    /// GIVEN a target that is another process THEN it is not read and the
    /// report is incomplete; GIVEN a stop request THEN no result is given.
    #[test]
    fn given_another_process_or_a_stop_then_nothing_is_read() {
        let mut scanner = Scanner::new();
        let other = ScanTarget {
            pid: 1,
            process: ModuleProcess::Ui,
        };
        let result = scanner
            .scan_until(&[other], &|| false)
            .expect("not stopped");
        assert!(result.modules.is_empty());
        assert!(result.incomplete);
        assert!(scanner.scan_until(&[core_target()], &|| true).is_none());
    }
}

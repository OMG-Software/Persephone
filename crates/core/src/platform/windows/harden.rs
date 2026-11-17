//! Process hardening against injected DLLs.
//!
//! Third-party software can load a DLL into any process of the user, for
//! example through a global window hook. The core holds the keys, so it
//! refuses such images. [`block_injected_dlls`] sets three process mitigation
//! policies. Windows never lets a process undo them.

use std::{ffi::c_void, io};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessMitigationPolicy, PROCESS_MITIGATION_POLICY,
    ProcessExtensionPointDisablePolicy, ProcessImageLoadPolicy, ProcessSignaturePolicy,
    SetProcessMitigationPolicy,
};

/// Each policy and the flags it must have. Every structure is one 32-bit
/// flags field.
const POLICIES: [(PROCESS_MITIGATION_POLICY, u32); 3] = [
    // Bit 0, `DisableExtensionPoints`: no global window hook, AppInit,
    // legacy IME or Winsock LSP DLLs, whoever signed them.
    (ProcessExtensionPointDisablePolicy, 0b1),
    // Bits 0 to 2: `NoRemoteImages` (no image from a network share),
    // `NoLowMandatoryLabel` (no image file labelled low integrity, such as
    // one a sandboxed process wrote) and `PreferSystem32Images` (System32
    // first, so a signed DLL planted next to the executable is not loaded
    // in its place).
    (ProcessImageLoadPolicy, 0b111),
    // Bit 0, `MicrosoftSignedOnly`: images must be signed through Microsoft.
    (ProcessSignaturePolicy, 0b1),
];

/// Blocks, for the rest of this process's life, the legacy extension points
/// (global window hooks, AppInit DLLs, legacy IMEs, Winsock LSPs), images
/// from network shares or labelled low integrity, and images not signed by
/// Microsoft, and makes DLL searches prefer System32. Modules loaded before
/// the call stay loaded. Returns an error if any policy cannot be set or is
/// not fully reported on afterwards.
pub fn block_injected_dlls() -> io::Result<()> {
    for (policy, flags) in POLICIES {
        // SAFETY: the buffer is a live u32, the size of each policy structure.
        let ok = unsafe {
            SetProcessMitigationPolicy(
                policy,
                (&raw const flags).cast::<c_void>(),
                size_of::<u32>(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    for (policy, wanted) in POLICIES {
        let mut flags = 0u32;
        // SAFETY: the pseudo handle needs no release; the buffer is a live
        // u32, the size of each policy structure.
        let ok = unsafe {
            GetProcessMitigationPolicy(
                GetCurrentProcess(),
                policy,
                (&raw mut flags).cast::<c_void>(),
                size_of::<u32>(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if flags & wanted != wanted {
            return Err(io::Error::other("a mitigation policy is not in effect"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::block_injected_dlls;
    use std::{
        ffi::{OsString, c_void},
        fs,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::{Path, PathBuf},
        process::Command,
    };

    const PROBE_ENV: &str = "PERSEPHONE_HARDEN_PROBE_DIR";
    const HELPER: &str = "platform::windows::harden::tests::probe_child";
    const ERROR_INVALID_IMAGE_HASH: u32 = 577;
    const ERROR_ACCESS_DENIED: u32 = 5;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
        fn GetLastError() -> u32;
        fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
    }

    /// The path of `name` in the installed Windows system directory, which
    /// need not be on drive C.
    fn system_dll(name: &str) -> PathBuf {
        let mut buffer = [0u16; 260];
        // SAFETY: the buffer is writable for the stated number of UTF-16 units.
        let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        let len = usize::try_from(len).expect("u32 fits usize");
        assert!(len > 0 && len < buffer.len(), "system directory not read");
        PathBuf::from(OsString::from_wide(&buffer[..len])).join(name)
    }

    /// Loads the DLL at `path`; `Err` carries the Win32 error code.
    fn load(path: &Path) -> Result<*mut c_void, u32> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; the file
        // handle is reserved and must be null.
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), 0) };
        if module.is_null() {
            // SAFETY: reads the calling thread's last error.
            Err(unsafe { GetLastError() })
        } else {
            Ok(module)
        }
    }

    fn free(module: *mut c_void) {
        // SAFETY: `module` came from a successful load in this process.
        assert_ne!(unsafe { FreeLibrary(module) }, 0);
    }

    /// Runs in the child process only. The policy is one-way, so the shared
    /// test process must never apply it.
    ///
    /// GIVEN altered copies of a Microsoft DLL, intact copies of it with and
    /// without a low integrity label, and a genuine one
    /// WHEN the process blocks injected DLLs
    /// THEN an altered and a low-labelled copy loaded before; afterwards the
    /// altered copy fails with ERROR_INVALID_IMAGE_HASH, the low-labelled
    /// copy is refused, and the unlabelled copy and the genuine DLL load
    #[test]
    #[ignore = "runs in a child process of hardening_blocks_unsigned_images"]
    fn probe_child() {
        let Some(dir) = std::env::var_os(PROBE_ENV) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let before = load(&dir.join("probe-a.dll")).expect("altered DLL loads before hardening");
        free(before);
        let low = load(&dir.join("low-a.dll")).expect("low-labelled DLL loads before hardening");
        free(low);
        block_injected_dlls().expect("policies set");
        let after = load(&dir.join("probe-b.dll"));
        assert_eq!(after.err(), Some(ERROR_INVALID_IMAGE_HASH));
        let low = load(&dir.join("low-b.dll"));
        assert_eq!(low.err(), Some(ERROR_ACCESS_DENIED), "low-labelled image");
        let plain = load(&dir.join("plain-b.dll")).expect("an unlabelled intact copy loads");
        free(plain);
        let genuine = load(&system_dll("msimg32.dll")).expect("Microsoft-signed DLL still loads");
        free(genuine);
    }

    /// GIVEN altered copies of a Microsoft DLL with an invalid signature, and
    /// intact copies with and without a low integrity label
    /// WHEN a child process hardens itself and then loads them
    /// THEN the altered and low-labelled loads are refused, while the same
    /// kinds of load worked before
    #[test]
    fn hardening_blocks_unsigned_images() {
        let dir = std::env::temp_dir().join(format!("persephone-harden-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        let original = fs::read(system_dll("version.dll")).expect("read version.dll");
        let marker = b"This program cannot be run";
        let at = original
            .windows(marker.len())
            .position(|window| window == marker)
            .expect("DOS stub text");
        for name in ["low-a.dll", "low-b.dll", "plain-b.dll"] {
            fs::write(dir.join(name), &original).expect("write intact copy");
        }
        for name in ["low-a.dll", "low-b.dll"] {
            let labelled = Command::new("icacls")
                .arg(dir.join(name))
                .args(["/setintegritylevel", "low"])
                .output()
                .expect("start icacls");
            assert!(labelled.status.success(), "icacls failed for {name}");
        }
        let mut altered = original;
        altered[at] = b't';
        for name in ["probe-a.dll", "probe-b.dll"] {
            fs::write(dir.join(name), &altered).expect("write probe");
        }
        let output = Command::new(std::env::current_exe().expect("test exe"))
            .args([
                "--exact",
                HELPER,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PROBE_ENV, &dir)
            .output()
            .expect("start child");
        let _ = fs::remove_dir_all(&dir);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "child failed:\n{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed"), "helper did not run:\n{stdout}");
    }
}

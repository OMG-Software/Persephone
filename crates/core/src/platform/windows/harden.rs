//! Process hardening against injected DLLs.
//!
//! Third-party software can load a DLL into any process of the user, for
//! example through a global window hook. The core holds the keys, so it
//! refuses such images. [`block_injected_dlls`] sets two process mitigation
//! policies. Windows never lets a process undo them.

use std::{ffi::c_void, io};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessMitigationPolicy, ProcessExtensionPointDisablePolicy,
    ProcessSignaturePolicy, SetProcessMitigationPolicy,
};

/// Bit 0 of both policy structures: `MicrosoftSignedOnly` for the signature
/// policy, `DisableExtensionPoints` for the extension-point policy.
const ENABLE: u32 = 1;

/// Blocks, for the rest of this process's life, images not signed by
/// Microsoft and the legacy extension points (global window hooks,
/// AppInit DLLs, legacy IMEs, Winsock LSPs). Modules loaded before the call
/// stay loaded. Returns an error if either policy cannot be set or is not
/// reported on afterwards.
pub fn block_injected_dlls() -> io::Result<()> {
    let policies = [ProcessExtensionPointDisablePolicy, ProcessSignaturePolicy];
    for policy in policies {
        let flags = ENABLE;
        // SAFETY: the buffer is a live u32, the size of both policy structures.
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
    for policy in policies {
        let mut flags = 0u32;
        // SAFETY: the pseudo handle needs no release; the buffer is a live
        // u32, the size of both policy structures.
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
        if flags & ENABLE == 0 {
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
    /// GIVEN two altered copies of a Microsoft DLL and a genuine one
    /// WHEN the process blocks injected DLLs
    /// THEN the first copy loaded before, the second fails with
    /// ERROR_INVALID_IMAGE_HASH and the genuine DLL still loads
    #[test]
    #[ignore = "runs in a child process of hardening_blocks_unsigned_images"]
    fn probe_child() {
        let Some(dir) = std::env::var_os(PROBE_ENV) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let before = load(&dir.join("probe-a.dll")).expect("altered DLL loads before hardening");
        free(before);
        block_injected_dlls().expect("policies set");
        let after = load(&dir.join("probe-b.dll"));
        assert_eq!(after.err(), Some(ERROR_INVALID_IMAGE_HASH));
        let genuine = load(&system_dll("msimg32.dll")).expect("Microsoft-signed DLL still loads");
        free(genuine);
    }

    /// GIVEN altered copies of a Microsoft DLL with an invalid signature
    /// WHEN a child process hardens itself and then loads one of them
    /// THEN the load is refused, while the same kind of load worked before
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

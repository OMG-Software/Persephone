//! Process hardening for the UI process.
//!
//! Third-party hook DLLs, such as overlay and monitoring tools that load
//! through global window hooks, can read decrypted message text in this
//! process. The UI blocks them at start-up. This does not cover the web view
//! processes (`msedgewebview2.exe`), which the WebView2 runtime starts.

use std::{ffi::c_void, io};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessMitigationPolicy, ProcessExtensionPointDisablePolicy,
    ProcessSignaturePolicy, SetProcessMitigationPolicy,
};

/// Bit 0 of both policy structures: `MicrosoftSignedOnly` and
/// `DisableExtensionPoints`. Each structure is one 32-bit flags field.
const POLICY_ENABLED: u32 = 1;

/// Blocks, for the rest of this process's life, images not signed by
/// Microsoft and the legacy extension points (global window hooks,
/// AppInit DLLs, legacy IMEs, Winsock LSPs). Modules loaded before the call
/// stay loaded. Returns an error if either policy cannot be set or is not
/// reported on afterwards.
///
/// Call this before any window or web view state exists. It does not cover
/// the WebView2 processes.
pub fn block_injected_dlls() -> io::Result<()> {
    set_and_check(ProcessExtensionPointDisablePolicy)?;
    set_and_check(ProcessSignaturePolicy)
}

fn set_and_check(policy: i32) -> io::Result<()> {
    let flags: u32 = POLICY_ENABLED;
    // SAFETY: `flags` is a live 4-byte buffer, the size of both policy
    // structures, and the call only reads it.
    let set = unsafe {
        SetProcessMitigationPolicy(
            policy,
            (&raw const flags).cast::<c_void>(),
            size_of::<u32>(),
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut current: u32 = 0;
    // SAFETY: the pseudo handle is always valid, and `current` is a live
    // 4-byte buffer that the call writes.
    let get = unsafe {
        GetProcessMitigationPolicy(
            GetCurrentProcess(),
            policy,
            (&raw mut current).cast::<c_void>(),
            size_of::<u32>(),
        )
    };
    if get == 0 {
        return Err(io::Error::last_os_error());
    }
    if current & POLICY_ENABLED == 0 {
        return Err(io::Error::other("mitigation policy not reported as set"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::OsString,
        fs,
        os::windows::ffi::OsStringExt,
        path::{Path, PathBuf},
        process::Command,
    };

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

    const ERROR_INVALID_IMAGE_HASH: u32 = 577;
    const PROBE_DIR: &str = "PERSEPHONE_HARDEN_PROBE_DIR";
    const CHILD_TEST: &str = "harden::tests::child_probe";

    /// Loads a library by full path; returns the Win32 error on failure.
    fn load(path: &Path) -> Result<(), u32> {
        let wide: Vec<u16> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain([0])
            .collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call.
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), 0) };
        if module.is_null() {
            // SAFETY: reads the calling thread's last error, with no inputs.
            return Err(unsafe { GetLastError() });
        }
        // SAFETY: `module` came from a successful load above.
        unsafe { FreeLibrary(module) };
        Ok(())
    }

    /// The child half of the test. It does nothing unless the parent test
    /// sets the probe directory, because the policy is one-way and
    /// process-wide.
    #[test]
    #[ignore = "runs only as a child of the parent test"]
    fn child_probe() {
        let Some(dir) = std::env::var_os(PROBE_DIR) else {
            return;
        };
        let dir = PathBuf::from(dir);
        assert_eq!(load(&dir.join("probe-a.dll")), Ok(()), "before hardening");
        block_injected_dlls().expect("block_injected_dlls");
        assert_eq!(
            load(&dir.join("probe-b.dll")),
            Err(ERROR_INVALID_IMAGE_HASH),
            "tampered DLL after hardening"
        );
        assert_eq!(
            load(&system_dll("msimg32.dll")),
            Ok(()),
            "Microsoft DLL after hardening"
        );
    }

    /// Copies `version.dll` and changes one byte of its DOS stub text, so
    /// no signature or catalog matches the copy.
    fn tampered_copy(to: &Path) {
        let mut bytes = fs::read(system_dll("version.dll")).expect("read version.dll");
        let text = b"This program cannot be run";
        let at = bytes
            .windows(text.len())
            .position(|w| w == text)
            .expect("DOS stub text");
        bytes[at] = b't';
        fs::write(to, bytes).expect("write tampered copy");
    }

    /// GIVEN a child process with a loadable tampered DLL WHEN it calls
    /// `block_injected_dlls` THEN a second tampered DLL fails with
    /// `ERROR_INVALID_IMAGE_HASH` and a Microsoft DLL still loads.
    #[test]
    fn given_hardened_child_then_unsigned_dll_is_refused() {
        let dir = std::env::temp_dir().join(format!("persephone-harden-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create probe dir");
        tampered_copy(&dir.join("probe-a.dll"));
        tampered_copy(&dir.join("probe-b.dll"));
        let output = Command::new(std::env::current_exe().expect("current exe"))
            .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
            .arg("--test-threads=1")
            .env(PROBE_DIR, &dir)
            .output()
            .expect("run child");
        let _ = fs::remove_dir_all(&dir);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "child failed:\n{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed"), "child did not run:\n{stdout}");
    }
}

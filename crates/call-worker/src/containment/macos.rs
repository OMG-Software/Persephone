//! The macOS call containment (ADR-0049): the call Seatbelt profile, applied
//! with `sandbox_init` before the worker reads any input, and a self-check
//! that the profile in force is this one. macOS code is not compiled on the
//! development machine; CI builds and tests it, and a Mac measures the
//! profile.

use std::ffi::{CString, c_char, c_int};
use std::fs::OpenOptions;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;

use super::{CHECK_FAILED, MISSING_SANDBOX};

/// The call profile in SBPL. It denies by default.
///
/// Every list of services and classes below is a best first list from Apple's
/// documented services. It is "to be measured on a Mac (ADR-0049)": a service
/// that is missing fails a call closed, and one that is not needed is removed
/// after the measurement. The measured list goes in the evidence record.
///
/// There is no `process-exec`, `process-fork`, `file-write*` or Apple Events
/// (`appleevent-send`, `mach-lookup` of `com.apple.coreservices.appleevents`).
const PROFILE: &str = r#"(version 1)
(deny default)

; Read the system frameworks and libraries, name resolution and the TLS
; configuration, and the audio and camera plug-in folders.
(allow file-read*
  (subpath "/System/Library")
  (subpath "/System/Cryptexes")
  (subpath "/usr/lib")
  (subpath "/usr/share/icu")
  (subpath "/private/etc")
  (subpath "/Library/Audio/Plug-Ins/HAL")
  (subpath "/Library/CoreMediaIO/Plug-Ins")
  (literal "/dev/urandom")
  (literal "/dev/random"))
; Path resolution needs the metadata of the links on the way.
(allow file-read-metadata
  (literal "/")
  (literal "/etc")
  (literal "/var")
  (literal "/private")
  (literal "/private/var"))
(allow sysctl-read)

; The SFU, the TURN servers and the STUN servers: outgoing connections, local
; UDP and TCP sockets for ICE, and the replies on a bound UDP socket.
(allow network-outbound)
(allow network-bind (local udp) (local tcp))
(allow network-inbound (local udp))

; The microphone and the camera. TCC still asks the user (ADR-0049).
(allow device-microphone)
(allow device-camera)

; To be measured on a Mac (ADR-0049): the Mach services of Core Audio, Core
; Media IO, name resolution (mDNSResponder), configd, trustd and tccd.
(allow mach-lookup
  (global-name "com.apple.audio.coreaudiod")
  (global-name "com.apple.audio.audiohald")
  (global-name "com.apple.audio.AudioComponentRegistrar")
  (global-name "com.apple.cmio.registerassistantservice")
  (global-name "com.apple.cmio.AppleCameraAssistant")
  (global-name "com.apple.cmio.IIDCVideoAssistant")
  (global-name "com.apple.cmio.VDCAssistant")
  (global-name "com.apple.dnssd.service")
  (global-name "com.apple.mDNSResponder.dnsctl")
  (global-name "com.apple.SystemConfiguration.configd")
  (global-name "com.apple.SystemConfiguration.DNSConfiguration")
  (global-name "com.apple.trustd")
  (global-name "com.apple.trustd.agent")
  (global-name "com.apple.tccd")
  (global-name "com.apple.tccd.system")
  (global-name "com.apple.cfprefsd.daemon")
  (global-name "com.apple.cfprefsd.agent")
  (global-name "com.apple.system.opendirectoryd.libinfo")
  (global-name "com.apple.system.notification_center"))

; To be measured on a Mac (ADR-0049): the I/O Kit classes of Core Audio and
; Core Media IO.
(allow iokit-open
  (iokit-user-client-class "IOAudioControlUserClient")
  (iokit-user-client-class "IOAudioEngineUserClient")
  (iokit-user-client-class "AppleCameraInterfaceUserClient"))
"#;

#[link(name = "sandbox", kind = "dylib")]
unsafe extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
    /// Returns 1 for `pid` under a sandbox when `operation` is null. With an
    /// operation, it returns 0 when the operation is allowed.
    fn sandbox_check(pid: i32, operation: *const c_char, filter: c_int, ...) -> c_int;
}

/// The bits of the profile's result, set once. The self-check runs after the
/// profile is applied, and a second call must not apply it again.
static RESULT: OnceLock<u32> = OnceLock::new();

pub fn missing_controls() -> u32 {
    *RESULT.get_or_init(|| apply().unwrap_or_else(|bit| bit))
}

/// Applies the profile, then checks it. `Ok(0)`: in force. `Err(bit)`: the
/// missing control.
fn apply() -> Result<u32, u32> {
    let profile = CString::new(PROFILE).map_err(|_| CHECK_FAILED)?;
    let mut error: *mut c_char = null_mut();
    // SAFETY: `profile` is a NUL-terminated string that outlives the call.
    // Flags 0 mean that it is SBPL text. `error` receives a library-allocated
    // message or null, which is freed below and not used again.
    let status = unsafe {
        let status = sandbox_init(profile.as_ptr(), 0, &mut error);
        if !error.is_null() {
            sandbox_free_error(error);
        }
        status
    };
    if status != 0 {
        return Err(MISSING_SANDBOX);
    }
    Ok(check())
}

/// The self-check: the process is sandboxed, a write that no call needs is
/// refused, and the camera, which only the call profile allows, is allowed.
fn check() -> u32 {
    let pid = std::process::id() as i32;
    // SAFETY: A null operation asks only whether `pid` is sandboxed; the
    // variadic arguments are not read for filter 0.
    if unsafe { sandbox_check(pid, null(), 0) } != 1 {
        return MISSING_SANDBOX;
    }
    // `/dev/null` is writable for every user, so only the sandbox refuses it.
    if OpenOptions::new().write(true).open("/dev/null").is_ok() {
        return MISSING_SANDBOX;
    }
    // SAFETY: The operation name is a NUL-terminated literal and filter 0
    // reads no further argument.
    if unsafe { sandbox_check(pid, c"device-camera".as_ptr(), 0) } != 0 {
        return MISSING_SANDBOX;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN the profile text WHEN it is read THEN it denies by default and
    /// allows no file write, process start or Apple Events.
    #[test]
    fn given_profile_then_it_denies_by_default_and_allows_no_writes() {
        assert!(PROFILE.contains("(deny default)"));
        for denied in ["file-write", "process-exec", "process-fork", "appleevent"] {
            for line in PROFILE
                .lines()
                .filter(|line| !line.trim_start().starts_with(';'))
            {
                assert!(!line.contains(denied), "{line}");
            }
        }
        assert!(CString::new(PROFILE).is_ok());
    }
}

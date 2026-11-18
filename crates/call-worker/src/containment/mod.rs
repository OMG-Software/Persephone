//! The call worker's self-check of the containment its launcher applies
//! (ADR-0043, ADR-0049). Each missing control sets one bit; the core refuses a
//! call unless the result is zero.
//!
//! One module per OS: `windows.rs` (AppContainer and Job), `linux.rs`
//! (`no_new_privs`, seccomp, Landlock and non-dumpable) and `macos.rs` (the
//! call Seatbelt profile). Bit values never change once released. Bit 31
//! means the OS has no reviewed call containment, so calls are refused there.
//!
//! The call worker does not require the dynamic-code prohibition or the
//! Win32k lockdown on Windows, because the audio stack needs both turned off.

/// The process token is not an AppContainer token.
#[cfg(windows)]
pub const MISSING_APPCONTAINER: u32 = 1 << 0;
/// The process is not inside a Job object.
#[cfg(windows)]
pub const MISSING_JOB: u32 = 1 << 1;

/// Linux: `no_new_privs` is not set.
#[cfg(target_os = "linux")]
#[allow(dead_code)] // set by the Linux backend (ADR-0049)
pub const MISSING_NO_NEW_PRIVS: u32 = 1 << 0;
/// Linux: the seccomp filter is not in place.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub const MISSING_SECCOMP: u32 = 1 << 1;
/// Linux: Landlock (ABI 3 or newer) is not in force.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub const MISSING_LANDLOCK: u32 = 1 << 2;
/// Linux: the process is still dumpable.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub const MISSING_DUMPABLE_OFF: u32 = 1 << 3;

/// macOS: the call Seatbelt profile is not applied.
#[cfg(target_os = "macos")]
#[allow(dead_code)] // set by the macOS backend (ADR-0049)
pub const MISSING_SANDBOX: u32 = 1 << 0;

/// A check could not be evaluated, or the worker's arguments are not the ones
/// the launcher passes; treated as missing. The same bit on every OS.
pub const CHECK_FAILED: u32 = 1 << 4;
/// Call containment is not implemented for this operating system.
#[cfg(not(windows))]
#[allow(dead_code)] // only the fallback for other OSes and the tests use it
pub const UNSUPPORTED_PLATFORM: u32 = 1 << 31;

/// Returns the bits of the missing controls; zero only when all are present.
pub fn missing_controls() -> u32 {
    #[cfg(windows)]
    return windows::missing_controls();
    #[cfg(target_os = "linux")]
    return linux::missing_controls();
    #[cfg(target_os = "macos")]
    return macos::missing_controls();
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    return UNSUPPORTED_PLATFORM;
}

/// Stage 2 of the Linux containment (ADR-0049): no new `AF_UNIX` socket and no
/// more Landlock calls. Call it after the sound server connection and before
/// the worker talks to the SFU. Returns false when it cannot be installed; the
/// call then ends with `Containment`. Other OSes have no second stage.
pub fn apply_stage2() -> bool {
    #[cfg(target_os = "linux")]
    return linux::stage2().is_ok();
    #[cfg(not(target_os = "linux"))]
    return true;
}

/// The most `/dev/video<N>` nodes the Linux worker allows and probes.
#[cfg(target_os = "linux")]
pub use linux::MAX_VIDEO_NODES;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN the bit constants of every OS WHEN compared THEN the Windows bits
    /// are the released values and the shared bits do not collide.
    #[test]
    fn given_bit_constants_then_they_are_stable_and_distinct() {
        assert_eq!(CHECK_FAILED, 1 << 4);
        #[cfg(windows)]
        assert_eq!((MISSING_APPCONTAINER, MISSING_JOB), (1, 2));
        #[cfg(target_os = "linux")]
        {
            let bits = [
                MISSING_NO_NEW_PRIVS,
                MISSING_SECCOMP,
                MISSING_LANDLOCK,
                MISSING_DUMPABLE_OFF,
                CHECK_FAILED,
            ];
            let all = bits.iter().fold(0, |all, bit| all | bit);
            assert_eq!(all.count_ones() as usize, bits.len());
        }
        #[cfg(not(windows))]
        assert_eq!(UNSUPPORTED_PLATFORM, 1 << 31);
    }
}

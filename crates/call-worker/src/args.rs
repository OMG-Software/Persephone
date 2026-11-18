//! The worker's command line (ADR-0049). The launcher passes at most one
//! argument, and only on Linux: `--camera=portal`, `--camera=v4l2` or
//! `--camera=none`. It tells the worker which camera path it may use. Any other
//! argument, or any argument on another OS, makes the worker refuse to start:
//! it reports a missing control and ends, as for a failed containment check.

use std::ffi::OsString;
use std::sync::OnceLock;

/// Which camera path the worker may use. The core chose it before it started
/// the worker; the worker never changes it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(dead_code)] // the camera backends read the mode (ADR-0049)
pub enum CameraMode {
    /// The PipeWire remote that the core sends over the camera channel.
    Portal,
    /// The worker opens `/dev/video*` itself.
    V4l2,
    /// No camera path. Also the mode of every other OS.
    #[default]
    None,
}

/// Why the arguments were refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BadArguments;

/// Parses the arguments after the program name. `linux` says whether the OS
/// takes the camera argument.
pub fn parse<I>(args: I, linux: bool) -> Result<CameraMode, BadArguments>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(CameraMode::None);
    };
    if !linux || args.next().is_some() {
        return Err(BadArguments);
    }
    match first.to_str() {
        Some("--camera=portal") => Ok(CameraMode::Portal),
        Some("--camera=v4l2") => Ok(CameraMode::V4l2),
        Some("--camera=none") => Ok(CameraMode::None),
        _ => Err(BadArguments),
    }
}

static MODE: OnceLock<CameraMode> = OnceLock::new();

/// Reads the process arguments once and keeps the camera mode. Returns
/// whether they were valid; on `false` the mode stays `None`.
pub fn init() -> bool {
    let parsed = parse(std::env::args_os().skip(1), cfg!(target_os = "linux"));
    let _ = MODE.set(parsed.unwrap_or_default());
    parsed.is_ok()
}

/// The camera mode chosen by the launcher; `None` before [`init`] or after
/// refused arguments.
pub fn camera_mode() -> CameraMode {
    MODE.get().copied().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str], linux: bool) -> Result<CameraMode, BadArguments> {
        parse(args.iter().map(OsString::from), linux)
    }

    /// GIVEN the Linux launcher's arguments WHEN parsed THEN each camera mode
    /// is read and no argument means none.
    #[test]
    fn given_linux_camera_argument_then_the_mode_is_read() {
        assert_eq!(run(&[], true), Ok(CameraMode::None));
        assert_eq!(run(&["--camera=portal"], true), Ok(CameraMode::Portal));
        assert_eq!(run(&["--camera=v4l2"], true), Ok(CameraMode::V4l2));
        assert_eq!(run(&["--camera=none"], true), Ok(CameraMode::None));
    }

    /// GIVEN any other argument, a second argument, or an argument on
    /// another OS WHEN parsed THEN the worker refuses to start.
    #[test]
    fn given_other_arguments_then_they_are_refused() {
        for args in [
            &["--camera"][..],
            &["--camera="],
            &["--camera=PORTAL"],
            &["--camera=portal "],
            &["--camera=portal", "--camera=none"],
            &["--camera=none", "x"],
            &["--help"],
            &[""],
        ] {
            assert_eq!(run(args, true), Err(BadArguments), "{args:?}");
        }
        assert_eq!(run(&[], false), Ok(CameraMode::None));
        assert_eq!(run(&["--camera=none"], false), Err(BadArguments));
        assert_eq!(run(&["--camera=portal"], false), Err(BadArguments));
    }

    /// GIVEN an argument that is not valid UTF-8 WHEN parsed THEN it is
    /// refused.
    #[cfg(unix)]
    #[test]
    fn given_non_utf8_argument_then_it_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        let arg = OsString::from_vec(b"--camera=\xff".to_vec());
        assert_eq!(parse([arg], true), Err(BadArguments));
    }
}

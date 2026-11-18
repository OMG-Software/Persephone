//! The camera through the desktop's camera portal
//! (`org.freedesktop.portal.Camera`, ADR-0049).
//!
//! The core, not the call worker, talks to the session bus. The flow for the
//! portal path:
//!
//! 1. [`probe`] decides the camera path before the worker starts: the portal
//!    when the session bus has the Camera interface, V4L2 when it has no such
//!    portal, no camera when the bus fails in some other way.
//! 2. [`open_remote`] runs when the camera turns on, or when cameras are
//!    listed: `AccessCamera` (the desktop may ask the user), a wait of at most
//!    [`ACCESS_TIMEOUT`] for the `Response` signal, then `OpenPipeWireRemote`.
//!    The returned descriptor goes to the worker over the camera channel.
//!
//! A refusal, a timeout or any failure ends in an error. There is no fallback
//! to V4L2 after a portal exists: a refused camera stays off. The worker
//! never sees the bus.
//!
//! The caller can end the request early with a cancel flag. Nothing is
//! asked once the flag is set. The method calls and the wait for the user
//! look at it every [`CANCEL_CHECK`]; a cancel then closes the request and
//! the request's own bus connection, so the dialog goes away and a call or
//! profile shutdown waits neither for the user nor for a stalled portal.

use super::bus::{self, BusError};
use crate::calls::worker::CameraPath;
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use zbus::blocking::Proxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const CAMERA: &str = "org.freedesktop.portal.Camera";
const REQUEST: &str = "org.freedesktop.portal.Request";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// The longest wait for the user's answer to the access request.
pub const ACCESS_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the wait for the user's answer looks at the cancel flag.
const CANCEL_CHECK: Duration = Duration::from_millis(100);

/// Why no remote came.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraError {
    /// The session has no camera portal.
    NoPortal,
    /// The user or the desktop refused access, or the request was ended.
    Refused,
    /// The bus, the portal or the reply failed.
    Failed,
    /// The portal did not answer in time.
    TimedOut,
    /// The caller ended the request.
    Cancelled,
}

// --- Pure parts --------------------------------------------------------------

/// What the probe of the Camera interface found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Probe {
    /// The interface answered.
    Present,
    /// No session bus, no portal service, or a portal without the Camera
    /// interface.
    NoPortal,
    /// The bus or the portal failed in another way (a timeout, a refusal).
    Failed,
}

/// The camera path for a probe result. A failed probe offers no camera: the
/// worker must not open `/dev/video*` while a portal may be present.
fn path_for(probe: Probe) -> CameraPath {
    match probe {
        Probe::Present => CameraPath::Portal,
        Probe::NoPortal => CameraPath::V4l2,
        Probe::Failed => CameraPath::None,
    }
}

/// Whether a D-Bus error name says that the object has no such interface or
/// property. The portal's GDBus skeleton answers `InvalidArgs` for an
/// interface it does not export.
fn lacks_interface(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.DBus.Error.UnknownInterface"
            | "org.freedesktop.DBus.Error.UnknownProperty"
            | "org.freedesktop.DBus.Error.UnknownObject"
            | "org.freedesktop.DBus.Error.UnknownMethod"
            | "org.freedesktop.DBus.Error.InvalidArgs"
    )
}

/// The sender part of a request path: a unique bus name without its leading
/// colon, with dots as underscores (`:1.42` is `1_42`). `None` for a name
/// that is not a unique name.
fn sender_part(unique_name: &str) -> Option<String> {
    let rest = unique_name.strip_prefix(':')?;
    let clean = !rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    clean.then(|| rest.replace(['.', '-'], "_"))
}

/// The object path of the request that the portal makes for `token`.
fn request_path(unique_name: &str, token: &str) -> Option<String> {
    let sender = sender_part(unique_name)?;
    let token_ok =
        !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    token_ok.then(|| format!("/org/freedesktop/portal/desktop/request/{sender}/{token}"))
}

/// A fresh handle token: `persephone_` and 16 hex digits of randomness.
fn new_token() -> Option<String> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).ok()?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Some(format!("persephone_{hex}"))
}

/// The options of `AccessCamera`: the handle token only.
fn access_options(token: &str) -> HashMap<&'static str, Value<'_>> {
    HashMap::from([("handle_token", Value::from(token))])
}

/// What the `Response` code of a request means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Granted,
    Refused,
}

/// Response code 0 is success; 1 is a user cancel and 2 is any other end. An
/// unknown code is not a grant.
fn outcome(code: u32) -> Outcome {
    if code == 0 {
        Outcome::Granted
    } else {
        Outcome::Refused
    }
}

/// Whether the descriptor the portal returned is a socket. The worker would
/// refuse anything else when it connects; the core does not forward it.
fn is_socket(fd: &OwnedFd) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: `stat` is plain data; fstat writes it for an open descriptor.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `stat` is a valid out-parameter and the descriptor is open.
    let ok = unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } == 0;
    ok && stat.st_mode & libc::S_IFMT == libc::S_IFSOCK
}

// --- The probe -----------------------------------------------------------------

/// Decides the camera path (ADR-0049).
pub fn probe() -> CameraPath {
    path_for(probe_interface())
}

fn probe_interface() -> Probe {
    let connection = match bus::session() {
        Ok(connection) => connection,
        Err(BusError::Absent) => return Probe::NoPortal,
        Err(BusError::Failed) => return Probe::Failed,
    };
    // The `version` property exists on every export of the interface.
    let reply = connection.call_method(
        Some(PORTAL),
        PORTAL_PATH,
        Some(PROPERTIES),
        "Get",
        &(CAMERA, "version"),
    );
    match reply {
        Ok(_) => Probe::Present,
        Err(error) => match &error {
            zbus::Error::MethodError(name, _, _) if lacks_interface(name.as_str()) => {
                Probe::NoPortal
            }
            zbus::Error::FDO(inner) if is_lacking_fdo(inner) => Probe::NoPortal,
            _ => match bus::classify(&error) {
                BusError::Absent => Probe::NoPortal,
                BusError::Failed => Probe::Failed,
            },
        },
    }
}

fn is_lacking_fdo(error: &zbus::fdo::Error) -> bool {
    matches!(
        error,
        zbus::fdo::Error::UnknownInterface(_)
            | zbus::fdo::Error::UnknownProperty(_)
            | zbus::fdo::Error::UnknownObject(_)
            | zbus::fdo::Error::UnknownMethod(_)
            | zbus::fdo::Error::InvalidArgs(_)
    )
}

// --- The request ---------------------------------------------------------------

/// Asks the portal for camera access and returns the PipeWire remote.
/// Blocks for up to [`ACCESS_TIMEOUT`] while the desktop asks the user; call
/// it from a thread that may block. Setting `cancel` ends the wait for the
/// user within [`CANCEL_CHECK`] and closes the request; a set flag also stops
/// the remote from being opened after a grant.
pub fn open_remote(cancel: &AtomicBool) -> Result<OwnedFd, CameraError> {
    if cancel.load(Ordering::SeqCst) {
        return Err(CameraError::Cancelled);
    }
    let connection = bus::session().map_err(|error| match error {
        BusError::Absent => CameraError::NoPortal,
        BusError::Failed => CameraError::Failed,
    })?;
    let unique = connection.unique_name().ok_or(CameraError::Failed)?;
    let token = new_token().ok_or(CameraError::Failed)?;
    let path = request_path(unique.as_str(), &token).ok_or(CameraError::Failed)?;

    // Listen for the response before the request, so it cannot be missed.
    let request = Proxy::new_owned(connection.clone(), PORTAL, path.clone(), REQUEST)
        .map_err(|_| CameraError::Failed)?;
    let mut responses = request
        .receive_signal("Response")
        .map_err(|_| CameraError::Failed)?;
    let (sender, receiver) = mpsc::channel();
    let waiter = std::thread::Builder::new()
        .name("camera-portal".to_owned())
        .spawn(move || {
            let _ = sender.send(responses.next());
        })
        .map_err(|_| CameraError::Failed)?;

    // A cancel during the bus setup asks nothing.
    let access = if cancel.load(Ordering::SeqCst) {
        Err(CameraError::Cancelled)
    } else {
        let options_token = token.clone();
        cancellable(&connection, cancel, move |connection| {
            connection
                .call_method(
                    Some(PORTAL),
                    PORTAL_PATH,
                    Some(CAMERA),
                    "AccessCamera",
                    &(access_options(&options_token),),
                )
                .map_err(|error| failure(&error))
                .and_then(|reply| {
                    reply
                        .body()
                        .deserialize::<OwnedObjectPath>()
                        .map_err(|_| CameraError::Failed)
                })
        })
    };
    // The portal must have made the request at the path that was listened on.
    let granted = match access {
        Ok(handle) if handle.as_str() == path => wait_for_response(&receiver, cancel),
        Ok(_) => Err(CameraError::Failed),
        Err(error) => Err(error),
    };
    if granted.is_err() {
        // End the request and the wait. `Close` waits for no reply: after a
        // timeout the portal may not answer, and a reply wait would add the
        // bus's own method timeout. Closing the connection ends the waiting
        // thread, which would otherwise stay until the response.
        if let Ok(close) = close_message(path.as_str()) {
            let _ = connection.send(&close);
        }
        let _ = connection.clone().close();
    }
    // The waiting thread ends with the connection or with the response; it is
    // not joined, so a stuck bus cannot hold the caller.
    drop(waiter);
    granted?;
    if cancel.load(Ordering::SeqCst) {
        let _ = connection.close();
        return Err(CameraError::Cancelled);
    }

    let remote = cancellable(&connection, cancel, |connection| {
        let reply = connection
            .call_method(
                Some(PORTAL),
                PORTAL_PATH,
                Some(CAMERA),
                "OpenPipeWireRemote",
                &(HashMap::<&str, Value<'_>>::new(),),
            )
            .map_err(|error| failure(&error))?;
        let fd: zbus::zvariant::OwnedFd = reply
            .body()
            .deserialize()
            .map_err(|_| CameraError::Failed)?;
        Ok(OwnedFd::from(fd))
    });
    let fd = match remote {
        Ok(fd) => fd,
        Err(error) => {
            // Ends a call still waiting on the bus.
            let _ = connection.close();
            return Err(error);
        }
    };
    if is_socket(&fd) {
        Ok(fd)
    } else {
        Err(CameraError::Failed)
    }
}

/// Runs the method call `call` on its own thread and waits for its result,
/// looking at `cancel` every [`CANCEL_CHECK`]. A set flag ends the wait at
/// once with [`CameraError::Cancelled`]; the caller then closes the
/// connection, which ends the call. The thread is not joined, so a stalled
/// portal cannot hold the caller.
fn cancellable<T, F>(
    connection: &zbus::blocking::Connection,
    cancel: &AtomicBool,
    call: F,
) -> Result<T, CameraError>
where
    T: Send + 'static,
    F: FnOnce(&zbus::blocking::Connection) -> Result<T, CameraError> + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    let connection = connection.clone();
    std::thread::Builder::new()
        .name("camera-portal-call".to_owned())
        .spawn(move || {
            let _ = sender.send(call(&connection));
        })
        .map_err(|_| CameraError::Failed)?;
    wait_cancellable(&receiver, cancel)
}

/// Waits for one result on `receiver` until `cancel` is set. A closed
/// channel (the thread ended without a result) is a failure.
fn wait_cancellable<T>(
    receiver: &mpsc::Receiver<Result<T, CameraError>>,
    cancel: &AtomicBool,
) -> Result<T, CameraError> {
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(CameraError::Cancelled);
        }
        match receiver.recv_timeout(CANCEL_CHECK) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(CameraError::Failed),
        }
    }
}

/// A failed call. An access error from the portal is a refusal; a missing
/// service is no portal.
fn failure(error: &zbus::Error) -> CameraError {
    let denied = match error {
        zbus::Error::MethodError(name, _, _) => {
            name.as_str() == "org.freedesktop.DBus.Error.AccessDenied"
        }
        zbus::Error::FDO(inner) => matches!(**inner, zbus::fdo::Error::AccessDenied(_)),
        _ => false,
    };
    if denied {
        return CameraError::Refused;
    }
    match bus::classify(error) {
        BusError::Absent => CameraError::NoPortal,
        BusError::Failed => CameraError::Failed,
    }
}

/// The `Close` call of the request at `path`, marked so that the portal sends
/// no reply and nothing waits for one.
fn close_message(path: &str) -> zbus::Result<zbus::Message> {
    zbus::Message::method_call(path, "Close")?
        .destination(PORTAL)?
        .interface(REQUEST)?
        .with_flags(zbus::message::Flags::NoReplyExpected)?
        .build(&())
}

/// Waits for the `Response` message, for at most [`ACCESS_TIMEOUT`]. Success
/// only for code 0. A set `cancel` ends the wait within [`CANCEL_CHECK`].
fn wait_for_response(
    receiver: &mpsc::Receiver<Option<zbus::Message>>,
    cancel: &AtomicBool,
) -> Result<(), CameraError> {
    let deadline = Instant::now() + ACCESS_TIMEOUT;
    let message = loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(CameraError::Cancelled);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(CameraError::TimedOut);
        }
        match receiver.recv_timeout(left.min(CANCEL_CHECK)) {
            Ok(Some(message)) => break message,
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(CameraError::Failed);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    let (code, _results): (u32, HashMap<String, OwnedValue>) = message
        .body()
        .deserialize()
        .map_err(|_| CameraError::Failed)?;
    match outcome(code) {
        Outcome::Granted => Ok(()),
        Outcome::Refused => Err(CameraError::Refused),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::DynamicType;

    /// GIVEN each probe result WHEN the path is chosen THEN a portal gives the
    /// portal path, a missing portal gives V4L2 and a failed probe gives no
    /// camera at all (never V4L2 beside a portal that may exist).
    #[test]
    fn given_probe_results_then_the_path_is_chosen() {
        assert_eq!(path_for(Probe::Present), CameraPath::Portal);
        assert_eq!(path_for(Probe::NoPortal), CameraPath::V4l2);
        assert_eq!(path_for(Probe::Failed), CameraPath::None);
    }

    /// GIVEN D-Bus error names THEN only "no such interface or property" names
    /// say that the portal lacks the Camera interface; a refusal or a timeout
    /// does not.
    #[test]
    fn given_error_names_then_only_missing_interface_names_count() {
        for name in [
            "org.freedesktop.DBus.Error.UnknownInterface",
            "org.freedesktop.DBus.Error.UnknownProperty",
            "org.freedesktop.DBus.Error.UnknownObject",
            "org.freedesktop.DBus.Error.UnknownMethod",
            "org.freedesktop.DBus.Error.InvalidArgs",
        ] {
            assert!(lacks_interface(name), "{name}");
        }
        for name in [
            "org.freedesktop.DBus.Error.AccessDenied",
            "org.freedesktop.DBus.Error.NoReply",
            "org.freedesktop.DBus.Error.Timeout",
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "",
        ] {
            assert!(!lacks_interface(name), "{name}");
        }
        assert!(is_lacking_fdo(&zbus::fdo::Error::UnknownInterface(
            "x".into()
        )));
        assert!(!is_lacking_fdo(&zbus::fdo::Error::AccessDenied("x".into())));
    }

    /// GIVEN unique bus names and tokens WHEN the request path is built THEN
    /// it follows the portal's naming and bad input gives no path.
    #[test]
    fn given_names_and_tokens_then_the_request_path_follows_the_portal() {
        assert_eq!(
            request_path(":1.42", "persephone_0123abcd").as_deref(),
            Some("/org/freedesktop/portal/desktop/request/1_42/persephone_0123abcd")
        );
        assert_eq!(sender_part(":1.4-2").as_deref(), Some("1_4_2"));
        for bad in ["1.42", ":", "", ":1.4/2", ":1.4 2", "org.example.Name"] {
            assert_eq!(sender_part(bad), None, "{bad:?}");
        }
        for bad in ["", "a/b", "a b", "a.b", "tok-en"] {
            assert_eq!(request_path(":1.1", bad), None, "{bad:?}");
        }
        assert_eq!(request_path("bad", "token"), None);
    }

    /// GIVEN two tokens THEN each is a valid path element and they differ.
    #[test]
    fn given_two_tokens_then_they_are_valid_and_different() {
        let (first, second) = (new_token().expect("token"), new_token().expect("token"));
        assert_ne!(first, second);
        for token in [&first, &second] {
            assert!(token.starts_with("persephone_"));
            assert_eq!(token.len(), "persephone_".len() + 16);
            assert!(request_path(":1.1", token).is_some());
        }
    }

    /// GIVEN the access options THEN they hold the handle token as one string
    /// entry of an `a{sv}` dictionary.
    #[test]
    fn given_the_options_then_they_are_a_string_variant_dictionary() {
        let options = access_options("persephone_x");
        assert_eq!(options.len(), 1);
        assert_eq!(options["handle_token"], Value::from("persephone_x"));
        let body = (options,);
        assert_eq!(body.signature().to_string(), "(a{sv})");
        let empty = (HashMap::<&str, Value<'_>>::new(),);
        assert_eq!(empty.signature().to_string(), "(a{sv})");
    }

    /// GIVEN a request path WHEN its `Close` call is built THEN it goes to
    /// the portal's request interface and expects no reply.
    #[test]
    fn given_a_request_path_then_close_expects_no_reply() {
        let path = "/org/freedesktop/portal/desktop/request/1_1/persephone_x";
        let message = close_message(path).expect("a message");
        let header = message.header();
        assert!(
            header
                .primary()
                .flags()
                .contains(zbus::message::Flags::NoReplyExpected)
        );
        assert_eq!(header.member().map(|m| m.as_str()), Some("Close"));
        assert_eq!(header.interface().map(|i| i.as_str()), Some(REQUEST));
        assert_eq!(header.destination().map(|d| d.as_str()), Some(PORTAL));
        assert_eq!(header.path().map(|p| p.as_str()), Some(path));
    }

    /// GIVEN response codes THEN only 0 is a grant; a cancel, an end and an
    /// unknown code are refusals.
    #[test]
    fn given_response_codes_then_only_zero_grants_access() {
        assert_eq!(outcome(0), Outcome::Granted);
        for code in [1, 2, 3, 255, u32::MAX] {
            assert_eq!(outcome(code), Outcome::Refused, "{code}");
        }
    }

    /// GIVEN errors from a call THEN an access refusal is a refusal, a missing
    /// service is no portal and the rest are failures.
    #[test]
    fn given_call_errors_then_they_are_sorted() {
        let denied = zbus::Error::FDO(Box::new(zbus::fdo::Error::AccessDenied("no".into())));
        assert_eq!(failure(&denied), CameraError::Refused);
        let timeout = zbus::Error::FDO(Box::new(zbus::fdo::Error::Timeout("slow".into())));
        assert_eq!(failure(&timeout), CameraError::Failed);
        let unknown = zbus::Error::FDO(Box::new(zbus::fdo::Error::ServiceUnknown("x".into())));
        assert_eq!(failure(&unknown), CameraError::NoPortal);
        assert_eq!(failure(&zbus::Error::InvalidReply), CameraError::Failed);
    }

    /// GIVEN a regular file and a socket WHEN checked THEN only the socket is
    /// accepted as a PipeWire remote.
    #[test]
    fn given_a_file_and_a_socket_then_only_the_socket_is_a_remote() {
        let (socket, _other) = std::os::unix::net::UnixStream::pair().expect("pair");
        assert!(is_socket(&OwnedFd::from(socket)));
        let file = std::fs::File::open("/dev/null").expect("open");
        assert!(!is_socket(&OwnedFd::from(file)));
    }

    /// GIVEN a response that never comes WHEN waited for THEN a closed channel
    /// or an ended stream is a failure, never a grant.
    #[test]
    fn given_no_response_when_the_channel_closes_then_it_fails() {
        let cancel = AtomicBool::new(false);
        let (sender, receiver) = mpsc::channel::<Option<zbus::Message>>();
        drop(sender);
        assert_eq!(
            wait_for_response(&receiver, &cancel),
            Err(CameraError::Failed)
        );
        let (sender, receiver) = mpsc::channel::<Option<zbus::Message>>();
        sender.send(None).expect("send");
        assert_eq!(
            wait_for_response(&receiver, &cancel),
            Err(CameraError::Failed)
        );
    }

    /// GIVEN a set cancel flag WHEN a request starts THEN it ends at once,
    /// before any bus connection, so an ended call asks nothing.
    #[test]
    fn given_a_cancel_before_the_request_then_nothing_is_asked() {
        let cancel = AtomicBool::new(true);
        assert_eq!(open_remote(&cancel).err(), Some(CameraError::Cancelled));
    }

    /// GIVEN a method call that never answers WHEN the cancel flag is set
    /// THEN the wait ends as cancelled; a result that comes first is
    /// returned, and a thread that ends without one is a failure.
    #[test]
    fn given_a_stalled_method_call_then_a_cancel_ends_the_wait() {
        let (_sender, receiver) = mpsc::channel::<Result<u8, CameraError>>();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        assert_eq!(
            wait_cancellable(&receiver, &cancel),
            Err(CameraError::Cancelled)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        setter.join().expect("setter");
        let idle = AtomicBool::new(false);
        let (sender, receiver) = mpsc::channel();
        sender.send(Ok(7u8)).expect("send");
        assert_eq!(wait_cancellable(&receiver, &idle), Ok(7));
        drop(sender);
        assert_eq!(wait_cancellable(&receiver, &idle), Err(CameraError::Failed));
    }

    /// GIVEN a wait for the user's answer WHEN the cancel flag is set, before
    /// or during the wait THEN it ends as cancelled within a few checks, far
    /// before the access timeout.
    #[test]
    fn given_a_cancel_flag_then_the_wait_ends_without_the_user() {
        let (_sender, receiver) = mpsc::channel::<Option<zbus::Message>>();
        let set = AtomicBool::new(true);
        assert_eq!(
            wait_for_response(&receiver, &set),
            Err(CameraError::Cancelled)
        );
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        assert_eq!(
            wait_for_response(&receiver, &cancel),
            Err(CameraError::Cancelled)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        setter.join().expect("setter");
    }
}

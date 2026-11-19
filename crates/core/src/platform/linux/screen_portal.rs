//! The screen share through the desktop's ScreenCast portal
//! (`org.freedesktop.portal.ScreenCast`, ADR-0053).
//!
//! The core, not the call worker, talks to the session bus. The flow:
//!
//! 1. The core shows its own warning window: the shared screen can show
//!    Persephone's messages, because Linux does not hide the window from
//!    capture. "Cancel" ends the request; nothing else runs.
//! 2. `CreateSession`, `SelectSources` (monitor and window sources, one of
//!    them, `persist_mode` 0, no restore token, the cursor drawn into the
//!    picture where the portal offers it), `Start` (the desktop draws its own
//!    picker), then `OpenPipeWireRemote`.
//! 3. The node ID from the `Start` answer and the remote go to the worker.
//!    The portal session stays open for the whole share. It closes when the
//!    [`PortalSession`] drops (a stop, a worker end state, the end of the
//!    call). If the desktop closes it first (the user stops the share from
//!    the desktop), the `closed` signal fires.
//!
//! A cancelled picker, a refusal, a timeout or any failure ends in an
//! error. Nothing falls back to another source. The worker never sees the
//! bus.

use super::bus::{self, BusError};
use super::camera_portal::{close_message, is_socket, new_token, request_path};
use super::credentials::{prompt_until, screen_warning_alert, yes};
use crate::calls::screen::{
    Remote, SOURCE_TYPES, ScreenGrant, ScreenRefusal, ScreenSource, pick_node,
    preferred_cursor_mode,
};
use persephone_call_protocol::ScreenTarget;
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// The longest wait for the user's answer to one portal request. The picker
/// is a window the user may leave open for a while.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);
/// How often a wait looks at the cancel flag.
const CANCEL_CHECK: Duration = Duration::from_millis(100);

/// The ScreenCast source of the Linux desktop.
pub struct PortalScreen;

impl ScreenSource for PortalScreen {
    fn available(&self) -> bool {
        true
    }

    /// Warns, then asks the portal. The warning is the core's own window; a
    /// "Cancel", a closed window or a cancel of the pick ends the request
    /// before the bus is used. Only the user's "Share" goes on.
    fn choose(&self, cancel: &Arc<AtomicBool>) -> Result<ScreenGrant, ScreenRefusal> {
        if cancel.load(Ordering::SeqCst) {
            return Err(ScreenRefusal::Cancelled);
        }
        match prompt_until(screen_warning_alert(), cancel.clone())
            .and_then(|answer| yes(answer.response))
        {
            Ok(true) => {}
            Ok(false) => return Err(ScreenRefusal::Cancelled),
            // No display, or a window that did not run.
            Err(_) => return Err(ScreenRefusal::Unavailable),
        }
        if cancel.load(Ordering::SeqCst) {
            return Err(ScreenRefusal::Cancelled);
        }
        open_share(cancel)
    }
}

/// Why a portal step failed.
fn refusal(error: &zbus::Error) -> ScreenRefusal {
    let denied = match error {
        zbus::Error::MethodError(name, _, _) => {
            name.as_str() == "org.freedesktop.DBus.Error.AccessDenied"
        }
        zbus::Error::FDO(inner) => matches!(**inner, zbus::fdo::Error::AccessDenied(_)),
        _ => false,
    };
    if denied {
        return ScreenRefusal::Refused;
    }
    match bus::classify(error) {
        BusError::Absent => ScreenRefusal::Unavailable,
        BusError::Failed => ScreenRefusal::Refused,
    }
}

/// An open portal session. Dropping it closes the session and the bus
/// connection, so the desktop's "sharing" indicator goes away.
pub struct PortalSession {
    connection: Connection,
    path: OwnedObjectPath,
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        // `Close` waits for no reply: the portal may be gone.
        if let Ok(message) = zbus::Message::method_call(self.path.as_str(), "Close")
            .and_then(|builder| builder.destination(PORTAL))
            .and_then(|builder| builder.interface(SESSION))
            .and_then(|builder| builder.with_flags(zbus::message::Flags::NoReplyExpected))
            .and_then(|builder| builder.build(&()))
        {
            let _ = self.connection.send(&message);
        }
        let _ = self.connection.clone().close();
    }
}

/// One request of the portal: the method call and the wait for its
/// `Response`. Returns the results of the response. The `Response` listener
/// exists before the call, so the answer cannot be missed.
fn request(
    connection: &Connection,
    cancel: &AtomicBool,
    call: impl FnOnce(&Connection, &str) -> zbus::Result<zbus::Message> + Send + 'static,
) -> Result<HashMap<String, OwnedValue>, ScreenRefusal> {
    let unique = connection.unique_name().ok_or(ScreenRefusal::Refused)?;
    let token = new_token().ok_or(ScreenRefusal::Refused)?;
    let path = request_path(unique.as_str(), &token).ok_or(ScreenRefusal::Refused)?;
    let listener = Proxy::new_owned(connection.clone(), PORTAL, path.clone(), REQUEST)
        .map_err(|_| ScreenRefusal::Refused)?;
    let mut responses = listener
        .receive_signal("Response")
        .map_err(|_| ScreenRefusal::Refused)?;
    let (answer_tx, answer_rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("screen-portal".to_owned())
        .spawn(move || {
            let _ = answer_tx.send(responses.next());
        })
        .map_err(|_| ScreenRefusal::Refused)?;

    let handle = call_until(connection, cancel, move |connection| {
        call(connection, &token)
    })?;
    // The portal must have made the request at the path that was listened on.
    let reply_path: OwnedObjectPath = handle
        .body()
        .deserialize()
        .map_err(|_| ScreenRefusal::Refused)?;
    if reply_path.as_str() != path {
        return Err(ScreenRefusal::Refused);
    }
    let outcome = wait_response(&answer_rx, cancel);
    if outcome.is_err() {
        // End the request; `Close` waits for no reply.
        if let Ok(close) = close_message(path.as_str()) {
            let _ = connection.send(&close);
        }
    }
    outcome
}

/// Makes one method call on its own thread and waits for its reply until
/// `cancel` is set. A cancelled call's thread ends when the reply comes or
/// the connection closes ([`open_share`] closes it on every failure).
fn call_until(
    connection: &Connection,
    cancel: &AtomicBool,
    call: impl FnOnce(&Connection) -> zbus::Result<zbus::Message> + Send + 'static,
) -> Result<zbus::Message, ScreenRefusal> {
    let (call_tx, call_rx) = mpsc::channel();
    let call_connection = connection.clone();
    std::thread::Builder::new()
        .name("screen-portal-call".to_owned())
        .spawn(move || {
            let result = call(&call_connection).map_err(|error| refusal(&error));
            let _ = call_tx.send(result);
        })
        .map_err(|_| ScreenRefusal::Refused)?;
    wait(&call_rx, cancel).and_then(|outcome| outcome)
}

/// Waits for one result on `receiver` until `cancel` is set.
fn wait<T>(receiver: &mpsc::Receiver<T>, cancel: &AtomicBool) -> Result<T, ScreenRefusal> {
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(ScreenRefusal::Cancelled);
        }
        match receiver.recv_timeout(CANCEL_CHECK) {
            Ok(result) => return Ok(result),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(ScreenRefusal::Refused),
        }
    }
}

/// Waits for the `Response` message of a request, for at most
/// [`ANSWER_TIMEOUT`]. Code 0 is success; 1 is the user's cancel of the
/// desktop's picker; any other code is a refusal.
fn wait_response(
    receiver: &mpsc::Receiver<Option<zbus::Message>>,
    cancel: &AtomicBool,
) -> Result<HashMap<String, OwnedValue>, ScreenRefusal> {
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let message = loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(ScreenRefusal::Cancelled);
        }
        if deadline.saturating_duration_since(Instant::now()).is_zero() {
            return Err(ScreenRefusal::Unavailable);
        }
        match receiver.recv_timeout(CANCEL_CHECK) {
            Ok(Some(message)) => break message,
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(ScreenRefusal::Refused);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    let (code, results): (u32, HashMap<String, OwnedValue>) = message
        .body()
        .deserialize()
        .map_err(|_| ScreenRefusal::Refused)?;
    match code {
        0 => Ok(results),
        // The user closed the desktop's picker.
        1 => Err(ScreenRefusal::Cancelled),
        _ => Err(ScreenRefusal::Refused),
    }
}

/// A method call to the portal's ScreenCast object.
fn cast_call<B>(connection: &Connection, method: &str, body: &B) -> zbus::Result<zbus::Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    connection.call_method(Some(PORTAL), PORTAL_PATH, Some(SCREEN_CAST), method, body)
}

/// The cursor modes the portal offers, as a bit set; `0` when it says
/// nothing (the option is then left out).
fn available_cursor_modes(connection: &Connection) -> u32 {
    connection
        .call_method(
            Some(PORTAL),
            PORTAL_PATH,
            Some(PROPERTIES),
            "Get",
            &(SCREEN_CAST, "AvailableCursorModes"),
        )
        .ok()
        .and_then(|reply| reply.body().deserialize::<OwnedValue>().ok())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0)
}

/// The node IDs of the streams in the `Start` answer.
fn stream_nodes(results: &HashMap<String, OwnedValue>) -> Vec<u32> {
    let Some(streams) = results.get("streams") else {
        return Vec::new();
    };
    let Value::Array(array) = &**streams else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|item| match item {
            Value::Structure(structure) => match structure.fields().first() {
                Some(Value::U32(node)) => Some(*node),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Runs the portal flow and returns the grant: the node, the remote and the
/// open session.
fn open_share(cancel: &AtomicBool) -> Result<ScreenGrant, ScreenRefusal> {
    let connection = bus::session().map_err(|error| match error {
        BusError::Absent => ScreenRefusal::Unavailable,
        BusError::Failed => ScreenRefusal::Refused,
    })?;
    match share_on(&connection, cancel) {
        Ok(grant) => Ok(grant),
        Err(error) => {
            // Ends calls and dialogs still waiting on the bus.
            let _ = connection.close();
            Err(error)
        }
    }
}

fn share_on(connection: &Connection, cancel: &AtomicBool) -> Result<ScreenGrant, ScreenRefusal> {
    // CreateSession.
    let session_token = new_token().ok_or(ScreenRefusal::Refused)?;
    let created = request(connection, cancel, {
        let session_token = session_token.clone();
        move |connection, token| {
            let options = HashMap::from([
                ("handle_token", Value::from(token)),
                ("session_handle_token", Value::from(session_token.as_str())),
            ]);
            cast_call(connection, "CreateSession", &(options,))
        }
    })?;
    let session_path = created
        .get("session_handle")
        .and_then(|value| <&str>::try_from(&**value).ok())
        .and_then(|text| OwnedObjectPath::try_from(text).ok())
        .ok_or(ScreenRefusal::Refused)?;
    // From here the session closes with this value, on every path out.
    let session = PortalSession {
        connection: connection.clone(),
        path: session_path.clone(),
    };

    // SelectSources: monitor or window, one source, nothing remembered.
    let cursor = preferred_cursor_mode(available_cursor_modes(connection));
    let session_for_select = session_path.clone();
    request(connection, cancel, move |connection, token| {
        let mut options: HashMap<&str, Value<'_>> = HashMap::from([
            ("handle_token", Value::from(token)),
            ("types", Value::from(SOURCE_TYPES)),
            ("multiple", Value::from(false)),
            ("persist_mode", Value::from(0_u32)),
        ]);
        if let Some(mode) = cursor {
            options.insert("cursor_mode", Value::from(mode));
        }
        cast_call(
            connection,
            "SelectSources",
            &(ObjectPath::from(&session_for_select), options),
        )
    })?;

    // Start: the desktop shows its own picker.
    let session_for_start = session_path.clone();
    let started = request(connection, cancel, move |connection, token| {
        let options = HashMap::from([("handle_token", Value::from(token))]);
        cast_call(
            connection,
            "Start",
            &(ObjectPath::from(&session_for_start), "", options),
        )
    })?;
    let node = pick_node(&stream_nodes(&started))?;

    // The remote, then the signal for a session the desktop closes. A
    // portal that does not answer cannot hold a cancelled pick.
    let session_for_remote = session_path.clone();
    let reply = call_until(connection, cancel, move |connection| {
        cast_call(
            connection,
            "OpenPipeWireRemote",
            &(
                ObjectPath::from(&session_for_remote),
                HashMap::<&str, Value<'_>>::new(),
            ),
        )
    })?;
    let fd: zbus::zvariant::OwnedFd = reply
        .body()
        .deserialize()
        .map_err(|_| ScreenRefusal::Refused)?;
    let remote: Remote = OwnedFd::from(fd);
    if !is_socket(&remote) {
        return Err(ScreenRefusal::Refused);
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(ScreenRefusal::Cancelled);
    }
    let closed = watch_closed(connection, &session_path)?;
    Ok(ScreenGrant {
        target: ScreenTarget::Portal { node },
        remote: Some(remote),
        session: Some(Box::new(session)),
        closed: Some(closed),
    })
}

/// Watches the session's `Closed` signal on its own thread; the receiver
/// fires when the desktop closes the session. The thread ends with the
/// connection.
fn watch_closed(
    connection: &Connection,
    session_path: &OwnedObjectPath,
) -> Result<oneshot::Receiver<()>, ScreenRefusal> {
    let proxy = Proxy::new_owned(connection.clone(), PORTAL, session_path.to_owned(), SESSION)
        .map_err(|_| ScreenRefusal::Refused)?;
    let mut signals = proxy
        .receive_signal("Closed")
        .map_err(|_| ScreenRefusal::Refused)?;
    let (tx, rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("screen-portal-closed".to_owned())
        .spawn(move || {
            if signals.next().is_some() {
                let _ = tx.send(());
            }
        })
        .map_err(|_| ScreenRefusal::Refused)?;
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::names::ErrorName;

    /// A `Response` signal of a portal request with `code` and `results`.
    fn response(code: u32, results: HashMap<&str, Value<'_>>) -> zbus::Message {
        zbus::Message::signal(
            "/org/freedesktop/portal/desktop/request/1_1/t",
            REQUEST,
            "Response",
        )
        .unwrap()
        .build(&(code, results))
        .unwrap()
    }

    /// A method error from the bus with the D-Bus error `name`.
    fn method_error(name: &str) -> zbus::Error {
        let call = zbus::Message::method_call(PORTAL_PATH, "Start")
            .unwrap()
            .build(&())
            .unwrap();
        zbus::Error::MethodError(ErrorName::try_from(name).unwrap().into(), None, call)
    }

    /// GIVEN `Start` answers WHEN the nodes are read THEN each stream's node
    /// ID comes out, and an answer without streams, or with streams of
    /// another shape, gives none.
    #[test]
    fn given_a_start_answer_then_the_node_ids_are_read() {
        assert!(stream_nodes(&HashMap::new()).is_empty());
        let wrong = HashMap::from([("streams".to_owned(), OwnedValue::from(7_u32))]);
        assert!(stream_nodes(&wrong).is_empty());

        let streams = |value: Value<'_>| {
            HashMap::from([("streams".to_owned(), OwnedValue::try_from(value).unwrap())])
        };
        let two = streams(Value::from(vec![
            (41_u32, HashMap::<String, Value<'_>>::new()),
            (42_u32, HashMap::<String, Value<'_>>::new()),
        ]));
        assert_eq!(stream_nodes(&two), [41, 42]);
        // The node must be the structure's first field, and a `u32`.
        let named = streams(Value::from(vec![("41".to_owned(), 41_u32)]));
        assert!(stream_nodes(&named).is_empty());
        let bare = streams(Value::from(vec![41_u32, 42_u32]));
        assert!(stream_nodes(&bare).is_empty());
    }

    /// GIVEN the portal's refusal and absence errors WHEN mapped THEN access
    /// denied is a refusal, a missing service is unavailable and any other
    /// failure is a refusal, never a reason to say the portal is absent.
    #[test]
    fn given_portal_errors_then_the_refusal_names_the_cause() {
        let denied = zbus::Error::FDO(Box::new(zbus::fdo::Error::AccessDenied("x".into())));
        assert_eq!(refusal(&denied), ScreenRefusal::Refused);
        assert_eq!(
            refusal(&method_error("org.freedesktop.DBus.Error.AccessDenied")),
            ScreenRefusal::Refused
        );
        assert_eq!(
            refusal(&method_error("org.freedesktop.DBus.Error.ServiceUnknown")),
            ScreenRefusal::Unavailable
        );
        let unknown = zbus::Error::FDO(Box::new(zbus::fdo::Error::ServiceUnknown("x".into())));
        assert_eq!(refusal(&unknown), ScreenRefusal::Unavailable);
        assert_eq!(
            refusal(&method_error("org.freedesktop.DBus.Error.UnknownMethod")),
            ScreenRefusal::Refused
        );
        assert_eq!(refusal(&zbus::Error::InvalidReply), ScreenRefusal::Refused);
    }

    /// GIVEN a request cancelled before the warning WHEN a source is chosen
    /// THEN it ends as cancelled and neither the warning nor the bus runs.
    #[test]
    fn given_a_cancel_before_the_warning_then_the_choice_is_cancelled() {
        assert!(PortalScreen.available());
        let cancel = Arc::new(AtomicBool::new(true));
        assert!(matches!(
            PortalScreen.choose(&cancel),
            Err(ScreenRefusal::Cancelled)
        ));
    }

    /// GIVEN a wait for a call's result WHEN the result arrives, also after
    /// a cancel check, THEN it is returned; WHEN the request is cancelled
    /// THEN the wait ends as cancelled; WHEN the call's thread ends without
    /// a result THEN it is a refusal.
    #[test]
    fn given_a_wait_then_a_result_a_cancel_or_a_lost_call_ends_it() {
        let open = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel();
        tx.send(7).unwrap();
        assert_eq!(wait(&rx, &open), Ok(7));

        let late = std::thread::spawn(move || {
            std::thread::sleep(CANCEL_CHECK * 2);
            tx.send(8).unwrap();
        });
        assert_eq!(wait(&rx, &open), Ok(8));
        late.join().unwrap();
        assert_eq!(wait(&rx, &open), Err(ScreenRefusal::Refused));

        let (_tx, pending) = mpsc::channel::<u8>();
        assert_eq!(
            wait(&pending, &AtomicBool::new(true)),
            Err(ScreenRefusal::Cancelled)
        );
    }

    /// GIVEN `Response` signals WHEN read THEN code 0 gives the results,
    /// code 1 is the user's cancel and any other code is a refusal; a body of
    /// another shape, an ended signal stream and a lost listener are
    /// refusals, and a cancel ends the wait.
    #[test]
    fn given_portal_responses_then_only_code_zero_gives_the_results() {
        let open = AtomicBool::new(false);
        let read = |message: Option<zbus::Message>| {
            let (tx, rx) = mpsc::channel();
            tx.send(message).unwrap();
            wait_response(&rx, &open)
        };
        let results = read(Some(response(
            0,
            HashMap::from([("session_handle", Value::from("/x/session"))]),
        )))
        .unwrap();
        assert_eq!(
            results
                .get("session_handle")
                .and_then(|value| <&str>::try_from(&**value).ok()),
            Some("/x/session")
        );
        assert_eq!(
            read(Some(response(1, HashMap::new()))).err(),
            Some(ScreenRefusal::Cancelled)
        );
        assert_eq!(
            read(Some(response(2, HashMap::new()))).err(),
            Some(ScreenRefusal::Refused)
        );
        let shapeless = zbus::Message::signal(PORTAL_PATH, REQUEST, "Response")
            .unwrap()
            .build(&("0",))
            .unwrap();
        assert_eq!(read(Some(shapeless)).err(), Some(ScreenRefusal::Refused));
        assert_eq!(read(None).err(), Some(ScreenRefusal::Refused));

        let (tx, lost) = mpsc::channel::<Option<zbus::Message>>();
        drop(tx);
        assert_eq!(
            wait_response(&lost, &open).err(),
            Some(ScreenRefusal::Refused)
        );
        let (_tx, pending) = mpsc::channel::<Option<zbus::Message>>();
        assert_eq!(
            wait_response(&pending, &AtomicBool::new(true)).err(),
            Some(ScreenRefusal::Cancelled)
        );
    }

    /// The variable that tells [`share_in_a_child`] what to expect.
    const EXPECT: &str = "PERSEPHONE_TEST_SCREEN_BUS";

    /// GIVEN no session bus WHEN a share opens THEN screen sharing is
    /// unavailable; GIVEN a session bus setting that cannot be read THEN it
    /// is refused. Each case runs in a child copy of this test program, so
    /// no test changes this process's environment.
    #[test]
    fn given_no_usable_session_bus_then_the_share_does_not_open() {
        use std::process::Command;
        let runtime = tempfile::tempdir().unwrap();
        let run = |setting: Option<&str>, expect: &str| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "platform::linux::screen_portal::tests::share_in_a_child",
                    "--ignored",
                ])
                .env(EXPECT, expect)
                .env("XDG_RUNTIME_DIR", runtime.path());
            match setting {
                Some(value) => command.env("DBUS_SESSION_BUS_ADDRESS", value),
                None => command.env_remove("DBUS_SESSION_BUS_ADDRESS"),
            };
            let output = command.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "{expect}: {stdout}");
            assert!(stdout.contains("1 passed"), "{expect}: {stdout}");
        };
        run(None, "unavailable");
        run(Some(""), "refused");
    }

    /// Run only by [`given_no_usable_session_bus_then_the_share_does_not_open`],
    /// in a child process whose environment it set.
    #[test]
    #[ignore = "run in a child process by given_no_usable_session_bus_then_the_share_does_not_open"]
    fn share_in_a_child() {
        let expected = match std::env::var(EXPECT).as_deref() {
            Ok("unavailable") => ScreenRefusal::Unavailable,
            Ok("refused") => ScreenRefusal::Refused,
            _ => return,
        };
        let outcome = open_share(&AtomicBool::new(false));
        assert!(
            matches!(&outcome, Err(refusal) if *refusal == expected),
            "{:?}",
            outcome.err()
        );
    }
}

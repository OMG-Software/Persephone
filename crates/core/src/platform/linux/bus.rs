//! The user's session bus, shared by the Secret Service vault, the UI's
//! systemd scope and the OpenURI portal (ADR-0010).
//!
//! Each use opens its own connection and drops it afterwards: the core uses
//! the bus rarely (opening a profile, starting the UI, opening a link), and a
//! fresh connection carries no state from an earlier use.

use std::time::Duration;
use zbus::blocking::{Connection, connection::Builder};

/// Longest wait for one method reply. A Secret Service prompt is not a
/// method reply: its wait is bounded separately (`vault::PROMPT_TIMEOUT`).
pub const METHOD_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a bus call did not succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusError {
    /// There is no session bus (no address for one, or no socket where it
    /// should be), or no service owns the name and none can be started for
    /// it.
    Absent,
    /// The bus or the service refused, failed or timed out, or a reply did
    /// not have the expected form.
    Failed,
}

/// A new connection to the session bus, with [`METHOD_TIMEOUT`] on every
/// call. See [`configured_address`] and [`connect_error`] for what counts
/// as no session bus.
pub fn session() -> Result<Connection, BusError> {
    let configured = configured_address(std::env::var_os("DBUS_SESSION_BUS_ADDRESS").as_deref())?;
    Builder::session()
        .and_then(|builder| builder.method_timeout(METHOD_TIMEOUT).build())
        .map_err(|error| connect_error(&error, configured))
}

/// Whether `DBUS_SESSION_BUS_ADDRESS` (`value`) names a session bus. Unset
/// is `false`: zbus then uses the runtime directory's bus. A value that is
/// set but empty or not UTF-8 is [`BusError::Failed`]: zbus would ignore
/// it and fall back to that default, so a broken setting could be taken for
/// a missing bus.
pub fn configured_address(value: Option<&std::ffi::OsStr>) -> Result<bool, BusError> {
    match value {
        None => Ok(false),
        Some(value) if value.is_empty() || value.to_str().is_none() => Err(BusError::Failed),
        Some(_) => Ok(true),
    }
}

/// A new connection to the bus at `address` (tests use one with no bus
/// behind it), sorted as for [`session`], with `address` as a configured
/// address.
pub fn at(address: &str) -> Result<Connection, BusError> {
    Builder::address(address)
        .and_then(|builder| builder.method_timeout(METHOD_TIMEOUT).build())
        .map_err(|error| connect_error(&error, true))
}

/// Sorts a failed connection. Only a confirmed absence is
/// [`BusError::Absent`]: no address is configured and none can be made, or
/// no socket exists at the address. A configured address that cannot be
/// read, a socket that refuses the connection or the permission, a failed
/// authentication and anything else are [`BusError::Failed`], so a caller
/// never takes the "no session bus" path for a bus that may exist.
pub fn connect_error(error: &zbus::Error, configured: bool) -> BusError {
    let missing = |error: &std::io::Error| error.kind() == std::io::ErrorKind::NotFound;
    match error {
        zbus::Error::Address(_) if !configured => BusError::Absent,
        zbus::Error::Connection(error, _) | zbus::Error::InputOutput(error) if missing(error) => {
            BusError::Absent
        }
        _ => BusError::Failed,
    }
}

/// Whether a D-Bus error name says that no service owns the called name and
/// none can be activated for it.
pub fn absent_name(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner"
            | "org.freedesktop.DBus.Error.Spawn.ServiceNotFound"
    )
}

/// Sorts a failed call: a missing service is [`BusError::Absent`];
/// everything else, including a timeout and an access refusal, is
/// [`BusError::Failed`].
pub fn classify(error: &zbus::Error) -> BusError {
    let absent = match error {
        zbus::Error::MethodError(name, _, _) => absent_name(name.as_str()),
        zbus::Error::FDO(error) => matches!(
            **error,
            zbus::fdo::Error::ServiceUnknown(_)
                | zbus::fdo::Error::NameHasNoOwner(_)
                | zbus::fdo::Error::SpawnServiceNotFound(_)
        ),
        _ => false,
    };
    if absent {
        BusError::Absent
    } else {
        BusError::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN D-Bus error names THEN only the "no such service" names count
    /// as an absent service; a refusal, a timeout or an unknown method is a
    /// failure, never a reason to take the "absent" path.
    #[test]
    fn given_bus_error_names_then_only_missing_services_are_absent() {
        for name in [
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "org.freedesktop.DBus.Error.NameHasNoOwner",
            "org.freedesktop.DBus.Error.Spawn.ServiceNotFound",
        ] {
            assert!(absent_name(name), "{name}");
        }
        for name in [
            "org.freedesktop.DBus.Error.AccessDenied",
            "org.freedesktop.DBus.Error.NoReply",
            "org.freedesktop.DBus.Error.Timeout",
            "org.freedesktop.DBus.Error.UnknownMethod",
            "org.freedesktop.Secret.Error.IsLocked",
            "org.freedesktop.systemd1.UnitExists",
            "",
        ] {
            assert!(!absent_name(name), "{name}");
        }
        let unknown = zbus::Error::FDO(Box::new(zbus::fdo::Error::ServiceUnknown("x".into())));
        assert_eq!(classify(&unknown), BusError::Absent);
        let denied = zbus::Error::FDO(Box::new(zbus::fdo::Error::AccessDenied("x".into())));
        assert_eq!(classify(&denied), BusError::Failed);
        assert_eq!(classify(&zbus::Error::InvalidReply), BusError::Failed);
    }

    /// GIVEN the session bus setting THEN unset means "not configured", a
    /// readable value means configured, and an empty or non-UTF-8 value is a
    /// failure, never a missing bus.
    #[test]
    fn given_session_bus_settings_then_only_unset_is_unconfigured() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        assert_eq!(configured_address(None), Ok(false));
        assert_eq!(
            configured_address(Some(OsStr::new("unix:path=/run/user/1000/bus"))),
            Ok(true)
        );
        // Read but not parsed here: a malformed one fails at the connection.
        assert_eq!(
            configured_address(Some(OsStr::new("not-an-address"))),
            Ok(true)
        );
        assert_eq!(
            configured_address(Some(OsStr::new(""))),
            Err(BusError::Failed)
        );
        assert_eq!(
            configured_address(Some(OsStr::from_bytes(b"unix:path=/x\xff"))),
            Err(BusError::Failed)
        );
    }

    /// GIVEN a bus address with no socket behind it THEN there is no bus;
    /// GIVEN a socket that exists but refuses the connection THEN the
    /// connection failed, and is never taken for a missing bus.
    #[test]
    fn given_connection_failures_then_only_a_missing_socket_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = format!("unix:path={}", dir.path().join("no-bus").display());
        assert_eq!(at(&missing).err(), Some(BusError::Absent));
        // A socket file left by a listener that has gone: ECONNREFUSED.
        let stale = dir.path().join("stale-bus");
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
        let refused = format!("unix:path={}", stale.display());
        assert_eq!(at(&refused).err(), Some(BusError::Failed));
        // No address configured, and none could be made: no bus.
        assert_eq!(
            connect_error(&zbus::Error::Address("no session bus".into()), false),
            BusError::Absent
        );
        // A configured address that cannot be read is not a missing bus.
        for malformed in ["not-an-address", "unix:", "unix:path=/x,abstract=y"] {
            assert_eq!(at(malformed).err(), Some(BusError::Failed), "{malformed}");
        }
        assert_eq!(
            connect_error(&zbus::Error::Address("malformed".into()), true),
            BusError::Failed
        );
        assert_eq!(
            connect_error(&zbus::Error::Handshake("rejected".into()), false),
            BusError::Failed
        );
    }
}

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
    /// There is no session bus, or no service owns the name and none can be
    /// started for it.
    Absent,
    /// The service refused the call, or its reply did not have the expected
    /// form.
    Failed,
}

/// A new connection to the session bus, with [`METHOD_TIMEOUT`] on every
/// call. No reachable session bus is [`BusError::Absent`].
pub fn session() -> Result<Connection, BusError> {
    Builder::session()
        .and_then(|builder| builder.method_timeout(METHOD_TIMEOUT).build())
        .map_err(|_| BusError::Absent)
}

/// A new connection to the bus at `address` (tests use one with no bus
/// behind it). An unreachable address is [`BusError::Absent`].
pub fn at(address: &str) -> Result<Connection, BusError> {
    Builder::address(address)
        .and_then(|builder| builder.method_timeout(METHOD_TIMEOUT).build())
        .map_err(|_| BusError::Absent)
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
}

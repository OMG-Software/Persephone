//! Opening a confirmed link (ADR-0024, ADR-0030) through the desktop's
//! OpenURI portal (`org.freedesktop.portal.OpenURI`).
//!
//! The core passes the checked address as one D-Bus string argument; the
//! portal hands it to the user's default browser. No shell, `xdg-open`
//! script or other program is started by the core with the address, so no
//! part of it can be read as a command or an option.
//!
//! Without a portal (no session bus, or no `xdg-desktop-portal` service),
//! nothing opens and the call fails. Success means that the portal accepted
//! the request; the core does not wait for the browser to start.

use super::bus::{self, BusError};
use std::collections::HashMap;
use zbus::zvariant::Value;

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const OPEN_URI: &str = "org.freedesktop.portal.OpenURI";

/// The `OpenURI` arguments: no parent window, the address, no options (the
/// portal's defaults: not writable, no "ask" dialog forced).
pub fn open_uri_body(url: &str) -> (&'static str, &str, HashMap<&'static str, Value<'static>>) {
    ("", url, HashMap::new())
}

/// Asks the portal to open `url`, which the caller has checked.
pub fn open_uri(url: &str) -> Result<(), BusError> {
    let connection = bus::session()?;
    connection
        .call_method(
            Some(PORTAL),
            PORTAL_PATH,
            Some(OPEN_URI),
            "OpenURI",
            &open_uri_body(url),
        )
        .map(drop)
        .map_err(|error| bus::classify(&error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::DynamicType;

    /// GIVEN a link THEN the portal call carries it unchanged as one string,
    /// with no parent window and no options, in the `OpenURI` signature.
    #[test]
    fn given_a_link_then_the_portal_gets_it_as_one_argument() {
        let url = "https://example.org/a?b=c&d=$(e)";
        let body = open_uri_body(url);
        assert_eq!(body.0, "");
        assert_eq!(body.1, url);
        assert!(body.2.is_empty());
        assert_eq!(body.signature().to_string(), "(ssa{sv})");
    }
}

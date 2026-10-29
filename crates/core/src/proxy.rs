//! The profile's SOCKS5 proxy (ADR-0028): when set, every connection the
//! core makes goes through it, the homeserver's name resolved by the proxy.
//!
//! * It belongs to the profile, not the account: it applies before sign-in
//!   too, to discovery and to the login. It is one JSON record in the
//!   encrypted crypto store, so signing out wipes it with the profile.
//! * Only a host (a DNS name or an IP address) and a port are kept. The
//!   proxy is used as `socks5h`, so names are resolved by the proxy and not
//!   on this computer. No user name or password is kept.
//! * A record that cannot be read refuses every connection rather than
//!   connecting directly.
//! * A change applies to connections made after it: the running client
//!   keeps its own until Persephone starts again.

use crate::store::Stores;
use matrix_sdk_crypto::store::CryptoStore;
use serde::{Deserialize, Serialize};
use url::{Host, Url};

pub const PROXY_KEY: &str = "persephone.proxy.v1";
const MAX_PROXY_RECORD_BYTES: usize = 4096;
/// Longest proxy address either side accepts, in bytes.
pub const MAX_PROXY_BYTES: usize = 300;

/// A checked SOCKS5 proxy: its host and port.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Socks {
    host: String,
    port: u16,
}

impl Socks {
    /// Parses `host:port`, or the same after `socks5://` or `socks5h://`.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.is_empty()
            || raw.len() > MAX_PROXY_BYTES
            || raw.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return None;
        }
        let address = raw
            .strip_prefix("socks5h://")
            .or_else(|| raw.strip_prefix("socks5://"))
            .unwrap_or(raw);
        let url = Url::parse(&format!("socks5h://{address}")).ok()?;
        let port = url.port()?;
        let host = match url.host()? {
            Host::Domain(name) if !name.is_empty() => name.to_ascii_lowercase(),
            Host::Domain(_) => return None,
            Host::Ipv4(ip) => ip.to_string(),
            Host::Ipv6(ip) => format!("[{ip}]"),
        };
        let only_address = url.username().is_empty()
            && url.password().is_none()
            && matches!(url.path(), "" | "/")
            && url.query().is_none()
            && url.fragment().is_none()
            && port != 0;
        only_address.then_some(Self { host, port })
    }

    /// The address as shown and stored: `host:port`.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The proxy for every request, with names resolved by the proxy.
    pub fn reqwest(&self) -> reqwest::Result<reqwest::Proxy> {
        reqwest::Proxy::all(format!("socks5h://{}", self.address()))
    }
}

/// The proxy record cannot be read: connections must be refused.
#[derive(Debug, PartialEq, Eq)]
pub struct Unreadable;

/// The profile's proxy: `Ok(None)` when none is set.
pub async fn load(stores: &Stores) -> Result<Option<Socks>, Unreadable> {
    match stores.crypto().get_custom_value(PROXY_KEY).await {
        Ok(None) => Ok(None),
        Ok(Some(bytes)) if bytes.len() <= MAX_PROXY_RECORD_BYTES => {
            let stored: Option<Socks> = serde_json::from_slice(&bytes).map_err(|_| Unreadable)?;
            // A record from elsewhere is checked like typed input.
            match stored {
                None => Ok(None),
                Some(socks) => Socks::parse(&socks.address()).map(Some).ok_or(Unreadable),
            }
        }
        _ => Err(Unreadable),
    }
}

/// Replaces the profile's proxy; `None` removes it.
pub async fn save(stores: &Stores, socks: Option<&Socks>) -> bool {
    let Ok(bytes) = serde_json::to_vec(&socks) else {
        return false;
    };
    stores
        .crypto()
        .set_custom_value(PROXY_KEY, bytes)
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN proxy addresses WHEN parsed THEN a host and port pass, with or
    /// without a socks5 scheme, and anything with credentials, a path, no
    /// port or another scheme is refused.
    #[test]
    fn given_proxy_addresses_then_only_a_host_and_port_pass() {
        for (raw, address) in [
            ("127.0.0.1:9050", "127.0.0.1:9050"),
            ("socks5://localhost:9050", "localhost:9050"),
            ("socks5h://Proxy.Example.org:1080", "proxy.example.org:1080"),
            ("[::1]:9050", "[::1]:9050"),
        ] {
            assert_eq!(
                Socks::parse(raw).map(|socks| socks.address()).as_deref(),
                Some(address),
                "{raw}"
            );
        }
        for bad in [
            "",
            "localhost",
            "localhost:0",
            "localhost:99999",
            "user:secret@localhost:9050",
            "localhost:9050/path",
            "http://localhost:8080",
            "socks4://localhost:9050",
            "local host:9050",
            "localhost:9050?x",
        ] {
            assert!(Socks::parse(bad).is_none(), "{bad:?}");
        }
    }

    /// GIVEN a proxy WHEN made for reqwest THEN it resolves names remotely.
    #[test]
    fn given_a_proxy_then_it_is_used_as_socks5h() {
        let socks = Socks::parse("127.0.0.1:9050").unwrap();
        assert!(socks.reqwest().is_ok());
        assert_eq!(
            serde_json::to_string(&Some(&socks)).unwrap(),
            r#"{"host":"127.0.0.1","port":9050}"#
        );
    }
}

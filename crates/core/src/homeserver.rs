//! Which homeservers the core talks to, and how.
//!
//! The homeserver URL comes from the UI, so it is untrusted. Only `https://`
//! with a host and nothing else (no user info, query, fragment or path) is
//! accepted. The `dev-disposable-servers` feature, for the disposable servers in
//! `tools/servers`, also accepts `http://127.0.0.1:<port>` and
//! `http://[::1]:<port>`; it is off by default and in CI. Every connection uses
//! TLS 1.3 or later, no ambient proxy and no redirects.

use std::net::{Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

pub const MAX_HOMESERVER_URL_BYTES: usize = 2048;
/// Longest host shown in the credential prompt (a DNS name's limit).
pub const MAX_DISPLAY_HOST_CHARS: usize = 253;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeserverError {
    /// Not a URL this client accepts.
    Invalid,
    /// Plain HTTP, which only development builds accept, and only to loopback.
    PlainHttp,
}

/// A validated homeserver base URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Homeserver {
    url: Url,
}

fn is_loopback_literal(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => ip == Ipv4Addr::LOCALHOST,
        Some(Host::Ipv6(ip)) => ip == Ipv6Addr::LOCALHOST,
        _ => false,
    }
}

impl Homeserver {
    pub fn parse(input: &str) -> Result<Self, HomeserverError> {
        if input.is_empty() || input.len() > MAX_HOMESERVER_URL_BYTES || input.trim() != input {
            return Err(HomeserverError::Invalid);
        }
        let url = Url::parse(input).map_err(|_| HomeserverError::Invalid)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || url.host().is_none()
        {
            return Err(HomeserverError::Invalid);
        }
        match url.scheme() {
            "https" => Ok(Self { url }),
            "http"
                if cfg!(feature = "dev-disposable-servers")
                    && is_loopback_literal(&url)
                    && url.port().is_some() =>
            {
                Ok(Self { url })
            }
            "http" => Err(HomeserverError::PlainHttp),
            _ => Err(HomeserverError::Invalid),
        }
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    /// The host (and a non-default port) to show the user. `url` stores
    /// international domain names as punycode, so this is plain ASCII.
    pub fn display_host(&self) -> String {
        let host = self.url.host_str().unwrap_or_default();
        let shown = match self.url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        shown.chars().take(MAX_DISPLAY_HOST_CHARS).collect()
    }
}

/// The single HTTP client policy for Matrix connections: TLS 1.3 minimum, no
/// ambient proxy, no redirects (as the P0 fixtures' `policy_http_client_builder`).
pub fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .tls_version_min(reqwest::tls::Version::TLS_1_3)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_https_homeservers_then_they_are_accepted_and_shown_by_host() {
        for (input, shown) in [
            ("https://matrix.example.org", "matrix.example.org"),
            ("https://matrix.example.org/", "matrix.example.org"),
            ("https://matrix.example.org:8448", "matrix.example.org:8448"),
            ("https://bücher.example", "xn--bcher-kva.example"),
            ("https://[2001:db8::1]", "[2001:db8::1]"),
        ] {
            let homeserver = Homeserver::parse(input).unwrap_or_else(|e| panic!("{input}: {e:?}"));
            assert_eq!(homeserver.display_host(), shown, "{input}");
            assert!(homeserver.display_host().is_ascii());
        }
    }

    #[test]
    fn given_other_urls_then_they_are_refused() {
        for input in [
            "",
            " https://matrix.example.org",
            "https://matrix.example.org ",
            "matrix.example.org",
            "https://",
            "https://user:pass@matrix.example.org",
            "https://user@matrix.example.org",
            "https://matrix.example.org/_matrix",
            "https://matrix.example.org/?next=x",
            "https://matrix.example.org/#x",
            "ftp://matrix.example.org",
            "file:///C:/Windows",
            "javascript:alert(1)",
            "data:text/html,x",
            "wss://matrix.example.org",
            &format!("https://{}.org", "a".repeat(MAX_HOMESERVER_URL_BYTES)),
        ] {
            assert_eq!(
                Homeserver::parse(input),
                Err(HomeserverError::Invalid),
                "{input:?}"
            );
        }
    }

    #[test]
    fn given_plain_http_then_only_development_builds_accept_loopback_literals() {
        for input in ["http://127.0.0.1:8008", "http://[::1]:8008"] {
            let result = Homeserver::parse(input);
            if cfg!(feature = "dev-disposable-servers") {
                assert!(result.is_ok(), "{input}");
            } else {
                assert_eq!(result, Err(HomeserverError::PlainHttp), "{input}");
            }
        }
        // Never accepted, with or without the feature.
        for input in [
            "http://localhost:8008",
            "http://127.0.0.2:8008",
            "http://127.0.0.1",
            "http://0.0.0.0:8008",
            "http://192.168.1.10:8008",
            "http://matrix.example.org",
            "http://[::ffff:127.0.0.1]:8008",
        ] {
            assert_eq!(
                Homeserver::parse(input),
                Err(HomeserverError::PlainHttp),
                "{input}"
            );
        }
    }

    #[test]
    fn given_the_policy_client_then_it_builds() {
        assert!(http_client().is_ok());
    }
}

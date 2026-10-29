//! Finding a homeserver from a server name, through
//! `/.well-known/matrix/client` (ADR-0027).
//!
//! * The page sends a server name, such as `example.org`; it is checked to be
//!   a host with an optional port, nothing else.
//! * The core fetches `https://<name>/.well-known/matrix/client` with the
//!   Matrix client policy (`homeserver::http_client`: TLS 1.3, no ambient
//!   proxy, the profile's SOCKS5 proxy if set, no redirects), at most [`MAX_WELL_KNOWN_BYTES`] within [`DISCOVERY_TIMEOUT`].
//!   A 404 means none: the name itself is tried as the homeserver. Any other
//!   failure stops discovery.
//! * The `m.homeserver.base_url` found must pass [`Homeserver::parse`] like
//!   any typed address, so it is `https://` with a host and no path.
//! * The homeserver is then confirmed with `/_matrix/client/versions`.
//! * The page shows the address found before anything is sent to it: a
//!   `.well-known` file can name a server on another host.

use crate::homeserver::{self, Homeserver};
use crate::proxy::Socks;
use persephone_contracts::HomeserverFound;
use persephone_core_protocol::ErrorCode;
use std::time::Duration;
use url::Url;

/// Longest `.well-known` or `/versions` body read, in bytes.
pub const MAX_WELL_KNOWN_BYTES: usize = 64 * 1024;
/// Longest each discovery request may take.
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest server name accepted, in bytes (a DNS name with a port).
pub const MAX_SERVER_NAME_BYTES: usize = 255 + 6;

/// The `https://` origin of a server name: a host (DNS name, IPv4 or a
/// bracketed IPv6 address) with an optional port, and nothing else.
pub fn origin(name: &str) -> Option<Url> {
    if name.is_empty()
        || name.len() > MAX_SERVER_NAME_BYTES
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.:[]".contains(&b))
    {
        return None;
    }
    let url = Url::parse(&format!("https://{name}/")).ok()?;
    let host = url.host_str()?;
    let shown = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    // The URL parser must not have read anything else into the name. It
    // drops the default port, so an explicit `:443` is compared as well.
    let same = shown.eq_ignore_ascii_case(name)
        || (url.port().is_none() && format!("{host}:443").eq_ignore_ascii_case(name));
    (same && url.path() == "/").then_some(url)
}

/// The homeserver a `.well-known/matrix/client` body names. A trailing
/// slash is allowed; anything `Homeserver::parse` refuses is refused.
pub fn named_homeserver(body: &[u8]) -> Option<Homeserver> {
    let document: serde_json::Value = serde_json::from_slice(body).ok()?;
    let base = document.get("m.homeserver")?.get("base_url")?.as_str()?;
    let base = base.trim_end_matches('/');
    Homeserver::parse(&format!("{base}/")).ok()
}

/// Whether a `/_matrix/client/versions` body is a Matrix server's.
fn speaks_matrix(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|document| {
            document
                .get("versions")?
                .as_array()
                .map(|versions| !versions.is_empty())
        })
        .unwrap_or(false)
}

/// GETs `url`: the status and, for a success, the body, at most
/// [`MAX_WELL_KNOWN_BYTES`].
async fn fetch(client: &reqwest::Client, url: Url) -> Result<(u16, Vec<u8>), ErrorCode> {
    let mut response = client
        .get(url)
        .timeout(DISCOVERY_TIMEOUT)
        .send()
        .await
        .map_err(|_| ErrorCode::Unavailable)?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        return Ok((status, Vec::new()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ErrorCode::Unavailable)? {
        if body.len() + chunk.len() > MAX_WELL_KNOWN_BYTES {
            return Err(ErrorCode::Rejected);
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, body))
}

/// Finds and confirms the homeserver of `name`: `not_found` when there is
/// no Matrix server, `rejected` when the `.well-known` file is not valid,
/// `unavailable` when a server cannot be reached.
pub async fn discover(name: &str, proxy: Option<&Socks>) -> Result<HomeserverFound, ErrorCode> {
    let origin = origin(name).ok_or(ErrorCode::InvalidArgument)?;
    let client = homeserver::http_client(proxy).map_err(|_| ErrorCode::Internal)?;
    let well_known = origin
        .join(".well-known/matrix/client")
        .map_err(|_| ErrorCode::Internal)?;
    let (found, from_well_known) = match fetch(&client, well_known).await? {
        (200, body) => (named_homeserver(&body).ok_or(ErrorCode::Rejected)?, true),
        (404, _) => (
            Homeserver::parse(origin.as_str()).map_err(|_| ErrorCode::Rejected)?,
            false,
        ),
        _ => return Err(ErrorCode::Rejected),
    };
    let versions = found
        .url()
        .join("_matrix/client/versions")
        .map_err(|_| ErrorCode::Internal)?;
    match fetch(&client, versions).await? {
        (200, body) if speaks_matrix(&body) => Ok(HomeserverFound {
            homeserver: found.url().to_string(),
            from_well_known,
        }),
        _ => Err(ErrorCode::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN server names WHEN checked THEN hosts with an optional port pass,
    /// and anything carrying a scheme, path, user, query or space is refused.
    #[test]
    fn given_server_names_then_only_hosts_and_ports_pass() {
        for good in [
            "example.org",
            "matrix.example.org:8448",
            "Example.ORG",
            "[2001:db8::1]:443",
        ] {
            assert!(origin(good).is_some(), "{good}");
        }
        for bad in [
            "",
            "https://example.org",
            "example.org/path",
            "user@example.org",
            "example.org?x",
            "exa mple.org",
            "example.org#",
            "例え.jp",
            &"a".repeat(MAX_SERVER_NAME_BYTES + 1),
        ] {
            assert!(origin(bad).is_none(), "{bad:?}");
        }
        assert_eq!(
            origin("example.org:8448").unwrap().as_str(),
            "https://example.org:8448/"
        );
    }

    /// GIVEN `.well-known` bodies WHEN read THEN an https base URL is taken,
    /// with or without a trailing slash, and anything else is refused.
    #[test]
    fn given_well_known_bodies_then_only_valid_https_homeservers_are_named() {
        let named = |base: &str| {
            named_homeserver(
                serde_json::json!({ "m.homeserver": { "base_url": base } })
                    .to_string()
                    .as_bytes(),
            )
            .map(|found| found.url().to_string())
        };
        assert_eq!(
            named("https://matrix.example.org").as_deref(),
            Some("https://matrix.example.org/")
        );
        assert_eq!(
            named("https://matrix.example.org/").as_deref(),
            Some("https://matrix.example.org/")
        );
        for bad in [
            "http://matrix.example.org",
            "https://matrix.example.org/prefix",
            "https://user@matrix.example.org",
            "javascript:alert(1)",
            "",
        ] {
            assert_eq!(named(bad), None, "{bad:?}");
        }
        for body in [
            &b"not json"[..],
            b"{}",
            br#"{"m.homeserver":{}}"#,
            br#"{"m.homeserver":{"base_url":1}}"#,
        ] {
            assert!(named_homeserver(body).is_none());
        }
    }

    /// GIVEN `/versions` bodies WHEN read THEN only a list of versions counts.
    #[test]
    fn given_versions_bodies_then_only_a_version_list_counts() {
        assert!(speaks_matrix(br#"{"versions":["v1.11"]}"#));
        for body in [
            &b"{}"[..],
            br#"{"versions":[]}"#,
            br#"{"versions":"v1"}"#,
            b"<html>",
        ] {
            assert!(!speaks_matrix(body), "{body:?}");
        }
    }
}

// Copyright 2026 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::TransportError;

/// Map a tungstenite error from a WS handshake to a [`TransportError`].
///
/// When the server returns an HTTP error response during the upgrade (e.g. 404
/// on the /rtc endpoint), tungstenite surfaces it as `Error::Http(response)`.
/// We extract the status code and return `TransportError::Http { status }` so
/// callers can distinguish HTTP error codes (403, 404) from network errors.
fn map_ws_err(e: tokio_tungstenite::tungstenite::Error) -> TransportError {
    use tokio_tungstenite::tungstenite::Error;
    match e {
        Error::Http(resp) => TransportError::Http { status: resp.status().as_u16() },
        other => TransportError::Connection(other.to_string()),
    }
}

use std::sync::Arc;

use tokio::net::TcpStream;
use tokio_rustls::rustls::{self, RootCertStore};
use tokio_tungstenite::{connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream};

// Persephone (ADR-0043): this file once connected through HTTPS_PROXY or
// HTTP_PROXY. Those variables are no longer read, so the signalling socket
// always connects directly (the core asks the user before a direct call).

/// Persephone (ADR-0043): the one TLS client configuration of this crate.
/// TLS 1.3 only, the system root store, certificate verification on.
pub(super) fn tls13_client_config() -> Result<Arc<rustls::ClientConfig>, TransportError> {
    let loaded = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let (added, _ignored) = roots.add_parsable_certificates(loaded.certs);
    if added == 0 {
        return Err(TransportError::Connection("no usable native root certificates".into()));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| TransportError::Connection("TLS 1.3 is unavailable".into()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

pub(super) async fn connect_ws(
    request: http::Request<()>,
    _url: &url::Url,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, TransportError> {
    // Persephone (ADR-0043): an explicit TLS 1.3 connector replaces the
    // tokio-tungstenite default (which allows TLS 1.2), and there is no proxy.
    let connector = Connector::Rustls(tls13_client_config()?);
    let (ws_stream, _) = connect_async_tls_with_config(request, None, false, Some(connector))
        .await
        .map_err(map_ws_err)?;
    Ok(ws_stream)
}

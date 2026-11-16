# Persephone patch: livekit-net 0.2.0 (ADR-0043)

Upstream: crates.io `livekit-net` 0.2.0, Apache-2.0. The published crate has no
license file; `LICENSE-APACHE` is the standard Apache-2.0 text, copied from the
`serde` 1.0.229 crate. Used through `[patch.crates-io]` in the root
`Cargo.toml`. Reapply this patch for each LiveKit update. Every changed spot
carries a `// Persephone (ADR-0043):` comment.

Requires the feature `rustls-tls-native-roots` (it uses `rustls-native-certs`
and `tokio-rustls`). The call worker enables it.

## `src/native/proxy.rs`

- `connect_ws` no longer reads `HTTPS_PROXY`, `https_proxy`, `HTTP_PROXY` or
  `http_proxy`. The CONNECT-proxy code is removed. The signalling socket
  always connects directly (the core decides about proxies).
- `connect_ws` calls `connect_async_tls_with_config` with an explicit
  `Connector::Rustls`, instead of `connect_async`, whose default
  `tokio-tungstenite` connector also accepts TLS 1.2.
- New `tls13_client_config()`: root store from `rustls_native_certs`
  (the operating system's store; fails when it has no usable root), the `ring`
  provider, `with_protocol_versions(&[&rustls::version::TLS13])`, certificate
  verification on, no client authentication.

## `src/native/mod.rs`

- `NativeTransport::new` builds its `reqwest` client with `.no_proxy()` and
  `.use_preconfigured_tls(tls13_client_config())`, so HTTP requests (region and
  validate calls) are also TLS 1.3 only and ignore proxy variables.
- When the TLS 1.3 client cannot be built (for example, no usable root in the
  system store), `NativeTransport::new` keeps no client instead of panicking.
  Each HTTP request then returns `TransportError::Connection`. The signalling
  socket fails the same way through `connect_ws`.

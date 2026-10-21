# Rust dependency and advisory review

## Current workspace audit

Checked 19 October 2026 against the single root [`Cargo.lock`](../../Cargo.lock). The [current OSV result](rust-advisories.json) queried 418 exact crate/version pairs in five batches and completed without query failures. It contains two maintenance advisories:

| Crate | Advisory | Classification |
| --- | --- | --- |
| `anymap2 0.13.0` | [RUSTSEC-2026-0319](https://rustsec.org/advisories/RUSTSEC-2026-0319.html) | Unmaintained; the advisory recommends `anymap3`. |
| `derivative 2.2.0` | [RUSTSEC-2024-0388](https://rustsec.org/advisories/RUSTSEC-2024-0388.html) | Unmaintained; the advisory suggests alternatives. |

These records identify maintenance status, not an executed exploit or proof of a reachable vulnerability. The JSON preserves the exact OSV responses and package versions. `cargo-audit` was checked and was not installed, so no `cargo audit` run is claimed. The result covers the root Rust lockfile; it does not review npm packages, system libraries, packaged artifacts, or exploitability.

The root workspace check passed 39 Rust tests and 3 frontend tests, with formatting, Clippy, TypeScript, generated-contract drift, and frontend build checks also passing. Dependency policy, file-level native/build license review, other-target graphs, and the OS sandbox acceptance gates remain open. The maintenance disposition is recorded below.

## Maintenance disposition and compiled-graph review

Checked 20 October 2026 against the same root lockfile (unchanged, SHA-256
`599ee5f0…d830`), using Cargo's feature resolver. Only the Windows x64 graph
resolves offline: Linux, macOS, Windows ARM64 and wasm targets need crate sources
that were never downloaded, and none were fetched for this review. Facts that
depend on target-gated dependencies are therefore established for Windows x64
only. Feature-gated facts below were also traced in the cached manifests.

| Crate | Compiled for our features? | Path | Disposition |
| --- | --- | --- | --- |
| `derivative 2.2.0` | **No** (lockfile entry only) | `matrix-sdk` → optional `matrix-sdk-indexeddb` → `matrix_indexed_db_futures` → `wasm_evt_listener` → `derivative` | Not reachable: `matrix-sdk-indexeddb` is an optional dependency enabled only by matrix-sdk's `indexeddb` feature, which no workspace manifest selects. No action; recheck if a browser store is ever enabled. |
| `anymap2 0.13.0` | **Yes**, unconditionally | `matrix-sdk 0.19.1` (required dependency) | **Retain at the current pin; adopt the upstream fix in the next matrix-sdk release.** Do not patch or fork. |

For `anymap2`, matrix-sdk uses one type-keyed map in
`event_handler/mod.rs`. It is filled through `Client::add_event_handler_context`
and read when a handler that takes a `Ctx<T>` argument runs. Source review of
`anymap2 0.13.0`:

- Its 23 `unsafe` sites are unchecked downcasts. Each is reached only after a
  lookup by `TypeId::of::<T>()` in the safe `Map` API. The `RawMap` insert
  methods that could break that invariant are themselves `unsafe`, and matrix-sdk
  does not use them.
- Cloning goes through per-trait `clone_to_any*` methods. No trait-object
  transmute is involved.
- Its single `mem::transmute::<TypeId, u64>` is inside a `#[cfg(test)]` test
  and is not compiled for consumers.
- The production `TypeIdHasher` asserts it receives exactly one 8-byte write.
  `TypeId` is 16 bytes on rustc 1.99.1, but std still hashes it as one `u64`
  write, measured for six types. A toolchain change there would make context
  registration panic, not corrupt memory.

Upstream replaced the crate in
[matrix-org/matrix-rust-sdk#7185](https://github.com/matrix-org/matrix-rust-sdk/pull/7185)
("Switch to anymap3"), merged to `main` on 2 October 2026 and closing
[matrix-org/matrix-rust-sdk#7184](https://github.com/matrix-org/matrix-rust-sdk/issues/7184).
It changed only the manifest, lockfile and `event_handler/mod.rs`. matrix-sdk
0.19.1 (18 September 2026) is still the latest release, so no pinned release
contains the fix. Two regression tests in
[`dependency_contracts.rs`](../../tests/matrix/tests/dependency_contracts.rs)
guard the retained crate. One checks the single-`u64` `TypeId` hashing contract.
The other inserts and replaces event-handler contexts through the public SDK API
against a mock server and confirms no request is sent.

### Compiled graph, Windows x64

Cargo's feature resolver compiles **284 third-party packages** for normal and
build dependencies; 9 more are test-only (`wiremock` and its dependencies). This
is fewer than the 418 lockfile entries the OSV query covered, because the
lockfile also records optional and other-target dependencies.

- **Native code:** `aws-lc-sys`/`aws-lc-rs` (the sole TLS crypto provider) and
  `libsqlite3-sys` (bundled SQLite). `ring`, OpenSSL, `native-tls`, QUIC
  (`quinn`, `h3`) and `wasm-bindgen` are absent. A raw `cargo metadata` walk lists
  `ring` and `quinn`; that graph is not feature-pruned and overstates the build.
- **Selected features:** `matrix-sdk` uses `e2e-encryption` and `rustls-aws-lc-rs`
  with defaults off. `matrix-sdk-sqlite` uses `bundled` and `crypto-store`, plus
  its defaults: `state-store`, `event-cache-store` and `media-store`. `reqwest`
  adds `gzip`, `http2` and `stream`. `rustls` has `aws-lc-rs`, `std` and
  **`tls12`**. reqwest 0.13.5 enables `tls12` itself, so it cannot be removed
  without patching reqwest. The owner decided on **TLS 1.3 only** (20 October
  2026). It is enforced at runtime: every Matrix HTTP client is built from
  `policy_http_client_builder()`, which sets a TLS 1.3 minimum.
  [`tls_policy.rs`](../../tests/matrix/tests/tls_policy.rs) captures the real
  ClientHello (`supported_versions` is TLS 1.3 only) and shows the client
  aborts with a fatal `protocol_version` alert when a server selects TLS 1.2.
  Unrestricted-client controls show both checks would detect a regression.
  rustls still lists TLS 1.2 cipher suites in the ClientHello; that is a
  fingerprint detail, not a negotiable version.
- **Licenses (declared metadata):** all compiled packages are permissive (MIT,
  Apache-2.0, BSD, ISC, Zlib, Unicode-3.0, BSL-1.0, CC0, Unlicense combinations),
  except seven MPL-2.0 crates: `as_variant`, `eyeball`, `eyeball-im`, `readlock`,
  `readlock-tokio`, `imbl` and `imbl-sized-chunks` (the last two MPL-2.0+). None
  of their source files carries the Exhibit B "Incompatible With Secondary
  Licenses" notice; the matches are only the license template text. MPL-2.0 §3.3
  therefore permits combination into this GPL-3.0-only work. Four of them
  (`eyeball`, `eyeball-im`, `readlock`, `readlock-tokio`) ship no license file in
  their crate package, so release notices must supply the MPL text.

Later the same day the WebAssembly decoder added 21 third-party packages (`wasmi` 2.0.0 and its 7 crates in the host; `png` 0.18.1, `zune-jpeg` 0.5.15 and their dependencies in the guest; `wat`/`wast`/`wasm-encoder` for tests only), all MIT, Apache-2.0, Zlib or 0BSD. No existing version changed. The 284-package count above predates them. See the [WebAssembly decoder evidence](wasm-decoder.md).

The Tauri shell then added 202 package versions to the lockfile (Tauri 2.12.4, tauri-build 2.7.3, wry, tao, webview2-com and their dependencies) and `@tauri-apps/api` 2.12.4 to the frontend (no dependencies; `npm audit` reports 0 advisories). No existing version changed. The pinned Tauri releases were six days old when adopted; see the [Tauri shell evidence](tauri-shell.md). Their licence review is pending.

This is a metadata and source-notice review, not a legal opinion. It does not
cover file-level licenses inside `aws-lc-sys` or bundled SQLite, npm packages,
system libraries or packaged artifacts. Those, an SBOM, and the Linux/macOS
graphs remain release work.

## Advisory after the Tauri shell: `glib`

Checked 21 October 2026 against the root lockfile at `f6094cc`. GitHub
Dependabot raised alert 1, [GHSA-wrw7-89jp-8q8g](https://github.com/advisories/GHSA-wrw7-89jp-8q8g)
(medium): unsound `Iterator` and `DoubleEndedIterator` implementations for
`glib::VariantStrIter`, affecting `glib` 0.15.0 up to 0.20.0. GitHub lists no
other identifier for it. The lockfile holds `glib 0.18.5`, added with the Tauri
shell after the OSV audit above, which therefore did not cover it. OSV has not
been re-queried for this entry.

| Crate | Compiled for our features? | Path | Disposition |
| --- | --- | --- | --- |
| `glib 0.18.5` | **Linux only**; not compiled for Windows x64 (`cargo tree -i glib --target x86_64-pc-windows-msvc` prints nothing) | `persephone-desktop` → `tauri 2.12.4` → `gtk`/`webkit2gtk`/`muda`/`tao` → `glib` (Tauri's GTK 0.18 stack) | **Retain at the current pin; adopt a fixed GTK binding through a Tauri release.** Recheck before any Linux build is distributed. |

The Linux graph (`cargo tree -i glib@0.18.5 --target x86_64-unknown-linux-gnu
-e normal`) has 17 crates that depend on `glib` directly or transitively: `atk`, `cairo-rs`, `gdk`,
`gdk-pixbuf`, `gio`, `gtk`, `javascriptcore-rs`, `muda`, `pango`, `soup3`,
`tao`, `tauri`, `tauri-runtime`, `tauri-runtime-wry`, `webkit2gtk`, `wry` and
`persephone-desktop`. A source search of the cached sources of those exact versions found
no use of `VariantStrIter`, `str_iter` or `array_iter_str`; only `glib`'s own
definitions match, and `src-tauri` does not use `glib` directly. This is a
textual search, not proof that no caller reaches the iterator through another
API, and no Linux build has been run. Moving to `glib` 0.20 needs GTK bindings
that Tauri 2.12.4 does not use; patching or forking the GTK stack is out of
scope.

## Historical per-package inventory

Before the repository restructure, five separate Rust lockfiles were audited: matrix (412 registry entries), storage (260), resources (0), Windows sandbox (2), and contracts (20), for 418 distinct crate/version pairs after deduplication. Those counts and package-specific notes describe that earlier workspace shape. They are not the scope of the current single-lockfile result above, and the current JSON must not be interpreted as five separate lockfile reports.

In that earlier layout, the Matrix SDK package selected version 0.19.1 with default features disabled and explicit end-to-end encryption and AWS-LC rustls features. The separate storage package used Matrix SDK 0.19.1 crates and enabled SQLite state, event-cache, media, crypto-store, and bundled features. These were feasibility configurations; they do not set approved application feature policy. The current workspace manifests are the source of truth for present package configuration.

The earlier package metadata noted that all five Rust packages were unpublished and declared `GPL-3.0-only`. It also recorded exact storage and DTO-generator dependency pins in the [storage evidence](storage.md) and [contracts evidence](contracts.md). Those lockfile and test details remain historical run facts, not a current per-package dependency audit.

The previous graph review found `anymap2` under the Matrix SDK package and did not find `derivative` in the selected target feature graph, despite its earlier lockfile entry. Recheck reachability against the current workspace before making a maintenance decision. The current lockfile-level OSV matches alone do not establish reachability.

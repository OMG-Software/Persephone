# Use SDK-owned Matrix cryptography and explicit features

* Status: accepted
* Date: 2026-10-19

## Context and Problem Statement

Persephone needs Matrix synchronization, device identity, encryption, key
sharing, recovery and encrypted media while treating incoming data as hostile.
Implementing those protocols independently would add a large security-critical
surface before the first release. Broad SDK defaults can also select behavior
that has not been reviewed for this client's threat model.

P0 has executable evidence for Matrix Rust SDK 0.19.1, but remaining
interoperability, lifecycle, storage, maintenance and platform gates prevent
production approval of that exact dependency resolution.

## Decision Drivers

* Keep Matrix cryptographic implementation out of application and frontend code.
* Preserve the single Rust session authority and general Matrix interoperability.
* Make dependency features, transitive resolution and key-sharing behavior reviewable.
* Distinguish a successful feasibility check from a production security guarantee.

## Considered Options

* Use the Matrix Rust SDK for protocol and cryptography, with explicit features.
* Implement Matrix protocol and encryption handling within Persephone.
* Place a separate Matrix/crypto stack in the frontend webview.

## Decision Outcome

Chosen option: **use the Matrix Rust SDK for Matrix protocol and cryptography,
with explicit feature selection**. The application coordinates and authorizes
operations, validates public projections and owns lifecycle policy; it does not
implement cryptographic primitives or move long-lived keys into JavaScript.

Use Matrix SDK 0.19.1 for the current compatibility test programs and commit one
root Cargo lockfile. The Matrix integration-test package disables SDK defaults
and enables `e2e-encryption` and `rustls-aws-lc-rs`; storage tests exercise store
capabilities separately. The SDK's defaults include automatic room-key
forwarding. Its production activation and recipient/key-sharing policy require
explicit review; they must not become an accidental consequence of dependency
feature unification across the workspace.

This accepts the integration direction, **not production approval of 0.19.1,
its current transitive graph, or any unverified SDK behavior**. Before P1/product
adoption, address the [P0 workbench](../planning/feasibility.md): broader encrypted interoperability,
SDK stop/drain behavior, unlock/store protection, unmaintained-dependency
disposition, and platform/transport feasibility. Re-evaluate the exact version
and feature graph when those gates or dependency resolutions change. Never
work around an SDK/server encryption failure by sending plaintext.

### Positive Consequences

* Matrix identities, key/session formats and cryptographic processing remain SDK-owned.
* The native and classic adapters can share one SDK client and approved stores.
* Feature changes and lockfile updates become visible review events.

### Negative Consequences

* SDK internals and dependencies still require source review, tests and maintenance triage.
* The SDK does not supply the application's lock boundary or media sandbox.
* Response processing can outlive caller cancellation; shutdown needs tested coordination.
* The current SDK graph includes an unmaintained direct dependency, requiring disposition.

## Links

* [P0 Matrix evidence](../evidence/matrix-sync.md)
* [P0 storage evidence](../evidence/storage.md)
* [Dependency review](../evidence/dependencies.md)
* [Matrix SDK 0.19.1 features](https://docs.rs/crate/matrix-sdk/0.19.1/source/Cargo.toml)
* [SDK stop/drain source](https://docs.rs/crate/matrix-sdk/0.19.1/source/src/sliding_sync/mod.rs)

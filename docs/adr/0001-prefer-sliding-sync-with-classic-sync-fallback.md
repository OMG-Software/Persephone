# Prefer Sliding Sync with classic sync fallback

* Status: accepted
* Date: 2026-10-19

## Context and Problem Statement

Persephone must support general Matrix homeservers, with Merovingian and
Synapse as the first integration targets. The project owner requires Sliding
Sync as the default for capable servers and regular `/sync` when it is not
available. The selected SDK's Sliding Sync variant and its lifecycle must be
verified rather than inferred from a server's name.

## Decision Drivers

* Use compatible Sliding Sync on the primary integration targets.
* Retain interoperability with homeservers without that capability.
* Preserve device identity, encryption state, and application behavior across modes.
* Classify unsupported features separately from connectivity, authentication, and security failures.

## Considered Options

* Prefer compatible Sliding Sync and fall back to classic `/sync` when unavailable.
* Require Sliding Sync and reject other homeservers.
* Use classic `/sync` for every homeserver.

## Decision Outcome

Chosen option: **prefer compatible Sliding Sync, with classic `/sync` as the
fallback when unavailable**, as requested by the project owner on 2026-10-19.
The two modes sit behind one application-facing synchronization coordinator
and project into the same room/timeline contracts.

Use capability discovery and the pinned SDK's supported variant. Current SDK
documentation describes native MSC4186 support; do not assume older variants
or external proxies are interchangeable. Do not automatically forward session
credentials to a third-party sync proxy.

An SDK discovery helper's empty result is not sufficient proof of absence if
the helper suppresses errors. Preserve successful capability discovery separately
from failure, and verify the behavior of the exact SDK release selected.

Fallback requires a positively classified unsupported capability or endpoint.
Timeouts, rate limits, transient server errors, TLS failures, authentication
errors, and expired Sliding Sync positions must use their own recovery paths.
A mode switch stops and awaits the previous engine and associated encryption
sync tasks before starting the replacement. Keep account/device identity,
keys, and approved persistence; keep Sliding positions and classic `since`
tokens separate. No concurrent independent sync loops may advance the same
account state.

Test Merovingian and Synapse with Sliding Sync, and a classic-only/controlled
unsupported-capability deployment for fallback. Both paths must support the
first-release encrypted messaging, device verification, and history behavior.
The [capability evidence](../evidence/capability-fallback.md) now records initial
native/classic selection, real classic-only Synapse and failure controls in the
Rust integration fixture. Runtime renegotiation, unsupported-endpoint
reclassification, full task drain and the production coordinator remain open.
This decision does not establish production readiness.

### Positive Consequences

* Capable homeservers use the preferred protocol without user configuration.
* Homeservers without compatible Sliding Sync remain usable.
* UI and session authority do not depend on a particular sync transport.

### Negative Consequences

* Two adapters and negotiation/recovery paths need maintenance and verification.
* Switching requires explicit task, checkpoint, and timeline-cache coordination.
* Optional features can differ; unsupported capabilities must be reported rather than fabricated.

## Links

* [Architecture and development plan](../architecture/development-plan.md)
* [Tauri decision](0000-use-tauri-for-the-desktop-client.md)
* [SDK Sliding Sync service](https://matrix-org.github.io/matrix-rust-sdk/matrix_sdk_ui/sync_service/struct.SyncService.html)
* [SDK Sliding Sync version discovery](https://matrix-org.github.io/matrix-rust-sdk/matrix_sdk/sliding_sync/enum.VersionBuilder.html)
* [SDK classic sync APIs](https://matrix-org.github.io/matrix-rust-sdk/matrix_sdk/struct.Client.html)

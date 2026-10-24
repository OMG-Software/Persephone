# Protect keys and contain processes on macOS

* Status: accepted
* Deciders: project owner
* Date: 2026-10-24

## Context and Problem Statement

Persephone must run on macOS. It needs macOS equivalents of the Windows vault,
the core's native windows for secrets, the UI memory cap and the media worker
containment (ADR-0003 to ADR-0006). AppKit requires its windows on the
process's main thread, which the core uses today. The project has no Mac, so
macOS can be built and tested only on CI runners, which have no display.

## Decision Drivers

* Keep the same two layers for the store key as on Windows.
* Secrets are typed into the core's own windows, never the page.
* The worker must not reach the network, files or other processes.
* Do not claim live verification that has not happened.

## Considered Options

* Vault: Keychain; the passphrase alone.
* Worker: self-applied Seatbelt sandbox; a separate XPC service with App
  Sandbox entitlements.
* UI memory: accept no cap; block macOS releases until a cap exists.

## Decision Outcome

Chosen options (owner decisions, 24 October 2026, with the design approved in
the same plan):

* **Vault: Keychain.** A wrapping secret is kept as a generic password in the
  user's login keychain, not synchronized. Access follows that keychain's lock
  state and the item's access list. The data protection keychain, whose
  "this device only" classes would be stricter, needs a signed entitlement
  that development builds lack, so it is not used. The store key is wrapped
  with the secret and sealed with the passphrase.
* **Prompts: AppKit on the main thread.** On macOS the main thread runs the
  AppKit run loop; the core's other work moves to a separate thread, and its
  prompts are shown on the main thread. The file picker is `NSOpenPanel`, with
  the same read rules as on Linux.
* **UI process: kill-on-exit, no memory cap.** macOS has no equivalent of the
  Windows Job. The owner accepts the residual risk: a compromised page can
  use much of the Mac's memory, but it cannot reach the core's memory or keys,
  and the UI ends when the core ends.
* **Worker: Seatbelt.** The worker applies the pure-computation sandbox profile
  to itself before it reads input, with rlimits and an empty environment.
  It is expected to work with the hardened runtime of a notarized app; that
  is not verified yet, because no signed and notarized build has run.
* **Verification: CI only.** macOS releases are marked as not tested live
  until someone runs the live checks on a Mac.

### Positive Consequences

* The store key has the same layers as on Windows and Linux.
* The worker has no file, network or IPC access after it starts.

### Negative Consequences

* The UI's memory is not capped on macOS.
* `sandbox_init` is a deprecated interface. It still works and is used by
  major browsers, but Apple may remove it.
* Nothing on macOS has been seen to work with a real user, display or
  Keychain prompt.

## Links

* [ADR-0010](0010-linux-platform-layer.md) (Linux), [ADR-0012](0012-sign-and-publish-releases-as-drafts.md) (release)
* [ADR-0003](0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md),
  [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md),
  [ADR-0006](0006-require-an-unlock-secret-for-the-local-profile.md)

# Use Tauri for the desktop client

* Status: accepted
* Date: 2026-10-19

## Context and Problem Statement

Persephone needs a Rust desktop Matrix client for Windows, Linux, and macOS,
supporting general Matrix homeservers with Merovingian tested first. Security
for high-risk users takes priority: messages and other server-supplied content
remain untrusted after decryption. We need a UI framework that supports an
explicit boundary between rendering hostile content and privileged session
operations.

## Decision Drivers

* Keep long-lived credentials and cryptographic state outside the rendering UI.
* Constrain UI authority through narrow operations and explicit permissions.
* Support all three desktop operating systems, accessibility, and maintainable UI development.
* Treat content handling, media isolation, dependency updates, and release verification as architectural requirements.

## Considered Options

* Tauri with a Rust core and locally bundled web UI.
* Slint with Rust application logic and a compiled declarative UI.
* iced with a Rust-authored UI.
* GTK4 with gtk-rs bindings.

## Decision Outcome

Chosen option: **Tauri with a Rust core and locally bundled web UI**, because
its process and IPC model gives us a framework boundary at which to constrain
frontend requests while keeping session authority in Rust. A webview can
display untrusted messages as data without allowing the homeserver or other
participants to supply application code. Avoiding web technologies alone
would not establish a safer product.

The project owner accepted this choice on 2026-10-19. This selects Tauri for
implementation; it does not assert that an application or platform sandbox
has passed security verification.

The choice carries these security constraints:

* Bundle reviewed UI assets locally. Do not load remote application scripts,
  untrusted frames, or server-supplied UI definitions; control navigation and
  new-window creation.
* Render messages through text APIs or a deliberately constrained rich-text
  representation. Never insert raw received content as executable markup.
  Apply the same policy to names, room metadata, links, avatars, and attachments.
* Keep long-lived tokens, cryptographic state, Matrix networking, persistence,
  and authorization in Rust. The UI receives the plaintext it displays, so
  renderer compromise can still expose that content.
* Expose narrow application operations over IPC. Validate every request in
  the receiving Rust code, including account/room scope and resource limits;
  do not grant the UI general shell, filesystem, or HTTP authority.
* Explicitly configure restrictive CSP and effective command permissions,
  including custom application commands. Do not assume framework defaults
  establish our security policy or that an allowed call proves human intent.
* Decode untrusted media in an OS-sandboxed helper without session keys or
  network access; validate bounded pixel output before display. Unsupported
  isolation must not silently fall back to unrestricted decoding.
* Verify dependency versions, webview patching, platform containment,
  accessibility, and adversarial content handling before making release
  security claims.

The frontend framework, exact dependency versions, minimum OS versions,
packaging, detailed sandbox mechanisms, and application license remain
separate decisions.

### Positive Consequences

* Rendering and long-lived session authority have an explicit architectural boundary.
* UI requests can be reviewed and tested against a small set of permitted operations.
* The client can use web UI tooling while retaining its Matrix and security logic in Rust.

### Negative Consequences

* The webview, frontend dependencies, and IPC handlers add surfaces to review and patch.
* Webview engines and containment differ across platforms; each supported environment needs verification.
* A compromised UI can read displayed content and misuse permitted operations even without raw keys.
* Media sandboxing and secure configuration require application work; Tauri does not complete those requirements for us.

## Pros and Cons of the Options

### Tauri

* Good, because its process model and permission controls support an explicit frontend/core boundary.
* Bad, because incorrect content rendering, permissive commands, or an unpatched webview can undermine that boundary.

### Slint

* Good, because it offers a compiled native UI path and documented accessibility integration.
* Bad, because our reviewed desktop configuration retains image/SVG parsers and still requires application-designed containment.

### iced

* Good, because its optional features support a codec-free image widget and Rust-authored UI.
* Bad, because the reviewed release has unresolved accessibility questions and dependency concerns that require further work.

### GTK4 with gtk-rs

* Good, because it provides native widgets and documented accessibility integration.
* Bad, because Rust bindings retain a native C/FFI stack, and our media isolation and cross-platform packaging remain unverified.

## Links

* [Security design](../security-design.md)
* [GTK/Tauri security-model comparison](../gtk-tauri-security-model.md)
* [Slint/iced release and dependency research](../slint-iced-security-research.md)
* [Project plan](../project-plan.md)
* [Tauri process model](https://v2.tauri.app/concept/process-model/)
* [Tauri capabilities and command defaults](https://v2.tauri.app/security/capabilities/)
* Format reference: [Merovingian ADR-0001, MADR 2.1.2](https://github.com/OMG-Software/Merovingian/blob/4c56ea521b4f8df699676a811e13660800f98537/docs/adr/0001-use-markdown-architectural-decision-records.md)
* Template reference: [Merovingian ADR template](https://github.com/OMG-Software/Merovingian/blob/4c56ea521b4f8df699676a811e13660800f98537/docs/adr/template.md)

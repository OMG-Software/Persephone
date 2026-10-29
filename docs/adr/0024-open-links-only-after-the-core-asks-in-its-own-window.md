# Open links only after the core asks in its own window

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-28

Technical Story: on 28 October 2026 the owner asked for links in messages
to be shown in full and opened only after a confirmation.

## Context and Problem Statement

Messages are untrusted, and so is the web view that shows them (ADR-0004).
Links in messages were plain text: nothing could be opened. A clickable
link is a common phishing and tracking route, and look-alike letters,
international domain names and bidirectional controls can make an address
look like another. Opening a link also hands an address to another program
on the computer. Who decides that a link opens, and what does the user see
first?

## Decision Drivers

* The web view must not gain a way to open programs or addresses itself.
* The user must see the real destination before anything opens.
* Only ordinary web links; no other schemes reach the operating system.

## Considered Options

* Keep links as plain text.
* Let the page open links with a Tauri opener plugin, after a dialog in
  the page.
* Let the page ask the core, which checks the link, asks in its own window
  and opens it.

## Decision Outcome

Proposed option: "the page asks the core, which checks, asks and opens".

* The page shows each `http` or `https` address in a message (at most 20)
  as a button, never an anchor, and sends its exact text with `open_link`.
  The web view still navigates nowhere and has no opener permission.
* The core accepts only `http` and `https` links with a host, without a user
  name or password, without spaces, controls, or invisible or
  direction-changing characters, and at most 2048 bytes.
* The core asks in its own window ("No" is the default), showing the
  address as the `url` crate serializes it: the host in ASCII, so an
  international or look-alike host shows as `xn--…`, and other non-ASCII
  percent-encoded. The question names the host, warns about international
  letters and unencrypted `http`, and says the browser learns the IP
  address.
* Only after an explicit yes does the core open the link: on Windows with
  `ShellExecuteW` and the `open` verb (`Win32_UI_Shell` was already a
  dependency feature). Other platforms have no implementation and fail
  closed (`unavailable`).
* One question at a time; the reply says whether the link opened.

### Positive Consequences

* A compromised page can only ask: every opened link was seen and approved
  in a window the page does not control.
* No new dependency or permission in the web view.

### Negative Consequences

* Two steps for every link, by design.
* Links in other forms (no scheme, `www.` only) stay text.
* The Windows prompt and opener are untested by automated tests (a fake
  source tests the decision); no packaged or hand check is recorded.

## Links

* Refines [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md).

# Notify through push rules and content-free desktop notifications

* Status: proposed
* Deciders: project owner (decision pending)
* Date: 2026-10-27

Technical Story: on 27 October 2026 the owner asked for a notification level
(mentions only, all activity, or none) and for desktop notifications
(Windows toasts) with an on/off setting. The owner decided that the level is
account-wide through push rules, and that a toast shows the sender and room
only, never the message text.

## Context and Problem Statement

The security design requires hidden notification content. Persephone showed
no notifications. A level kept only in Persephone would disagree with the
user's other clients and push gateways. Showing an OS notification needs a
desktop API in a process that may have one, and a new dependency. Where do
the level and the notifications live, and what may a notification show?

## Decision Drivers

* Use standard Matrix push rules so every client and server agree.
* Never put message text in a notification (security design).
* Keep the web view keyless and give it no new capability (ADR-0004).
* Add the one dependency deliberately: exact pins, no features, no page
  permissions.

## Considered Options

* A level in the account settings, applied only by Persephone.
* The account's push rules on the homeserver.
* Notifications shown by the page through the plugin's JavaScript API.
* Notifications decided in the core and shown by the UI process's Rust code.

## Decision Outcome

Proposed options: "the account's push rules" and "decided in the core, shown
by the UI process's Rust code".

* `notification_level` reads the SDK's `notification_settings()`: the master
  rule on is `none`; otherwise the four default room rules (one-to-one or
  group, encrypted or not) all notifying is `all`, all silent is `mentions`,
  and a mix another client set is `custom` (read only).
  `set_notification_level` sets `none` by turning on `.m.rule.master`, and
  `all` or `mentions` by turning it off and setting the four default rules;
  rooms with their own rules keep them.
* Desktop notifications are a setting in the encrypted profile, off by
  default. The core registers an SDK notification handler, which fires only
  for events the push rules say notify. While the setting is on, the core
  takes the room's name and the sender's name, projected like any name and
  at most 512 bytes each, and skips the account's own events and events
  older than two minutes. It sends them as `Event::Notification`; only the
  latest one waits, so the queue stays bounded.
* The UI process shows it with `tauri-plugin-notification` 2.7.0's Rust API
  (room as title, "New message from" the sender as body) while the main
  window is not focused, and never forwards it to the page. The plugin is
  pinned exactly, has no features enabled, and no capability grants the page
  any `notification:` permission; the IPC tests show every plugin command is
  denied.

### Positive Consequences

* The level agrees with every other client and push gateway of the account.
* Toasts show no message text, and the page gains no notification power.

### Negative Consequences

* New dependency graph: the plugin pulls in `notify-rust` and, on Windows,
  `tauri-winrt-notification`; on Linux `zbus` (pure Rust, D-Bus). 35 crates
  were added to `Cargo.lock`; no existing version changed (one dependency
  line now names `rand_chacha 0.3.1`, as two versions are present).
* The plugin injects a small script that redefines `window.Notification`;
  its calls are denied by the capability.
* Sender and room names still reach the OS notification centre, which may
  keep a history; the setting is off by default for this reason.
* On Windows, a toast from an application that is not installed (no Start
  menu shortcut with Persephone's application ID) may not show.
* The level and toasts were tested against a mock homeserver only.

## Links

* Keeps [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* Follows `docs/architecture/security-design.md` (hidden notification
  content)

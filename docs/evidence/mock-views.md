# Bounded room and timeline views (synthetic data)

Checked 20 October 2026 on Windows 11 Pro x64 (build 26200), debug builds,
WebView2. P1 "mock bounded views": the core serves a room list and paged
timelines from synthetic data through the [core/UI split](ipc-transport.md#after-the-coreui-split).
**Result: hostile names and bodies render as inert, direction-isolated text;
pages are bounded by count and bytes and walk history completely.** No Matrix
data yet; the views use synthetic rooms only.

## Design

* **Commands:** `room_list` and `timeline_page(room_id, cursor)`, added to the
  allowlist and capability alongside `app_status`. The UI length-checks
  arguments before forwarding (room ID 255 bytes, cursor 64) and checks every
  reply against the generated contracts, which reject unknown fields. The page
  sees only `invalid_argument`, `not_found` or `core_unavailable`.
* **Protocol:** `Call::TimelinePage` requires `cursor` to be present (it may be
  null); serde would otherwise default a missing `Option` to `None`.
* **Core projection** ([`views.rs`](../../crates/core/src/views.rs)):
  * Display names: bidi formatting, control and BOM characters become U+FFFD,
    whitespace runs collapse, and names are cut to 128 characters.
  * Bodies keep their text, including bidi characters, which the UI isolates;
    controls other than tab and newline become U+FFFD; bodies are cut to 16 KiB.
  * Pages hold at most 100 items **and** at most 112 KiB serialized, so every
    reply fits one 128 KiB control frame. Cursors are opaque, strictly parsed
    (`t1.<n>`, no leading zeros or non-ASCII digits).
* **Frontend:** room list and timeline components use native buttons, a
  labelled `nav`, `aria-current` for the open room and a live error `alert`.
  Names render inside `<bdi>` and bodies with `dir="auto"`; nothing uses raw
  HTML.

## Tests

`python tools/dev.py check --offline` passed 175 tests (152 Rust, 8 frontend, 2
server-registration and 13 controller tests). New:

* Core (7): name and body projection, complete back-paging of 250 messages with
  no gaps or overlap, a byte-bounded page, specific errors for bad rooms and
  cursors, room list projection; and a request-loop test showing every page of
  large messages fits a control frame and bad arguments get error replies while
  the session continues.
* Protocol (in the existing refusal test): unknown `room_list` field, missing
  `cursor`, extra `timeline_page` field.
* UI (2): replies relayed and contract-checked (a reply carrying an
  `accessToken` field is refused); oversized arguments refused without reaching
  the core.
* Frontend (3): hostile room names escaped and isolated, unread counts capped at
  `999+`, single `aria-current`; hostile bodies escaped with `dir="auto"`; end of
  history and error states.

## Real application walkthrough

The core was launched with WebView2's debugging port and driven over DevTools:

| Check | Result |
| --- | --- |
| Room names | Markup shown as text; `Admin‮gnp.exe` shown as `Admin�gnp.exe`; controls replaced; 500-character name cut to 128 + `…`; unnamed room shows its ID |
| Injected elements | One `<img>` on the page (the logo); none in the timeline |
| Unread badges | `3`, `999+` (for `u32::MAX`), `1` |
| Paging the 250-message room | 100 → 200 → 250 items, then "Start of history"; first "Planning note 0", last "Planning note 249" |
| 60 × 8 KiB messages | First page has 13 items (byte budget, not the 100-item limit) |
| Hostile bodies | No script or img elements; bidi text kept and isolated; controls replaced; the 32 KiB body cut to 16,385 characters |
| Keyboard | Tab reached the first room button in two runs; Enter activation could not be driven reliably through DevTools |

## Investigation note

During this check the DevTools endpoint intermittently stopped answering. I
first read that as the web view process dying, and attributed it to the UI
Job's `DIE_ON_UNHANDLED_EXCEPTION` flag. Ten repeated trials that counted the
UI's WebView2 processes instead showed no process exits in either Job
configuration: all six survived and the room rendered every time the DevTools
connection opened. The flag stays as committed. The failures belong to the
debugging channel, not the application.

## Limits

Synthetic data only. Keyboard activation, screen readers, scaling and IME are
not verified here (P6). Release builds and Linux/macOS are not covered. Core
events (new messages arriving) are not implemented; views are fetched on
demand.

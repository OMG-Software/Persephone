# Room tools

Updated 1 November 2026. Owner requests on this date added drafts, search,
ignore/report, room organisation, date navigation, shared pins, attachment
browsing, search filters and connection health. Their requirements are
approved; [ADR-0032](adr/0032-keep-bounded-room-drafts-in-the-encrypted-profile.md)
to [ADR-0036](adr/0036-bound-room-navigation-and-inspection-in-the-core.md)
remain proposed. The features are implemented, and automated checks are
recorded in [room-navigation and media health evidence](evidence/room-navigation-media-health.md).
Native UI and live-server acceptance remain open. No acceptance or security
gate is closed.

## Drafts

Each room can keep its text and reply reference in the encrypted local profile.
Changing rooms restores that room's draft. An empty body with no reply reference
clears it. Whitespace-only text is kept. Drafts do not queue or send messages.
Image and file selections are not saved as text drafts.

Use **Saved drafts** to view or discard drafts for inactive rooms, including
rooms you have left. The manager cannot discard the current editor's draft;
use **Discard draft** in that room. If the record is full, the manager can
still open while current text is unsaved. Remove another draft, close the
manager, then retry the current save. A failed restore blocks composing until
the restore succeeds or you explicitly discard the unread draft.

The UI saves after a short typing pause (currently 350 ms), serializes
saves and checks the active room and session generation before applying a load.
Save errors stay visible. Automated race checks pass; native UI checks remain
open in the verification record. A visible unsaved draft is not a
durable-save guarantee. A crash before a save completes can lose text.

The frontend requests lock and confirmed sign-out without waiting for its
draft-save promise or for a message to enter the send queue. Lock closes core
admission and announces the UI clear before it waits for admitted work.
Sign-out first attempts to record a local marker so an interrupted sign-out
does not restore the account on the next start. It then attempts a homeserver
logout. While that request is pending, sign-out has not yet closed admission,
cleared the UI or protected the session. After the request, it attempts to
record the revocation result and remove the stored key, then calls lock.
Lock completion still waits for admitted work, including draft writes that
hold a session lease. A timeout can leave the core in `locking`; it does not
authorize store handoff. Unsaved text can be lost. Normal room and account
switches wait for draft saving and show failures.

The core accepts at most 16 KiB of UTF-8 body text per draft, 100 rooms and
100 KiB of aggregate draft bodies. The serialized record must fit 112 KiB, so
JSON escaping and metadata can reduce the usable text capacity. Reply event
IDs and room IDs must be valid Matrix IDs of at most 255 bytes. Controls other
than newline and tab are refused. A corrupt record produces an error; it is
not reset to empty on the next save.

## Local message search

Open message search and enter a query. Choose the current room or all joined
rooms. This differs from quick room switching and from homeserver user-directory
search.

Search reads existing encrypted SDK cache chunks. It sends no HTTP request,
downloads no history and requests no keys. It creates no separate index. It
matches only text already decrypted in that cache. Results exclude ignored
senders. Cached edits and redactions affect the visible text, but missing cache
events can leave an incomplete view of later changes.

Every search has partial coverage. A missing match does not prove that a
message does not exist. Limits are 100 rooms, 2,000 scanned events, 50 results
and 512 UTF-8 bytes per snippet. Gaps, undecrypted events and byte/work limits
can reduce coverage further. Results are bounded to one 112 KiB reply. Selecting
a result can fetch up to five normal history pages to locate it, keeping at
most 500 timeline items. It stops before an older page would exceed this limit
and keeps the cursor needed to read that page later. It does not remove recent
items to fit an older page. If the starting context exceeds 500 items, it loads
a fresh newest page within the same five-request limit. This navigation sends
room history requests, but does not send the search term. If the message cannot
be located within these limits, the UI says so.

The cache grows from SDK sync updates. History fetched directly for the
timeline is not guaranteed to be in this cache. Reads of separate chunks are
not an atomic snapshot; concurrent sync can change cached history. Available
valid edits use the greatest server timestamp, then the greatest event ID for
a tie. A room with an oversized cached event is omitted rather than returning
text that such an event might have replaced or deleted.

## Ignore users and report messages

Ignore hides a user's message content in the timeline and local search. Manage
the list to undo an ignore. The list belongs to your Matrix account and is
stored on your homeserver. The app refuses a new entry once the list has 100
users. An existing larger list can still be reduced; filtering uses its full
set even if the UI list is truncated. Room state events can remain visible.
Oversized raw ignore account data (above 256 KiB) is refused, not treated as an
empty list.
Ignoring does not remove the user, revoke room access or withdraw shared keys.
Room mute is a separate notification control.

The ignored-user dialog cannot be dismissed while an ignore change is pending.
This keeps the dialog active until it can refresh the visible timeline after
success. Account and session changes still invalidate pending UI results.

Report a message only after you intend to disclose its reference and your
reason to your own homeserver. Enter a nonempty reason of at most 1,000 UTF-8
bytes and confirm the report. The request contains the room ID, event ID and
reason. It attaches no message body. The operator can read the reason even in
an encrypted room. Do not copy private message text into it unintentionally.
Server support, policy and response determine any moderation action.

## Favourites and sections

Mark a room as a favourite or give it a named section to organise the room list.
These choices belong to the encrypted local profile. They are not uploaded as
Matrix `m.tag` data and do not follow the account to another device.

Unsaved grouping edits and save errors stay visible when another room's
preferences change. Changing rooms or changing this room's saved values restores
the controls from those saved values.

The core trims section names and requires 1 to 64 UTF-8 bytes with no controls.
At most 100 room preferences and 16 distinct sections are stored. Clear both
the favourite flag and section to remove an entry and free its room slot.
The room organisation controls also list saved preferences for rooms no longer
in the displayed room list, so their entries can be removed. Local draft and
preference operations validate the room ID but do not require current joined
membership; they grant no homeserver operation or ability to rejoin a room.

## Profile and lifecycle limits

The core owns the stores, keys, cache search and Matrix requests. The web view
shows inert text and has no browser-storage copy of these records. Signing out
deletes local profile data under the existing sign-out rules; it does not erase
server ignore data or reports.

The SDK event-cache subscription retains sync events for local search. It adds
background work and does not prove full shutdown. Lock after a client has run
still requires restart (`restart_required`). See the
[SDK lifecycle contract](architecture/sdk-lifecycle.md).

## Date navigation

Jump to date sends the room ID and requested timestamp to the homeserver. It
sends no search term. The core requests the first event at or after the time
(`since`). It tries the last event before the time (`until`) only when the
first request returns HTTP 404. The result uses bounded event context and
marks the newer end with a gap. Its page cursor resumes before the oldest
retained item. It does not scan history to simulate a date lookup. The server
still learns the room and time.

The client selects a visible event from that bounded context window. Ignored
messages and relation-only events cannot be the focus. A window with no
eligible visible event returns no result; it does not trigger a history scan.

Focused date and pinned-message context does not send read markers. If a live
room update invalidates focused context, the client cancels an in-flight
context request and clears the displayed focused timeline. It removes received
reply and saved-reply quote text but keeps your draft and edit body. The UI
shows **Room changed. Return to latest to refresh.** Select **Return to latest**
to load the current room view. The client does not silently replace focused
context with a new timeline.

## Shared pinned messages

Pins use standard `m.room.pinned_events` room state. The state contains event
IDs only; it stores no plaintext snippets. Matrix room state is unencrypted
metadata, including in encrypted rooms. The core reads current state and
checks the user's current power level before a write. The panel shows at most
100 IDs and refuses additions at that limit. To clean up larger lists created
by other clients, removal preserves all other IDs within a 512-ID state limit
and a 112 KiB replacement-body limit. Larger changes fail without a write.
State replacement has no compare-and-swap guarantee, so
concurrent writers can overwrite one another. This is shared room metadata,
not a private profile setting.

Pin loading has a total time budget of 10 seconds. The core checks session
admission before each content or edit request. It checks at most 20 replacement
events per pin and applies the latest validated edit from the original sender.
Equal timestamps are ordered by event ID. Each replacement response, including
authentication errors and one SDK refresh retry, is limited to 256 KiB. The
core checks streamed chunks before adding them to its buffer and checks the
event count before SDK decryption. This cap
applies to pin replacement reads; it does not cap every SDK response.
If an edit read is unavailable, incomplete or undecryptable, the panel keeps
the pin reference and hides its original preview. The panel reports unavailable
previews or loading limits. This does not establish a complete SDK shutdown.

## Attachment browser

The browser scans one history chunk of at most 50 raw events per page, then
filters the projected items by attachment kind, exact sender and date. It
uses opaque cursors scoped to the room. A page can contain no attachments and
still have a next cursor. It returns metadata only and marks coverage partial.
It does not fetch media. An explicit existing media action is required for a
download or display. The UI retains at most 500 items. Gaps, undecryptable
events, redactions, byte limits and the bounded page history can hide matches.

## Search filters

Local cache search accepts an exact sender, inclusive start time and exclusive
end time with the query. Dates use the original message timestamp, even when
the displayed text comes from an edit or redaction reconstruction. Unknown
timestamps do not match a date filter. Filters create no index and do not
cause cache growth or an HTTP request. Search remains partial because of
cache gaps, undecryptable events and work and byte limits.

## Connection health

The panel shows a manual snapshot of existing generation-fenced sync metrics.
It adds no probe or background task. The displayed timestamps use the local
computer clock. They do not prove current connectivity. Locking resets the
snapshot and generation fencing rejects stale sync updates.

## Verification state

The implementation is in place. Automated checks are recorded in the
[original record](evidence/room-navigation-media-health.md) and
[review-fix record](evidence/room-navigation-review-fixes.md), with later
[monitor fixes](evidence/room-navigation-monitor-fixes.md). Native UI and
live-server acceptance remain open. Existing SDK drain and restart requirements
remain unresolved. Passing feature checks do not establish a
complete SDK shutdown or production readiness.

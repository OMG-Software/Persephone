# Room tools

Updated 1 November 2026. The owner requested these features on this date.
The current source implements them; their technical ADRs remain proposed and
[automated checks pass](evidence/room-tools.md). Native UI and live-server
acceptance remain open. No acceptance gate is closed.

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
Sign-out first attempts a homeserver logout. While that request is pending,
sign-out has not yet closed admission, cleared the UI or protected the session.
It then attempts to record local sign-out and remove the stored key before
calling lock.
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

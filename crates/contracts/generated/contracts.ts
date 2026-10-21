export type StoreState = "not_configured" | "protected" | "locked" | "locking" | "lock_failed" | "key_missing" | "key_unreadable" | "key_rejected" | "profile_in_use" | "store_unavailable" | "unsupported";

export type AppStatus = { 
/**
 * The application version, from the desktop crate's manifest.
 */
version: string, 
/**
 * The current development milestone, e.g. "P1".
 */
milestone: string, store: StoreState, };

export type RoomSummary = { roomId: string, displayName: string | null, 
/**
 * u32 caps this count below JavaScript's exact-integer limit.
 */
unreadCount: number, };

export type RoomList = { rooms: Array<RoomSummary>, truncated: boolean, };

export type TimelineItem = { "kind": "message", event_id: string, sender: string, body: string, } | { "kind": "gap", next_cursor: string | null, };

export type TimelinePage = { roomId: string, items: Array<TimelineItem>, nextCursor: string | null, };

export const roomSummaryFixture = {
  "roomId": "!example:server.invalid",
  "displayName": null,
  "unreadCount": 4294967295
} as const satisfies RoomSummary;

export const timelinePageFixture = {
  "roomId": "!example:server.invalid",
  "items": [
    {
      "kind": "message",
      "event_id": "$event",
      "sender": "@alice:server.invalid",
      "body": "hostile <img src=x onerror=alert(1)>"
    },
    {
      "kind": "gap",
      "next_cursor": null
    }
  ],
  "nextCursor": "cursor-2"
} as const satisfies TimelinePage;

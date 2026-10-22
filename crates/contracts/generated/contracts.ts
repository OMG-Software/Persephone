export type StoreState = "not_configured" | "protected" | "locked" | "locking" | "lock_failed" | "restart_required" | "key_missing" | "key_unreadable" | "key_rejected" | "profile_in_use" | "store_unavailable" | "unsupported";

export type AccountState = "signed_out" | "signing_in" | "signed_in" | "sign_in_failed";

export type SignInError = "cancelled" | "invalid_credentials" | "unreachable" | "homeserver_refused" | "unsupported" | "sync_failed" | "session_not_revoked" | "failed";

export type AccountStatus = { state: AccountState, 
/**
 * The signed-in Matrix user ID, at most 255 bytes.
 */
userId: string | null, error: SignInError | null, };

export type AppStatus = { 
/**
 * The application version, from the desktop crate's manifest.
 */
version: string, 
/**
 * The current development milestone, e.g. "P1".
 */
milestone: string, store: StoreState, account: AccountStatus, };

export type IdentityState = "unknown" | "none" | "unverified" | "verified";

export type RecoveryState = "unknown" | "disabled" | "incomplete" | "enabled";

export type SecurityOperation = "idle" | "waiting_for_password" | "waiting_for_recovery_key" | "working";

export type SecurityError = "cancelled" | "unsupported" | "invalid_password" | "identity_exists" | "wrong_recovery_key" | "backup_exists_on_server" | "recovery_key_not_shown" | "unreachable" | "failed";

export type SecurityStatus = { identity: IdentityState, recovery: RecoveryState, operation: SecurityOperation, error: SecurityError | null, };

export type RoomSummary = { roomId: string, displayName: string | null, 
/**
 * u32 caps this count below JavaScript's exact-integer limit.
 */
unreadCount: number, };

export type RoomList = { rooms: Array<RoomSummary>, truncated: boolean, };

export type NoticeKind = "undecryptable" | "withheld" | "untrusted_sender" | "unsupported";

export type TimelineItem = { "kind": "message", event_id: string, sender: string, body: string, } | { "kind": "gap", next_cursor: string | null, } | { "kind": "notice", event_id: string, sender: string, notice: NoticeKind, };

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
      "kind": "notice",
      "event_id": "$encrypted",
      "sender": "@bob:server.invalid",
      "notice": "undecryptable"
    },
    {
      "kind": "gap",
      "next_cursor": null
    }
  ],
  "nextCursor": "cursor-2"
} as const satisfies TimelinePage;

export type StoreState = "not_configured" | "protected" | "locked" | "locking" | "unlocking" | "lock_failed" | "restart_required" | "key_missing" | "key_unreadable" | "secret_rejected" | "key_rejected" | "profile_in_use" | "store_unavailable" | "unsupported";

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

export type SecurityOperation = "idle" | "waiting_for_password" | "waiting_for_recovery_key" | "waiting_for_confirmation" | "working";

export type SecurityError = "cancelled" | "unsupported" | "invalid_password" | "identity_exists" | "wrong_recovery_key" | "backup_exists_on_server" | "recovery_key_not_shown" | "unreachable" | "contact_has_no_identity" | "cannot_sign_contacts" | "not_direct_room" | "failed";

export type VerificationPhase = "incoming" | "waiting" | "compare" | "confirmed" | "done" | "cancelled";

export type SasEmoji = { symbol: string, description: string, };

export type VerificationView = { flowId: string, phase: VerificationPhase, 
/**
 * Seven emoji once they can be compared, otherwise empty.
 */
emoji: Array<SasEmoji>, 
/**
 * The other device's name as it reported it (untrusted, projected).
 */
otherDevice: string | null, 
/**
 * The contact's user ID when this verifies another person (untrusted,
 * projected); none when it verifies this device.
 */
contact: string | null, };

export type SecurityStatus = { identity: IdentityState, recovery: RecoveryState, operation: SecurityOperation, error: SecurityError | null, verification: VerificationView | null, 
/**
 * This device can set up recovery alone (it holds the private keys of a
 * published identity whose recovery is not set up).
 */
canSetUpRecovery: boolean, };

export type ContactTrust = "unknown" | "no_identity" | "unverified" | "verified" | "changed" | "verified_changed";

export type ContactStatus = { roomId: string, 
/**
 * Untrusted, projected like a name.
 */
userId: string, trust: ContactTrust, };

export type IdentityChange = { 
/**
 * A valid Matrix user ID (printable ASCII, at most 255 bytes), so the
 * page can name it back to the core; untrusted text all the same.
 */
userId: string, 
/**
 * This account had verified the old identity.
 */
wasVerified: boolean, };

export type RoomIdentityChanges = { roomId: string, changes: Array<IdentityChange>, 
/**
 * More members changed than are listed.
 */
truncated: boolean, };

export type RoomEncryption = "encrypted" | "not_encrypted" | "unknown";

export type RoomSummary = { roomId: string, displayName: string | null, 
/**
 * u32 caps this count below JavaScript's exact-integer limit.
 */
unreadCount: number, encryption: RoomEncryption, 
/**
 * In a direct-message room with one other person, that person's user ID
 * (untrusted, projected like a name). Otherwise none.
 */
contact: string | null, };

export type PendingState = "queued" | "sending" | "sent" | "failed" | "unknown";

export type SendFailure = "confirmation_required" | "device_not_verified" | "identity_changed" | "not_joined" | "rejected" | "failed";

export type PendingMessage = { localId: number, roomId: string, body: string, state: PendingState, eventId: string | null, failure: SendFailure | null, 
/**
 * When it was queued on this device, by this computer's clock.
 */
queuedAt: number, };

export type RoomList = { rooms: Array<RoomSummary>, truncated: boolean, };

export type NoticeKind = "undecryptable" | "withheld" | "untrusted_sender" | "unsupported";

export type TimelineItem = { "kind": "message", event_id: string, sender: string, body: string, 
/**
 * When the sender's homeserver says it received the event
 * (`origin_server_ts`). That server sets it and nothing checks it,
 * so it can be wrong. `None` when the event has no usable time.
 */
origin_server_ts: number | null, } | { "kind": "gap", next_cursor: string | null, } | { "kind": "notice", event_id: string, sender: string, notice: NoticeKind, 
/**
 * As for messages.
 */
origin_server_ts: number | null, };

export type TimelinePage = { roomId: string, items: Array<TimelineItem>, nextCursor: string | null, };

export const roomSummaryFixture = {
  "roomId": "!example:server.invalid",
  "displayName": null,
  "unreadCount": 4294967295,
  "encryption": "encrypted",
  "contact": "@alice:server.invalid"
} as const satisfies RoomSummary;

export const timelinePageFixture = {
  "roomId": "!example:server.invalid",
  "items": [
    {
      "kind": "message",
      "event_id": "$event",
      "sender": "@alice:server.invalid",
      "body": "hostile <img src=x onerror=alert(1)>",
      "origin_server_ts": 1767258000000
    },
    {
      "kind": "notice",
      "event_id": "$encrypted",
      "sender": "@bob:server.invalid",
      "notice": "undecryptable",
      "origin_server_ts": null
    },
    {
      "kind": "gap",
      "next_cursor": null
    }
  ],
  "nextCursor": "cursor-2"
} as const satisfies TimelinePage;

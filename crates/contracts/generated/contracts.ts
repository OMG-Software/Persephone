export type StoreState = "not_configured" | "protected" | "locked" | "locking" | "unlocking" | "lock_failed" | "restart_required" | "key_missing" | "key_unreadable" | "secret_rejected" | "key_rejected" | "profile_in_use" | "store_unavailable" | "unsupported";

export type AccountState = "signed_out" | "signing_in" | "signed_in" | "sign_in_failed";

export type SignInError = "cancelled" | "invalid_credentials" | "unreachable" | "homeserver_refused" | "unsupported" | "sync_failed" | "session_not_revoked" | "username_taken" | "invalid_username" | "username_reserved" | "registration_disabled" | "token_rejected" | "weak_password" | "unsupported_registration" | "rate_limited" | "failed";

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
 * The commit the core was built from: its first 12 hex digits, or
 * "unknown". It does not show uncommitted changes.
 */
revision: string, 
/**
 * The current development milestone, e.g. "P1".
 */
milestone: string, store: StoreState, account: AccountStatus, };

export type IdentityState = "unknown" | "none" | "unverified" | "verified";

export type RecoveryState = "unknown" | "disabled" | "incomplete" | "enabled";

export type SecurityOperation = "idle" | "waiting_for_password" | "waiting_for_recovery_key" | "waiting_for_confirmation" | "working";

export type SecurityError = "cancelled" | "unsupported" | "invalid_password" | "identity_exists" | "wrong_recovery_key" | "backup_exists_on_server" | "recovery_key_not_shown" | "unreachable" | "contact_has_no_identity" | "cannot_sign_contacts" | "not_direct_room" | "cannot_sign_devices" | "device_not_found" | "recovery_key_not_saved" | "failed";

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

export type PendingKind = "text" | "image" | "edit";

export type PendingMessage = { localId: number, roomId: string, kind: PendingKind, body: string, state: PendingState, eventId: string | null, failure: SendFailure | null, 
/**
 * When it was queued on this device, by this computer's clock.
 */
queuedAt: number, };

export type RoomList = { rooms: Array<RoomSummary>, truncated: boolean, };

export type InviteSummary = { roomId: string, 
/**
 * The room's name, from the invitation's room state, if it has one.
 */
roomName: string | null, 
/**
 * Whether the inviter marked it as a direct-message invitation.
 */
direct: boolean, 
/**
 * The user ID of whoever sent the invitation, when the invitation says.
 */
inviter: string | null, 
/**
 * The inviter's display name in that room, if they set one.
 */
inviterName: string | null, };

export type InviteList = { invites: Array<InviteSummary>, truncated: boolean, };

export type ConversationStart = { "kind": "existing", roomId: string, } | { "kind": "created", roomId: string, } | { "kind": "no_identity", } | { "kind": "refused", };

export type RoomCreation = { "kind": "created", roomId: string, } | { "kind": "no_identity", userId: string, } | { "kind": "refused", };

export type RoomInvite = { "kind": "invited", } | { "kind": "no_identity", } | { "kind": "refused", };

export type JoinRule = "invite" | "knock" | "public" | "restricted" | "knock_restricted" | "private" | "other";

export type HistoryVisibility = "invited" | "joined" | "shared" | "world_readable" | "other";

export type RoomNotifications = "default" | "all" | "mentions" | "mute";

export type LevelKey = "users_default" | "events_default" | "state_default" | "invite" | "kick" | "ban" | "redact" | "notify_room" | "name" | "topic" | "avatar" | "canonical_alias" | "join_rules" | "history_visibility" | "guest_access" | "encryption" | "power_levels" | "server_acl" | "tombstone" | "pinned_events";

export type RoomLevel = { key: LevelKey, level: number, };

export type RoomPermissions = { name: boolean, topic: boolean, 
/**
 * The main and published addresses.
 */
canonicalAlias: boolean, joinRules: boolean, historyVisibility: boolean, guestAccess: boolean, encryption: boolean, powerLevels: boolean, invite: boolean, kick: boolean, ban: boolean, };

export type RoomSettings = { roomId: string, name: string | null, 
/**
 * Line breaks kept, like a message body.
 */
topic: string | null, 
/**
 * The main address (`m.room.canonical_alias`).
 */
canonicalAlias: string | null, 
/**
 * The other published addresses.
 */
altAliases: Array<string>, 
/**
 * Addresses on this account's homeserver that lead to the room; none
 * when the homeserver did not say.
 */
localAliases: Array<string> | null, joinRule: JoinRule, historyVisibility: HistoryVisibility, 
/**
 * Guests (accounts without a password) may join.
 */
guestAccess: boolean, encryption: RoomEncryption, 
/**
 * Listed in this homeserver's public room directory; none when the
 * homeserver did not say.
 */
published: boolean | null, 
/**
 * Untrusted, projected like a name.
 */
roomVersion: string | null, 
/**
 * Marked as a direct-message room.
 */
direct: boolean, notifications: RoomNotifications, 
/**
 * Every [`LevelKey`], in [`LevelKey::ALL`] order, with the level now in
 * effect.
 */
levels: Array<RoomLevel>, 
/**
 * This account's level; none for a creator of a room whose version
 * gives creators every permission.
 */
ownLevel: number | null, may: RoomPermissions, };

export type Membership = "join" | "invite" | "knock" | "ban";

export type RoomMember = { 
/**
 * The member's ID unchanged when it is valid, visible ASCII; otherwise
 * projected, and every `can` field is false.
 */
userId: string, displayName: string | null, membership: Membership, 
/**
 * None for a creator with every permission.
 */
level: number | null, 
/**
 * This account.
 */
own: boolean, 
/**
 * Hints for the page, by the room's power levels.
 */
canKick: boolean, canBan: boolean, canUnban: boolean, canChangeLevel: boolean, };

export type RoomMembers = { roomId: string, members: Array<RoomMember>, 
/**
 * More members matched than are listed.
 */
truncated: boolean, };

export type RoomChange = { "kind": "name", name: string, } | { "kind": "topic", topic: string, } | { "kind": "join_rule", rule: JoinRule, } | { "kind": "history_visibility", visibility: HistoryVisibility, } | { "kind": "guest_access", allowed: boolean, } | { "kind": "encryption", } | { "kind": "published", published: boolean, } | { "kind": "add_alias", alias: string, } | { "kind": "remove_alias", alias: string, } | { "kind": "canonical_alias", alias: string | null, altAliases: Array<string>, } | { "kind": "level", key: LevelKey, level: number, } | { "kind": "member_level", userId: string, level: number, } | { "kind": "kick", userId: string, } | { "kind": "ban", userId: string, } | { "kind": "unban", userId: string, } | { "kind": "notifications", mode: RoomNotifications, } | { "kind": "leave", };

export type DisplayName = { displayName: string | null, };

export type DeviceSummary = { deviceId: string, displayName: string | null, 
/**
 * When the homeserver last saw the device, by the homeserver's clock.
 */
lastSeen: number | null, 
/**
 * This device.
 */
current: boolean, 
/**
 * Whether this account's cross-signing identity signed the device, as
 * this device's store knows it.
 */
verified: boolean, };

export type DeviceList = { devices: Array<DeviceSummary>, truncated: boolean, };

export type ProxySetting = { proxy: string | null, };

export type HomeserverFound = { homeserver: string, fromWellKnown: boolean, };

export type NewSession = { deviceId: string, displayName: string | null, };

export type NewSessions = { sessions: Array<NewSession>, truncated: boolean, };

export type AccountSettings = { 
/**
 * Enter sends a message; otherwise it starts a new line. On by default.
 */
enterSends: boolean, 
/**
 * The size of message text, in percent of the normal size, from
 * [`TEXT_PERCENT_MIN`] to [`CHAT_TEXT_PERCENT_MAX`]. 100 by default.
 */
chatTextPercent: number, 
/**
 * The size of the rest of the interface, in percent of the normal size,
 * from [`TEXT_PERCENT_MIN`] to [`INTERFACE_TEXT_PERCENT_MAX`]. 100 by
 * default.
 */
interfaceTextPercent: number, 
/**
 * Show a desktop notification (a toast) for a message that notifies
 * under the account's push rules, with the sender and room only, never
 * the text. Off by default.
 */
desktopNotifications: boolean, 
/**
 * Tell the people in a room while this account is typing there
 * (typing notices). Off by default (ADR-0023).
 */
sendTyping: boolean, 
/**
 * Show the people in a room how far this account has read (public
 * read receipts). Off by default; the fully-read marker and a private
 * receipt, which only this account sees, are sent either way
 * (ADR-0023).
 */
sendReadReceipts: boolean, 
/**
 * Lock the profile after this many minutes without keyboard or mouse
 * input on this computer, from 1 to [`IDLE_LOCK_MINUTES_MAX`]; 0 never
 * does. Off (0) by default (ADR-0025).
 */
idleLockMinutes: number, 
/**
 * Desktop notifications name the sender and room; otherwise they say
 * only that a message arrived, and no name leaves the core. On by
 * default (ADR-0022).
 */
notificationNames: boolean, };

export type RoomActivity = { typing: Array<string>, readBy: Array<string>, truncated: boolean, };

export type RoomServer = { name: string, members: number, own: boolean, };

export type RoomServers = { servers: Array<RoomServer>, truncated: boolean, };

export type RoomReader = { userId: string, trust: ContactTrust, receiving: number, refused: number, 
/**
 * This account: its other devices read the room too.
 */
own: boolean, };

export type RoomReaders = { readers: Array<RoomReader>, truncated: boolean, };

export type NotificationLevel = "all" | "mentions" | "none" | "custom";

export type NotificationSettings = { level: NotificationLevel, };

export type ThreePidMedium = "email" | "msisdn";

export type ThreePid = { medium: ThreePidMedium, address: string, };

export type ThreePidList = { threePids: Array<ThreePid>, truncated: boolean, 
/**
 * The homeserver lets this account add and remove them (its
 * `m.3pid_changes` capability, on unless it says otherwise).
 */
canChange: boolean, 
/**
 * An address is waiting to be confirmed: `add_three_pid` adds it.
 */
pending: ThreePid | null, };

export type AccountChange = "done" | "token_sent" | "cancelled" | "unsupported" | "invalid_password" | "weak_password" | "not_confirmed" | "in_use" | "denied" | "medium_not_supported" | "changes_disabled" | "unsupported_auth" | "nothing_pending" | "rate_limited" | "unreachable" | "failed";

export type ProfileSlot = { slot: number, 
/**
 * The profile open now.
 */
active: boolean, };

export type ProfileList = { profiles: Array<ProfileSlot>, 
/**
 * Whether a free slot is left for `add_profile`.
 */
canAdd: boolean, };

export type UserMatch = { userId: string, 
/**
 * The display name the directory gave, projected like any name.
 */
displayName: string | null, };

export type UserSearch = { users: Array<UserMatch>, truncated: boolean, };

export type NoticeKind = "undecryptable" | "withheld" | "untrusted_sender" | "unsupported" | "deleted";

export type MembershipChange = "joined" | "left" | "invited" | "invite_rejected" | "invite_withdrawn" | "kicked" | "banned" | "unbanned" | "knocked" | "knock_withdrawn" | "knock_denied";

export type StateChange = { "type": "member", target: string, membership: MembershipChange, } | { "type": "display_name", target: string, name: string | null, } | { "type": "name", name: string | null, } | { "type": "topic", topic: string | null, } | { "type": "avatar", } | { "type": "encryption", } | { "type": "join_rule", rule: JoinRule, } | { "type": "history_visibility", visibility: HistoryVisibility, } | { "type": "power_levels", } | { "type": "addresses", } | { "type": "created", } | { "type": "server_acl", } | { "type": "upgraded", };

export type TimelineItem = { "kind": "message", event_id: string, sender: string, 
/**
 * The text, without a reply's quoted fallback.
 */
body: string, 
/**
 * When the sender's homeserver says it received the event
 * (`origin_server_ts`). That server sets it and nothing checks it,
 * so it can be wrong. `None` when the event has no usable time.
 */
origin_server_ts: number | null, 
/**
 * The event this message replies to, as the sender claims. The
 * page shows it only from history it has loaded itself; the
 * sender's quoted copy of it is never shown.
 */
reply_to: string | null, } | { "kind": "gap", next_cursor: string | null, } | { "kind": "notice", event_id: string, sender: string, notice: NoticeKind, 
/**
 * As for messages.
 */
origin_server_ts: number | null, } | { "kind": "image", event_id: string, sender: string, 
/**
 * As for messages.
 */
origin_server_ts: number | null, name: string, 
/**
 * Declared file size in bytes.
 */
size: number | null, 
/**
 * Declared width and height, both set or both none.
 */
width: number | null, height: number | null, 
/**
 * The file is end-to-end encrypted.
 */
encrypted: boolean, } | { "kind": "edit", event_id: string, sender: string, 
/**
 * As for messages.
 */
origin_server_ts: number | null, replaces: string, body: string, } | { "kind": "reaction", event_id: string, sender: string, 
/**
 * As for messages.
 */
origin_server_ts: number | null, reacts_to: string, key: string, } | { "kind": "redaction", event_id: string, sender: string, 
/**
 * As for messages.
 */
origin_server_ts: number | null, redacts: string, } | { "kind": "state", event_id: string, sender: string, 
/**
 * As for messages.
 */
origin_server_ts: number | null, change: StateChange, };

export type TimelinePage = { roomId: string, items: Array<TimelineItem>, nextCursor: string | null, };

export type ImageMime = "image/png" | "image/jpeg";

export type ImageView = { handle: string, 
/**
 * The shown size, at most 2048 x 2048, after the EXIF orientation.
 */
width: number, height: number, bands: number, bandRows: number, };

export type ImagePick = { "kind": "draft", preview: ImageView, 
/**
 * The stripped copy's size in bytes.
 */
size: number, mime: ImageMime, } | { "kind": "cancelled", };

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
      "origin_server_ts": 1767258000000,
      "reply_to": null
    },
    {
      "kind": "notice",
      "event_id": "$encrypted",
      "sender": "@bob:server.invalid",
      "notice": "undecryptable",
      "origin_server_ts": null
    },
    {
      "kind": "image",
      "event_id": "$image",
      "sender": "@alice:server.invalid",
      "origin_server_ts": 1767258060000,
      "name": "cat <script>.png",
      "size": 48213,
      "width": 640,
      "height": 480,
      "encrypted": true
    },
    {
      "kind": "state",
      "event_id": "$joined",
      "sender": "@bob:server.invalid",
      "origin_server_ts": 1767258090000,
      "change": {
        "type": "member",
        "target": "@bob:server.invalid",
        "membership": "joined"
      }
    },
    {
      "kind": "gap",
      "next_cursor": null
    }
  ],
  "nextCursor": "cursor-2"
} as const satisfies TimelinePage;

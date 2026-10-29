// The only module that talks to the Rust host. Components call these typed
// functions; nothing else may import @tauri-apps/api (enforced by a test).
// Types come from the generated public contracts. They describe the reply but
// do not validate it; the Rust side checks reply shapes and owns authorization.
import { Channel, invoke } from '@tauri-apps/api/core';
import type {
  AccountChange,
  AccountSettings,
  AccountStatus,
  AppStatus,
  ContactStatus,
  ConversationStart,
  DeviceList,
  DeviceSummary,
  DisplayName,
  ImageMime,
  ImagePick,
  HistoryVisibility,
  ImageView,
  InviteList,
  InviteSummary,
  JoinRule,
  LevelKey,
  Membership,
  HomeserverFound,
  ProxySetting,
  NewSessions,
  NotificationLevel,
  NotificationSettings,
  PendingMessage,
  ProfileList,
  ProfileSlot,
  RoomChange,
  RoomCreation,
  RoomIdentityChanges,
  RoomInvite,
  RoomLevel,
  RoomMember,
  RoomMembers,
  RoomActivity,
  RoomReaders,
  RoomServers,
  RoomNotifications,
  RoomPermissions,
  RoomSettings,
  SignInError,
  RoomList,
  SecurityStatus,
  StoreState,
  ThreePid,
  ThreePidList,
  ThreePidMedium,
  TimelineItem,
  TimelinePage,
  UserMatch,
  UserSearch
} from '../../../crates/contracts/generated/contracts';
import { parseBand, type Band } from '../image.ts';

export type {
  AccountChange,
  AccountSettings,
  AccountStatus,
  AppStatus,
  ContactStatus,
  ConversationStart,
  DeviceList,
  DeviceSummary,
  DisplayName,
  ImageMime,
  ImagePick,
  HistoryVisibility,
  ImageView,
  InviteList,
  InviteSummary,
  JoinRule,
  LevelKey,
  Membership,
  HomeserverFound,
  ProxySetting,
  NewSessions,
  NotificationLevel,
  NotificationSettings,
  ProfileList,
  ProfileSlot,
  RoomChange,
  RoomCreation,
  RoomIdentityChanges,
  RoomInvite,
  RoomLevel,
  RoomMember,
  RoomMembers,
  RoomActivity,
  RoomReaders,
  RoomServers,
  RoomNotifications,
  RoomPermissions,
  RoomSettings,
  PendingMessage,
  RoomList,
  SecurityStatus,
  SignInError,
  StoreState,
  ThreePid,
  ThreePidList,
  ThreePidMedium,
  TimelineItem,
  TimelinePage,
  UserMatch,
  UserSearch
};

export function appStatus(): Promise<AppStatus> {
  return invoke<AppStatus>('app_status');
}

export function roomList(): Promise<RoomList> {
  return invoke<RoomList>('room_list');
}

/** One page older than `cursor`, or the newest page when it is null. */
export function timelinePage(roomId: string, cursor: string | null): Promise<TimelinePage> {
  return invoke<TimelinePage>('timeline_page', { roomId, cursor });
}

/**
 * Calls `onChange` whenever the room changes. Messages carry no content; fetch
 * the newest page to see what changed. Resolves to a function that ends the
 * subscription.
 */
export async function subscribeRoom(
  roomId: string,
  onChange: () => void
): Promise<() => Promise<void>> {
  const channel = new Channel<null>(() => onChange());
  await invoke<null>('subscribe_room', { roomId, onChange: channel });
  return () => invoke<void>('unsubscribe_room', { roomId });
}

/**
 * Calls `onSession` whenever the core's session locks, unlocks or changes
 * state: everything shown from the session must be dropped then, and the host
 * has already ended every room subscription. Calls `onRooms` when rooms were
 * added or changed, so the room list should be fetched again, and
 * `onSecurity` when the account's encryption state changed (it clears
 * nothing). Anything else counts as a session change, which clears the page.
 */
export async function watchSession(
  onSession: () => void,
  onRooms: () => void,
  onSecurity: () => void
): Promise<void> {
  const channel = new Channel<string>((message) =>
    message === 'rooms' ? onRooms() : message === 'security' ? onSecurity() : onSession()
  );
  await invoke<null>('watch_session', { onChange: channel });
}

/** Locks the session; `locking` means work is still finishing (call again). */
export function lockSession(): Promise<StoreState> {
  return invoke<StoreState>('lock_session');
}

/**
 * Asks the core to unlock. The core asks for the profile passphrase in its own
 * window; the reply is `unlocking` and the outcome arrives as a session change.
 */
export function unlockSession(): Promise<StoreState> {
  return invoke<StoreState>('unlock_session');
}

/**
 * Asks the core to reset a profile whose passphrase was forgotten. The core
 * shows its own warning and deletes nothing without a yes there; the reply is
 * `unlocking` and the outcome arrives as a session change.
 */
export function resetProfile(): Promise<StoreState> {
  return invoke<StoreState>('reset_profile');
}

/**
 * Starts signing in to `homeserver`. Only the URL is sent: the core asks for
 * the username and password itself, in a Windows sign-in window. The outcome
 * arrives as a session change.
 */
export function signIn(homeserver: string): Promise<AccountStatus> {
  return invoke<AccountStatus>('sign_in', { homeserver });
}

/**
 * Starts signing in through the homeserver's OAuth 2.0 provider (ADR-0030).
 * The core asks before the provider's page opens in the browser, where the
 * password is entered; this page never sees it. Resolves with the status at
 * once; the outcome arrives as a session change.
 */
export function signInWithOidc(homeserver: string): Promise<AccountStatus> {
  return invoke<AccountStatus>('sign_in_with_oidc', { homeserver });
}

/**
 * Starts registering a new account named `username` (a localpart) on
 * `homeserver`. Only these two are sent: the core checks the name is free,
 * then asks for the password and any registration token in its own windows.
 * The outcome, or why it failed (such as `username_taken`), arrives as a
 * session change.
 */
export function register(homeserver: string, username: string): Promise<AccountStatus> {
  return invoke<AccountStatus>('register', { homeserver, username });
}

/**
 * Signs out: the core revokes the session if the homeserver can be reached
 * and deletes this device's local profile, keys first. The outcome arrives as
 * a session change; Persephone must then be restarted to sign in again.
 */
export function signOut(): Promise<StoreState> {
  return invoke<StoreState>('sign_out');
}

/** The account's encryption state. Codes only. */
export function securityStatus(): Promise<SecurityStatus> {
  return invoke<SecurityStatus>('security_status');
}

/**
 * Starts setting up encryption. The core asks for the password and shows the
 * recovery key in its own Windows windows; neither reaches this page.
 * Progress arrives as security changes.
 */
export function setUpEncryption(): Promise<SecurityStatus> {
  return invoke<SecurityStatus>('set_up_encryption');
}

/** Starts verifying this device with the recovery key, asked by the core. */
export function recoverWithKey(): Promise<SecurityStatus> {
  return invoke<SecurityStatus>('recover_with_key');
}

/**
 * Queues a text message. The core decides when it is sent: in an encrypted
 * room only from a verified device, in a room that is not encrypted only after
 * `allowUnencrypted`. Progress arrives as a change of the room.
 */
export function sendMessage(
  roomId: string,
  body: string,
  replyTo: string | null = null
): Promise<PendingMessage> {
  return invoke<PendingMessage>('send_message', { roomId, body, replyTo });
}

/** The room's messages sent from this device in this session. */
export function pendingMessages(roomId: string): Promise<PendingMessage[]> {
  return invoke<PendingMessage[]>('pending_messages', { roomId });
}

/** Sends a failed or unknown message again; it cannot arrive twice. */
export function retryMessage(localId: number): Promise<void> {
  return invoke<void>('retry_message', { localId });
}

export function discardMessage(localId: number): Promise<void> {
  return invoke<void>('discard_message', { localId });
}

/** Records consent to send unencrypted messages in a room; ask the user first. */
export function allowUnencrypted(roomId: string): Promise<void> {
  return invoke<void>('allow_unencrypted', { roomId });
}

/** Sets up recovery alone when an earlier attempt failed; no password needed. */
export function setUpRecovery(): Promise<SecurityStatus> {
  return invoke<SecurityStatus>('set_up_recovery');
}

/** Asks the account's other devices to verify this one by comparing emoji. */
export function verifyThisDevice(): Promise<SecurityStatus> {
  return invoke<SecurityStatus>('verify_this_device');
}

/** Accepts the verification another of the account's devices asked for. */
export function acceptVerification(flowId: string): Promise<SecurityStatus> {
  return invoke<SecurityStatus>('accept_verification', { flowId });
}

/** Confirms that the emoji match those on the other device. */
export function confirmVerification(flowId: string): Promise<SecurityStatus> {
  return invoke<SecurityStatus>('confirm_verification', { flowId });
}

/** Cancels the verification; while comparing, says the emoji differ. */
export function cancelVerification(flowId: string): Promise<SecurityStatus> {
  return invoke<SecurityStatus>('cancel_verification', { flowId });
}

/** The other person in a direct-message room and how far the account trusts them. */
export function contactStatus(roomId: string): Promise<ContactStatus> {
  return invoke<ContactStatus>('contact_status', { roomId });
}

/**
 * Asks the other person in this direct-message room to verify each other by
 * comparing emoji. The core finds the person; progress arrives as security
 * changes.
 */
export function verifyContact(roomId: string): Promise<SecurityStatus> {
  return invoke<SecurityStatus>('verify_contact', { roomId });
}

/** The members of a room whose encryption identity changed (ADR-0007). */
export function identityChanges(roomId: string): Promise<RoomIdentityChanges> {
  return invoke<RoomIdentityChanges>('identity_changes', { roomId });
}

/**
 * Asks the core to accept a member's new identity. The core confirms in its
 * own window; the outcome arrives as a security change.
 */
export function acceptIdentity(roomId: string, userId: string): Promise<SecurityStatus> {
  return invoke<SecurityStatus>('accept_identity', { roomId, userId });
}

/**
 * Downloads, decrypts and decodes the image of an `m.image` event in the
 * core's contained worker. Call it only when the user asks (ADR-0009). The
 * reply names the pixels by an opaque handle; release it when done.
 */
export function loadImage(roomId: string, eventId: string): Promise<ImageView> {
  return invoke<ImageView>('load_image', { roomId, eventId });
}

/**
 * One band of rows of a loaded or picked image. The host answers with raw
 * bytes, never JSON: width and rows as little-endian u32, then the RGBA
 * pixels. Rejects with `bad_image` when the answer is not exactly that.
 */
export async function imageBand(handle: string, band: number): Promise<Band> {
  return parseBand(await invoke<unknown>('image_band', { handle, band }));
}

/** Drops a loaded image's pixels in the core. */
export function releaseImage(handle: string): Promise<void> {
  return invoke<void>('release_image', { handle });
}

/**
 * Asks the user for a PNG or JPEG file in the core's own file dialog; no path
 * reaches this page. The reply is a draft (a copy without metadata, and its
 * preview) or `cancelled`. It arrives when the dialog closes.
 */
export function pickImage(roomId: string): Promise<ImagePick> {
  return invoke<ImagePick>('pick_image', { roomId });
}

/**
 * Queues a new version of one of this account's text messages, like
 * `sendMessage`. The core sends it only if the message is this account's
 * own text message, and fails it as `rejected` otherwise.
 */
export function editMessage(roomId: string, eventId: string, body: string): Promise<PendingMessage> {
  return invoke<PendingMessage>('edit_message', { roomId, eventId, body });
}

/**
 * Who can read a room's new messages: each member, how far this account
 * trusts them, and how many of their devices receive room keys or are
 * refused them (ADR-0005). At most 100 members.
 */
export function roomReaders(roomId: string): Promise<RoomReaders> {
  return invoke<RoomReaders>('room_readers', { roomId });
}

/**
 * The homeservers with members in a room: each receives the room's messages
 * and metadata. The most members first, at most 50.
 */
export function roomServers(roomId: string): Promise<RoomServers> {
  return invoke<RoomServers>('room_servers', { roomId });
}

/**
 * Asks the core to open a link from a message. The core checks it and asks
 * the user in its own window first (ADR-0024). Resolves `true` when it
 * opened and `false` when the user said no.
 */
export function openLink(url: string): Promise<boolean> {
  return invoke<boolean>('open_link', { url });
}

/**
 * Who else is typing in a room and, for an event, who else has read up to
 * it. At most ten names each; `truncated` says more exist.
 */
export function roomActivity(roomId: string, eventId: string | null): Promise<RoomActivity> {
  return invoke<RoomActivity>('room_activity', { roomId, eventId });
}

/**
 * Tells a room this account started or stopped typing. Rejected with
 * `rejected` while the account's setting is off; call it only when on.
 */
export function setTyping(roomId: string, typing: boolean): Promise<void> {
  return invoke<void>('set_typing', { roomId, typing });
}

/**
 * Marks a room read up to an event, privately: its fully-read marker and a
 * private read receipt, which other people do not see.
 */
export function markRead(roomId: string, eventId: string): Promise<void> {
  return invoke<void>('mark_read', { roomId, eventId });
}

/**
 * Reacts to an event with a short key, usually an emoji. Sent like a message:
 * in a room that is not encrypted only after `allowUnencrypted`. A reaction
 * is taken back with `deleteMessage` on its own event.
 */
export function react(roomId: string, eventId: string, key: string): Promise<void> {
  return invoke<void>('react', { roomId, eventId, key });
}

/**
 * Deletes (redacts) a message in a joined room, for everyone. The homeserver
 * decides whether this account may (`rejected` otherwise).
 */
export function deleteMessage(roomId: string, eventId: string): Promise<void> {
  return invoke<void>('delete_message', { roomId, eventId });
}

/**
 * Asks the user for a file to send, in the core's own dialog, and queues it
 * at once (ADR-0029). Resolves with the queued message, or null when the
 * user cancelled.
 */
export function pickFile(roomId: string): Promise<PendingMessage | null> {
  return invoke<PendingMessage | null>('pick_file', { roomId });
}

/**
 * Saves a received file: the core downloads and decrypts it, then asks where
 * in its own Save dialog. Resolves `true` when saved, `false` when cancelled.
 */
export function saveFile(roomId: string, eventId: string): Promise<boolean> {
  return invoke<boolean>('save_file', { roomId, eventId });
}

/** Queues a picked image, by its draft handle, like `sendMessage`. */
export function sendImage(roomId: string, handle: string): Promise<PendingMessage> {
  return invoke<PendingMessage>('send_image', { roomId, handle });
}

/** Drops a picked image that was not sent. */
export function discardImage(handle: string): Promise<void> {
  return invoke<void>('discard_image', { handle });
}

/** Room invitations the account has not answered, direct-message or not. */
export function inviteList(): Promise<InviteList> {
  return invoke<InviteList>('invite_list');
}

/** Joins the room of an invitation. */
export function acceptInvite(roomId: string): Promise<void> {
  return invoke<void>('accept_invite', { roomId });
}

/** Refuses an invitation. */
export function declineInvite(roomId: string): Promise<void> {
  return invoke<void>('decline_invite', { roomId });
}

/**
 * Opens a conversation with a user: an existing direct-message room with just
 * them, or a new encrypted one they are invited to. The core refuses a user
 * without a cross-signing identity.
 */
export function startConversation(userId: string): Promise<ConversationStart> {
  return invoke<ConversationStart>('start_conversation', { userId });
}

/**
 * Searches the homeserver's user directory. The homeserver sees every term:
 * call this only while the user has turned directory search on.
 */
export function searchUsers(term: string): Promise<UserSearch> {
  return invoke<UserSearch>('search_users', { term });
}

/**
 * Creates an encrypted, invite-only room with this name and invites these
 * people. The core refuses, creating nothing, when one of them has no
 * cross-signing identity.
 */
export function createRoom(name: string, invite: string[]): Promise<RoomCreation> {
  return invoke<RoomCreation>('create_room', { name, invite });
}

/** Invites one person to a joined room. */
export function inviteUser(roomId: string, userId: string): Promise<RoomInvite> {
  return invoke<RoomInvite>('invite_user', { roomId, userId });
}

/**
 * A joined room's settings: name, topic, addresses, access, encryption,
 * notifications and power levels, with what this account may change. The
 * permissions are hints; the homeserver decides.
 */
export function roomSettings(roomId: string): Promise<RoomSettings> {
  return invoke<RoomSettings>('room_settings', { roomId });
}

/**
 * A joined room's members (joined, invited, asking to join or banned) whose
 * ID or display name contains `filter`; an empty filter lists everyone, up to
 * the core's limit.
 */
export function roomMembers(roomId: string, filter: string): Promise<RoomMembers> {
  return invoke<RoomMembers>('room_members', { roomId, filter });
}

/**
 * Changes one setting of a joined room, or acts on one member. Rejects with
 * `rejected` when the homeserver refuses, for example for lack of permission.
 */
export function changeRoom(roomId: string, change: RoomChange): Promise<void> {
  return invoke<void>('change_room', { roomId, change });
}

/** This account's display name, as the homeserver has it. */
export function displayName(): Promise<DisplayName> {
  return invoke<DisplayName>('display_name');
}

/** Sets this account's display name; an empty name removes it. */
export function setDisplayName(name: string): Promise<DisplayName> {
  return invoke<DisplayName>('set_display_name', { name });
}

/** This account's settings, kept in its encrypted profile. */
export function accountSettings(): Promise<AccountSettings> {
  return invoke<AccountSettings>('account_settings');
}

/**
 * Replaces this account's settings; every setting is sent, and nothing else.
 * Rejects with `invalid_argument` when a text size is out of its bounds.
 */
export function setAccountSettings(settings: AccountSettings): Promise<void> {
  return invoke<void>('set_account_settings', {
    settings: {
      enterSends: settings.enterSends,
      chatTextPercent: settings.chatTextPercent,
      interfaceTextPercent: settings.interfaceTextPercent,
      desktopNotifications: settings.desktopNotifications,
      sendTyping: settings.sendTyping,
      sendReadReceipts: settings.sendReadReceipts,
      idleLockMinutes: settings.idleLockMinutes,
      notificationNames: settings.notificationNames
    }
  });
}

/** The profile's SOCKS5 proxy as `host:port`, or none (ADR-0028). */
export function proxySetting(): Promise<ProxySetting> {
  return invoke<ProxySetting>('proxy_setting');
}

/**
 * Sets the profile's SOCKS5 proxy (`host:port`), or removes it with `null`.
 * It applies to connections Persephone makes from its next start.
 */
export function setProxySetting(proxy: string | null): Promise<void> {
  return invoke<void>('set_proxy_setting', { proxy });
}

/**
 * Finds the homeserver of a server name such as `example.org`, through its
 * `.well-known` file, and confirms it speaks Matrix (ADR-0027). Rejects with
 * `not_found` when no Matrix server is there.
 */
export function discoverHomeserver(serverName: string): Promise<HomeserverFound> {
  return invoke<HomeserverFound>('discover_homeserver', { serverName });
}

/**
 * The sessions of this account the user has not been shown yet. The first
 * time, the current ones are recorded and none is new (ADR-0026).
 */
export function newSessions(): Promise<NewSessions> {
  return invoke<NewSessions>('new_sessions');
}

/** Records that the user has seen these sessions (at most ten device IDs). */
export function acknowledgeSessions(deviceIds: string[]): Promise<void> {
  return invoke<void>('acknowledge_sessions', { deviceIds });
}

/**
 * Signs out another session of this account. The core asks for the
 * password in its own window; no password crosses. Resolves with how it
 * ended; this device's own ID is `invalid_argument`.
 */
export function removeDevice(deviceId: string): Promise<AccountChange> {
  return invoke<AccountChange>('remove_device', { deviceId });
}

/**
 * Changes this account's password. The core asks for the current password
 * and the new one (twice) in its own windows; no password crosses. Resolves
 * with how it ended. `signOutOthers` signs out every other device.
 */
export function changePassword(signOutOthers: boolean): Promise<AccountChange> {
  return invoke<AccountChange>('change_password', { signOutOthers });
}

/** This account's email addresses and phone numbers, and any one waiting. */
export function threePids(): Promise<ThreePidList> {
  return invoke<ThreePidList>('three_pids');
}

/** Asks the homeserver to send a confirmation link to this email address. */
export function requestEmailToken(email: string): Promise<AccountChange> {
  return invoke<AccountChange>('request_email_token', { email });
}

/**
 * Asks the homeserver to send a code to this phone number: a two-letter
 * country code and the number's digits as dialled in that country.
 */
export function requestPhoneToken(country: string, phoneNumber: string): Promise<AccountChange> {
  return invoke<AccountChange>('request_phone_token', { country, phoneNumber });
}

/**
 * Adds the address waiting to be added. The core asks for a phone's code and
 * the account password in its own windows.
 */
export function addThreePid(): Promise<AccountChange> {
  return invoke<AccountChange>('add_three_pid');
}

/** Removes an email address or phone number from this account. */
export function removeThreePid(medium: ThreePidMedium, address: string): Promise<AccountChange> {
  return invoke<AccountChange>('remove_three_pid', { medium, address });
}

/**
 * Asks another of this account's devices, by its ID, to verify each other by
 * comparing emoji. Progress arrives as security changes.
 */
export function verifyDevice(deviceId: string): Promise<SecurityStatus> {
  return invoke<SecurityStatus>('verify_device', { deviceId });
}

/**
 * Replaces the recovery key. The core warns, shows the new key and offers to
 * save it in its own windows; the key never reaches this page.
 */
export function changeRecoveryKey(): Promise<SecurityStatus> {
  return invoke<SecurityStatus>('change_recovery_key');
}

/** This account's notification level, from its push rules. */
export function notificationLevel(): Promise<NotificationSettings> {
  return invoke<NotificationSettings>('notification_level');
}

/** Sets this account's notification level in its push rules. */
export function setNotificationLevel(level: Exclude<NotificationLevel, 'custom'>): Promise<void> {
  return invoke<void>('set_notification_level', { level });
}

/** The devices (sessions) signed in to this account. */
export function deviceList(): Promise<DeviceList> {
  return invoke<DeviceList>('device_list');
}

/** The local profiles (accounts) on this computer, by slot number only. */
export function profileList(): Promise<ProfileList> {
  return invoke<ProfileList>('profile_list');
}

/**
 * Makes a new profile and switches to it. The core then closes this window,
 * locks the stores and starts again with the new profile.
 */
export function addProfile(): Promise<void> {
  return invoke<void>('add_profile');
}

/** Switches to the profile in this slot, as `addProfile` does. */
export function switchProfile(slot: number): Promise<void> {
  return invoke<void>('switch_profile', { slot });
}

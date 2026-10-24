// The only module that talks to the Rust host. Components call these typed
// functions; nothing else may import @tauri-apps/api (enforced by a test).
// Types come from the generated public contracts. They describe the reply but
// do not validate it; the Rust side checks reply shapes and owns authorization.
import { Channel, invoke } from '@tauri-apps/api/core';
import type {
  AccountStatus,
  AppStatus,
  ContactStatus,
  ImageMime,
  ImagePick,
  ImageView,
  PendingMessage,
  RoomIdentityChanges,
  SignInError,
  RoomList,
  SecurityStatus,
  StoreState,
  TimelineItem,
  TimelinePage
} from '../../../crates/contracts/generated/contracts';
import { parseBand, type Band } from '../image';

export type {
  AccountStatus,
  AppStatus,
  ContactStatus,
  ImageMime,
  ImagePick,
  ImageView,
  RoomIdentityChanges,
  PendingMessage,
  RoomList,
  SecurityStatus,
  SignInError,
  StoreState,
  TimelineItem,
  TimelinePage
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
export function sendMessage(roomId: string, body: string): Promise<PendingMessage> {
  return invoke<PendingMessage>('send_message', { roomId, body });
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

/** Queues a picked image, by its draft handle, like `sendMessage`. */
export function sendImage(roomId: string, handle: string): Promise<PendingMessage> {
  return invoke<PendingMessage>('send_image', { roomId, handle });
}

/** Drops a picked image that was not sent. */
export function discardImage(handle: string): Promise<void> {
  return invoke<void>('discard_image', { handle });
}

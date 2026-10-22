// The only module that talks to the Rust host. Components call these typed
// functions; nothing else may import @tauri-apps/api (enforced by a test).
// Types come from the generated public contracts. They describe the reply but
// do not validate it; the Rust side checks reply shapes and owns authorization.
import { Channel, invoke } from '@tauri-apps/api/core';
import type {
  AccountStatus,
  AppStatus,
  PendingMessage,
  SignInError,
  RoomList,
  SecurityStatus,
  StoreState,
  TimelineItem,
  TimelinePage
} from '../../../crates/contracts/generated/contracts';

export type {
  AccountStatus,
  AppStatus,
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

export function unlockSession(): Promise<StoreState> {
  return invoke<StoreState>('unlock_session');
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

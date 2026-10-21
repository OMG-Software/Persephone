// The only module that talks to the Rust host. Components call these typed
// functions; nothing else may import @tauri-apps/api (enforced by a test).
// Types come from the generated public contracts. They describe the reply but
// do not validate it; the Rust side checks reply shapes and owns authorization.
import { Channel, invoke } from '@tauri-apps/api/core';
import type {
  AppStatus,
  RoomList,
  StoreState,
  TimelineItem,
  TimelinePage
} from '../../../crates/contracts/generated/contracts';

export type { AppStatus, RoomList, StoreState, TimelineItem, TimelinePage };

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
 * Calls `onChange` whenever the core's session locks, unlocks or changes
 * state. Everything shown from the session must be dropped then; the host has
 * already ended every room subscription.
 */
export async function watchSession(onChange: () => void): Promise<void> {
  const channel = new Channel<null>(() => onChange());
  await invoke<null>('watch_session', { onChange: channel });
}

/** Locks the session; `locking` means work is still finishing (call again). */
export function lockSession(): Promise<StoreState> {
  return invoke<StoreState>('lock_session');
}

export function unlockSession(): Promise<StoreState> {
  return invoke<StoreState>('unlock_session');
}

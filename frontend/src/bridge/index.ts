// The only module that talks to the Rust host. Components call these typed
// functions; nothing else may import @tauri-apps/api (enforced by a test).
// Types come from the generated public contracts. They describe the reply but
// do not validate it; the Rust host owns validation and authorization.
import { invoke } from '@tauri-apps/api/core';
import type { AppStatus } from '../../../crates/contracts/generated/contracts';

export type { AppStatus };

export function appStatus(): Promise<AppStatus> {
  return invoke<AppStatus>('app_status');
}

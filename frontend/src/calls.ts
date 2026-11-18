// What the call UI shows, as plain data. The core sends codes, flags and
// bounded IDs only; every text comes from the tables here. Each table is a
// full `Record`, so a new code in the contract fails type checking until it
// has a text.
import type {
  CallCamera,
  CallDevice,
  CallDevices,
  CallEnd,
  CallJoinError,
  CallPhase,
  CallReadiness,
  CallStatus,
  CallUnavailable
} from '../../crates/contracts/generated/contracts';

/** Shown when a camera command fails before the core answers. */
export const CAMERA_COMMAND_TEXT = 'The camera did not change. Try again.';

export const UNAVAILABLE_TEXT: Record<CallUnavailable, string> = {
  unsupported_platform: 'Calls are not available on this system yet.',
  not_joined: 'Join the room to start a call.',
  not_encrypted: 'Calls need an encrypted room.',
  no_call_server: 'Your homeserver has no call service.',
  server_unreachable: 'Cannot reach your homeserver to check for calls.'
};

export const JOIN_ERROR_TEXT: Record<CallJoinError, string> = {
  already_in_call: 'You are already in a call. Leave it first.',
  not_joined: 'Join the room to start a call.',
  not_encrypted: 'Calls need an encrypted room.',
  no_call_server: 'Your homeserver has no call service.',
  declined: 'The call was not started.',
  unsupported_platform: 'Calls are not available on this system yet.',
  containment: 'The call did not start: its process could not be contained.',
  no_worker: 'The call component is missing. Reinstall Persephone.',
  no_delayed_events:
    'Your homeserver cannot end a call membership safely, so the call was not started.',
  membership: 'Your homeserver refused the call membership.',
  call_server: 'The call server refused the connection.',
  keys: 'The call keys could not be made.',
  proxy_unreadable: 'The proxy setting cannot be read, so the call was not started.',
  device_setting_unreadable:
    'The call device setting cannot be read. Choose the devices again in Preferences.'
};

// `left` has no text: the bar just goes away.
export const END_TEXT: Record<CallEnd, string> = {
  left: '',
  connection_failed: 'The connection to the call server failed.',
  audio_device: 'The microphone or speaker is not available.',
  device_not_found:
    'The chosen microphone or speaker is not connected. Choose another in Preferences.',
  containment: 'The call stopped: its process was not contained.',
  worker_stopped: 'The call process stopped.',
  membership_lost: 'Your homeserver ended your call membership.',
  keys_unavailable: 'The call keys are not available.',
  session_ended: 'The call ended because Persephone locked.',
  internal: 'The call ended because of an error.'
};

export const PHASE_TEXT: Record<CallPhase, string> = {
  idle: '',
  joining: 'Connecting…',
  active: 'In call',
  ended: 'Call ended'
};

export const IN_CALL_TEXT = 'You are already in a call.';
export const STARTING_TEXT = 'Starting the call…';
export const NO_KEY_TEXT = 'Cannot hear yet: no key';
export const UNREACHABLE_TEXT = 'On another call server: not reachable';

export const IDLE_CALL: CallStatus = {
  phase: 'idle',
  roomId: null,
  muted: false,
  participants: [],
  ended: null,
  joinError: null,
  camera: 'off',
  video: []
};

/** This account is in a call, or joining one. */
export function inCall(status: CallStatus): boolean {
  return status.phase === 'joining' || status.phase === 'active';
}

export type CallButtonView = {
  hidden: boolean;
  disabled: boolean;
  label: string;
  // Why the button is disabled; shown as its accessible description.
  reason: string | null;
};

/**
 * The room header's call button. `pending` is true while a join request is
 * waiting for the core, so a second click cannot start another one.
 */
export function callButton(
  roomId: string,
  readiness: CallReadiness | null,
  status: CallStatus,
  pending: boolean
): CallButtonView {
  const count = readiness?.activeMembers ?? 0;
  const label = count > 0 ? `Join call (${count})` : 'Start call';
  if (inCall(status)) {
    const here = status.roomId === roomId;
    return { hidden: here, disabled: true, label, reason: here ? null : IN_CALL_TEXT };
  }
  if (pending) return { hidden: false, disabled: true, label, reason: null };
  if (readiness === null) return { hidden: false, disabled: true, label, reason: null };
  if (!readiness.available) {
    const reason = readiness.reason === null ? null : UNAVAILABLE_TEXT[readiness.reason];
    return { hidden: false, disabled: true, label, reason };
  }
  return { hidden: false, disabled: false, label, reason: null };
}

export const VIDEO_CALL_LABEL = 'Video call';

/**
 * The room header's video call button. It follows the call button's rules:
 * the same readiness, the same reasons and the same pending state. Only the
 * label differs.
 */
export function videoCallButton(
  roomId: string,
  readiness: CallReadiness | null,
  status: CallStatus,
  pending: boolean
): CallButtonView {
  const view = callButton(roomId, readiness, status, pending);
  const count = readiness?.activeMembers ?? 0;
  return { ...view, label: count > 0 ? `Join with video (${count})` : VIDEO_CALL_LABEL };
}

export const CAMERA_ON_LABEL = 'Turn camera on';
export const CAMERA_OFF_LABEL = 'Turn camera off';
export const CAMERA_STARTING_TEXT = 'Starting the camera…';
export const CAMERA_NOT_FOUND_TEXT = 'The chosen camera is not connected';
export const CAMERA_UNAVAILABLE_TEXT = 'The camera could not start';

/** The note beside the camera toggle; empty when the camera needs none. */
export const CAMERA_TEXT: Record<CallCamera, string> = {
  off: '',
  starting: CAMERA_STARTING_TEXT,
  on: '',
  not_found: CAMERA_NOT_FOUND_TEXT,
  unavailable: CAMERA_UNAVAILABLE_TEXT
};

export type CameraButtonView = {
  label: string;
  // The camera is on, or is starting.
  pressed: boolean;
};

/**
 * The camera toggle in the call bar. A camera that failed (`not_found`,
 * `unavailable`) is off, so the toggle offers to turn it on again.
 */
export function cameraButton(camera: CallCamera): CameraButtonView {
  const pressed = camera === 'on' || camera === 'starting';
  return { label: pressed ? CAMERA_OFF_LABEL : CAMERA_ON_LABEL, pressed };
}

/** The label of the own tile. */
export const OWN_TILE_LABEL = 'You';
/** The label of a remote tile the core sent without a device. */
export const PARTICIPANT_TILE_LABEL = 'Participant';

/** The call has a camera picture to show: at least one video tile. */
export function hasVideo(status: CallStatus): boolean {
  return inCall(status) && status.video.length > 0;
}

/**
 * Whether the call bar shows. An ended call shows until dismissed, except
 * `left`, which needs no text.
 */
export function showCallBar(status: CallStatus, dismissed: boolean): boolean {
  if (inCall(status)) return true;
  return status.phase === 'ended' && !dismissed && status.ended !== 'left';
}

/** The text announced for a phase change; empty when nothing is announced. */
export function callAnnouncement(status: CallStatus, dismissed: boolean): string {
  if (!showCallBar(status, dismissed)) return '';
  if (status.phase === 'ended') {
    return status.ended === null ? END_TEXT.internal : END_TEXT[status.ended];
  }
  return PHASE_TEXT[status.phase];
}

/** Orders call operations, so a late reply cannot undo a later one. */
export class CallOperations {
  #latest = 0;
  #completed = 0;

  /** Starts an operation. The newest token is the only current one. */
  begin(): number {
    this.#latest += 1;
    return this.#latest;
  }

  /** True while no later operation has started. */
  isCurrent(token: number): boolean {
    return token === this.#latest;
  }

  /** An operation finished, with or without a reply. */
  complete(): void {
    this.#completed += 1;
  }

  /** How many operations have finished. A status read compares it before and after. */
  get completed(): number {
    return this.#completed;
  }
}

/**
 * Runs `fetch` for call status. A request that arrives while one is in
 * flight makes at most one more run after it. `reset` (a session change)
 * clears the shown call at once and drops any reply still on its way.
 * `completed` counts the call commands that have finished: a read during
 * which one finished is dropped, because that command's reply is newer. The
 * command requests a refresh when it finishes, so a fresh read follows.
 */
export class CallRefresher {
  #fetch: () => Promise<CallStatus>;
  #apply: (status: CallStatus) => void;
  #completed: () => number;
  #running = false;
  #again = false;
  #generation = 0;

  constructor(
    fetch: () => Promise<CallStatus>,
    apply: (status: CallStatus) => void,
    completed: () => number = () => 0
  ) {
    this.#fetch = fetch;
    this.#apply = apply;
    this.#completed = completed;
  }

  request(): void {
    if (this.#running) {
      this.#again = true;
      return;
    }
    void this.#run();
  }

  reset(): void {
    this.#generation += 1;
    this.#again = false;
    this.#apply(IDLE_CALL);
  }

  async #run(): Promise<void> {
    this.#running = true;
    try {
      do {
        this.#again = false;
        const generation = this.#generation;
        const completed = this.#completed();
        try {
          const status = await this.#fetch();
          if (generation === this.#generation && completed === this.#completed()) this.#apply(status);
        } catch {
          // Keep what is shown; the next change asks again.
        }
      } while (this.#again);
    } finally {
      this.#running = false;
    }
  }
}

/** One choice in a device menu. `value` is the device ID, or '' for the default. */
export interface DeviceOption {
  value: string;
  label: string;
}

export const DEFAULT_DEVICE_LABEL = 'System default';
export const UNNAMED_DEVICE_LABEL = 'Unnamed device';
export const MISSING_DEVICE_LABEL = 'Saved device (not connected)';

/**
 * The choices of one device menu: the system default, each connected device
 * by name, and one extra choice when the saved device is not connected, so
 * the saved choice stays visible and is not lost by looking at the list.
 * Names are untrusted text: the labels are for text rendering only.
 */
export function deviceOptions(devices: readonly CallDevice[], selected: string | null): DeviceOption[] {
  const options: DeviceOption[] = [{ value: '', label: DEFAULT_DEVICE_LABEL }];
  for (const device of devices) {
    options.push({ value: device.id, label: device.name === '' ? UNNAMED_DEVICE_LABEL : device.name });
  }
  if (selected !== null && selected !== '' && !devices.some((device) => device.id === selected)) {
    options.push({ value: selected, label: MISSING_DEVICE_LABEL });
  }
  return options;
}

/** The value a menu shows for a saved choice. */
export function deviceValue(selected: string | null): string {
  return selected ?? '';
}

/** The saved choice a menu value stands for: '' is the system default, `null`. */
export function deviceChoice(value: string): string | null {
  return value === '' ? null : value;
}

export const DEVICES_UNSUPPORTED_TEXT = 'Calls are not available on this system yet.';
export const DEVICES_UNREADABLE_TEXT = 'The saved call devices could not be read. Choose them again.';
export const DEVICES_NOTE_TEXT = 'Changes apply from the next call.';

/** The note under the menus, for a device reply. */
export function devicesNote(devices: CallDevices): string {
  if (!devices.supported) return DEVICES_UNSUPPORTED_TEXT;
  return devices.unreadable ? DEVICES_UNREADABLE_TEXT : DEVICES_NOTE_TEXT;
}

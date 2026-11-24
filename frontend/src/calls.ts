// What the call UI shows, as plain data. The core sends codes, flags and
// bounded IDs only; every text comes from the tables here. Each table is a
// full `Record`, so a new code in the contract fails type checking until it
// has a text.
import type {
  CallCamera,
  CallDevice,
  CallDeviceChange,
  CallDevices,
  CallDeviceTestKind,
  CallDeviceTestState,
  CallDeviceTestStatus,
  CALL_DEVICE_ID_BYTES_MAX,
  CALL_PARTICIPANTS_MAX,
  CALL_USER_ID_BYTES_MAX,
  CallEnd,
  CallJoinError,
  CallParticipantLevel,
  CallPhase,
  CallReadiness,
  CallScreen,
  CallRing,
  CallRings,
  CallStatus,
  CallUnavailable
} from '../../crates/contracts/generated/contracts';

/** Shown when a camera command fails before the core answers. */
export const CAMERA_COMMAND_TEXT = 'The camera did not change. Try again.';
/** Shown when a screen share command fails before the core answers. */
export const SCREEN_COMMAND_TEXT = 'The screen share did not change. Try again.';

export const UNAVAILABLE_TEXT: Record<CallUnavailable, string> = {
  unsupported_platform: 'Calls are not available on this system yet.',
  not_joined: 'Join the room to start a call.',
  not_encrypted: 'Calls need an encrypted room.',
  not_permitted: 'You do not have permission to start a call in this room.',
  no_call_server: 'Your homeserver has no call service.',
  call_server_unsupported: "Your homeserver's call service is not one Persephone supports.",
  server_unreachable: 'Cannot reach your homeserver to check for calls.'
};

export const JOIN_ERROR_TEXT: Record<CallJoinError, string> = {
  already_in_call: 'You are already in a call. Leave it first.',
  not_joined: 'Join the room to start a call.',
  not_encrypted: 'Calls need an encrypted room.',
  no_call_server: 'Your homeserver has no call service.',
  call_server_unsupported: "Your homeserver's call service is not one Persephone supports.",
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
    'The call device setting cannot be read. Choose the devices again in Settings, Calls.'
};

// `left` has no text: the bar just goes away.
export const END_TEXT: Record<CallEnd, string> = {
  left: '',
  connection_failed: 'The connection to the call server failed.',
  audio_device: 'The microphone or speaker is not available.',
  device_not_found:
    'The chosen microphone or speaker is not connected. Choose another in Settings, Calls.',
  containment: 'The call stopped: its process was not contained.',
  worker_stopped: 'The call process stopped.',
  worker_crashed: 'The call process failed with an internal error.',
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
/**
 * Shown while a join waits for the core. A profile with a proxy makes the
 * core open its own warning window first, and the page cannot see that
 * window, so the text names it.
 */
export const STARTING_TEXT = 'Starting the call… If a proxy warning window is open, answer it to go on.';
export const NO_KEY_TEXT = 'Cannot hear yet: no key';
export const UNREACHABLE_TEXT = 'On another call server: not reachable';

export const IDLE_CALL: CallStatus = {
  phase: 'idle',
  roomId: null,
  muted: false,
  deafened: false,
  volume: 100,
  participants: [],
  ended: null,
  joinError: null,
  camera: 'off',
  screen: 'off',
  video: [],
  ringDeclined: false,
  deviceChange: null
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
  if (pending) return { hidden: false, disabled: true, label, reason: STARTING_TEXT };
  if (readiness === null) return { hidden: false, disabled: true, label, reason: null };
  if (!readiness.available) {
    const reason = readiness.reason === null ? null : UNAVAILABLE_TEXT[readiness.reason];
    return { hidden: false, disabled: true, label, reason };
  }
  return { hidden: false, disabled: false, label, reason: null };
}

/**
 * The visible note after a click on a call button that cannot start a call.
 * `asked` is the room where that click was, or `null`. The note follows the
 * current view: it changes or goes when the reason does, and a room change
 * removes it. A disabled button without a reason has nothing to say.
 */
export function callReasonNote(
  roomId: string,
  asked: string | null,
  view: CallButtonView | null
): string | null {
  if (asked !== roomId || view === null || view.hidden || !view.disabled) return null;
  return view.reason;
}

/**
 * The click that [`callReasonNote`] keeps: `asked` while its note shows,
 * otherwise `null`. A note that goes (the room is ready, the button hides,
 * the room changes) needs a new click to come back.
 */
export function keptReasonRoom(
  roomId: string | null,
  asked: string | null,
  view: CallButtonView | null
): string | null {
  return roomId !== null && callReasonNote(roomId, asked, view) !== null ? asked : null;
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

export const SCREEN_ON_LABEL = 'Share screen';
export const SCREEN_OFF_LABEL = 'Stop sharing';
export const SCREEN_STARTING_TEXT = 'Starting the screen share…';
export const SCREEN_SHARING_TEXT = 'You are sharing your screen';
export const SCREEN_NOT_FOUND_TEXT = 'The chosen screen is not available any more';
export const SCREEN_UNAVAILABLE_TEXT = 'Screen sharing is not available here';
export const SCREEN_REFUSED_TEXT = 'Your system did not allow the screen share';

/** The note beside the screen share button; empty when it needs none. */
export const SCREEN_TEXT: Record<CallScreen, string> = {
  off: '',
  starting: SCREEN_STARTING_TEXT,
  on: SCREEN_SHARING_TEXT,
  not_found: SCREEN_NOT_FOUND_TEXT,
  unavailable: SCREEN_UNAVAILABLE_TEXT,
  refused: SCREEN_REFUSED_TEXT
};

export type ScreenButtonView = {
  label: string;
  // The share runs, or is starting.
  pressed: boolean;
};

/**
 * The screen share button in the call bar. A share that failed (`not_found`,
 * `unavailable`, `refused`) is off, so the button offers to start it again.
 * The button never says what is shared: the core and the desktop ask the user.
 */
export function screenButton(screen: CallScreen): ScreenButtonView {
  const pressed = screen === 'on' || screen === 'starting';
  return { label: pressed ? SCREEN_OFF_LABEL : SCREEN_ON_LABEL, pressed };
}

/** The mark beside a participant who shares a screen. Plain text. */
export const SHARING_TEXT = 'is sharing their screen';
/** The label of the remote screen tile. */
export const SCREEN_TILE_LABEL = 'Shared screen';

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
  // The last token of the previous session; older tokens count for nothing.
  #session = 0;

  /** Starts an operation. The newest token is the only current one. */
  begin(): number {
    this.#latest += 1;
    return this.#latest;
  }

  /** True while no later operation has started and no reset came after it. */
  isCurrent(token: number): boolean {
    return token === this.#latest && token > this.#session;
  }

  /**
   * The operation `token` finished, with or without a reply. An operation
   * of a session before the last `reset` is not counted: it asks for no
   * fresh read in the new session, so it must not drop that session's read.
   */
  complete(token: number): void {
    if (token > this.#session) this.#completed += 1;
  }

  /** A session change: operations started before it are neither current nor counted. */
  reset(): void {
    this.#session = this.#latest;
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

/** The highest call volume: the sound as received (ADR-0055). */
export const VOLUME_MAX = 100;
export const VOLUME_LABEL = 'Call volume';
export const DEAFEN_LABEL = 'Deafen';
export const UNDEAFEN_LABEL = 'Undeafen';
/** The accessible description of the deafen button: what it does besides silencing the call. */
export const DEAFEN_TEXT = 'Turns off all call sound and mutes your microphone';
export const VOLUME_SAVE_TEXT = 'The call volume could not be saved.';

/**
 * A slider value as a volume: a whole number from 0 to 100. Anything that is
 * not a finite number is full volume, so a bad value never silences a call.
 */
export function volumeValue(value: number): number {
  if (!Number.isFinite(value)) return VOLUME_MAX;
  return Math.min(VOLUME_MAX, Math.max(0, Math.round(value)));
}

/** The text a volume is read and shown as. */
export function volumeText(percent: number): string {
  return `${volumeValue(percent)}%`;
}

/**
 * The label of the own microphone's level meter (ADR-0055, ADR-0059): it
 * shows what the microphone picks up, so a silent meter means the call does
 * not hear you.
 */
export const SOUND_LEVEL_LABEL = 'Your microphone';
/** The top of the meter: full scale, 0 dBFS. 0 is -60 dBFS or quieter. */
export const SOUND_LEVEL_MAX = 100;
/** From this level (-6 dBFS) the meter shows the sound as loud. */
export const SOUND_LEVEL_LOUD = 90;

/**
 * The meter level the host sent, or `null` when it is not a whole number from
 * 0 to {@link SOUND_LEVEL_MAX}. The core sends nothing else; this keeps a bad
 * value off the page.
 */
export function parseSoundLevel(value: unknown): number | null {
  return typeof value === 'number' && Number.isInteger(value) && value >= 0 && value <= SOUND_LEVEL_MAX
    ? value
    : null;
}

/**
 * The own microphone level the page shows (ADR-0055, ADR-0059). It is 0
 * while a call joins, so a new call never starts with an old level. In every
 * other phase it shows: the call's own level while the call is active, and a
 * device test's level while no call runs, also while the bar still shows how
 * the last call ended. The core sends 0 when a call ends, and the call bar
 * shows no meter after the end.
 */
export function shownSoundLevel(phase: CallPhase, level: number): number {
  return phase === 'joining' ? 0 : level;
}

/** The meter's spoken value: "Silent", else the level in decibels below full scale. */
export function soundLevelText(level: number): string {
  if (level <= 0) return 'Silent';
  const decibels = Math.round(Math.min(level, SOUND_LEVEL_MAX) * 0.6 - 60);
  return decibels === 0 ? '0 dB' : `−${-decibels} dB`;
}

/**
 * Sends only the newest value. While one send waits, later values replace
 * each other, and the last one goes when the send ends. So a slider that
 * moves fast sends at most one value per round trip, and its final position
 * is always sent. `settled` runs when no send waits and none is due. It is
 * told whether the last send failed: a failure that a later send replaced
 * does not count, because the later value is the one that matters.
 * `clear()` also disowns a send that is still waiting (a session change): its
 * outcome is not reported, and `settled` runs only after a send that started
 * after the latest `clear()`.
 */
export class LatestOnly<T> {
  #send: (value: T) => Promise<void>;
  #settled: (failed: boolean) => void;
  #sending = false;
  #next: { value: T } | null = null;
  // Goes up on each `clear()`; a send started in an older epoch is disowned.
  #epoch = 0;

  constructor(send: (value: T) => Promise<void>, settled: (failed: boolean) => void = () => {}) {
    this.#send = send;
    this.#settled = settled;
  }

  /** Sends `value` now, or after the send that waits. */
  set(value: T): void {
    this.#next = { value };
    if (!this.#sending) void this.#run();
  }

  /**
   * Drops a value that has not gone yet, and disowns a send that waits (a
   * session change): its failure is never reported.
   */
  clear(): void {
    this.#next = null;
    this.#epoch += 1;
  }

  /** True while a send waits or a value is due. */
  get busy(): boolean {
    return this.#sending || this.#next !== null;
  }

  async #run(): Promise<void> {
    this.#sending = true;
    let failed = false;
    // Whether the last finished send started after the latest `clear()`.
    let owned = false;
    while (this.#next !== null) {
      const { value } = this.#next;
      this.#next = null;
      const epoch = this.#epoch;
      let sendFailed = false;
      try {
        await this.#send(value);
      } catch {
        sendFailed = true;
      }
      owned = epoch === this.#epoch;
      if (owned) failed = sendFailed;
    }
    this.#sending = false;
    if (owned) this.#settled(failed);
  }
}

export const DEVICES_UNSUPPORTED_TEXT = 'Calls are not available on this system yet.';
export const DEVICES_UNREADABLE_TEXT = 'The saved call devices could not be read. Choose them again.';
export const DEVICES_NOTE_TEXT =
  'A microphone or speaker change applies to a running call at once, except on Linux, where it applies from the next call. A camera change applies from the next camera start. The volume changes at once.';

/** The note under the menus, for a device reply. */
export function devicesNote(devices: CallDevices): string {
  if (!devices.supported) return DEVICES_UNSUPPORTED_TEXT;
  return devices.unreadable ? DEVICES_UNREADABLE_TEXT : DEVICES_NOTE_TEXT;
}

// Incoming call rings (ADR-0052). The core sends the room ID, the caller's
// user ID, the intent and the seconds left. The page keeps at most four, shows
// each until it ends and never joins by itself: only the answer buttons do.

/** Most rings the page shows at once. The core keeps no more. */
export const MAX_RINGS = 4;

export const RING_TITLE = 'Incoming call';
export const RING_ANSWER_LABEL = 'Answer';
export const RING_ANSWER_VIDEO_LABEL = 'Answer with video';
export const RING_DECLINE_LABEL = 'Decline';
export const RING_DECLINED_TEXT = 'Declined';

/** A ring and the time, in milliseconds since the epoch, at which it ends. */
export type ShownRing = CallRing & { endsAt: number };

/**
 * The rings the core listed, stamped with their end on this page's clock.
 * At most [`MAX_RINGS`], one for each room, and none without time left.
 * The core's list is the whole truth: a ring not in it has ended.
 */
export function shownRings(list: CallRings, now: number): ShownRing[] {
  const seen = new Set<string>();
  const shown: ShownRing[] = [];
  for (const ring of list.rings) {
    if (shown.length >= MAX_RINGS) break;
    if (seen.has(ring.roomId) || !(ring.secondsLeft >= 1)) continue;
    seen.add(ring.roomId);
    shown.push({ ...ring, endsAt: now + ring.secondsLeft * 1000 });
  }
  return shown;
}

/** The rings that have not ended at `now`. */
export function liveRings(rings: readonly ShownRing[], now: number): ShownRing[] {
  return rings.filter((ring) => ring.endsAt > now);
}

/** Milliseconds until the first ring ends, or `null` for no rings. */
export function nextRingEnd(rings: readonly ShownRing[], now: number): number | null {
  if (rings.length === 0) return null;
  return Math.max(0, Math.min(...rings.map((ring) => ring.endsAt)) - now);
}

/** Whether the devices listing shows a camera. Without a listing, none. */
export function cameraAvailable(devices: CallDevices | null): boolean {
  return devices !== null && devices.supported && devices.cameras.length > 0;
}

export type RingButtons = {
  answer: string;
  // `null` hides the button: no camera is available.
  video: string | null;
  decline: string;
};

/** The buttons of a ring's prompt. Answering with video needs a camera. */
export function ringButtons(camera: boolean): RingButtons {
  return {
    answer: RING_ANSWER_LABEL,
    video: camera ? RING_ANSWER_VIDEO_LABEL : null,
    decline: RING_DECLINE_LABEL
  };
}

/** What a screen reader hears when the rings change; empty for none. */
export function ringsAnnouncement(rings: readonly ShownRing[]): string {
  return rings.length === 0 ? '' : RING_TITLE;
}

/**
 * Which rings are shown: the core's number for each, never the time left.
 * The prompt renders the announcement again when this changes, so a new or
 * a replacing ring is announced although the text names nobody. A new ring
 * from the same caller in the same room has a new number.
 */
export function ringsKey(rings: readonly ShownRing[]): string {
  return rings.map((ring) => ring.ringId).join(',');
}

/**
 * The most devices one levels message names: the contract's
 * `CALL_PARTICIPANTS_MAX`. Its type is that constant's literal, so a change of
 * the contract fails the type check here.
 */
export const MAX_CALL_LEVELS: typeof CALL_PARTICIPANTS_MAX = 32;
/**
 * The longest user ID and device ID, in UTF-8 bytes: the contract's limits,
 * typed as their literals so a change of the contract fails the type check.
 */
const MAX_USER_ID_BYTES: typeof CALL_USER_ID_BYTES_MAX = 255;
const MAX_DEVICE_ID_BYTES: typeof CALL_DEVICE_ID_BYTES_MAX = 255;

const utf8Bytes = (text: string) => new TextEncoder().encode(text).byteLength;

/**
 * The other devices' levels the host sent (ADR-0059), or `null` when the
 * value is anything but a list of at most {@link MAX_CALL_LEVELS} entries with
 * a bounded user ID, device ID and a whole level from 0 to
 * {@link SOUND_LEVEL_MAX}. Extra fields are refused too.
 */
export function parseLevels(value: unknown): CallParticipantLevel[] | null {
  if (!Array.isArray(value) || value.length > MAX_CALL_LEVELS) return null;
  const levels: CallParticipantLevel[] = [];
  for (const entry of value) {
    if (typeof entry !== 'object' || entry === null) return null;
    const keys = Object.keys(entry);
    if (keys.length !== 3) return null;
    const { userId, deviceId, level } = entry as Record<string, unknown>;
    if (typeof userId !== 'string' || typeof deviceId !== 'string') return null;
    if (utf8Bytes(userId) > MAX_USER_ID_BYTES || utf8Bytes(deviceId) > MAX_DEVICE_ID_BYTES) return null;
    if (parseSoundLevel(level) === null) return null;
    levels.push({ userId, deviceId, level: level as number });
  }
  return levels;
}

/** The key of one device in a call, for level lookups. */
export function deviceKey(userId: string, deviceId: string): string {
  return JSON.stringify([userId, deviceId]);
}

/**
 * The newest level of each device. Each message names the devices that are
 * not silent, so a device it leaves out is silent now.
 */
export function levelMap(levels: readonly CallParticipantLevel[]): Map<string, number> {
  return new Map(levels.map((entry) => [deviceKey(entry.userId, entry.deviceId), entry.level]));
}

/** Read beside a participant whose microphone is off (ADR-0059). */
export const PARTICIPANT_MUTED_LABEL = 'Microphone off';

/** What a failed device change in a call means (ADR-0059). */
export const DEVICE_CHANGE_TEXT: Record<CallDeviceChange, string> = {
  not_found: 'The chosen microphone or speaker is not connected. The call keeps the one it had.',
  unsupported: 'This change cannot be made during a call on this system. It applies to the next call.',
  failed: 'The microphone or speaker could not be changed, and the sound may have stopped. Choose a device again, or leave and rejoin the call.'
};

/** The buttons that start each device test. */
export const DEVICE_TEST_LABEL: Record<CallDeviceTestKind, string> = {
  microphone: 'Test microphone',
  speaker: 'Test speaker',
  camera: 'Test camera'
};
export const DEVICE_TEST_STOP_LABEL = 'Stop test';

/** What a running test asks of the user. */
const RUNNING_TEXT: Record<CallDeviceTestKind, string> = {
  microphone: 'Speak now. The meter moves when Persephone hears your microphone.',
  speaker: 'Playing a tone. You should hear it now.',
  camera: 'The camera picture shows below when Persephone can use the camera.'
};

/** What an ended test found. */
const DONE_TEXT: Record<CallDeviceTestKind, string> = {
  microphone: 'The microphone test ended. If the meter did not move, Persephone did not hear the microphone.',
  speaker: 'The tone played. If you heard nothing, check the speaker and its volume.',
  camera: 'The camera test ended.'
};

/** The test's state as text, for the states every test shares. */
const STATE_TEXT: Record<Exclude<CallDeviceTestState, 'running' | 'done'>, string> = {
  idle: '',
  starting: 'Starting the test…',
  not_found: 'The chosen device is not connected.',
  denied: 'Your system did not allow Persephone to use this device. Check its privacy settings.',
  unsupported: 'This test is not available on this system yet.',
  unavailable: 'The device could not start. Another app may be using it.',
  in_call: 'Leave the call to test a device.',
  failed: 'The test could not run.'
};

/**
 * The camera's `unavailable` text. The camera path reports one state for an
 * app that holds the camera and for the system's privacy settings, so the
 * text names both (ADR-0059).
 */
const CAMERA_TEST_UNAVAILABLE_TEXT =
  'The camera could not start. Another app may be using it, or your system’s privacy settings may block it.';

/**
 * The text beside the `kind` test's button, or '' when that test is not
 * the newest one.
 */
export function deviceTestText(status: CallDeviceTestStatus, kind: CallDeviceTestKind): string {
  if (status.test !== kind) return '';
  if (status.state === 'running') return RUNNING_TEXT[kind];
  if (status.state === 'done') return DONE_TEXT[kind];
  if (kind === 'camera' && status.state === 'unavailable') return CAMERA_TEST_UNAVAILABLE_TEXT;
  return STATE_TEXT[status.state];
}

/** Whether the test runs or starts, so its button stops it. */
export function deviceTestActive(status: CallDeviceTestStatus, kind: CallDeviceTestKind): boolean {
  return status.test === kind && (status.state === 'starting' || status.state === 'running');
}

/** No device test yet. */
export const IDLE_DEVICE_TEST: CallDeviceTestStatus = { id: 0, test: null, state: 'idle', stream: 0 };

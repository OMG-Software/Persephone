import type { CallDevices } from '../../crates/contracts/generated/contracts';
import {
  DEVICE_FAILURE_TEXT,
  DEVICE_SAVE_FAILURE_TEXT,
  EFFECT_SAVE_TEXT,
  deviceChoice,
  deviceValue,
  plainEffect,
  withVideoEffectChange,
  type VideoEffectChange,
  type VideoEffectPart
} from './calls.ts';

/** The parts of the video effect controls that a change may send. */
const EFFECT_PARTS: readonly VideoEffectPart[] = ['effect', 'mirror'];

/** The three menus of the device panel; `''` is the system default. */
export interface DeviceMenus {
  recording: string;
  playout: string;
  camera: string;
}

/** What the device panel shows besides the menus. */
export interface DevicePanelState {
  devices: CallDevices | null;
  loading: boolean;
  saving: boolean;
  saved: boolean;
  failure: string | null;
}

/**
 * What the panel shows about saving the video effect (ADR-0068). A refusal
 * by the worker is not here: it comes after the reply, in the call status.
 */
export interface EffectState {
  saving: boolean;
  failure: string | null;
}

/** The bridge calls the panel uses; the component passes the real ones. */
export interface DevicePanelBridge {
  list: () => Promise<CallDevices>;
  save: (recording: string | null, playout: string | null, camera: string | null) => Promise<unknown>;
  effect: (change: VideoEffectChange) => Promise<unknown>;
}

const failureText = (reason: unknown, fallback: string) => DEVICE_FAILURE_TEXT[String(reason)] ?? fallback;

/**
 * The work of the call window's device panel (ADR-0065), apart from the
 * view so its order rules have tests.
 *
 * - A load reads the lists and the saved choices. It does not start while a
 *   load or a save runs, because it would replace the menus under them.
 * - Saves go to the core one at a time, in the order asked, so a menu
 *   changed while a save runs is sent after it and none is dropped. Each
 *   save sends the three menus as they were when it was asked, so the last
 *   one holds every change. Only the newest save sets the result line.
 * - The video effect (ADR-0068) goes into the same queue, as each save
 *   changes the same device record. Each change sends only the part the
 *   user changed, the effect or the mirror, so this panel's copy of the
 *   other part, which the main window may have changed, is never sent.
 *   The newest answer for each part sets that part's result, also when a
 *   change of the other part was sent after it: a failure shows until that
 *   part is sent again, and that part's controls go back to the saved
 *   value, as in the main window. The reply comes
 *   before the worker has answered, so whether the worker refused the
 *   effect is read from the call status, not from the reply.
 * - After `close`, no answer changes the panel.
 */
export class DevicePanel {
  readonly #bridge: DevicePanelBridge;
  readonly #onstate: (state: DevicePanelState) => void;
  readonly #onmenus: (menus: DeviceMenus) => void;
  readonly #oneffect: (state: EffectState) => void;
  readonly #oncontrols: (saved: VideoEffectChange) => void;
  #state: DevicePanelState = { devices: null, loading: false, saving: false, saved: false, failure: null };
  #effect: EffectState = { saving: false, failure: null };
  #queue: Promise<unknown> = Promise.resolve();
  #saves = 0;
  #newest = 0;
  #effectSaves = 0;
  #effectRequests = 0;
  /** The newest effect request for each part, and its failure. */
  #newestPart: Record<VideoEffectPart, number> = { effect: 0, mirror: 0 };
  #partFailure: Record<VideoEffectPart, string | null> = { effect: null, mirror: null };
  #open = true;

  constructor(
    bridge: DevicePanelBridge,
    onstate: (state: DevicePanelState) => void,
    onmenus: (menus: DeviceMenus) => void,
    oneffect: (state: EffectState) => void = () => {},
    oncontrols: (saved: VideoEffectChange) => void = () => {}
  ) {
    this.#bridge = bridge;
    this.#onstate = onstate;
    this.#onmenus = onmenus;
    this.#oneffect = oneffect;
    this.#oncontrols = oncontrols;
  }

  get state(): DevicePanelState {
    return this.#state;
  }

  get effect(): EffectState {
    return this.#effect;
  }

  #setEffect(saving: boolean) {
    this.#effect = { saving, failure: this.#partFailure.effect ?? this.#partFailure.mirror };
    this.#oneffect(this.#effect);
  }

  #set(change: Partial<DevicePanelState>) {
    this.#state = { ...this.#state, ...change };
    this.#onstate(this.#state);
  }

  async load(): Promise<void> {
    if (!this.#open || this.#state.loading || this.#saves > 0 || this.#effectSaves > 0) return;
    this.#set({ loading: true, failure: null });
    try {
      const devices = await this.#bridge.list();
      if (!this.#open) return;
      this.#set({ devices, loading: false });
      this.#onmenus({
        recording: deviceValue(devices.selectedRecording),
        playout: deviceValue(devices.selectedPlayout),
        camera: deviceValue(devices.selectedCamera)
      });
    } catch (reason) {
      if (this.#open) this.#set({ loading: false, failure: failureText(reason, DEVICE_FAILURE_TEXT.core_unavailable) });
    }
  }

  async save(menus: DeviceMenus): Promise<void> {
    if (!this.#open) return;
    const mine = ++this.#newest;
    this.#saves += 1;
    this.#set({ saving: true, saved: false, failure: null });
    const choice = [deviceChoice(menus.recording), deviceChoice(menus.playout), deviceChoice(menus.camera)] as const;
    const run = () => this.#bridge.save(...choice);
    const turn = this.#queue.then(run, run);
    this.#queue = turn.catch(() => undefined);
    let failure: string | null = null;
    try {
      await turn;
    } catch (reason) {
      failure = failureText(reason, DEVICE_SAVE_FAILURE_TEXT);
    }
    this.#saves -= 1;
    if (!this.#open) return;
    if (mine === this.#newest) {
      this.#set({ saving: this.#saves > 0, saved: failure === null, failure });
    } else {
      this.#set({ saving: this.#saves > 0 });
    }
  }

  /**
   * Sends one change of the video effect controls (ADR-0068), the effect,
   * the mirror or both, after the saves before it. A change with neither
   * sends nothing. A saved change goes into the panel's copy of the record.
   * When the newest change of a part fails, the controls of that part go
   * back to that copy (`oncontrols`, with only those parts).
   */
  async saveEffect(change: VideoEffectChange): Promise<void> {
    if (!this.#open) return;
    const wanted: VideoEffectChange = {};
    if (change.effect) wanted.effect = plainEffect(change.effect);
    if (typeof change.mirror === 'boolean') wanted.mirror = change.mirror;
    const parts = EFFECT_PARTS.filter((part) => wanted[part] !== undefined);
    if (parts.length === 0) return;
    const mine = ++this.#effectRequests;
    for (const part of parts) {
      this.#newestPart[part] = mine;
      this.#partFailure[part] = null;
    }
    this.#effectSaves += 1;
    this.#setEffect(true);
    const run = () => this.#bridge.effect(wanted);
    const turn = this.#queue.then(run, run);
    this.#queue = turn.catch(() => undefined);
    let failure: string | null = null;
    try {
      await turn;
      const devices = this.#state.devices;
      if (this.#open && devices) this.#set({ devices: withVideoEffectChange(devices, wanted) });
    } catch (reason) {
      failure = failureText(reason, EFFECT_SAVE_TEXT);
    }
    this.#effectSaves -= 1;
    if (!this.#open) return;
    // Only the parts that no later change sent take this answer.
    const newest = parts.filter((part) => this.#newestPart[part] === mine);
    for (const part of newest) this.#partFailure[part] = failure;
    this.#setEffect(this.#effectSaves > 0);
    const saved = this.#state.devices;
    if (failure === null || !saved || newest.length === 0) return;
    const back: VideoEffectChange = {};
    if (newest.includes('effect')) back.effect = saved.effect;
    if (newest.includes('mirror')) back.mirror = saved.mirror;
    this.#oncontrols(back);
  }

  close(): void {
    this.#open = false;
  }
}

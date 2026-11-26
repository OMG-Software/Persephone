import type { CallDevices, CallVideoEffect } from '../../crates/contracts/generated/contracts';
import {
  DEVICE_FAILURE_TEXT,
  DEVICE_SAVE_FAILURE_TEXT,
  EFFECT_SAVE_TEXT,
  deviceChoice,
  deviceValue,
  plainEffect,
  withVideoEffectChange,
  type VideoEffectChange
} from './calls.ts';

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
 *   other part, which the main window may have changed, is never sent. Its
 *   newest answer sets its own result; when that one failed, the controls
 *   go back to the saved values, as in the main window. The reply comes
 *   before the worker has answered, so whether the worker refused the
 *   effect is read from the call status, not from the reply.
 * - After `close`, no answer changes the panel.
 */
export class DevicePanel {
  readonly #bridge: DevicePanelBridge;
  readonly #onstate: (state: DevicePanelState) => void;
  readonly #onmenus: (menus: DeviceMenus) => void;
  readonly #oneffect: (state: EffectState) => void;
  readonly #oncontrols: (effect: CallVideoEffect, mirror: boolean) => void;
  #state: DevicePanelState = { devices: null, loading: false, saving: false, saved: false, failure: null };
  #effect: EffectState = { saving: false, failure: null };
  #queue: Promise<unknown> = Promise.resolve();
  #saves = 0;
  #newest = 0;
  #effectSaves = 0;
  #newestEffect = 0;
  #open = true;

  constructor(
    bridge: DevicePanelBridge,
    onstate: (state: DevicePanelState) => void,
    onmenus: (menus: DeviceMenus) => void,
    oneffect: (state: EffectState) => void = () => {},
    oncontrols: (effect: CallVideoEffect, mirror: boolean) => void = () => {}
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

  #setEffect(change: Partial<EffectState>) {
    this.#effect = { ...this.#effect, ...change };
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
   * Sends one change of the video effect controls (ADR-0068), the effect or
   * the mirror, after the saves before it. A saved change goes into the
   * panel's copy of the record. When the newest change fails, the controls
   * go back to that copy (`oncontrols`).
   */
  async saveEffect(change: VideoEffectChange): Promise<void> {
    if (!this.#open) return;
    const mine = ++this.#newestEffect;
    this.#effectSaves += 1;
    this.#setEffect({ saving: true, failure: null });
    const wanted: VideoEffectChange = {};
    if (change.effect) wanted.effect = plainEffect(change.effect);
    if (typeof change.mirror === 'boolean') wanted.mirror = change.mirror;
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
    if (mine === this.#newestEffect) {
      this.#setEffect({ saving: this.#effectSaves > 0, failure });
      const saved = this.#state.devices;
      if (failure !== null && saved) this.#oncontrols(saved.effect, saved.mirror);
    } else {
      this.#setEffect({ saving: this.#effectSaves > 0 });
    }
  }

  close(): void {
    this.#open = false;
  }
}

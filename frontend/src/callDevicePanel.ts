import type { CallDevices } from '../../crates/contracts/generated/contracts';
import { DEVICE_FAILURE_TEXT, DEVICE_SAVE_FAILURE_TEXT, deviceChoice, deviceValue } from './calls.ts';

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

/** The bridge calls the panel uses; the component passes the real ones. */
export interface DevicePanelBridge {
  list: () => Promise<CallDevices>;
  save: (recording: string | null, playout: string | null, camera: string | null) => Promise<unknown>;
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
 * - After `close`, no answer changes the panel.
 */
export class DevicePanel {
  readonly #bridge: DevicePanelBridge;
  readonly #onstate: (state: DevicePanelState) => void;
  readonly #onmenus: (menus: DeviceMenus) => void;
  #state: DevicePanelState = { devices: null, loading: false, saving: false, saved: false, failure: null };
  #queue: Promise<unknown> = Promise.resolve();
  #saves = 0;
  #newest = 0;
  #open = true;

  constructor(
    bridge: DevicePanelBridge,
    onstate: (state: DevicePanelState) => void,
    onmenus: (menus: DeviceMenus) => void
  ) {
    this.#bridge = bridge;
    this.#onstate = onstate;
    this.#onmenus = onmenus;
  }

  get state(): DevicePanelState {
    return this.#state;
  }

  #set(change: Partial<DevicePanelState>) {
    this.#state = { ...this.#state, ...change };
    this.#onstate(this.#state);
  }

  async load(): Promise<void> {
    if (!this.#open || this.#state.loading || this.#saves > 0) return;
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

  close(): void {
    this.#open = false;
  }
}

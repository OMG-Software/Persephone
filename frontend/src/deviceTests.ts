// The device tests' request rules (ADR-0059), apart from the settings page
// so they can be tested without a window. The core runs one test at a time
// in a contained worker; this page only asks for it, reads its status and
// stops it.
import type {
  CallDeviceTestKind,
  CallDeviceTestStatus,
  CallStatus
} from '../../crates/contracts/generated/contracts';
import { deviceTestActive } from './calls.ts';

/** The core's device test commands, as the bridge gives them. */
export type DeviceTestBridge = {
  start: (kind: CallDeviceTestKind, device: string | null) => Promise<CallDeviceTestStatus>;
  stop: (id?: number | null) => Promise<void>;
  status: () => Promise<CallDeviceTestStatus>;
};

/** Whether a test holds a device: it starts or runs. */
export function testLive(status: CallDeviceTestStatus): boolean {
  return status.state === 'starting' || status.state === 'running';
}

/** The microphone test's meter: the own level only while that test runs. */
export function testLevel(status: CallDeviceTestStatus, soundLevel: number): number {
  return deviceTestActive(status, 'microphone') ? soundLevel : 0;
}

/** The camera test's preview: its tile only while the camera runs. */
export function testTiles(status: CallDeviceTestStatus): CallStatus['video'] {
  return status.test === 'camera' && status.state === 'running'
    ? [{ tile: 0, participant: null, own: true, stream: status.stream }]
    : [];
}

/**
 * Starts, stops and reads the device test for one settings page. Each
 * request has a number, so an older answer never replaces a newer one. When
 * the page closes, a test that runs is stopped, and a start that answers
 * later is stopped too: only that test, by its ID, so a test of a reopened
 * page keeps running.
 */
export class DeviceTests {
  #request = 0;
  #closed = false;
  #shown: CallDeviceTestStatus;
  readonly #bridge: DeviceTestBridge;
  readonly #show: (status: CallDeviceTestStatus) => void;

  constructor(bridge: DeviceTestBridge, show: (status: CallDeviceTestStatus) => void, shown: CallDeviceTestStatus) {
    this.#bridge = bridge;
    this.#show = show;
    this.#shown = shown;
  }

  #take(mine: number, next: CallDeviceTestStatus) {
    if (mine !== this.#request || this.#closed) return;
    this.#shown = next;
    this.#show(next);
  }

  /** Reads the newest status; a failed read keeps what is shown. */
  async read() {
    const mine = ++this.#request;
    try {
      this.#take(mine, await this.#bridge.status());
    } catch {
      // The next change reads it again.
    }
  }

  /**
   * Stops the `kind` test when it runs, else starts it on `device`. A
   * refusal rejects with the core's reason.
   */
  async run(kind: CallDeviceTestKind, device: string | null) {
    const mine = ++this.#request;
    if (deviceTestActive(this.#shown, kind)) {
      await this.#bridge.stop();
      return;
    }
    const next = await this.#bridge.start(kind, device);
    if (this.#closed) {
      if (testLive(next)) void this.#bridge.stop(next.id).catch(() => {});
      return;
    }
    this.#take(mine, next);
  }

  /** The page closes: the test it shows stops, by its ID. */
  close() {
    this.#closed = true;
    if (testLive(this.#shown)) void this.#bridge.stop(this.#shown.id).catch(() => {});
  }
}

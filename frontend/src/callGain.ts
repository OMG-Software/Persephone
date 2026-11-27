// Sends the microphone gain of a call (ADR-0060). The host runs each command
// on its own task, so two sends that overlap can arrive in another order, and
// the core keeps the one that arrives last. This sender keeps one send in
// flight and sends only the newest value after it, so the last value the
// viewer chose is the last value the core hears.
import { LatestOnly } from './calls.ts';

export class GainSender<R> {
  #latest = 0;
  #only: LatestOnly<number>;

  /**
   * `call` sends one gain and returns the reply. `onReply` gets the reply and
   * whether its gain is still the newest chosen one. `settled` runs after the
   * newest send, and says whether it failed.
   */
  constructor(
    call: (gain: number) => Promise<R>,
    onReply: (reply: R, newest: boolean) => void,
    settled: (failed: boolean) => void
  ) {
    this.#only = new LatestOnly<number>(async (gain) => {
      const reply = await call(gain);
      onReply(reply, gain === this.#latest);
    }, settled);
  }

  /** Chooses `gain`: it goes now, or after the send that waits. */
  set(gain: number): void {
    this.#latest = gain;
    this.#only.set(gain);
  }
}

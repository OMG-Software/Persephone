// Typing notices this account sends. Pure logic: the caller passes the
// sender and the clock.

/** How often a still-typing notice is sent again while typing goes on. */
export const TYPING_RESEND_MS = 4000;
/** How long without a key press before typing counts as stopped. */
export const TYPING_IDLE_MS = 5000;

type Timer = ReturnType<typeof setTimeout>;

/**
 * Sends "typing" when typing starts and again every `TYPING_RESEND_MS`
 * while it goes on, and "stopped" after `TYPING_IDLE_MS` without input or
 * when the message is sent. Only create one while the account's setting
 * sends typing notices (ADR-0023). Send failures are the sender's to drop.
 */
export class TypingNotifier {
  readonly #send: (typing: boolean) => void;
  readonly #now: () => number;
  readonly #schedule: (run: () => void, ms: number) => Timer;
  readonly #cancel: (timer: Timer) => void;
  #lastSent = -Infinity;
  #idle: Timer | null = null;

  constructor(
    send: (typing: boolean) => void,
    now: () => number = Date.now,
    schedule: (run: () => void, ms: number) => Timer = setTimeout,
    cancel: (timer: Timer) => void = clearTimeout
  ) {
    this.#send = send;
    this.#now = now;
    this.#schedule = schedule;
    this.#cancel = cancel;
  }

  /** The user typed. */
  input() {
    const now = this.#now();
    if (now - this.#lastSent >= TYPING_RESEND_MS) {
      this.#lastSent = now;
      this.#send(true);
    }
    if (this.#idle !== null) this.#cancel(this.#idle);
    this.#idle = this.#schedule(() => this.stop(), TYPING_IDLE_MS);
  }

  /** Typing ended: the message was sent, the draft emptied or the room left. */
  stop() {
    if (this.#idle !== null) {
      this.#cancel(this.#idle);
      this.#idle = null;
    }
    if (this.#lastSent !== -Infinity) {
      this.#lastSent = -Infinity;
      this.#send(false);
    }
  }
}

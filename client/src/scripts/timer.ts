// Voice-command countdown timer — UI side (spec 0052).
//
// Hands-free session timer set by speaking, e.g. "imposta timer di 10 minuti" or
// "set a 5 minute timer". The clever part: we DON'T add a second STT/Whisper
// pipeline. The local speaker's own speech is already transcribed by Deepgram and
// arrives over the call WS as a `subtitle_final` for `speaker_id === myId`. app.ts
// feeds that final text to `handleTranscript` here — zero extra latency, zero
// extra cost — and a matching command starts a countdown badge on the stage.
//
// The intent parser + formatters live in ./timer-intent (pure, unit-tested); this
// module owns the DOM + the countdown clock.

import { formatClock, parseTimerCommand, type TimerCommand } from './timer-intent';

export { formatClock, parseTimerCommand, spokenDuration, type TimerCommand } from './timer-intent';

export interface CallTimerDeps {
  /** The badge overlay element (toggled .hidden). */
  badge: HTMLElement;
  /** Element whose textContent is the live MM:SS label. */
  remaining: HTMLElement;
  /** The × button that cancels a running timer. */
  cancelBtn: HTMLElement;
  t: (k: string) => string;
  /** Fired when a timer starts (manual or voice) — wire confirmation feedback
   *  (toast / sound / spoken). */
  onSet: (cmd: TimerCommand) => void;
  /** Fired when the countdown reaches zero. */
  onDone: (cmd: TimerCommand) => void;
  /** Fired when the user cancels a running timer (× or manual). */
  onCancel?: () => void;
}

/** Owns the countdown state + badge. Drives a 250 ms tick so the label stays in
 *  step with the wall clock even if a tick is delayed (tab throttling). */
export class CallTimer {
  private deadline = 0;
  private current: TimerCommand | null = null;
  private tickId = 0;
  private hideId = 0;

  constructor(private deps: CallTimerDeps) {
    deps.cancelBtn.addEventListener('click', () => this.cancel());
  }

  /** True while a countdown is running (excludes the post-done "Time's up" hold). */
  get active(): boolean {
    return this.tickId !== 0;
  }

  /** Try to read a final local transcript as a timer command. Returns true (and
   *  starts the timer) when it matches. */
  handleTranscript(text: string): boolean {
    const cmd = parseTimerCommand(text);
    if (!cmd) return false;
    this.start(cmd);
    return true;
  }

  /** Start (or restart) a countdown for `cmd`. */
  start(cmd: TimerCommand): void {
    this.clearTimers();
    this.current = cmd;
    this.deadline = Date.now() + cmd.seconds * 1000;
    this.deps.badge.classList.remove('hidden', 'timer-badge-done');
    this.render();
    this.tickId = window.setInterval(() => this.tick(), 250);
    this.deps.onSet(cmd);
  }

  /** Stop a running countdown and hide the badge. No-op past the done hold. */
  cancel(): void {
    if (!this.active) return;
    this.clearTimers();
    this.current = null;
    this.deps.badge.classList.add('hidden');
    this.deps.onCancel?.();
  }

  /** Full teardown for leaving the call: stop everything, hide the badge. */
  reset(): void {
    this.clearTimers();
    this.current = null;
    this.deps.badge.classList.add('hidden');
    this.deps.badge.classList.remove('timer-badge-done');
  }

  private remainingSeconds(): number {
    return Math.max(0, (this.deadline - Date.now()) / 1000);
  }

  private tick(): void {
    if (this.remainingSeconds() <= 0) {
      this.fire();
      return;
    }
    this.render();
  }

  private render(): void {
    this.deps.remaining.textContent = formatClock(this.remainingSeconds());
  }

  /** Countdown hit zero: announce, flash the badge, auto-hide after a short hold. */
  private fire(): void {
    const cmd = this.current;
    window.clearInterval(this.tickId);
    this.tickId = 0;
    this.deps.remaining.textContent = formatClock(0);
    this.deps.badge.classList.add('timer-badge-done');
    if (cmd) this.deps.onDone(cmd);
    this.current = null;
    this.hideId = window.setTimeout(() => {
      this.deps.badge.classList.add('hidden');
      this.deps.badge.classList.remove('timer-badge-done');
    }, 6000);
  }

  private clearTimers(): void {
    if (this.tickId) {
      window.clearInterval(this.tickId);
      this.tickId = 0;
    }
    if (this.hideId) {
      window.clearTimeout(this.hideId);
      this.hideId = 0;
    }
  }
}

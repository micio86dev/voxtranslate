// Runtime health monitor for Vox Voices. The benchmark is a one-time snapshot; this
// watches ACTUAL playback startup during a session. If Vox's time-to-first-audio
// exceeds the configured threshold for MAX_STRIKES consecutive utterances, we degrade
// to Browser Voice — for THIS SESSION ONLY. Vox is retried next session (reset()); it
// is never permanently disabled because of a temporary slowdown. Pure & unit-tested.

import { TTS_CONFIG } from './config';

export class HealthMonitor {
  private strikes = 0;
  private degraded = false;

  constructor(
    private startupMs: number = TTS_CONFIG.HEALTH_STARTUP_MS,
    private maxStrikes: number = TTS_CONFIG.HEALTH_MAX_STRIKES,
  ) {}

  /** Record one Vox utterance's startup latency (ms). Returns true IFF this call
   *  transitioned into the degraded state, so the caller notifies exactly once. A
   *  healthy utterance clears the streak, so only a SUSTAINED slowdown degrades. */
  record(ms: number): boolean {
    if (this.degraded) return false;
    if (ms > this.startupMs) {
      this.strikes++;
      if (this.strikes >= this.maxStrikes) {
        this.degraded = true;
        return true;
      }
    } else {
      this.strikes = 0;
    }
    return false;
  }

  /** Force degrade (e.g. a failed benchmark). Returns true if it was a transition. */
  forceDegrade(): boolean {
    const was = this.degraded;
    this.degraded = true;
    return !was;
  }

  isDegraded(): boolean {
    return this.degraded;
  }

  /** New session: clear the streak and degrade so Vox is tried again. */
  reset(): void {
    this.strikes = 0;
    this.degraded = false;
  }
}

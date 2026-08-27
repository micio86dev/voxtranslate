// Stopping the phone from translating itself (spec 0110, brief §9 and §44).
//
// Talk to Anyone plays translated speech out of the same device that is listening. Left
// alone that is a loop with no natural end:
//
//   user speaks Italian → Spanish plays → the mic hears Spanish → Italian plays →
//   the mic hears Italian → …
//
// Four layers stop it, in order of preference:
//
//  1. `echoCancellation` on the capture track. Browser AEC already covers Web Audio
//     output to `ctx.destination`, which is exactly how translated audio is played, so
//     on a phone held normally most of the echo never reaches us. It is not enough on
//     speakerphone, and it is much weaker over Bluetooth.
//  2. The ECHO DIRECTION is dropped server-side before it is ever sent (see
//     `server/src/talk/utterance.rs`), so the user never hears their own words back.
//  3. This guard: while translated audio is audible, microphone frames are held. Driven
//     by playback EDGES from the audio worklet — never a timer. The brief is explicit
//     (§44): a fixed delay after every response is not an acceptable answer, because the
//     product target is minimum latency. The gate closes when speech starts and opens the
//     instant it stops.
//  4. Barge-in: a real person talking over the translation is louder than AEC residue,
//     so sustained energy while gated cancels the playback and reopens the microphone.
//
// `mode` is the seam for full duplex. `'gate'` is the default and is honest about what
// today's devices can do; `'open'` trusts AEC and never gates, which is the switch to
// flip once echo-cancellation quality has been measured per device class. Nothing else
// has to change.

import { rmsLevel } from '../mic-meter';

/**
 * RMS above which we believe a human, not echo residue, is speaking.
 *
 * Normal speech sits around 0.05–0.3 (see `mic-meter.ts`), and `echoCancellation` is on,
 * so what is left of our own playback in the captured signal is far below this. Lowered
 * from 0.08: while the gate is shut the microphone is DROPPING frames, so every
 * millisecond spent deciding is speech nobody gets back.
 */
export const BARGE_IN_RMS = 0.06;

/**
 * How long that energy must persist before it counts. A cough is not an interruption.
 *
 * Halved from 250 ms for the same reason: at a 50 ms sample interval the old window cost
 * up to 300 ms — reliably the first word of whoever cut in. 150 ms is still three
 * consecutive samples, which rhythmic playback peaks do not produce (they dip between
 * syllables and the run resets).
 */
export const BARGE_IN_MS = 150;

/** How often the level is sampled while gated. */
export const LEVEL_SAMPLE_MS = 50;

/**
 * `'gate'` holds microphone frames while translated audio plays (the default, and what
 * today's echo cancellation actually supports). `'open'` never gates — full duplex,
 * trusting AEC.
 */
export type GuardMode = 'gate' | 'open';

export interface GuardDeps {
  /** Hold or release microphone frames — `PcmCapture.setGated`. */
  setGated: (gated: boolean) => void;
  /** Drop queued translated audio — `pcmPlayback.reset()`. */
  cancelPlayback: () => void;
  /** Called when a human interrupts, so the UI and analytics can react. */
  onBargeIn?: () => void;
  /** Injected for determinism in tests. */
  now?: () => number;
}

export class SelfAudioGuard {
  private mode: GuardMode = 'gate';
  private playing = false;
  private gated = false;
  /** When the current run of loud-enough input began, or null if it is not loud. */
  private loudSince: number | null = null;
  private readonly now: () => number;

  /** Diagnostics, surfaced through `stats()` rather than logged per event. */
  private gateCount = 0;
  private bargeCount = 0;

  constructor(private readonly deps: GuardDeps) {
    this.now = deps.now ?? (() => Date.now());
  }

  setMode(mode: GuardMode): void {
    this.mode = mode;
    // Switching to full duplex must release an already-closed gate, or the microphone
    // stays shut until the current utterance happens to end.
    if (mode === 'open') this.release();
  }

  getMode(): GuardMode {
    return this.mode;
  }

  isGated(): boolean {
    return this.gated;
  }

  /** Feed the playback edges from `PcmPlayback.setPlayingListener`. */
  onPlaybackChange(playing: boolean): void {
    this.playing = playing;
    if (!playing) {
      // The voice finished. Open immediately — waiting even a little here is how the
      // first syllable of the reply goes missing.
      this.release();
      return;
    }
    if (this.mode === 'gate') this.close();
  }

  /**
   * Feed a 0..1 RMS level. Only consulted while gated: when the microphone is already
   * flowing there is nothing to interrupt.
   */
  onLevel(level: number): void {
    if (!this.gated || !this.playing) {
      this.loudSince = null;
      return;
    }
    if (level < BARGE_IN_RMS) {
      // The run has to be CONTINUOUS. Resetting here is what stops the rhythmic peaks
      // of the translation's own audio from adding up to a false interruption.
      this.loudSince = null;
      return;
    }
    const now = this.now();
    if (this.loudSince === null) {
      this.loudSince = now;
      return;
    }
    if (now - this.loudSince < BARGE_IN_MS) return;

    // Someone is talking over the translation. Their turn wins: drop what is queued so
    // an obsolete sentence cannot keep playing over a live one.
    this.bargeCount += 1;
    this.loudSince = null;
    this.deps.cancelPlayback();
    this.release();
    this.deps.onBargeIn?.();
  }

  /** Release everything. Safe to call more than once. */
  reset(): void {
    this.playing = false;
    this.loudSince = null;
    this.release();
  }

  stats(): { gateCount: number; bargeCount: number } {
    return { gateCount: this.gateCount, bargeCount: this.bargeCount };
  }

  private close(): void {
    if (this.gated) return;
    this.gated = true;
    this.gateCount += 1;
    this.deps.setGated(true);
  }

  private release(): void {
    this.loudSince = null;
    if (!this.gated) return;
    this.gated = false;
    this.deps.setGated(false);
  }
}

/** The pieces of `AnalyserNode` the monitor needs, so tests can supply a fake. */
export interface LevelSource {
  // `Uint8Array<ArrayBuffer>` rather than a bare `Uint8Array`: that is the signature
  // `AnalyserNode` declares, and the looser one makes a real analyser unassignable here.
  getByteTimeDomainData: (buf: Uint8Array<ArrayBuffer>) => void;
  fftSize: number;
}

/**
 * Sample `source` every [`LEVEL_SAMPLE_MS`] and report the RMS.
 *
 * Deliberately `setInterval` and NOT `requestAnimationFrame` (which is what
 * `MicMeter` uses for its level halo): rAF is throttled or stopped outright in a
 * backgrounded tab, and barge-in detection that quietly dies when the screen dims is
 * worse than none — the microphone would stay gated with nothing left to reopen it.
 *
 * Returns a stop function.
 */
export function startLevelMonitor(
  source: LevelSource,
  onLevel: (level: number) => void,
  setIntervalImpl: typeof setInterval = setInterval,
  clearIntervalImpl: typeof clearInterval = clearInterval,
): () => void {
  const buf = new Uint8Array(new ArrayBuffer(source.fftSize));
  const id = setIntervalImpl(() => {
    source.getByteTimeDomainData(buf);
    onLevel(rmsLevel(buf));
  }, LEVEL_SAMPLE_MS);
  return () => clearIntervalImpl(id);
}

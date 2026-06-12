// Synthesised UI sound cues (Web Audio). No audio asset files: the cues are
// short tones generated on the fly, so the bundle stays lean and the sounds are
// identical across browsers. One shared AudioContext, lazily created on first
// use — the call screen is only reached after a click (Join), so the autoplay
// policy is already satisfied and the context can resume.

let ctx: AudioContext | null = null;
let enabled = true;

/** Lazily create / resume the shared AudioContext. Returns null when Web Audio
 *  is unavailable (e.g. the node test environment). */
function audioCtx(): AudioContext | null {
  const Ctor =
    (globalThis as any).AudioContext ?? (globalThis as any).webkitAudioContext;
  if (!Ctor) return null;
  if (!ctx) ctx = new Ctor();
  // The autoplay policy can leave a context suspended until a gesture resumes
  // it; resuming is a no-op when it is already running.
  if (ctx && ctx.state === 'suspended') void ctx.resume().catch(() => {});
  return ctx;
}

/** Mute / unmute every UI sound cue (single switch for a future preference). */
export function setSfxEnabled(on: boolean): void {
  enabled = on;
}

interface Note {
  freq: number; // pitch in Hz
  start: number; // offset from "now", seconds
  dur: number; // tone length, seconds
}

interface ToneOpts {
  type?: OscillatorType;
  gain?: number; // peak gain (kept low so the cues stay subtle)
}

function play(notes: Note[], { type = 'sine', gain = 0.06 }: ToneOpts = {}): void {
  if (!enabled) return;
  const ac = audioCtx();
  if (!ac) return;
  const now = ac.currentTime;
  for (const n of notes) {
    const osc = ac.createOscillator();
    const g = ac.createGain();
    osc.type = type;
    osc.frequency.value = n.freq;
    const t0 = now + n.start;
    const t1 = t0 + n.dur;
    // Click-free envelope: quick exponential attack, exponential decay to near
    // silence (exponential ramps cannot reach exactly 0).
    g.gain.setValueAtTime(0.0001, t0);
    g.gain.exponentialRampToValueAtTime(gain, t0 + 0.012);
    g.gain.exponentialRampToValueAtTime(0.0001, t1);
    osc.connect(g).connect(ac.destination);
    osc.start(t0);
    osc.stop(t1 + 0.02);
  }
}

/** A peer joined the room: gentle two-note rising chime. */
export function playJoinSound(): void {
  play([
    { freq: 587.33, start: 0, dur: 0.14 }, // D5
    { freq: 880.0, start: 0.1, dur: 0.18 }, // A5
  ]);
}

/** A peer left the room: gentle two-note falling chime — the inverse of join. */
export function playLeaveSound(): void {
  play([
    { freq: 880.0, start: 0, dur: 0.14 }, // A5
    { freq: 587.33, start: 0.1, dur: 0.18 }, // D5
  ]);
}

/** Recording started: assertive low→high two-note cue (a distinct timbre, since
 *  starting a recording is privacy-relevant and worth noticing). */
export function playRecordingStartSound(): void {
  play(
    [
      { freq: 392.0, start: 0, dur: 0.1 }, // G4
      { freq: 587.33, start: 0.1, dur: 0.2 }, // D5 (a rising fifth)
    ],
    { type: 'triangle', gain: 0.055 },
  );
}

/** Someone raised their hand: soft single ping. */
export function playHandRaiseSound(): void {
  play([{ freq: 784.0, start: 0, dur: 0.22 }], { type: 'triangle', gain: 0.05 }); // G5
}

/** Screen sharing started: short three-note rising motif. */
export function playScreenShareSound(): void {
  play([
    { freq: 523.25, start: 0, dur: 0.1 }, // C5
    { freq: 659.25, start: 0.08, dur: 0.1 }, // E5
    { freq: 783.99, start: 0.16, dur: 0.16 }, // G5
  ]);
}

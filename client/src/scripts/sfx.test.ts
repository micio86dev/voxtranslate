import { afterEach, describe, expect, it, vi } from 'vitest';

// Captures every oscillator the module creates during a test so we can assert on
// the number of tones each cue plays and that they are actually scheduled.
let created: FakeOsc[] = [];
let resume: ReturnType<typeof vi.fn>;

class FakeParam {
  setValueAtTime = vi.fn();
  exponentialRampToValueAtTime = vi.fn();
}
class FakeOsc {
  type = 'sine';
  frequency = { value: 0 };
  connect = vi.fn((dest: any) => dest); // chainable: osc.connect(gain).connect(dest)
  start = vi.fn();
  stop = vi.fn();
}
class FakeGain {
  gain = new FakeParam();
  connect = vi.fn((dest: any) => dest);
}

function installAudioContext(initialState = 'suspended'): void {
  created = [];
  resume = vi.fn(async function (this: any) {
    this.state = 'running';
  });
  class FakeAudioContext {
    state = initialState;
    currentTime = 0;
    destination = {};
    resume = resume;
    createOscillator = vi.fn(() => {
      const o = new FakeOsc();
      created.push(o);
      return o;
    });
    createGain = vi.fn(() => new FakeGain());
  }
  (globalThis as any).AudioContext = FakeAudioContext;
}

/** Fresh module instance per test so the cached AudioContext / enabled flag
 *  don't leak between cases. */
async function loadSfx() {
  vi.resetModules();
  return import('./sfx');
}

afterEach(() => {
  delete (globalThis as any).AudioContext;
  delete (globalThis as any).webkitAudioContext;
  vi.restoreAllMocks();
});

describe('sfx', () => {
  it('playJoinSound plays two tones and resumes a suspended context', async () => {
    installAudioContext('suspended');
    const sfx = await loadSfx();
    sfx.playJoinSound();
    expect(created.length).toBe(2);
    expect(resume).toHaveBeenCalled();
    for (const osc of created) {
      expect(osc.start).toHaveBeenCalled();
      expect(osc.stop).toHaveBeenCalled();
      expect(osc.frequency.value).toBeGreaterThan(0);
      expect(osc.connect).toHaveBeenCalled();
    }
  });

  it('playHandRaiseSound plays a single triangle tone', async () => {
    installAudioContext('running');
    const sfx = await loadSfx();
    sfx.playHandRaiseSound();
    expect(created.length).toBe(1);
    expect(created[0].type).toBe('triangle');
  });

  it('playScreenShareSound plays three tones', async () => {
    installAudioContext('running');
    const sfx = await loadSfx();
    sfx.playScreenShareSound();
    expect(created.length).toBe(3);
  });

  it('setSfxEnabled(false) silences cues, true restores them', async () => {
    installAudioContext('running');
    const sfx = await loadSfx();
    sfx.setSfxEnabled(false);
    sfx.playJoinSound();
    expect(created.length).toBe(0);
    sfx.setSfxEnabled(true);
    sfx.playJoinSound(); // reuses the same (already running) context
    expect(created.length).toBe(2);
    expect(resume).not.toHaveBeenCalled(); // already running → no resume needed
  });

  it('does not throw when Web Audio is unavailable', async () => {
    // No AudioContext installed.
    const sfx = await loadSfx();
    expect(() => sfx.playJoinSound()).not.toThrow();
  });

  it('falls back to webkitAudioContext when AudioContext is absent', async () => {
    installAudioContext('running');
    (globalThis as any).webkitAudioContext = (globalThis as any).AudioContext;
    delete (globalThis as any).AudioContext;
    const sfx = await loadSfx();
    sfx.playScreenShareSound();
    expect(created.length).toBe(3);
  });
});

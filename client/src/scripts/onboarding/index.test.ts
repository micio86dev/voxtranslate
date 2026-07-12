// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';

// The orchestrator's collaborators are mocked; the real help-button wiring stays
// in place so the "?" launchers are exercised end-to-end.
vi.mock('./home-wizard', () => ({
  initHomeWizard: vi.fn(),
  openHomeWizard: vi.fn(),
}));
vi.mock('./call-tour', () => ({
  startCallTour: vi.fn(),
}));

type Onboarding = typeof import('./index');

const HOME_FLAG = 'vox_home_tour_seen';
const CALL_FLAG = 'vox_call_tour_seen';

async function setup(): Promise<{
  onb: Onboarding;
  hw: ReturnType<typeof vi.mocked<typeof import('./home-wizard')>>;
  ct: ReturnType<typeof vi.mocked<typeof import('./call-tour')>>;
  deps: {
    show: ReturnType<typeof vi.fn>;
    isLoggedIn: ReturnType<typeof vi.fn>;
    isB2B: ReturnType<typeof vi.fn>;
    forceMore: ReturnType<typeof vi.fn>;
  };
}> {
  vi.resetModules();
  vi.clearAllMocks(); // mock factory results survive resetModules — drop stale call counts
  localStorage.clear();
  document.body.innerHTML =
    '<button id="onb-home-help"></button><button id="onb-call-help"></button>';
  const onb = await import('./index');
  const hw = vi.mocked(await import('./home-wizard'));
  const ct = vi.mocked(await import('./call-tour'));
  const deps = {
    show: vi.fn(),
    isLoggedIn: vi.fn(() => true),
    isB2B: vi.fn(() => false),
    forceMore: vi.fn(),
  };
  onb.initOnboarding(deps);
  return { onb, hw, ct, deps };
}

afterEach(() => {
  document.body.innerHTML = '';
  vi.unstubAllGlobals();
});

describe('initOnboarding', () => {
  it('passes show/isLoggedIn/isB2B through to the home wizard', async () => {
    const { hw, deps } = await setup();
    expect(hw.initHomeWizard).toHaveBeenCalledWith({
      show: deps.show,
      isLoggedIn: deps.isLoggedIn,
      isB2B: deps.isB2B,
    });
  });

  it('wires the home "?" to always open the wizard and mark it seen', async () => {
    const { hw } = await setup();
    document.getElementById('onb-home-help')!.click();
    expect(hw.openHomeWizard).toHaveBeenCalledOnce();
    expect(localStorage.getItem(HOME_FLAG)).toBe('1');

    // The launcher keeps working after the flag is set.
    document.getElementById('onb-home-help')!.click();
    expect(hw.openHomeWizard).toHaveBeenCalledTimes(2);
  });

  it('wires the call "?" to start the tour with the forceMore relay', async () => {
    const { ct, deps } = await setup();
    document.getElementById('onb-call-help')!.click();
    expect(ct.startCallTour).toHaveBeenCalledOnce();
    expect(localStorage.getItem(CALL_FLAG)).toBe('1');

    ct.startCallTour.mock.calls[0][0].forceMore(true);
    expect(deps.forceMore).toHaveBeenCalledWith(true);
  });
});

describe('maybeAutoStartHome', () => {
  it('opens once on first visit, then never again', async () => {
    const { onb, hw } = await setup();
    onb.maybeAutoStartHome(() => true);
    expect(hw.openHomeWizard).toHaveBeenCalledOnce();
    expect(localStorage.getItem(HOME_FLAG)).toBe('1');

    onb.maybeAutoStartHome(() => true);
    expect(hw.openHomeWizard).toHaveBeenCalledOnce();
  });

  it('defers while a blocking overlay vetoes, without burning the flag', async () => {
    const { onb, hw } = await setup();
    onb.maybeAutoStartHome(() => false); // consent gate still up
    expect(hw.openHomeWizard).not.toHaveBeenCalled();
    expect(localStorage.getItem(HOME_FLAG)).toBeNull();

    onb.maybeAutoStartHome(() => true); // gate dismissed → retry succeeds
    expect(hw.openHomeWizard).toHaveBeenCalledOnce();
  });

  it('respects a flag set in an earlier session', async () => {
    const { onb, hw } = await setup();
    localStorage.setItem(HOME_FLAG, '1');
    onb.maybeAutoStartHome(() => true);
    expect(hw.openHomeWizard).not.toHaveBeenCalled();
  });
});

describe('maybeAutoStartCall', () => {
  it('starts the tour after a double rAF on first entry only', async () => {
    const { onb, ct, deps } = await setup();
    const raf = vi.fn((cb: FrameRequestCallback): number => {
      cb(0);
      return 0;
    });
    vi.stubGlobal('requestAnimationFrame', raf);

    onb.maybeAutoStartCall();
    expect(raf).toHaveBeenCalledTimes(2); // deferred until the call UI laid out
    expect(ct.startCallTour).toHaveBeenCalledOnce();
    expect(localStorage.getItem(CALL_FLAG)).toBe('1');

    ct.startCallTour.mock.calls[0][0].forceMore(false);
    expect(deps.forceMore).toHaveBeenCalledWith(false);

    onb.maybeAutoStartCall(); // seen → no second auto-start
    expect(ct.startCallTour).toHaveBeenCalledOnce();
  });

  it('does not even schedule a frame when already seen', async () => {
    const { onb, ct } = await setup();
    localStorage.setItem(CALL_FLAG, '1');
    const raf = vi.fn();
    vi.stubGlobal('requestAnimationFrame', raf);
    onb.maybeAutoStartCall();
    expect(raf).not.toHaveBeenCalled();
    expect(ct.startCallTour).not.toHaveBeenCalled();
  });
});

describe('private-mode storage fallback', () => {
  it('tracks the seen flags in memory when localStorage is blocked', async () => {
    const { onb, hw } = await setup();
    const desc = Object.getOwnPropertyDescriptor(window, 'localStorage')!;
    Object.defineProperty(window, 'localStorage', {
      configurable: true,
      get() {
        throw new Error('blocked (private mode)');
      },
    });
    try {
      onb.maybeAutoStartHome(() => true); // seen() must not throw
      expect(hw.openHomeWizard).toHaveBeenCalledOnce();

      onb.maybeAutoStartHome(() => true); // in-memory flag now set
      expect(hw.openHomeWizard).toHaveBeenCalledOnce();
    } finally {
      Object.defineProperty(window, 'localStorage', desc);
    }
  });
});

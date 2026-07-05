// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { driver, type Config, type DriveStep } from 'driver.js';

import en from '../i18n/en.json';
import { startCallTour } from './call-tour';

const h = vi.hoisted(() => ({ drive: vi.fn() }));

vi.mock('driver.js', () => ({
  driver: vi.fn(() => ({ drive: h.drive })),
}));
vi.mock('driver.js/dist/driver.css', () => ({}));

const driverMock = vi.mocked(driver);

/** All the controls the full tour spotlights, present and visible. */
const ALL_IDS = [
  'btn-mic',
  'btn-cam',
  'btn-subtitle',
  'btn-tts',
  'btn-chat',
  'call-room',
  'btn-more',
  'btn-share',
  'mi-invite',
  'btn-invite',
  'btn-leave',
];

function renderCallUi(ids: string[] = ALL_IDS, hidden: string[] = []): void {
  document.body.innerHTML = ids
    .map((id) => `<button id="${id}"${hidden.includes(id) ? ' class="hidden"' : ''}></button>`)
    .join('');
}

function lastConfig(): Config {
  const cfg = driverMock.mock.calls.at(-1)?.[0];
  expect(cfg).toBeDefined();
  return cfg!;
}

const fire = (hook: unknown): void => (hook as () => void)();

beforeEach(() => {
  vi.clearAllMocks();
});

afterEach(() => {
  document.body.innerHTML = '';
  vi.unstubAllGlobals();
});

describe('startCallTour', () => {
  it('drives all 12 steps with localized copy when every control is present', () => {
    renderCallUi();
    startCallTour({ forceMore: vi.fn() });

    expect(driverMock).toHaveBeenCalledOnce();
    expect(h.drive).toHaveBeenCalledOnce();

    const cfg = lastConfig();
    const steps = cfg.steps as DriveStep[];
    expect(steps.length).toBe(12);

    // Centered intro (no element), localized title/body.
    expect(steps[0].element).toBeUndefined();
    expect(steps[0].popover?.title).toBe(en.onbCallIntroTitle);
    expect(steps[0].popover?.description).toBe(en.onbCallIntroBody);
    expect(steps.at(-1)?.popover?.title).toBe(en.onbCallDoneTitle);

    // Spotlight steps carry their selector and side.
    expect(steps[1].element).toBe('#btn-mic');
    expect(steps[1].popover?.side).toBe('top');
    const room = steps.find((s) => s.element === '#call-room');
    expect(room?.popover?.side).toBe('bottom');

    // Button labels + the {n}/{total} → {{current}}/{{total}} progress mapping.
    expect(cfg.nextBtnText).toBe(en.onbNext);
    expect(cfg.prevBtnText).toBe(en.onbBack);
    expect(cfg.doneBtnText).toBe(en.onbDone);
    expect(cfg.progressText).toBe(
      en.onbStepCounter.replace('{n}', '{{current}}').replace('{total}', '{{total}}'),
    );
    expect(cfg.popoverClass).toBe('onb-driver');
    expect(cfg.animate).toBe(true); // no matchMedia in jsdom → no reduced-motion
  });

  it('opens the ⋯ menu for share/invite steps, closes it elsewhere and on destroy', () => {
    renderCallUi();
    const forceMore = vi.fn();
    startCallTour({ forceMore });

    const steps = lastConfig().steps as DriveStep[];
    const mic = steps.find((s) => s.element === '#btn-mic')!;
    const share = steps.find((s) => s.element === '#btn-share')!;
    const invite = steps.find((s) => s.element === '#btn-invite')!;

    fire(mic.onHighlightStarted);
    expect(forceMore).toHaveBeenLastCalledWith(false);
    fire(share.onHighlightStarted);
    expect(forceMore).toHaveBeenLastCalledWith(true);
    fire(invite.onHighlightStarted);
    expect(forceMore).toHaveBeenLastCalledWith(true);

    fire(lastConfig().onDestroyed);
    expect(forceMore).toHaveBeenLastCalledWith(false);
  });

  it('skips steps whose control is missing or hidden in the current layout', () => {
    // No camera button (mobile audio-only), share hidden (guest), no invite menu item.
    renderCallUi(
      ALL_IDS.filter((id) => id !== 'btn-cam' && id !== 'mi-invite'),
      ['btn-share'],
    );
    startCallTour({ forceMore: vi.fn() });

    const steps = lastConfig().steps as DriveStep[];
    const elements = steps.map((s) => s.element);
    expect(elements).not.toContain('#btn-cam'); // selector missing
    expect(elements).not.toContain('#btn-share'); // available() → false
    expect(elements).not.toContain('#btn-invite'); // available() → false
    expect(steps.length).toBe(9);
    // Intro/outro survive regardless.
    expect(steps[0].popover?.title).toBe(en.onbCallIntroTitle);
    expect(steps.at(-1)?.popover?.title).toBe(en.onbCallDoneTitle);
  });

  it('disables animation under prefers-reduced-motion', () => {
    renderCallUi();
    vi.stubGlobal(
      'matchMedia',
      vi.fn(() => ({ matches: true }) as MediaQueryList),
    );
    startCallTour({ forceMore: vi.fn() });
    expect(lastConfig().animate).toBe(false);
  });
});

// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';

import {
  CALL_TOUR_STEPS,
  HOME_WIZARD_STEPS,
  selectWizardSteps,
  shouldShowWebinarStep,
} from './steps';

afterEach(() => {
  document.body.innerHTML = '';
});

describe('HOME_WIZARD_STEPS', () => {
  it('has unique keys and complete i18n/glyph references', () => {
    const keys = HOME_WIZARD_STEPS.map((s) => s.key);
    expect(new Set(keys).size).toBe(keys.length);
    for (const s of HOME_WIZARD_STEPS) {
      expect(['all', 'guest', 'authed', 'b2b']).toContain(s.role);
      expect(s.glyph).not.toBe('');
      expect(s.titleKey).not.toBe('');
      expect(s.bodyKey).not.toBe('');
    }
  });

  it('tailors exactly one step per audience (credits ↔ sign-up ↔ webinar)', () => {
    expect(HOME_WIZARD_STEPS.filter((s) => s.role === 'authed').map((s) => s.key)).toEqual([
      'credits',
    ]);
    expect(HOME_WIZARD_STEPS.filter((s) => s.role === 'guest').map((s) => s.key)).toEqual([
      'guest',
    ]);
    expect(HOME_WIZARD_STEPS.filter((s) => s.role === 'b2b').map((s) => s.key)).toEqual([
      'webinar',
    ]);
  });

  it('opens with the welcome step and ends on the CTA', () => {
    expect(HOME_WIZARD_STEPS[0].key).toBe('welcome');
    expect(HOME_WIZARD_STEPS[HOME_WIZARD_STEPS.length - 1].key).toBe('cta');
  });
});

describe('shouldShowWebinarStep', () => {
  it('shows only for B2B users (≥1 org), regardless of sign-in flag pairing', () => {
    expect(shouldShowWebinarStep({ isLoggedIn: true, isB2B: true })).toBe(true);
    expect(shouldShowWebinarStep({ isLoggedIn: true, isB2B: false })).toBe(false);
    expect(shouldShowWebinarStep({ isLoggedIn: false, isB2B: false })).toBe(false);
  });
});

describe('selectWizardSteps', () => {
  const keys = (a: { isLoggedIn: boolean; isB2B: boolean }): string[] =>
    selectWizardSteps(a).map((s) => s.key);

  it('gives a guest the sign-up step but neither credits nor webinar', () => {
    expect(keys({ isLoggedIn: false, isB2B: false })).toEqual([
      'welcome',
      'start',
      'tiers',
      'guest',
      'cta',
    ]);
  });

  it('gives a signed-in consumer credits but no webinar step', () => {
    expect(keys({ isLoggedIn: true, isB2B: false })).toEqual([
      'welcome',
      'start',
      'tiers',
      'credits',
      'cta',
    ]);
  });

  it('adds the webinar step for a B2B user after credits', () => {
    expect(keys({ isLoggedIn: true, isB2B: true })).toEqual([
      'welcome',
      'start',
      'tiers',
      'credits',
      'webinar',
      'cta',
    ]);
  });
});

describe('CALL_TOUR_STEPS', () => {
  it('has unique keys; intro/outro are centered (no selector), the rest target a control', () => {
    const keys = CALL_TOUR_STEPS.map((s) => s.key);
    expect(new Set(keys).size).toBe(keys.length);
    for (const s of CALL_TOUR_STEPS) {
      if (s.key === 'intro' || s.key === 'done') expect(s.selector).toBeUndefined();
      else expect(s.selector).toMatch(/^#/);
    }
  });

  it('every step living in the ⋯ menu declares an availability guard', () => {
    for (const s of CALL_TOUR_STEPS.filter((x) => x.openMore)) {
      expect(s.available, s.key).toBeTypeOf('function');
    }
  });
});

describe('the share step availability guard', () => {
  const available = CALL_TOUR_STEPS.find((s) => s.key === 'share')!.available!;

  it('is false when the button is absent', () => {
    expect(available()).toBe(false);
  });

  it('is true when the button exists and is not hidden', () => {
    document.body.innerHTML = '<button id="btn-share"></button>';
    expect(available()).toBe(true);
  });

  it('is false when hidden by the .hidden class (the show() convention)', () => {
    document.body.innerHTML = '<button id="btn-share" class="hidden"></button>';
    expect(available()).toBe(false);
  });

  it('is false when hidden by the HTML hidden attribute', () => {
    document.body.innerHTML = '<button id="btn-share" hidden></button>';
    expect(available()).toBe(false);
  });
});

describe('the invite step availability guard', () => {
  const available = CALL_TOUR_STEPS.find((s) => s.key === 'invite')!.available!;

  it('requires BOTH the menu item and the button to be shown', () => {
    expect(available()).toBe(false);

    document.body.innerHTML = '<button id="btn-invite"></button>';
    expect(available()).toBe(false); // mi-invite missing

    document.body.innerHTML = '<div id="mi-invite"></div><button id="btn-invite"></button>';
    expect(available()).toBe(true);

    document.body.innerHTML =
      '<div id="mi-invite" class="hidden"></div><button id="btn-invite"></button>';
    expect(available()).toBe(false); // guest layout hides the menu item
  });
});

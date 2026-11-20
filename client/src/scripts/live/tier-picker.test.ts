// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';
import en from '../i18n/en.json';
import { mountTierPicker } from './tier-picker';
import type { LiveEngine, LiveEngines } from './tiers';

const EN = en as Record<string, string>;

const std = (over: Partial<LiveEngine> = {}): LiveEngine => ({
  id: 'standard',
  display_name: 'Standard',
  tier: 'standard',
  description: 'server standard text',
  rate_per_minute: 0.0045,
  min_balance: 0.05,
  available: true,
  scales_per_language: true,
  client_direct: false,
  ...over,
});
const premium = (over: Partial<LiveEngine> = {}): LiveEngine =>
  std({ id: 'gemini_live_translate', display_name: 'Premium', tier: 'premium', rate_per_minute: 0.03, min_balance: 0.4, ...over });
const both = (): LiveEngines => ({ default: 'standard', engines: [std(), premium()] });

let root: HTMLElement;
let hint: HTMLElement;
let balance: number | undefined;
let stored: string | null;
const save = vi.fn();

function mount() {
  return mountTierPicker(root, { idPrefix: 'lh', hint, balance: () => balance, load: () => stored, save });
}
const radios = () => [...root.querySelectorAll<HTMLInputElement>('input[type="radio"]')];
const radio = (id: string) => radios().find((r) => r.value === id) as HTMLInputElement;
const card = (id: string) => radio(id).closest('label') as HTMLElement;

beforeEach(() => {
  document.body.innerHTML = '<div role="radiogroup" aria-labelledby="x" id="root"></div><small id="hint"></small>';
  root = document.getElementById('root') as HTMLElement;
  hint = document.getElementById('hint') as HTMLElement;
  balance = 9;
  stored = null;
  save.mockReset();
});

describe('before the list arrives (and when it never does)', () => {
  it('shows Standard only, selected, with no cost claim and no error', () => {
    const p = mount();
    expect(radios().map((r) => r.value)).toEqual(['standard']);
    expect(radio('standard').checked).toBe(true);
    expect(p.get()).toBe('standard');
    expect(card('standard').textContent).toContain('Standard');
    expect(card('standard').textContent).not.toMatch(/\$/);
    expect(root.querySelector('[role="alert"]')).toBeNull();
  });
});

describe('with the tiers the server lists', () => {
  it('draws one radio card per tier: name, description, cost per language stream', () => {
    const p = mount();
    p.setEngines(both());
    expect(radios().map((r) => r.value)).toEqual(['standard', 'gemini_live_translate']);
    const s = card('standard').textContent ?? '';
    const pr = card('gemini_live_translate').textContent ?? '';
    expect(s).toContain('Standard');
    // Meet's own wording is reused, not a parallel copy.
    expect(s).toContain(EN.engineDescStandard);
    expect(pr).toContain(EN.engineDescPremium);
    expect(s).toContain('from $0.0045 / min per language');
    expect(pr).toContain('from $0.03 / min per language');
    expect(p.get()).toBe('standard');
  });

  it('is a labelled group of native radios (keyboard-operable): one name, a label per card', () => {
    mount().setEngines(both());
    expect(new Set(radios().map((r) => r.name)).size).toBe(1);
    for (const r of radios()) {
      expect(r.closest('label')).not.toBeNull();
      expect(r.disabled).toBe(false);
    }
  });

  it('lists only what the server returned: no Premium card when it is not configured', () => {
    mount().setEngines({ default: 'standard', engines: [std()] });
    expect(radios().map((r) => r.value)).toEqual(['standard']);
  });

  it('falls back to the description the server sent for a tier it has no copy for', () => {
    mount().setEngines({ default: 'x', engines: [std({ id: 'x', tier: 'mystery', display_name: 'Mystery', description: 'from the server' })] });
    expect(card('x').textContent).toContain('from the server');
  });

  it('explains once that the rate is per language, only when a tier scales that way', () => {
    const p = mount();
    p.setEngines(both());
    expect(hint.textContent).toBe(EN.engineCostPerLanguage);
    p.setEngines({ default: 'standard', engines: [std({ scales_per_language: false })] });
    expect(hint.textContent).toBe('');
  });

  it('marks a tier that is at capacity as busy, but still lets the host pick it', () => {
    const p = mount();
    p.setEngines({ default: 'standard', engines: [std(), premium({ available: false })] });
    expect(card('gemini_live_translate').textContent).toContain(EN.liveTierBusy);
    expect(radio('gemini_live_translate').disabled).toBe(false);
    radio('gemini_live_translate').click();
    expect(p.get()).toBe('gemini_live_translate');
  });
});

describe('choosing and remembering', () => {
  it('selecting a card changes the choice and remembers it', () => {
    const p = mount();
    p.setEngines(both());
    radio('gemini_live_translate').click();
    expect(p.get()).toBe('gemini_live_translate');
    expect(save).toHaveBeenCalledWith('gemini_live_translate');
    radio('standard').click();
    expect(p.get()).toBe('standard');
    expect(save).toHaveBeenLastCalledWith('standard');
  });

  it('restores a remembered tier once the list confirms it is still offered', () => {
    stored = 'gemini_live_translate';
    const p = mount();
    expect(p.get()).toBe('standard'); // not known yet: never claim a tier before the list
    p.setEngines(both());
    expect(p.get()).toBe('gemini_live_translate');
    expect(radio('gemini_live_translate').checked).toBe(true);
  });

  it('drops a remembered tier that is no longer offered', () => {
    stored = 'gemini_live_translate';
    const p = mount();
    p.setEngines({ default: 'standard', engines: [std()] });
    expect(p.get()).toBe('standard');
    expect(radio('standard').checked).toBe(true);
  });
});

describe('balance', () => {
  it('disables a tier whose minimum balance the host does not have, with the reason', () => {
    balance = 0.1;
    mount().setEngines(both());
    const r = radio('gemini_live_translate');
    expect(r.disabled).toBe(true);
    const reason = document.getElementById(r.getAttribute('aria-describedby') as string);
    expect(reason?.textContent).toBe('Needs at least $0.40 in your balance.');
    expect(radio('standard').disabled).toBe(false);
  });

  it('never disables anything when the balance is not known (the server decides)', () => {
    balance = undefined;
    mount().setEngines(both());
    expect(radios().every((r) => !r.disabled)).toBe(true);
  });

  it('a remembered tier the host can no longer afford falls back to the default', () => {
    stored = 'gemini_live_translate';
    balance = 0.1;
    const p = mount();
    p.setEngines(both());
    expect(p.get()).toBe('standard');
  });
});

describe('Enhanced card', () => {
  const enhanced = (over: Partial<LiveEngine> = {}): LiveEngine =>
    std({ id: 'cartesia', display_name: 'Enhanced', tier: 'enhanced', rate_per_minute: 0.06, min_balance: 0.6, client_direct: true, description: 'server enhanced text', ...over });
  const three = (): LiveEngines => ({ default: 'standard', engines: [std(), enhanced(), premium()] });

  it('sits between Standard and Premium in the order the server gave, with Meet\'s wording and the per-language cost', () => {
    const p = mount();
    p.setEngines(three());
    expect(radios().map((r) => r.value)).toEqual(['standard', 'cartesia', 'gemini_live_translate']);
    expect(card('cartesia').textContent).toContain(EN.engineDescEnhanced);
    expect(card('cartesia').textContent).not.toContain('server enhanced text');
    expect(card('cartesia').textContent).toContain('from $0.06 / min per language');
    expect(hint.textContent).toBe(EN.engineCostPerLanguage);
  });

  it('uses the Enhanced wording even when the server leaves its tier name out', () => {
    const p = mount();
    p.setEngines({ default: 'standard', engines: [std(), enhanced({ tier: '' })] });
    expect(card('cartesia').textContent).toContain(EN.engineDescEnhanced);
  });

  it('is disabled with the reason when the balance cannot start it', () => {
    balance = 0.1;
    mount().setEngines(three());
    expect(radio('cartesia').disabled).toBe(true);
    expect(card('cartesia').textContent).toContain('Needs at least $0.60 in your balance.');
  });

  it('tells the page when another card is picked, and when the list lands (the resolved choice)', () => {
    const onChange = vi.fn();
    const p = mountTierPicker(root, { idPrefix: 'lh', hint, balance: () => balance, load: () => 'cartesia', save, onChange });
    p.setEngines(three());
    expect(onChange).toHaveBeenLastCalledWith('cartesia');
    radio('standard').click();
    expect(onChange).toHaveBeenLastCalledWith('standard');
    expect(save).toHaveBeenCalledWith('standard');
  });
});

import { describe, expect, it } from 'vitest';
import { fallbackEngines, needsMoreBalance, parseEngines, resolveTier, tierLabel, type LiveEngine } from './tiers';

const std = (over: Partial<LiveEngine> = {}): LiveEngine => ({
  id: 'standard',
  display_name: 'Standard',
  tier: 'standard',
  description: 'd',
  rate_per_minute: 0.0045,
  min_balance: 0.05,
  available: true,
  scales_per_language: true,
  client_direct: false,
  ...over,
});
const premium = (over: Partial<LiveEngine> = {}): LiveEngine =>
  std({ id: 'gemini_live_translate', display_name: 'Premium', tier: 'premium', rate_per_minute: 0.03, min_balance: 0.4, ...over });

describe('parseEngines', () => {
  it('keeps only the tiers the server listed, in order, with the default', () => {
    const out = parseEngines({ default: 'standard', engines: [std(), premium()] });
    expect(out?.default).toBe('standard');
    expect(out?.engines.map((e) => e.id)).toEqual(['standard', 'gemini_live_translate']);
  });

  it('drops malformed entries and fills the optional fields', () => {
    const out = parseEngines({
      default: 'standard',
      engines: [{ id: 'standard', display_name: 'Standard', tier: 'standard' }, { nope: 1 }, null, 7],
    });
    expect(out?.engines).toHaveLength(1);
    expect(out?.engines[0]).toMatchObject({ rate_per_minute: 0, min_balance: 0, available: true, scales_per_language: false });
  });

  it('returns null for anything that is not a usable list', () => {
    expect(parseEngines(null)).toBeNull();
    expect(parseEngines('x')).toBeNull();
    expect(parseEngines({ engines: [] })).toBeNull();
    expect(parseEngines({ engines: 'x' })).toBeNull();
  });

  it('falls back to the first tier when the default is not in the list', () => {
    const out = parseEngines({ default: 'ghost', engines: [premium()] });
    expect(out?.default).toBe('gemini_live_translate');
  });
});

describe('fallbackEngines', () => {
  it('is Standard only', () => {
    const f = fallbackEngines();
    expect(f.default).toBe('standard');
    expect(f.engines.map((e) => e.id)).toEqual(['standard']);
  });
});

describe('needsMoreBalance', () => {
  it('is true only for a non-default tier whose minimum exceeds a KNOWN balance', () => {
    expect(needsMoreBalance(premium(), 0.1)).toBe(true);
    expect(needsMoreBalance(premium(), 0.4)).toBe(false);
    expect(needsMoreBalance(premium(), undefined)).toBe(false);
    // Standard is never disabled: the server is the authority and a 402 shows the buy path.
    expect(needsMoreBalance(std(), 0)).toBe(false);
  });
});

describe('resolveTier', () => {
  const list = { default: 'standard', engines: [std(), premium()] };
  it('keeps a stored choice that is still offered and affordable', () => {
    expect(resolveTier('gemini_live_translate', list, 5)).toBe('gemini_live_translate');
  });
  it('falls back to the default when the stored tier is gone', () => {
    expect(resolveTier('gemini_live_translate', { default: 'standard', engines: [std()] }, 5)).toBe('standard');
    expect(resolveTier('enhanced', list, 5)).toBe('standard');
    expect(resolveTier(null, list, 5)).toBe('standard');
  });
  it('falls back when the stored tier is not affordable', () => {
    expect(resolveTier('gemini_live_translate', list, 0.1)).toBe('standard');
  });
});

describe('tierLabel', () => {
  it('names the known tiers and echoes an unknown id', () => {
    expect(tierLabel('standard')).toBe('Standard');
    expect(tierLabel('gemini_live_translate')).toBe('Premium');
    expect(tierLabel('x')).toBe('x');
  });
});

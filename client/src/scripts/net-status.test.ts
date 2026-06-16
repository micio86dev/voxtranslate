import { describe, expect, it } from 'vitest';

import { nextBannerState } from './net-status';

describe('nextBannerState', () => {
  it('hides while everything is fine (no flash on first paint)', () => {
    expect(nextBannerState('ok', true, false)).toEqual({ health: 'ok', banner: 'hidden' });
  });

  it('shows the red offline pill when the browser is offline', () => {
    expect(nextBannerState('ok', false, false)).toEqual({ health: 'offline', banner: 'offline' });
  });

  it('offline wins even if the transport is also flagged degraded', () => {
    expect(nextBannerState('ok', false, true)).toEqual({ health: 'offline', banner: 'offline' });
  });

  it('shows the amber reconnecting pill when online but transport degraded', () => {
    expect(nextBannerState('ok', true, true)).toEqual({ health: 'degraded', banner: 'degraded' });
  });

  it('flashes green when recovering from offline', () => {
    expect(nextBannerState('offline', true, false)).toEqual({ health: 'ok', banner: 'restored' });
  });

  it('flashes green when recovering from a degraded transport', () => {
    expect(nextBannerState('degraded', true, false)).toEqual({ health: 'ok', banner: 'restored' });
  });

  it('does not re-flash green once already settled', () => {
    expect(nextBannerState('ok', true, false)).toEqual({ health: 'ok', banner: 'hidden' });
  });
});

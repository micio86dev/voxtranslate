// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { ApiResult, StaffDesk } from './api';

vi.mock('../auth', () => ({ HTTP_BASE: 'http://api.test', authHeaders: () => ({}) }));

import { revealDesksEntry } from './desks-menu';

const desk: StaffDesk = { id: 'd1', org_id: 'o', org_name: 'Acme', name: 'Desk', location_label: null, default_lang: 'en', waiting: 0 };
const entry = () => document.getElementById('desks-btn') as HTMLElement;

beforeEach(() => {
  document.body.innerHTML = '<a id="desks-btn" class="acct-item hidden" href="/live/desks"></a>';
});

describe('revealDesksEntry', () => {
  it('stays hidden by default', () => {
    expect(entry().classList.contains('hidden')).toBe(true);
  });

  it('reveals the menu item when the member has at least one desk', async () => {
    const f = vi.fn(async (): Promise<ApiResult<{ desks: StaffDesk[] }>> => ({ ok: true, data: { desks: [desk] } }));
    expect(await revealDesksEntry(f)).toBe(true);
    expect(entry().classList.contains('hidden')).toBe(false);
    expect(f).toHaveBeenCalledTimes(1);
  });

  it('stays hidden when the member has no desk', async () => {
    expect(await revealDesksEntry(async () => ({ ok: true, data: { desks: [] } }))).toBe(false);
    expect(entry().classList.contains('hidden')).toBe(true);
  });

  it.each([[401], [403], [500], [0]])('stays hidden when the lookup fails (%s)', async (status) => {
    expect(await revealDesksEntry(async () => ({ ok: false, status, code: 'x' }))).toBe(false);
    expect(entry().classList.contains('hidden')).toBe(true);
  });

  it('never throws, even if the lookup itself blows up or the menu item is not on the page', async () => {
    await expect(revealDesksEntry(async () => { throw new Error('boom'); })).resolves.toBe(false);
    document.body.innerHTML = '';
    await expect(revealDesksEntry(async () => ({ ok: true, data: { desks: [desk] } }))).resolves.toBe(false);
  });

  it('does not ask again once revealed (the home screen is entered many times per visit)', async () => {
    const f = vi.fn(async (): Promise<ApiResult<{ desks: StaffDesk[] }>> => ({ ok: true, data: { desks: [desk] } }));
    await revealDesksEntry(f);
    expect(await revealDesksEntry(f)).toBe(true);
    expect(f).toHaveBeenCalledTimes(1);
  });
});

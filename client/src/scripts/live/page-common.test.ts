// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { byId, initPage, setHidden } from './page-common';

beforeEach(() => {
  document.head.innerHTML = '';
  document.body.innerHTML = '<p data-i18n="liveConnect">x</p><p id="a"></p>';
  document.cookie = 'vt_lang=; Max-Age=0';
});

describe('initPage', () => {
  it('paints the data-i18n slots and returns the UI language', () => {
    const lang = initPage();
    expect(lang).toBe('en');
    expect(document.querySelector('[data-i18n]')?.textContent).toBe('Live Connect');
    expect(document.documentElement.lang).toBe('en');
  });

  it('wires the connection-status pill (offline / reconnecting)', () => {
    const spy = vi.spyOn(window, 'addEventListener');
    initPage();
    expect(spy.mock.calls.map((c) => c[0])).toEqual(expect.arrayContaining(['online', 'offline']));
    spy.mockRestore();
  });

  it('adds a no-referrer policy exactly once (the invitation link is a bearer secret)', () => {
    initPage();
    initPage();
    const metas = document.head.querySelectorAll('meta[name="referrer"]');
    expect(metas).toHaveLength(1);
    expect(metas[0].getAttribute('content')).toBe('no-referrer');
  });
});

describe('helpers', () => {
  it('byId and setHidden tolerate a missing element', () => {
    expect(byId('nope')).toBeNull();
    expect(() => setHidden('nope', true)).not.toThrow();
    setHidden('a', true);
    expect(byId('a')?.hidden).toBe(true);
  });
});

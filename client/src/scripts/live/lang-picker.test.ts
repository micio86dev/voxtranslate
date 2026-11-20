// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { defaultLanguage, mountLangPicker, pickableLanguages } from './lang-picker';

let box: HTMLElement;
beforeEach(() => {
  document.body.innerHTML = '<div id="box"></div>';
  box = document.getElementById('box') as HTMLElement;
});

describe('pickableLanguages', () => {
  it('offers only languages the engine can speak, in the shared map order', () => {
    const list = pickableLanguages();
    expect(list.length).toBeGreaterThan(5);
    expect(list.some((l) => l.code === 'it')).toBe(true);
    expect(new Set(list.map((l) => l.code)).size).toBe(list.length);
  });
});

describe('defaultLanguage', () => {
  const codes = ['en', 'it', 'pt', 'zh'];
  it('maps a regional browser language onto the base language', () => {
    expect(defaultLanguage(codes, 'it-IT')).toBe('it');
    expect(defaultLanguage(codes, 'pt-BR')).toBe('pt');
    expect(defaultLanguage(codes, 'zh-Hans-CN')).toBe('zh');
  });
  it('tries each candidate in order and falls back to English, then to the first code', () => {
    expect(defaultLanguage(codes, 'xx', 'it')).toBe('it');
    expect(defaultLanguage(codes, 'xx', undefined, null)).toBe('en');
    expect(defaultLanguage(['de', 'fr'], 'xx')).toBe('de');
  });
});

describe('mountLangPicker', () => {
  const codes = ['en', 'it', 'es'];

  it('starts with the initial language selected and the panel closed', () => {
    const p = mountLangPicker(box, { idPrefix: 'x', codes, initial: 'it' });
    expect(p.get()).toBe('it');
    const trigger = box.querySelector('button.lc-lang-trigger') as HTMLButtonElement;
    expect(trigger.getAttribute('aria-expanded')).toBe('false');
    expect(trigger.textContent).toContain('Italiano');
    expect((box.querySelector('.lc-lang-panel') as HTMLElement).hidden).toBe(true);
  });

  it('shows the placeholder when nothing is selected', () => {
    const p = mountLangPicker(box, { idPrefix: 'x', codes, initial: null });
    expect(p.get()).toBeNull();
    expect(box.querySelector('.lc-lang-text')?.textContent).toBe('Choose a language');
  });

  it('opens, lists endonym + English name, filters by search, and selects', () => {
    const onChange = vi.fn();
    const p = mountLangPicker(box, { idPrefix: 'x', codes, initial: null, onChange });
    (box.querySelector('.lc-lang-trigger') as HTMLButtonElement).click();
    expect((box.querySelector('.lc-lang-panel') as HTMLElement).hidden).toBe(false);
    expect(box.querySelectorAll('.lc-lang-opt')).toHaveLength(3);
    const search = box.querySelector('input') as HTMLInputElement;
    search.value = 'span';
    search.dispatchEvent(new Event('input'));
    const opts = box.querySelectorAll<HTMLButtonElement>('.lc-lang-opt');
    expect(opts).toHaveLength(1);
    expect(opts[0].textContent).toContain('Spanish');
    opts[0].click();
    expect(p.get()).toBe('es');
    expect(onChange).toHaveBeenCalledWith('es');
    expect((box.querySelector('.lc-lang-panel') as HTMLElement).hidden).toBe(true);
    expect(box.querySelector('.lc-lang-text')?.textContent).toContain('Español');
  });

  it('shows the empty state when nothing matches', () => {
    mountLangPicker(box, { idPrefix: 'x', codes, initial: null });
    (box.querySelector('.lc-lang-trigger') as HTMLButtonElement).click();
    const search = box.querySelector('input') as HTMLInputElement;
    search.value = 'zzzz';
    search.dispatchEvent(new Event('input'));
    expect((box.querySelector('.lc-lang-empty') as HTMLElement).hidden).toBe(false);
  });

  it('closes on Escape and on an outside click, but not on a click inside', () => {
    mountLangPicker(box, { idPrefix: 'x', codes, initial: null });
    const trigger = box.querySelector('.lc-lang-trigger') as HTMLButtonElement;
    const panel = box.querySelector('.lc-lang-panel') as HTMLElement;
    trigger.click();
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    expect(panel.hidden).toBe(true);
    trigger.click();
    (box.querySelector('input') as HTMLInputElement).dispatchEvent(new MouseEvent('click', { bubbles: true }));
    expect(panel.hidden).toBe(false);
    document.body.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    expect(panel.hidden).toBe(true);
  });

  it('set() updates the label programmatically; an unknown code is ignored', () => {
    const p = mountLangPicker(box, { idPrefix: 'x', codes, initial: 'en' });
    p.set('es');
    expect(p.get()).toBe('es');
    p.set('zz');
    expect(p.get()).toBe('es');
  });

  it('exposes listbox semantics with stable ids and an accessible name', () => {
    mountLangPicker(box, { idPrefix: 'lh', codes, initial: null, labelKey: 'liveYourLang' });
    const list = box.querySelector('[role="listbox"]') as HTMLElement;
    expect(list.id).toBe('lh-lang-list');
    expect(list.getAttribute('aria-label')).toBe('Your language');
    expect(box.querySelector('.lc-lang-trigger')?.getAttribute('aria-controls')).toBe('lh-lang-list');
    expect(box.querySelector('.lc-lang-trigger')?.getAttribute('aria-haspopup')).toBe('listbox');
  });
});

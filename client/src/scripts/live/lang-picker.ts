// Live Connect — language picker (spec 0121).
//
// The same shape as the Talk page's picker (trigger, search box, listbox of endonym +
// English name), built from the shared language map so this page and the engine can never
// disagree about what exists. Built in code so the host and guest pages share one
// implementation instead of two copies of the markup.

import { t } from '../i18n';
import { LANGUAGES, tierOutputLangs, type LangMeta } from '../langmap';

/** Languages the engine can speak AND understand: what Live Connect's default tier outputs. */
export function pickableLanguages(): LangMeta[] {
  const offered = new Set(tierOutputLangs('standard'));
  const list = offered.size ? LANGUAGES.filter((l) => offered.has(l.code)) : [...LANGUAGES];
  return list;
}

/**
 * First candidate that maps onto an offered code, else English, else the first code.
 * `pt-BR` and `zh-Hans-CN` map onto their base language: the map has no regional variants.
 */
export function defaultLanguage(codes: readonly string[], ...candidates: Array<string | null | undefined>): string {
  for (const c of candidates) {
    if (!c) continue;
    const lower = c.toLowerCase();
    if (codes.includes(lower)) return lower;
    const base = lower.split(/[-_]/)[0];
    if (codes.includes(base)) return base;
  }
  return codes.includes('en') ? 'en' : codes[0];
}

export interface LangPicker {
  get(): string | null;
  set(code: string): void;
}

export interface LangPickerOptions {
  /** Prefixes element ids so two pickers (or a test and a page) never collide. */
  idPrefix: string;
  codes: readonly string[];
  initial: string | null;
  /** i18n key naming the control for assistive tech. Defaults to the placeholder text. */
  labelKey?: string;
  onChange?: (code: string) => void;
}

export function mountLangPicker(container: HTMLElement, opts: LangPickerOptions): LangPicker {
  const doc = container.ownerDocument;
  const allowed = LANGUAGES.filter((l) => opts.codes.includes(l.code));
  let selected: string | null = null;
  const label = t(opts.labelKey ?? 'liveOtherLangPick');
  const listId = `${opts.idPrefix}-lang-list`;

  const trigger = doc.createElement('button');
  trigger.type = 'button';
  trigger.className = 'lc-lang-trigger';
  trigger.setAttribute('aria-haspopup', 'listbox');
  trigger.setAttribute('aria-expanded', 'false');
  trigger.setAttribute('aria-controls', listId);
  const flag = doc.createElement('span');
  flag.className = 'lc-lang-flag';
  flag.setAttribute('aria-hidden', 'true');
  const text = doc.createElement('span');
  text.className = 'lc-lang-text';
  const chevron = doc.createElement('span');
  chevron.className = 'lc-lang-chevron';
  chevron.setAttribute('aria-hidden', 'true');
  chevron.textContent = '▾';
  trigger.append(flag, text, chevron);

  const panel = doc.createElement('div');
  panel.className = 'lc-lang-panel';
  panel.hidden = true;
  const searchBox = doc.createElement('div');
  searchBox.className = 'lc-lang-search';
  const search = doc.createElement('input');
  search.type = 'text';
  search.autocomplete = 'off';
  search.spellcheck = false;
  search.placeholder = t('langSearchPlaceholder');
  search.setAttribute('aria-label', t('langSearchPlaceholder'));
  searchBox.append(search);
  const list = doc.createElement('div');
  list.id = listId;
  list.className = 'lc-lang-list';
  list.setAttribute('role', 'listbox');
  list.setAttribute('aria-label', label);
  const empty = doc.createElement('p');
  empty.className = 'lc-lang-empty';
  empty.hidden = true;
  empty.textContent = t('langSearchEmpty');
  panel.append(searchBox, list, empty);
  container.append(trigger, panel);

  const paintTrigger = (): void => {
    const meta = selected ? allowed.find((l) => l.code === selected) : undefined;
    flag.textContent = meta?.flag ?? '';
    text.textContent = meta ? `${meta.native} (${meta.english})` : label;
    trigger.setAttribute('aria-label', meta ? `${label}: ${meta.native}` : label);
  };

  const close = (): void => {
    panel.hidden = true;
    trigger.setAttribute('aria-expanded', 'false');
  };
  const open = (): void => {
    renderList('');
    search.value = '';
    panel.hidden = false;
    trigger.setAttribute('aria-expanded', 'true');
    search.focus();
  };

  const choose = (code: string): void => {
    selected = code;
    paintTrigger();
    close();
    opts.onChange?.(code);
  };

  function renderList(query: string): void {
    const q = query.trim().toLowerCase();
    const matches = allowed.filter(
      (l) => !q || l.code.includes(q) || l.english.toLowerCase().includes(q) || l.native.toLowerCase().includes(q),
    );
    list.textContent = '';
    for (const meta of matches) {
      const opt = doc.createElement('button');
      opt.type = 'button';
      opt.className = 'lc-lang-opt';
      opt.setAttribute('role', 'option');
      opt.setAttribute('aria-selected', String(meta.code === selected));
      if (meta.rtl) opt.dir = 'rtl';
      const f = doc.createElement('span');
      f.className = 'lc-lang-opt-flag';
      f.setAttribute('aria-hidden', 'true');
      f.textContent = meta.flag;
      const n = doc.createElement('span');
      n.className = 'lc-lang-opt-native';
      n.textContent = meta.native;
      const en = doc.createElement('span');
      en.className = 'lc-lang-opt-en';
      en.textContent = meta.english;
      opt.append(f, n, en);
      opt.addEventListener('click', () => choose(meta.code));
      list.append(opt);
    }
    empty.hidden = matches.length > 0;
  }

  trigger.addEventListener('click', () => (panel.hidden ? open() : close()));
  search.addEventListener('input', () => renderList(search.value));
  doc.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') close();
  });
  doc.addEventListener('click', (e) => {
    if (panel.hidden) return;
    const target = e.target as Node;
    if (!panel.contains(target) && !trigger.contains(target)) close();
  });

  if (opts.initial && allowed.some((l) => l.code === opts.initial)) selected = opts.initial;
  paintTrigger();

  return {
    get: () => selected,
    set(code: string) {
      if (!allowed.some((l) => l.code === code)) return;
      selected = code;
      paintTrigger();
    },
  };
}

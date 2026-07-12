// Homepage onboarding wizard — a custom multi-step modal (no external library), following the
// project's existing `.modal-overlay` pattern. Focus-trap / ESC / focus-restore are reused from
// app.ts's show() (passed in via deps); this owns step rendering, Back/Next/Skip, the progress
// dots, ←/→ navigation, and guest-vs-authed step selection.

import { t } from '../i18n';
import { selectWizardSteps, type WizardStep } from './steps';

interface WizardDeps {
  show: (el: HTMLElement, visible: boolean) => void;
  isLoggedIn: () => boolean;
  /** Whether the signed-in user belongs to ≥1 organization (drives the webinar step). */
  isB2B: () => boolean;
}

const byId = <T extends HTMLElement = HTMLElement>(id: string): T | null =>
  document.getElementById(id) as T | null;

// Inline line-glyphs (stroke = accent, set in CSS). Self-contained so the wizard needs no icon
// dependency; each is a calm, single-subject mark rather than a generic decoration.
const GLYPHS: Record<string, string> = {
  globe:
    '<svg viewBox="0 0 24 24" fill="none"><circle cx="12" cy="12" r="9"/><path d="M3 12h18"/><path d="M12 3c2.5 2.6 3.9 5.7 3.9 9s-1.4 6.4-3.9 9c-2.5-2.6-3.9-5.7-3.9-9S9.5 5.6 12 3Z"/></svg>',
  call:
    '<svg viewBox="0 0 24 24" fill="none"><rect x="3" y="6" width="12.5" height="12" rx="2.5"/><path d="m15.5 10 5.5-3v10l-5.5-3"/></svg>',
  tiers:
    '<svg viewBox="0 0 24 24" fill="none"><path d="m12 3.5 8.5 4-8.5 4-8.5-4 8.5-4Z"/><path d="m3.5 12 8.5 4 8.5-4"/><path d="m3.5 16 8.5 4 8.5-4"/></svg>',
  credits:
    '<svg viewBox="0 0 24 24" fill="none"><circle cx="12" cy="12" r="8.5"/><path d="M12 7v10M14.6 9.2c0-1.2-1.2-1.9-2.6-1.9s-2.6.7-2.6 1.8 1.2 1.7 2.6 1.7 2.6.6 2.6 1.8-1.2 1.9-2.6 1.9-2.6-.7-2.6-1.9"/></svg>',
  guest:
    '<svg viewBox="0 0 24 24" fill="none"><circle cx="12" cy="8" r="3.6"/><path d="M5 20c0-3.7 3.1-6.2 7-6.2s7 2.5 7 6.2"/></svg>',
  rocket:
    '<svg viewBox="0 0 24 24" fill="none"><path d="M12 3c3.5 1.7 5.4 4.9 5.4 9 0 1.7-.4 3.3-1.2 4.8H7.8C7 15.3 6.6 13.7 6.6 12 6.6 7.9 8.5 4.7 12 3Z"/><circle cx="12" cy="10" r="1.7"/><path d="M9 17.5c-1.5.6-2.4 2-2.5 3.5 1.5-.1 2.9-1 3.5-2.4M15 17.5c1.5.6 2.4 2 2.5 3.5-1.5-.1-2.9-1-3.5-2.4"/></svg>',
  broadcast:
    '<svg viewBox="0 0 24 24" fill="none"><circle cx="12" cy="12" r="2.4"/><path d="M8.2 8.2a5.4 5.4 0 0 0 0 7.6M15.8 8.2a5.4 5.4 0 0 1 0 7.6M5.6 5.6a9 9 0 0 0 0 12.8M18.4 5.6a9 9 0 0 1 0 12.8"/></svg>',
};

let deps: WizardDeps;
let steps: WizardStep[] = [];
let idx = 0;

let modal: HTMLElement | null = null;
let content: HTMLElement | null = null;
let illus: HTMLElement | null = null;
let titleEl: HTMLElement | null = null;
let bodyEl: HTMLElement | null = null;
let dots: HTMLElement | null = null;
let countEl: HTMLElement | null = null;
let backBtn: HTMLButtonElement | null = null;
let nextBtn: HTMLButtonElement | null = null;

export function initHomeWizard(d: WizardDeps): void {
  deps = d;
  modal = byId('onb-home-modal');
  if (!modal) return;
  content = byId('onb-home-content');
  illus = byId('onb-home-illus');
  titleEl = byId('onb-home-title');
  bodyEl = byId('onb-home-body');
  dots = byId('onb-home-dots');
  countEl = byId('onb-home-count');
  backBtn = byId<HTMLButtonElement>('onb-home-back');
  nextBtn = byId<HTMLButtonElement>('onb-home-next');
  backBtn?.addEventListener('click', prev);
  nextBtn?.addEventListener('click', next);
  byId('onb-home-skip')?.addEventListener('click', close);
  modal.addEventListener('keydown', onKey);
}

export function openHomeWizard(): void {
  if (!modal) return;
  steps = selectWizardSteps({ isLoggedIn: deps.isLoggedIn(), isB2B: deps.isB2B() });
  idx = 0;
  deps.show(modal, true); // focus-trap + ESC + focus-restore (app.ts)
  render();
}

function close(): void {
  if (modal) deps.show(modal, false);
}

function render(): void {
  const s = steps[idx];
  if (!s) return;
  if (illus) illus.innerHTML = GLYPHS[s.glyph] ?? GLYPHS.globe;
  if (titleEl) titleEl.textContent = t(s.titleKey);
  if (bodyEl) bodyEl.textContent = t(s.bodyKey);
  if (dots) {
    dots.innerHTML = steps
      .map((_, i) => `<span class="onb-dot${i === idx ? ' active' : ''}"></span>`)
      .join('');
  }
  if (countEl) {
    countEl.textContent = t('onbStepCounter')
      .replace('{n}', String(idx + 1))
      .replace('{total}', String(steps.length));
  }
  if (backBtn) backBtn.hidden = idx === 0;
  const last = idx === steps.length - 1;
  if (nextBtn) nextBtn.textContent = last ? t('onbHomeCtaButton') : t('onbNext');
  // Restart the per-step transition (a no-op under prefers-reduced-motion, handled globally).
  if (content) {
    content.classList.remove('onb-in');
    void content.offsetWidth;
    content.classList.add('onb-in');
  }
  nextBtn?.focus();
}

function next(): void {
  if (idx < steps.length - 1) {
    idx += 1;
    render();
  } else {
    close();
    byId('name')?.focus(); // land the user on the first real input
  }
}

function prev(): void {
  if (idx > 0) {
    idx -= 1;
    render();
  }
}

function onKey(e: KeyboardEvent): void {
  if (e.key === 'ArrowRight') {
    e.preventDefault();
    next();
  } else if (e.key === 'ArrowLeft') {
    e.preventDefault();
    prev();
  }
}

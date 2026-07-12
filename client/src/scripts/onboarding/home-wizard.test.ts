// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

import en from '../i18n/en.json';

type Wizard = typeof import('./home-wizard');

const MODAL_HTML = `
  <div id="onb-home-modal">
    <div id="onb-home-content">
      <div id="onb-home-illus"></div>
      <h2 id="onb-home-title"></h2>
      <p id="onb-home-body"></p>
    </div>
    <div id="onb-home-dots"></div>
    <span id="onb-home-count"></span>
    <button id="onb-home-back"></button>
    <button id="onb-home-next"></button>
    <button id="onb-home-skip"></button>
  </div>
  <input id="name" />
`;

// The wizard keeps its element refs + step index in module scope, so every test
// gets a fresh module instance via resetModules + dynamic import.
async function setup(
  loggedIn: boolean,
  withModal = true,
  isB2B = false,
): Promise<{ mod: Wizard; show: ReturnType<typeof vi.fn> }> {
  vi.resetModules();
  document.body.innerHTML = withModal ? MODAL_HTML : '<input id="name" />';
  const mod = await import('./home-wizard');
  const show = vi.fn();
  mod.initHomeWizard({ show, isLoggedIn: () => loggedIn, isB2B: () => isB2B });
  return { mod, show };
}

const byId = (id: string): HTMLElement => document.getElementById(id)!;
const title = (): string => byId('onb-home-title').textContent ?? '';
const counter = (): string => byId('onb-home-count').textContent ?? '';
const next = (): HTMLButtonElement => byId('onb-home-next') as HTMLButtonElement;
const back = (): HTMLButtonElement => byId('onb-home-back') as HTMLButtonElement;

const stepCounter = (n: number, total: number): string =>
  en.onbStepCounter.replace('{n}', String(n)).replace('{total}', String(total));

describe('initHomeWizard / openHomeWizard', () => {
  it('does nothing when the modal markup is absent', async () => {
    const { mod, show } = await setup(true, false);
    expect(() => mod.openHomeWizard()).not.toThrow();
    expect(show).not.toHaveBeenCalled();
  });

  it('opens on step 1 with dots, counter, hidden Back and a focused Next', async () => {
    const { mod, show } = await setup(true);
    mod.openHomeWizard();

    expect(show).toHaveBeenCalledWith(byId('onb-home-modal'), true);
    expect(title()).toBe(en.onbHomeWelcomeTitle);
    expect(byId('onb-home-body').textContent).toBe(en.onbHomeWelcomeBody);
    expect(byId('onb-home-illus').querySelector('svg')).not.toBeNull();

    const dots = document.querySelectorAll('#onb-home-dots .onb-dot');
    expect(dots.length).toBe(5); // all(4) + credits (authed)
    expect(dots[0].classList.contains('active')).toBe(true);
    expect(counter()).toBe(stepCounter(1, 5));

    expect(back().hidden).toBe(true);
    expect(next().textContent).toBe(en.onbNext);
    expect(byId('onb-home-content').classList.contains('onb-in')).toBe(true);
    expect(document.activeElement).toBe(next());
  });

  it('shows the credits step to an authed user', async () => {
    const { mod } = await setup(true);
    mod.openHomeWizard();
    next().click();
    next().click();
    next().click();
    expect(title()).toBe(en.onbHomeCreditsTitle);
  });

  it('shows the sign-up pitch to a guest instead', async () => {
    const { mod } = await setup(false);
    mod.openHomeWizard();
    next().click();
    next().click();
    next().click();
    expect(title()).toBe(en.onbHomeGuestTitle);
    expect(counter()).toBe(stepCounter(4, 5));
  });

  it('adds the webinar step for a B2B user (≥1 org) after credits', async () => {
    const { mod } = await setup(true, true, true);
    mod.openHomeWizard();
    // welcome → start → tiers → credits (authed) → webinar (b2b)
    for (let i = 0; i < 4; i++) next().click();
    expect(title()).toBe(en.wizWebinarTitle);
    expect(byId('onb-home-body').textContent).toBe(en.wizWebinarBody);
    expect(document.querySelectorAll('#onb-home-dots .onb-dot').length).toBe(6);
    expect(counter()).toBe(stepCounter(5, 6));
  });

  it('hides the webinar step for a non-B2B authed user', async () => {
    const { mod } = await setup(true, true, false);
    mod.openHomeWizard();
    for (let i = 0; i < 4; i++) next().click();
    expect(title()).toBe(en.onbHomeCtaTitle); // straight to CTA, no webinar step
    expect(document.querySelectorAll('#onb-home-dots .onb-dot').length).toBe(5);
  });
});

describe('navigation', () => {
  it('walks forward to the CTA, back again, and closes into the name field', async () => {
    const { mod, show } = await setup(true);
    mod.openHomeWizard();

    for (let i = 0; i < 4; i++) next().click();
    expect(title()).toBe(en.onbHomeCtaTitle);
    expect(next().textContent).toBe(en.onbHomeCtaButton); // last step CTA label
    expect(back().hidden).toBe(false);
    expect(counter()).toBe(stepCounter(5, 5));

    back().click();
    expect(counter()).toBe(stepCounter(4, 5));
    next().click(); // back to the CTA

    next().click(); // finish
    expect(show).toHaveBeenLastCalledWith(byId('onb-home-modal'), false);
    expect(document.activeElement?.id).toBe('name');
  });

  it('does not step back past the first step', async () => {
    const { mod } = await setup(true);
    mod.openHomeWizard();
    back().click();
    expect(counter()).toBe(stepCounter(1, 5));
  });

  it('navigates with ← / → and ignores other keys', async () => {
    const { mod } = await setup(true);
    mod.openHomeWizard();
    const modal = byId('onb-home-modal');

    modal.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight' }));
    expect(counter()).toBe(stepCounter(2, 5));

    modal.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowLeft' }));
    expect(counter()).toBe(stepCounter(1, 5));

    modal.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter' }));
    expect(counter()).toBe(stepCounter(1, 5));
  });

  it('re-opening resets to step 1', async () => {
    const { mod } = await setup(true);
    mod.openHomeWizard();
    next().click();
    mod.openHomeWizard();
    expect(counter()).toBe(stepCounter(1, 5));
  });

  it('Skip closes the wizard', async () => {
    const { mod, show } = await setup(true);
    mod.openHomeWizard();
    byId('onb-home-skip').click();
    expect(show).toHaveBeenLastCalledWith(byId('onb-home-modal'), false);
  });
});

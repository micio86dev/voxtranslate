// @vitest-environment jsdom
// Unit tests for the always-available "Report a problem" FAB + modal (spec 0071).
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const postBugReport = vi.hoisted(() => vi.fn());
vi.mock('./api', () => ({ postBugReport }));

import { initBugReport } from './bug-report';
import { t } from './i18n';

function mount(withIcoSpan = true): void {
  document.body.innerHTML = `
    <button id="bug-report-btn" aria-expanded="false">${withIcoSpan ? '<span class="bug-fab-ico"></span>' : ''}</button>
    <div id="bug-report-modal" class="hidden"><div class="bug-dialog"></div></div>
    <button id="bug-report-close"></button>
    <textarea id="bug-report-text"></textarea>
    <button id="bug-report-send"></button>
    <p id="bug-report-status"></p>`;
}

const $ = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;
const modal = (): HTMLElement => $('bug-report-modal');
const isOpen = (): boolean => !modal().classList.contains('hidden');

beforeEach(() => {
  postBugReport.mockReset();
  mount();
  initBugReport();
});

afterEach(() => {
  vi.useRealTimers();
  document.body.innerHTML = '';
});

describe('initBugReport wiring', () => {
  it('bails quietly when the markup is missing', () => {
    document.body.innerHTML = '';
    expect(() => initBugReport()).not.toThrow();
  });

  it('tolerates a button without the icon span', () => {
    mount(false);
    expect(() => initBugReport()).not.toThrow();
  });

  it('injects the bug + close icons', () => {
    expect($('bug-report-btn').querySelector('.bug-fab-ico')?.innerHTML).toContain('<svg');
    expect($('bug-report-close').innerHTML).toContain('<svg');
  });
});

describe('open/close behavior', () => {
  it('opens on the FAB click, clearing status and focusing the textarea', () => {
    $('bug-report-status').textContent = 'stale';
    $<HTMLButtonElement>('bug-report-btn').click();
    expect(isOpen()).toBe(true);
    expect($('bug-report-btn').getAttribute('aria-expanded')).toBe('true');
    expect($('bug-report-status').textContent).toBe('');
    expect(document.activeElement).toBe($('bug-report-text'));
  });

  it('closes via the close button', () => {
    $<HTMLButtonElement>('bug-report-btn').click();
    $<HTMLButtonElement>('bug-report-close').click();
    expect(isOpen()).toBe(false);
    expect($('bug-report-btn').getAttribute('aria-expanded')).toBe('false');
  });

  it('closes on a backdrop click but not on a click inside the dialog', () => {
    $<HTMLButtonElement>('bug-report-btn').click();
    modal()
      .querySelector('.bug-dialog')!
      .dispatchEvent(new MouseEvent('click', { bubbles: true }));
    expect(isOpen()).toBe(true);
    modal().dispatchEvent(new MouseEvent('click', { bubbles: true }));
    expect(isOpen()).toBe(false);
  });

  it('closes on Escape only while open', () => {
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    expect(isOpen()).toBe(false); // stays closed, no crash
    $<HTMLButtonElement>('bug-report-btn').click();
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    expect(isOpen()).toBe(false);
  });
});

describe('sending', () => {
  it('warns on an empty message without calling the API', async () => {
    $<HTMLButtonElement>('bug-report-btn').click();
    $<HTMLTextAreaElement>('bug-report-text').value = '   ';
    $<HTMLButtonElement>('bug-report-send').click();
    await Promise.resolve();
    expect(postBugReport).not.toHaveBeenCalled();
    expect($('bug-report-status').textContent).toBe(t('bugReportEmpty'));
  });

  it('sends the trimmed message + page URL, then auto-closes after 1.3s', async () => {
    vi.useFakeTimers();
    postBugReport.mockResolvedValue(true);
    $<HTMLButtonElement>('bug-report-btn').click();
    $<HTMLTextAreaElement>('bug-report-text').value = '  mic died  ';
    const send = $<HTMLButtonElement>('bug-report-send');
    send.click();
    expect(send.disabled).toBe(true); // disabled while in flight
    await vi.advanceTimersByTimeAsync(0);
    expect(postBugReport).toHaveBeenCalledWith('mic died', '/');
    expect(send.disabled).toBe(false);
    expect($('bug-report-status').textContent).toBe(t('bugReportSent'));
    expect($<HTMLTextAreaElement>('bug-report-text').value).toBe('');
    expect(isOpen()).toBe(true); // success message lingers…
    await vi.advanceTimersByTimeAsync(1300);
    expect(isOpen()).toBe(false); // …then the modal closes itself
  });

  it('shows the error and keeps the draft when the POST fails', async () => {
    vi.useFakeTimers();
    postBugReport.mockResolvedValue(false);
    $<HTMLButtonElement>('bug-report-btn').click();
    $<HTMLTextAreaElement>('bug-report-text').value = 'no audio';
    $<HTMLButtonElement>('bug-report-send').click();
    await vi.advanceTimersByTimeAsync(0);
    expect($('bug-report-status').textContent).toBe(t('bugReportError'));
    expect($<HTMLTextAreaElement>('bug-report-text').value).toBe('no audio');
    await vi.advanceTimersByTimeAsync(5000);
    expect(isOpen()).toBe(true); // no auto-close on failure
  });
});

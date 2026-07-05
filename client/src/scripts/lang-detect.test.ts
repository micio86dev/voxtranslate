// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from 'vitest';

import { SUPPORTED } from './i18n';
import { dismissLangToast, initLangDetect, onLanguageDetected } from './lang-detect';

let send: Mock<(msg: Record<string, unknown>) => void>;
let restartCapture: Mock<() => void>;

const toastEl = (): HTMLElement | null => document.querySelector<HTMLElement>('.lang-toast');

function openPicker(): HTMLSelectElement {
  toastEl()!.querySelector<HTMLButtonElement>('.lang-toast-change')!.click();
  return toastEl()!.querySelector<HTMLSelectElement>('.lang-toast-select')!;
}

beforeEach(() => {
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
  vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback): number => {
    cb(0);
    return 0;
  });
  send = vi.fn();
  restartCapture = vi.fn();
  initLangDetect({ send, restartCapture });
});

afterEach(() => {
  dismissLangToast(); // reset module state between tests
  document.body.innerHTML = '';
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe('onLanguageDetected', () => {
  it('shows an interactive toast with the endonym + flag', () => {
    onLanguageDetected('it');
    const el = toastEl()!;
    expect(el.getAttribute('role')).toBe('status');
    expect(el.classList.contains('show')).toBe(true); // rAF ran synchronously
    expect(el.querySelector('.lang-toast-text')?.textContent).toBe(
      'Detected language: Italiano 🇮🇹',
    );
    expect(el.querySelector('.lang-toast-change')?.textContent).toBe('Change');
  });

  it('upper-cases an unknown code instead of showing nothing', () => {
    onLanguageDetected('zz');
    expect(toastEl()!.querySelector('.lang-toast-text')?.textContent).toBe(
      'Detected language: ZZ',
    );
  });

  it('auto-dismisses after 8s plus the fade', () => {
    onLanguageDetected('it');
    const el = toastEl()!;
    vi.advanceTimersByTime(8000);
    expect(el.classList.contains('show')).toBe(false);
    expect(document.body.contains(el)).toBe(true);
    vi.advanceTimersByTime(300);
    expect(document.body.contains(el)).toBe(false);
  });

  it('replaces a previous toast instead of stacking', () => {
    onLanguageDetected('it');
    onLanguageDetected('fr');
    const toasts = document.querySelectorAll('.lang-toast');
    expect(toasts.length).toBe(1);
    expect(toasts[0].querySelector('.lang-toast-text')?.textContent).toContain('Français');
  });
});

describe('dismissLangToast', () => {
  it('removes the toast instantly and cancels the pending auto-dismiss', () => {
    onLanguageDetected('it');
    dismissLangToast();
    expect(toastEl()).toBeNull();
    expect(() => vi.advanceTimersByTime(10_000)).not.toThrow();
  });

  it('is a no-op when nothing is showing', () => {
    expect(() => dismissLangToast()).not.toThrow();
  });
});

describe('the Change picker', () => {
  it('swaps the button for a focused select of every UI language, detected preselected', () => {
    onLanguageDetected('it');
    const sel = openPicker();
    expect(toastEl()!.querySelector('.lang-toast-change')).toBeNull(); // replaced
    expect(sel.options.length).toBe(SUPPORTED.length);
    expect(sel.value).toBe('it');
    expect(document.activeElement).toBe(sel);
  });

  it('extends the auto-dismiss window to 20s while the picker is open', () => {
    onLanguageDetected('it');
    openPicker();
    vi.advanceTimersByTime(8000); // the original window has been cancelled
    expect(toastEl()).not.toBeNull();
    vi.advanceTimersByTime(12_000); // …the extended one fires at 20s
    vi.advanceTimersByTime(300);
    expect(toastEl()).toBeNull();
  });

  it('sends set_lang and restarts capture when a different language is picked', () => {
    onLanguageDetected('it');
    const sel = openPicker();
    sel.value = 'fr';
    sel.dispatchEvent(new Event('change'));
    expect(send).toHaveBeenCalledWith({ type: 'set_lang', lang: 'fr' });
    expect(restartCapture).toHaveBeenCalledOnce();
    vi.advanceTimersByTime(300);
    expect(toastEl()).toBeNull(); // fades out after the choice
  });

  it('just closes when the detected language is confirmed unchanged', () => {
    onLanguageDetected('it');
    const sel = openPicker();
    sel.dispatchEvent(new Event('change')); // still "it"
    expect(send).not.toHaveBeenCalled();
    expect(restartCapture).not.toHaveBeenCalled();
    vi.advanceTimersByTime(300);
    expect(toastEl()).toBeNull();
  });
});

// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { toast } from './toast';

beforeEach(() => {
  // Only fake the timeout pair — rAF is stubbed synchronously below so the
  // `.show` class lands deterministically without a frame loop.
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
  vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback): number => {
    cb(0);
    return 0;
  });
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  document.body.innerHTML = '';
});

describe('toast', () => {
  it('shows an info toast (default kind) and auto-dismisses after 3.5s + fade', () => {
    toast('saved');
    const el = document.querySelector('.vox-toast');
    expect(el).not.toBeNull();
    expect(el!.textContent).toBe('saved');
    expect(el!.getAttribute('role')).toBe('status');
    // rAF stub ran synchronously → the enter transition class is already on.
    expect(el!.className).toBe('vox-toast show');

    vi.advanceTimersByTime(3500);
    expect(el!.classList.contains('show')).toBe(false);
    expect(document.body.contains(el!)).toBe(true); // fading out, not yet removed

    vi.advanceTimersByTime(300);
    expect(document.body.contains(el!)).toBe(false);
  });

  it('renders an error toast as a role=alert with the err modifier', () => {
    toast('boom', 'err');
    const el = document.querySelector('.vox-toast')!;
    expect(el.className).toBe('vox-toast err show');
    expect(el.getAttribute('role')).toBe('alert');
  });

  it('renders a success toast as a role=status with the ok modifier', () => {
    toast('done', 'ok');
    const el = document.querySelector('.vox-toast')!;
    expect(el.className).toBe('vox-toast ok show');
    expect(el.getAttribute('role')).toBe('status');
  });

  it('marks a success toast with a check glyph, hidden from screen readers', () => {
    toast('Link copied', 'ok');
    const el = document.querySelector('.vox-toast')!;
    const ico = el.querySelector('.vox-toast-ico')!;
    expect(ico).not.toBeNull();
    expect(ico.textContent).toBe('✓');
    // The glyph is decoration: the announced text must stay just the message.
    expect(ico.getAttribute('aria-hidden')).toBe('true');
    expect(el.querySelector('.vox-toast-msg')!.textContent).toBe('Link copied');
  });

  it('leaves info and error toasts glyph-free', () => {
    toast('saved');
    toast('boom', 'err');
    expect(document.querySelectorAll('.vox-toast-ico')).toHaveLength(0);
  });

  it('can stack several toasts independently', () => {
    toast('one');
    toast('two', 'ok');
    expect(document.querySelectorAll('.vox-toast').length).toBe(2);
    vi.advanceTimersByTime(3800);
    expect(document.querySelectorAll('.vox-toast').length).toBe(0);
  });
});

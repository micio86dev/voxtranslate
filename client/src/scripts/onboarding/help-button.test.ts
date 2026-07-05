// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';

import { wireHelpButton } from './help-button';

afterEach(() => {
  document.body.innerHTML = '';
});

describe('wireHelpButton', () => {
  it('is a silent no-op when the button is not in the DOM', () => {
    const launch = vi.fn();
    expect(() => wireHelpButton('missing-help', launch)).not.toThrow();
    expect(launch).not.toHaveBeenCalled();
  });

  it('launches the guide on click and prevents the default action', () => {
    document.body.innerHTML = '<button id="onb-home-help"></button>';
    const launch = vi.fn();
    wireHelpButton('onb-home-help', launch);

    const ev = new MouseEvent('click', { bubbles: true, cancelable: true });
    document.getElementById('onb-home-help')!.dispatchEvent(ev);

    expect(launch).toHaveBeenCalledOnce();
    expect(ev.defaultPrevented).toBe(true);
  });

  it('always relaunches — the button ignores the tour-seen flags', () => {
    document.body.innerHTML = '<button id="onb-call-help"></button>';
    const launch = vi.fn();
    wireHelpButton('onb-call-help', launch);
    const btn = document.getElementById('onb-call-help')!;
    btn.dispatchEvent(new MouseEvent('click', { cancelable: true }));
    btn.dispatchEvent(new MouseEvent('click', { cancelable: true }));
    expect(launch).toHaveBeenCalledTimes(2);
  });
});

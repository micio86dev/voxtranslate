// @vitest-environment jsdom
// Circular participant avatars, shared by the call app and /world.
import { beforeEach, describe, expect, it, vi } from 'vitest';

const avatarUrl = vi.fn<(url: string | null | undefined, size: number) => string | null>();
vi.mock('./auth', () => ({ avatarUrl: (u: string | null | undefined, s: number) => avatarUrl(u, s) }));

import { avatarGradient, fillAvatar } from './avatar';

beforeEach(() => {
  avatarUrl.mockReset();
  document.body.innerHTML = '';
});

const host = (): HTMLElement => {
  const el = document.createElement('span');
  document.body.appendChild(el);
  return el;
};

describe('avatarGradient', () => {
  it('is stable for a name, so the same person always looks the same', () => {
    expect(avatarGradient('Marco')).toBe(avatarGradient('Marco'));
  });

  it('separates different names', () => {
    expect(avatarGradient('Marco')).not.toBe(avatarGradient('Yuki'));
  });

  it('is a gradient the element can take as a background', () => {
    expect(avatarGradient('Marco')).toMatch(/^linear-gradient\(135deg, hsl\(\d+,60%,25%\)/);
  });
});

describe('fillAvatar', () => {
  it('falls back to initials when there is no picture', () => {
    avatarUrl.mockReturnValue(null);
    const el = host();
    fillAvatar(el, 'Marco', null, 56, 1);
    expect(el.textContent).toBe('M');
    expect(el.querySelector('img')).toBeNull();
    expect(el.style.background).toContain('linear-gradient');
  });

  it('honours the requested number of initials', () => {
    avatarUrl.mockReturnValue(null);
    const el = host();
    fillAvatar(el, 'Marco', null, 64, 2);
    expect(el.textContent).toBe('MA');
  });

  it('renders the picture at the requested size when one exists', () => {
    avatarUrl.mockReturnValue('https://cdn.test/a.png');
    const el = host();
    fillAvatar(el, 'Yuki', 'https://cdn.test/raw.png', 56, 1);
    expect(avatarUrl).toHaveBeenCalledWith('https://cdn.test/raw.png', 56);
    const img = el.querySelector('img')!;
    expect(img.src).toBe('https://cdn.test/a.png');
    // Google avatars 403 when a referrer is sent.
    expect(img.referrerPolicy).toBe('no-referrer');
    // Decorative: the name is already rendered next to it.
    expect(img.alt).toBe('');
  });

  it('drops back to initials when the picture fails to load', () => {
    avatarUrl.mockReturnValue('https://cdn.test/gone.png');
    const el = host();
    fillAvatar(el, 'Yuki', 'https://cdn.test/gone.png', 56, 1);
    el.querySelector('img')!.dispatchEvent(new Event('error'));
    expect(el.querySelector('img')).toBeNull();
    expect(el.textContent).toBe('Y');
  });

  it('replaces whatever the element held before', () => {
    avatarUrl.mockReturnValue(null);
    const el = host();
    el.textContent = 'stale';
    fillAvatar(el, 'Marco', null, 56, 1);
    expect(el.textContent).toBe('M');
  });
});

// Circular participant avatars, shared by the call app and the /world discovery
// page. Extracted from app.ts so both render an identical face — `recording/utils.ts`
// already re-implements the same hue hash, which is one copy too many.

import { avatarUrl } from './auth';

/** Stable brand-tinted gradient per name, so the same person always looks the same. */
export function avatarGradient(name: string): string {
  let hash = 0;
  for (const ch of name) hash = ch.charCodeAt(0) + ((hash << 5) - hash);
  const hue = Math.abs(hash) % 360;
  return `linear-gradient(135deg, hsl(${hue},60%,25%), hsl(${(hue + 40) % 360},60%,15%))`;
}

/**
 * Fill a circular avatar element with the user's picture, falling back to a
 * gradient + initials when no URL exists or the image fails to load (spec 0070
 * R2.3). Mirrors the in-cell avatar so the participant badge + roster show real
 * faces instead of a single letter.
 */
export function fillAvatar(
  el: HTMLElement,
  name: string,
  avatarSrc: string | null | undefined,
  sizePx: number,
  initialsLen = 2,
): void {
  const initials = name.slice(0, initialsLen).toUpperCase();
  el.textContent = '';
  el.style.background = avatarGradient(name);
  const av = avatarUrl(avatarSrc, sizePx);
  if (!av) {
    el.textContent = initials;
    return;
  }
  const img = document.createElement('img');
  img.referrerPolicy = 'no-referrer';
  img.alt = '';
  img.src = av;
  // Keep the gradient + initials if a (Google) avatar 404s or is blocked.
  img.addEventListener('error', () => {
    img.remove();
    el.textContent = initials;
  });
  el.appendChild(img);
}

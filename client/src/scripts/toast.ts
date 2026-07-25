// Transient bottom-center toast, created on document.body. Shared by app.ts and
// the session-detail AI tools (report/sentiment/email) so async actions can
// report completion or failure without a per-module implementation. Styling
// lives in index.astro under `.vox-toast` (+ `.ok` / `.err` modifiers).

export type ToastKind = 'info' | 'ok' | 'err';

export function toast(msg: string, kind: ToastKind = 'info'): void {
  const el = document.createElement('div');
  el.className = kind === 'info' ? 'vox-toast' : `vox-toast ${kind}`;
  el.setAttribute('role', kind === 'err' ? 'alert' : 'status');
  if (kind === 'ok') {
    // A success toast leads with a check so the outcome reads before the text does.
    // Decoration only — aria-hidden keeps the announcement to the message itself.
    const ico = document.createElement('span');
    ico.className = 'vox-toast-ico';
    ico.setAttribute('aria-hidden', 'true');
    ico.textContent = '✓';
    const text = document.createElement('span');
    text.className = 'vox-toast-msg';
    text.textContent = msg;
    el.append(ico, text);
  } else {
    el.textContent = msg;
  }
  document.body.appendChild(el);
  requestAnimationFrame(() => el.classList.add('show'));
  setTimeout(() => {
    el.classList.remove('show');
    setTimeout(() => el.remove(), 300);
  }, 3500);
}

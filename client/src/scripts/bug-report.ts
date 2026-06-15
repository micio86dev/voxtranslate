// Always-available "Report a problem" button + modal (spec 0071). Any user (guest
// or signed-in) can describe a problem; it POSTs to /api/bug-report, which stores it
// and emails the admins. Kept tiny and self-contained — wired once at startup.

import { postBugReport } from './api';
import { t } from './i18n';
import { icon } from './icons';

export function initBugReport(): void {
  const btn = document.getElementById('bug-report-btn');
  const modal = document.getElementById('bug-report-modal');
  const closeBtn = document.getElementById('bug-report-close');
  const textarea = document.getElementById('bug-report-text') as HTMLTextAreaElement | null;
  const sendBtn = document.getElementById('bug-report-send') as HTMLButtonElement | null;
  const status = document.getElementById('bug-report-status');
  if (!btn || !modal || !closeBtn || !textarea || !sendBtn || !status) return;

  const ico = btn.querySelector('.bug-fab-ico');
  if (ico) ico.innerHTML = icon('bug', 16);
  closeBtn.innerHTML = icon('close', 16);

  const setStatus = (text: string, color: string): void => {
    status.textContent = text;
    status.style.color = color;
  };
  const open = (show: boolean): void => {
    modal.classList.toggle('hidden', !show);
    btn.setAttribute('aria-expanded', String(show));
    if (show) {
      setStatus('', '');
      textarea.focus();
    }
  };

  btn.addEventListener('click', () => open(true));
  closeBtn.addEventListener('click', () => open(false));
  // Click the backdrop (not the dialog) to dismiss.
  modal.addEventListener('click', (e) => {
    if (e.target === modal) open(false);
  });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && !modal.classList.contains('hidden')) open(false);
  });

  sendBtn.addEventListener('click', async () => {
    const msg = textarea.value.trim();
    if (!msg) {
      setStatus(t('bugReportEmpty'), 'var(--warning)');
      return;
    }
    sendBtn.disabled = true;
    setStatus('', '');
    const ok = await postBugReport(msg, location.pathname + location.search);
    sendBtn.disabled = false;
    if (ok) {
      setStatus(t('bugReportSent'), 'var(--success)');
      textarea.value = '';
      window.setTimeout(() => open(false), 1300);
    } else {
      setStatus(t('bugReportError'), 'var(--danger)');
    }
  });
}

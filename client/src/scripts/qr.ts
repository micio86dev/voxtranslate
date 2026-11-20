// QR helpers shared by the webinar flow (app.ts) and Live Connect (scripts/live).
//
// Extracted verbatim from app.ts so a standalone page (/live) can render a QR without
// importing the ~9k-line call controller. The `qrcode` package stays a lazy chunk: it is
// only fetched when a QR is actually drawn.

import { t } from './i18n';
import { toast } from './toast';
import { qrDownloadFilename } from './webinar';

/** Render a QR of `text` into `img` as a data URL. The `qrcode` browser bundle is a
 *  lazy chunk (dynamic import), so it never weighs on the main app load. */
export async function renderQr(img: HTMLImageElement, text: string): Promise<void> {
  try {
    const { toDataURL } = await import('qrcode');
    img.src = await toDataURL(text, { margin: 1, width: 160 });
  } catch {
    // QR is a convenience — the copyable link still works if the chunk fails.
    img.classList.add('hidden');
  }
}

/** Generate a larger, print/download-quality QR data URL for `text`. Falls back to
 *  the already-rendered card `<img>` src if the lazy chunk fails to load. */
export async function qrDataUrl(text: string, fallbackSrc: string): Promise<string> {
  try {
    const { toDataURL } = await import('qrcode');
    return await toDataURL(text, { margin: 2, width: 512 });
  } catch {
    return fallbackSrc;
  }
}

/** Save a QR data URL as a PNG file (`webinar-{code}.png`). */
export function downloadQr(dataUrl: string, code: string): void {
  const a = document.createElement('a');
  a.href = dataUrl;
  a.download = qrDownloadFilename(code);
  document.body.appendChild(a);
  a.click();
  a.remove();
}

/** Open a print window showing just the QR + the webinar title and join URL, centered.
 *  Built with DOM APIs (no `document.write`); user-supplied text is set via
 *  `textContent`, so there's no HTML-injection surface. */
export function printQr(dataUrl: string, title: string, joinUrl: string): void {
  const win = window.open('', '_blank', 'width=520,height=640');
  if (!win) {
    toast(t('webinarQrPrintBlocked'), 'err');
    return;
  }
  const doc = win.document;
  doc.title = title;
  const style = doc.createElement('style');
  style.textContent =
    'html,body{height:100%;margin:0}' +
    'body{display:flex;flex-direction:column;align-items:center;justify-content:center;' +
    'gap:16px;font-family:system-ui,-apple-system,sans-serif;padding:24px;text-align:center}' +
    'h1{font-size:20px;margin:0}img{width:320px;height:320px}' +
    'p{margin:0;color:#444;word-break:break-all;max-width:360px}';
  doc.head.appendChild(style);
  const h1 = doc.createElement('h1');
  h1.textContent = title;
  const img = doc.createElement('img');
  img.src = dataUrl;
  img.alt = '';
  const p = doc.createElement('p');
  p.textContent = joinUrl;
  doc.body.append(h1, img, p);
  const fire = () => {
    win.focus();
    win.print();
  };
  // Print once the QR image has decoded, else fall back to a short delay.
  if (img.complete) fire();
  else {
    img.addEventListener('load', fire, { once: true });
    img.addEventListener('error', fire, { once: true });
  }
}

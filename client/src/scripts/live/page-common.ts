// Live Connect — page bootstrap shared by /live and /lc/<code> (spec 0121).
//
// Standalone pages never load app.ts, so each does its own minimal boot: pick the UI
// language, paint the server-rendered English in the user's language, and stop the browser
// from leaking the invitation link in a Referer header.

import { applyI18n, loadLocale, resolveStoredLang, setUiLang } from '../i18n';
import { initNetStatus } from '../net-status';

export function initPage(doc: Document = document): string {
  const lang = resolveStoredLang();
  setUiLang(lang);
  applyI18n();
  void loadLocale(lang).then(applyI18n);
  // The offline / reconnecting pill: a phone leaving Wi-Fi is the normal way this breaks.
  initNetStatus();
  // vercel.json sends the same policy as a header; the meta covers a static host or a
  // header-stripping proxy. An invitation link is a bearer secret.
  if (!doc.querySelector('meta[name="referrer"]')) {
    const meta = doc.createElement('meta');
    meta.name = 'referrer';
    meta.content = 'no-referrer';
    doc.head.append(meta);
  }
  return lang;
}

export function byId<T extends HTMLElement = HTMLElement>(id: string, doc: Document = document): T | null {
  return doc.getElementById(id) as T | null;
}

export function setHidden(id: string, hidden: boolean, doc: Document = document): void {
  const el = doc.getElementById(id);
  if (el) el.hidden = hidden;
}

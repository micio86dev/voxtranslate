// Runtime-managed content from the backoffice (Directus → server → here).
// UI strings and legal pages live in the database so editors can change copy and
// translations without a redeploy; the client fetches them and layers them over
// its bundled defaults. Every fetch fails safe: on any error the app keeps the
// strings/pages it shipped with, so it works offline and if the API is down.

import { HTTP_BASE } from './auth';
import { I18N } from './i18n';

async function getJson(url: string, timeoutMs = 3000): Promise<unknown | null> {
  try {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), timeoutMs);
    // Default caching: honour the server's short Cache-Control so we don't
    // refetch the full string map on every boot (the server sets a ~60s TTL
    // with background revalidation).
    const res = await fetch(url, { signal: ctrl.signal });
    clearTimeout(timer);
    if (!res.ok) return null;
    return await res.json();
  } catch {
    return null;
  }
}

/**
 * Fetch DB-managed UI strings (`{ lang: { key: value } }`) and merge them over
 * the bundled `I18N` defaults — DB values win. Returns true if anything merged.
 */
export async function loadRemoteI18n(base: string = HTTP_BASE, timeoutMs = 3000): Promise<boolean> {
  const data = await getJson(`${base}/api/content/i18n`, timeoutMs);
  if (!data || typeof data !== 'object') return false;
  let merged = false;
  for (const [lang, dict] of Object.entries(data as Record<string, Record<string, string>>)) {
    if (dict && typeof dict === 'object') {
      I18N[lang] = { ...(I18N[lang] || {}), ...dict };
      merged = true;
    }
  }
  return merged;
}

export interface LegalDoc {
  slug: string;
  version: string;
  title: string;
  body: string;
}

/**
 * Fetch a managed legal page for `slug` in `lang`. Returns null when the page
 * isn't managed (the caller then keeps its bundled copy).
 */
export async function fetchLegal(
  slug: string,
  lang: string,
  base: string = HTTP_BASE,
): Promise<LegalDoc | null> {
  const url = `${base}/api/content/legal/${encodeURIComponent(slug)}?lang=${encodeURIComponent(lang)}`;
  const data = await getJson(url);
  if (!data || typeof (data as LegalDoc).body !== 'string') return null;
  return data as LegalDoc;
}

/**
 * Minimal markdown → HTML for admin-authored legal pages: `#`/`##`/`###`
 * headings, `-`/`*` lists, `**bold**`, `[text](url)` links, and paragraphs.
 * Input is authored in the trusted backoffice; raw HTML angle brackets are
 * escaped so stray markup can't break the page.
 */
export function renderMarkdown(md: string): string {
  const esc = (s: string) =>
    s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  const inline = (s: string) =>
    esc(s)
      .replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>')
      .replace(/\[([^\]]+)\]\(([^)]+)\)/g, (_m, text, url) => {
        // Block dangerous schemes only — encodeURI() does NOT strip `javascript:`/
        // `data:`, so `[x](javascript:…)` would be a click-to-XSS. Relative links
        // (/terms) and http(s)/mailto stay intact. Defense-in-depth (spec 0028).
        const safe = /^\s*(javascript|data|vbscript|file):/i.test(url) ? '#' : encodeURI(url);
        return `<a href="${safe}" target="_blank" rel="noopener">${text}</a>`;
      });

  const lines = md.replace(/\r\n/g, '\n').split('\n');
  const out: string[] = [];
  let inList = false;
  let para: string[] = [];
  const closeList = () => {
    if (inList) {
      out.push('</ul>');
      inList = false;
    }
  };
  const flushPara = () => {
    if (para.length) {
      out.push(`<p>${inline(para.join(' '))}</p>`);
      para = [];
    }
  };

  for (const raw of lines) {
    const line = raw.trim();
    if (!line) {
      flushPara();
      closeList();
      continue;
    }
    const heading = /^(#{1,3})\s+(.*)$/.exec(line);
    if (heading) {
      flushPara();
      closeList();
      const lvl = heading[1].length;
      out.push(`<h${lvl}>${inline(heading[2])}</h${lvl}>`);
      continue;
    }
    if (/^[-*]\s+/.test(line)) {
      flushPara();
      if (!inList) {
        out.push('<ul>');
        inList = true;
      }
      out.push(`<li>${inline(line.replace(/^[-*]\s+/, ''))}</li>`);
      continue;
    }
    para.push(line);
  }
  flushPara();
  closeList();
  return out.join('\n');
}

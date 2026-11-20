import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

interface Rule {
  source: string;
  headers: Array<{ key: string; value: string }>;
}
const rules = (JSON.parse(readFileSync(new URL('../../../vercel.json', import.meta.url), 'utf8')) as { headers: Rule[] }).headers;

/** What Vercel would send for `path`, later rules winning per header like the platform's merge. */
function headersFor(path: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const r of rules) {
    if (!new RegExp(`^${r.source}$`).test(path)) continue;
    for (const h of r.headers) out[h.key] = h.value;
  }
  return out;
}

describe('headers of the Live Connect routes', () => {
  it('a desk QR URL is never cached and never leaks through a Referer header (the segment is a bearer secret)', () => {
    const h = headersFor('/lc/d/pub123.SECRETMAC');
    expect(h['Referrer-Policy']).toBe('no-referrer');
    expect(h['Cache-Control']).toBe('private, no-store');
  });

  it('the staff desk list ships the no-referrer policy like /live', () => {
    expect(headersFor('/live/desks')['Referrer-Policy']).toBe('no-referrer');
    expect(headersFor('/live')['Referrer-Policy']).toBe('no-referrer');
  });

  it('leaves every other page on the default policy', () => {
    expect(headersFor('/talk')['Referrer-Policy']).toBe('strict-origin-when-cross-origin');
  });
});

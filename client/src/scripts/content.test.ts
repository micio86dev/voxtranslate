import { describe, it, expect, vi } from 'vitest';

// content.ts statically imports auth.ts, which reads `location` at module-eval
// time. `vi.hoisted` runs before the imports are evaluated, so the global is in
// place when auth.ts loads.
vi.hoisted(() => {
  (globalThis as unknown as { location: unknown }).location = {
    protocol: 'http:',
    host: 'localhost:4321',
  };
  (globalThis as unknown as { localStorage: unknown }).localStorage = {
    getItem: () => null,
    setItem: () => {},
    removeItem: () => {},
  };
});

import { loadRemoteI18n, fetchLegal, renderMarkdown } from './content';
import { I18N, loadLocale } from './i18n';

function okJson(body: unknown, status = 200) {
  return { ok: status >= 200 && status < 300, status, json: async () => body } as Response;
}

describe('loadRemoteI18n', () => {
  it('merges DB overrides over the bundled defaults (DB wins)', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(okJson({ en: { connect: 'Join now', brandNew: 'Fresh' } })),
    );
    expect(await loadRemoteI18n('http://x')).toBe(true);
    expect(I18N.en.connect).toBe('Join now'); // overridden
    expect(I18N.en.brandNew).toBe('Fresh'); // added
  });

  it('returns false and keeps defaults on a non-ok response', async () => {
    await loadLocale('it'); // lazy locales aren't in memory until loaded (spec 0104)
    const before = I18N.it.connect;
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('no', 503)));
    expect(await loadRemoteI18n('http://x')).toBe(false);
    expect(I18N.it.connect).toBe(before);
  });

  it('returns false when fetch throws (offline)', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new Error('net')));
    expect(await loadRemoteI18n('http://x')).toBe(false);
  });
});

describe('fetchLegal', () => {
  it('returns the document when present', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(okJson({ slug: 'terms', version: 'v1', title: 'T', body: '# Hi' })),
    );
    const doc = await fetchLegal('terms', 'en', 'http://x');
    expect(doc?.title).toBe('T');
    expect(doc?.body).toBe('# Hi');
  });

  it('returns null on 404 (unmanaged page → bundled copy)', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson('nope', 404)));
    expect(await fetchLegal('terms', 'en', 'http://x')).toBeNull();
  });

  it('returns null when the payload has no body', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okJson({ slug: 'terms' })));
    expect(await fetchLegal('terms', 'en', 'http://x')).toBeNull();
  });
});

describe('renderMarkdown', () => {
  it('renders headings, lists, bold, links and paragraphs', () => {
    const html = renderMarkdown(
      '# Title\n\nIntro **bold** text.\n\n## Section\n\n- one\n- two\n\nSee [terms](/terms).',
    );
    expect(html).toContain('<h1>Title</h1>');
    expect(html).toContain('<h2>Section</h2>');
    expect(html).toContain('<strong>bold</strong>');
    expect(html).toContain('<ul>');
    expect(html).toContain('<li>one</li>');
    expect(html).toContain('<a href="/terms" target="_blank" rel="noopener">terms</a>');
    expect(html).toContain('<p>Intro <strong>bold</strong> text.</p>');
  });

  it('escapes raw angle brackets in text', () => {
    const html = renderMarkdown('a <script>evil</script> b');
    expect(html).toContain('&lt;script&gt;');
    expect(html).not.toContain('<script>');
  });

  it('neutralizes dangerous link schemes but keeps relative + http links (spec 0028)', () => {
    expect(renderMarkdown('[x](javascript:alert(1))')).toContain('href="#"');
    expect(renderMarkdown('[x](javascript:alert(1))')).not.toContain('javascript:');
    expect(renderMarkdown('[x](DATA:text/html,evil)')).toContain('href="#"');
    // Legitimate links are untouched.
    expect(renderMarkdown('[t](/terms)')).toContain('href="/terms"');
    expect(renderMarkdown('[h](https://x.test)')).toContain('href="https://x.test"');
  });
});

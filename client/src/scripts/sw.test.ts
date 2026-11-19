// Fetch-handler behaviour of public/sw.js: what it may and may not write to the Cache Storage.
// The file is a classic worker script, so it is evaluated in a vm context with fakes for
// `self`, `caches` and `indexedDB` (the push/IndexedDB parts are covered in push.test.ts).
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { describe, expect, it } from 'vitest';

const SOURCE = readFileSync(new URL('../../public/sw.js', import.meta.url), 'utf8');
const ORIGIN = 'https://app.example';

type Handler = (event: unknown) => void;

function load(network: (url: string) => Response | Promise<Response>) {
  const handlers: Record<string, Handler> = {};
  const stored = new Map<string, Response>();
  const cache = {
    put: async (key: string | { url: string }, res: Response) => {
      stored.set(typeof key === 'string' ? key : new URL(key.url).pathname, res);
    },
    addAll: async () => {},
  };
  const caches = {
    open: async () => cache,
    match: async (key: string | { url: string }) =>
      stored.get(typeof key === 'string' ? key : new URL(key.url).pathname),
    keys: async () => [],
    delete: async () => true,
  };
  const self = {
    location: { origin: ORIGIN },
    addEventListener: (type: string, h: Handler) => (handlers[type] = h),
    skipWaiting: async () => {},
    clients: { claim: async () => {} },
  };
  const sandbox = {
    self,
    caches,
    fetch: async (req: { url: string }) => network(req.url),
    Response,
    URL,
    indexedDB: {},
    decodeURIComponent,
  };
  vm.runInNewContext(SOURCE, sandbox);

  /** Fire one fetch event; returns the response and waits for the cache writes to settle. */
  async function navigate(path: string, mode: 'navigate' | 'cors' = 'navigate') {
    const request = { method: 'GET', url: ORIGIN + path, mode };
    let responded: Promise<Response> | undefined;
    handlers.fetch({ request, respondWith: (p: Promise<Response>) => (responded = p) });
    expect(responded).toBeDefined();
    const res = await responded!;
    await new Promise((r) => setTimeout(r, 0)); // let the un-awaited caches.open().then(put) run
    return res;
  }
  return { navigate, stored, handlers, caches };
}

const page = (headers: Record<string, string> = {}, status = 200) =>
  new Response('<html data-code="SECRET-INVITE"></html>', { status, headers });

describe('service worker: navigation caching', () => {
  it('still caches an ordinary page as the offline shell', async () => {
    const sw = load(() => page({ 'Cache-Control': 'public, max-age=0, must-revalidate' }));
    await sw.navigate('/world');
    expect(sw.stored.has('/')).toBe(true);
  });

  it('caches a page with no Cache-Control header (previous behaviour)', async () => {
    const sw = load(() => page());
    await sw.navigate('/talk');
    expect(sw.stored.has('/')).toBe(true);
  });

  it.each(['/lc/ABC123', '/lc/d/some-desk-token', '/lc', '/live', '/live/desks', '/live?x=1'])(
    'never caches %s, neither under its own URL nor as the offline shell',
    async (path) => {
      const sw = load(() => page());
      const res = await sw.navigate(path);
      expect(res.status).toBe(200); // the page itself is still served
      expect(sw.stored.size).toBe(0);
    },
  );

  it.each(['private, no-store', 'no-store', 'private', 'public, NO-STORE', 'max-age=0, private'])(
    'does not cache a response with Cache-Control: %s',
    async (cc) => {
      const sw = load(() => page({ 'Cache-Control': cc }));
      await sw.navigate('/world');
      expect(sw.stored.size).toBe(0);
    },
  );

  it.each([404, 500, 503])('does not cache a %i response', async (status) => {
    const sw = load(() => page({}, status));
    await sw.navigate('/world');
    expect(sw.stored.size).toBe(0);
  });

  it('does not let a later /lc page overwrite an earlier offline shell', async () => {
    let n = 0;
    const sw = load(() => (n++ === 0 ? new Response('shell') : page()));
    await sw.navigate('/world');
    await sw.navigate('/lc/ABC123');
    expect(await sw.stored.get('/')!.text()).toBe('shell');
  });

  it('degrades to a clean 504 when offline and nothing is cached', async () => {
    const sw = load(() => Promise.reject(new Error('offline')));
    const res = await sw.navigate('/lc/ABC123');
    expect(res.status).toBe(504);
  });
});

describe('service worker: hashed assets', () => {
  it('caches an OK /_astro asset', async () => {
    const sw = load(() => new Response('js', { status: 200 }));
    await sw.navigate('/_astro/app.abc123.js', 'cors');
    expect(sw.stored.has('/_astro/app.abc123.js')).toBe(true);
  });

  it('does not cache a failed /_astro response', async () => {
    const sw = load(() => new Response('nope', { status: 404 }));
    await sw.navigate('/_astro/missing.js', 'cors');
    expect(sw.stored.size).toBe(0);
  });
});

describe('service worker: cache version', () => {
  it('uses a cache name newer than v3, so activation drops caches that may hold an invite page', () => {
    const version = Number(/const CACHE = 'voxtranslate-v(\d+)'/.exec(SOURCE)?.[1]);
    expect(version).toBeGreaterThanOrEqual(4);
  });
});

// VoxTranslate service worker — makes the app installable + offline-capable.
// Conservative caching: network-first for navigations (so the app stays fresh),
// cache-first for immutable hashed assets, and cross-origin requests (the
// Railway API/WebSocket) are left untouched.

const CACHE = 'voxtranslate-v3';
const SHELL = [
  '/',
  '/manifest.webmanifest',
  '/icon.png',
  '/icon-maskable.png',
  // Notification assets: precached so a push rendered on a flaky connection (or
  // with the app closed) still shows our artwork instead of a generic glyph.
  '/icon-192.png',
  '/badge-96.png',
];

// `respondWith` THROWS "Failed to convert value to 'Response'" if its promise
// resolves to `undefined` — which a `fetch().catch(() => caches.match(req))` does
// whenever the network fails AND the cache misses (common on a flaky connection).
// Coerce any cache miss to a real Response so a failed request degrades to a clean
// 504 instead of an uncaught service-worker error.
const OFFLINE = () => new Response('', { status: 504, statusText: 'Offline' });
const orOffline = (res) => res || OFFLINE();

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches
      .open(CACHE)
      .then((c) => c.addAll(SHELL))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener('fetch', (event) => {
  const req = event.request;
  if (req.method !== 'GET') return;
  const url = new URL(req.url);

  // Let cross-origin requests (Railway /rooms, etc.) and WebSockets go straight
  // to the network — never cache backend responses.
  if (url.origin !== self.location.origin) return;

  // Navigations: network-first, fall back to the cached app shell when offline.
  if (req.mode === 'navigate') {
    event.respondWith(
      fetch(req)
        .then((res) => {
          const copy = res.clone();
          caches.open(CACHE).then((c) => c.put('/', copy));
          return res;
        })
        .catch(() => caches.match('/').then(orOffline)),
    );
    return;
  }

  // Content-hashed build assets are immutable — cache-first.
  if (url.pathname.startsWith('/_astro/')) {
    event.respondWith(
      caches.match(req).then(
        (cached) =>
          cached ||
          fetch(req)
            .then((res) => {
              const copy = res.clone();
              caches.open(CACHE).then((c) => c.put(req, copy));
              return res;
            })
            .catch(() => OFFLINE()),
      ),
    );
    return;
  }

  // Vox Voices model bytes: served same-origin from IndexedDB (never the network),
  // so the local TTS engine loads them within `connect-src 'self'` / `worker-src 'self'`
  // and keeps working offline. See the vox helpers below.
  if (url.pathname.startsWith(VOX_PREFIX)) {
    const key = decodeURIComponent(url.pathname.slice(VOX_PREFIX.length));
    event.respondWith(
      voxGetFile(key)
        .then((blob) =>
          blob
            ? new Response(blob, { headers: { 'Cache-Control': 'no-store' } })
            : new Response('', { status: 404, statusText: 'Not Installed' }),
        )
        .catch(() => new Response('', { status: 404, statusText: 'Not Installed' })),
    );
    return;
  }

  // Everything else same-origin: network, fall back to cache.
  event.respondWith(fetch(req).catch(() => caches.match(req).then(orOffline)));
});

// --- Vox Voices model store (IndexedDB, shared with src/scripts/tts/storage.ts) ---
// The app downloads + integrity-verifies voice-pack files and stores them in IndexedDB;
// we serve them here at /vox-models/<packId>/<version>/<path>. Keep this DB/store/key
// contract in sync with storage.ts. We mirror the SAME schema in onupgradeneeded so
// whichever side opens the DB first creates the correct stores (no schema race that
// would leave the DB store-less and break the app).
const VOX_DB = 'vox-voices';
const VOX_DB_VERSION = 1;
const VOX_FILES = 'files';
const VOX_PREFIX = '/vox-models/';

function voxOpenDb() {
  return new Promise((resolve, reject) => {
    const open = indexedDB.open(VOX_DB, VOX_DB_VERSION);
    open.onupgradeneeded = () => {
      const db = open.result;
      if (!db.objectStoreNames.contains('files')) db.createObjectStore('files', { keyPath: 'key' });
      if (!db.objectStoreNames.contains('meta')) db.createObjectStore('meta', { keyPath: 'packId' });
      if (!db.objectStoreNames.contains('bench'))
        db.createObjectStore('bench', { keyPath: 'packId' });
    };
    open.onsuccess = () => resolve(open.result);
    open.onerror = () => reject(open.error);
  });
}

function voxGetFile(key) {
  return voxOpenDb().then(
    (db) =>
      new Promise((resolve) => {
        let store;
        try {
          store = db.transaction(VOX_FILES, 'readonly').objectStore(VOX_FILES);
        } catch {
          return resolve(undefined); // store missing (fresh DB) → treat as a miss
        }
        const r = store.get(key);
        r.onsuccess = () => resolve(r.result ? r.result.blob : undefined);
        r.onerror = () => resolve(undefined);
      }),
  );
}

// --- Web Push (spec: scheduled meetings, Phase 1e) ---
// Payload from the server: { title, body, data:{ join_url, meeting_id, ... } }.
self.addEventListener('push', (event) => {
  let payload = {};
  try {
    payload = event.data ? event.data.json() : {};
  } catch {
    payload = { title: 'VoxTranslate', body: event.data ? event.data.text() : '' };
  }
  const data = payload.data || {};
  event.waitUntil(
    self.registration.showNotification(payload.title || 'VoxTranslate', {
      body: payload.body || '',
      // `icon` is the full-colour image (~64dp): the 512x512 app icon is 238 KB and
      // too heavy to fetch reliably on mobile, so we ship a 192px copy.
      icon: '/icon-192.png',
      // `badge` is NOT drawn as a picture — Android uses its ALPHA channel as a
      // stencil, tints it, and puts it in circular chrome. An opaque image (like
      // icon.png) therefore renders as a solid square. Must stay a transparent,
      // monochrome glyph: see public/badge.svg for the source art.
      badge: '/badge-96.png',
      data,
    }),
  );
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const data = event.notification.data || {};
  const url = data.join_url || '/';
  event.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((clients) => {
      for (const client of clients) {
        if ('focus' in client) {
          client.focus();
          if ('navigate' in client && url.startsWith('/')) client.navigate(url);
          return;
        }
      }
      return self.clients.openWindow(url);
    }),
  );
});

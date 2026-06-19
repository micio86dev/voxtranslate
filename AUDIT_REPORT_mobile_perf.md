# Mobile Performance Audit — 2026-06-19

**Diagnosis only — no source files were modified.**

## How this was measured

- **Tool:** Lighthouse 13.4.0 (CLI), `--preset=perf`, `--only-categories=performance`
- **Target:** local **production build** served via `astro preview` at `http://localhost:4321`
  (clean `astro build`, minified, non-coverage — i.e. byte-for-byte what Vercel ships)
- **Emulation:** mobile form factor, screen emulation on, **CPU 4× slowdown**, **Slow-4G** throttling
  (RTT 150 ms, ~1474 Kbps down) — Lighthouse defaults that mirror PageSpeed Insights mobile.
- Artifacts in repo root: `lighthouse-report.html` (open this), `lighthouse-report.report.json`, `lighthouse-report.report.html`.

> Note on absolute numbers: this run hits the local origin (TTFB ≈ 17 ms, no CDN), so it **isolates app-level
> causes** rather than network/edge. Production (Vercel + Cloudflare) adds brotli + HTTP/2 + caching, which
> lowers transfer sizes somewhat — but every root cause below (the i18n chunk, client-rendered text, the
> favicon) is present identically in prod and is exactly what drags the PSI mobile score down.

## Lighthouse Scores (Mobile)

- **Performance: 71**
- FCP: **1.8 s** (score 0.89)
- LCP: **5.9 s** (score 0.14) ← dominant failure
- TBT: **190 ms** (score 0.91)
- CLS: **0.094** (score 0.91)
- Speed Index: **3.7 s** (score 0.85)
- TTI / Interactive: **5.8 s** (score 0.67)
- Total page weight: **930 KiB**; Server response time: 10 ms (not a factor)

### Why the score is 71 and not higher

LCP carries the most weight and it is failing badly (5.9 s). The LCP **breakdown** is the whole story:

| LCP subpart | Duration |
|---|---|
| Time to first byte | 17 ms |
| Load delay / load time | — (text element, no image) |
| **Element render delay** | **5,882 ms** |

99.7% of LCP is *render delay* — the browser has the HTML in 17 ms but can't paint the largest element until JavaScript runs.

## Opportunities (from Lighthouse)

- **Reduce unused JavaScript** — ~47 KB, all inside the 64 KB entry chunk `index.astro…CBuBXswH.js` (~150 ms).
- **Render-blocking requests** — `index.8lC6OQxv.css` (13 KB), est. savings ~50–152 ms.
- **Reduce unused CSS** — ~12 KB.
- Image delivery, cache, duplicated JS, legacy JS, modern HTTP, font-display, third-parties: **all pass** (score 1) — there is no third-party JS, no web fonts, no legacy polyfills.

## Diagnostics (from Lighthouse)

- **LCP element = `body > main.app > #cookie-banner > span#cookie-text`** — the cookie-banner text.
- That `<span>` ships to the browser **empty**: `<span id="cookie-text" data-i18n="cookieText"></span>`. Its text (and nearly all visible UI text) is injected client-side by `applyI18n()` at runtime.
- **Largest network transfers:** `content.B541p6Vs.js` **577 KB** (gzip; 1,834 KB raw) ▸ `icon.png` **238 KB** ▸ entry JS 64 KB ▸ document 14 KB ▸ CSS 13 KB.
- **No `modulepreload`** for the 577 KB chunk; entry chunk must parse before it's even discovered (serial waterfall).
- **No `preconnect`** to `api.voxtranslate.app`, despite ~5 `/api/*` fetches firing on load.
- 1 layout shift on `div.home-body` (CLS 0.094) — caused by text/chrome appearing after i18n hydration.
- DOM size 741 elements — fine.

## Identified Causes (ordered by estimated impact)

### 1. The full 84-language i18n dictionary is a render-blocking static dependency — **impact: HIGH** (owns ~4 s of LCP)
`src/scripts/i18n.ts` does:
```ts
const localeModules = import.meta.glob('./i18n/*.json', { eager: true, import: 'default' });
```
`eager: true` inlines **all 84 locale JSON files (1.76 MB raw → one 1.83 MB / 577 KB-gzip chunk: `content.B541p6Vs.js`)**. Verified: the string `glossaryImport` (one i18n key) appears **84 times** in that single chunk.
The landing entry script statically imports it: `app.ts → import { loadRemoteI18n } from './content'` and `import … from './i18n'`, and the built entry contains `…}from"./content.B541p6Vs.js"` (a **static** `import … from`, not a lazy `import()`). ES modules don't execute until their entire static import graph is fetched + parsed, so **`app.ts` (and therefore `applyI18n()`) cannot run until all 577 KB is downloaded and parsed** — ~3.1 s of download alone on Slow-4G, plus parse, plus the waterfall (HTML → 64 KB entry → 577 KB chunk). That is the 5,882 ms render delay.
Because every visible string is an empty `data-i18n` placeholder filled by `applyI18n()`, the user sees blank chrome until this resolves. A user only ever needs **one** UI language (~16 KB), not 84.

### 2. `icon.png` (238 KB) is downloaded as the favicon on every load — **impact: MEDIUM**
`dist/index.html` references the same 238 KB PNG four ways: `og:image`, `twitter:image`, **`rel="icon"`**, and **`rel="apple-touch-icon"`**. The `rel="icon"` makes the browser pull the full 238 KB image as the tab favicon during initial load, competing with `content.js` for the throttled mobile pipe. It's the 2nd-heaviest resource and ~26% of total page weight. (og:image is off the critical path; the favicon/apple-touch usage is the waste.)

### 3. No `modulepreload` for the big chunk → serial request waterfall — **impact: LOW-MEDIUM**
`index.html` emits only `<script type="module" src="…entry.js">` with no `modulepreload` hint for `content.B541p6Vs.js`. The browser must download + parse the 64 KB entry before it discovers the 577 KB dependency, adding a full round-trip of latency on top of an already-too-big payload. (Fixing #1 makes this moot; until then a preload hint recovers part of the waterfall.)

### 4. Render-blocking CSS — **impact: LOW**
Single 13 KB stylesheet blocks first paint (~50–152 ms). Of that, ~12 KB is reported unused.

### 5. Unused JavaScript in the entry chunk — **impact: LOW**
~47 KB of the 64 KB entry chunk is unused at load (~150 ms).

### 6. No `preconnect` to the API origin — **impact: LOW (local) / LOW-MEDIUM (prod)**
Five `/api/*` requests fire on load with no `preconnect`/`dns-prefetch` to `api.voxtranslate.app`. Negligible locally (RTT ≈ 0) but in prod each cold connection pays DNS + TCP + TLS round-trips behind Cloudflare.

### 7. Layout shift from late hydration — **impact: LOW** (symptom of #1)
CLS 0.094 on `div.home-body`: text/chrome reflows when i18n populates the DOM. Resolving #1 should also reduce this.

## i18n Bundling

- **Loading type:** **static / eager** — `import.meta.glob('./i18n/*.json', { eager: true })` in `src/scripts/i18n.ts`, statically imported by the landing entry via `app.ts`/`content.ts`.
- **All languages in one chunk:** **YES** — all 84 locale dictionaries are inlined into a single chunk, `content.B541p6Vs.js`.
- **Total translation weight in bundle:** **~1.76 MB raw / 1.83 MB minified / 577 KB gzip** (84 files, `src/scripts/i18n/*.json`). This is ~88% of the JS shipped and the direct cause of the LCP failure.
- (Separately, a 5 KB `/api/content/i18n` fetch supplies remote string overrides at runtime — small, not the issue.)

## Recommended Actions (ordered by impact)

1. **Stop eager-bundling all 84 locales (fixes the LCP failure).** Switch the glob to lazy (`eager: false`) and dynamically `import()` only the detected UI language (+`en` fallback) — roughly 577 KB → ~10–16 KB on first load. Alternatively/additionally, **server-render the visible text into the HTML** (so the page paints real content at FCP instead of empty `data-i18n` placeholders). Either change should move LCP from ~5.9 s toward ~2 s and lift Performance well into the 90s. *(Per project memory: `languages.json` is the shared SSOT; the per-locale UI dictionaries in `src/scripts/i18n/` are the heavy part and can be split independently of it.)*
2. **Replace the 238 KB favicon.** Ship a small dedicated favicon (32×32 PNG/ICO or SVG, ~1–2 KB) for `rel="icon"` and a correctly-sized 180×180 `apple-touch-icon`; keep the large `icon.png` only for `og:image`/`twitter:image` (off critical path).
3. **Add a `modulepreload` hint** for the main app chunk (only relevant while it stays large — #1 supersedes this).
4. **Add `preconnect` to `https://api.voxtranslate.app`** in the document head to cut API round-trips in production.
5. **Trim unused CSS/JS** (~12 KB CSS, ~47 KB JS) — minor, do after the big wins.

## Baseline

No prior Lighthouse report existed in the repo. **Record these as the baseline:** Perf **71**, FCP 1.8 s, LCP 5.9 s, TBT 190 ms, CLS 0.094, SI 3.7 s, TTI 5.8 s (Lighthouse 13.4.0, mobile, Slow-4G, CPU 4×, local production build).

## Post-Fix Results

Implemented as **spec 0104** (branch `perf/0104-mobile-lazy-i18n`). Re-measured with the **same**
setup (Lighthouse 13.4.0, mobile form factor, Slow-4G, CPU 4×, local `astro preview` production
build).

| Metric | Baseline | Post-fix | Δ |
|---|---|---|---|
| **Performance** | **71** | **93** | **+22** |
| FCP | 1.8 s | 2.0 s | +0.2 s |
| **LCP** | **5.9 s** | **2.8 s** | **−3.1 s** |
| TBT | 190 ms | 100 ms | −90 ms |
| **CLS** | **0.094** | **0.005** | **−0.089** |
| Speed Index | 3.7 s | 2.4 s | −1.3 s |
| TTI / Interactive | 5.8 s | 2.8 s | −3.0 s |
| Total page weight | 930 KiB | **383 KiB** | **−547 KiB** |

What changed (in impact order):

1. **Lazy i18n (cause #1).** `i18n.ts` now ships only `en` eagerly (~16 KB fallback, kept
   synchronous so `t()`/`applyI18n()` are untouched) and lazy-loads every other locale on demand
   via `loadLocale()`. The 577 KB-gzip `content.*.js` mega-chunk is **gone** — that chunk is now
   **~15 KB gzip** (app logic + `en`), and the 84 locales are separate per-locale chunks
   (`ar.*.js`, `es.*.js`, …) fetched only when that UI language is selected. First-load JS dropped
   from ~640 KB to **~80 KB gzip**. This collapsed the LCP render delay (5,882 ms → ~0).
2. **Server-rendered English text (cause #1 / #7).** All 254 `data-i18n*` slots in `index.astro`
   are filled at build time from `en.json` via a frontmatter `T()` helper, so the page paints real
   text at FCP (the LCP element is no longer an empty `<span>`). `applyI18n()` still localizes on
   the client for non-English users. This drove **CLS to 0.005** — text no longer reflows in after
   hydration.
3. **Small favicon (cause #2).** `rel="icon"` now points at a dedicated 32×32 **`favicon-32.png`
   (~2.4 KB)** instead of the 238 KB `icon.png` (which stays for `og:image`/`twitter:image`).
   `apple-touch-icon` is a 180×180 PNG (iOS home-screen only, off the critical path).
4. **Resource hints (causes #3, #6).** The env-driven `preconnect` to the API origin was already
   in `Base.astro`; added a `dns-prefetch` fallback. `modulepreload` is moot now that the big
   chunk is gone.

**Result: Performance 93 (target 90+ met), LCP 2.8 s (−3.1 s), CLS 0.005 (−0.089), page weight
−59%.** Production (Vercel + Cloudflare: brotli + HTTP/2 + CDN) should score equal-or-better.

### Remaining (deferred, low priority)

- ~47 KB "unused JavaScript" still sits in the 205 KB-raw entry chunk — it bundles in-call
  modules (WebRTC, chat, Soniox, glossary) that the landing page never exercises (causes #4/#5).
  Code-splitting those behind the call flow would push LCP under 2.5 s and the score toward 95+,
  but it is a larger, higher-risk refactor of the core call path — left as a follow-up.

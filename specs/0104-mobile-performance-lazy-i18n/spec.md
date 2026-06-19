# 0104 — Mobile performance: lazy i18n + server-rendered text (PSI mobile 71 → 90+)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-19 |
| **Shipped** | 2026-06-19 |
| **Version** | 1.1.x |
| **Commits** | `d25ced0` (#289) |
| **Depends on** | [0102](../0102-language-first-ux/spec.md) |

## 1. Context & Problem

A Lighthouse mobile audit (`AUDIT_REPORT_mobile_perf.md`, Slow-4G + CPU 4×, local production
build) scored **Performance 71**, dominated by **LCP 5.9 s** of which **99.7 % was render delay**.
Root cause: `client/src/scripts/i18n.ts` eager-bundled **all 84 UI locales** via
`import.meta.glob('./i18n/*.json', { eager: true })` into a single **577 KB-gzip** chunk
(`content.*.js`) that the landing entry statically imports — so nothing paints until that whole
chunk downloads + parses. A user only ever needs **one** UI language (~16 KB). Two amplifiers:
every visible string shipped as an **empty `data-i18n` placeholder** (the LCP element was a blank
`<span id="cookie-text">`), and the **238 KB `icon.png`** doubled as the `rel="icon"` favicon.

## 2. Goals / Non-Goals

**Goals**
- Mobile Performance **≥ 90** (from 71); LCP toward < 2.5 s; CLS < 0.05.
- First-load JS **< 100 KB gzip** (from ~640 KB).
- No behavioural change: same languages, same detection order, same remote-override path.

**Non-Goals**
- Touching backend, billing, WebRTC, or translation-engine logic.
- Changing `server/src/engine/languages.json` (the shared SSOT) or the `/api/content/i18n`
  endpoint.
- Code-splitting the in-call modules out of the entry chunk (deferred follow-up — see §8).

## 3. Requirements

- **R1 — Lazy locales.** As a visitor, I want only my UI language to load, so the page paints fast.
  - *Given* a fresh load, *when* the entry script runs, *then* only `en` (fallback) + the active
    locale chunk are fetched — never all 84.
- **R2 — Text at first paint.** As a visitor on a slow phone, I want to read real text before JS.
  - *Given* JS is still downloading, *when* the HTML renders, *then* every visible label already
    shows its English default (no empty `data-i18n` spans).
- **R3 — No regression.** *Given* a non-English browser/cookie/selection, *when* the locale chunk
  resolves, *then* `applyI18n()` swaps the UI to that language (incl. RTL `dir`), exactly as before.
- **R4 — Cheap favicon.** *Given* any page load, *then* `rel="icon"` is a ~2 KB asset, not 238 KB.

## 4. Design & Architecture

- **Components / files:**
  - `i18n.ts` — ship `en.json` eagerly (`import enDict`); lazy-glob the rest
    (`import.meta.glob(..., { import: 'default' })`); `SUPPORTED` derived from glob **keys** (no
    loading, so detection stays sync); new `loadLocale(lang)` populates `I18N[lang]`. `t()`,
    `applyI18n()`, `detectLang()`, `getUiLang/setUiLang`, `ENDONYM/FLAG/isRtl` **unchanged**.
  - `app.ts` — import `loadLocale`; add a `withLocale(lang, after)` helper; at the four
    `setUiLang → applyI18n` sites (initial render, `langSel` change, `rebuildLangOptions`,
    `selectLang`) repaint once the locale chunk lands. `boot()`'s post-`loadRemoteI18n` repaint
    is unchanged.
  - `index.astro` — frontmatter `T(k)` helper reads `en.json`; all 254 `data-i18n*` slots filled
    at build time (text content, `placeholder`, `title`+`aria-label`, `aria-label`).
  - `Base.astro` — `rel="icon"` → `/favicon-32.png` (32×32), `apple-touch-icon` → 180×180;
    `icon.png` kept for `og:image`/`twitter:image`. Added `dns-prefetch` fallback to the API origin.
  - `public/favicon-32.png`, `public/apple-touch-icon.png` — committed, generated from `icon.png`.
- **Key decisions:**
  - *Eager-`en` + lazy-rest, keep `t()` synchronous* → rationale: `t()` is called synchronously in
    ~399 sites; a full async refactor (the audit's alternative) is high-risk for the same result.
    English is always present as the fallback, so `t()` never blocks. *Rejected:* top-level-await /
    async `applyI18n()`.
  - *Server-render English, not the detected language* → the static build has no request context;
    English is the universal fallback and matches `<html lang="en">`. Non-English users get a brief
    English paint, then the locale swaps in — strictly better than today's blank-then-text.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Eager `en` + lazy glob + `loadLocale()`; `SUPPORTED` from glob keys | `client/src/scripts/i18n.ts` |
| S1 | `withLocale()` + repaint-on-load at the 4 locale-set sites | `client/src/scripts/app.ts` |
| S2 | `T()` frontmatter helper; fill all 254 `data-i18n*` slots from `en.json` | `client/src/pages/index.astro` |
| S3 | Small favicon / apple-touch-icon + dns-prefetch | `client/src/layouts/Base.astro`, `client/public/*` |
| S4 | Tests: completeness suite reads its own eager glob; `content.test.ts` lazy-loads `it` | `client/src/scripts/{i18n,content}.test.ts` |

## 6. Testing & Verification

- **Unit (`npm run test:unit`):** 542 tests green. The i18n completeness suite (every locale has
  exactly the `en` key set, placeholders preserved, no empty values) now assembles the full
  dictionary set from a **test-only** eager glob, since runtime `I18N` is lazy; `SUPPORTED` is
  asserted to equal those keys. `content.test.ts` lazy-loads `it` before asserting on it.
- **Type check:** `npm run check` (astro check) — 0 errors.
- **Bundle audit:** after `npm run build`, the 577 KB `content.*.js` is gone (now ~15 KB gzip);
  84 per-locale chunks exist; first-load JS ≈ 80 KB gzip; built `index.html` has 0 empty
  `data-i18n` elements and real text in the LCP span.
- **Lighthouse (same setup as baseline):** Performance **93**, LCP **2.8 s**, CLS **0.005**, weight
  **383 KiB** — see `AUDIT_REPORT_mobile_perf.md` → Post-Fix Results.

## 7. Deployment & Operations

- No env vars, migrations, or feature flags. Static client only — Vercel auto-deploys on merge to
  `main`. No server change. The API-origin `preconnect` continues to depend on `PUBLIC_WS_HOST`
  (set in Vercel prod).

## 8. Risks / Open Items

- **Entry chunk still ~65 KB gzip** (205 KB raw) carrying in-call modules (WebRTC, chat, Soniox,
  glossary) the landing page never runs (~47 KB "unused JS"). Code-splitting these behind the call
  flow would push LCP < 2.5 s and the score toward 95+, but is a larger refactor of the core call
  path — **deferred**.
- `content.*.js` (the eager static import) is not `modulepreload`ed, leaving a tiny second-hop
  waterfall; negligible at 15 KB, not worth a hardcoded hash hint.

## 9. References

- Files: `client/src/scripts/i18n.ts`, `client/src/scripts/app.ts`, `client/src/pages/index.astro`,
  `client/src/layouts/Base.astro`, `client/public/favicon-32.png`, `client/public/apple-touch-icon.png`
- Audit: `AUDIT_REPORT_mobile_perf.md`

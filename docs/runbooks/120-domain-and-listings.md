# 120 — Root-domain consolidation, Chrome Web Store, and directory listings

Growth blockers, in the order they must be executed. The order is not cosmetic: two of
these steps invalidate each other if swapped (see §0).

Status: **§1 not started · §2 ready to submit · §3 not started**. Written 2026-08-05.

## Access needed (checked 2026-08-05)

The domain cutover cannot be driven from this workspace. Verified, not assumed:

- The Cloudflare **account** is reachable (R2 lists `vox-web-assets`, `vox-voices`,
  `vox-terraform-state`), but the exposed toolset is R2 / KV / D1 / Hyperdrive / Workers
  only — there is **no DNS, Rulesets/Origin Rules, or Pages custom-domain surface**.
- The `oauth_token` in `~/Library/Preferences/.wrangler/config/default.toml` is rejected by
  the DNS API (`Authentication error`), so the REST route is closed too.

§1 therefore needs a human in the Cloudflare dashboard, or an API token scoped to
`Zone:DNS:Edit` + `Zone:Config:Edit`.

**It should not be done unattended in any case.** §1 is not a config-only change: the call
app and the marketing site are both Astro and both emit `/_astro/*`, so putting them on one
origin collides on asset paths unless the client is rebuilt under a `/app/` base. That is a
code change in `client/` plus a Vercel deploy that has to land in the same window as the
routing change, and the verification list in §1 includes a real Google sign-in and a real
paid call — neither of which can be exercised without a live account.

---

## 0. Why the order matters

The extension declares its host permissions at package time:

```ts
// voxtranslate-chrome-extension/manifest.config.ts
host_permissions: [`${env.apiOrigin}/*`, `${env.appOrigin}/*`],
```

`appOrigin` is `https://voxtranslate.app` today. Changing an extension's host
permissions after publication requires a **new upload and a new store review** — and
until that review clears, published users sit on an origin the app no longer serves.

So: **decide the app origin BEFORE the first store submission.** Publishing first and
migrating after burns a review cycle and breaks live installs.

This is the single reason §1 precedes §2.

---

## 1. Root-domain consolidation

### The problem

All marketing content — 5 locales × (home + blog index + 16 posts + business) ≈ 96 URLs —
is served from `website.voxtranslate.app`. The root `voxtranslate.app` serves the call app
and publishes a 4-URL sitemap (`/`, `/privacy`, `/terms`, `/acceptable-use`).

Search authority accrues to the subdomain. The strongest hostname in the estate carries
almost nothing indexable.

### Two ways to fix it

**Option A — move the app to `app.voxtranslate.app`, marketing takes the root.**

The obvious reading, and the expensive one. Changing the app's origin touches:

| Surface | Breaks how |
|---|---|
| `client/src/pages/w/[code].astro` | Every webinar join link already shared in the wild |
| Google OAuth | Authorised redirect URIs + consent-screen origin |
| Stripe | `success_url` / `cancel_url` / portal return URLs |
| Extension | `appOrigin` host permission + the PKCE handoff at `/extension/connect` |
| Legal docs | `directus/legal/*.{en,it,es,de,fr,pt,ja,zh}.md` hardcode the app origin |
| Server CORS | `ALLOWED_ORIGINS` |
| Chrome Web Store | Privacy-policy URL in the listing |
| Betterstack | `infra/betterstack/monitors.json` |

Every one of those is recoverable, but they must land together, and a missed webinar
link is a customer-visible 404 on a link someone else already emailed.

**Option B — keep the origin, route by path. RECOMMENDED.**

Put Cloudflare in front of the root hostname and split by path:

```
voxtranslate.app/                      → marketing (Cloudflare Pages)
voxtranslate.app/{en,it,es,de,fr}/…    → marketing
voxtranslate.app/blog/…                → marketing
voxtranslate.app/app/…                 → the call app (Vercel)
voxtranslate.app/w/…                   → the call app  (webinar links KEEP working)
voxtranslate.app/extension/…           → the call app  (PKCE handoff unchanged)
voxtranslate.app/{privacy,terms,acceptable-use} → the call app (legal URLs unchanged)
```

Same origin means **zero** breakage across the whole table above: OAuth, Stripe, CORS,
the extension manifest, the legal docs and the monitors all keep working untouched.
The only behavioural change is that `/` stops being the app and becomes the marketing
home — which is precisely the goal.

The app loses `/` as its entry point and gains `/app/`. Old bookmarks to the bare root
land on marketing, which now carries an "Open app" CTA.

### Executing Option B

Root DNS currently points straight at Vercel (`216.198.79.1`), i.e. **not** proxied
through Cloudflare. That has to change first.

1. **Cloudflare** — make `voxtranslate.app` a proxied record (orange cloud) in zone
   `b34db831d71b90f99f3df64d507246d8`.
2. **Route the app paths to Vercel.** Cloudflare Rules → Origin Rules, or a Worker:
   send `/app/*`, `/w/*`, `/extension/*`, `/privacy`, `/terms`, `/acceptable-use`,
   `/api/*` to the Vercel origin; everything else to the Pages project
   `voxtranslate-website`.
3. **Astro config** — `website/astro.config.mjs`: `SITE` → `https://voxtranslate.app`.
   `website/src/lib/site.ts`: `SITE_ORIGIN` likewise, and `APP_URL` → `https://voxtranslate.app/app`.
4. **Client base path** — `client/` must build under `/app/` (`base: '/app'` in its Astro
   config), or the Origin Rule must rewrite the prefix away. Prefer the explicit base:
   a rewrite that strips a prefix breaks absolute asset URLs in subtle ways.
5. **301 the old subdomain.** `website.voxtranslate.app/*` → `voxtranslate.app/*`,
   path-preserving. Keep it up permanently — the blog URLs are the ones with backlinks.
6. **Sitemap + robots** — the root robots must point at the marketing sitemap. The 4-URL
   app sitemap gets folded in or dropped.
7. **Search Console** — add `voxtranslate.app` as a property, submit the new sitemap, and
   use the Change of Address tool from the subdomain property.
8. **Verify** before calling it done:
   - a webinar link `voxtranslate.app/w/<code>` still resolves
   - Google sign-in still completes
   - `voxtranslate.app/privacy` still resolves (Stripe and the store listing point there)
   - `website.voxtranslate.app/en/blog/how-voxtranslate-works/` 301s to the root equivalent

---

## 2. Chrome Web Store submission

The listing assets are already in the repo — this is a submission, not a build:

- `docs/store/screenshots/` — 4 screenshots
- `docs/store/promo/` — marquee 1400×560, small 440×280
- `docs/store/permissions.md` — permission justifications, paste verbatim
- `docs/store/data-disclosure.md` — privacy practices answers
- `docs/store/release-checklist.md` — the gate to work through

Two things to settle before uploading:

- **`appOrigin` must be final** (see §0).
- **The listing copy must not oversell.** `release-checklist.md` forbids quoting a
  latency number that has not been measured on the shipped build, and forbids claiming
  languages a tier does not produce. Standard produces 29, Enhanced 61, Premium 84 —
  the store description must not inherit the homepage's "84 languages" headline.

### The positioning gap this closes

The extension uses `activeTab` + `tabCapture`: it translates **the audio playing in the
current tab**. That covers Google Meet and Zoom on the web, and it does so without
requesting `<all_urls>` — a genuinely strong privacy story.

None of that is on the marketing site today. The site says "browser-native, no
installation required" and never mentions Meet or Zoom, so the first objection every
buyer has — *"but we run our meetings in Meet"* — goes unanswered on every page.

Fix the site copy in the same cycle as the submission. The `/pricing` FAQ added in
`feature/pricing-page` answers it; the homepage and `/business` still do not.

---

## 3. Directory listings

Free, and where B2B buyers actually compare. None exist today.

| Where | Notes |
|---|---|
| **G2** | Category: Language Translation / Video Conferencing. Free vendor profile. Seed reviews from the first design partners. |
| **Capterra** | Same. Capterra and G2 feed most "best X 2026" listicles, which is how every competitor in this category gets discovered. |
| **Product Hunt** | Launch **after** §1 lands — a PH launch drives a traffic spike to whatever the canonical domain is that day, and you want that spike and its backlinks on the root. |
| **AppSumo** | Optional. Cash plus first public reviews, at the cost of a discounted cohort. |

Competitors ranking for "best live translation 2026" today: jotme.io, mirrorcaption.com,
transyncai.com, whisperr.co, palabra.ai. All of them run comparison content. None of that
surface is contested by VoxTranslate yet.

---

## 4. Open questions

- **Currency mismatch.** Consumer billing is USD (`server/src/stripe_handler.rs` →
  `currency: "usd"`), but `/business` quotes €49 / €199. Either the Business Stripe prices
  really are EUR — in which case the site should say which is which — or one of the two is
  wrong. The new `/pricing` page quotes USD throughout, so the two pages now disagree in
  plain sight. Worth confirming before either gets traffic.

- **The WAF blocks CI from the API.** `GET /api/engines` returns **403** to a GitHub Actions
  runner (Cloudflare WAF + Bot Fight Mode, runbook 111) while answering any client from a
  normal network, including plain `curl` and `undici` user agents — so it is the origin ASN
  being filtered, not the agent string.

  This surfaced when the pricing page fetched the catalogue at build time and every
  production locale shipped an "indicative prices" banner. Fixed by committing the catalogue
  (`website/src/data/engines.json` + `scripts/refresh-engines.mjs`) rather than opening the
  WAF: a static marketing build that cannot succeed without the production API is a
  liability, and weakening the origin's protection for it is the wrong trade.

  Worth knowing generally — **any** CI job that expects to reach `api.voxtranslate.app` will
  get a 403 and, unless it checks, will do so silently.
- **`traduzione-simultanea-videochiamate.astro`** exists in `website/src/pages/` but is not
  in the sitemap. Either it is a deliberate orphan landing page or it is an SEO asset that
  nothing links to. Decide which.

- **Pin a single Vite in `website/`.** `node_modules/vite` is 8.1.5 (hoisted, pulled in for
  `@tailwindcss/vite`) while Astro 5 bundles `astro/node_modules/vite` 6.4.3, and
  `astro check` rejects the Tailwind plugin on nominal type identity alone. It is currently
  worked around with an `any` cast at the single call site in `astro.config.mjs`.

  The real fix is a `vite` devDependency (or a `pnpm.overrides` entry) pinned to the version
  Astro uses — `@tailwindcss/vite`'s peer range is `^5.2.0 || ^6 || ^7 || ^8`, so 6.4.3
  satisfies it. It needs a regenerated lockfile, which is why it was not done inline:
  `pnpm-lock.yaml` contains only `vite@6.4.3` today, so package.json and the lockfile agree,
  and adding a dependency without updating the lockfile would break CI's
  `--frozen-lockfile` install. Do it in a commit that runs `pnpm install` for real.

# 114 — Staging environment (Stripe sandbox, isolated DB)

A complete, **non-indexed, password-protected** staging stack that mirrors production
on its own database, so you can exercise Stripe **test-mode** subscriptions (B2C credits
+ B2B org plans) end-to-end without touching production data or real money.

```
            ┌─────────────────────────── STAGING (isolated) ───────────────────────────┐
 Browser ─▶ │  app-staging.voxtranslate.app   (Vercel · client · password + noindex)   │
            │  dash-staging.voxtranslate.app  (Vercel · dashboard · password + noindex) │
            │  www-staging.voxtranslate.app   (Cloudflare · website · Basic Auth+noindex)│
            │            │                                                               │
            │            ▼                                                               │
            │  api-staging.voxtranslate.app   (Railway · server · STRIPE TEST keys)      │
            │            │                                                               │
            │            ▼                                                               │
            │  Supabase STAGING project (separate Postgres + Storage buckets)            │
            └────────────────────────────────────────────────────────────────────────────┘
                         ▲
                         └── Stripe **Test mode** (sandbox) — test products/prices/webhook
```

---

## What's already done in code (automated) ✅

These ship in the repo and activate **only** when the staging env vars are set, so prod
is never affected:

- **noindex on staging:** client (`Base.astro`) and website (`BaseLayout.astro`) force
  `<meta robots="noindex,nofollow">` when `PUBLIC_STAGING=true`; the website Cloudflare
  function also sets `X-Robots-Tag: noindex` on every staging response. The dashboard is
  already `noindex` on every page.
- **Password on the website:** `website/functions/_middleware.ts` enforces HTTP Basic
  Auth whenever `STAGING_BASIC_AUTH` ("user:pass") is set, and skips the canonical-host
  redirect so staging serves under its own domain.
- **Migrations** run automatically on server boot against whatever `DATABASE_URL` points
  at — so the staging DB gets schema-built on first deploy (incl. PostGIS geometry).

## What only you can do (manual — needs your accounts) 🔑

I cannot create third-party accounts/keys or provision paid infra. Do these:

1. **Supabase staging project** (separate DB + Storage) → `DATABASE_URL`, `SUPABASE_URL`,
   `SUPABASE_SERVICE_KEY`, buckets.
2. **Stripe — switch to Test mode**, create test products/prices + a test webhook → test
   keys, price IDs, `*_WEBHOOK_SECRET`.
3. **Set the env vars** on Railway/Vercel/Cloudflare (secrets — I never store these).
4. **Vercel Deployment Protection** (password) on the two staging Vercel projects.
5. **DNS** for the `*-staging` hostnames (Cloudflare) if you want stable URLs.

Everything below is copy-paste ready.

---

## 1. Staging database (Supabase) — MANUAL

1. Supabase → **New project** (e.g. `voxtranslate-staging`), same region as prod.
2. Create two **Storage buckets**: `chat-files`, `recordings` (private).
3. Collect: the connection string (`DATABASE_URL`, the *session/pooler* URI),
   `SUPABASE_URL` (project URL), `SUPABASE_SERVICE_KEY` (service_role key).
4. Nothing to migrate by hand — the server runs all migrations on boot, and
   `location::ensure_geometry` enables PostGIS (Supabase supports it).

> Isolation guarantee: as long as `DATABASE_URL` points at the staging project, no
> staging write can reach production.

## 2. Stripe sandbox (Test mode) — MANUAL

In the Stripe Dashboard, flip the **Test mode** toggle (top-right), then:

1. **B2C credit packages** — create a test **Product + Price** for each pack in
   `CREDIT_PACKAGES` (starter/plus/pro/business). Copy each test `price_…` id.
2. **B2B org plans** — create test Prices for Business & Enterprise, monthly & annual →
   `ORG_PRICE_BUSINESS_MONTHLY/ANNUAL`, `ORG_PRICE_ENTERPRISE_MONTHLY/ANNUAL`.
3. **API keys** (Developers → API keys, Test mode) → `STRIPE_SECRET_KEY` (`sk_test_…`).
4. **Webhooks** (Developers → Webhooks, Test mode) → add an endpoint
   `https://api-staging.voxtranslate.app/api/stripe/webhook` (B2C) and, if separate,
   the org webhook path → copy each signing secret (`whsec_…`) into `STRIPE_WEBHOOK_SECRET`
   / `ORG_STRIPE_WEBHOOK_SECRET`. (Locally you can use `stripe listen --forward-to …`.)
5. Test cards: `4242 4242 4242 4242` (success), `4000 0000 0000 9995` (decline),
   `4000 0027 6000 3184` (3DS). Any future expiry + any CVC.

## 3. Server — Railway staging

Create a **staging environment** (or a second service) on the `voxtranslate-server`
Railway project, then set the env below. Most values are copied from prod; the **bold**
ones differ for staging.

| Var | Staging value |
|---|---|
| **`DATABASE_URL`** | the Supabase **staging** URI (step 1) |
| **`SUPABASE_URL` / `SUPABASE_SERVICE_KEY`** | staging project's |
| **`APP_BASE_URL`** | `https://app-staging.voxtranslate.app` |
| **`DASHBOARD_BASE_URL`** | `https://dash-staging.voxtranslate.app` |
| **`ALLOWED_ORIGINS`** | the three `*-staging` origins (comma-sep) |
| **`STRIPE_SECRET_KEY`** | `sk_test_…` |
| **`STRIPE_WEBHOOK_SECRET`** / **`ORG_STRIPE_WEBHOOK_SECRET`** | `whsec_…` (test) |
| **`CREDIT_PACKAGES`** | same JSON, with the **test** `stripe_price_id`s |
| **`ORG_PRICE_*`** | the **test** price ids (step 2) |
| **`STRIPE_SUCCESS_URL` / `STRIPE_CANCEL_URL`** | `https://app-staging…/?checkout=success|cancel` |
| **`ORG_STRIPE_SUCCESS_URL` / `_CANCEL_URL` / `_PORTAL_RETURN_URL`** | `https://dash-staging…/…` |
| `JWT_SECRET` | a **fresh** secret (don't reuse prod) — `openssl rand -hex 32` |
| `GOOGLE_TOKEN_ENC_KEY` | a **fresh** base64 32-byte key |
| `VAPID_PUBLIC_KEY` / `VAPID_PRIVATE_KEY` / `VAPID_SUBJECT` | a fresh VAPID pair (or reuse) |
| `GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET` | add the `*-staging` origins as Authorized origins/redirects on the OAuth client (or use a separate OAuth client) |
| `DEEPGRAM_API_KEY` / `GROQ_API_KEY` / `RESEND_*` / `SUPABASE_*` / `TURN_*` | copy from prod (or test keys) |

Deploy: `railway up server --path-as-root --service voxtranslate-server --environment staging --detach`

> Reuse the same OAuth client by adding the staging origins, OR create a dedicated
> "VoxTranslate (staging)" OAuth client to keep consent screens separate.

## 4. Frontends — staging deploys

**Client + Dashboard (Vercel):** create a **separate Vercel project** for each (e.g.
`voxtranslate-staging`, `voxtranslate-dashboard-staging`) tracking the same repo, OR a
staging branch. Env per project:

- Client: `PUBLIC_WS_HOST=api-staging.voxtranslate.app`, `PUBLIC_GOOGLE_CLIENT_ID=…`,
  **`PUBLIC_STAGING=true`** (forces noindex).
- Dashboard: `PUBLIC_API_BASE=https://api-staging.voxtranslate.app`,
  `PUBLIC_GOOGLE_CLIENT_ID=…` (dashboard is always noindex).
- **Password:** Vercel → Project → **Settings → Deployment Protection → Standard
  Protection** (password) or **Vercel Authentication**. This is the secure, native way
  for Vercel; do it on **both** staging projects. (CLI can't toggle this — dashboard or
  the Vercel REST API `PATCH /v9/projects/{id}` with `passwordProtection`.)
- Build/deploy: `vercel build --prod && vercel deploy --prebuilt --prod` from the staging
  project (or let Vercel's Git integration deploy the staging branch).

**Website (Cloudflare Pages):** create a staging Pages project (or a preview branch).
Env (Pages → Settings → Environment variables):

- **`STAGING_BASIC_AUTH=stage:<strong-password>`** → enables the Basic Auth gate +
  noindex header (already coded in `functions/_middleware.ts`).
- **`PUBLIC_STAGING=true`** (build-time meta noindex).
- `CANONICAL_HOST=www-staging.voxtranslate.app` (optional).
- `POCKETBASE_URL` → a staging PocketBase or leave unset (bundled fallback posts).

## 5. Verification checklist (do this after it's up)

- [ ] All three staging URLs prompt for a password / sign-in and are **not** in Google
      (`site:app-staging.voxtranslate.app` → nothing; response has `noindex`).
- [ ] Sign in with Google on staging; account lands in the **staging** DB only.
- [ ] **B2C:** buy a credit pack with `4242…` → balance rises; webhook 200; a `usage`/
      `purchase` ledger row exists; decline card `4000…9995` fails cleanly.
- [ ] **B2B:** create an org, subscribe to Business monthly (test card) → plan active;
      open the **billing portal** → upgrade to annual / Enterprise → cancel → status flips;
      every event traces to a `whsec_test` webhook delivery (Stripe → Webhooks → logs).
- [ ] Schedule a meeting → Google Calendar event + invite emails (Resend) work.
- [ ] No staging row appears in the production DB (spot-check a couple of tables).

## Security recap

| Surface | noindex | password |
|---|---|---|
| Client (Vercel) | `PUBLIC_STAGING=true` meta | Vercel Deployment Protection |
| Dashboard (Vercel) | always noindex | Vercel Deployment Protection |
| Website (Cloudflare) | `X-Robots-Tag` + meta | Basic Auth (`STAGING_BASIC_AUTH`) |

Never point a staging webhook or base URL at a production host, and never reuse the prod
`JWT_SECRET`/`GOOGLE_TOKEN_ENC_KEY` — a fresh secret keeps staging sessions/tokens
non-interchangeable with prod.

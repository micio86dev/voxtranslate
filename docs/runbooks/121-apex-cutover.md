# 121 — Apex cutover: marketing to `voxtranslate.app`, app to `app.voxtranslate.app`

Option A from runbook 120, chosen by the owner on 2026-08-05.

The code is written and verified, on held branches. **Nothing is released** — deploying
any single piece before DNS moves points canonicals at the wrong origin. This runbook is
the order that makes the swap safe.

## Branches holding the change

| Repo | Branch | Commit |
|---|---|---|
| parent (client, server, infra) | `feature/root-domain-migration` | `e65d39a0` |
| website | `feature/root-domain-migration` | `6e74ede` |
| chrome-extension | `feature/app-subdomain-origin` | `fcb7788` |

Verified before commit: client 1779 tests + build, server 420 lib tests + clippy,
website `astro check` 0 errors + build 108 pages, extension 185 tests + repackaged.

---

## What breaks if you get the order wrong

The app currently owns the apex. Three things live there that exist outside our control:

1. **`/w/<code>` webinar links** — already emailed, printed as QR codes. Cannot be recalled.
2. **`/?room=<code>` invite links** — shared in chats and calendar invites. A **query on the
   root**, so no static `_redirects` rule can match them. Handled in the Pages middleware.
3. **`/privacy`, `/terms`, `/acceptable-use`** — referenced by Stripe, the Chrome Web Store
   listing, and Google's OAuth consent screen.

All three are covered by 301s in `website/functions/_middleware.ts`, but only once the
marketing site is the thing answering on the apex. Until then they resolve natively.

---

## Cutover order

### 1. Create the app hostname first, while the apex still works

- Vercel: add `app.voxtranslate.app` as a domain on the client project. Keep
  `voxtranslate.app` attached for now — both serve the app during the overlap.
- Cloudflare DNS: `app` → CNAME → the Vercel target, proxied.
- Verify `https://app.voxtranslate.app/` serves the app **before** touching the apex.

This step is reversible and invisible to users.

### 2. Update the third-party allow-lists (before the swap, not after)

These reject unknown origins, so they must accept the new one while the old one still works:

- **Google OAuth** (console): add `https://app.voxtranslate.app` to **Authorised JavaScript
  origins** only. Leave the apex entry in place until step 6.

  **No redirect URI change is needed.** Sign-in uses `google.accounts.oauth2.initCodeClient`
  with `ux_mode: 'popup'` (`client/src/scripts/app.ts`), and the server exchanges the code
  with `redirect_uri: 'postmessage'` (`client/src/scripts/auth.ts`). The popup code flow is
  validated against the JavaScript origin; `postmessage` is not a URI that gets registered.

  The dashboard already runs on `dashboard.voxtranslate.app` with the same client id and is
  unaffected. The extension is unaffected too: it authenticates through
  `chrome.identity.launchWebAuthFlow` against `chrome.identity.getRedirectURL()`
  (`https://<extension-id>.chromiumapp.org/`), which has nothing to do with the app origin —
  what moves is the `/extension/connect` handoff page, already covered by the `appOrigin`
  change.

- **Media servers (Hetzner)** — `infra/media/`. Easy to miss and it breaks webinars outright:
  - `mediamtx.yml` + `replica/mediamtx.yml`: `hlsAllowOrigins` gates HLS playback for webinar
    viewers. Now lists **both** origins so a webinar in progress does not lose playback the
    moment DNS flips.
  - `Caddyfile`: the WHIP `Access-Control-Allow-Origin` for the broadcaster. These are
    credentialed requests, so the header is single-valued and cannot be a wildcard — it moves
    to the app origin and must be deployed **in the cutover window**, not before.
- **Railway** (server service, **production only**). Staging is unaffected: it runs on
  `voxtranslate-staging.vercel.app` and never referenced the apex — verified, not assumed.
  - `ALLOWED_ORIGINS` — add `https://app.voxtranslate.app`, keep the apex for now.
  - `APP_BASE_URL` — was **unset**, so the code default applied. Set explicitly so the
    value that builds `/w/<code>` email links is visible rather than implied.
  - `STRIPE_SUCCESS_URL` / `STRIPE_CANCEL_URL` — these pointed at `voxtranslate.app/?checkout=…`.
    After the swap the apex is the marketing home and the middleware only rescues `?room=`,
    so a returning payer would have landed on marketing. They move to the app host.
  - `ORG_STRIPE_*` all point at `dashboard.voxtranslate.app` — unaffected.
  - Set with **skipDeploys**: the values are all valid the moment `app.` is live, and the
    step-3 release applies them without an extra restart that would drop live calls.

  **DONE 2026-08-05** — all four set on production, verified.

### 2b. Optional — set the origin variables explicitly

Every origin now reads from the environment and **defaults to production**, so the
cutover works with nothing set. Setting them makes the value visible rather than implied,
which is worth doing on the hosts:

| Where | Variables |
|---|---|
| Cloudflare Pages (`voxtranslate-website`) | `PUBLIC_SITE_ORIGIN`, `PUBLIC_APP_URL`, `PUBLIC_DASHBOARD_URL`, `PUBLIC_API_BASE` — plus the Function vars `APP_HOST` and `LEGACY_MARKETING_HOST` if they ever differ from the defaults |
| Vercel (client) | `PUBLIC_SITE_ORIGIN=https://app.voxtranslate.app`, `PUBLIC_DASHBOARD_URL` |
| dashboard | already in the committed `.env.production` (`PUBLIC_APP_URL` added) |

Defaults are production, never localhost: these are static builds on the host, and a
missing variable must not bake a dead URL into the shipped HTML.

### 3. Ship the code

Merge and release, in this order:

1. parent `feature/root-domain-migration` → server + client deploy
2. website `feature/root-domain-migration` → **do not deploy yet** (step 4 does)
3. extension `feature/app-subdomain-origin` → no deploy; it is packaged, not published

After the client deploys, `app.voxtranslate.app` is fully correct and the apex still works.

### 4. Swap the apex

> **DO NOT run `vercel domains rm voxtranslate.app`.** It removes the domain from the
> whole Vercel account, not from one project, and takes **every subdomain registered
> under it** with it. Running it here 404'd `app.voxtranslate.app` AND
> `dashboard.voxtranslate.app` simultaneously — a live outage on both, recovered by
> re-adding each subdomain to its project (`vercel domains add <sub> <project>`).
> The CLI does warn ("This domain's 3 aliases will be removed") — read it.
>
> Detaching the apex from Vercel is **not required**: once DNS points at Pages, Vercel
> simply stops receiving that traffic. Leave it alone.

- Vercel: nothing to remove. (See the warning above.)
- Cloudflare: point `voxtranslate.app` at the Pages project `voxtranslate-website`.
- Cloudflare Pages: add `voxtranslate.app` as a custom domain on that project.
- Keep `website.voxtranslate.app` attached to the same project — the middleware needs it
  to keep answering so it can 301. **Do not delete it.**
- Deploy the website from `main`.

### 5. Verify before declaring done

Every one of these, not a sample:

- [ ] `https://voxtranslate.app/` → language redirect to `/en/` etc. (marketing)
- [ ] `https://website.voxtranslate.app/en/blog/how-voxtranslate-works/` → **301** to the apex
- [ ] `https://voxtranslate.app/w/<a real code>` → **301** to `app.voxtranslate.app/w/<code>`
- [ ] `https://voxtranslate.app/?room=test` → **301** to `app.voxtranslate.app/?room=test`
- [ ] `https://voxtranslate.app/privacy` → **301** to the app
- [ ] Google sign-in completes on `app.voxtranslate.app`
- [ ] A real call connects and translates (WebSocket to `api.voxtranslate.app` passes CORS)
- [ ] A Stripe top-up returns to the right URL
- [ ] **A webinar plays** — start one, join as a viewer from `app.voxtranslate.app/w/<code>`,
      confirm video and translated audio. This exercises the WHIP publish CORS and the HLS
      playback CORS, the two things the media-server config change covers.
- [ ] Betterstack: both monitors green

The OAuth and paid-call checks cannot be done from CI. They need a person with an account.

### 6. Clean up, a week later

- Remove the apex entries from Google OAuth and `ALLOWED_ORIGINS`.
- Google Search Console: add `voxtranslate.app` as a property, submit the sitemap, and run
  **Change of Address** from the `website.voxtranslate.app` property.
- Leave the `website.` 301 up permanently. The blog backlinks are the asset.

---

## Consequences the owner accepted

Option A was chosen over the same-origin path routing (runbook 120 §1, Option B). B would
have kept one origin and broken none of the above. A was chosen anyway; these are the costs,
recorded so they are not rediscovered as surprises:

- Every link in the wild on the apex now takes a 301 hop rather than resolving natively.
- OAuth, Stripe and CORS all need coordinated allow-list edits.
- The extension needed repackaging before it was ever published. It cost nothing this time
  **because it is not yet in the store** — after publication the same change would need a
  full store review while live installs pointed at a dead origin.
- Search Console needs a Change of Address, and rankings typically wobble for a few weeks
  before settling on the stronger domain.

The upside is the reason it was chosen: all content authority consolidates on the apex,
which is the strongest hostname and currently carries almost nothing indexable.

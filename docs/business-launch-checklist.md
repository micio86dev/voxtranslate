# VoxTranslate for Business — launch & verification checklist (spec 0106)

Phase 5 of the Business rollout. This is the runbook to merge, deploy, and verify
the org workspace end-to-end. Work top-to-bottom; nothing here changes the
consumer flow, which must keep working identically throughout.

## 0. Pull requests & merge order

The work shipped as stacked PRs across three repos. Merge in this order (retarget
each PR's base to `main` as its parent merges):

**Backend — `voxtranslate` (stacked):**
1. #295 — schema, RLS, migration tests (migration `016`)
2. #296 — orgs / members / invites / projects API
3. #297 — room binding, cloud recording, diarized transcription, transcript translate/export, call history, org-credit spend
4. #298 — org subscriptions + Billing Portal + one-off top-up + webhook (migration `017`)

**Dashboard — `voxtranslate-dashboard` (new submodule, stacked):**
5. #1 — scaffold + auth + orgs/members/projects
6. #2 — history, call detail, credits, settings

**Wiring & marketing:**
7. `voxtranslate` #299 — call-room hooks, navbar Workspace link, `./dashboard` submodule registration (off `main`)
8. `voxtranslate-website` #1 — `/business` page, homepage section, blog post (off `main`)

After the dashboard PRs merge to its `main`, bump the submodule pointer:
`git submodule update --remote dashboard && git commit -am "chore: bump dashboard submodule"`.
Same for `website` once its PR merges (the existing site-bump flow).

## 1. Database (Railway Postgres)

- Migrations `016_business_workspace.sql` and `017_org_subscriptions.sql` apply
  automatically on server boot (`db::migrate`). Confirm in the deploy logs.
- They are additive/idempotent; existing rows and the consumer flow are unaffected.

## 2. Supabase

- Create a **private** Storage bucket named **`recordings`** (no public access;
  service_role + signed URLs only). Override the name with `SUPABASE_RECORDINGS_BUCKET`
  if different.
- The same `SUPABASE_URL` + `SUPABASE_SERVICE_KEY` already used for chat files are reused.

## 3. Stripe (org billing)

- Create one **Product** per plan with **recurring Prices**:
  - Business — monthly + annual (annual discounted)
  - Enterprise — monthly + annual
- Enable the **Customer Billing Portal** (cancel at period end, switch plan, update card).
- Add a **webhook endpoint** → `https://<api-host>/api/business/stripe/webhook`,
  subscribed to: `checkout.session.completed`, `invoice.payment_succeeded`,
  `invoice.payment_failed`, `customer.subscription.updated`, `customer.subscription.deleted`.
  Use a **separate signing secret** from the consumer webhook.

## 4. Environment variables

**Server (Railway):**
| Var | Purpose |
|---|---|
| `ALLOWED_ORIGINS` | **Add the dashboard origin** (e.g. `https://dashboard.voxtranslate.app`) so the browser can call `/api/business/...`. |
| `ORG_STRIPE_WEBHOOK_SECRET` | Activates org billing; signing secret for the org webhook. |
| `ORG_STRIPE_SUCCESS_URL` / `ORG_STRIPE_CANCEL_URL` / `ORG_STRIPE_PORTAL_RETURN_URL` | Checkout/portal return URLs (point at the dashboard). |
| `ORG_PRICE_BUSINESS_MONTHLY` / `_ANNUAL`, `ORG_PRICE_ENTERPRISE_MONTHLY` / `_ANNUAL` | Stripe price ids. |
| `ORG_CREDITS_BUSINESS_MONTHLY` / `ORG_CREDITS_ENTERPRISE_MONTHLY` | Monthly credit allotment (annual grants 12×). |
| `ORG_CREDIT_UNIT_CENTS` | One-off top-up price per credit (default 100). |
| `SUPABASE_RECORDINGS_BUCKET` / `SUPABASE_RECORDINGS_TTL_SECS` | Optional; default `recordings` / `3600`. |

**Dashboard (Cloudflare Pages):** `PUBLIC_API_BASE` (the API origin, no trailing slash),
`PUBLIC_GOOGLE_CLIENT_ID` (same Google project as the consumer app).

## 5. Deploy

- **Dashboard:** create a Cloudflare Pages project `voxtranslate-dashboard` + DNS
  (`dashboard.voxtranslate.app`); build `npm run build`, output `dist/`. Add the env vars above.
- **Server:** `railway up` (does not auto-deploy) once the backend PRs are merged.
- **Website:** Cloudflare Pages auto-deploys on submodule bump (existing flow).
- After deploy, update the marketing CTAs' `DASHBOARD_URL` / `CONTACT_EMAIL` in
  `website/src/lib/site.ts` if the final domains differ.

## 6. Verification

**Automated (already green in CI/local):**
- [ ] `cd server && cargo test --locked` with `DATABASE_URL` set → all business tests pass
      (`business_schema` 6, `business_orgs` 6, `business_calls` 5, `business_billing` 3),
      plus zero **new** failures vs the pre-existing local WS-join tests.
- [ ] `cd client && npm run check && npm run test:unit` → 546/546 (incl. the 84-locale
      i18n completeness test).
- [ ] Dashboard + website `astro check` + build clean.

**Manual smoke (against deployed services):**
- [ ] **Consumer regression:** a normal call with no org works exactly as before
      (no Workspace link, no project selector, local recording download).
- [ ] **Tenant isolation:** two users in different orgs cannot see each other's
      orgs/members/projects/history (API returns 404/403).
- [ ] **Onboarding:** sign in to the dashboard → create org → invite a member by email.
- [ ] **Invite:** open the `/join?token=…` link as the invitee → accept → membership appears.
- [ ] **Projects:** create a project; bind a call to it from the call app's pre-join.
- [ ] **Recording:** enable cloud recording in pre-join → run a short call → on end, the
      recording uploads (badge + GDPR notice shown), `transcript_status` → `processing`.
- [ ] **Transcription:** the call appears in History; status reaches `ready`; the diarized
      transcript renders with speaker labels.
- [ ] **Translate:** translate the transcript to IT, then DE; the **2nd request to the same
      language is cached** (0 credits). Export **TXT** and **PDF**.
- [ ] **Recording playback:** the call detail page plays the recording via a 1h signed URL.
- [ ] **Audit:** with `compliance_mode` on, `audit_logs` gains `transcript.view` /
      `transcript.export` / `recording.play` / `member.invite` rows.
- [ ] **Subscription:** start a Business subscription (Checkout) → webhook grants credits,
      `subscription_status='active'`; open the Billing Portal → toggle cancel-at-period-end →
      webhook syncs `cancel_at_period_end`. Verify a replayed invoice grants **once**.
- [ ] **Credits separation:** org spend never touches consumer balances and vice versa.

## 7. Rollback

- The dashboard + marketing are separate deploys — roll back independently with no
  impact on the call app.
- The backend is additive: org billing stays dormant unless `ORG_STRIPE_WEBHOOK_SECRET`
  is set; recording storage stays dormant unless the bucket/config exist. Unsetting them
  disables the Business surface while the consumer flow continues untouched.

## 8. Known follow-ups

- Org invite emails currently link to `app_base_url/join`; repoint to the dashboard origin.
- Backfill the remaining 79 locales for the dashboard (English fallback is safe today).
- Pre-existing formatter drift in both repos (rustfmt / prettier) is unrelated to this work.

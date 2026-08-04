# VoxTranslate — Legal / Compliance / IP TODO

> Action checklist before going commercial. Built from a full audit of the repo
> (licenses, fonts, AI assets) plus research on each AI provider's Terms of Service
> and EU/Spain trademark rules.
>
> ⚠️ **This is general information, not legal advice.** For the trademark filing and
> the final wording of Terms/Privacy, a one-off review by a Spanish *agente de la
> propiedad industrial* / EU trademark or tech lawyer is recommended.
>
> Priorities: **P0** = do before charging real users · **P1** = before enabling the
> Enhanced/Pro/Premium tiers in production · **P2** = before public launch / marketing.

---

## ✅ Already done / verified (no action needed)

- [x] Project licensed **PolyForm Shield 1.0.0** (`LICENSE`, © 2026 Alessandro Micelli) — source-available, anti-competition; `license` field updated in `server/Cargo.toml` (`LicenseRef-PolyForm-Shield-1.0.0`) and `client/package.json` (`SEE LICENSE IN LICENSE`), README updated. Note: not an OSI "open source" license (by design — true OSS can't forbid competition).
- [x] **Dependency licenses clean** — 0 GPL/AGPL/SSPL in 572 Rust crates and ~350 npm packages. Only permissive (MIT/Apache/BSD/ISC) + harmless LGPL native image lib (`sharp`/libvips, build-time).
- [x] **Fonts** — Noto Sans (+JP/SC) under SIL Open Font License, and `OFL.txt` is bundled alongside them (`server/assets/fonts/`). Correct.
- [x] **Cartesia access tokens hardened** — short-lived (`expires_in: 3600`, ≤1 h), scoped to STT+TTS grants, minted server-side; the raw `CARTESIA_API_KEY` never reaches the browser (`server/src/api.rs` `mint_cartesia_token`).
- [x] **AI icons** (ChatGPT) — usable commercially (OpenAI assigns output rights). Protect via trademark (logo), not copyright — see P2.
- [x] **Terms/Privacy pages exist**, GDPR-aligned, 18+, with a sub-processor table.
- [x] **EU AI Act Art. 50 transparency implemented** (2026-08-04, released as v1.36.3) — AI disclosure on the dashboard assistants, the webinar viewer and the Chrome extension; machine-readable marking on transcript PDFs (metadata keywords) and WebVTT exports. Role, classification and the exemptions relied on are recorded in `docs/eu-ai-act-compliance.md`. Remaining work in the AI Act section below.
- [x] **Pro tier removed from all public copy** (site + blog, 5 languages, CMS migrated in production).

---

## 🔴 P0 — Provider configuration (the real risk; do first)

### 1. Gemini — enable Cloud Billing (MOST IMPORTANT) ⚠️
The Gemini **free / AI-Studio-unpaid tier trains on your users' audio and humans may review it.** The paid tier does NOT. The switch is just enabling billing — and you can still use Google Cloud's free credits, so it costs nothing now.

- [x] Go to **https://aistudio.google.com** → identify the Google Cloud **project** behind your `GOOGLE_AI_API_KEY`.
- [x] Open **Google Cloud Console → Billing** (https://console.cloud.google.com/billing) and **link a billing account** to that project (new accounts get ~$300 free credit).
- [x] Confirm you're on the **paid tier**: https://ai.google.dev/gemini-api/docs/pricing (paid tier = no training on your data, abuse logs ≤55 days).
- [x] **Never enable the Premium tier in production while the key's project is on the free tier.**

### 2. Zero Data Retention on the other providers (free settings)
- [x] **OpenAI (Pro tier):** request/enable **ZDR** for your org — https://platform.openai.com (Settings → Data controls, or via sales). Note: without ZDR, API logs are being preserved under the NYT court order.
- [x] **Groq:** console.groq.com → **Settings → Data Controls** → enable Zero Data Retention. Ref: https://console.groq.com/docs/your-data
- [x] **Cartesia:** zero-retention is the **default** (no action) — but **request a DPA** (see #4).

### 3. Deepgram — keep the discount now, tighten later ✅ (decided)
**Decision:** keep the **Model Improvement Program ON** (do NOT add `mip_opt_out`) for now, to preserve the self-serve discount and stretch your **$200 bonus** while testing with first users.

- [ ] Confirm your account is on the **2024 commercial MSA**, not the 2017 "personal, non-commercial" website terms (the 2017 terms grant you NO commercial license / NO output ownership). Check in the Deepgram console / your signed Order Form. MSA: https://static.deepgram.com/business/MSA_20240315.pdf
- [x] Be transparent with first users that the STT provider may process audio to improve its service (see P1 privacy note).
- [ ] **Reminder for when you scale past the bonus/testing phase:** switch to no-training by appending `&mip_opt_out=true` to the Deepgram URLs in `server/src/deepgram.rs` (the `wss://…/v1/listen` at line ~99 and the REST `…/v1/listen` at lines ~136, ~140, ~185), and/or negotiate a Zero-Data-Retention + DPA with your Deepgram AE. Docs: https://developers.deepgram.com/docs/the-deepgram-model-improvement-partnership-program

### 4. Request DPAs / data agreements
- [ ] **Deepgram** — request DPA/ZDR *(trigger: first B2B client or end of $200 bonus)*
- [ ] **Cartesia** — request DPA/BAA *(trigger: first B2B client)*
- [x] OpenAI / Groq / Google publish standard DPAs — accept/keep on file. (standard DPA, auto-accepted)

### 5. Don't train on provider output
- [x] Do **not** use Deepgram / OpenAI / Gemini output to train or fine-tune your own STT/translation model — all three have "no competing model" clauses. (Groq's is narrower; review Cartesia's terms with the DPA. acknowledged — no fine-tuning pipeline planned)

---

## 🟡 P1 — Legal / policy copy (before enabling Enhanced / Pro / Premium)

### 6. Add the new providers to the privacy sub-processor table
`client/src/pages/privacy.astro` currently lists Deepgram + Groq but not the providers behind the new tiers. **When you enable a tier, add its row.** Ready-to-paste (matches the existing table format, section 4):

```html
<tr><td>Cartesia</td><td>Speech-to-text &amp; translation (Enhanced tier; the browser connects directly)</td><td>Streamed audio, transcripts (transient)</td></tr>
<tr><td>OpenAI</td><td>Real-time speech translation &amp; synthesized voice (Pro tier)</td><td>Streamed audio, transcripts (transient)</td></tr>
<tr><td>Google (Gemini API)</td><td>Real-time speech translation &amp; synthesized voice (Premium tier)</td><td>Streamed audio, transcripts (transient)</td></tr>
```

- [x] **DONE** — Cartesia, OpenAI, Google (Gemini) rows added to `privacy.astro` (all tiers confirmed live).

### 7. Add an "AI-generated content" disclosure (required by OpenAI; EU AI Act Art. 50)
- [x] **DONE** — "AI-generated content" section added to `client/src/pages/terms.astro` (now §12; Contact → §13):

```html
<h2>X. AI-generated content</h2>
<p>
  The Service uses third-party artificial-intelligence systems to transcribe,
  translate and — on certain tiers — synthesize a spoken translation of what
  participants say. Transcripts, translations and any synthesized voice are
  generated automatically by AI, may contain errors, and should not be relied upon
  for critical, legal, medical or safety-related decisions. The translated voice you
  hear on AI-voice tiers is computer-generated and is not a recording of the speaker.
</p>
```

- [x] **DONE** — in-call AI notice added under the transcript label (`index.astro` + `.ai-notice` style; i18n key `aiGeneratedNotice`). **Translated into all 84 locales** (`i18n/*.json`).

### 8. Privacy honesty note for the Deepgram-MIP decision
- [x] **DONE** — transparency note added to `privacy.astro` section 4 (covers keeping the Deepgram discount + the Enhanced client-direct path):

```html
<p>Some speech-to-text providers may process streamed audio under their own terms, including to maintain and improve their services. We select providers that offer data-processing controls and will tighten these settings as the Service matures.</p>
```

### 9. Acceptable Use Policy
- [x] **DONE** — `/acceptable-use` exists; added section "6. AI features and third-party AI policies" mirroring the providers' prohibited uses (no impersonation/deepfakes, no illegal/harmful generation, no AI-as-professional-advice / high-stakes automated decisions, no model extraction or competing-model training, don't hide AI-generated notices). Contact → §7.

---

## 🟡 P1 — Product

- [x] **18+ enforcement** — verified + hardened. The client already shows a **blocking, non-dismissible** consent modal at login (`app.ts` `ensureConsent`, Escape disabled for it). The gap was that the WS join gate (`authorize` in `server/src/lib.rs`) checked token + ban + balance but **not consent**, so 18+ was client-only / bypassable. **Fixed:** added `SafetyService::has_consented()` and a server-side check in `authorize` that rejects join with code `consent_required` (client re-opens the modal) — fails closed on DB error like `can_join`. Server compiles, client type-checks. *(Guests: now gated client-side too — a blocking 18+/ToS modal on entry **and** a `startCall` backstop, persisted via `auth.guestConsentGiven()` in localStorage. Accounts remain gated server-side.)*
- [ ] **Consent scope** — confirm the consent modal copy explicitly covers *recording / AI-processing of audio*, and that every call participant is a consenting account holder (you carry the indemnity for input-audio rights with Deepgram/OpenAI/Groq).

---

## 🔴 P0 — EU AI Act (Art. 50 applicable since 2 August 2026)

> Implementation is done and live; what is left is judgement, paperwork and the things
> that must be re-checked when the product changes. Full reasoning:
> `docs/eu-ai-act-compliance.md`. Legal basis: Regulation (EU) 2024/1689 as amended by
> Regulation (EU) 2026/1744 (Digital Omnibus, in force 27 July 2026).

### 14. Lawyer review of the compliance record ⚠️
The two load-bearing judgement calls were made by an engineer, not a lawyer, and both
carry the Art. 99(3) penalty band if wrong.
- [ ] **§5 — reliance on the real-time ephemeral exemption** for unmarked live synthetic audio. This is the one that saves the watermarking work; if it does not hold, marking was due by 2 December 2026.
- [ ] **§4 — why text-only sentiment analysis sits outside the Art. 5(1)(f) prohibition.** Sound per the Commission guidelines, but it is the difference between a normal feature and a prohibited practice in a workplace context.
- [ ] Fold this into the same engagement as the Terms/Privacy review already noted at the top of this file — one lawyer, one sitting. *(trigger: same as the existing legal review budget)*

### 15. Tell Business customers the deepfake disclosure duty is theirs
Under Art. 50(4) the duty sits with the **deployer** — the organisation running the
meeting — not with us. We provide the means; they owe the disclosure.
- [ ] Add a line to Business onboarding / the org admin docs, specifically for orgs that enable Enhanced voice cloning. *(trigger: next Business customer)*

### 16. Re-check the exemption before shipping anything that STORES audio ⚠️
The unmarked live audio depends on three conditions (compliance record §5). Two of them
break the moment recording ships:
- [ ] Call recording of translated audio → the recording is a stored artifact and **must** be marked.
- [ ] Stored voice messages / stored cloned voice → a stored cloned voice is a deepfake under Art. 3(60), with a marking duty on us.
- [ ] Any new surface that plays synthesized audio must carry a session-level notice, or it removes the exemption's basis for itself. *(trigger: before merging either feature)*

### 17. Code of Practice on Transparency of AI-Generated Content
- [ ] Watch for the final text and decide whether to sign. Voluntary, but it is the cheapest available evidence of good faith if a regulator ever asks.

### 18. GDPR — DPIA for per-speaker sentiment analysis
Independent of the AI Act: scoring named participants' sentiment is profiling.
- [ ] Required if it is ever sold as an employee-facing feature to employers. *(trigger: before marketing sentiment analysis to Business)*

### 19. AI literacy record (Art. 4, in force since Feb 2025)
- [ ] Record the date each person working on the translation / assistant / `ai/` code paths read `docs/eu-ai-act-compliance.md`. The record IS the evidence; for a team this size that is the whole obligation.

---

## 🟡 P1 — Infrastructure & DX (found during the 2026-08-04 release)

### 20. The Railway CLI makes CI lie about deploys ⚠️
`deploy-staging` and `deploy-server` both reported **failure** while the deploys actually
succeeded. `railway up --ci` dies with `Failed to stream build logs` after ~70s and exits
1; the build carries on server-side and reaches SUCCESS.
- [x] **DONE** — both jobs now call `.github/scripts/railway-deploy.sh`, which starts the deploy with `--detach` and polls `railway deployment list --json` until the deployment reaches a terminal state. Fail-closed: no new deployment, an unreadable status, a terminal failure or the 25-minute timeout all exit non-zero.
- [ ] **Verify on the next real deploy.** The read-only queries were validated against the live API with a user session, but CI uses a *project* token and `railway deployment list` was never exercised with one. If it is not permitted there the job goes red — the same symptom as before, no worse — and the fix is to read the status via `railway api` instead.
> A CI that is red when everything works is a CI you stop reading. The one thing this
> script must never do is go green when it lost track of the deploy.

### 21. Dead `RAILWAY_API_TOKEN` in the shell environment
It is set, Railway rejects it, and it **shadows** the working browser session — so every
`railway` command fails with `Unauthorized` until you strip it.
- [x] **Already fixed on disk** — `~/.zshrc:134` has the export commented out, and the value still in the environment is byte-identical to it: it is inherited from a shell started before that edit. A fresh shell clears it; nothing to change.
- [ ] If it ever comes back, the workaround is `env -u RAILWAY_API_TOKEN railway …`.
> Same shape as Homebrew's rustc shadowing rustup and breaking `cargo` — see the
> toolchain note. Two instances of the same trap is a pattern worth watching for.

### 22. PocketBase deploys are directory uploads, not git
The `voxtranslate-pocketbase` service has **no source configured**, so a UI "Redeploy"
republishes the old snapshot and silently ignores new migrations.
- [x] **DONE** — documented in `website/CLAUDE.md` under "PocketBase (Railway)": the `railway link` → `railway up --detach` → `gh workflow run deploy.yml` sequence, plus the `RAILWAY_API_TOKEN` shadowing, the Cloudflare stale-page trap and the backup command.
- [ ] Consider attaching a GitHub source so this stops being tribal knowledge.

### 23. Dashboard CI never runs on `develop`
Its `ci.yml` triggers on push to `main` and on pull_request only, so merging to `develop`
gets no gate at all before the staging deploy.
- [x] **DONE** — `develop` added to the push triggers.

> **Do NOT add the `server_changed` gate to `deploy-staging`.** Its absence is deliberate
> and the reasoning is in the comment above that job: staging once sat on a v1.27 build
> while develop was at v1.32 and nobody noticed. On staging, silent drift is worse than
> the wasted rebuild. (Noted here because it looks like an obvious optimisation, and it
> was proposed and withdrawn on 2026-08-04.)

### 24. Retire the pre-migration CMS backup
`/Volumes/Scheda SSD/VoxTranslate/backups/pocketbase-posts-pre-migration-006-2026-08-04.json`
— 17 records with i18n, taken before migration 006, whose down-migration is a no-op by
design.
- [ ] Delete once the migrated blog has been live for a while without complaints.

### 25. Housekeeping
- [ ] `website/.github/workflows/deploy.yml` — actions still target Node 20 (forced onto 24 with a deprecation warning): bump `actions/checkout`, `actions/setup-node`, `pnpm/action-setup`, `cloudflare/wrangler-action`.
- [ ] `pnpm install` failed to fetch one tarball from the npm registry (`undici-types`, repeated ENOENT) during the release; builds were verified with the local `astro` binary instead. Confirm it is not systemic.

---

## 🟢 P2 — Trademark "VoxTranslate" (before public launch — EU is **first-to-file**)

> The EU/Spain protect whoever **files first**, not who used it first. Register before you publicise the name, or someone can file ahead of you and block you.

### 10. Free prior search (done)
- [x] **TMview** (global, ~75 offices): https://www.tmdn.org/tmview/ — "VoxTranslate" = 0 results; "Vox Translate" = 1 result (Mexico/IMPI, class 42, filed 2025-11-20 by Enrique Sanchez Iniestra — different jurisdiction, no EU conflict).
- [x] **EUIPO eSearch plus**: https://www.euipo.europa.eu/en/search-ip — 0 results.
- [x] **OEPM Localizador** (Spain): https://consultas2.oepm.es/LocalizadorWeb/ — 0 results.
- [x] Check `.com/.eu/.app` domains + social handles for conflicts.
  - voxtranslate.com → Italian human translation agency (different segment, no EU trademark registered, low conflict risk)
  - voxtranslate.eu → redirects to voxtranslate.com (same entity)
  - voxtranslate.app → ours ✅
  - App Store: "Vox Translate" iOS app (real-time voice, similar category — monitor when launching mobile)

### 11. Decide mark type — mitigate descriptiveness ⚠️
- [ ] **File a figurative / logo mark** (AI icon + styled wordmark, not bare word) — lowers refusal risk + protects the icon. *(trigger: EUTM filing)*
- [ ] Optional: short **attorney distinctiveness opinion** (~few hundred €) before paying EUIPO fees. *(trigger: EUTM filing)*
- [x] Rebranding considered and excluded — name change cost too high at this stage (84 locales, domain, UI).

### 12. File the EUTM *(trigger: first €500 revenue)*
- [ ] Create EUIPO account and file via Fast Track, figurative mark, classes 9+42+38 (~€1,050)
- [ ] **SME Fund Voucher 2** (75% back, max €700) — Voucher 2026 EXHAUSTED.
      Apply on **2 February 2027** first thing — funds go fast.
- [ ] Timeline: ~3 weeks publication + 3-month opposition → registered ≈ 4-4.5 months if unopposed.

### 13. Later — international expansion
- [ ] Extend via **WIPO Madrid** using EUTM as base mark when entering US/UK market. *(trigger: first paying US/UK customers)*

---

## 🟢 P2 — Icon
- [x] Keep ChatGPT-generated icon for commercial use. Copyright protection limited (pure AI image) — protected via figurative trademark filing (see step 11/12).

---

### Quick "this week" shortlist — updated 2026-08-04
1. [x] **Railway CLI flake fixed in CI** (step 20) — needs one real deploy to confirm the project token can read deployment status.
2. [x] **Dead `RAILWAY_API_TOKEN`** (step 21) — already commented out in `~/.zshrc`; only a stale inherited shell still carries it.
3. [x] **PocketBase deploy path documented** (step 22).
4. [x] **`develop` added to the dashboard CI triggers** (step 23).
5. [ ] Add the AI Act items (14-19) to whatever the lawyer engagement ends up covering.
6. [ ] Bump the deprecated Node 20 actions in the website deploy workflow (step 25) — left alone deliberately: `wrangler-action` and `pnpm/action-setup` majors change inputs, and that is not a change to make without watching a deploy.

### Earlier shortlist (done)
1. [x] Gemini Cloud Billing enabled — new GCP project, paid tier, €10 prepaid, €25/month cap, new API key deployed to Railway.
2. [x] ZDR enabled on Groq (Global ZDR toggle); OpenAI API call logging set to Disabled; Gemini paid tier (no training by default). Deepgram MSA version — email sent to support, awaiting reply.
3. [x] Trademark searches done — TMview, EUIPO eSearch, OEPM all clear. Logo mark strategy confirmed (step 11).

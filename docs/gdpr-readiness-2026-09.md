# GDPR readiness — gap analysis for the GSOC enterprise call

**Date:** 2026-09-04 · **Branch:** `develop` @ `28e49a67` · **Scope:** `server/`, `client/`,
`dashboard/`, `directus/legal/`, plus live Railway + Supabase configuration read through the
management APIs.

**This is an engineering audit, not legal advice.** Everything below is what the code and the
deployed configuration actually do. Where a fact is a business fact I cannot see from the
repository (signed contracts, provider commercial terms), it is marked **UNVERIFIED** and must
not be asserted on the call.

---

## 0. Three premises in the brief are wrong — correct them before the call

| Briefed | Actual |
|---|---|
| "Standard: Deepgram + Groq" | **Standard is Qwen realtime on Alibaba Cloud Model Studio** (`server/src/engine/standard.rs:1`). Deepgram is **batch only** — uploads, cloud recordings, voice messages. No live tier uses Deepgram. |
| "Premium: Gemini Live, Pro: OpenAI Realtime" | Correct as tiers, but the persisted engine ids do **not** match the labels: `OPENAI_ID == "premium"` is the **Pro** tier (`server/src/engine/mod.rs:43-53`). Do not quote engine ids from the database as tier names. |
| "ImageKit" | **ImageKit is not in the codebase at all.** No references anywhere. File storage is Supabase Storage only. Do not put ImageKit on a sub-processor list. |

Getting a sub-processor list wrong in a GSOC procurement review is worse than admitting a gap.

---

## 1. Infrastructure data residency

### Railway backend — **EU, verified**

- Production service `voxtranslate-server` deploys to `europe-west4-drams3a` — GCP
  **europe-west4, Netherlands**, single replica (`multiRegionConfig`, read live from the Railway
  API on 2026-09-04).
- Custom domain `api.voxtranslate.app`, Cloudflare in front (`CF_ORIGIN_SECRET` set).
- A `directus` CMS service sits in the same project and therefore the same region.

⚠️ **Stale doc:** `docs/runbooks/113-railway-region.md` still tells you to pin **`us-east`**, on
the reasoning that Deepgram and Groq are US-based. That reasoning is obsolete (Deepgram left the
live path) and the deployment already contradicts it. Fix the runbook before anyone follows it.

⚠️ **Single region, no failover.** Rooms are in-memory per instance; multi-region needs a shared
room registry that does not exist. A 24/7/365 GSOC will ask about availability, not just
residency. There is no EU-region redundancy today.

### Supabase — **EU, verified**

| Project | Region |
|---|---|
| `VoxTranslate` (production) | **eu-west-1** — Ireland |
| `voxtranslate-staging` | **eu-central-1** — Frankfurt |

Postgres 17.6, status healthy. This one project holds the database **and** the Storage buckets, so
stored transcripts, chat attachments and cloud recordings are all in Ireland.

**UNVERIFIED:** whether the Supabase plan carries a contractual EU-only residency commitment as
opposed to a region choice. Region selection is not the same thing as a residency guarantee, and a
GSOC lawyer knows that. Check the plan before promising it.

### DragonflyDB translation cache — **not deployed**

This is the cleanest answer in the whole audit: **it does not exist in production.**

- No Dragonfly service in the Railway project (only `directus` and `voxtranslate-server`).
- Neither `DRAGONFLY_PRIVATE_URL` nor `TRANSLATION_CACHE_ENABLED` is set in production.
- The code (`server/src/cache/mod.rs`, spec 0107) is complete but dormant and fail-open.

If it is ever switched on, know what it would hold:

- **Key** = `MD5(normalize(text) + "|" + src + "|" + tgt + "|" + glossary_fingerprint)`
- **Value** = **the plaintext translated text**
- **TTL** = `604800` seconds — **7 days** (`TRANSLATION_CACHE_TTL_SECONDS` default)
- Only phrases up to `TRANSLATION_CACHE_MAX_WORDS` (default 8) are cached.

So: no raw audio, but not "just cache keys" either — it caches translated utterance text for a
week. And MD5 over a short normalized phrase is **not** a defensible pseudonymisation: the
keyspace of 8-word phrases is trivially enumerable. If a customer asks whether the cache is
anonymised, the honest answer is no. Leave it off for this customer, and say so as a control.

---

## 2. Third-party AI engine data flows

Every endpoint below is read from the code; none is guessed.

| Tier | Provider | Endpoint | What leaves | Path |
|---|---|---|---|---|
| **Standard** (default + capacity fallback) | Alibaba Cloud Model Studio — Qwen realtime | `wss://dashscope-intl.aliyuncs.com/api-ws/v1/realtime` (`qwen.rs:525`) | Streamed audio **and** transcript text | Browser → our EU server → **Alibaba, Singapore** |
| **Enhanced** | Cartesia (Ink-2 STT, Sonic-3.5 TTS) | `wss://api.cartesia.ai/stt/websocket`, `/tts/websocket` (`cartesia.rs:101-103`) | Streamed audio; voice clip + synthetic voice if cloning is used | **Browser → Cartesia, US directly.** Our server never sees the audio; it only mints a short-lived access token (`api.rs:107`) |
| **Pro** | OpenAI Realtime | `wss://api.openai.com/v1/realtime/translations` (`openai.rs:139`) | Streamed audio and transcript | Browser → our EU server → **OpenAI, US** |
| **Premium** | Google Gemini Live | `wss://generativelanguage.googleapis.com/ws/…` (`gemini.rs:59-61`) | Streamed audio and transcript | Browser → our EU server → **Google, global/US endpoint** |
| **All tiers** | Groq | `https://api.groq.com/openai/v1/chat/completions` (`groq.rs:8`) | Subtitle, chat and transcript **text** | Our EU server → **Groq, US** |
| Batch only | Deepgram | `https://api.deepgram.com/v1/listen` (`deepgram.rs:75,155`) | Uploaded files, cloud recordings, voice messages | Our EU server → **Deepgram, US** |

### What this means

**Every single tier exports personal data outside the EEA.** The EU region on Railway and Supabase
controls where data is *stored*; it does not stop the live processing hop. That is the honest
headline for this call.

`QWEN_REALTIME_ENDPOINT` is **not set** in production, so the default international endpoint is in
use — Alibaba **Singapore**. Per `CLAUDE.md`, Model Studio only carries realtime models in Beijing
or Singapore; **US (Virginia) authenticates and then has no realtime model to open.** There is
therefore **no EU processing region for the Standard tier**, and Standard is the default *and* the
capacity-fallback engine — a session can land on it even when the customer picked something else.

Two consequences worth naming out loud:

1. **Alibaba Singapore is a third-country transfer with no adequacy decision** and, for a security
   operations centre, a Chinese-headquartered processor is likely a procurement blocker
   independent of GDPR. Expect this to be the hardest question on the call.
2. **The Enhanced tier's browser-direct flow does not exempt us.** Audio going browser → Cartesia
   without touching our server is a good architectural fact for *retention*, but VoxTranslate
   still determines the purpose and means, so it is still our transfer to paper.

### Provider EU region options and DPAs — **UNVERIFIED, do not assert**

Nothing about a provider's commercial offering is knowable from this repository, and my own
knowledge of provider region availability is not current enough to state on a sales call. What the
code proves is only that **we are not using an EU endpoint for any of them today** — every URL
above is a global or US host.

`directus/legal/privacy.en.md` §4 names all sub-processors correctly and states that "where that is
the case we rely on appropriate safeguards such as the EU Standard Contractual Clauses." **Whether
a DPA has actually been executed with each provider is a business fact not visible in the code.**
Confirm each one before Tuesday, and if a signature is missing, say "in progress" rather than
"in place".

---

## 3. Retention and the existing "compliance mode"

### What compliance mode actually does — this is the section to read twice

Grepped across server, dashboard and migrations, `compliance_mode` does exactly **two** things:

1. **It is Enterprise-plan gated.** A Business-plan org that tries to enable it gets a 403
   (`business/organizations.rs:196-212`).
2. **When true, the retention sweep writes one extra `audit_logs` row** per purge, action
   `retention.purge` (`business/retention.rs:105-125`).

**That is the entire implementation.** It does not change encryption. It does not change access
control. It does not impose a retention limit. It does not restrict sub-processors or regions.

And the audit trail it gates is largely redundant: `business/audit.rs:19-21` states that audit
events are "Always logged now (not just in compliance mode) so every org keeps an activity
history."

> **Do not describe "compliance mode" as a compliance feature on this call.** It is an
> Enterprise-gated flag that adds one audit row to retention purges. If a GSOC buys on the strength
> of the name and later reads the code, that is a contract problem.

### Retention

- `retention_days` is an org setting, defaulting to **90** (`migrations/016_business_workspace.sql:40`).
- Enforcement is the background sweep in `business/retention.rs`, which **ships dormant** — it only
  runs when `RETENTION_SWEEP_ENABLED` is truthy.
- `RETENTION_SWEEP_ENABLED` **is present** in production. Its value came back redacted through the
  connected Railway integration, so **verify it in the dashboard before Tuesday.** If it is not
  truthy, the configured retention window is documentation, not enforcement. This is the single
  highest-value five-minute check in this report.
- The sweep is correctly built: bounded batches, storage object deleted **before** the DB pointer is
  cleared (so a failure retries rather than orphaning), transcript rows and recording deleted in one
  transaction, session marked `transcript_status = 'expired'`.

### What happens to audio and transcripts after a session

- **Live audio is never stored by us.** It is streamed to the engine, played, gone. On Enhanced it
  never reaches our servers at all.
- **Transcripts persist** for any call with at least one signed-in participant. Guest-only sessions
  are purged at finalize (`transcripts.rs:278`, test `finalize_purges_guest_only_sessions`).
- **Cloud recording is opt-in and subscription-gated** (`business/recording.rs`). The browser uploads
  the video straight to a private Supabase `recordings` bucket via a one-shot signed URL; playback
  URLs default to a 24-hour TTL. **Then Deepgram fetches the recording by signed URL to transcribe
  it** — so enabling cloud recording sends recorded meeting audio to Deepgram in the US. Flag that
  explicitly; it is easy to miss.
- The **translation cache is not deployed**, so nothing is cached today.

### Right to erasure

Real mechanisms exist:

- `GET /api/user/data` — export (`api.rs:4006`)
- `DELETE /api/user` — erase (`api.rs:4020`)
- `POST /api/admin/user/delete` — operator-initiated erase (`admin.rs:494`)

Erasure is `DELETE FROM users WHERE id = $1` relying on FK cascade; `transcript_events.speaker_user_id`
and `session_participants.user_id` are both `ON DELETE CASCADE` (`migrations/004_transcripts.sql`).

**Correction (2026-09-05, second pass).** The first version of this report said erasure left
"orphaned objects with no pointer". **The mechanism was wrong, and the truth was worse.** No table
carrying a storage path has a cascading FK to `users`: `chat_files` was session-scoped with no user
column at all, `project_voice_messages.created_by` is `ON DELETE SET NULL`, and `call_sessions` has
no user column. So erasure deleted none of those rows — the objects were not orphaned, they were
simply **never erased**, and `sender_name` / `created_by_name` survived the account as live PII.
The "never calls `storage.delete()`" half of the finding was correct.

**Status of the three gaps:**

1. **Storage objects were not deleted — FIXED for chat uploads** (migration 053 + `safety.rs`).
   `delete_user` now resolves the user's chat-file objects, deletes them from Supabase Storage
   **before** touching the account, and fails loudly if storage cannot be cleared — so a failure
   leaves the account fully intact and the operation can simply be retried. Covered by
   `server/tests/gdpr_erasure.rs`. **Two carve-outs remain by design**, both because the
   organisation is the controller for that data (migration 016: *"Org-owned: keep the project if
   the creator's personal account is deleted"*): cloud recordings are multi-party artifacts, and
   `project_voice_messages` follows the same org-owned rule. Both need the tenant-admin path in
   gap 2. **And one hard limitation: chat files uploaded before migration 053 carry no uploader
   id and are permanently unattributable** — they cannot be reached by erasure, ever. Say this
   plainly if asked; it is the kind of thing an auditor finds.
2. **No org-level or per-subject erasure for a B2B customer.** A GSOC will need "erase this
   employee's data" as an admin action. Today only the end user or a VoxTranslate operator can
   trigger erasure. There is no tenant-admin route. This is now the *largest* remaining Art. 17
   gap, and it is what would close the two carve-outs above. It can reuse the fixed deletion path.
3. **The export is incomplete.** `EXPORT_SQL` (`safety.rs`) covers profile, credit
   transactions, usage sessions, reports, call sessions and transcript events. It omits chat
   messages, file attachments, business/org transcripts, voice messages, and the Cartesia
   voice-clone identifier. As an Art. 20 portability response it is under-inclusive.
4. **Webinar chat attachments are still unreachable by erasure.** `webinar_chat_messages`
   stores only an expiring signed URL, never an object path, and the path is supplied by the
   browser — so wiring it up needs both a schema change and server-side path validation to stop a
   client naming someone else's object. Not attempted here.

---

## 4. Security baseline

### In transit — solid

- WebRTC media is DTLS-SRTP by construction, peer-to-peer, never through our server.
- TURN relay is configured over TLS (`TURN_TLS_URLS`, Cloudflare TURN) and relays ciphertext it
  cannot read.
- API and WebSocket traffic is TLS via Cloudflare to `api.voxtranslate.app`.
- Every provider connection is `wss://` or `https://`.

### At rest

- Supabase Postgres and Storage: platform-managed encryption at rest; buckets are **private**, access
  only via time-limited signed URLs (24h default for recordings, `SUPABASE_SIGNED_URL_TTL_SECS`,
  reduced from 7 days in issue #117).
- Google OAuth tokens are encrypted at the application layer (`GOOGLE_TOKEN_ENC_KEY`).
- **No customer-managed keys / BYOK anywhere.** If the GSOC asks for CMK, the answer is no.
- DragonflyDB: not deployed. ImageKit: does not exist.

### Logging — clean, and this is a genuine strength

I grepped every `tracing::*` call in `server/src`. **No raw transcript text, chat content or audio
is logged anywhere.** Errors log identifiers and failure causes only. The cache module logs the MD5
key and explicitly documents why (`cache/mod.rs`, spec 0107 R7/R8).

Logs ship to **Better Stack** when `BETTERSTACK_SOURCE_TOKEN` is set — it is set in production.
`BETTERSTACK_INGEST_URL` is also set but its value was redacted, so **confirm whether the ingest
endpoint is the EU one**. Log retention is governed by the Better Stack plan, not by our code, and
the privacy policy says only "a limited period" — a GSOC will want a number.

### Prior security assessment

`docs/security/2026-08-28-redteam-assessment.md` is a real static audit: parameterized queries
throughout, `WHERE id = $1 AND org_id = $2` on every multi-tenant query, HS256-pinned JWT,
constant-time admin secret comparison, Stripe webhook signature + timestamp freshness, zero
`cargo audit` vulnerabilities across 668 crates.

Its one HIGH finding — anonymous unmetered Groq spend via the `translate_text` WS frame — **has
been fixed.** The frame now rejects guests and takes an admission permit
(`lib.rs:2337-2366`, test `translate_text_rejects_a_guest_even_on_a_client_direct_engine`). You can
hand this document over; it reads well and the finding is closed.

---

## 5. Gaps and quick wins

### ✅ Honestly GDPR-aligned today

- Backend in **europe-west4 (Netherlands)**, database and storage in **eu-west-1 (Ireland)**.
- **Live audio is never stored.** Meeting media is P2P; the server never touches video.
- **No raw transcript or audio content in application logs.** Verified by grep, not by assertion.
- Working **export and erasure endpoints**, plus an operator-initiated route.
- Guest-only calls leave **no transcript at all**.
- Private buckets, short-lived signed URLs, encryption in transit end to end.
- A **complete and accurate sub-processor list** in the published privacy policy that matches the code.
- A well-built retention sweep with correct delete ordering.
- **Translation cache not deployed** — nothing is cached anywhere.
- Clean security assessment with its one HIGH finding remediated.
- An existing, thoughtful **EU AI Act compliance record** (`docs/eu-ai-act-compliance.md`) — a GSOC
  buying real-time translation in 2026 will ask about Art. 50, and having this written down is a
  differentiator.

### 🟡 Fast fixes before Tuesday (config, doc, or verification only)

1. **Verify `RETENTION_SWEEP_ENABLED` is truthy in the Railway production environment.** Five
   minutes. Without it, the 90-day default retention is not enforced. Highest value item here.
2. **Verify `BETTERSTACK_INGEST_URL`** points at the EU ingest endpoint, and get the log retention
   period in days from the Better Stack plan.
3. **Fix `docs/runbooks/113-railway-region.md`** — it still recommends `us-east` and contradicts the
   actual EU deployment. Doc-only change, but it is the document a customer's auditor would read.
4. **Confirm DPA / SCC status with each of the seven sub-processors** and write the answers down.
   Business action, no code. Say "in progress" where it is.
5. **Write a one-page data-flow diagram per tier** from §2 of this document. This is the artifact
   that wins the technical part of the call.
6. **Decide and state the tier policy for this customer.** If Standard is unacceptable because of
   Alibaba Singapore, note that Standard is also the **capacity-fallback** engine — the policy has to
   be enforced in configuration, not just in a slide.
7. **Correct the sub-processor list in the brief** (no Deepgram on live, no ImageKit).

### 🔴 Real gaps — do not paper over these

1. **No EU processing region for any AI engine.** Every tier exports audio or text outside the EEA;
   the Standard default goes to **Alibaba Singapore**. This is the central gap and no config change
   fixes it. It needs either a provider with an EU region, or a self-hosted engine tier.
2. **Standard is the capacity-fallback engine and never refuses a session.** Even a customer pinned
   to Pro or Premium can be silently served by Standard under load. For a customer whose objection
   is specifically Alibaba, this needs a hard per-org engine allow-list that does not exist today.
3. **Account erasure leaves orphaned storage objects.** Cloud recordings and chat attachments
   survive `DELETE /api/user`. This is a genuine Art. 17 defect, not a nuance. Needs code.
4. **No tenant-admin erasure or per-subject deletion for B2B.** A GSOC admin cannot erase a
   departed employee's data themselves.
5. **The data export is under-inclusive** for Art. 20 — missing chat, files, business transcripts,
   voice messages and the voice-clone identifier.
6. **"Compliance mode" does not do what its name implies.** Fix the name, the docs, or the feature —
   but do not sell it as-is.
7. **Voice cloning stores a voice clip and a synthetic voice at Cartesia (US).** A voiceprint is
   plausibly Art. 9 special-category biometric data. There is no DPIA on file. For a 24/7 security
   operations centre this deserves its own conversation, and `docs/eu-ai-act-compliance.md` §5
   already flags that a *stored* cloned voice loses the live-audio exemption.
8. **No DPIA, no records of processing (Art. 30), no CMK, no formal sub-processor change-notice
   process.** Standard enterprise-procurement asks. None exist today.
9. **Single-region, single-replica deployment.** No EU failover for a 24/7/365 customer.

---

## Suggested posture for the call

Lead with what is true and verifiable: **EU infrastructure, no stored audio, no content in logs,
P2P media, working erasure, a published and accurate sub-processor list, and a documented AI Act
position.** That is a stronger starting hand than most vendors in this category bring.

Then get ahead of the one thing they will find anyway: **live translation necessarily sends audio to
a model provider, and today none of ours runs in the EU.** Name it before they do, name Alibaba
Singapore specifically for the default tier, and bring the tier-policy question as a proposal rather
than a concession.

Do not say "GDPR compliant". Say **"EU-hosted, with these transfers, papered these ways, and these
three items on the roadmap"** — and have the roadmap dates ready.

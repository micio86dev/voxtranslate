# 0111 — Real-time translated telephone calls (VoIP)

| | |
|---|---|
| **Status** | 🚧 In progress — see §5 for what is built and §8 for what is not |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-09-09 |
| **Shipped** | — |
| **Version** | — |
| **Depends on** | [0093](../0093-premium-translation-engine/spec.md), [0043](../0043-low-latency-capture/spec.md), [0106](../0106-voxtranslate-for-business/spec.md), [0110](../0110-talk-to-anyone/spec.md), [0009](../0009-session-transcripts/spec.md), [0010](../0010-composite-recording/spec.md) |

## 1. Context & Problem

Every surface VoxTranslate has today requires the other person to be **on the internet, in
a browser, running our code**. Rooms, webinars, the extension and Talk to Anyone all
assume it. That is a hard ceiling on the B2B pitch: an Italian sales rep cannot call a
supplier in Shenzhen, because the supplier is not going to install anything — they are
going to answer a telephone.

This spec adds the one surface that removes the requirement entirely:

> The caller speaks Italian into VoxTranslate. The recipient's ordinary mobile phone
> rings, they answer, and they hear Mandarin. They speak Mandarin; the caller hears
> Italian. No account, no app, no browser, no internet on the recipient's side.

**Decisive finding: this is mostly already built.** A telephone call is a room with one
browser peer and one *phone peer*. Everything downstream of "audio arrived for a speaker"
— the engine registry, the tiers, capacity fallback, subtitles, transcripts, recordings,
projects, AI analysis, credits, retention — already works and is reused unchanged. What is
genuinely new is narrow: a telephony provider abstraction, a media bridge that makes a
phone leg look like a speaker, destination-priced billing with reservations, recipient
consent, and a dialer.

**Second decisive finding: the provider's media limits choose the architecture for us.**
Telnyx allows *one* streaming operation and *one* bidirectional RTP stream **per call
leg**. That rules out any design that bridges the two parties and forks a mixed stream —
and, conveniently, bridging is exactly what would leak untranslated audio. See §4.

**Third decisive finding, and it is uncomfortable: nothing we run processes in the EU.**
`docs/gdpr-readiness-2026-09.md` says it plainly and the code confirms it — Standard goes
to Alibaba in Singapore, Pro to OpenAI in the US, Premium to Google, and **Groq in the US
handles subtitle/chat/transcript text on every single tier**. Moving Qwen to Frankfurt
(already supported: `qwen_catalogue.rs:202`) does not fix it, because Groq is still there.
So EU-only processing is implemented as an enforced, honest, currently-unsatisfiable
capability rather than a marketing claim. See §4 and `docs/voip-data-flow.md`.

## 2. Goals / Non-Goals

**Goals**

- **Mode 1 first**: an authenticated Business/Enterprise user dials an E.164 number from
  the dashboard and both directions are translated in real time, full duplex.
- One `TelephonyProvider` abstraction with a `TelnyxTelephonyProvider` and a
  `MockTelephonyProvider`. Provider ids and payloads stop at the adapter boundary.
- Reuse the engine registry, tiers and capacity fallback verbatim. **No new translation
  architecture.**
- Reuse the org credit pool and its ledger. **No second currency.** Add the one thing it
  lacks — atomic reservations — so concurrent calls cannot overspend one balance.
- A configurable minimum **gross margin** (default 20%) that is a *proven invariant*, not
  a comment.
- The telephone participant is told, in their own language, before anything is recorded or
  transcribed.
- Every failure mode in §7 has defined behaviour. No undefined behaviour on a paid call.
- Modes 2/3/4 are *architecturally reachable* without rewriting business logic.

**Non-Goals**

- Not implementing Modes 2 (inbound), 3 (PSTN↔PSTN) and 4 (SIP) in this spec's first
  milestone. The abstraction admits them; the orchestration does not yet.
- No video *to a telephone*. Video is an opt-in upgrade to a browser room (§4, K).
- No China GA. The route is gated off until physically validated (§4, J).
- No consumer/guest access. This is a Business/Enterprise entitlement; a guest cannot
  dial, for the same reason the extension is account-only — it spends real money.
- No claim of GDPR compliance, and no EU-only advertising, until §4 F's two migrations land.

## 3. Requirements

> Numbered so tests and tasks reference them.

**Dialing and entitlement**

- **R1 — Entitled dial.** As a member of an org with a live Business/Enterprise
  subscription and the `voip_outbound` capability, I dial a supported E.164 number.
  - *Given* the org's subscription is active per `credits::SUBSCRIPTION_ACTIVE_SQL`,
    *when* I submit a valid E.164 destination, *then* the call is created and reaches
    `Dialing`.
  - *Given* no live subscription, or the capability disabled, *then* the request is
    refused with a structured reason and no provider call is created.
- **R2 — E.164 only.** *Given* a destination that is not valid E.164 after normalisation,
  *then* the request is refused before any provider call, with reason `invalid_number`.
- **R3 — Destination policy.** *Given* the destination country is in
  `VOIP_BLOCKED_COUNTRIES`, or absent from a non-empty `VOIP_ALLOWED_COUNTRIES`, or
  international while `VOIP_ALLOW_INTERNATIONAL` is false, *then* the call is refused
  before dialing.
- **R4 — Rate ceiling.** *Given* the destination's known provider cost per minute exceeds
  `VOIP_MAX_DESTINATION_RATE`, *then* the call is refused before dialing with reason
  `destination_too_expensive`.
- **R5 — Unknown rate fails closed.** *Given* no rate is known for the destination (empty
  or stale rate deck), *then* the call is refused with reason `rate_unavailable`. It is
  never dialed on a guessed price.
- **R6 — Concurrency caps.** Per-user, per-org and global concurrent-call caps are
  enforced atomically; exceeding one refuses before dialing.

**Billing**

- **R7 — Minimum gross margin.** For every quote, `(charge − provider_cost) / charge >=
  VOIP_MIN_GROSS_MARGIN`. Proven by property test over random costs, margins and buffers.
- **R8 — Decimal money.** No floating-point currency arithmetic anywhere in the VoIP
  billing path. Credits are integers at 1 credit = $0.01; intermediate money is `Decimal`.
- **R9 — Reservation.** *Given* an estimated duration, *when* the call is created, *then*
  credits are reserved atomically against the org pool; two simultaneous calls cannot both
  reserve the same last credit.
- **R10 — Settlement.** *When* the call ends, *then* the reservation is settled to actual
  consumption and the unused remainder released, in one transaction. Reserved + released +
  settled always reconciles; no credits are created or destroyed.
- **R11 — Insufficient credits.** *Given* the pool cannot cover the reservation, *then*
  the call is refused with reason `insufficient_credits`. *Given* it empties mid-call,
  *then* new billable translation work stops and the call ends cleanly per policy.
- **R12 — Reconcile.** *When* provider CDR cost arrives, *then* `actual_provider_cost` and
  the realised `gross_margin` are recorded; a realised margin below the configured minimum
  raises an alert and is never silently absorbed.
- **R13 — Both directions.** Translation cost accounts for both conversational directions
  and honours `cost_scales_per_language`.

**Media and translation**

- **R14 — Full duplex, cross-wired.** Each party hears only the translation of the other.
  Raw audio from one leg never reaches the other unless `hear_original` is explicitly
  enabled.
- **R15 — Barge-in.** *When* a party starts speaking while synthesized audio is queued for
  them, *then* the queue is cleared before new audio is written.
- **R16 — Codec.** The stream codec is negotiated (L16 16 kHz preferred, PCMU fallback),
  recorded per call, and resampled to/from the engines' PCM16 24 kHz without unnecessary
  transcoding.
- **R17 — Media resilience.** A dropped media WebSocket reconnects with backoff; the call
  is not torn down for a transient media loss inside the configured grace window.
- **R18 — DTMF.** Digits pressed by the telephone participant are received and routed
  (consent flow, and future IVR), never mistaken for speech.

**Consent, privacy, EU**

- **R19 — Disclosure before capture.** *Given* recording or transcription is enabled,
  *then* a localized spoken announcement is played to the telephone participant **before**
  any recording or transcript is persisted, and `disclosure_played_at` /
  `disclosure_language` are stamped.
- **R20 — Consent policy.** `NOTICE_ONLY | PRESS_KEY_TO_CONSENT | VERBAL_CONSENT |
  DISABLED`, resolved per org with country override. *Given* `PRESS_KEY_TO_CONSENT` and no
  consenting digit within the timeout, *then* recording/transcription does **not** start;
  the call either continues unrecorded or ends politely per org policy.
- **R21 — Truthful announcement.** The announcement states only what is actually enabled.
  With both disabled it is not played at all and claims nothing.
- **R22 — EU processing gate.** *Given* `VOIP_REQUIRE_EU_PROCESSING` is true and the
  selected tier's `supports_eu_only` is false, *then* the call is refused with reason
  `eu_processing_unavailable`, logged. It is never silently downgraded or silently allowed.
- **R23 — No content in logs.** Phone numbers, transcript text and audio never appear in
  normal logs. Numbers are stored pseudonymised with a last-4 display form.

**Webhooks and state**

- **R24 — Signature.** A webhook with a missing, malformed, wrong or expired
  (>`VOIP_WEBHOOK_TOLERANCE_SECS`) signature is rejected and changes no state.
- **R25 — Idempotent.** A redelivered provider event is accepted and changes state exactly
  once, enforced by `UNIQUE(provider, provider_event_id)`.
- **R26 — Out-of-order.** Events arriving out of business order never regress the call
  state; transitions are deterministic and monotonic.
- **R27 — Restart-safe.** Authoritative call state survives process restart and horizontal
  scaling; no in-memory-only state is required for correctness or for money.

**Tenancy and surface**

- **R28 — Isolation.** A member of org A can never read a call, transcript, recording,
  number or setting belonging to org B. Non-admin members see only calls they took part in.
- **R29 — History and project.** A completed call appears in org history and, when bound,
  in its project alongside transcript, translated transcript, recording, AI summary,
  sentiment, cost and participants.
- **R30 — Accessible dialer.** The dialer is fully keyboard operable; call-state changes
  are announced to assistive technology; no state is conveyed by colour alone.
- **R31 — i18n complete.** Every new user-facing string ships in all 84 client locales and
  all dashboard/website locales in the same change.

## 4. Design & Architecture

### Components / files

| Module | Responsibility |
|---|---|
| `server/src/telephony/mod.rs` | `TelephonyProvider` trait, `CallLeg`, `DialRequest`, `ProviderEvent`, `ProviderError`, registry |
| `server/src/telephony/telnyx.rs` | Telnyx Call Control v2 adapter (EU base), webhook verification |
| `server/src/telephony/mock.rs` | In-process provider used by every automated test and E2E |
| `server/src/telephony/e164.rs` | Normalisation, country/prefix extraction, pseudonymisation |
| `server/src/voip/mod.rs` | `VoipSession` orchestration — the fourth engine consumer |
| `server/src/voip/state.rs` | Deterministic, monotonic call state machine |
| `server/src/voip/media.rs` | Media WebSocket terminator; phone leg ⇄ engine wiring |
| `server/src/voip/codec.rs` | L16/PCMU ⇄ PCM16 24 kHz, resampling, barge-in queue |
| `server/src/voip/pricing.rs` | Cost model, margin guard, rate deck, quotes |
| `server/src/voip/reservation.rs` | Atomic credit hold / settle / release |
| `server/src/voip/consent.rs` | Policy resolution, announcement text, DTMF gather |
| `server/src/voip/routes.rs` | HTTP surface, mounted by `business/routes.rs` conventions |
| `server/src/voip/webhook.rs` | Signature, idempotent ingestion, transition dispatch |
| `dashboard/src/pages/[lang]/phone/` | Dialer, active call, history, detail, admin |
| `website/src/pages/…` | Marketing page + FAQ driven by capability data |

### Data model

Migration `056_voip.sql`, idempotent per repo convention. A VoIP call **is** a
`call_sessions` row with `kind = 'phone'`, so projects, transcripts, recordings, AI
reports, search, retention and erasure keep working with no changes; `voip_calls` carries
only what is telephony-specific and references it.

- `voip_calls`, `voip_call_quality`, `voip_provider_events`, `voip_credit_reservations`,
  `voip_rates`, `voip_numbers`, `voip_org_settings`, `voip_route_validations`.
- Structured columns, not JSON blobs. Indexes for org history, user history, project
  calls, provider-leg lookup, date range, billing reports and audit queries.

### Key decisions

**D1 — Telnyx as the initial provider, behind an abstraction.**
Rationale: EU Voice API with Frankfurt media anchoring (`api.telnyx.eu`), bidirectional
media streaming with L16, a public rate deck (`GET /v2/public/pricing?primitive=voice`),
Ed25519 webhooks. *Alternative rejected:* coupling the domain to Telnyx — provider ids and
payloads are confined to `telephony/telnyx.rs`, and `MockTelephonyProvider` proves it by
being the provider every test uses.

**D2 — The two legs are NEVER bridged.**
Rationale: the provider allows one streaming operation and one bidirectional RTP stream
per leg. Bridging plus forking cannot express "each party hears only the translation".
Independent legs, cross-wired through the engine, satisfy the limit *and* make raw-audio
leakage structurally impossible rather than a thing we remember to prevent.
*Alternative rejected:* bridge + `fork_media` — one stream budget, and it mixes the parties.

**D3 — In Mode 1 the caller has no provider leg at all.**
The caller is an ordinary browser peer on the existing PCM16 24 kHz capture path (spec
0043) with the existing translated-audio return path. Only the recipient is a telephone
call. *Rationale:* halves provider cost, removes the Telnyx WebRTC SDK and its
China-blocked-CDN exposure, keeps the caller's audio out of the telephony provider
entirely, and reuses `audio-capture.ts`, the engines and the subtitle pipeline verbatim.
Modes 2/3 replace the browser peer with a second `CallLeg`; the media bridge does not care.

**D4 — Media plane placement is configuration, not architecture.**
The media WebSocket terminator is part of the same binary, addressed by
`VOIP_MEDIA_WS_BASE`. Default is the control-plane deployment, because it already carries
384 kbps per speaking browser peer and a phone leg at L16 16 kHz is 256 kbps in+out — less
than one existing speaker. Moving it to the Hetzner boxes is a config change **gated on a
measured capacity report**; webinar workloads must be shown not to starve, and vice versa.
*No capacity number is stated without measurement.* *Alternative rejected:* Telnyx Edge
Compute — its EU-only execution cannot be shown for our workload, and the goal forbids
assuming it.

**D5 — Gross margin is a guard over the existing markup convention.**
The codebase prices as `cost × (1 + markup)`; the commercial requirement is a margin
floor. They are the same statement: `margin = markup / (1 + markup)`, so today's 25%
markup **is** a 20% gross margin. Rather than introduce a competing pricing scheme, the
VoIP quote computes `price = cost × (1 + buffer) / (1 − min_margin)` and a property test
asserts the invariant for all inputs. *Alternative rejected:* a separate VoIP price table.

**D6 — Credit reservations, because the existing ledger has none.**
`deduct_org_credits_tx` is atomic per deduction but nothing holds funds for a call that
has not happened yet, so N simultaneous dials can each see the same balance. A `held` row
plus the same `FOR UPDATE` discipline closes it, and settlement is one transaction so
reserved = settled + released always holds. *Alternative rejected:* deduct optimistically
and refund — it can leave a customer at a negative balance mid-call.

**D7 — Consent is spoken to the phone, not shown in a UI.**
The recipient has no interface. Announcement is TTS in *their* language, played before any
persistence, with an optional DTMF gate. Provider recording-beep is an additional signal,
never a substitute. Legal conclusions are not encoded — policies are configurable per org
and per country and the documentation says final legal review is required.

**D8 — EU-only is a capability that is currently false everywhere.**
`supports_eu_only` is read from config per engine, defaults false, and
`VOIP_REQUIRE_EU_PROCESSING` refuses rather than downgrades. Documented in
`docs/voip-data-flow.md` with the real per-provider regions. It becomes true only when
**both** the Standard tier's Qwen route moves to `dashscope.eu-central-1` *and* Groq's
text path has an EU region — one without the other changes nothing.

**D9 — Video upgrade reuses the existing WebRTC room.**
A short-lived signed guest URL into a private VoxTranslate room, carrying call context,
project and languages. *Alternative rejected:* Telnyx Video Rooms — no advantage over the
shipped mesh and migrating the meeting architecture is out of scope. Video failure must
never disturb the PSTN call.

**D10 — China is gated on measured evidence.**
`VOIP_CHINA_ENABLED` (default off) and `VOIP_CHINA_REQUIRE_VALIDATED_ROUTE` (default on)
read a recorded `voip_route_validations` row. VPN tests do not qualify and the runbook says
so. Nothing about China is advertised until a physically-in-China call is recorded.

**D11 — Rate deck sync fails closed.**
Rates are synced from the provider's public pricing endpoint into `voip_rates` with
`fetched_at`. Past `VOIP_RATE_MAX_AGE_SECS` a rate is stale; a stale or missing rate
refuses the call (R5) rather than guessing. *Alternative rejected:* a hand-maintained table.

### Protocol / API

```
POST   /api/business/organizations/{org}/voip/quote      → estimate before dialing
POST   /api/business/organizations/{org}/voip/calls      → create + dial
GET    /api/business/organizations/{org}/voip/calls      → history (paginated, RBAC)
GET    /api/business/organizations/{org}/voip/calls/{id} → detail
POST   /api/business/organizations/{org}/voip/calls/{id}/hangup
POST   /api/business/organizations/{org}/voip/calls/{id}/dtmf
POST   /api/business/organizations/{org}/voip/calls/{id}/video-invite
GET/PUT/api/business/organizations/{org}/voip/settings   → admin policy
GET/POST/DELETE …/voip/numbers                           → number management
POST   /api/voip/webhooks/{provider}                     → signed, idempotent
WS     {VOIP_MEDIA_WS_BASE}/voip/media/{leg_token}       → provider media stream
```

### Sequence — Mode 1 happy path

1. Dashboard requests a quote; server normalises E.164, looks up the rate (longest prefix,
   freshness-checked), prices tier + telephony + options, returns rate/min and balance.
2. User dials. Server checks entitlement, policy, caps, EU gate → creates the
   `call_sessions` (`kind='phone'`) row and `voip_calls` → **reserves credits atomically**.
3. Provider `dial` with EU base + Frankfurt anchorsite; leg id stored; state `Dialing`.
4. `call.answered` webhook (verified, idempotent) → state `Answered`; `streaming_start`
   with L16 bidirectional on the recipient leg.
5. Consent: announcement TTS played in the recipient's language; DTMF gathered if policy
   requires; columns stamped; recording/transcription started only if consented.
6. Media bridge runs: recipient inbound → resample → engine session (target = caller's
   language) → translated audio written to the **caller's** browser path; caller's browser
   capture → engine (target = recipient's language) → resample → written back on the
   recipient leg. Barge-in clears the opposite queue.
7. Meter ticks per second through `CreditAccumulator` against the reservation.
8. Hangup at either side → `call.hangup` → state `Completed`; media closed; transcript
   finalised; reservation settled and remainder released; AI analysis enqueued if enabled.
9. CDR arrives → `actual_provider_cost`, realised margin, quality row.

## 5. Implementation

| Slice | What | Status | Key files |
|-------|------|--------|-----------|
| S0 | Spec, config surface, migration `056` | ✅ | this file, `config.rs`, `migrations/056_voip.sql` |
| S1 | `TelephonyProvider`, `MockTelephonyProvider`, E.164, state machine | ✅ | `telephony/`, `voip/state.rs` |
| S2 | Pricing, rate deck, margin guard, reservations | ✅ | `voip/pricing.rs`, `voip/reservation.rs` |
| S3 | Telnyx adapter, webhook signature + idempotency, settlement | ✅ | `telephony/telnyx.rs`, `voip/webhook.rs` |
| S4 | Codec, stateful resampling, barge-in, media bridge, media ticket | ✅ | `voip/{codec,media,token}.rs` |
| S5 | Pre-dial gate, dial orchestration, HTTP surface, tenancy | ✅ | `voip/{policy,service,routes}.rs` |
| S6a | Consent policy + the spoken disclosure in all 84 languages | ✅ | `voip/consent.rs`, `assets/voip-disclosure.json` |
| S6b | Consent **execution** (speak, gather, resolve, timeout) + recording start | ✅ | `voip/disclosure.rs` |
| S6c | Recording handle + retention deletion at the carrier | ✅ | `migrations/057_*.sql`, `business/retention.rs` |
| S6d | AI-analysis auto-enqueue on call end | ⛔ **not built** — the manual report route already works | — |
| S7 | Dashboard dialer, history, i18n, browser e2e | ✅ | `dashboard/src/{pages/[lang]/phone.astro,scripts/phone-dialer.ts}` |
| S8 | Website page, FAQ from capability data, SEO | ✅ | `website/src/pages/phone-call-translation.astro` |
| S9 | Metrics, k6 load suite, China gate, runbook | ✅ | `metrics.rs`, `loadtest/voip-*.js`, `docs/runbooks/122-voip-operations.md` |
| S10 | Media socket **route**, call session assembly | ✅ | `voip/session.rs`, `voip/routes.rs` |
| S11 | Video upgrade | ⛔ architecture only (D9) | — |

### Where the call stands

A call placed today reaches the carrier, rings, is billed correctly, settles correctly,
appears in history — **and carries translated audio in both directions.** S10 closed that:
the dial creates a private room with a synthetic phone peer, the answer webhook mints a
single-use ticket and asks the carrier to open a media stream against it, and the socket
that arrives claims the parked leg, opens an engine session for the telephone as a speaker
and runs the pump. From there the room's ordinary fan-out does the rest — nothing
downstream knows one of its peers is a telephone.

S6b closed the second gap: the recipient now hears the disclosure, in their own language,
before anything is kept, and a press-key gate is genuinely open and genuinely resolved.

S6c closed the third: a recording now carries a **durable handle** on the carrier's copy,
and the retention sweep deletes the bytes there before forgetting where they were.

The transcript and the project needed no hookup at all, and finding that out was the point
of S10's session-id decision: the phone peer writes into the same `call_sessions` row as
the browser caller, so `transcript_events`, the project link and the existing AI-report
route already work on a phone call exactly as they do on a meeting.

What is left is S6d — enqueueing the AI analysis automatically when a call ends. The
manual route (`POST …/report`) already accepts a phone session today, so this is a
convenience rather than a gap in capability. That is the honest state, stated here rather
than in a status update nobody will re-read.

## 6. Testing & Verification

Test-driven throughout: failing test first, minimal implementation, refactor with the
suite green. Coverage floors are not lowered (`--fail-under-lines 66` server, 85% client).

- **Domain/unit** — E.164 normalisation and pseudonymisation; longest-prefix rate match
  and staleness (R5); every state transition including illegal and out-of-order (R26);
  codec conversion against fixtures (R16); consent policy resolution (R20); capability
  gating (R22).
- **Billing** — margin invariant as a **property test** (R7); decimal arithmetic and
  rounding (R8); cheap and expensive destinations; both directions (R13); recording and
  transcription surcharges; reservation and concurrent reservation (R9); settlement
  reconciliation (R10); early hangup; provider cost over estimate (R12); refund/release;
  zero balance and mid-call exhaustion (R11); monthly bonus and purchased credits;
  pricing unavailable (R5).
- **Webhooks** — valid/invalid/missing/expired signature (R24); duplicate delivery (R25);
  out-of-order (R26); unknown event type; restart replay (R27).
- **Provider contract** — `MockTelephonyProvider` drives the real state machine; recorded
  Telnyx fixtures pin the wire format, including the one-bidirectional-stream constraint.
- **Authorization / tenancy** — cross-org denial on every route, mirroring
  `tests/*_scope.rs` (R28).
- **E2E (Playwright)** — dialer → project → number → languages/tier → cost estimate →
  simulated call → ringing → connected → transcript → end → credits updated → history →
  project → disclosure state → AI analysis. Provider mocked at the boundary.
- **Live smoke** — `VOIP_LIVE_TESTS=true` only; never in CI; no credentials in fixtures.
- **Load** — k6 `voip-control.js`, `voip-webhooks.js`, `voip-ledger.js` plus a synthetic
  media-socket harness. Results reported with real numbers or not claimed.

## 7. Deployment & Operations

**Failure modes — all defined, none undefined**

| Event | Behaviour |
|---|---|
| Recipient busy / rejects / no answer | `Failed` with the provider reason mapped to a stable `failure_reason`; reservation released in full |
| Invalid number / unsupported country / rate unknown | Refused **before** dialing; nothing reserved |
| Insufficient credits (pre / mid call) | Refused / billable work stops and call ends cleanly per policy |
| Provider balance or outage | `provider_unavailable`; reservation released; surfaced, not retried blindly |
| STT / translation / TTS failure | Tier capacity fallback first; then the direction degrades to subtitles where possible; call is not silently dropped |
| Media interruption / WS reconnect | Backoff reconnect inside grace window; beyond it, call ends with `media_lost` |
| Caller network loss | Call ends cleanly; billing stops at last authoritative tick |
| Duplicate / out-of-order / delayed webhook | Idempotent, monotonic; recording-saved-after-hangup is handled |
| DB timeout | Provider event is not acknowledged so it is redelivered; no state is invented |
| Project deleted / user removed / subscription expires mid-call | Call continues to a clean end; new billable work and new calls are blocked; audited |
| Call exceeds `VOIP_MAX_CALL_DURATION_MINUTES` | Warned then terminated |
| Provider cost above estimate | Settled honestly, margin alert raised (R12) |

**Configuration** (names follow repo convention; secrets are server-side only)

```
VOIP_ENABLED, VOIP_PROVIDER, VOIP_ROLLOUT_STAGE, VOIP_BETA_ORG_IDS
VOIP_REQUIRE_EU_PROCESSING, VOIP_DEFAULT_REGION, VOIP_MEDIA_WS_BASE
VOIP_MIN_GROSS_MARGIN, VOIP_COST_SAFETY_BUFFER, VOIP_CURRENCY
VOIP_MAX_DESTINATION_RATE, VOIP_DAILY_PROVIDER_SPEND_LIMIT
VOIP_MAX_CALL_DURATION_MINUTES
VOIP_MAX_CONCURRENT_CALLS_{GLOBAL,PER_ORG,PER_USER}
VOIP_ALLOW_INTERNATIONAL, VOIP_ALLOWED_COUNTRIES, VOIP_BLOCKED_COUNTRIES
VOIP_CHINA_ENABLED, VOIP_CHINA_REQUIRE_VALIDATED_ROUTE
VOIP_RECORDING_ENABLED, VOIP_TRANSCRIPTION_ENABLED, VOIP_VIDEO_ENABLED
VOIP_RATE_MAX_AGE_SECS, VOIP_WEBHOOK_TOLERANCE_SECS, VOIP_LIVE_TESTS
TELNYX_API_KEY, TELNYX_API_BASE, TELNYX_CONNECTION_ID,
TELNYX_OUTBOUND_VOICE_PROFILE_ID, TELNYX_PUBLIC_KEY,
TELNYX_DEFAULT_CALLER_ID, TELNYX_MEDIA_ANCHOR
```

**Rollout** — `disabled → internal → beta → business → ga`, reversible at every stage,
with an explicit beta org allowlist. Git Flow: `feature/translated-voip` off `develop`,
staging verified green, then release with `main` first and a back-merge into `develop`.

**Runbooks** — `docs/runbooks/` gains VoIP operations, rollback and troubleshooting;
`docs/voip-data-flow.md`, `docs/voip-billing.md`, `docs/voip-china-validation.md`,
`docs/voip-telnyx-setup.md`, `docs/voip-live-tests.md`.

## 8. Risks / Open Items

1. **No EU-only tier exists.** Verified. Requires *both* the Qwen Frankfurt move and an EU
   route for Groq. Until then EU-only mode is enforced but unsatisfiable, and nothing is
   advertised as EU-processed.
2. **One bidirectional stream per leg** is a provider limit the whole design leans on.
   Pinned by contract test; re-verify on provider changes.
3. **EU endpoint resource parity** is not fully documented; probe with a real account
   before go-live.
4. **Media-plane capacity is unmeasured.** No throughput claim until the load suite runs.
5. **Rate deck freshness** — stale means refuse, and that is a deliberate availability cost.
6. **China route quality unproven**; gated off.
7. **Provider account limits** (concurrency, verification level, caller-ID rules) are a
   human/commercial dependency, not a code one.
8. **Legal text requires human review.** Drafted, flagged, never published autonomously.
9. **The media plane is single-instance.** Rooms are in-memory per instance across the
   whole product, so the media socket must reach the instance holding the call's room.
   Today's deployment is single-replica, which is why this works. State and money survive a
   restart (Postgres, idempotent webhooks); a *live socket* cannot, on any architecture.
   When rooms become distributed, `voip::session::LiveCalls` moves with them — and the
   coupling is written down in that module rather than left to be found by an outage.
10. **The announcement is spoken by the carrier**, via a new `PlayRequest::Speak` and the
    `speech_synthesis` capability. A provider that cannot speak means capture does not
    start — every branch in `voip::disclosure` fails towards *not capturing*. The cost is
    the carrier's voice quality and its language list, which is narrower than our 84; the
    text already falls back to English and records that it did.
11. **A phone recording lives on the carrier's storage, not ours.** `provider_recording_id`
    is the durable handle and `business::retention::sweep_voip_recordings_once` deletes
    through it, bytes first and pointer second. Two consequences are deliberate: a
    provider that saves a recording without giving us an id produces an **un-erasable**
    recording, counted by `voip_unerasable_recordings_total` and logged at ERROR; and an
    org with no `recording_retention_days` keeps its recordings forever, because inventing
    a default would delete a customer's data on a schedule they never set.
    `voip_calls.user_id` is ON DELETE SET NULL by design, so individual account erasure
    does **not** reach these — matching the scope rule `SafetyService::delete_user` already
    documents for multi-party, org-owned artifacts. Deletion belongs to the tenant.
12. **Video upgrade is architecture only** (D9). No signed guest link is minted and no room
    is joined.

## 9. References

- `.claude/goals/voxtranslate-voip.md` — originating requirements
- `docs/gdpr-readiness-2026-09.md` — verified processing regions
- `docs/eu-ai-act-compliance.md` — AI disclosure obligations
- `docs/pricing-standard-qwen.md`, `docs/pricing-talk-to-anyone.md` — existing pricing semantics
- Telnyx: Voice API in Europe; Media Streaming over WebSockets; `streaming_start`;
  webhook Ed25519 signing; `GET /v2/public/pricing`

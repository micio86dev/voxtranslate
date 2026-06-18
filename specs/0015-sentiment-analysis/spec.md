# 0015 — Sentiment analysis (chunked Groq scoring, cached per session)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | micio86dev |
| **Created** | 2026-06-11 *(retroactive backfill)* |
| **Shipped** | 2026-06-10 |
| **Version** | post-v1.0.0 AI bundle |
| **Commits** | `d5ce553` |
| **Depends on** | [0014](../0014-ai-session-report/spec.md) (AI billing pattern, model probe), [0005](../0005-accounts-credits-billing/spec.md) (credits — this feature IS billed), [0009](../0009-session-transcripts/spec.md) (transcripts), [0013](../0013-call-bookmarks/spec.md) (bookmark markers on the chart) |

*Authored retroactively on 2026-06-11 from the shipped code and commit history (backfill of the AI-bundle specs).*

## 1. Context & Problem

A translated meeting's transcript (spec 0009) tells you *what* was said but not
*how it went*. Did the call start tense and recover? Who carried the
conversation, and in what mood? The AI session report (spec 0014) summarizes
content; nothing captures emotional dynamics over time.

This spec adds a per-session sentiment analysis: the transcript is sliced into
time windows, each window is scored by Groq (overall tone plus per-speaker
tone, on a −1.0…+1.0 scale), and the results are aggregated into a mood
timeline, a per-speaker breakdown with talk share, and up to 8 "key moments".
Unlike the report — regenerable with new guidelines — sentiment is a property
of the session itself, so it is computed **once**, cached in the database, and
shared free with every participant afterwards.

## 2. Goals / Non-Goals

**Goals**
- One-click mood analysis of a finished session: timeline (one score per time
  window), overall mood, per-speaker mood + talk-time share, key moments.
- Bounded cost and latency regardless of session length: never more than 30
  Groq calls per analysis (windows widen instead), at most 4 in flight.
- **Cached per session:** the first successful run is billed; every later
  request — POST or GET, from *any* participant — returns the stored result
  without re-running or re-charging. There is no regenerate path.
- User-favorable billing identical to the report: model failure never charges;
  partial chunk failures degrade the result instead of failing it.
- Chart rendered without a chart library, themed to the app, with bookmark
  markers (spec 0013) and clickable key moments that jump the transcript.

**Non-Goals**
- No regeneration / recomputation — the `UNIQUE(session_id)` row is final
  (deliberate: sentiment has no user-supplied inputs to vary, unlike the report).
- No live, in-call sentiment (offline analysis of the stored transcript only).
- No per-utterance scoring — resolution is the time window (≥120 s).
- No emotion taxonomy beyond a scalar score bucketed into
  positive/neutral/negative/mixed.

## 3. Requirements

- **R1 — Run a paid analysis.** As a logged-in participant of a session with
  transcript events, I want a sentiment analysis, billed once.
  - *Given* no analysis exists, *when* I POST `/api/sessions/{id}/sentiment`,
    *then* I get a 201 with the result and am charged
    `base + per_participant × N + per_minute × ⌈minutes⌉`
    (defaults $0.05 + $0.01·N + $0.002·min; ledger kind `ai_sentiment`).
  - *Given* my balance is below the cost, *then* the pre-check returns the
    standard 402 `insufficient_credits` body (feature `ai_sentiment`) before
    any Groq call is made.
  - *Given* the session has no transcript events, *then* 422.
- **R2 — Cache contract.** As any participant, I read the one shared result.
  - *Given* a stored analysis, *when* anyone POSTs again, *then* 200 with
    `cached: true`, no deduction, no ledger row, no `balance` echo.
  - *Given* a stored analysis, *when* I GET the endpoint, *then* the same
    payload (`cached: true`); *given* none yet, GET is 404.
  - *Given* two concurrent generators, *then* `ON CONFLICT (session_id) DO
    NOTHING` lets the first insert win; the loser still delivers the result it
    computed (and was charged for) rather than an error.
  - Standard gates on both verbs: 401 no token, 403 non-participant, 404
    unknown session, 503 when transcripts/billing/DB are not configured.
- **R3 — Analysis content.** As a user, I get an interpretable result.
  - Timeline: one point per non-silent window, score clamped to [−1, 1],
    rounded to 2 dp. Silent windows simply have no point.
  - Overall: mean score; mood `positive` (≥ 0.15) / `negative` (≤ −0.15) /
    `neutral`, except **`mixed`** when the timeline swings ≥ +0.3 *and* ≤ −0.3.
  - Speakers: every session participant listed with talk share (% of speech
    characters; chat excluded; silent participants at 0%) plus the average of
    their model-scored chunks (null score/mood if the model never scored them).
  - Key moments: model-labelled notable moments, strongest 8 by |score| kept,
    presented chronologically.
- **R4 — Robustness & fair billing.** As a user, a flaky model run degrades
  gracefully and never costs me money for nothing.
  - *Given* the primary model 4xxs on the first chunk (decommissioned id),
    *then* the whole run retries on the fallback model (same probe as 0014).
  - *Given* an individual chunk fails or returns malformed JSON (no numeric
    `score`), *then* that point is dropped (logged) and the analysis continues.
  - *Given* **no** chunk succeeds, *then* 502 "you were not charged" and the
    balance/ledger are untouched. *Given* the deduction races to insufficient
    funds after generation, *then* 402 and the result is withheld; *given* the
    deduction fails for OUR reasons, *then* the result is delivered free.
- **R5 — UI.** As a user on the session detail screen, I get a "📊 Sentiment
  analysis" card (slot `#ai-sentiment-slot`): a cost preview computed from the
  pricing endpoint + transcript context (run button disabled with an
  insufficient-credits tooltip when broke), then — after running or when a
  cached result exists — the overall mood (emoji + label + score), a canvas
  timeline, per-speaker cards, and clickable key moments that smooth-scroll the
  transcript to the nearest event and flash it. Localized in all 8 UI
  languages; hidden for guests and empty sessions.

## 4. Design & Architecture

- **Components / files:**
  - `server/src/ai/sentiment.rs` — cost formula, window sizing, chunking,
    talk-share, pure `aggregate()`, `analyze()` orchestration,
    `save_sentiment`/`get_sentiment` persistence.
  - `server/src/api.rs` — `sentiment_generate` (POST) and `sentiment_latest`
    (GET) handlers; `ai_pricing` exposes the `sentiment` price block.
  - `server/src/config.rs` — `AiConfig` fields + `CREDITS_SENTIMENT_*` env.
  - `client/src/scripts/sentiment.ts` — card UI in `#ai-sentiment-slot`.
  - `client/src/scripts/sentiment-chart.ts` — `drawSentimentTimeline` canvas
    renderer. `api.ts` — `fetchSentiment`/`generateSentiment` wrappers +
    types; `session-screen.ts` — `initSentimentSlot(ref)` on open,
    `updateSentimentContext(...)` once the transcript doc lands, `data-ts` on
    transcript rows for key-moment jumps; `pages/index.astro` — scoped
    `.ai-sentiment-view` / `.ai-mood-*` / `.ai-moment*` / `.tr-flash` styles.
- **Data model:** `session_sentiments` (migration `005_features.sql`): id,
  **session_id UNIQUE** (the cache contract), user_id (who paid), result_json
  JSONB, model, cost DECIMAL(10,6), created_at. Cascades with both the session
  and the paying user's account (GDPR).
- **Result JSON shape:** `{ overall: {score, mood}, timeline: [{t, score}],
  speakers: [{name, talk_pct, score|null, mood|null}], key_moments:
  [{t, label, score}], window_secs }` — `t` is seconds from session start.
- **Protocol / API:**
  - `POST /api/sessions/{id}/sentiment` (no body) → 201 fresh
    `{id, result, model, cost, created_at, cached:false, balance}` · 200
    cached (`cached:true`, no balance) · 402 · 422 empty transcript · 502
    model failure (no charge) · 401/403/404/503.
  - `GET /api/sessions/{id}/sentiment` → 200 cached row (`cached:true`) · 404
    none yet.
  - `GET /api/billing/ai-pricing` → `"sentiment": {base, per_participant,
    per_minute}` for the client cost preview.
- **Chunking strategy:** events are bucketed by
  `(event.ts − started_at) / window_secs` into `Speaker: text` lines (chat
  lines tagged `[chat]`). Base window 120 s; for long sessions
  `effective_window()` widens it in whole 120 s steps so an analysis never
  needs more than `MAX_CHUNKS = 30` model calls (e.g. ≤59:59 → 120 s, one
  hour → 240 s, two hours → 360 s). Empty (silent) windows produce no chunk.
- **Scoring:** each chunk goes through Groq JSON mode (`chat_json`,
  max_tokens 256, 20 s timeout, 3 retries on 429) with a system prompt asking
  for `{score: −1.0…1.0, speakers: {name: score}, moment: label|null}` —
  moment labels in the segment's language, max 10 words.
- **Sequence (happy path):** POST → transcript flush barrier → participant
  gate → cache lookup (hit returns immediately) → export + 422 check → cost →
  advisory balance pre-check → first chunk on `GROQ_REPORT_MODEL` (4xx → flip
  whole run to `GROQ_FALLBACK_MODEL`) → remaining chunks `buffered(4)` →
  `aggregate()` → atomic `deduct_feature` → insert (`ON CONFLICT DO NOTHING`)
  → 201. Client paints overall/speakers/moments and draws the canvas on the
  next animation frame.
- **Key decisions:**
  - *Widen windows instead of truncating the transcript* → every minute of a
    long session still influences the timeline, while cost/latency stay
    bounded at 30 calls. Alternative (cap at the first hour) rejected: tails
    of long meetings are exactly where mood shifts.
  - *`UNIQUE(session_id)` as the cache* → the schema, not application logic,
    guarantees "billed once per session"; concurrent generators collapse via
    `ON CONFLICT DO NOTHING`, and a loser that already charged delivers its
    computed result rather than erroring (never charge-and-withhold).
  - *Drop bad chunks, fail only on zero successes* → one flaky 429/parse error
    costs a timeline point, not the whole (paid) analysis; a total failure is
    a 502 that never charges.
  - *Talk share computed from transcript characters, not the model* →
    deterministic, includes silent participants at 0%, and excludes chat
    (typing isn't talking). The model only contributes tone scores.
  - *`mixed` overall mood for ±0.3 swings* → a heated-then-resolved call would
    otherwise average to a misleading "neutral".
  - *Vanilla canvas chart (DPR-aware, CSS-var themed)* → no chart-library
    dependency for one line chart; bookmarks render as dashed `--warning`
    vertical lines, key moments as rings (`--danger` when negative).
  - *Per-participant price term* (unlike the report) → each chunk scores every
    speaker, so work scales with the roster; the formula mirrors that.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Config + storage (shipped with AI-bundle foundations) | `config.rs` (`CREDITS_SENTIMENT_*`), `migrations/005_features.sql` (`session_sentiments`) |
| S1 | Analysis engine: windowing, chunking, talk share, aggregate, fallback probe, persistence | `server/src/ai/sentiment.rs`, `server/src/ai/mod.rs` (`billed_minutes`) |
| S2 | REST endpoints + routes (cache-first POST, GET) | `server/src/api.rs`, `server/src/lib.rs` |
| S3 | Client API wrappers + types | `client/src/scripts/api.ts` |
| S4 | Card UI, canvas chart, transcript jump wiring, styles + i18n (8 langs) | `sentiment.ts`, `sentiment-chart.ts`, `session-screen.ts`, `pages/index.astro`, `i18n.ts` |

## 6. Testing & Verification

- **Unit (server, 7 in `sentiment.rs`):** cost formula (base + participants +
  ceil-minutes, 1-minute floor); `effective_window` widening at the 30-chunk
  cap; chunking on window edges, `[chat]` tagging, silent windows skipped;
  talk share speech-only with silent participants at 0; `aggregate` —
  malformed chunks dropped, clamping to [−1, 1], non-numeric speaker scores
  ignored, `mixed` on both-ways swings, null-score speakers (R3); empty
  aggregate is neutral with speakers still listed; key moments keep the
  strongest 8 then re-sort chronologically.
- **Integration (`tests/transcripts.rs::sentiment_cache_billing_and_gates`,
  DB-gated):** 401/403/404 gates, 404 before any analysis, 402 pre-check with
  the `ai_sentiment` body, Groq-failure → 502 + untouched balance (R1/R4);
  `save_sentiment` UNIQUE race (second insert returns `None`); cached POST is
  200 `cached:true` for *both* participants with no `balance` echo and zero
  `ai_sentiment` ledger rows; GET mirrors the cached POST (R2).
- **Client:** no dedicated unit tests for `sentiment.ts`/`sentiment-chart.ts`
  (DOM/canvas heavy) — covered by strict `astro check` and manual runs; the
  pure server aggregate carries the logic-correctness load.
- **Gates:** server `cargo llvm-cov` ≥85% lines, client vitest thresholds ≥85,
  `astro check` 0 errors, Playwright e2e suite green (the card is auth-gated,
  so the guest e2e backend never reaches it).

## 7. Deployment & Operations

- **Env (server):** `CREDITS_SENTIMENT_BASE` (default `0.05`),
  `CREDITS_SENTIMENT_PER_PARTICIPANT` (`0.01`),
  `CREDITS_SENTIMENT_PER_MINUTE` (`0.002`); models via `GROQ_REPORT_MODEL`
  (default `openai/gpt-oss-120b`) and `GROQ_FALLBACK_MODEL`
  (`openai/gpt-oss-20b`) — all shared with spec 0014. No new secrets.
- **Migration:** `005_features.sql` (`session_sentiments`) runs at startup;
  shipped with the AI-bundle foundations.
- **Availability:** requires transcripts + billing + DB configured; otherwise
  both endpoints 503 and the client card never gets pricing.
- Rollout: server via `railway up` from `server/`; client autodeploys on push.

## 8. Risks / Open Items

- **Cache is forever:** an analysis run mid-session (or before a late flush
  straggler) freezes an early snapshot — there is no recompute or invalidation
  API. Acceptable today (the card lives on the post-call session screen);
  revisit if sessions ever become analyzable while live.
- `session_sentiments.user_id` cascades on account deletion, so the paying
  user deleting their account (GDPR) deletes the shared analysis for every
  participant. Consistent with the report/bookmark tables, but a shared
  artifact arguably shouldn't die with one member.
- Talk share uses character count as a proxy for talk time — verbose-but-brief
  speakers are over-weighted versus slow talkers. True durations would need
  per-utterance timing Deepgram already emits but the transcript doesn't store.
- The chart redraws only on `updateSentimentContext`/result changes, not on
  window resize — a resized viewport keeps the stale raster until re-entry.
- Key-moment labels are model-generated in the segment's language; mixed-
  language calls can yield mixed-language labels (no translation pass).

## 9. References

- Commits: `d5ce553` (formatting-only touch-ups since; behavior unchanged)
- Files: `server/src/ai/sentiment.rs`, `server/src/api.rs`,
  `server/src/config.rs`, `server/migrations/005_features.sql`,
  `server/tests/transcripts.rs`, `client/src/scripts/sentiment.ts`,
  `client/src/scripts/sentiment-chart.ts`, `client/src/scripts/api.ts`,
  `client/src/scripts/session-screen.ts`, `client/src/pages/index.astro`
- Sibling specs: [0014 AI session report](../0014-ai-session-report/spec.md),
  [0016 follow-up email](../0016-follow-up-email/spec.md),
  [0013 in-call bookmarks](../0013-call-bookmarks/spec.md),
  [0005 accounts & billing](../0005-accounts-credits-billing/spec.md),
  [0009 session transcripts](../0009-session-transcripts/spec.md)
- Groq chat API (JSON mode): https://console.groq.com/docs/text-chat

# 0068 — AI transcript correction on export

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | micio86dev |
| **Created** | 2026-06-15 |
| **Shipped** | 2026-06-15 |
| **Version** | — |
| **Commits** | `f3cb0fc` |
| **Depends on** | [0005](../0005-accounts-credits-billing/spec.md), [0009](../0009-session-transcripts/spec.md), [0014](../0014-ai-session-report/spec.md), [0015](../0015-sentiment-analysis/spec.md) |

## 1. Context & Problem

Transcripts are produced two utterances at a time: Deepgram STT hears each chunk in
isolation, and the live translator (Llama 3.1 8B Instant) renders each line with no
view of the surrounding conversation. The result is correct-enough for live captions
but rough on export — misheard homophones, missing punctuation/casing, names spelled
inconsistently, and translations that ignore terms established earlier in the call.

The Session Details screen lets a participant download the transcript as JSON, PDF, or
SRT/VTT. Users want an **opt-in, paid** "AI text correction" that cleans up the
exported text using the *whole dialogue* as context, with a clear up-front price and the
corrected result cached so re-exporting the same thing never charges twice.

The UI scaffold (checkbox + cost-estimate label, `.btn-ghost:disabled` styling, the
`transcript_correction` pricing field, `aiCorrectLabel` i18n) shipped in **#135**
(cleaned from the conflicting #134). This spec adds the functional backend + wiring.

## 2. Goals / Non-Goals

**Goals**
- Opt-in checkbox in the downloads area applies AI correction to the exported text.
- Correction uses a good Groq model (`GROQ_REPORT_MODEL`, default `llama-3.3-70b-versatile`)
  with full-dialogue context, degrading to the fallback model on model retirement.
- Cost is shown before paying, scales with transcript **size** (per event) and the
  selected export **mode**, and is charged atomically with the report/sentiment policy
  ("you weren't charged on failure").
- The corrected result is **cached in the DB** keyed by `(session, mode, lang)`; a repeat
  export of the same mode/lang charges nothing.
- Corrected text flows through the existing JSON/PDF/SRT/VTT renderers unchanged.

**Non-Goals**
- No real-time / in-call correction — this is export-time only.
- Does not mutate `transcript_events`; corrections live in a separate cache table and
  never alter the authoritative raw transcript or the live captions.
- No manual editing UI.
- Re-translation from scratch is out of scope: "translated" correction polishes the
  *existing* translation in context, it does not re-run translation for missing langs.

## 3. Requirements

- **R1 — Opt-in corrected export.** As a participant on Session Details, I want a
  checkbox that makes my download corrected, so the file I share reads cleanly.
  - *Given* a session with ≥1 event, *when* I tick "AI text correction" and click a
    download button, *then* the downloaded file's text is the corrected text.
  - *Given* `event_count == 0`, *then* the checkbox is disabled.
- **R2 — Price shown up front, by size + mode.** As a user, I want to see the cost
  before paying.
  - *Given* the screen is open, *then* the estimate = `base + per_event × event_count`,
    doubled when the selected mode is `both` (two text fields per event).
  - *Given* a correction already cached for the selected mode/lang, *then* the label
    shows it is free.
- **R3 — Charged once, cached.** As a user, I don't want to pay twice for the same thing.
  - *Given* I paid for `(session, both, it)`, *when* I export the same mode/lang again,
    *then* nothing is charged and the same corrected text is served.
  - *Given* I then export a *different* mode/lang, *then* that is a separate charge.
- **R4 — User-favorable failure policy.** As a user, I am never charged for our failures.
  - *Given* Groq fails, *then* 502 and nothing is charged.
  - *Given* my balance is below cost, *then* 402 `insufficient_credits` and no correction.
  - *Given* the deduct fails after a successful generation for any other reason, *then*
    the correction is delivered free and the error is logged.
- **R5 — Access-gated.** Only session participants can request/download corrections
  (404 unknown / 403 stranger), same gate as the raw transcript.

## 4. Design & Architecture

- **Components / files**
  - `server/src/ai/correction.rs` *(new)* — cost fn, prompt, batched JSON-mode
    generation with whole-dialogue context, `apply_correction`, DB save/get. Mirrors
    `ai/report.rs` + `ai/sentiment.rs`.
  - `server/src/api.rs` — `correction_generate` (POST) + `correction_status` (GET);
    `transcript_correction` added to `ai_pricing`; the four download handlers gain a
    `corrected` flag that overlays the cached correction onto the export before rendering.
  - `server/src/config.rs` — `correction_base`, `correction_per_event` on `AiConfig`.
  - `server/migrations/008_corrections.sql` *(new)* — `session_corrections` table.
  - `client/src/scripts/api.ts` — `ensureCorrection()` / `fetchCorrectionStatus()`;
    `transcript_correction` already on `AiPricing` (#135).
  - `client/src/scripts/session-screen.ts` — wire checkbox → POST correction → corrected
    download; mode-aware estimate; "free when cached".
  - `client/src/scripts/auth.ts` — `downloadTranscript(..., corrected)`.

- **Data model** — `session_corrections`:
  `id, session_id, mode TEXT CHECK (mode IN ('original','translated','both')),
  lang TEXT NOT NULL DEFAULT '' (empty for mode=original), result_json JSONB,
  model TEXT, cost DECIMAL(10,6), user_id, created_at`,
  `UNIQUE (session_id, mode, lang)` — the cache contract (one paid result per key).
  `result_json` is `[{ "i": <event index>, "original"?: "…", "translated"?: "…" }]`;
  only the fields the mode covers are stored.

- **Protocol / API**
  - `POST /api/sessions/{id}/correction?mode=&lang=` → ensures a cached correction
    exists (generate+charge if missing, free if cached). Returns
    `{ cached, cost, charged, balance?, event_count, mode, lang, model }`. Does **not**
    return the corrected text — the download endpoints render it.
  - `GET  /api/sessions/{id}/correction?mode=&lang=` → `{ cached, cost?, mode, lang }`
    so the client can label the export "free" when already paid.
  - Download endpoints accept `&corrected=1`. The handler derives `(mode, lang)` from the
    format (see below), loads the cached row, and `apply_correction`s it onto the export
    before the unchanged renderer runs. Missing cache with `corrected=1` → 409 (the client
    always POSTs first, so this is a safety net).
  - `GET /api/billing/ai-pricing` gains `"transcript_correction": { "base", "per_event" }`.

- **Mode per format** (what the renderer shows ⇒ what we correct):
  - `json` → `mode=original` (correct source text only; JSON carries every language —
    we polish the authoritative originals, not N translations).
  - `pdf` → `mode=both`, `lang` = PDF lang (original + the shown translation).
  - `srt`/`vtt` → the lang-mode dropdown (`original`/`translated`/`both`), `lang` = target.

- **Cost** — `correction_cost(ai, mode, event_count) = base + per_event × event_count × passes`,
  `passes = if both { 2 } else { 1 }`. Defaults `base=0.05`, `per_event=0.001` (match the
  #135 client fallback). Rounded to 6 dp like the other features.

- **Generation** (`generate_correction`)
  1. Build a **whole-dialogue context** string via the existing `condense_transcript`
     (cheap fallback model map-reduce for long calls; passthrough when short) — this is
     what gives the model "context of the entire dialogue" without re-sending 100k chars
     in every batch.
  2. Correct in **batches** of `CORRECTION_BATCH` events. Each batch is a JSON-mode call
     on the good model: system = task + the context summary + the participant-name
     glossary; user = `{"lines":[{"i","speaker","lang","text"}]}`; expected reply
     `{"lines":[{"i","text"}]}`. Results align by `i`; any missing/empty correction falls
     back to the original string (never lose a line). First-batch 4xx → whole run retries
     on the fallback model (mirrors report/sentiment).
  3. `mode=original` corrects the `original` field; `translated` corrects
     `translations[lang]`; `both` does both passes. A line with no translation for `lang`
     is left as-is in the translated pass.

- **Sequence (happy path, corrected SRT, mode=both, it)**
  1. Client: `POST …/correction?mode=both&lang=it` → cache miss → generate → charge →
     `201 {charged:true, cost, balance}`.
  2. Client: `GET …/transcript.srt?lang=both&target=it&corrected=1` → handler loads the
     cached row, overlays corrected `original`+`translations["it"]`, renders SRT.
  3. Re-export later → step 1 is a cache hit (`200 {cached:true, charged:false}`), step 2
     identical.

- **Key decisions**
  - *Cache by `(session, mode, lang)`, separate table* → matches the user's "cost & cache
    depend on the chosen mode" decision; reuses the proven sentiment UNIQUE-cache contract.
  - *Overlay onto `TranscriptExport`, render unchanged* → zero changes to pdf/subtitle/json
    renderers; correction is a pure pre-render transform.
  - *Context via `condense_transcript`, correct in batches* → whole-dialogue awareness with
    bounded tokens; per-line JSON alignment keeps every event mapped 1:1.
  - *Two-step (POST then download)* rather than correcting inside the download handler →
    keeps billing/latency off the file response and lets the client show a "Correcting…"
    state; a corrected download is a fast cache read.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `session_corrections` migration | `server/migrations/008_corrections.sql` |
| S1 | `correction_base`/`per_event` on `AiConfig` + `ai_pricing` field | `server/src/config.rs`, `server/src/api.rs` |
| S2 | `ai/correction.rs`: cost, prompt, batched generation, `apply_correction`, save/get | `server/src/ai/correction.rs`, `server/src/ai/mod.rs` |
| S3 | `correction_generate` (POST) + `correction_status` (GET) + route | `server/src/api.rs`, `server/src/lib.rs` |
| S4 | `corrected=1` overlay in the 4 download handlers | `server/src/api.rs` |
| S5 | Client: `ensureCorrection`, corrected download, mode-aware estimate, free-when-cached | `client/src/scripts/api.ts`, `session-screen.ts`, `auth.ts` |

## 6. Testing & Verification

- `correction_cost`: base + per_event × events; `both` doubles; floors at 1 event (R2).
- `apply_correction`: overlays `original`/`translations[lang]` by index; extra/short cache
  arrays don't panic, untouched langs preserved.
- Prompt builder: includes the context summary + name glossary; JSON-mode shape.
- Batch alignment: a missing `i` in the reply falls back to the original line.
- Handler (where DB-backed test harness exists): cache miss charges once and caches;
  second call is free (R3); Groq failure → 502 uncharged; low balance → 402 (R4);
  stranger → 403 (R5).
- Client typecheck (`astro check`) + existing vitest green.

## 7. Deployment & Operations

- **Migration** `008_corrections.sql` runs on server boot (`sqlx::migrate!`). Additive,
  idempotent (`CREATE TABLE IF NOT EXISTS`). Railway server deploy is **manual**
  (`railway up` from `server/`); Vercel client auto-deploys on `main`.
- **Env (all optional, sane defaults):** `CREDITS_CORRECTION_BASE` (0.05),
  `CREDITS_CORRECTION_PER_EVENT` (0.001). Model reuses `GROQ_REPORT_MODEL` /
  `GROQ_FALLBACK_MODEL`.
- No feature flag — the checkbox is the opt-in; with billing disabled the endpoint 503s
  like the other AI features.

## 8. Risks / Open Items

- **Index alignment** assumes the export's event order is stable between the POST and the
  corrected download. True for ended sessions (deterministic `ORDER BY ts` + `flush()`
  barrier); the overlay applies only to overlapping indices, so a mismatch degrades to
  partial correction rather than a wrong mapping.
- **Long calls** rely on `condense_transcript` for context; very large transcripts cost
  more model calls (batched) — bounded by per-event pricing.
- **Headline estimate** tracks the lang-mode dropdown; PDF (`both`) and JSON (`original`)
  use their own fixed mode, so the authoritative cost is the value returned by the POST.

## 9. References

- Supersedes the inert scaffold from #135 (cleaned from #134).
- Files: `server/src/ai/report.rs`, `server/src/ai/sentiment.rs` (patterns mirrored),
  `server/src/api.rs` (download handlers, billing flow).
- External: Groq chat-completions (`server/src/groq.rs`).

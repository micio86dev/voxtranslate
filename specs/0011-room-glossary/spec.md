# 0011 — Room glossary: enforced terminology in translations

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | micio86dev |
| **Created** | 2026-06-11 *(retroactive backfill)* |
| **Shipped** | 2026-06-10 |
| **Version** | post-v1.0.0 AI bundle |
| **Commits** | `18d20f8` |
| **Depends on** | [0001](../0001-voice-translation-rooms/spec.md) |

*Authored retroactively on 2026-06-11 from the shipped code and commit history (backfill of the AI-bundle specs).*

## 1. Context & Problem

The Groq translation model renders fluently but paraphrases freely: "fattura" becomes
"bill" in one sentence and "invoice" in the next, product names get
"helpfully" translated, and legal/sales jargon drifts per utterance. For a
recurring meeting that is worse than a one-off mistake — the same term must
come out the same way every time, in every direction the room uses.

Rooms are ephemeral TEXT codes (there is no rooms table), but teams reuse the
same code for their standing meetings. That makes the room code a natural key
for a persistent terminology list: define "fattura → invoice" once, and every
future call on that code translates it exactly that way — in speech subtitles
and in chat — without any per-utterance latency cost.

## 2. Goals / Non-Goals

**Goals**
- Per-room term pairs (`source_lang/target_lang/source_term/target_term`)
  injected into the Groq translation prompt for **both** translation paths:
  speech finals (Deepgram pipeline) and chat fan-out.
- Zero added latency on the hot path: term resolution per utterance is a
  synchronous in-memory cache read — never a DB query.
- Mid-call edits apply to the very next sentence (no rejoin, no restart).
- Bulk entry via CSV import (server-parsed, per-line errors), merging
  last-wins over saved entries.
- Everyone in the room can *see* a glossary is active (in-call badge),
  including guests; editing is auth-only.

**Non-Goals**
- No hard enforcement or post-hoc verification that the model obeyed — the
  terminology is a prompt-level instruction ("MANDATORY TERMINOLOGY").
- No per-user or per-org glossaries; exactly one per room code.
- No ownership ACL: any signed-in user who knows the room code may edit
  (whoever has the code is in the meeting — same trust model as the room).
- No morphological/fuzzy matching of inflected forms; the model is told
  "any capitalization" and left to apply it.
- Interim subtitles untouched (only finals are translated anyway, spec 0002).

## 3. Requirements

- **R1 — Enforced terms in speech translation.** As a room with agreed
  terminology, I want every spoken final translated with my terms, so
  subtitles use the room's exact vocabulary.
  - *Given* an entry `it→en fattura→invoice`, *when* an Italian speaker's
    final is translated to English, *then* the system prompt carries a
    `MANDATORY TERMINOLOGY` block with `"fattura" -> "invoice"`.
  - *Given* entries for other directions (`en→it`, `it→fr`), *then* only the
    pairs matching the utterance's exact (source, target) direction are
    injected, capped at 50 per direction (`MAX_INJECTED`).
  - *Given* no matching terms, *then* the prompt has no glossary block at all.
- **R2 — Chat honors the glossary too.** *Given* the same entry, *when* a chat
  message is fanned out (`handle_chat`), *then* the same cached snapshot and
  the same per-direction filter apply.
- **R3 — Live edits.** As an editor, I change terms mid-call and they apply
  immediately.
  - *Given* a save/import/delete during a call, *then* the cache is refreshed
    from the DB before the handler responds, and the next utterance uses the
    new snapshot (`GlossaryService::cached` is read per utterance).
- **R4 — CRUD over REST, auth-gated and validated.**
  - *Given* no bearer token, *then* every glossary verb returns 401; *given* a
    guest-only deployment (no database), *then* 503 "auth/billing not
    configured".
  - *Given* a save payload, *then* entries are trimmed, language codes
    lowercased, duplicates of `(source_lang, target_lang, source_term)`
    deduped **last-wins**, and the result capped at `GLOSSARY_MAX_ENTRIES`
    (default 200) — over-cap, empty/oversize (>200 chars) terms, missing or
    equal language codes are 400s naming the 1-based offending entry.
  - *Given* a glossary name, *then* it is trimmed, empty → `NULL`,
    >100 chars → 400. Room codes are trimmed and must be 1–64 chars.
  - *Given* a DELETE, *then* 204 idempotently, and the cache holds an empty
    snapshot (replace, never remove).
- **R5 — CSV import.** As an editor with an existing termbase, I paste CSV
  instead of typing rows.
  - *Given* `source_lang,target_lang,source_term,target_term` lines (optional
    header, blank lines skipped, double-quoted fields with `""` escapes so
    terms may contain commas), *then* rows are parsed server-side; a wrong
    field count fails with the 1-based line number.
  - *Given* saved entries, *then* the import merges over them — imported rows
    override same-key entries (existing-first + last-wins dedupe) and the
    glossary name is kept. An empty CSV is a 400 ("no entries in CSV").
- **R6 — Visibility badge.** As any participant, I can tell the room enforces
  terminology.
  - *Given* a room with entries, *when* I join, *then* I receive
    `glossary_active {name?, entries}` right after `room_joined` and the
    in-call header badge shows `📖 Name (N)`.
  - *Given* any CRUD mutation, *then* `glossary_active` is re-broadcast to the
    whole room; `entries == 0` (after delete) hides the badge.
  - *Given* I am a guest, *then* I see the badge but it is disabled (the
    editor is auth-only); the home-screen 📖 entry point is hidden entirely
    when logged out.

## 4. Design & Architecture

- **Components / files:**
  - `server/src/glossary.rs` — `GlossaryService` (sqlx persistence + `DashMap`
    cache of `Arc<RoomGlossary>` snapshots keyed by room code),
    `RoomGlossary::terms_for` (direction filter + `MAX_INJECTED` cap),
    `normalize_entries` (trim/lowercase/last-wins dedupe/cap-after-dedupe),
    `import_csv` + minimal quote-aware line splitter.
  - `server/src/groq.rs` — `translate(text, source, target, terms)`;
    `translation_prompt` appends the `MANDATORY TERMINOLOGY` block only when
    terms exist.
  - `server/src/translator.rs` — `translate_fanout(…, glossary:
    Option<&RoomGlossary>)` resolves `terms_for(src, tgt)` per parallel
    direction.
  - `server/src/deepgram.rs` — `SpeakerCtx.glossary: Option<GlossaryService>`;
    the finals loop snapshots `glossary.cached(&room)` per utterance.
  - `server/src/lib.rs` — `AppState.glossary` (built in `init` only when the
    database is configured), cache warm + badge send at room join,
    `handle_chat` snapshot, REST routes.
  - `server/src/protocol.rs` — `ServerMessage::GlossaryActive { name?, entries }`
    (`name` omitted from JSON when `None`).
  - `server/src/api.rs` — `glossary_get/save/delete/import` handlers +
    `broadcast_glossary`.
  - `client/src/scripts/glossary.ts` — editor modal (term table, CSV import,
    two-click delete) + badge logic; `api.ts` REST wrappers (`fetchGlossary`,
    `saveGlossary`, `importGlossaryCsv`, `deleteGlossary`); `app.ts` wires
    `initGlossary({ show })` (focus trap), `setGlossaryRoom` on join/leave,
    `onGlossaryActive` on the WS frame; modal markup + scoped CSS in
    `pages/index.astro`; `book` icon in `icons.ts`; 22 `glossary*` i18n keys
    × 8 languages in `i18n.ts`.
- **Data model** (`migrations/005_features.sql`):
  - `room_glossaries` — `room TEXT PRIMARY KEY`, `name`, `created_by → users`
    (cascade), timestamps.
  - `glossary_entries` — `id UUID`, `room → room_glossaries` (cascade),
    `source_lang`, `target_lang`, `source_term`/`target_term VARCHAR(200)`,
    `UNIQUE (room, source_lang, target_lang, source_term)`, index on
    `(room, source_lang)`. Canonical (and editor) order is alphabetical —
    batch-inserted rows share a `created_at`, so it cannot order them.
- **Protocol / API:**
  - `GET /api/rooms/{room}/glossary` → `{ name, entries[{id,…}], max_entries }`
    (empty glossary for fresh rooms).
  - `POST /api/rooms/{room}/glossary` `{name?, entries[]}` → full replace,
    200 with the saved (normalized, re-ordered) glossary · 400 validation.
  - `DELETE /api/rooms/{room}/glossary` → 204 (idempotent).
  - `POST /api/rooms/{room}/glossary/import` `{csv}` → merge + save, same
    response shape · 400 parse/validation.
  - All four: `AuthUser`-gated (401), 503 without a database. WS (server →
    client only; edits never travel over the room socket):
    `glossary_active {name?, entries}`.
- **Sequence (happy path):** editor saves → `normalize_entries` →
  `GlossaryService::save` (one transaction: upsert header, delete + reinsert
  entries) → `reload` refreshes the cache snapshot → `broadcast_glossary`
  updates every badge → a speaker finishes a sentence → finals task reads
  `cached(room)` synchronously → `translate_fanout` filters `terms_for(src,
  tgt)` per target language → `translation_prompt` appends the MANDATORY
  TERMINOLOGY block → subtitle/chat broadcast carries conforming translations.
- **Key decisions:**
  - *Cache keyed by room code, populated at join, read synchronously per
    utterance* → the translation hot path never awaits the DB. A room with no
    glossary caches an **empty** snapshot, so the lookup is uniform.
    Alternative (query per utterance) rejected: per-sentence latency on the
    real-time path.
  - *Replace, never remove, in the cache* — delete inserts an empty snapshot
    instead of evicting the key, so concurrent readers can never re-trigger a
    load or see a stale Some.
  - *Save is a full replace in one transaction* — the editor always submits
    the whole table, which kills diffing complexity and makes last-wins
    semantics trivial. Import reuses it by prepending existing entries before
    the dedupe.
  - *Prompt-level enforcement, capped at `MAX_INJECTED = 50` per direction* —
    keeps the system prompt bounded for latency; the stored cap
    (`GLOSSARY_MAX_ENTRIES`, default 200) spans all directions.
  - *Any signed-in user may edit* — rooms have no owner (spec 0001), the code
    itself is the credential; an ACL would have required inventing room
    ownership for this feature alone.
  - *CSV parsed server-side* with a deliberately minimal splitter (quotes +
    `""` escapes only) → one validation path (`normalize_entries`) for both
    JSON saves and imports, and precise per-line 400s.
  - *Client normalizes the room code* (`trim().toLowerCase()`) exactly like
    the join flow, so home-screen edits before joining hit the same key.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Schema: glossary tables + index | `server/migrations/005_features.sql` |
| S1 | Domain + service: normalize, CSV parse, DashMap cache, save/delete/reload | `server/src/glossary.rs` |
| S2 | Prompt injection + fan-out plumbing | `server/src/groq.rs`, `server/src/translator.rs` |
| S3 | Both translation paths: speech finals + chat | `server/src/deepgram.rs` (`SpeakerCtx`), `server/src/lib.rs` (`handle_chat`, join warm-up) |
| S4 | REST CRUD + import + broadcast, routes, config cap | `server/src/api.rs`, `server/src/lib.rs`, `server/src/config.rs` (`GLOSSARY_MAX_ENTRIES`) |
| S5 | WS badge message | `server/src/protocol.rs` (`GlossaryActive`) |
| S6 | Client: editor modal, badge, API wrappers, i18n ×8 | `client/src/scripts/glossary.ts`, `api.ts`, `app.ts`, `pages/index.astro`, `i18n.ts`, `icons.ts` |

## 6. Testing & Verification

- **Unit (server, `glossary.rs`, 8 tests):** `terms_for` filters by exact
  direction and caps at `MAX_INJECTED` (R1); `normalize_entries` trims,
  lowercases, dedupes last-wins, rejects bad entries with 1-based indices,
  caps **after** dedupe (R4); CSV parsing — header/blank-line skip, quoted
  commas + `""` escapes, wrong-field-count line numbers (too few *and* too
  many), empty input (R5).
- **Unit (server, `groq.rs`):**
  `translation_prompt_glossary_block_only_when_terms_exist` pins the exact
  `"term" -> "translation"` block format and its absence without terms (R1).
  `translator.rs` fan-out unit covers the `None`-glossary path.
- **Integration (`server/tests/glossary.rs`, DB-gated — skips without
  `DATABASE_URL`):**
  - `glossary_rest_crud_validation_and_import` — 401 on every verb, empty
    glossary + advertised cap for fresh rooms, save normalization
    (trim/lowercase/last-wins) and alphabetical order, over-cap and
    bad-entry 400s, CSV merge overriding same-key rows while keeping the
    name, import 400s (line number, empty file), idempotent delete (R4, R5).
  - `glossary_badge_on_join_and_live_updates` — badge after `room_joined` for
    an authed joiner *and* a guest, re-broadcast to everyone on live edit,
    `entries: 0` with `name` absent after delete (R6).
- R1/R2 end-to-end (the model actually obeying the block) is verified
  manually — it depends on live Groq output, which is not pinned in CI.
- **Gates at ship (commit `18d20f8`):** server llvm-cov and clippy clean,
  `astro check` 0 errors, e2e suite green; the editor itself is auth-gated so
  the guest-mode e2e backend exercises only the badge path.

## 7. Deployment & Operations

- **Env:** `GLOSSARY_MAX_ENTRIES` (default `200`) — stored-entries cap,
  advertised to the editor as `max_entries`. No new secrets.
- **Requires the database:** `GlossaryService` is constructed only when
  billing/auth is configured (`DATABASE_URL` present). In guest-only mode the
  REST endpoints 503, no badge is ever sent, and translation proceeds
  glossary-less — fully degraded, never broken.
- **Migration:** `005_features.sql` (shared with bookmarks/reports) runs at
  startup; `IF NOT EXISTS` everywhere, no backfill needed.
- Rollout: server via `railway up` from `server/`; client autodeploys on push.
  No feature flag — an empty glossary is a no-op on every path.

## 8. Risks / Open Items

- **Soft enforcement.** The terminology is a prompt instruction; an 8B model
  can occasionally miss a term (especially inflected forms — matching is
  delegated to the model "any capitalization", with no morphology and no
  post-hoc verification).
- **Silent truncation at `MAX_INJECTED`.** A direction with >50 pairs is
  quietly capped per utterance; the editor shows only the stored cap (200).
  No warning surfaces to the user.
- **`auto` source bypasses the glossary** (interaction with spec 0012, which
  shipped after this): a chat message sent before language detection resolves
  is translated with `source = "auto"`, and `terms_for("auto", …)` matches
  nothing. Speech finals are unaffected (detection resolves before streaming
  starts).
- **Room-code keyed persistence cuts both ways:** a standing meeting inherits
  its glossary for free, but an unrelated group reusing the same code inherits
  it too — and any signed-in user with the code can edit or delete it.
- **Per-process cache.** Mutations refresh only the instance that served them;
  a multi-instance deployment would serve stale snapshots elsewhere. Fine on
  today's single-instance Railway; revisit before horizontal scaling.
- Post-ship drift: none behavioral. `translation_prompt` later gained the
  `auto`-source clause (spec 0012, `a594e94`) and `lang_name` went `pub` for
  the AI report (`c2bc646`); glossary code itself saw only `cargo fmt`
  reflows. This spec describes the current code.

## 9. References

- Commits: `18d20f8`
- Files: `server/src/glossary.rs`, `server/src/groq.rs`,
  `server/src/translator.rs`, `server/src/deepgram.rs`, `server/src/lib.rs`,
  `server/src/protocol.rs`, `server/src/api.rs`,
  `server/migrations/005_features.sql`, `server/tests/glossary.rs`,
  `client/src/scripts/glossary.ts`, `client/src/scripts/api.ts`,
  `client/src/scripts/app.ts`, `client/src/pages/index.astro`
- Sibling specs: [0001](../0001-voice-translation-rooms/spec.md) (rooms),
  [0002](../0002-video-calls-translated-chat/spec.md) (chat fan-out),
  [0016](../0016-follow-up-email/spec.md) (AI-bundle spec style precedent)

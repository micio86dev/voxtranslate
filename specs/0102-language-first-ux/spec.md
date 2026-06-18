# 0102 — Language-first UX, full language union & i18n

| | |
|---|---|
| **Status** | In progress |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-18 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | — |
| **Depends on** | [0093](../0093-premium-translation-engine/spec.md), [0099](../0099-premium-listener-pays/spec.md), [0100](../0100-pro-gemini-live-translate/spec.md), [0101](../0101-soniox-enhanced-tier/spec.md) |

## 1. Context & Problem

The home screen shows a **target-language `<select>` (8 languages) and the tier/engine
selector side-by-side**. Two problems:

1. **Only 8 target languages.** Every engine's `output_languages` is hardcoded to the same
   8 (`it,en,es,fr,de,pt,ja,zh`) and the picker offers the cross-engine *intersection*
   (`commonLangs`, spec 0094). But the providers support far more — Soniox 60+, OpenAI 13,
   Gemini 70+. We surface a fraction of what we can actually translate.
2. **Tier-first is the wrong order.** Under **listener-pays** (spec 0099, live in prod) each
   *listener* independently picks the quality THEY receive, so the cross-engine intersection
   is moot — there is no single shared language set. The natural flow is **language-first**:
   pick the target language, then choose among the tiers that can output it.

This spec flips the model to **language-first**, expands coverage to the full **union** of
every engine's languages, filters tiers per language, and internationalizes the **entire UI**
into all union languages (with RTL).

**Decisive finding:** the backend already tolerates any language. `SetLang`
(`lib.rs`) accepts any `[a-z0-9-]{1,8}` code; the chosen target flows through as a raw string
to every engine (`groq::lang_name`, `openai::session_update_json`, `gemini::setup_json`,
`soniox.ts` client-direct). `output_languages` is **UI metadata only**. So expanding coverage
is a metadata + UI + prompt-quality change — **no provider-call, pricing, billing or markup
changes**.

## 2. Goals / Non-Goals

**Goals**
- A **language-first picker**: pick target language (full union, region-grouped, searchable,
  flag + native + English name, browser auto-detect) → see only the tiers that output it,
  cheapest pre-selected, with rate + estimated minutes and a low-credit top-up nudge.
- **Maximum language coverage**: a single `server/src/engine/languages.json` source of truth
  (per-language metadata + per-tier output lists), consumed by **both** the Rust backend and
  the TS frontend.
- **Full UI i18n** in every union language, high quality, with **RTL** (`ar he fa ur …`).
- Ship behind a flag (`LANGUAGE_FIRST_UX`, default OFF), verified in prod before flip-on.

**Non-Goals**
- No change to provider API integrations (how we call Deepgram/Soniox/OpenAI/Gemini).
- No change to pricing/cost/markup/billing/the listener meter (rates stay env-derived,
  surfaced only as `rate_per_minute`).
- No change to the legacy (flag-off / speaker-pays) picker behaviour. Standard keeps its
  proven 8 languages so the legacy `commonLangs` intersection does **not** shrink.

## 3. Requirements

- **R1 — Language-first.** *Given* the flag is on, *when* I open the picker, *then* I pick a
  target language from the full union (region-grouped, searchable), pre-selected to my
  browser language.
- **R2 — Tier filtering.** *Given* I pick a language, *then* I see only the enabled tiers
  whose `output_languages` include it, sorted cheapest-first, with the cheapest pre-selected.
- **R3 — Single-tier note.** *Given* exactly one tier supports the language, *then* it is
  auto-selected and a note explains why it's the only option.
- **R4 — Coverage.** *Given* any union language, *then* at least Premium (the universal
  fallback) can output it; the server translates to it with no provider-call change.
- **R5 — Full i18n + RTL.** *Given* I pick a language with a UI translation, *then* the whole
  UI renders in it; for an RTL language `document.documentElement.dir === 'rtl'`.
- **R6 — Flag-gated.** *Given* `LANGUAGE_FIRST_UX` is unset, *then* the legacy side-by-side
  picker renders unchanged and `commonLangs` still offers the current 8.
- **R7 — No billing change.** *Given* any selection, *then* rates/credits/metering behave
  exactly as before; only `rate_per_minute` is shown.

## 4. Design & Architecture

- **Shared source of truth — `server/src/engine/languages.json`** (NEW; lives under `server/`
  so it's inside the server's Railway/Docker build context, which only uploads `server/`):
  - `languages[]`: `{ code, native, english, region, rtl, flag }` — the union universe.
  - `regions[]`: `europe, asia, mena, subsaharan, americas` (picker grouping order).
  - `tiers{}`: per-tier output-language code lists. `premium` = the full union (universal
    fallback); every tier list is a **superset of the legacy 8** so `commonLangs` is stable.
  - Consumed by Rust via `include_str!` + serde (`server/src/engine/langmap.rs`) and by TS via
    a JSON import (`client/src/scripts/langmap.ts`).
- **Backend:** the four `*_LANGS` consts in `engine/{standard,pro,premium,soniox}.rs` are
  replaced by `langmap::tier_output_langs(tier)`. `groq::lang_name` gains English names for
  all union languages (prompt quality only). No DTO change — `EngineInfo.output_languages`
  carries the per-tier lists to the client as today.
- **Flag:** `Config.language_first_ux` (`env_flag("LANGUAGE_FIRST_UX")`), exposed to the
  client by wrapping `GET /api/engines` as `{ engines, flags: { language_first_ux } }`
  (guest-safe — `/api/auth/config` 503s for guests; the new picker must work for guests too).
- **Frontend:** `engines.ts` gains `allLanguages()` (union, region-grouped),
  `getAvailableTiers(lang, engines)` (filter by `output_languages`, sort by rate) and
  `cheapestTier()`; `commonLangs` stays for the legacy path. `app.ts` branches in
  `initEngines`: flag on → `renderLanguageFirstPicker()`, else legacy
  `renderEngineSelector()`/`rebuildLangOptions()`. The picker reuses the `.engine-opt*` DOM
  for the tier cards. `index.astro` keeps the legacy `<select id="lang">` and adds the
  language-picker container.
- **i18n:** locale dicts split into `client/src/scripts/i18n/<code>.ts`; `i18n.ts` assembles
  `I18N`, so `t()`/`applyI18n()`/`setUiLang()` and the `/api/content/i18n` DB override are
  unchanged. `SUPPORTED`/`ENDONYM`/`FLAG` are derived from the shared map. RTL via `isRtl()` +
  `document.documentElement.dir` in `setUiLang`/`applyI18n`, plus a CSS logical-property audit.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Spec | this file |
| S1 | Shared map + `langmap` (Rust+TS) + replace `*_LANGS` + `lang_name` | `server/src/engine/languages.json`, `engine/langmap.rs`, `engine/{standard,pro,premium,soniox}.rs`, `groq.rs`, `engine/mod.rs`, `client/src/scripts/langmap.ts` |
| S2 | `LANGUAGE_FIRST_UX` flag + `/api/engines` `{engines,flags}` | `config.rs`, `api.rs`, fixtures (`config.rs`, `tests/integration.rs`, `tests/billing.rs`), `engines.ts`, `app.ts` |
| S3 | `allLanguages`/`getAvailableTiers`/`cheapestTier` | `engines.ts` |
| S4 | Language-first picker + tier cards | `app.ts`, `index.astro` |
| S5 | Full i18n locale files + RTL + map-driven `SUPPORTED/ENDONYM/FLAG` | `i18n.ts`, `i18n/<code>.ts` |
| S6 | Tests (Rust unit, vitest, Playwright) | `engine/langmap.rs`, `engines.test.ts`, `i18n.test.ts`, `e2e/language-first.spec.ts` |

## 6. Testing & Verification

- **Server:** `cargo fmt --check` + `cargo clippy --all-targets` + `cargo test`. New: `langmap`
  parses; every tier list ⊆ `languages`; `premium` = union; legacy-8 ⊆ every tier (commonLangs
  guard). Flag default test. Config fixtures updated.
- **Client:** `astro check` (0 errors); vitest for `getAvailableTiers`/`allLanguages`/
  `cheapestTier`, every locale's key set === `en`'s, `isRtl`, `detectLang` for a new locale.
- **E2E:** `language-first.spec.ts` — flag on → pick language → only supporting tiers, cheapest
  pre-selected → join, zero console errors; RTL → `dir==='rtl'` + screenshot-diff; flag off →
  legacy regression guard. `a11y.spec.ts` extended (axe + keyboard nav of the search list).

## 7. Deployment & Operations

- **Env:** `LANGUAGE_FIRST_UX=1` to enable. No new keys/pricing.
- **Rollout:** deploy with the flag OFF (no behaviour change; `/api/engines` already serves the
  expanded `output_languages`), confirm prod green, then flip `LANGUAGE_FIRST_UX=1`.
- **Rollback:** unset `LANGUAGE_FIRST_UX` → legacy picker returns; everything else unchanged.

## 8. Risks / Open Items

- Provider language lists are assumptions until verified against official docs (Soniox,
  OpenAI realtime translation, Gemini Live Translate) — the JSON localizes that risk to one
  file; correct lists there without code changes.
- UI translation quality for long-tail languages — gated by the per-locale same-key-set test;
  the `/api/content/i18n` DB override is the no-redeploy fix path.
- Flag emoji for non-country languages (Arabic, Swahili…) — explicit `flag` field, never
  guessed, to avoid wrong-flag sensitivity.
- Standard kept at 8 (not the conservative 6) so the flag-off `commonLangs` intersection does
  not regress; revisit shrinking it once the legacy path is retired.

## 9. References

- Soniox STT+translation languages; OpenAI realtime translation output languages; Gemini Live
  Translate `targetLanguageCode`.
- Internal: 0093 (engine registry), 0094 (`commonLangs`), 0099 (listener-pays), 0100 (Pro/
  Premium), 0101 (Enhanced/Soniox client-direct).

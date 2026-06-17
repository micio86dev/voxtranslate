# 0097 — Usage analytics: Standard vs Premium

| | |
|---|---|
| **Status** | Pipeline complete (events + roll-up + dashboards script); deploy + run script to go live |
| **Owner** | micio86dev |
| **Created** | 2026-06-17 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | _(pin on merge)_ |
| **Depends on** | 0093 (engine registry → plan tier), 004 (call_sessions), 0096 (RLS) |

## 1. Context & Problem

Issue #241 asks for a production-grade, event-sourced analytics system to compare
Standard vs Premium usage, fast for Directus dashboards, append-only, GDPR-safe,
without breaking the existing schema.

## 2. Reconciliation with the existing schema (important)

The issue's proposed `sessions(user_id, started_at, ended_at, duration, status)`
**duplicates** what already exists:
- `usage_sessions` — per billed user per call: cost, started/ended, engine.
- `call_sessions` — the room-lifetime call (id, room, started/ended).

Per the issue's own constraint ("do NOT break existing schema, prefer extending"),
**we do not add a `sessions` table.** Instead:
- `session_usage_events.session_id` → references **`call_sessions(id)`**.
- `plan` = the engine **tier** (`standard`|`premium`), already chosen per
  connection (spec 0093/0094); `analytics::plan_for_tier` derives it.

## 3. Design — two layers

**Layer 1 — raw events (source of truth, append-only).** `session_usage_events`
(migration 012): session_id, user_id (NULL for guests), plan, event_type, feature,
mode, duration_seconds, cost_cents, language_from/to, metadata jsonb, created_at —
indexed on session/user/plan/type/feature/time. NEVER updated.

**Layer 2 — aggregates (read-optimized).** `plan_usage_daily` (unique per day+plan)
and `user_usage_stats` (per user). Directus reads ONLY these — never the raw table.

**Ingestion (`server/src/analytics.rs`).** Fire-and-forget: `record_event` spawns
the insert and logs+drops any error, so analytics **never blocks or fails the call**
(issue rule). `UsageEvent` builder + `plan_for_tier` are unit-tested; `insert_event`
is the awaitable core.

## 4. What landed in this change

- **Migration 012** — `session_usage_events` + `plan_usage_daily` +
  `user_usage_stats`, all with RLS enabled (consistent with 0096). Auto-applies via
  `sqlx::migrate!` on deploy.
- **`analytics` module** — `UsageEvent`, `record_event` (non-blocking),
  `insert_event`, `plan_for_tier`. Unit-tested; `--test billing` green post-012.
- **One proof ingestion point** — a `session_started` event emitted (non-blocking)
  when a billed session is created in `handle_peer`.

## 5. What landed (full pipeline)

- **Event emission (server-observable, non-invasive):** `session_started` (on
  join), `session_ended` (on teardown, carrying the connection duration — the core
  "minutes per plan" KPI), `screen_shared` (on the share signal). All carry the
  plan (engine tier) and are fire-and-forget.
- **Roll-up loop:** `analytics::run_rollup` is spawned once at startup (every 5 min)
  and folds raw events into `plan_usage_daily` + `user_usage_stats` via idempotent
  upserts (full recompute). Integration test (`analytics_rollup_aggregates_session_events`)
  verifies a premium 180 s event → 3 premium minutes in `user_usage_stats`.
- **Directus dashboards:** `directus/setup-analytics-dashboards.mjs` registers the
  aggregate collections + builds the 5 dashboards (Plan Distribution, Revenue,
  Usage Trends, Feature Breakdown, User Intelligence). Same pattern + auth as
  `setup-backoffice.mjs`; idempotent.

## 6. To go live + remaining nice-to-haves

1. **Deploy** the branch → sqlx applies migration 012 (creates the RLS-enabled
   tables); event emission + roll-up start automatically.
2. **Run** `node directus/setup-analytics-dashboards.mjs` with the Directus admin
   env (the script needs the tables to exist first).
3. *Nice-to-have follow-ups:* `translation_used` per-utterance (needs threading the
   engine tier into the speech path / `SpeakerCtx`); client-reported feature events
   (`subtitles_on`, `voice_generated`, `recording_started`) via a small analytics
   ping; per-event `cost_cents` to light up the revenue-per-plan panels (Stripe
   revenue is already authoritative on the Revenue dashboard).

## 6. GDPR / safety

`user_id` is nullable (guests are unattributed); `ON DELETE CASCADE` from `users`
erases a user's events + stats on account deletion. No new PII columns; raw text is
not stored (only counts/durations/costs/language codes). RLS denies the Supabase
anon/authenticated roles (0096).

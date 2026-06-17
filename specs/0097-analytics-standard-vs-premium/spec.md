# 0097 — Usage analytics: Standard vs Premium

| | |
|---|---|
| **Status** | Foundation landed (schema + ingestion seam); roll-up + dashboards pending |
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

## 5. Remaining work (needs the running system / Directus)

- **Emit the full event set** at their points: `translation_used` (per finalized
  translation, with language_from/to + cost_cents), `subtitles_on`,
  `voice_generated`, `screen_shared`, `recording_started`. Each is one
  `record_event(...)` call — the seam is proven.
- **Roll-up job** — a periodic task (or Postgres scheduled function) that folds
  raw events into `plan_usage_daily` / `user_usage_stats`. Cron or a Tokio interval
  on the server; idempotent upsert keyed on `(day, plan)` / `user_id`.
- **Directus dashboards** — 5 dashboards over the aggregate tables (plan
  distribution, revenue, usage trends, feature breakdown, user intelligence). Needs
  the Directus instance; no raw-event scans.
- **KPIs** — minutes/sessions/revenue per plan, ARPU, retention (D1/D7/D30), feature
  breakdown — all computable from the two aggregate tables.

## 6. GDPR / safety

`user_id` is nullable (guests are unattributed); `ON DELETE CASCADE` from `users`
erases a user's events + stats on account deletion. No new PII columns; raw text is
not stored (only counts/durations/costs/language codes). RLS denies the Supabase
anon/authenticated roles (0096).

# 0098 — Quiz history + per-session analytics

| | |
|---|---|
| **Status** | Foundation landed (schema); persistence + UI pending |
| **Owner** | micio86dev |
| **Created** | 2026-06-17 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | _(pin on merge)_ |
| **Depends on** | 0046/0067 (quiz), 004 (call_sessions), 0096 (RLS) |

## 1. Context & Problem

Quizzes (specs 0046/0067) run live in-call as relayed game state and are **lost
when the call ends** (issue #221). We want to persist each quiz + per-user results
and surface them on the session-detail page after the call.

## 2. Design

**Schema (migration 013, RLS-enabled, reuses `call_sessions(id)`):**
- `session_quizzes` — one row per quiz: title, `questions` jsonb
  (`[{prompt, options[], correct_index}]`), `created_by` (host, NULL for guest),
  status, created_at.
- `quiz_results` — one row per participant per quiz: user_id (NULL for guests),
  peer_id, display_name, score, total, `answers` jsonb, created_at.

GDPR: `ON DELETE CASCADE` from both `call_sessions` and `users` erases a person's
quiz data with the session or the account; guests are stored by peer_id +
display_name only (already part of the transcript model).

## 3. Remaining vertical slice (needs the running system)

1. **Persist on completion.** The quiz host (or the server, which already relays
   quiz game state) writes the quiz + each participant's score when a quiz reaches
   `done`/`cancelled`. New authed endpoint `POST /api/sessions/:id/quizzes` (host
   only) inserting `session_quizzes` + `quiz_results` in one transaction.
2. **Fetch for the detail page.** `GET /api/sessions/:id/quizzes` → quizzes with
   nested results (any participant of that session may read).
3. **Session-detail UI.** A "Quizzes" sub-section in the unified outputs card
   (spec 0224): per quiz, a table of participant → score; summary stats (best
   performer, average). New i18n keys (`quizzesLabel`, `bestPerformer`,
   `averageScore`, `noQuizzes`) across all 8 languages.

## 4. Why foundation-only here

The persistence + UI is a full server+client vertical slice whose correctness is
only meaningful against a live call (quiz lifecycle → POST → DB → detail page). The
schema is the safe, auto-applying part (like 012); the slice is the focused
follow-up. Migration 013 verified to apply cleanly locally.

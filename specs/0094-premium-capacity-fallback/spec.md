# 0094 — Premium capacity fallback to Standard

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Micio Dev |
| **Created** | 2026-06-17 |
| **Shipped** | 2026-06-17 |
| **Version** | — |
| **Commits** | `<sha>` |
| **Depends on** | [0093](../0093-premium-translation-engine/spec.md) |

## 1. Context & Problem

The Premium engine (spec 0093) opens one paid OpenAI realtime session per distinct
target language, bounded by a process-wide semaphore (`OPENAI_REALTIME_MAX_SESSIONS`).
The original code acquired permits with a **blocking** `acquire().await`: once the cap
was reached, a new speaker's session-open **queued indefinitely** — the speaker got
**no translation, no error, no fallback** until a slot freed. Translations are the
product's core, and at scale (max 4/room but tens of thousands of concurrent rooms)
the cap is reached routinely, so silent queueing is unacceptable.

## 2. Goals / Non-Goals

**Goals**
- Never block/queue a speaker on Premium capacity — degrade gracefully to Standard.
- Bill the speaker for what they actually get (Standard rate during fallback).
- Tell the user clearly when it happens.

**Non-Goals**
- Mid-call auto-upgrade back to Premium when capacity frees (the speaker stays on
  Standard for the rest of the call; they can rejoin to retry Premium).
- Raising OpenAI account limits / autoscaling (operational, separate).

## 3. Requirements

- **R1 — No silent queue.** *Given* Premium is at its session cap, *when* a speaker
  starts, *then* they are served by the default (Standard) engine immediately — never
  left without translation.
- **R2 — Fair billing.** *Given* a capacity fallback, *when* the speaker is metered,
  *then* they are charged the **Standard** rate (flat, not per-stream), because that's
  the engine actually serving them.
- **R3 — Transparency.** *Given* a fallback, *then* the speaker sees a notice
  ("Premium is at capacity — using Standard…") and listeners resume browser TTS for
  that speaker.

## 4. Design & Architecture

- **`SessionOutcome`** (`engine/mod.rs`): `start_session` now returns
  `Started(sender) | AtCapacity | Failed` instead of `Option`. Only premium engines
  return `AtCapacity`; Standard returns `Started`/`Failed`.
- **Non-blocking reservation** (`engine/premium.rs`): `start_session` reserves one
  permit per target language up-front with **`try_acquire_owned`**, all-or-nothing. If
  any fails → `AtCapacity` (acquired permits drop = released). Otherwise the permits
  are handed to `run_session` (no more in-task blocking acquire). No half-Premium: a
  speaker is fully Premium or fully Standard.
- **Handler fallback** (`lib.rs` `Start`): on `AtCapacity`, switch `active_engine` to
  the default, broadcast `engine_downgraded{ reason: "premium_at_capacity" }`, and
  retry `start_session` on Standard. The meter then uses the Standard rate +
  `cost_scales_per_language=false` (R2). Reuses the spec 0093 S5 downgrade machinery
  (same message + client capture-swap). Distinct from credit exhaustion only by the
  `reason` string (client shows a capacity-specific notice).

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `SessionOutcome` trait return; Standard maps Option→outcome | `engine/mod.rs`, `engine/standard.rs` |
| S1 | Premium `try_acquire` all-or-nothing → `AtCapacity`; permits passed to `run_session` | `engine/premium.rs` |
| S2 | Handler capacity fallback + meter at Standard rate | `lib.rs` |
| S3 | Client capacity-specific notice (`enginePremiumBusy`, it+en) | `app.ts`, `i18n.ts` |

## 6. Testing & Verification

- `premium::tests::at_capacity_returns_atcapacity` (exhausted semaphore → `AtCapacity`),
  `no_targets_returns_started`. Existing integration (`audio_produces_subtitles`) proves
  Standard still returns `Started` through the handler. Server 172 unit + integration
  green; client 229 unit, typecheck + build clean.

## 7. Deployment & Operations

- No new env. With the cap no longer blocking, operators can raise
  `OPENAI_REALTIME_MAX_SESSIONS` to their OpenAI account's concurrency limit (per
  instance) without risk of stuck speakers — excess simply falls back to Standard.

## 8. Risks / Open Items

- No mid-call upgrade-back-to-Premium (acceptable; revisit if demand).
- Brief (~1 round-trip) window where the first Standard session receives PCM before
  the client swaps to WebM capture on `engine_downgraded`; subtitles resume within ~1s.

## 9. References

- Commits: `<sha>` · Files: `server/src/engine/`, `server/src/lib.rs`, `client/src/scripts/`
- Depends on spec 0093 (S5 downgrade machinery, `engine_downgraded`).

# VoxTranslate — Specification History

This directory is the **Spec-Driven Development (SDD)** record of VoxTranslate: one
spec per shipped capability, reconstructed from the codebase and git history and
kept as the source of truth going forward.

> **Reading order.** Specs are numbered in the order they were built. Each one is
> self-contained but assumes the ones before it. Start at `0001` for the core
> pipeline, or jump to the feature you care about via the table below.

## What "Spec-Driven Development" means here

Every capability is described **before code at the level of intent** — problem,
requirements, design, acceptance criteria — and the spec is then kept in lock-step
with the implementation. The specs in `0001`–`0008` were authored *retroactively*
from the shipped code (the project was built fast, ahead of its specs); from now on
new features should start from `_TEMPLATE.md` and land their spec in the same PR as
the code.

Each spec follows the same skeleton:

1. **Context & Problem** — why the feature exists
2. **Goals / Non-Goals** — what is and isn't in scope
3. **Requirements** — user stories + Given/When/Then acceptance criteria
4. **Design & Architecture** — components, data model, protocol/API, sequences
5. **Implementation** — slices/tasks and the key files that realize them
6. **Testing & Verification** — how we know it works
7. **Deployment & Operations** — how it ships and runs
8. **Risks / Open Items** — known gaps and follow-ups
9. **References** — commits, files, external docs

## Feature map

| #    | Feature | Status | Shipped | Primary commits |
|------|---------|--------|---------|-----------------|
| [0001](0001-voice-translation-rooms/spec.md) | Real-time multilingual voice-translation rooms | ✅ Shipped | 2026-06-09 | `7cea003` |
| [0002](0002-video-calls-translated-chat/spec.md) | P2P video calls (WebRTC mesh ≤4) + auto-translated chat | ✅ Shipped | 2026-06-09 | `a2c0f2b` |
| [0003](0003-client-experience-pwa/spec.md) | Client experience: PWA, pre-join, call layout, icons | ✅ Shipped | 2026-06-09 | `30a705c`, `62bda76`, `bf8ebec` |
| [0004](0004-quality-testing-ci/spec.md) | Quality gate: test suites ≥85% + CI (fmt/clippy) | ✅ Shipped | 2026-06-09 | `f1e724c`, `df238d6`, `eb98664` |
| [0005](0005-accounts-credits-billing/spec.md) | Optional accounts, credits, Stripe billing + usage metering | ✅ Shipped | 2026-06-09 | `4c4ca33` → `24f04b2` (v1.0.0) |
| [0006](0006-trust-safety-gdpr/spec.md) | Trust & safety + GDPR (consent, moderation, report/block, legal) | ✅ Shipped | 2026-06-10 | `4b84f87`, `b166d9b` |
| [0007](0007-backoffice-directus/spec.md) | Backoffice: admin actions + managed content + Directus studio | ✅ Shipped | 2026-06-10 | `ce06868`, `c0a80af`, `41305ec` |
| [0008](0008-managed-content-i18n/spec.md) | Managed content & i18n: DB-overridable strings, legal pages, 404 | ✅ Shipped | 2026-06-10 | `151980c`, `90492d1`, `c10a2df` |
| [0009](0009-session-transcripts/spec.md) | Session transcript download (PDF + JSON) | ✅ Shipped | 2026-06-10 | `7c969de` |
| [0010](0010-composite-recording/spec.md) | Composite video recording (client-side) | ✅ Shipped | 2026-06-10 | `7c969de` |
| [0011](0011-room-glossary/spec.md) | Room glossary: enforced terminology in translations | ✅ Shipped | 2026-06-10 | `18d20f8` |
| [0012](0012-auto-language-detection/spec.md) | Auto language detection (join with "auto") | ✅ Shipped | 2026-06-10 | `a594e94` |
| [0013](0013-call-bookmarks/spec.md) | In-call bookmarks: labels, side panel, exports | ✅ Shipped | 2026-06-10 | `f6eb14a` |
| [0014](0014-ai-session-report/spec.md) | AI session report (Groq, credit-billed) | ✅ Shipped | 2026-06-10 | `c2bc646` |
| [0015](0015-sentiment-analysis/spec.md) | Sentiment analysis (chunked scoring, cached) | ✅ Shipped | 2026-06-10 | `d5ce553` |
| [0016](0016-follow-up-email/spec.md) | Follow-up email: AI draft + Resend delivery | ✅ Shipped | 2026-06-11 | `2e82394` |
| [0017](0017-virtual-background/spec.md) | Virtual background (camera blur) | ✅ Shipped | 2026-06-12 | PR #6 |
| [0018](0018-chat-file-upload/spec.md) | Chat file upload (Supabase Storage, signed URLs) | ✅ Shipped | 2026-06-12 | `d04604a` |
| [0019](0019-admin-bonus-credits/spec.md) | Admin bonus credits + email notification | ✅ Shipped | 2026-06-12 | `69bbacd` (v1.1.0) |
| [0020](0020-session-sound-cues-sticky-reactions/spec.md) | Session sound cues (leave / recording) + sticky emoji reactions | ✅ Shipped | 2026-06-12 | (this PR) |

> Numbers 0011–0015 were claimed by commit messages while the AI bundle shipped
> without spec docs (and 0011/0012 were each reused twice); the assignments
> above are now canonical. Their specs were backfilled retroactively on
> 2026-06-11 from the shipped code and commit history.

## System at a glance

```
Browser (Astro 5 + vanilla TS)                Rust server (Axum 0.8 + Tokio)
┌──────────────────────────────┐             ┌──────────────────────────────────┐
│ mic ──┬─ WebRTC ─────────────┼── P2P ──────┼─▶ (server never sees A/V streams) │
│       └─ MediaRecorder ──────┼── WS bin ───┼─▶ Deepgram Nova-2 streaming STT    │
│ camera ─ WebRTC ─────────────┼── P2P ──────┤                                   │
│ chat / signaling / mute ─────┼── WS text ──┼─▶ rooms · Groq translate fan-out  │
│ SpeechSynthesis (TTS) ◀──────┼── WS text ──┼── subtitles / chat / balance      │
│ auth.ts / billing UI ◀──────▶┼── HTTP ─────┼─▶ auth · billing · safety · admin │
└──────────────────────────────┘             │      └─▶ Postgres (Supabase)       │
                                              └──────────────────────────────────┘
        Stripe ◀── checkout/webhook ──▶ server          Directus 11 ──▶ reads DB,
                                                          edits content, Flows → /api/admin/*
```

- **Frontend:** Astro 5 static + vanilla TypeScript (`client/`), deployed on **Vercel** (autodeploy on push to `main`).
- **Backend:** Rust / Axum 0.8 / Tokio (`server/`), deployed on **Railway** (`railway up`).
- **Data:** Postgres on **Supabase** (migrations `001`–`003` run at startup).
- **STT:** Deepgram Nova-2 streaming WS · **Translation:** Groq Llama 3.1 8B Instant · **TTS:** browser SpeechSynthesis.
- **Backoffice:** Directus 11 on Railway, reading the same Postgres; privileged writes go through the server's secret-guarded `/api/admin/*`.

## Conventions

- Specs are immutable history once shipped; **amend** a spec (with a dated note) rather than rewriting it when the feature evolves.
- Cross-link related specs with relative links.
- Keep money/PII details accurate but never paste real secrets, price IDs, or keys into a spec.

See also: root [`CLAUDE.md`](../CLAUDE.md) (project charter) and [`directus/README.md`](../directus/README.md) (backoffice runbook).

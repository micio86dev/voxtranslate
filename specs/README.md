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
| [0020](0020-session-sound-cues-sticky-reactions/spec.md) | Session sound cues (leave / recording) + sticky emoji reactions | ✅ Shipped | 2026-06-12 | `c13fd02` |
| [0021](0021-display-fixes-mirror-stacking/spec.md) | Display fixes: screen-share mirroring + raised-hand sidebar stacking | ✅ Shipped | 2026-06-12 | `c7c7463` |
| [0022](0022-guest-public-room-gate/spec.md) | Guest sign-in gate for public rooms (benefits modal) | ✅ Shipped | 2026-06-12 | `569aaab` |
| [0023](0023-call-toolbar-overflow-menu/spec.md) | Declutter the in-call toolbar (overflow "More" menu) | ✅ Shipped | 2026-06-12 | `1df3ce7` |
| [0024](0024-self-call-cues-hand-border/spec.md) | Self call cues (enter/leave beep) + raised-hand tile border | ✅ Shipped | 2026-06-12 | `1fd0340` |
| [0025](0025-bookmark-mobile-form/spec.md) | Mobile-friendly bookmark quick-label form | ✅ Shipped | 2026-06-12 | `aa3c5a0` |
| [0026](0026-turn-relay/spec.md) | TURN relay for cross-NAT WebRTC (server-issued ICE) | 🚧 Plumbing | 2026-06-12 | `d5f502c` |
| [0027](0027-load-testing-k6/spec.md) | Load testing the server with k6 (signaling + HTTP) | ✅ Shipped | 2026-06-12 | `dc52d60` |
| [0028](0028-security-hardening/spec.md) | Security hardening (XSS, CORS, rate limits, headers, deps) | ✅ Shipped | 2026-06-12 | `972d29a` |
| [0029](0029-security-followups/spec.md) | Security follow-ups (upload throttle, PDF timeout, rate-limiter eviction, CI audit) | ✅ Shipped | 2026-06-12 | `48e4277` |
| [0030](0030-mobile-bitrate-weak-network/spec.md) | Mobile-friendly video bitrate + weak-network warning | ✅ Shipped | 2026-06-12 | `d78c37d` |
| [0031](0031-adaptive-bitrate/spec.md) | Room-size-adaptive video bitrate (budget ÷ peers) | ✅ Shipped | 2026-06-12 | `3a4a78f` |
| [0032](0032-adaptive-budget/spec.md) | Network-adaptive video budget (AIMD via getStats) | ✅ Shipped | 2026-06-12 | `8ef35cd` |
| [0033](0033-screenshare-pan-zoom/spec.md) | Screen-share signaling + mobile pan/zoom (bigger icon, pinch) | ✅ Shipped | 2026-06-12 | `724d75f` |
| [0034](0034-ui-cta-zfix/spec.md) | Secondary CTA restyle + ⋯ overflow-menu z-index fix | ✅ Shipped | 2026-06-12 | `b049e50` |
| [0035](0035-meet-style-reactions/spec.md) | Google-Meet-style emoji reactions (big, centred, named) | ✅ Shipped | 2026-06-12 | `5e60a9c` |
| [0036](0036-reaction-anim-guest-auth-menu/spec.md) | Reaction animation polish + guest-auth & overflow-menu fixes | ✅ Shipped | 2026-06-13 | `5295378` |
| [0037](0037-guest-signin-cta/spec.md) | Guest sign-in CTA on the home screen | ✅ Shipped | 2026-06-13 | `3b051b4` |
| [0038](0038-session-glossary-ux/spec.md) | Session-details & glossary UX polish (CTA tonal, dedupe participants, save feedback) | ✅ Shipped | 2026-06-13 | `043916a` |
| [0039](0039-bookmark-require-label/spec.md) | Bookmarks always require a label (label-first prompt) | ✅ Shipped | 2026-06-13 | `a275f9c` |
| [0040](0040-tts-no-cut/spec.md) | Translated-voice TTS queues (no cut-off on rapid sentences) | ✅ Shipped | 2026-06-13 | `aa97a58` |
| [0041](0041-issue50-visibility-blur/spec.md) | Issue #50: room-visibility consistency + mobile blur aspect | ✅ Shipped | 2026-06-13 | `ff5950d` |
| [0042](0042-tts-voice-selection/spec.md) | TTS voice selection: prefer local + premium (delay-first) | ✅ Shipped | 2026-06-13 | `462cde2` |
| [0043](0043-low-latency-capture/spec.md) | Lower translation delay: 100 ms audio capture chunks | ✅ Shipped | 2026-06-13 | `115fa45` |
| [0044](0044-env-video-budget/spec.md) | Video upload budget configurable via Vercel env | ✅ Shipped | 2026-06-13 | `d1d9633` |
| [0045](0045-collaborative-whiteboard/spec.md) | Collaborative whiteboard (MVP) — issue #21 part 1 | ✅ Shipped | 2026-06-13 | `b5dea9c` |
| [0046](0046-minigame-tictactoe/spec.md) | Mini-game: Tic-Tac-Toe (game-agnostic relay) — issue #21 part 2 | ✅ Shipped | 2026-06-13 | `0c74b68` |
| [0047](0047-minigame-quiz/spec.md) | Mini-game: trivia quiz (built-in pack, host-authoritative) | ✅ Shipped | 2026-06-13 | `d3a4315` |
| [0048](0048-quiz-localized/spec.md) | Quiz questions localized per player (pre-translated pack) | ✅ Shipped | 2026-06-13 | `23f502e` |
| [0049](0049-quiz-pack-40/spec.md) | Quiz pack expanded to 40 questions (localized) | ✅ Shipped | 2026-06-13 | `f98b61b` |
| [0050](0050-observability/spec.md) | Observability: canonical logs, request IDs, structured JSON logging | ✅ Shipped | 2026-06-13 | `8ad971f` |
| [0051](0051-directus-backoffice/spec.md) | Directus backoffice: KPI dashboards, Stripe movements, acquisition `source` | ✅ Shipped | 2026-06-14 | `627325b` |
| [0052](0052-voice-command-timer/spec.md) | Voice-command countdown timer (intent-parsed from your own STT) | ✅ Shipped | 2026-06-14 | `6c91ff4` |
| [0053](0053-screenshare-camera-pip/spec.md) | Keep camera visible (PiP) during screen-share | ✅ Shipped | 2026-06-14 | `867c03b` |
| [0054](0054-whiteboard-layering-mobile-zoom/spec.md) | Whiteboard layering, mobile visibility & double-tap zoom | ✅ Shipped | 2026-06-14 | `acff8c7` |
| [0055](0055-meet-like-session-ui/spec.md) | Meet-like session UI: duration, participant count, quick reactions | ✅ Shipped | 2026-06-14 | `f3722a2` |
| [0056](0056-overflow-menu-aria/spec.md) | Overflow ⋯ menu: fix ARIA aria-required-children violation | ✅ Shipped | 2026-06-14 | `df5a85f` |
| [0057](0057-pip-controls/spec.md) | Picture-in-Picture: in-window controls + discoverability | ✅ Shipped | 2026-06-14 | `b12ba4e` |
| [0058](0058-metrics-endpoint/spec.md) | Prometheus `/metrics` endpoint (request/error/p95 + room gauges) | ✅ Shipped | 2026-06-14 | `f3bfe6d` |
| [0059](0059-turn-static-creds/spec.md) | TURN static-credential support (managed-relay fallback) | ✅ Shipped | 2026-06-14 | `873c6a9` |
| [0060](0060-meet-ui-refinements/spec.md) | Meet-style refinements: floating reactions, header clock, clearer count | ✅ Shipped | 2026-06-14 | `54d7dd6` |
| [0061](0061-immersive-call-overlays/spec.md) | Immersive call overlays: reaction chips, on-video clock/room/info + participant badge | ✅ Shipped | 2026-06-14 | `f3eb22b` |
| [0062](0062-advanced-whiteboard/spec.md) | Advanced whiteboard: multi-page, shapes, highlighter, PNG/PDF export | ✅ Shipped | 2026-06-14 | `82a7514` |
| [0063](0063-betterstack-log-shipping/spec.md) | App-side Better Stack log shipping (opt-in, NDJSON, issue #69) | ✅ Shipped | 2026-06-14 | `2f41f12` |
| [0064](0064-high-traffic-abuse-hardening/spec.md) | High-traffic abuse hardening: WS/HTTP flood rate-limits + caps (#114) | ✅ Shipped | 2026-06-14 | `da1c994` |
| [0065](0065-bounded-hot-path-channels/spec.md) | Bounded hot-path channels (out_tx/audio_tx) + backpressure (#123) | ✅ Shipped | 2026-06-15 | `8a90104` |
| [0066](0066-trusted-client-ip-header/spec.md) | Opt-in trusted client-IP header (Cloudflare-ready, #111) | ✅ Shipped | 2026-06-15 | `c233bd4` |
| [0067](0067-ai-quiz-on-demand/spec.md) | On-demand AI quiz from a prompt (Groq, credits, #124) | ✅ Shipped | 2026-06-15 | `85959de` |
| [0068](0068-ai-transcript-correction/spec.md) | AI transcript correction on export (Groq, credits, cached) | ✅ Shipped | 2026-06-15 | `f3cb0fc` |
| [0069](0069-bounded-translate-fanout/spec.md) | Bounded translation fan-out (admission semaphore on Groq) | ✅ Shipped | 2026-06-15 | `155f536` |
| [0070](0070-call-chat-game-ux-fixes/spec.md) | UX fixes batch: chat/emoji, call-header overlaps + avatars, mini-game lifecycle, secondary CTAs | ✅ Shipped | 2026-06-15 | `36d9b17`, `1bcb702`, `c6d85df`, `bd10c02`, `b58d8de` (+ `2988bc8`, `9dc9126`, `5c3bae6`) |
| [0071](0071-user-bug-report/spec.md) | User bug/error reporting (email to admins + backoffice triage) | ✅ Shipped | 2026-06-15 | `52ba019` |
| [0072](0072-public-rooms-avatars/spec.md) | Public-rooms lobby: overlapping participant avatars | ✅ Shipped | 2026-06-15 | `75c50b8` |
| [0073](0073-connection-status-banner/spec.md) | YouTube-style connection-status banner (offline / reconnecting / back online) | ✅ Shipped | 2026-06-16 | `1cb624d` |
| [0074](0074-chat-document-attachments/spec.md) | Chat attachments: documents-only + 5 MB + docx extraction + pay-to-translate billing | ✅ Shipped | 2026-06-16 | `aa4fe27`, `1d5afe7` |
| [0080](0080-webrtc-reconnect-recovery/spec.md) | WebRTC reconnect recovery: perfect negotiation + ICE restart + same-id supersede (no permanent black screen) | ✅ Shipped | 2026-06-16 | `0e36c82` |
| [0081](0081-pwa-launcher-title/spec.md) | PWA launcher title legibility: white splash background for a dark, high-contrast app title | ✅ Shipped | 2026-06-16 | `bfb4db2` |
| [0082](0082-in-call-invite-branded-email/spec.md) | In-call invite link (one-tap copy + email to people you know) + branded, Outlook-safe transactional email shell | ✅ Shipped | 2026-06-16 | `3027b5a` |
| [0083](0083-quiz-gating-gsi-polish/spec.md) | AI-quiz gating CTAs (guest → sign-in, no credits → buy) + Google sign-in button frame polish | ✅ Shipped | 2026-06-16 | `1525a6d` |
| [0084](0084-chat-composer-two-row/spec.md) | Two-row chat composer: full-width textarea + slim action bar (room to write on desktop and mobile) | ✅ Shipped | 2026-06-16 | `042382f` |

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

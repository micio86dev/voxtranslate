# CLAUDE.md — VoxTranslate

## Project

Real-time voice translation app evolving into a video meeting with live translation.
Current state: working pipeline with video calls, webinars and a Chrome widget.
Next step: add P2P video calling (WebRTC mesh, max 4) + auto-translated text chat.

## Stack

- Backend: Rust (Axum 0.8 + Tokio)
- Frontend: Astro 5 (vanilla TypeScript)
- Live translation (Standard tier): Qwen realtime on Alibaba Model Studio —
  `qwen3.5-livetranslate-flash-realtime` (env `QWEN_REALTIME_MODEL`), speech in,
  translated speech + subtitles out. The realtime ASR model backing the original-language
  transcript is a SECOND catalogue entry, `qwen3-asr-flash-realtime` (env `QWEN_ASR_MODEL`)
- Text translation (chat, webinar subtitles, transcripts): Groq `openai/gpt-oss-20b`
  (env-configurable via `GROQ_TRANSLATION_MODEL`)
- Batch transcription (uploads, recordings, voice messages): Deepgram REST — the only
  thing left on Deepgram; no live tier uses it
- TTS: server-streamed translated audio; browser SpeechSynthesis only as a fallback
- Video/Audio P2P: WebRTC mesh topology
- Audio codec: Opus/WebM, 32kbps mono, 100ms chunks (low-latency capture, spec 0043)

## Architecture

- Server: room management, WebRTC signaling relay, Qwen realtime translation sessions, Groq text fan-out, chat relay
- Server does NOT touch video/audio streams (P2P via WebRTC)
- Each speaker gets one upstream session PER TARGET LANGUAGE (deduped, semaphore-capped)
- Audio dual path: WebRTC (peers hear you P2P) + PCM16 capture (server gets audio for
  translation). PCM16 @ 24 kHz is the universal capture format — driven by the engine's
  `translated_audio` capability, never a hardcoded engine id
- Standard is the default AND capacity-fallback engine, so it never rejects a session:
  at capacity it starts the languages it can and recovers the rest on reconcile
- Webinars deliberately do NOT use the per-language shape: one transcribe-only session
  plus a Groq text fan-out, because a broadcast has many viewer languages and text-only
  subtitles

## Guests (unauthenticated peers)

- **Public rooms are account-only — to OPEN and to JOIN.** A room advertised to
  everybody must not admit anonymous strangers: moderation needs an identity it can
  report, ban and audit, and the Google account is that identity. Walk-in discovery was
  shipped and withdrawn in 1.46.2; don't reintroduce it.
- The gate keys off the room's **canonical visibility**, never off the client's
  `?public=` query param, which is spoofable (#232). A guest claiming `public=false` on
  a live public room is still refused with `login_required`. Enforcement is server-side
  in `handle_peer`; the client gates are UX, not security.
- **Private rooms are untouched**: a guest opens and joins them with no account. That is
  the whole point of the split — keep it working when you touch the gate.
- **Discovery stays open on purpose.** `GET /rooms` is unauthenticated and `/world`
  lists live rooms to anyone; seeing real conversations is what prompts the sign-up.
  Clicking one raises the sign-in gate via `&from=world`. That deep-link param must NOT
  consume the pending room: signing in re-enters `enterHome()` in the same document, and
  the pending room is what carries the new account into the room they clicked.
- Guests are pinned server-side to the **default (Standard) engine**, org-sponsored
  rooms included, so a crafted `?engine=premium` can't open a paid session nobody pays
  for. There is no "guest picks a tier".
- `GUEST_MAX_MINUTES` (default 10) caps **speaking time only** — cumulative per IP over
  a 1h rolling window, so reconnecting does not reset it. Lifted when signed in OR when
  the room is org-sponsored (the customer booked the meeting and pays for its guests).
  A guest who only listens is never capped; that is a deliberate, quantified loss
  leader, not an oversight.

## Conventions

- Rust: idiomatic async, no unwrap in production paths, tracing for logs
- TypeScript: strict mode, modular files under src/scripts/
- JSON over WS text frames for messages, binary frames for audio
- Environment variables via dotenvy
- Emoji reactions and hand-raise are relayed without translation
- **i18n is all-or-nothing: every new user-facing string MUST ship in all 84 locales**
  under `client/src/scripts/i18n/`. No English-only placeholders, no "main languages
  first, the rest later" — a partial rollout leaves most users staring at a key name or
  at English in an app whose entire promise is that it speaks their language. There is no
  translation script in the repo; generate the missing locales (Groq) as part of the same
  change and verify every file parses and carries the key.
- SQL migrations MUST be idempotent: `CREATE TABLE/INDEX IF NOT EXISTS`, `ADD COLUMN IF NOT EXISTS`, `DROP … IF EXISTS`. A non-idempotent `ADD COLUMN` (e.g. migration 044) fails and locks a DB whose schema has drifted ahead of the `_sqlx_migrations` ledger. NEVER edit an already-applied migration — `sqlx::migrate!` checksums them, so a content change breaks server boot.

## Branching & releases (Git Flow)

Project-wide rule — applies to this repo and every submodule (`dashboard/`, `website/`, …):

- `feature/<name>` off `develop`; `release/<X.Y.Z>` (three-number version); `hotfix/<name>` off `main`.
- `develop` → **staging** deploy; `main` → **production** deploy (on release/hotfix close).
  BOTH deploy from CI (`deploy-staging` / `deploy-server`), each running `railway up`
  with an environment-scoped token — neither Railway service has a GitHub source
  attached, so a branch can never reach an environment on its own. Attaching one needs
  the Railway GitHub App installed for the workspace; without that consent the API
  records an inert source and the service just redeploys its cached image.
- Merge with `--no-ff`; tag releases `vX.Y.Z`; merge a release into both `main` and `develop`.
- **Merge order is `main` first, then back-merge into `develop`** — for releases and
  hotfixes alike. Tag `main` at the merge, so the tag always names the exact tree that
  shipped to production. Deploying staging first and production second is tempting (it
  reads as safer) but it inverts the flow and leaves `main` briefly behind the code that
  was already validated; verify on staging BEFORE opening the release/hotfix instead.
- Before cutting anything, decide whether the work is a **hotfix** (off `main`, patch
  bump) or a **release** (off `develop`, minor/major bump) — the branch it comes from is
  what makes it one or the other.
- After every merge, prune the closed branch locally **and** on the remote.
- Each submodule is its own repo + deploy target; bump the parent's submodule pointer after a release.

## API Keys

- DASHSCOPE_API_KEY (alias QWEN_API_KEY) — **required**, the server refuses to boot
  without it. Must come from a Model Studio region that actually carries realtime models:
  Beijing or Singapore. US (Virginia) authenticates fine and then has no realtime model
  to open. Verify a region before pointing production at it:
  `cargo run -p voxtranslate-server --bin qwen-catalogue` (add `-- --fallback` for the
  second route). It checks BOTH models — the translate model and the realtime ASR one.
- QWEN_FALLBACK_ENDPOINT / QWEN_FALLBACK_API_KEY / QWEN_FALLBACK_WORKSPACE_ID —
  optional second Model Studio region. Unset (the default) = single route, unchanged
  behaviour. Set, and a session that cannot OPEN on the primary retries once there,
  logging at WARN both times. This exists because Standard never reports `AtCapacity`,
  so without it a broken primary region means Pro/Premium overflow lands on a dead
  engine and the room goes silent.
- GROQ_API_KEY — **required**, text translation + every `ai/` feature
- DEEPGRAM_API_KEY — optional, batch transcription only; unset it and those features
  degrade while every live tier keeps working

Pricing note: the Standard tier bills PER TARGET LANGUAGE, not flat. See
`docs/pricing-standard-qwen.md` before changing `max_room_size` or the rate.
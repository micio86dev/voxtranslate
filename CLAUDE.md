# CLAUDE.md — VoxTranslate

## Project

Real-time voice translation app evolving into a video meeting with live translation.
Current state: working pipeline with video calls, webinars and a Chrome widget.
Next step: add P2P video calling (WebRTC mesh, max 4) + auto-translated text chat.

## Stack

- Backend: Rust (Axum 0.8 + Tokio)
- Frontend: Astro 5 (vanilla TypeScript)
- Live translation (Standard tier): Qwen realtime on Alibaba Model Studio —
  `qwen3-livetranslate-flash-realtime`, speech in, translated speech + subtitles out
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

## Conventions

- Rust: idiomatic async, no unwrap in production paths, tracing for logs
- TypeScript: strict mode, modular files under src/scripts/
- JSON over WS text frames for messages, binary frames for audio
- Environment variables via dotenvy
- Emoji reactions and hand-raise are relayed without translation
- SQL migrations MUST be idempotent: `CREATE TABLE/INDEX IF NOT EXISTS`, `ADD COLUMN IF NOT EXISTS`, `DROP … IF EXISTS`. A non-idempotent `ADD COLUMN` (e.g. migration 044) fails and locks a DB whose schema has drifted ahead of the `_sqlx_migrations` ledger. NEVER edit an already-applied migration — `sqlx::migrate!` checksums them, so a content change breaks server boot.

## Branching & releases (Git Flow)

Project-wide rule — applies to this repo and every submodule (`dashboard/`, `website/`, …):

- `feature/<name>` off `develop`; `release/<X.Y.Z>` (three-number version); `hotfix/<name>` off `main`.
- `develop` → **staging** deploy; `main` → **production** deploy (on release/hotfix close).
  Staging deploys via Railway's own GitHub integration (service source: this repo,
  branch `develop`, root directory `server`) — NOT via CI. Production deploys from the
  CI `deploy-server` job, which runs `railway up` on pushes to `main`; production has no
  GitHub source attached, deliberately, so `develop` can never reach it.
- Merge with `--no-ff`; tag releases `vX.Y.Z`; merge a release into both `main` and `develop`.
- After every merge, prune the closed branch locally **and** on the remote.
- Each submodule is its own repo + deploy target; bump the parent's submodule pointer after a release.

## API Keys

- DASHSCOPE_API_KEY (alias QWEN_API_KEY) — **required**, the server refuses to boot
  without it. Must come from a Model Studio region that actually carries realtime models:
  Beijing or Singapore. US (Virginia) authenticates fine and then has no realtime model
  to open.
- GROQ_API_KEY — **required**, text translation + every `ai/` feature
- DEEPGRAM_API_KEY — optional, batch transcription only; unset it and those features
  degrade while every live tier keeps working

Pricing note: the Standard tier bills PER TARGET LANGUAGE, not flat. See
`docs/pricing-standard-qwen.md` before changing `max_room_size` or the rate.
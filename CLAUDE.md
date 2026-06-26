# CLAUDE.md — VoxTranslate

## Project

Real-time voice translation app evolving into a video meeting with live translation.
Current state: working audio-only pipeline (Deepgram STT + Groq translation + TTS).
Next step: add P2P video calling (WebRTC mesh, max 4) + auto-translated text chat.

## Stack

- Backend: Rust (Axum 0.8 + Tokio)
- Frontend: Astro 5 (vanilla TypeScript)
- STT: Deepgram Nova-2 streaming WebSocket
- Translation: Groq `openai/gpt-oss-20b` (env-configurable via `GROQ_TRANSLATION_MODEL`)
- TTS: Browser SpeechSynthesis API
- Video/Audio P2P: WebRTC mesh topology
- Audio codec: Opus/WebM, 32kbps mono, 100ms chunks (low-latency capture, spec 0043)

## Architecture

- Server: room management, WebRTC signaling relay, Deepgram streaming STT, Groq translation fan-out, chat relay
- Server does NOT touch video/audio streams (P2P via WebRTC)
- Each peer gets a dedicated Deepgram WS for streaming STT
- Audio dual path: WebRTC (peers hear you P2P) + MediaRecorder (server gets audio for STT)
- Translations fan out in parallel to all unique target languages in the room

## Conventions

- Rust: idiomatic async, no unwrap in production paths, tracing for logs
- TypeScript: strict mode, modular files under src/scripts/
- JSON over WS text frames for messages, binary frames for audio
- Environment variables via dotenvy
- Emoji reactions and hand-raise are relayed without translation

## Branching & releases (Git Flow)

Project-wide rule — applies to this repo and every submodule (`dashboard/`, `website/`, …):

- `feature/<name>` off `develop`; `release/<X.Y.Z>` (three-number version); `hotfix/<name>` off `main`.
- `develop` → **staging** deploy; `main` → **production** deploy (on release/hotfix close).
- Merge with `--no-ff`; tag releases `vX.Y.Z`; merge a release into both `main` and `develop`.
- After every merge, prune the closed branch locally **and** on the remote.
- Each submodule is its own repo + deploy target; bump the parent's submodule pointer after a release.

## API Keys

- DEEPGRAM_API_KEY — Nova-2 streaming STT
- GROQ_API_KEY — Groq translation (default `openai/gpt-oss-20b`)
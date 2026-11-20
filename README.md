# VoxTranslate

[![CI](https://github.com/micio86dev/voxtranslate/actions/workflows/ci.yml/badge.svg)](https://github.com/micio86dev/voxtranslate/actions/workflows/ci.yml)

Real-time translated communication, in four surfaces behind one translation pipeline.
Up to 4 people talk face-to-face over P2P **video calls**, each in their own language.
**Webinars** broadcast one host to many viewers with translated text subtitles. A
**VoIP phone dialer** places translated calls over the regular phone network. A
**Chrome browser widget** overlays translated subtitles on the audio of any browser
tab. Speech in calls is translated live speech-to-speech by Qwen realtime (with a
second realtime session backing the original-language transcript); chat, webinar
subtitles and transcripts are translated as text by Groq; uploads, recordings and
voice messages are transcribed in batch by Deepgram.

```
Each peer (browser)
  ├─ camera + mic ─────────────► WebRTC mesh ────► other peers hear/see you directly (P2P)
  └─ same mic track ──► PCM16 @ 24kHz capture ──binary WS──► Axum server
                                                                ├─► Qwen realtime (qwen3.5-livetranslate-flash-realtime)
                                                                │     one session PER TARGET LANGUAGE, semaphore-capped
                                                                │     speech in ──► translated speech + subtitles out
                                                                └─► Qwen realtime ASR (qwen3-asr-flash-realtime)
                                                                      original-language transcript
  Chat + transcripts translated via Groq (openai/gpt-oss-20b) fan-out.
  Server streams translated audio back to each peer; browser SpeechSynthesis is a fallback only.
  WebRTC signaling (offer/answer/ice) and chat are relayed by the server; media never touches it.
```

## Features

- 📹 **P2P video calls** — WebRTC full mesh, up to 4 peers (server never touches media).
- 🌍 **Live translated speech + subtitles** — each speaker's audio is translated
  directly by Qwen realtime into every target language in the room (one upstream
  session per speaker per target language, deduped and semaphore-capped); a second
  Qwen realtime session backs the original-language transcript.
- 📡 **Webinars** — one host broadcasts to many viewers; instead of per-language audio
  sessions, a single transcribe-only session plus a Groq text fan-out drives translated
  subtitles for however many viewer languages are in the room.
- ☎️ **VoIP / phone dialer** — translated phone calls over the PSTN (Telnyx carrier
  integration), with per-locale call-recording and AI-translation consent disclosures.
- 🧩 **Chrome browser widget** — a separate extension that overlays real-time
  translated subtitles on the audio of any browser tab (YouTube, Twitch, podcasts,
  streaming platforms, anything with sound).
- 💬 **Auto-translated chat** — messages arrive in your language, original shown
  below; supports file attachments.
- 🧑‍🤝‍🧑 **Guests** — private rooms admit unauthenticated guests with no account;
  public rooms are account-only to open **and** join. Guests are pinned to the default
  Standard engine and capped by `GUEST_MAX_MINUTES` of speaking time (listening is
  never capped), lifted when signed in or when the room is org-sponsored.
- 🖊️ **Whiteboard & screen share** — collaborative whiteboard ops and screen sharing
  (with its own audio flag so shared audio isn't ducked).
- 😀 **Emoji reactions** & ✋ **hand raise**, relayed without translation.
- 🎚️ **Controls** — mute mic, camera on/off, speak-translations (TTS), hand raise,
  chat, leave.
- 🏠 **Lobby / world discovery** — `GET /rooms` and `/world` are open and
  unauthenticated; public rooms list their online members, tap to join.
- 🎛️ **Pre-join** — camera preview + camera/mic device selectors before entering.
- 🌐 **Localized UI** — 84 locales under `client/src/scripts/i18n/`, auto-detected
  from the browser (fallback English). The same 84-locale bar applies to VoIP consent
  disclosures; narrower bars apply to push-notification copy and the business
  dashboard (see below).
- 📊 **Business dashboard** — a separate Astro submodule for org admins.
- 📱 **Mobile-first** — responsive video grid, chat as a bottom-sheet drawer.

**Localization bars** (three surfaces, three different failure modes — see
`CLAUDE.md`): `client/src/scripts/i18n/` and `server/assets/voip-disclosure.json` ship
**all 84** locales; `server/src/notify_copy.rs` (push-notification copy) ships **8**
(en/de/es/fr/it/ja/pt/zh); `dashboard/` (B2B admin console) ships **5**
(en/it/es/de/fr).

## Stack

| Layer                         | Tech                                                                 |
|--------------------------------|-----------------------------------------------------------------------|
| Backend                       | Rust — Axum 0.8 + Tokio                                              |
| Video/Audio                   | WebRTC mesh (P2P), STUN by default                                  |
| Live translation (Standard tier) | Qwen realtime — `qwen3.5-livetranslate-flash-realtime` (speech in, translated speech + subtitles out) |
| Live transcript ASR           | Qwen realtime — `qwen3-asr-flash-realtime` (original-language transcript) |
| Text translation               | Groq `openai/gpt-oss-20b` (chat, webinar subtitles, transcripts)    |
| Batch transcription            | Deepgram REST (uploads, recordings, voice messages — no live tier uses it) |
| TTS                            | Server-streamed translated audio; browser `SpeechSynthesis` as fallback |
| Telephony                      | Telnyx (PSTN carrier integration) for VoIP calling                  |
| Frontend                       | Astro 5 + vanilla TypeScript modules (`client/src/scripts/`)        |
| Dashboard                      | Astro 5 + Tailwind v4 (separate git submodule)                      |
| Browser widget                 | Chrome extension, Manifest V3 (separate git submodule)              |
| Database                       | Postgres + pgvector (accounts, billing, etc.) — call/room state itself stays in-memory and ephemeral |

Standard/Qwen is the default **and** capacity-fallback engine tier — a small registry
of additional engine tiers (behind one trait, `server/src/engine/`) exists for
higher-tier voice options; Standard is what a new deployment needs to work.

## Protocol

Peers connect to `GET /ws?room=..&lang=..&name=..&id=..&public=..` and exchange JSON
text frames (audio is sent as binary frames):

- **Client → server:** `start` / `stop` (speaking session), `offer` / `answer` / `ice`
  (WebRTC, relayed to `to`), `chat`, `mute_audio` / `mute_video`, `emoji` (reaction),
  `hand_raise` (toggle), whiteboard operations.
- **Server → client:** `room_joined` (your id + existing peers), `peer_joined` /
  `peer_left` (peer info can carry a cloned-voice id for higher-tier playback),
  `room_full`, relayed `offer` / `answer` / `ice` (with `from`), `chat_message` (with a
  `translations` map and optional file attachment), `peer_muted`, `emoji_reaction`,
  `hand_raised`, `subtitle_interim` / `subtitle_final` (with a `translations` map),
  screen-share start/stop, moderation-blocked-message notices.
- `GET /rooms` / `GET /world` — open, unauthenticated discovery of public rooms.
  `GET /health` — health check. `GET /metrics` — Prometheus scrape endpoint.

Existing peers initiate the WebRTC offer toward a newcomer (avoids offer glare).

## Prerequisites

- Rust (stable) + Cargo · Node 18+ + npm
- API keys: **`DASHSCOPE_API_KEY`** (alias `QWEN_API_KEY`, required — the server
  refuses to boot without it) and **`GROQ_API_KEY`** (required, text translation +
  every `ai/` feature). **`DEEPGRAM_API_KEY`** is optional — batch transcription only;
  unset it and every live tier keeps working.
- Optional: `QWEN_FALLBACK_ENDPOINT` / `QWEN_FALLBACK_API_KEY` /
  `QWEN_FALLBACK_WORKSPACE_ID` for a second Model Studio region fallback.
- A local Postgres (pgvector) instance if you're not running `docker compose` — see
  `docker-compose.yml`.

## Run locally

```bash
cp server/.env.example server/.env     # add your keys
```

**Server (port 3001):** `cd server && cargo run`

**Client (port 4321):** `cd client && npm install && PUBLIC_WS_HOST=localhost:3001 npm run dev`

Open **http://localhost:4321** in two tabs (or two devices on your LAN), pick a language
in each, join the same room, and you're on a translated call.

> **HTTPS:** `getUserMedia` and WebRTC need a secure context. `localhost` is exempt for
> dev; use HTTPS for LAN/remote.

## Run with Docker

```bash
cp server/.env.example server/.env
docker compose up --build       # postgres :55432 (loopback) · server :3001 · client :4321
```

## Deploy (Git Flow: `develop` → staging, `main` → production)

Frontend and backend deploy separately — Vercel is serverless and **cannot host the
persistent WebSocket relay**, so the Rust server runs on Railway. Both environments
deploy from CI with environment-scoped Railway tokens; neither Railway service has a
GitHub source attached, so a branch can never reach an environment on its own.

### Backend → Railway (CI-automated)
1. Service **Root Directory = `server`** (uses `server/Dockerfile` +
   `server/railway.toml`, `/health` healthcheck).
2. Variables: `DASHSCOPE_API_KEY` (alias `QWEN_API_KEY`), `GROQ_API_KEY`, optionally
   `DEEPGRAM_API_KEY` and `QWEN_FALLBACK_*` (Railway injects `PORT`). Optional ops:
   `LOG_FORMAT=json` (structured logs), `BETTERSTACK_SOURCE_TOKEN` (+ optional
   `BETTERSTACK_INGEST_URL`) to ship logs to Better Stack.
3. CI runs the `deploy-server` job on push to `main` and `deploy-staging` on push to
   `develop`, each running `railway up` via `railway-deploy.sh` with an
   environment-scoped Railway token — no manual `railway up` needed.

### Frontend → Vercel
1. Import this repo. **Root Directory = `client`** (Astro auto-detected).
2. Env **`PUBLIC_WS_HOST`** = your Railway domain (host only, no protocol).
3. Deploys on push to `main`.

### Webinar media → Hetzner (separate, narrow adjunct)
The Axum control plane stays on Railway; only the WHIP/LL-HLS webinar media server
runs on a small Hetzner box — see [`DEPLOY-HETZNER.md`](DEPLOY-HETZNER.md).

> **Production WebRTC:** this uses STUN only by default. For reliable connectivity
> across symmetric NATs, add a TURN server to the ICE server list in
> `client/src/scripts/webrtc.ts`. Also restrict CORS to your origin.

## Observability

- **Metrics:** Prometheus `GET /metrics` — request totals by status class, a
  request-latency histogram, live room/peer gauges, plus time-to-first-audio and
  connect-latency histograms for the realtime engines.
- **Logs:** structured JSON logs with request IDs when `LOG_FORMAT=json`; optional
  app-side shipping to Better Stack Logs when `BETTERSTACK_SOURCE_TOKEN` is set.
- **Uptime + alerts:** a GitHub Actions cron (`.github/workflows/uptime.yml`) pings the
  server `/health` and the client, and scrapes `/metrics` to alert on **5xx error rate**
  and **p95 latency** (job failure → owner email). Railway auto-restarts on crash.
- **External monitors:** Better Stack uptime monitors, provisioned reproducibly from
  [`infra/betterstack/`](infra/betterstack/) (`monitors.json` + `setup-monitors.mjs`).

## Testing

**Server — Rust unit + integration tests:**

```bash
cd server
cargo test
# coverage:
cargo llvm-cov test --summary-only
```

CI enforces an 85% line-coverage floor (`cargo llvm-cov test --fail-under-lines 85`);
run the command above locally to see the current number. VoIP/telephony modules have
their own separate, narrower coverage check in CI.

**Client — Vitest unit + Playwright e2e:**

```bash
cd client
npm run test:unit       # vitest run --coverage
npm run test:e2e        # playwright test
```

**Multi-party subtitle pipeline (standalone, no browser)** — `scripts/pipeline-test.mjs`
connects three peers (it/en/es), each speaks, asserting the `subtitle_final` fan-out:

```bash
say -v Alice -o it.aiff "Ciao a tutti, come va oggi?"
ffmpeg -y -i it.aiff -ac 1 -ar 16000 -c:a libopus -b:a 32k -f webm -live 1 it.webm
node scripts/pipeline-test.mjs it.webm
```

## Project layout

```
server/     Rust/Axum control plane
  src/engine/     translation-engine registry (Standard/Qwen is the default; a small
                  set of additional tiers exists behind the same trait)
  src/webinar/    broadcast control plane — transcribe-only session + Groq fan-out
  src/voip/       translated PSTN calling — numbers, routing, pricing, consent
  src/telephony/  carrier integration (Telnyx), E.164 formatting
  Dockerfile · railway.toml · .env.example
client/     Astro 5 SPA
  src/pages/                 screens
  src/scripts/{app,webrtc,audio-capture,chat,webinar*,voip,phone-call,phone-dialer}.ts
  src/scripts/i18n/          84 locale JSON files
dashboard/                        git submodule — business admin console (5 locales)
voxtranslate-chrome-extension/    git submodule — Chrome MV3 widget
docs/       pricing, compliance, runbooks, security assessments
infra/      Better Stack monitor provisioning
scripts/    pipeline-test.mjs
docker-compose.yml · LICENSE (PolyForm Shield 1.0.0)
```

## Notes

- **Dual audio path**: the same mic track feeds WebRTC (peers hear you live) and a
  PCM16 @ 24 kHz capture stream (server-side translation) — a MediaStreamTrack
  supports multiple consumers. WebRTC/dual-capture audio uses Opus/WebM, 32kbps mono,
  100ms chunks.
- Each speaker gets one upstream translation session **per target language** in the
  room, deduped and semaphore-capped. Standard is the default AND capacity-fallback
  engine, so it never rejects a session outright — at capacity it starts the
  languages it can and recovers the rest on reconcile.
- Webinars deliberately skip the per-language shape: one transcribe-only session plus
  a Groq text fan-out, because a broadcast can have far more viewer languages than a
  call ever has peers.
- Deepgram is REST-only now (batch transcription for uploads, recordings, voice
  messages) — no live tier depends on it.

## License

Source-available under the [PolyForm Shield License 1.0.0](./LICENSE) © 2026 Alessandro
Micelli. You may read, modify and use the code for any purpose **except** providing a
product or service that competes with VoxTranslate. This is not an OSI "open source"
license.

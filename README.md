# VoxTranslate

[![CI](https://github.com/micio86dev/voxtranslate/actions/workflows/ci.yml/badge.svg)](https://github.com/micio86dev/voxtranslate/actions/workflows/ci.yml)

Real-time **translated video calls**. Up to 4 people talk face-to-face over P2P
WebRTC, each in their own language — speech is transcribed, translated into every
participant's language in parallel, and shown as live subtitles on each speaker's
video. Includes an auto-translated text chat.

```
Each peer (browser)
  ├─ camera + mic ──► WebRTC mesh ──► other peers hear/see you directly (P2P)
  └─ same mic track ──► MediaRecorder (webm/opus, 250ms) ──binary WS──► Axum server
                                                                          └─► per-speaker Deepgram WS (STT)
                                                                                 interim → subtitle_interim (broadcast)
                                                                                 final  → Groq fan-out → subtitle_final
                                                                                          { it, en, es… } → each peer picks its lang
  WebRTC signaling (offer/answer/ice) and chat are relayed by the server.
```

## Features

- 📹 **P2P video calls** — WebRTC full mesh, up to 4 peers (server never touches media).
- 🌍 **Live translated subtitles** — each utterance is transcribed and translated into
  every language in the room **in parallel**, shown on the speaker's video cell in your language.
- 💬 **Auto-translated chat** — messages arrive in your language, original shown below.
- 😀 **Emoji reactions** — send quick emoji reactions (👍 ❤️ 😂 👏 🎉 🔥 ...) that float over the speaker's video.
- ✋ **Hand raise** — raise your hand like in Google Meet to signal you want to speak.
- 🎚️ **Controls** — mute mic, camera on/off, speak-translations (TTS), hand raise, chat, leave.
- 🏠 **Lobby** — public rooms list their online members; tap to join. Rooms can be public or private.
- 🎛️ **Pre-join** — camera preview + camera/mic device selectors before entering.
- 🌐 **Localized UI** — all 8 supported languages, auto-detected from the browser (fallback English).
- 📱 **Mobile-first** — responsive video grid, chat as a bottom-sheet drawer.

Supported languages: Italian, English, Spanish, French, German, Portuguese, Japanese, Chinese.

## Stack

| Layer        | Tech                                                        |
|--------------|------------------------------------------------------------|
| Backend      | Rust — Axum 0.8 + Tokio (WS relay + signaling)             |
| Video/Audio  | WebRTC mesh (P2P), STUN-only                                |
| STT          | Deepgram Nova-2 streaming WebSocket                         |
| Translation  | Groq `llama-3.1-8b-instant` (parallel fan-out)             |
| Frontend     | Astro 5 + vanilla TypeScript modules (`src/scripts/`)      |
| TTS          | Browser `SpeechSynthesis` API                              |

Audio/video flows **peer-to-peer**; the server only relays signaling, runs STT, fans
out translations, and relays chat. Rooms are ephemeral (in-memory `DashMap`, no DB).

## Protocol

Peers connect to `GET /ws?room=..&lang=..&name=..&id=..&public=..` and exchange JSON
text frames (audio is sent as binary frames):

- **Client → server:** `start` / `stop` (speaking session), `offer` / `answer` / `ice`
  (WebRTC, relayed to `to`), `chat`, `mute_audio` / `mute_video`, `emoji` (reaction),
  `hand_raise` (toggle).
- **Server → client:** `room_joined` (your id + existing peers), `peer_joined`,
  `peer_left`, `room_full`, relayed `offer` / `answer` / `ice` (with `from`),
  `chat_message` (with a `translations` map), `peer_muted`, `emoji_reaction`,
  `hand_raised`, `subtitle_interim`, `subtitle_final` (with a `translations` map).
- `GET /rooms` — lobby (public rooms + online members). `GET /health` — health check.

Existing peers initiate the WebRTC offer toward a newcomer (avoids offer glare).

## Prerequisites

- Rust (stable) + Cargo · Node 18+ + npm
- API keys: **`DEEPGRAM_API_KEY`** (Nova-2 STT) and **`GROQ_API_KEY`** (Llama translation)

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
docker compose up --build       # client :4321 · server :3001
```

## Deploy (production, autodeploy on `main`)

Frontend and backend deploy separately — Vercel is serverless and **cannot host the
persistent WebSocket relay**, so the Rust server runs on Railway.

### Backend → Railway
1. New Project → Deploy from GitHub → this repo. Service **Root Directory = `server`**
   (uses `server/Dockerfile` + `server/railway.toml`, `/health` healthcheck).
2. Variables: `DEEPGRAM_API_KEY`, `GROQ_API_KEY` (Railway injects `PORT`).
   Optional ops: `LOG_FORMAT=json` (structured logs), and `BETTERSTACK_SOURCE_TOKEN`
   (+ optional `BETTERSTACK_INGEST_URL`) to ship logs to Better Stack (spec 0063, off
   unless set).
3. Deploy, copy the public domain. Railway deploys are **manual**: `railway up` from `server/`.

### Frontend → Vercel
1. Import this repo. **Root Directory = `client`** (Astro auto-detected).
2. Env **`PUBLIC_WS_HOST`** = your Railway domain (host only, no protocol).
3. Deploy.

Pushes to `main` auto-deploy the **frontend**; the Railway backend is deployed manually.

> **Production WebRTC:** this uses STUN only (~85% of NATs connect). For reliable
> connectivity across symmetric NATs, add a TURN server to the `ICE_SERVERS` list in
> `client/src/scripts/webrtc.ts`. Also restrict `CorsLayer::permissive()` to your origin.

## Observability

- **Uptime + alerts:** a GitHub Actions cron (`.github/workflows/uptime.yml`) pings the
  server `/health` and the client, and scrapes `/metrics` to alert on **5xx error rate**
  and **p95 latency** (job failure → owner email). Railway auto-restarts on crash.
- **External monitors:** Better Stack uptime monitors, provisioned reproducibly from
  [`infra/betterstack/`](infra/betterstack/) (`monitors.json` + `setup-monitors.mjs`).
- **Metrics:** Prometheus `GET /metrics` (request totals by status class, latency
  histogram, live room/peer gauges) — spec 0058.
- **Logs:** canonical JSON logs with request IDs (spec 0050); optional app-side shipping
  to Better Stack Logs (spec 0063) when `BETTERSTACK_SOURCE_TOKEN` is set.

## Testing

The backend (`:3001`) must be running with real keys for the network-backed tests
(chat/subtitles) — they're skipped if `DEEPGRAM_API_KEY` / `GROQ_API_KEY` are absent.

**Server — Rust unit + integration tests** (lifecycle / signaling / max-4 / mute need no
APIs; chat + audio drive real Deepgram/Groq):

```bash
cd server
cargo test
# coverage (rustup toolchain):
rustup run stable cargo llvm-cov test --summary-only   # ~86% lines
```

**Client — Playwright e2e** (home/lobby, pre-join toggles, WebRTC video, translated chat,
subtitles, controls, room-full) with V8 coverage mapped to `src/scripts/*.ts`:

```bash
cd client
npm run test:e2e        # builds an instrumented bundle, serves it, runs e2e
# → prints "client script coverage: ~88% lines"; HTML report in client/coverage/
```

**Multi-party subtitle pipeline (standalone, no browser)** — `scripts/pipeline-test.mjs`
connects three peers (it/en/es), each speaks, asserting the `subtitle_final` fan-out:

```bash
say -v Alice -o it.aiff "Ciao a tutti, come va oggi?"
ffmpeg -y -i it.aiff -ac 1 -ar 16000 -c:a libopus -b:a 32k -f webm -live 1 it.webm
node scripts/pipeline-test.mjs it.webm
```

Coverage: **server ≈ 86% lines**, **client ≈ 88% lines** (both ≥ 85%).

## Project layout

```
server/   Rust/Axum relay
  src/{main,config,protocol,rooms,deepgram,groq,translator}.rs
  Dockerfile · railway.toml · .env.example
client/   Astro 5 SPA
  src/pages/index.astro          screens + styles
  src/scripts/{app,webrtc,audio-capture,chat,i18n}.ts
  src/layouts/Base.astro
scripts/  pipeline-test.mjs · docker-compose.yml · LICENSE (MIT)
```

## Notes

- **Deepgram input**: send `container=webm` only and let Deepgram auto-detect Opus/sample
  rate from the header (explicit `encoding`/`sample_rate` break container demuxing).
- **Dual audio path**: the same mic track feeds WebRTC (peers hear you live) and a
  MediaRecorder (server STT) — a MediaStreamTrack supports multiple consumers.
- The Groq model id lives in `server/src/groq.rs`. Deepgram auth uses `Token <key>`, Groq `Bearer <key>`.

## License

[MIT](./LICENSE) © 2026 Alessandro Micelli

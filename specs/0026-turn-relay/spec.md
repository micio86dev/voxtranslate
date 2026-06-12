# 0026 — TURN Relay for Cross-NAT WebRTC (server-issued ICE)

| | |
|---|---|
| **Status** | 🚧 In progress (plumbing shipped; coturn to deploy) |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md) |

## 1. Context & Problem

WebRTC video is peer-to-peer; the server never sees the media. A direct connection
needs ICE to find a working candidate pair. The client only had **public STUN**
(`stun.l.google.com`) and **no TURN** — so whenever both peers are behind
restrictive NATs (symmetric NAT / CGNAT, typical on mobile and **cross-border**
links) ICE can't find a path, the `RTCPeerConnection` reaches `connectionState
=== 'failed'`, and `webrtc.ts` calls `removePeer()` — the peer's tile **vanishes**.
That's the reported "I couldn't see my friend in Armenia (joined in English)" —
purely a connectivity gap (language is unrelated). The fix is a **TURN relay** plus
server-issued ICE config so credentials aren't baked into the bundle.

## 2. Goals / Non-Goals

**Goals**
- Cross-NAT / cross-border calls connect via a **TURN relay** when direct P2P fails.
- ICE config is **fetched from the server** (`/api/ice`), with **time-limited TURN
  credentials** (no long-lived secret in the client).
- Graceful: with no TURN configured, behaviour is unchanged (STUN-only).

**Non-Goals**
- ICE-restart / mid-call renegotiation resilience — the manual-signaling mesh makes
  this non-trivial; deferred (see §8). TURN fixes the reported *connect* failure.
- A per-tile connection-quality indicator / `getStats` diagnostics panel — deferred
  to a follow-up (the "in-depth video testing" ask).
- Picking/operating a specific TURN host — the plumbing is provider-agnostic; the
  runbook self-hosts coturn (user's choice) on Fly.io.

## 3. Requirements

- **R1 — Server ICE endpoint.** `GET /api/ice` (public) returns
  `{ iceServers: [...] }`: always STUN; plus a TURN entry **iff** `TURN_URLS` +
  `TURN_SECRET` are set, with `username = "<unix-expiry>:vox"` and `credential =
  base64(HMAC-SHA1(secret, username))` (coturn REST-API convention), expiring after
  `TURN_TTL_SECS` (default 3600).
- **R2 — Client uses it.** The client fetches `/api/ice` **before opening the
  signaling socket** (no race with incoming offers) and passes the servers to the
  mesh; on failure it falls back to the built-in STUN.
- **R3 — Graceful default.** With no `TURN_*` env, `/api/ice` returns STUN only and
  the call behaves exactly as before.
- **R4 — Relay actually used.** With coturn deployed + env set, a cross-NAT call
  yields a `relay` ICE candidate and connects where it previously dropped.

## 4. Design & Architecture

- **Server (`server/`):**
  - `config.rs` — new optional `TurnConfig { urls, secret, ttl_secs }` (all-or-
    nothing on `TURN_URLS` + `TURN_SECRET`), mirrored into `Config.turn`.
  - `api.rs::ice` — builds the response; mints the ephemeral credential with
    `hmac` + `sha1` + `base64` (new deps). Secret stays server-side.
  - `lib.rs` — `GET /api/ice` (public, before auth routes).
- **Client (`client/`):**
  - `webrtc.ts` — `MeshManager` takes an optional `iceServers` (defaults to the
    built-in STUN), used for every `RTCPeerConnection`.
  - `app.ts` — `fetchIceServers()` (GET `/api/ice`); `startCall()` is now async and
    awaits it before `openSocket()`, caching into `iceServers` for the mesh.
- **Infra (`infra/coturn/`):** `turnserver.conf` (REST-API mode, hardened) +
  `README.md` runbook (Fly.io deploy, env, the `external-ip`/relay-port-range
  caveats, and the `turns:`/TCP-on-5349 pragmatic fallback) — see [runbook](../../infra/coturn/README.md).
- **Key decisions:**
  - *Server-issued, ephemeral creds* — TURN credentials must not live in the static
    bundle; coturn's `use-auth-secret` lets the server mint short-lived ones from a
    shared secret it never exposes.
  - *Fetch before the socket, not in `onopen`* — awaiting inside `ws.onopen` could
    let an `offer` arrive before the mesh exists (lost peer); prefetching avoids it.
  - *coturn on Fly.io, not Railway* — TURN needs UDP + a public IP/port-range;
    Railway doesn't do UDP. Flagged to the owner before building.
  - *Provider-agnostic plumbing* — swapping coturn for a managed TURN is just
    different `TURN_*` env.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `TurnConfig` + `/api/ice` (HMAC-SHA1 ephemeral creds) | `config.rs`, `api.rs`, `lib.rs`, `Cargo.toml` |
| S1 | Client fetch + mesh `iceServers` param | `app.ts`, `webrtc.ts` |
| S2 | coturn config + Fly.io runbook | `infra/coturn/*` |
| S3 *(owner)* | Deploy coturn, set `TURN_URLS`/`TURN_SECRET` on the server | — |

## 6. Testing & Verification

- `cargo check` clean (server); `astro check` clean, 100/100 client unit tests,
  production build OK.
- The existing `webrtc.test.ts` still passes (the new `iceServers` param defaults to
  STUN, so behaviour is unchanged when unset).
- Post-deploy (owner): `curl /api/ice` shows a TURN entry with a minted credential;
  Trickle-ICE yields a `relay` candidate; a real cross-network call connects.
- ⚠️ The current E2E (`call.spec.ts`) runs both peers on **one machine (loopback)**,
  so it never exercises NAT — this class of bug can't surface there.

## 7. Deployment & Operations

- **Client**: ships with the Vercel autodeploy on `main`. No behaviour change until
  TURN env is set (graceful STUN-only).
- **Server**: needs a redeploy with `TURN_URLS` + `TURN_SECRET` (+ optional
  `TURN_TTL_SECS`) once coturn is up.
- **coturn**: a new service (Fly.io recommended) — see the runbook.

## 8. Risks / Open Items

- **No ICE restart yet** — a mid-call network change still drops the peer. Follow-up:
  initiator-side `createOffer({ iceRestart: true })` on `failed`, with glare guards.
- **No connection diagnostics** — a `getStats`-based per-tile quality/candidate-type
  indicator (relay/srflx/host, RTT, loss) is the natural next step for "in-depth
  video testing", and would make future failures self-evident.
- **TURN-on-PaaS networking** — `external-ip` + relay port range are the fragile
  bits; the runbook documents the `turns:`/TCP fallback. Managed TURN is the escape
  hatch.

## 9. References

- Symptom: cross-border call where the remote tile never appeared.
- Files: `server/src/{config,api,lib}.rs`, `client/src/scripts/{app,webrtc}.ts`,
  `infra/coturn/*`.
- coturn REST API: `use-auth-secret` / `static-auth-secret`.

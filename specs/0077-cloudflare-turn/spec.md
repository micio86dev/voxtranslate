# 0077 — Cloudflare Realtime TURN (geo-distributed relay)

| | |
|---|---|
| **Status** | In progress |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-16 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | pending |
| **Depends on** | [0026](../0026-turn-relay/spec.md), [0059](../0059-turn-static-creds/spec.md) |

## 1. Context & Problem

WebRTC media is P2P, but ~15% of calls (disproportionately **mobile + cross-border**,
behind symmetric NAT / CGNAT) can't connect with STUN alone — they need a **TURN
relay** (issue #112, the "I can't see my friend in Armenia" bug). The server already
returns TURN credentials from `/api/ice` when configured, in two modes (spec 0026
coturn-HMAC, spec 0059 managed static), but **production has no TURN configured**, so
those calls fail.

The user base is **global/mixed** (spec 0076), so a single-region relay adds hairpin
latency for distant users. The best fit is a **geo-distributed (anycast)** managed
relay that places the relay near each user. **Cloudflare Realtime TURN** is anycast,
cheap, and the project already has a Cloudflare account (it's also the #111 WAF path).

Cloudflare doesn't issue static credentials: the backend must **mint short-lived
credentials per session** by calling Cloudflare's API with a TURN key. The existing
modes (static / coturn-HMAC) don't cover this, so the server needs a new mode.

## 2. Goals / Non-Goals

**Goals**
- A **Cloudflare TURN mode**: `/api/ice` mints a short-lived credential per request
  via Cloudflare's API and returns the resulting (anycast) ICE servers.
- **Dormant until configured** — no behaviour change until the Cloudflare env vars
  are set; the secret `api_token` never reaches the client.
- **Graceful fallback** — any Cloudflare error → STUN-only, so the call still
  connects when direct P2P works (no hard dependency on Cloudflare's API).

**Non-Goals**
- Removing the coturn/managed modes (kept; Cloudflare just takes precedence).
- Client changes — `/api/ice` already returns an `iceServers` array the client
  passes straight to `RTCPeerConnection`; Cloudflare's object is a valid entry.
- Per-call credential caching (each `/api/ice` mints fresh creds; the existing
  per-IP throttle bounds the call volume).

## 3. Requirements

- **R1 — Cloudflare mode.** With `TURN_CLOUDFLARE_KEY_ID` + `TURN_CLOUDFLARE_API_TOKEN`
  set, `/api/ice` returns Cloudflare-minted TURN servers in addition to public STUN.
  - *Given* the env vars, *when* a client calls `/api/ice`, *then* the response
    includes Cloudflare's `urls` + a short-lived `username`/`credential`.
- **R2 — Precedence.** Cloudflare wins over `TURN_SECRET` and `TURN_USERNAME/PASSWORD`
  when several are set; Cloudflare needs **both** halves or it falls through.
- **R3 — No URL requirement for Cloudflare.** `TURN_URLS` is not required in this mode
  (Cloudflare returns its own URLs).
- **R4 — Fail-safe.** A Cloudflare network / non-2xx / parse failure yields STUN-only,
  never a 5xx or a hung request (5 s timeout).
- **R5 — Secret containment.** The `api_token` is server-only; only the minted
  `username`/`credential` reach the client.

## 4. Design & Architecture

- **Config (`config.rs`):** new `TurnCred::Cloudflare { key_id, api_token, ttl_secs }`.
  `TurnCred::pick` precedence = Cloudflare → Secret → Static. `TurnConfig::from_env`
  no longer requires `TURN_URLS` when the mode is Cloudflare.
- **Handler (`api.rs::ice`):** the credential `match` produces an optional
  `iceServers` entry; the Cloudflare arm calls
  `cloudflare_ice_servers(&state.http, key_id, api_token, ttl)`:
  `POST https://rtc.live.cloudflare.com/v1/turn/keys/{key_id}/credentials/generate`
  with `Authorization: Bearer <api_token>` and `{"ttl": <secs>}`, 5 s timeout, then
  returns the response's `iceServers` object (`parse_cf_ice_servers`). Errors → `None`.
- **Env (`.env.example`):** `TURN_CLOUDFLARE_KEY_ID`, `TURN_CLOUDFLARE_API_TOKEN`
  (+ existing `TURN_TTL_SECS`).
- **Key decisions:**
  - *Per-request minting* — matches Cloudflare's "short-lived per user" guidance;
    `/api/ice`'s per-IP throttle (30/min) bounds API calls. No static long-lived
    creds (Cloudflare doesn't offer them).
  - *Fail-open to STUN* — TURN is an availability aid, not a gate; a Cloudflare blip
    must not break call setup.
  - *Reuse `state.http`* — the shared pooled `reqwest::Client` already used for
    Groq/Deepgram/Supabase.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `TurnCred::Cloudflare` variant + `pick` precedence + `from_env` (URLs optional) | `server/src/config.rs` |
| S1 | `/api/ice` Cloudflare arm + `cloudflare_ice_servers` / `parse_cf_ice_servers` / `cf_turn_credentials_url` helpers | `server/src/api.rs` |
| S2 | Document the new env vars | `server/.env.example` |

## 6. Testing & Verification

- **Unit (added):** `pick` precedence (Cloudflare wins, needs both halves);
  `cf_turn_credentials_url` targets the key; `parse_cf_ice_servers` extracts a
  well-formed `iceServers` and returns `None` on a bad shape. `cargo clippy
  --all-targets` clean, `cargo fmt`.
- **Post-deploy (owner):** create a Cloudflare TURN key, set
  `TURN_CLOUDFLARE_KEY_ID` + `TURN_CLOUDFLARE_API_TOKEN` on Railway; confirm
  `GET /api/ice` returns a `turn:turn.cloudflare.com…` entry with a `credential`,
  then verify a real call connects behind symmetric NAT / 4G.

## 7. Deployment & Operations

- Ships dormant (no env vars ⇒ unchanged STUN-only behaviour). Activated by setting
  the two Cloudflare env vars on Railway (`railway variables`), then a restart/redeploy.
- The `api_token` is a secret — set it directly on Railway, never commit it.
- Cost/quota: Cloudflare Realtime TURN egress — watch usage (ties into the #109
  spend-alert follow-up).

## 8. Risks / Open Items

- **Cloudflare API latency on `/api/ice`** — bounded by a 5 s timeout + fail-open;
  if it proves hot, add short-lived per-process credential caching.
- **Token leakage** — server-only; rotate the TURN key if exposed (bounded by
  Cloudflare quotas regardless).

## 9. References

- Issue: #112 (audit master #114 §4).
- Files: `server/src/config.rs`, `server/src/api.rs`, `server/.env.example`.
- External: [Cloudflare Realtime TURN — generate credentials](https://developers.cloudflare.com/realtime/turn/generate-credentials/).

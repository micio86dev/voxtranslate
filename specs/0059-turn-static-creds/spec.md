# 0059 — TURN static-credential support (managed-relay fallback)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-14 |
| **Shipped** | 2026-06-14 |
| **Version** | — |
| **Commits** | `873c6a9` |
| **Depends on** | [0026](../0026-turn-relay/spec.md) |

## 1. Context & Problem

Cross-border / symmetric-NAT calls need a TURN relay; without one they fall back to STUN and
drop (issue #40 — "couldn't see my friend in Armenia"). `/api/ice` (spec 0026) returns TURN
**only** when configured, and today it supports a **single** credential mode: coturn's REST
convention — mint a short-lived HMAC credential from a shared `TURN_SECRET`.

That mode requires a coturn-compatible relay you run yourself (a Railway service + TCP proxy —
non-trivial ops). The issue's documented escape hatch is *"point `TURN_URLS`/creds at a free
managed relay (Metered / Twilio) — zero code change"* — but it **wasn't actually zero-code**:
managed relays hand you a **static username/password**, which the HMAC path can't use. So the
fallback was broken, leaving coturn as the only real option and #40 stuck on infra.

This adds a second credential mode so a managed relay becomes a true drop-in: paste the relay's
`TURN_URLS` + `TURN_USERNAME` + `TURN_PASSWORD` and TURN works — no coturn deploy.

## 2. Goals / Non-Goals

**Goals**
- `/api/ice` accepts **either** `TURN_SECRET` (existing HMAC/coturn) **or** `TURN_USERNAME` +
  `TURN_PASSWORD` (static, managed relay), with no other code change to enable a relay.
- Backwards compatible: an existing `TURN_SECRET` deployment behaves exactly as before.
- Keep TURN **off** unless fully configured (URLs + a usable credential).

**Non-Goals**
- Fetching ephemeral creds from a provider's *API* (Cloudflare/Twilio token endpoints) — that's a
  third mode for later; static + HMAC cover the common managed/self-hosted cases.
- Deploying or choosing the relay (the user's cost/reliability call) or the server env change
  itself (manual `railway up`).

## 3. Requirements

- **R1 — HMAC mode unchanged.** *Given* `TURN_URLS` + `TURN_SECRET`, *when* a client `GET`s
  `/api/ice`, *then* it gets the relay with a freshly-minted `username=<expiry>:vox` and an
  HMAC-SHA1 `credential` — identical to spec 0026.
- **R2 — Static mode.** *Given* `TURN_URLS` + `TURN_USERNAME` + `TURN_PASSWORD` (and no
  `TURN_SECRET`), *then* `/api/ice` returns the relay with that username/password passed through.
- **R3 — Precedence + off-by-default.** *Given* both a secret and static creds, HMAC wins.
  *Given* URLs but no usable credential (or no URLs), *then* TURN is **off** and `/api/ice`
  returns STUN only.

## 4. Design & Architecture

- **Components / files:**
  - `server/src/config.rs` — `TurnConfig` now holds `urls` + a `TurnCred` enum:
    `Secret { secret, ttl_secs }` | `Static { username, password }`. `TurnCred::pick(secret,
    username, password, ttl)` is a **pure** chooser (secret > static > none) — unit-tested.
    `TurnConfig::from_env()` returns `Option<Self>` and does all the gating (replacing the old
    external `present("TURN_URLS") && present("TURN_SECRET")` check).
  - `server/src/api.rs` — `ice` matches on `turn.cred`: HMAC-mint for `Secret`, pass-through for
    `Static`; the rest of the response (STUN + JSON shape) is unchanged.
  - `server/.env.example` — documents `TURN_USERNAME` / `TURN_PASSWORD` alongside the existing vars.
- **Key decisions:**
  - **Enum over optional fields** → makes "secret XOR static" unrepresentable-when-invalid and
    keeps the `ice` handler a clean two-arm match.
  - **Secret wins when both set** → a self-hoster who later pastes managed creds doesn't silently
    switch credential models.
  - **Security note:** static creds *do* reach the client (unlike the HMAC secret). That's inherent
    to managed relays; scope the account to relay-only use. Documented in `.env.example`.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `TurnCred` enum + pure `pick` + `TurnConfig::from_env() -> Option` | `server/src/config.rs` |
| S1 | `ice` matches HMAC vs static | `server/src/api.rs` |
| S2 | Document the new env vars | `server/.env.example` |

## 6. Testing & Verification

- **Unit (`config.rs`):** `turn_cred_pick_prefers_secret_then_static_then_none` — secret wins over
  static, static needs both halves, empty/partial → `None`.
- **Regression:** the existing `/api/ice` integration coverage + full `cargo test` (160) stay green;
  `cargo fmt --check` + `cargo clippy --all-targets` clean.
- **Manual (post-deploy):** set `TURN_URLS` + `TURN_USERNAME`/`TURN_PASSWORD` from a managed relay,
  `railway up`, then `curl …/api/ice` shows the `turn:` entry with those creds, and Trickle ICE
  yields a `relay` candidate.

## 7. Deployment & Operations

- Server-only Rust change; **Railway deploys are manual** (`railway up` from `server/`) — goes live
  on the next deploy. No migrations. To finish #40: pick a relay (managed = paste 3 vars; or
  self-host coturn = `TURN_SECRET`), set the env, redeploy, verify a `relay` candidate.

## 8. Risks / Open Items

- Static creds are visible to clients (per-relay account, not your infra) — acceptable for managed
  relays; the HMAC path remains for anyone who wants secrets to stay server-side.
- A free public relay (e.g. Metered Open Relay) is a fine stopgap but rate-limited/best-effort;
  a paid relay or self-hosted coturn is the production answer.

## 9. References

- Issue: #40 (TURN). Builds on spec 0026; unblocks its managed-relay fallback.
- Files: `server/src/config.rs`, `server/src/api.rs`, `server/.env.example`

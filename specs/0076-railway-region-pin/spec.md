# 0076 — Pin the Railway server region (EU West)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-16 |
| **Shipped** | 2026-06-16 |
| **Version** | — |
| **Commits** | `b06bd68` (#177) |
| **Depends on** | [0058](../0058-metrics-endpoint/spec.md) |

## 1. Context & Problem

Subtitles are **not P2P**: audio flows `speaker → server → Deepgram → Groq →
server → peer` (`README.md`). So the **server region sets the subtitle latency**
for most users — a distant server adds cross-region RTT on every utterance.

Issue **#113** flagged that the region was "not pinned": `server/railway.toml`
carried no region, and `railway status --json` confirms the deploy manifest's
`deploy.region` is `null`. The service nonetheless *runs* in **EU West** —
that's Railway's default placement for this account, not an explicit choice, so
it could **drift** on a future migration/recreate and silently shift latency.

The user base is **global/mixed**, so no single region is optimal; true
multi-region routing is out of scope because rooms are **in-memory per instance**
(audit #114 §1) and would need shared room state (Redis) first. The pragmatic
fix is to make the *current, sensible* placement **explicit and durable**.

## 2. Goals / Non-Goals

**Goals**
- Pin the region to **EU West** in version-controlled config so it can't drift.
- Keep the **single-instance** topology (one replica) — no behavioural change to
  placement (already EU West), just an explicit lock.

**Non-Goals**
- Multi-region signaling / replicas > 1 (needs shared room state — deferred,
  audit #114 §1 / #113 notes).
- Moving region to US/Asia (rejected: would penalise the existing EU-leaning
  traffic; no evidence of a US/Asia-dominant base).
- Co-locating Deepgram/Groq endpoints (those legs are US-ward regardless; the
  controllable lever is the user→server leg).

## 3. Requirements

- **R1 — Region pinned in config.** `server/railway.toml` declares EU West so a
  redeploy always lands there.
  - *Given* a fresh `railway up`, *when* it reads the manifest, *then* the
    service deploys to `europe-west4-drams3a` (EU West).
- **R2 — Single instance preserved.** Exactly one replica.
  - *Given* the pinned config, *when* deployed, *then* `numReplicas = 1` (no
    split-brain across the in-memory room registry).

## 4. Design & Architecture

- **Components / files:** `server/railway.toml` — `[deploy.multiRegionConfig]`.
- **Change:**
  ```toml
  [deploy.multiRegionConfig.europe-west4-drams3a]
  numReplicas = 1
  ```
  `europe-west4-drams3a` is Railway's documented identifier for **EU West**.
  Config-as-code exposes region pinning only through `multiRegionConfig`; a
  single-region map with one replica is the idiomatic way to pin one region.
- **Key decisions:**
  - *EU West as the global barycentre* — between the Americas and Asia, and close
    to the existing EU-leaning traffic; the server↔Deepgram/Groq legs are US-ward
    for any region, so optimise the user↔server leg for the largest cluster.
  - *Pin in `railway.toml`, not the dashboard* — version-controlled + applied by
    the existing CI `deploy-server` (`railway up`), consistent with the repo's
    config-as-code; survives service recreation.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Confirm current region (`railway status` → EU West) and that `deploy.region` is unset | — |
| S1 | Add the single-region `multiRegionConfig` (EU West, 1 replica) | `server/railway.toml` |

## 6. Testing & Verification

- **Pre-merge:** `railway.toml` parses (validated) and resolves to
  `{europe-west4-drams3a: {numReplicas: 1}}`.
- **Post-deploy:** CI `deploy-server` runs `railway up`; confirm `railway status`
  still reports **EU West** and the service is **Online** (health check passes).
- **Rollback:** revert this stanza; placement returns to default (already EU West).
  A wrong region id would fail the *new* deploy build while the previous healthy
  deployment keeps serving (Railway deploys atomically) — bounded downside, no
  outage.

## 7. Deployment & Operations

- No env vars / migration. Applied by the existing CI `deploy-server` on merge
  (it redeploys when `server/` changes). Brief restart; same region ⇒ no
  user-visible migration.

## 8. Risks / Open Items

- **Region id correctness** — mitigated by Railway's atomic deploy (a bad id
  fails forward without downtime).
- **Still single-region** — users far from EU keep cross-region subtitle RTT;
  the real fix (multi-region + shared room state) is deferred until the base is
  demonstrably bi-continental (audit #114 / #113).

## 9. References

- Issue: #113 (audit master #114 §4).
- Files: `server/railway.toml`, `README.md` (subtitle path).
- External: [Railway config-as-code](https://docs.railway.com/reference/config-as-code) (multiRegionConfig + region ids).

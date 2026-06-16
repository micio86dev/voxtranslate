# 0079 — Deepgram balance low-water alert

| | |
|---|---|
| **Status** | ✅ Shipped (dormant until `DEEPGRAM_API_KEY` secret added) |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-16 |
| **Shipped** | 2026-06-16 |
| **Version** | — |
| **Commits** | `1c8d323` (#185) |
| **Depends on** | [0058](../0058-metrics-endpoint/spec.md) |

## 1. Context & Problem

Deepgram (STT) has **no native dashboard spend/usage alert** — its prepaid balance
*is* the cap, so when the credit runs out STT just stops mid-service with no warning.
The account currently runs on a **$200 bonus credit** (no card → zero surprise-bill
risk), so the real risk isn't overspend, it's **running out unnoticed**. This is the
remaining slice of #109's "spend/quota alerts" that can't be a dashboard toggle.

Groq (spend limit + €9 alert) and Railway (usage soft/hard limits) already have native
alerts; only Deepgram needs an API-driven low-water check.

## 2. Goals / Non-Goals

**Goals**
- A scheduled job that reads the Deepgram balance via its billing API and **alerts
  (emails) when it drops below a threshold**, before STT stops.
- **Fail-safe**: never false-alarm — if the balance can't be read (key scope, API
  blip), warn and skip rather than alert.
- Threshold tunable; ships **dormant** until the API key is provided.

**Non-Goals**
- Groq / Railway (native alerts already configured by the owner).
- Auto-topup or any write to Deepgram.
- A bespoke alerting backend — reuse the GitHub-Actions "failed job → email" path,
  same as the uptime cron (`uptime.yml`).

## 3. Requirements

- **R1 — Low-water alert.** When the summed USD balance `< THRESHOLD_USD`, the job
  fails with a `::error`, which emails the repo watchers.
  - *Given* a balance below the threshold, *when* the daily job runs, *then* it exits
    non-zero with the remaining amount in the message.
- **R2 — Dormant + fail-safe.** With `DEEPGRAM_API_KEY` unset, or when the balance
  can't be read, the job exits **0** (skip/warn) — no false alert.
- **R3 — Tunable.** Threshold via repo variable `DEEPGRAM_BALANCE_ALERT_USD`
  (default `$20`).

## 4. Design & Architecture

- **`.github/workflows/spend-alerts.yml`** — daily (`cron: 0 9 * * *`) +
  `workflow_dispatch`. One job:
  1. `GET https://api.deepgram.com/v1/projects` (`Authorization: Token <key>`) →
     first `project_id`.
  2. `GET /v1/projects/{id}/balances` → sum `amount` where `units == "USD"`.
  3. `awk` float-compares to `THRESHOLD_USD`; below ⇒ `::error` + `exit 1`.
- **Secrets:** `DEEPGRAM_API_KEY` as a repo Actions secret (the key needs billing
  read scope; if the STT key lacks it, the job warns + skips → create a scoped key).
- **Key decisions:**
  - *Reuse the GitHub failed-job → email path* — no new infra; mirrors `uptime.yml`.
  - *Warn-and-skip on read failure* — a monitoring job must never cry wolf; only a
    *confirmed* low balance alerts.
  - *Separate daily workflow*, not a step in the 10-min uptime cron — the balance
    drains over weeks; daily is plenty and avoids hammering the billing API.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Daily workflow: resolve project → sum USD balances → alert below threshold; dormant without the key | `.github/workflows/spend-alerts.yml` |

## 6. Testing & Verification

- **Manual:** `workflow_dispatch` run with `DEEPGRAM_API_KEY` set → logs the balance
  and "Balance OK"; temporarily raise `DEEPGRAM_BALANCE_ALERT_USD` above the balance
  to confirm the failing-job email fires.
- **Dormant:** without the secret, the run logs "skipping" and exits 0.

## 7. Deployment & Operations

- Add `DEEPGRAM_API_KEY` (billing-read scope) as a repo Actions secret to activate.
  Optionally set `DEEPGRAM_BALANCE_ALERT_USD`. GitHub emails on the failed job —
  ensure Actions failure notifications are on for the owner.

## 8. Risks / Open Items

- **Key scope:** the STT key may lack billing read → job warns + skips (no alert).
  Mitigation documented; create a scoped key if needed.
- Cumulative balance only — no burn-rate/days-left projection (could add later).

## 9. References

- Issue: #109 (Deepgram slice). 
- Files: `.github/workflows/spend-alerts.yml`.
- External: [Deepgram — Get Project Balances](https://developers.deepgram.com/reference/manage/billing/list).

# 0117 — What the telephone is costing and doing

| | |
|---|---|
| **Status** | 🚧 In progress |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-09-12 |
| **Depends on** | [0112](../0112-business-phone-dashboard/spec.md), [0115](../0115-voip-numbers/spec.md), [0116](../0116-voip-inbound/spec.md) |

## 1. Context & Problem

An organisation can now dial out, take calls in, own numbers and keep an address book — and
the only way to see what any of it costs is to page through a call list twenty-five rows at
a time. `business/analytics.rs` already answers that question for meetings; it knows
nothing about telephony, because when it was written there was none.

The gap matters more here than it did there. A meeting costs translation minutes. A
telephone call costs translation minutes **plus a carrier**, and the carrier's share varies
by where you called — which is exactly the number a finance person asks for first and the
one nobody can currently produce.

Spec 0112 deferred the phone KPI on the dashboard home for this reason: a metric with no
data behind it is worse than no metric.

## 2. Goals / Non-Goals

**Goals**
- One answer to "what did the telephone do and cost, over a window".
- Split by the dimensions a B2B customer actually manages: direction, destination,
  language, tier, project.
- Answered rate and missed calls, now that a call can be missed (0116).
- The phone KPI 0112 deferred, on the dashboard home.

**Non-Goals**
- Per-user league tables. The member analytics endpoint already exists for meetings and
  extending it is a different decision, with a surveillance flavour that deserves its own
  conversation rather than arriving inside a telephony spec.
- Aggregate sentiment. The brief permits it "only where appropriate and meaningful"; over
  telephone calls to customers it is neither, and nothing in the product renders sentiment
  today anyway.
- Provider cost and margin. Business-internal, and the one thing spec 0112 R6 exists to
  keep off the wire.

## 3. Requirements

- **R1 — One window, one answer.** *Given* a look-back window, *then* the endpoint returns
  total calls, the inbound/outbound split, answered rate, missed calls, average duration,
  translated minutes and credits spent on telephony.

- **R2 — The dimensions that get managed.** *Given* the same window, *then* calls and spend
  are broken down by destination country, by language pair, by tier and by project.

- **R3 — Spend is an admin's business.** *Given* a plain member, *then* the endpoint is
  refused — matching the ADMIN gate the meeting analytics and the credits endpoint already
  use, for the same reason.

- **R4 — Our cost and our margin stay ours.** *Given* any response, *then* it contains
  neither, exactly as spec 0112 R6 requires of the call record.

- **R5 — The home page says whether the telephone is busy.** *Given* an organisation with
  phone calls enabled, *then* the dashboard home shows calls this month, minutes and missed
  calls, beside the KPIs that are already there.

## 4. Design & Architecture

One handler, `voip::analytics::summary`, beside the existing `business::analytics::summary`
and shaped like it: a `days` window clamped to `1..=365`, an ADMIN gate, and a JSON object
of named aggregates rather than a generic query language.

**Aggregated in SQL, not in Rust.** Postgres is where the rows are, `voip_calls` already
carries every dimension this needs, and pulling a window of calls into the process to fold
them would turn a cheap query into a memory profile that grows with the customer.

**Credits from the ledger, not from the calls.** `credits_consumed` on a call is what the
call metered; the ledger is what the organisation actually paid, and it includes the number
purchases and renewals a call row knows nothing about. They are different questions and the
response answers both separately rather than adding them up into one number that means
neither.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Spec | this file |
| S1 | `voip::analytics::summary` + tests | `server/src/voip/analytics.rs` |
| S2 | Dashboard: a phone analytics section | `dashboard/src/pages/[lang]/phone/analytics.astro` |
| S3 | Dashboard home: the KPI 0112 deferred | `dashboard/src/pages/[lang]/dashboard.astro` |

## 6. Testing & Verification

- A member is refused; an admin is not.
- The response contains no provider cost and no margin, asserted the way `voip_api.rs`
  already asserts it for the call record.
- Calls outside the window are excluded.
- Another organisation's calls never appear.
- A missed call counts as missed and not as answered.

## 7. Deployment & Operations

No migration, no env var, no provider surface. Reads what is already there.

## 8. Risks / Open Items

1. `actual_provider_cost_usd` is still NULL for every call, because `fetch_cdr` is
   unsupported (0111 §8 and `docs/voip-telnyx-setup.md` §7). So "what it cost US" remains
   unanswerable — which is fine here, because R4 says it must not be answered to a customer
   anyway, but it means the internal margin report this data would otherwise feed still has
   no input.
2. The window is a day count rather than a date range, matching the existing endpoint. A
   finance person wanting "last calendar month" cannot ask for it precisely.

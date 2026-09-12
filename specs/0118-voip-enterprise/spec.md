# 0118 — Enterprise telephony: hours, menus, and the honest edge of SIP

| | |
|---|---|
| **Status** | 🚧 In progress |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-09-12 |
| **Depends on** | [0116](../0116-voip-inbound/spec.md), [0115](../0115-voip-numbers/spec.md) |

## 1. Context & Problem

0116 made a number ring somebody. Every Enterprise customer's next three questions are the
same: *what happens outside office hours*, *can callers choose a department*, and *can this
plug into the PBX we already have*.

Two of those are answerable today. The third is not, and this spec says so rather than
shipping a screen that implies otherwise.

## 2. Goals / Non-Goals

**Goals**
- A number knows when the office is open, in the organisation's own timezone, and does
  something different when it is not.
- A caller can choose a department with one keypress, and each choice routes on its own.
- Both are configured in the same screen as the rest of a number's routing.

**Non-Goals, and why — each of these is a decision, not an omission**

- **Call queues.** A queue is not a list; it is hold music, a position announcement, an
  abandon policy and a concurrency model. The media plane is single-instance
  (0111 §8.9, restated in 0116 §8.1), so a queue would hold callers on one replica and
  lose all of them when it restarts. Building the screen before that is fixed would be
  selling a promise the architecture cannot keep.
- **SIP trunking and PBX integration.** The provider boundary is defined here so nothing
  has to be redesigned later, and the operations it needs answer `Unsupported` — the same
  treatment `fetch_rate_deck` and caller-ID verification already get. A SIP connection
  cannot be created, credentialled or tested without a live carrier account, and an
  adapter written against documentation and never run is an adapter that does not work.
  §7 says exactly what is needed to finish it.
- **Multi-level IVR trees.** One level, with a fallback. Nested menus are where IVRs
  become the thing customers hate, and the second level can wait until somebody asks for it.

- **The IVR's AUDIO flow.** The data model, the routing decision and the configuration
  screen ship here; playing the menu and acting on the keypress does not. The reason is
  specific rather than general: the consent gate (0111 R20) already owns the DTMF stream on
  an inbound leg, through `gather` and the `Dtmf` webhook, and it is the thing that decides
  whether a human being may be recorded. A second consumer of the same digits on the same
  call is a real design problem — which press belongs to which question — and getting it
  wrong breaks consent rather than breaking a menu. It needs its own pass, with the gate's
  state machine extended deliberately instead of shared by accident.

## 3. Requirements

- **R1 — A number knows when the office is open.** *Given* opening hours and a timezone,
  *when* a call arrives outside them, *then* the number's closed-hours action runs instead
  of ringing anybody.
  - *Given* no hours are configured, *then* the number is always open — an organisation
    that has not said otherwise has not asked to be closed.

- **R2 — Hours are evaluated in the organisation's timezone**, not the server's and not the
  caller's. A call at 09:00 in Milan is inside Milan office hours wherever our process runs.

- **R3 — A menu can be described, and a keypress resolved.** *Given* a menu is configured,
  *then* each option carries a digit, a label and its own ring target, and one digit means
  one thing.
  - *Given* a digit with an option behind it, *then* `route_for` returns that option.
  - *Given* a digit with nothing behind it, or no digit at all, *then* it returns nothing,
    so the caller can be told and the menu repeated rather than met with silence.
  - *Note:* the audio that asks the question is **not wired** — see Non-Goals. What ships
    is the configuration and the decision, both tested.

- **R4 — Menus and hours compose in one order.** Hours are checked first. A menu that
  offers departments nobody is in is worse than a closed sign.

- **R5 — Neither is required.** *Given* an organisation that configures neither, *then*
  0116's behaviour is unchanged, exactly.

## 4. Design & Architecture

**`voip_business_hours`**, one row per number: a timezone, seven day-windows, and what to
do when closed. Stored as minutes-from-midnight rather than as times, because arithmetic on
a `TIME` across a timezone boundary is where this kind of feature usually goes wrong.

**`voip_ivr_options`**, one row per key per number: a digit, a label, and the same ring
target shape the routing row already uses. Reusing that shape means an option routes by
exactly the rules a number does, rather than by a second implementation that drifts.

**The decision is pure.** `hours::is_open(now, timezone, windows)` and
`ivr::route_for(digit, options)` take their inputs and return an answer — no database, no
clock of their own, no provider. That is what makes "is 23:59 on a Sunday inside Monday's
window" a test rather than an argument, and it is the same discipline
`verify_webhook` already follows.

**The SIP boundary is declared, not implemented.** `TelephonyProvider` gains
`create_sip_connection` / `list_sip_connections`, both answering `Unsupported` in both
adapters. Declaring it costs one match arm and means the day somebody has an account, the
shape is already agreed.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Spec | this file |
| S1 | Migration 063 — hours and menu options | `server/migrations/063_voip_enterprise.sql` |
| S2 | Pure decisions + tests | `server/src/voip/hours.rs` |
| S3 | Inbound consults them | `server/src/voip/inbound.rs` |
| S4 | API + dashboard, in the routing screen | `server/src/voip/numbers.rs`, `dashboard` |
| S5 | The SIP boundary, declared and refused honestly | `server/src/telephony/*` |

## 6. Testing & Verification

- Open and closed on each side of a window boundary, including midnight and a window that
  crosses it.
- A call at 09:00 Europe/Rome is open while the process runs in UTC.
- No hours configured means always open.
- A digit with an option resolves to it; one without resolves to nothing, which is what
  lets the caller be told rather than met with silence.
- An organisation with neither configured behaves exactly as it did under 0116.

## 7. Deployment & Operations

- No new environment variable. Hours and menus are per-number configuration.
- **To finish SIP** a live Telnyx account is required, plus: a SIP Connection created in
  the portal, credentials or IP authentication decided, the FQDN and ports recorded, and a
  test call placed in each direction. None of that can be written against documentation and
  left unrun.

## 8. Risks / Open Items

1. **Queues remain unbuilt**, and the media plane is why. That constraint has now blocked
   the same feature twice; it is the single highest-value thing to fix in this product's
   telephony.
2. **Timezone data comes from the `chrono-tz` database compiled into the binary.** A
   country that changes its DST rules needs a dependency bump and a deploy, not a
   configuration change.
3. The menu is spoken by the carrier's voice (0111 §8.10), so it inherits the same narrower
   language list, and falls back to English the same way with the row recording that it did.

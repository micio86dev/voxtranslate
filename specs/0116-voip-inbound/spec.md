# 0116 — Receiving translated telephone calls

| | |
|---|---|
| **Status** | 🚧 In progress |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-09-12 |
| **Depends on** | [0111](../0111-translated-voip/spec.md), [0114](../0114-voip-contacts/spec.md), [0115](../0115-voip-numbers/spec.md) |

## 1. Context & Problem

Spec 0111 §2 named inbound a non-goal in its first milestone: *"The abstraction admits them;
the orchestration does not yet."* Three years of that sentence being true is what this spec
ends.

Today `service::dial` writes the SQL literal `'outbound'`, `TelephonyProvider::answer()`
has **zero callers anywhere in the server**, `ProviderCapabilities::inbound` is declared by
the Telnyx adapter and read by nothing, and `voip_numbers.inbound_enabled` has never been
queried. An organisation can buy a number (0115) and give it to a customer, and when that
customer rings it, nothing happens.

The pieces are all built. The consent announcement, the media pump, the codec, the room
assembly, the billing — every one of them is direction-agnostic. What is missing is the
half-dozen decisions that turn *a stranger is calling one of your numbers* into *somebody
in your organisation is talking to them, in their own language*.

## 2. Goals / Non-Goals

**Goals**
- A call to an organisation's number reaches somebody in that organisation, translated.
- The caller is identified when the address book knows them (0114), and the language they
  are answered in follows from that.
- A call nobody answers does something honest — voicemail, a forward, or a clean refusal —
  rather than ringing for ever.
- Routing is configured in one screen that never says "Call Control Application".
- A missed call is a record, not a gap.

**Non-Goals**
- IVR menus, call queues and business hours. Each is a scheduling problem with its own
  edge cases, and they belong with the Enterprise work in 0118.
- Two telephones bridged through us (PSTN↔PSTN). 0111 §2 non-goal, unchanged.
- Simultaneous ring across many devices with first-to-answer wins at the carrier. We ring
  people, and the first person to join the room gets the call.

## 3. Requirements

- **R1 — A call to our number becomes a call record.** *Given* the carrier reports an
  incoming call to a number an organisation owns and has `inbound_enabled`, *then* a
  `voip_calls` row is created with `direction = 'inbound'`, the caller's number, and the
  organisation resolved from the number that was rung.
  - *Given* the number is not ours, or inbound is off for it, *then* the call is refused
    and nothing is created.

- **R2 — We know who is calling, when we can.** *Given* the caller's number is in the
  organisation's address book, *then* the call names the contact and is answered in the
  language recorded for that number.
  - *Given* it is not, *then* the call proceeds with the number shown and the
    organisation's configured default language for strangers.

- **R3 — Somebody is rung.** *Given* an inbound call, *then* the users its number routes to
  are notified, through the notification system the product already has, with a way to join.
  - *Given* the routing names a team, *then* its members are the ones rung.
  - *Given* the routing names nobody, *then* the organisation's owners are rung, because a
    call nobody is told about is worse than a call the wrong person takes.

- **R4 — Nobody answering is a decision, not a hang.** *Given* nobody joins within the
  configured ring time, *then* the configured action runs: voicemail, a forward to another
  number, or a polite refusal.
  - *Given* voicemail, *then* the caller hears a message in their own language and the
    recording lands in the call's record.
  - *Given* the call is missed, *then* it appears in history as missed, with who was rung.

- **R5 — The recipient is told before anything is captured.** *Given* recording or
  transcription is on, *then* the consent machinery from 0111 R19 applies unchanged — a
  caller who rang US is owed exactly what a caller we rang is owed.

- **R6 — Routing is configured in human terms.** *Given* the routing screen, *then* it asks
  what should happen when somebody calls this number, and never uses a provider's
  vocabulary.

- **R7 — Inbound is billed like a call, not like a gift.** *Given* an inbound call,
  *then* its translation minutes are metered and settled against the organisation's credits
  exactly as an outbound call's are.

## 4. Design & Architecture

**The provider boundary gains one event kind.** `ProviderEventKind::Incoming { from, to }`
— distinct from `Initiated`, which means *our own dial has started*. Overloading one kind
for both would make every downstream branch ask "but which direction?", which is precisely
the question the type should answer.

**Routing lives on the number**, in `voip_number_routing`: who to ring, for how long, and
what to do when nobody does. One row per number, created with a sane default when the
number is bought so that an organisation is never one forgotten form away from silence.

**Ringing is the notification system, not a new one.** `notifications::notify` already
writes a row, respects per-user preferences and sends web push. An inbound call is a
notification with a join link — the same link the outbound dialer already hands out.

**The answer is a room**, assembled exactly as an outbound call's is: a phone peer, the
media socket, the engine session. `session::create_phone_peer` does not care which end
started it. The only new thing is that the *human* joins after the *telephone*, rather than
before.

**Nobody answered** is decided by a clock, not by hope: the existing sweep in `webhook.rs`
gains a pass over inbound calls whose ring deadline has passed. Same reasoning as
`fail_stalled_calls` — only a clock can notice an absence.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Spec | this file |
| S1 | `Incoming` event + Telnyx/mock normalisation | `server/src/telephony/*` |
| S2 | Migration 062 — routing, and inbound columns on calls | `server/migrations/062_voip_inbound.sql` |
| S3 | Inbound admission: identify, create, answer, ring | `server/src/voip/inbound.rs` |
| S4 | Ring timeout + no-answer actions | `server/src/voip/inbound.rs`, `webhook.rs` |
| S5 | Routing API + dashboard screen | `server/src/voip/routes.rs`, `dashboard/.../phone/routing.astro` |
| S6 | History and call detail show direction and who was rung | dashboard |

## 6. Testing & Verification

- A call to a number we do not own is refused and creates nothing.
- A call to a number with inbound off is refused.
- A known caller's call names the contact and uses that number's language.
- An unknown caller's call uses the org's stranger default.
- The routed users are notified; a team routes to its members; no routing routes to owners.
- A call nobody joins within the ring time becomes missed, and the configured action runs.
- Consent still applies: capture does not begin before the announcement.
- An inbound call holds and settles credits like an outbound one.

## 7. Deployment & Operations

- `VOIP_INBOUND_ENABLED`, default `false`. Inbound changes what a phone number DOES, and a
  deployment should opt into that deliberately.
- `VOIP_RING_SECONDS_DEFAULT`, default `25` — long enough not to clip a person walking to
  their desk, short enough not to feel broken.
- The carrier must be configured to send incoming calls to our Voice App; that is a portal
  step, documented in `docs/voip-telnyx-setup.md`.

## 8. Risks / Open Items

1. **The media plane is still single-instance** (0111 §8.9). Inbound makes that sharper: an
   incoming call arrives at whichever replica the carrier's webhook reached, and the room
   must be assembled there. Unchanged by this spec, and now load-bearing in a second
   direction.
2. **Ringing is a notification, not a ring.** There is no persistent socket to the
   dashboard, so "your phone is ringing" arrives as a push notification and a row. That is
   honest and it is not a desk phone; a real in-browser ring needs the dashboard to hold a
   connection, which is its own piece of work.
3. **Voicemail is a recording**, with everything that implies for consent and retention. It
   inherits the org's recording policy rather than inventing a second one.
4. **A forward is a second billable leg.** Priced and gated like any outbound call, and
   refused by the same policy gate — a forward must not become a way around the spend caps.

## 8b. Amendment — 2026-09-12

**Voicemail is built; forwarding is configured but not executed.**

R4 promised three no-answer actions and the first cut of the sweep implemented one: it
marked the call missed and hung up. Voicemail now works properly — the caller is told what
is about to happen, then recorded, and a second pass closes the line after two minutes and
lets the ordinary `RecordingSaved` webhook attach what was said. The `missed` column is
what distinguishes the two passes; without it the second would replay the prompt for ever,
because a call taking a message is still unanswered and still has a deadline.

**`forward` is accepted, validated and stored, and does not yet place the second call.** It
is refused at configuration time without a destination (a forward with nowhere to go dies
silently at the moment it matters most), and a number configured for it behaves as `refuse`
until the leg is wired. Wiring it means running the forward through `service::dial`'s policy
gate and reservation — not around them — because a forward that skipped the spend caps would
be a way to spend an organisation's money that its own limits did not see. That is the whole
of the remaining work, and it is deliberately not a shortcut through the billing path.

## 8c. Amendment — 2026-09-12

**Voicemail obeys the recording policy. It did not, and that was a consent defect.**

The sweep decided to take a message from `no_answer_action` alone. `service::capture_intent`
describes itself as "intersection, never union" and gates every outbound recording on the
deployment switch AND the organisation's — and the voicemail path walked straight past it.
An organisation that had never turned recording on still had its callers recorded, because
routing was the only thing consulted.

Routing asks; policy decides. A voicemail is a recording, and of a person who did not choose
to be recorded by us. The sweep now reads `voip_org_settings.recording_enabled` alongside
`VOIP_RECORDING_ENABLED` and takes a message only when both allow it; otherwise the caller is
hung up on and the call is closed as missed. The refusal is logged at WARN naming which of the
two switches is off, because an operator who configured voicemail and never sees one would
otherwise debug the router for a day.

This makes voicemail off by default, everywhere. That is the correct default and the same one
`OrgSettings::default_for_new_org` already argues for: a default that captures someone nobody
asked is a different kind of mistake from a default that refuses a call.

Guarded by `voicemail_obeys_the_orgs_recording_policy`, and by
`an_unanswered_call_takes_a_message_and_then_closes_it`, which had to be given both switches
before it would pass — which is the evidence the gate was missing.

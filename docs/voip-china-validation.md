# VoIP — mainland China validation

**Status:** gate CLOSED. No validated route on file. Nothing about China may be advertised.

A normal telephone call to a Chinese mobile is a carrier call, not an internet
connection, so the Great Firewall is not the relevant obstacle — that is the honest
technical position, and it is also the easy half. The hard half is that international
route quality into China, caller-ID presentation, carrier-side filtering of foreign
numbers and local telecom rules are all real, all vary by carrier and by week, and none of
them can be assessed from outside the country.

So the rule is simple and it is enforced in code, not in a promise:

> **China calling is disabled until a real call to a real handset physically in mainland
> China has been placed and its results recorded. A VPN test does not count.**

---

## 1. The gate

Two settings, both defaulting to the safe value:

| Setting | Default | Effect |
|---|---|---|
| `VOIP_CHINA_ENABLED` | `false` | Master switch. Off ⇒ any `+86` destination is refused before dialing with `destination_not_allowed`. |
| `VOIP_CHINA_REQUIRE_VALIDATED_ROUTE` | `true` | Even with the master switch on, a `+86` dial requires a matching `valid = true` row in `voip_route_validations`. |

The gate keys off `E164::region() == "CN"`, which resolves from the assigned calling code
— not off a client-supplied country field, which is spoofable.

`voip_route_validations.tested_from_country` is the column that makes "no VPN tests"
checkable rather than aspirational: a validation only counts when the **recipient was
physically in the country being validated**. A row where `tested_from_country != country`
is not a validation of that route and must not be marked `valid`.

## 2. What must be measured

One row per (carrier, number type). A validation is `valid = true` only when **every**
mandatory item passes.

| # | Measurement | Column | Mandatory | Pass criterion |
|---|---|---|---|---|
| 1 | Call setup success over ≥ 20 attempts | `call_setup_success` | yes | ≥ 95% |
| 2 | Post-dial delay (dial → ringing) | `post_dial_delay_ms` | yes | p95 < 6000 ms |
| 3 | One-way audio observed | `one_way_audio` | yes | never |
| 4 | Audio quality, subjective MOS both directions | `audio_quality_mos` | yes | ≥ 3.5 |
| 5 | End-to-end translation latency | `translation_latency_ms` | yes | p95 < 2500 ms |
| 6 | DTMF delivered and detected | `dtmf_ok` | yes | works — the consent gate depends on it |
| 7 | Caller ID as presented on the handset | `caller_id_presented` | yes | recorded verbatim, even if suppressed |
| 8 | Hangup detected within 5 s of the recipient hanging up | `hangup_detected` | yes | billing stops on time |
| 9 | Jitter / packet loss where the provider exposes them | `voip_call_quality` | no | recorded |
| 10 | Mandarin STT quality on the recipient's speech | `notes` | yes | intelligible; no systematic dropouts |
| 11 | Mandarin TTS intelligibility to the recipient | `notes` | yes | recipient confirms it is understandable |
| 12 | Recording disclosure understood | `notes` | yes | recipient confirms the announcement was clear |

Items 10–12 need a **native Mandarin speaker on the handset**. They are judgements, and a
judgement recorded honestly is worth more than a metric that measures the wrong thing.

## 3. Coverage required before the gate opens

At minimum:

- **China Mobile** mobile (largest subscriber base)
- **China Unicom** mobile
- **China Telecom** mobile
- Ideally one **Beijing or Shanghai fixed line**

Each with its own row. Passing on one carrier says nothing about the others — international
termination into China is carrier-specific.

## 4. Procedure

1. Recruit a tester **physically in mainland China**. Confirm their location independently
   (not by asking the browser, which is trivially wrong on a VPN).
2. Set `VOIP_CHINA_ENABLED=true` and `VOIP_CHINA_REQUIRE_VALIDATED_ROUTE=false` **in
   staging only**, for the duration of the test window.
3. Place the calls from a staging account with a small credit balance, so a routing
   surprise cannot become a large bill.
4. Record every measurement in §2 as it happens, not from memory afterwards.
5. Insert one `voip_route_validations` row per (carrier, number type), with
   `tested_from_country = 'CN'`, `tested_by` set, and `valid` reflecting §2 honestly.
6. Set `expires_at`. **Six months** is the recommended validity: international routes are
   re-negotiated and re-provisioned, and a two-year-old validation is a story, not
   evidence.
7. Revert staging to the defaults.
8. Only then consider enabling `VOIP_CHINA_ENABLED` in production, with
   `VOIP_CHINA_REQUIRE_VALIDATED_ROUTE=true` — so the gate keeps reading the evidence.

## 5. What must not be done

- **Never mark a route valid from a VPN test.** It measures our own network, not the
  Chinese carrier's.
- **Never mark a route valid from a single call.** Setup success is a rate.
- **Never enable the master switch in production without rows on file.** The switch exists
  to be able to turn China *off* quickly; it is not a shortcut to turning it on.
- **Never claim China support on the website** until the gate is open. The FAQ answer is
  generated from the capability data precisely so that the marketing copy cannot get ahead
  of the evidence.

## 6. Video and China

Video is a **browser** connection, so the Great Firewall becomes relevant again and none
of the above transfers. A separate validation is required before any claim about video to
a recipient in China, and it must include an audit of every third-party asset the invite
page loads — a single blocked font or script host makes the page fail in a way that looks
like our bug.

## 7. Current state

| Carrier | Type | Validated | Expires |
|---|---|---|---|
| — | — | **none on file** | — |

`VOIP_CHINA_ENABLED` is `false` in every environment.

# VoIP — data flow, subprocessors and processing regions

**Status:** current as of 2026-09-10. Extends `docs/gdpr-readiness-2026-09.md`; where the
two disagree, that document is the older one and this is the correction.

**This document does not give legal advice, and nothing in it should be published to
customers without legal review.** It records what the code actually does, so that a claim
made on a sales call can be checked against something.

---

## 1. The headline, stated plainly

> **Telephony and media for a translated phone call are processed in the EU.**
> **The translation itself is not.**

Both halves matter. Saying only the first is the mistake this document exists to prevent.

## 2. What happens to a translated phone call, step by step

Mode 1 — a VoxTranslate user in the browser calls a telephone number.

| # | Step | Who processes it | Where | Evidence |
|---|---|---|---|---|
| 1 | Caller's microphone → PCM16 24 kHz over WSS | VoxTranslate | EU — GCP `europe-west4`, Netherlands | Railway service config |
| 2 | Dial, SIP signalling, PSTN termination | **Telnyx** | EU — Frankfurt | `api.telnyx.eu`, anchorsite `Frankfurt, Germany`; `TelnyxConfig::is_eu()` requires both |
| 3 | Recipient audio → RTP → media WebSocket | **Telnyx** → VoxTranslate | EU → EU | `stream_url` is built from `VOIP_MEDIA_WS_BASE` |
| 4 | Speech → text (STT) | depends on tier — see §3 | **outside the EU on every tier** | §3 |
| 5 | Text → translated text | **Groq** on every tier | **United States** | `server/src/groq.rs:8` |
| 6 | Translated text → speech (TTS) | depends on tier — see §3 | **outside the EU on every tier** | §3 |
| 7 | Translated audio → recipient | VoxTranslate → **Telnyx** | EU → EU | bidirectional RTP on the same leg |
| 8 | Transcript rows, call metadata, credits | VoxTranslate | EU — Supabase `eu-west-1`, Ireland | `docs/gdpr-readiness-2026-09.md` §1 |
| 9 | Recording object (if enabled) | Telnyx, then VoxTranslate storage | EU | Telnyx EU storage; Supabase Ireland |
| 10 | AI summary / sentiment (if enabled) | **Groq** | **United States** | `server/src/ai/` |
| 11 | Logs and metrics | Better Stack | EU ingest endpoint — **unverified**, see §6 | `docs/gdpr-readiness-2026-09.md` §5 |

Steps 4–6 and 10 are the transfers. Everything else stays in the EU.

## 3. Processing region by translation tier

Verified against the code, not against a vendor page.

| Tier | Engine id | STT + translation + TTS | Region | Evidence |
|---|---|---|---|---|
| Standard (default and capacity fallback) | `standard` | Alibaba Model Studio, Qwen realtime | **Singapore** | `engine/qwen.rs` → `dashscope-intl.aliyuncs.com` |
| Enhanced | `cartesia` | Cartesia Ink / Sonic, **browser-direct** | **United States** | `engine/cartesia.rs`, `api.cartesia.ai` |
| Pro | `premium` *(historical id)* | OpenAI Realtime | **United States** | `engine/openai.rs` |
| Premium | `gemini_live_translate` | Google Gemini Live | **United States / global** | `engine/gemini.rs` |
| **Every tier** | — | Groq, for subtitle / chat / transcript **text** | **United States** | `groq.rs:8` |
| Batch only | — | Deepgram, for uploads and recordings | **United States** | `deepgram.rs` |

### Why fixing Qwen alone does not make any tier EU-only

This is the single most commonly repeated mistake about VoxTranslate's compliance posture,
so it is stated explicitly.

Alibaba does operate a Frankfurt Model Studio region, and **the code is already ready for
it** — `engine/qwen_catalogue.rs` handles `dashscope.eu-central-1.aliyuncs.com` and the
workspace-host form `{workspace}.eu-central-1.maas.aliyuncs.com`. Moving Standard there is
a change of `QWEN_ENDPOINT`, not a change of code.

It still would not make the Standard tier EU-only, **because Groq handles the text on
every tier** — subtitles, chat translation and transcript translation all go to
`api.groq.com` in the United States. Two migrations are needed, not one:

1. Qwen realtime → Alibaba Frankfurt (`eu-central-1`), and
2. Groq's text path → an EU region, or a substitute text model hosted in the EU.

Until **both** land, `supports_eu_only` is `false` for every tier and must stay that way.

## 4. How the code enforces this

`VOIP_REQUIRE_EU_PROCESSING` (spec 0111 R22) is implemented and enforced, and defaults to
`false`.

- When it is **off**, calls proceed and the transfers in §2 apply. The customer must be
  told about them; that is what the subprocessor list in §5 is for.
- When it is **on**, a call whose tier reports `supports_eu_only = false` is **refused
  before dialing**, with reason `eu_processing_unavailable`, and the refusal is logged.
  It is never silently downgraded and never silently allowed.
- Because every tier reports `false` today, **turning the flag on refuses every call.**
  That is the correct behaviour and it is deliberate: a flag that claims a guarantee it
  cannot keep is worse than no flag.

The tier capability is read from configuration (`*_EU_REGION`), not hardcoded, so it flips
without a code change on the day the two migrations above are done.

`TelnyxConfig::is_eu()` requires **both** the `api.telnyx.eu` base and an anchorsite in
Frankfurt, Amsterdam or London. Either half alone is a residency claim that is not true,
and both are one environment variable away from being wrong, which is why the check is in
code with a test rather than in a runbook.

## 5. Subprocessors introduced by this feature

Only Telnyx is new. Everything else was already a subprocessor for meetings.

| Subprocessor | Purpose | Personal data reaching it | Region | Transfer basis |
|---|---|---|---|---|
| **Telnyx** (new) | PSTN origination/termination, SIP, RTP media, optional recording storage | Recipient's telephone number, caller id, call audio in both directions, call metadata | EU — Frankfurt | Intra-EU; **DPA to be executed — action item** |
| Alibaba Cloud (Model Studio) | Standard tier STT/translation/TTS | Call audio, transcript text | Singapore | Third-country transfer — SCCs to be confirmed |
| Groq | Text translation on every tier; AI summaries | Transcript text, chat text | United States | Third-country transfer — SCCs to be confirmed |
| OpenAI | Pro tier | Call audio, transcript text | United States | Third-country transfer — SCCs to be confirmed |
| Google (Gemini) | Premium tier | Call audio, transcript text | United States | Third-country transfer — SCCs to be confirmed |
| Cartesia | Enhanced tier (browser-direct) | Call audio | United States | Third-country transfer — SCCs to be confirmed |
| Deepgram | Batch transcription of recordings | Recording audio | United States | Third-country transfer — SCCs to be confirmed |
| Supabase | Database and object storage | Call metadata, transcripts, recordings | EU — Ireland | Intra-EU |
| Railway / GCP | Control plane | Call metadata in transit | EU — Netherlands | Intra-EU |
| Better Stack | Logs | **No call content, no phone numbers** — see §6 | EU ingest, retention unverified | Intra-EU, unconfirmed |

"To be confirmed" means exactly that. `docs/gdpr-readiness-2026-09.md` already flags that
none of these transfer bases has been verified, and this feature does not change that.

## 6. What never leaves in logs

Spec 0111 R23. A telephone number is low-entropy — a national number space is
brute-forceable in seconds — so an unkeyed hash of one is not pseudonymisation. The GDPR
readiness review says the same about the existing MD5 translation-cache keys.

- Numbers reach logs, metrics labels and analytics **only** as a keyed HMAC-SHA256
  pseudonym, truncated to 16 hex characters (`telephony::e164::E164::pseudonym`). The key
  is `VOIP_PSEUDONYM_KEY`, server-side only, falling back to `JWT_SECRET` so a missing
  value cannot silently disable pseudonymisation.
- `E164`'s `Display` implementation is the **masked** form (`+86••••••8000`), not the real
  number, so a stray `{}` in a log line cannot leak one. The real value requires an
  explicit `.as_str()`.
- The full number **is** stored in `voip_calls.recipient_e164`. That is deliberate: a sales
  rep must be able to see who they called and the provider CDR is keyed on it. R23 is a
  rule about logs and observability, not about the record the customer paid for.
- Transcript and recording content never enters logs.

## 7. Retention and erasure

A VoIP call is a `call_sessions` row with `kind = 'phone'`, so it is already covered by the
existing retention sweep and the GDPR erasure path — no separate mechanism, and no
separate thing to forget. `voip_calls` is `ON DELETE CASCADE` from `call_sessions`, so
erasing a session takes the telephony record (including the phone number) with it rather
than leaving an orphan.

Recording retention is per-org (`voip_org_settings.recording_retention_days`), falling back
to the org's general retention setting. Deletion must propagate to provider-side storage;
that propagation is part of the recording work and is not complete until it is tested.

## 8. Open items

1. **Telnyx DPA** — not executed. Required before any production traffic.
2. **Transfer bases (SCCs) for the AI subprocessors** — unverified, inherited from
   `docs/gdpr-readiness-2026-09.md` §2.
3. **Qwen Frankfurt migration** — blocked on an Alibaba support ticket. Code is ready.
4. **Groq EU route** — no known EU region. This is the harder half of the EU-only problem
   and it currently has no plan.
5. **Better Stack ingest region and retention** — flagged as unverified in the readiness
   review; unchanged.
6. **EU endpoint resource parity** — Telnyx's documentation does not enumerate which
   resources are available on `api.telnyx.eu`. Must be probed against a live account before
   go-live.
7. **Legal review of the recipient announcement wording**, per jurisdiction.

## 9. What may and may not be said to a customer

Following the rule already established in `docs/gdpr-readiness-2026-09.md` §"positioning":

**May be said**, because it is verifiable in the code:

- Telephony, SIP signalling and RTP media are handled by Telnyx in Frankfurt.
- Call metadata, transcripts and recordings are stored in the EU (Ireland).
- The control plane runs in the EU (Netherlands).
- Phone numbers never appear in logs or metrics.
- Recording and transcription are disclosed to the telephone participant in their own
  language before anything is captured.

**Must not be said:**

- "GDPR compliant." Servers in Europe are not compliance.
- "Your call data never leaves the EU." It does — for translation, on every tier.
- "EU-only processing is available." It is implemented, and it currently refuses every
  call, which is not the same as being available.
- Anything about mainland China route quality. See `docs/voip-china-validation.md`.

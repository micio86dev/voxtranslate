# Live Connect — privacy and AI Act review

**Status (2026-11-20): engineer-prepared gap analysis. Not legal advice, and no lawyer sign-off.**
Same footing as `legal-review-235.md`, `gdpr-readiness-2026-09.md` and `eu-ai-act-compliance.md`: it records what the
feature does with personal data, which obligations that creates, what is already done, and what is left. The owner decided
(2026-06-17) to publish at their own risk until revenue allows a Spain/EU lawyer; this review is meant to make that risk
smaller and visible, not to remove it. Jurisdiction assumed: **Spain / EU** (controller: Alessandro Micelli, see the privacy policy).

## 1. What Live Connect does with personal data

| Data | Whose | Where it goes | Kept? |
|---|---|---|---|
| Voice (live audio) | host, guests, desk visitors | Standard: our server → Alibaba Cloud Qwen (Singapore). Premium: our server → Google Gemini. Enhanced: **the phone itself → Cartesia (US)**, not through our servers | Never stored by us (no raw audio persisted) |
| Transcript text (original + translation) | everyone who speaks | Groq (translation, Enhanced and captions), our DB | B2C: only if the host switches "save transcript" on. B2B desks: always, for the org's retention (default 90 days) |
| Guest display name, language, consent timestamp | guests / desk visitors | our DB (`live_participants`) | With the conversation; scrubbed on erasure |
| Host account data, wallet balance, spend | host | our DB | As the account |
| Voice sample / cloned voice identifier | **host only** | Cartesia (US) holds the synthetic voice; its identifier is in `users.cartesia_voice_id`; the identifier is sent to the **guests' phones** for conversations where the host opted in | Until the host deletes the account (clone not deleted by the feature itself) |
| IP address, user agent | everyone | rate limiting, logs, Cloudflare/Railway/Vercel edges | Operational logs |

## 2. Roles

- **B2C (host with an account):** VoxTranslate is the **controller** for the host's and the guests' data in the conversation. The host is a user of the
  service, not a controller of the guests' data (the household exemption applies to the host's own use, not to us).
- **B2B desk / organisation conversations:** the organisation is the **controller** of the visitor's conversation; VoxTranslate is the **processor**
  (Art. 28) for the content, and controller for its own account/billing data. A **data processing agreement** must exist in the terms for business plans.
  The business must tell visitors about the processing (Art. 13); our desk screen shows the notice and records the visitor's acknowledgement, but the
  organisation remains responsible for its own signage and legal basis.

## 3. Legal basis, transparency and what the product does today

| Topic | Position | In the product |
|---|---|---|
| Live translation of a guest's voice | Performance of the service the host/org requested + legitimate interest of the guest in being understood; an explicit **acknowledgement** is still collected | Guest ticks "I understand my voice is processed…" before the mic opens (`liveConsent`), timestamp stored (`consent_at`) |
| Transcript retention (B2C) | Only on the host's opt-in; the **guest is a third party to that choice** | Notice says the inviter *can* keep a text transcript; it does **not** say whether they did (see gap G3) |
| Transcript retention (desk) | Organisation's legal basis (usually legitimate interest / contract) | Desk notice + consent line state the business keeps a text transcript |
| Voice clone (host) | **Explicit consent (Art. 6(1)(a) and, to be safe, Art. 9(2)(a))**: a cloned voice can function as a voiceprint | Separate checkbox with its own sentence, required, per conversation; clone is never used unless ticked |
| AI-generated content (AI Act art. 50) | Everyone who hears AI output must be told; a voice that imitates a real person must be disclosed | `aiGeneratedNotice` now shown on the guest join page, the desk page and inside every conversation (1.64); guests see "the host chose an imitation of their own voice" in the room |
| Minors | The service is 18+ (privacy policy §9) | Guests are not age-gated (gap G5) |
| International transfers | SCCs / safeguards named generally in the privacy policy | Singapore (Qwen), US (Cartesia, Groq, Google) — keep transfer impact assessments on file (gap G6) |

## 4. Gaps, ordered by risk

**G1 — Privacy policy does not describe Live Connect (high; text change, no code).** The published policy (`directus/legal/privacy.*.md`) covers calls and webinars. It
should add: guests without an account; that the audio of Live Connect is never recorded; desk conversations and the organisation as controller; default 90-day retention for
organisation conversations and opt-in transcripts for personal ones; the Enhanced tier sending audio **directly from the phone to Cartesia**; how a guest asks for erasure
(see G8). Draft insert in §6.

**G2 — Voice clone and guests (high).** Done: per-conversation opt-in, explicit consent sentence, guest disclosure line, no clone for guests. Remaining: (a) the guest voice line
appears **in the room**, i.e. after the guest has accepted the join notice but before the host speaks; moving it into the join notice (the join response already carries the flag) is
cleaner; (b) no DPIA exists for voiceprints (gdpr-readiness item 7) — outline in §5; (c) the account's clone is not deleted when the account is, nor on request (needs a Cartesia
delete call); (d) a stored cloned voice loses the real-time exemption noted in `eu-ai-act-compliance.md` §5.

**G3 — The guest is not told whether a transcript is being kept (medium).** `liveJoinNotice` says the inviter "can choose". The server knows (`save_transcript`). The join
screen should say "this host IS saving a transcript" / "is NOT saving". Needs a join-response field and one new string in all locales (not done: translation cost).

**G4 — Cartesia in the notice (medium).** On Enhanced the guest's voice goes from their own phone to Cartesia. The generic "sent to be translated" is accurate but the recipient is
not named; the privacy link added to the join page now leads to the processor list, which names Cartesia. Consider naming the tier's provider in the notice for Enhanced.

**G5 — No age confirmation for guests (medium).** Service is 18+; guests tick a consent box only. Either add "I am 18 or older" to the join consent or document that the host
is responsible for who they invite.

**G6 — Transfers (medium).** Keep a short transfer impact assessment per non-adequate recipient (Alibaba Cloud Singapore, Cartesia US, Groq US, Google) next to the processor
list, with the contractual safeguard each relies on.

**G7 — Desk signage and DPA (medium, B2B).** The printable QR should carry a one-line privacy notice so the visitor is informed **before** scanning (Art. 13 timing); the business
terms must contain the Art. 28 clauses. Staff who answer are also monitored data subjects (employment-law rules of the customer's country).

**G8 — Guest data-subject requests (medium).** A guest has no account. Today erasure is performed by the host or an organisation admin (`DELETE …/live/sessions/{id}`) and by retention.
Add to the policy: how a guest identifies the conversation (name + time + link) and that requests go to privacy@voxtranslate.app; consider showing a short conversation reference
on the end screen to make that practical.

**G9 — Special-category content (medium, B2B).** A hotel, clinic or reception desk will hear health or other special-category data. The organisation must minimise retention and
choose its own legal basis; our default 90 days is a ceiling to lower, not a recommendation to keep.

**G10 — Records (low, paperwork).** Add Live Connect to the Art. 30 record of processing and to the sub-processor list with change-notice terms.

## 5. DPIA outline (voice cloning + live conversation translation)

1. **Purpose and necessity:** real-time translation between people who share no language; voice clone only to make the translation sound like the host (optional).
2. **Data and flows:** §1 table. Biometric-capable data: the voice sample and the synthetic voice at Cartesia.
3. **Risks to people:** impersonation of the host's voice; leak of the identifier; guests recorded by a host without knowing (mitigated: no audio is recorded; transcript opt-in; G3);
   special-category speech at desks; children as guests; transfers outside the EEA.
4. **Measures:** explicit per-conversation opt-in; identifier only given to phones in that conversation; rate limits and one-hour tokens; no raw audio stored; erasure paths;
   retention caps; kill switches (`LIVE_CONNECT_ENABLED`, `LIVE_ECHO_FILTER`); processor contracts.
5. **Residual risk:** medium for voiceprints until G2(c) (clone deletion) and a retention limit for unused clones exist; low for the translation itself.
6. **Decision and review date:** owner to sign; review at the first incident, at any new provider, and at least yearly.

## 6. Suggested wording (English; translate with the same care as the rest)

- **Guest join notice (replace `liveJoinNotice`):** "Your voice is sent to translation services in real time and is never recorded. The host {chose|did not choose} to keep a
  text transcript of this conversation. Translations and any translated voice are AI-generated and may contain errors. [Privacy]"
- **Enhanced addition:** "On this quality level your voice also goes directly from your phone to our voice provider, Cartesia (United States)."
- **Voice clone line for guests (before joining):** "The host chose to have the translations spoken in an imitation of their own voice. It is synthetic."
- **Host clone consent (keep, add retention):** "I agree that a sample of my voice is used to create a synthetic voice that speaks the translations on my guests' phones, for this
  conversation only. My guests' phones receive the identifier of that voice. I can delete my synthetic voice at any time."
- **Privacy policy insert (Live Connect):** "Live Connect lets you talk with up to three other people in different languages. Guests do not need an account. We translate speech in real
  time through our providers and never store the audio. Text transcripts are stored only when the person who started the conversation chooses so, or — for business desks — according to the
  business's retention settings (90 days by default), and can be erased by that person or the business. On the Enhanced level, audio is sent directly from your phone to Cartesia."

## 7. What a lawyer would still have to confirm

Whether a cloned voice used only for synthesis is Art. 9 biometric data (we assume yes, conservatively); that the "acknowledgement" model is enough for guests where consent is not
the legal basis; the Art. 28 clauses in the business terms; the interplay of art. 50(2)/(4) AI Act with the real-time exemption relied on elsewhere; and national rules on
recording or monitoring conversations in the customer's country.

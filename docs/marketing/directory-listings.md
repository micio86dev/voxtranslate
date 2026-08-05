# Directory listing copy — G2, Capterra, Product Hunt, Chrome Web Store

Submission-ready copy. Everything here is checked against the shipped product; nothing is
aspirational. Paste and submit — **account creation and publication are the owner's**, since
each of these binds the company to a third party's terms and puts a public profile in its name.

Written 2026-08-05 against server v1.36.4 / website v0.3.1 / extension v0.1.3.

---

## Accuracy rules these were written under

`voxtranslate-chrome-extension/docs/store/release-checklist.md` forbids overselling, and the
same discipline applies to every directory. Concretely:

- **No latency numbers.** None has been measured on the shipped build. Do not add one.
- **Per-tier language counts, never a blanket "84".** Standard 29 · Enhanced 61 · Premium 84.
  Only Premium covers 84. The homepage headline currently overstates this.
- **"Real-time translation", never "accurate" or "perfect".**
- **Never imply audio is never stored.** Retention follows the platform policy.
- **Chrome only** for the extension.
- **Do not quote the Business/Enterprise price** until the Stripe currency is confirmed —
  the site says €49/€199 while consumer billing is USD. Per-minute rates are safe: they come
  from the committed catalogue.

---

## Positioning (the through-line for all listings)

The category is crowded at both ends: the meeting platforms ship speech translation for a
handful of language pairs, and a dozen bot/extension add-ons compete on price. VoxTranslate's
defensible ground is the middle nobody covers:

1. **Language coverage the platforms don't have.** Google Meet does speech-to-speech on 5
   pairs, Zoom on 5 languages (US accounts, beta), Teams on 9 with Premium/Copilot. Premium
   does 84 languages, 2,000+ combinations, several per room at once.
2. **One-to-many, not just meetings.** Webinar attendees join by link or QR with no account
   and no download, each picking their own language.
3. **Pay per minute of speech, no seats.** Rates from $0.0045/min.
4. **EU posture.** Peer-to-peer WebRTC for calls, and an AI Act compliance record
   (`docs/eu-ai-act-compliance.md`) that almost no competitor in this category can show.
5. **Works inside Meet and Zoom on the web** via the extension, so adopting it does not
   require moving the meeting.

Lead with 1 and 2. They are the two things a buyer cannot get elsewhere.

---

## G2 / Capterra — vendor profile

**Categories:** Language Translation · Video Conferencing · Transcription

**Short description (≤ 160 chars)**

> Real-time translated video calls and webinars in up to 84 languages. Pay per minute of
> speech — no seats, no subscription required.

**Full description**

> VoxTranslate transcribes, translates and speaks every voice in a call or webinar, live.
>
> Each participant picks the language they want to hear and read. A room can carry several
> languages at once, so a multilingual meeting does not become a series of separate ones.
>
> Three engines, chosen per call:
> - **Standard** — speech-to-speech translation with translated voice and live subtitles,
>   29 languages, $0.0045 per minute
> - **Enhanced** — voice cloning so each speaker sounds like themselves, 61 languages;
>   audio streams straight from the browser to the provider and never touches our servers
> - **Premium** — highest-fidelity translation with natural translated voice, all 84 languages
>
> Rates are per minute of speech and per target language. Only the person speaking is
> billed — joining, listening and reading subtitles are free.
>
> **Webinars.** Attendees join by short link or QR code with no account and no download, and
> each chooses their own language for subtitles and spoken translation.
>
> **Inside your existing meetings.** The Chrome extension translates the audio playing in
> your tab, which covers Google Meet and Zoom on the web.
>
> **Beyond the call.** Recording, screen share, whiteboard, diarized transcripts, glossaries
> per room, post-call AI reports and sentiment analysis.
>
> **Built for European teams.** Calls run peer-to-peer over WebRTC. Business and Enterprise
> plans add shared credits, member management, call history, configurable retention, an audit
> log and a compliance mode. VoxTranslate maintains a written EU AI Act compliance record.
>
> Free starter credit, no credit card.

**"Best for"** — Organisations running meetings, training or webinars across languages that
the major platforms do not cover, and teams that would otherwise book an interpreter.

**Seeding reviews** — ask the first design partners once they have run a real session. Do not
solicit before there is something honest to review; both platforms penalise it and it reads
as astroturf.

---

## Product Hunt

Launch **after** the root-domain consolidation (runbook 120 §1). A launch drives a traffic and
backlink spike to whatever the canonical domain is that day, and today that is a subdomain.

**Name:** VoxTranslate
**Tagline (≤ 60 chars):** `Translated video calls and webinars in 84 languages`

**Description**

> VoxTranslate translates video calls and webinars live — transcribing, translating and
> speaking every voice, with each participant choosing their own language.
>
> The meeting platforms added speech translation for a handful of language pairs: Meet does 5,
> Zoom 5, Teams 9. If your pair isn't in that short list, or your room needs three languages at
> once, there was nothing. VoxTranslate does up to 84 languages and 2,000+ combinations, with
> several live in the same room.
>
> Webinar attendees join by link or QR — no account, no download — and pick their own language.
>
> You pay per minute of speech, from $0.0045. No seats, no subscription. Only the speaker is
> billed; listening is free.
>
> Enhanced clones each speaker's voice so they still sound like themselves, and streams audio
> straight from the browser to the provider — it never touches our servers.
>
> There's also a Chrome extension that translates whatever is playing in your tab, so it works
> inside Google Meet and Zoom without moving your meeting.

**First comment (maker)** — say plainly what it is not: not an interpreter replacement for
high-stakes legal or medical work, no measured latency figure published yet, Chrome-only for
the extension. Honest limits earn more trust on PH than a feature list.

---

## Chrome Web Store

Permission justifications and privacy answers are already written — paste from
`voxtranslate-chrome-extension/docs/store/permissions.md` and `docs/store/data-disclosure.md`.
Assets are in `docs/store/screenshots/` and `docs/store/promo/` (committed 2026-08-05).

**Name:** VoxTranslate
**Short description (≤ 132 chars)**

> Real-time translated subtitles and speech for the audio playing in your tab.

**Detailed description**

> VoxTranslate translates the audio playing in your browser tab and gives you live subtitles
> and spoken translation.
>
> Because it works on the tab's audio, it covers Google Meet and Zoom on the web, as well as
> recorded talks, webinars and live streams.
>
> - Live subtitles in your language
> - Spoken translation alongside the original audio, with a volume control for each
> - Pick your language per session
> - Usage and remaining credit shown in the side panel
>
> **Permissions, and why.** VoxTranslate does not request access to your browsing. It uses
> `activeTab`, so clicking the toolbar button grants access to that one tab and nothing else.
> There is no `<all_urls>` permission, no access to tabs, history, cookies or network requests.
>
> Requires a VoxTranslate account. Free starter credit, no credit card.
>
> Chrome only.

**Before uploading:** `docs/manual-testing.md` must be completed on the packaged build. The
ZIP is produced by `bun run package` and is gitignored by design — build it fresh at
submission time so the version in the store matches the tag.

---

## Ordering

1. Chrome Web Store — not blocked by anything outstanding. Under the recommended same-origin
   routing (runbook 120 §1, Option B), `appOrigin` stays `https://voxtranslate.app`, so the
   baked host permissions stay valid. **Only Option A would force a resubmission.**
2. G2 and Capterra — free, and where B2B buyers compare. They also feed the "best X 2026"
   listicles that currently rank for this category without mentioning VoxTranslate.
3. Product Hunt — last, after the domain work, so the spike lands on the canonical origin.

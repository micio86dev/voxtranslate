# EU AI Act — compliance record

**Status: transparency obligations implemented (2026-08-04). Not legal advice, and no
lawyer sign-off.** Same footing as `legal-review-235.md`: this is a gap analysis plus a
record of the choices made, written by an engineer. A qualified EU lawyer should review
the reliance on the real-time exemption in §5 before it is ever tested by a regulator.

Reference: Regulation (EU) 2024/1689 (AI Act), as amended by Regulation (EU) 2026/1744
(Digital Omnibus, in force 27 July 2026).

## 1. Why this document exists

Article 50 transparency obligations became applicable on **2 August 2026**. They are not
satisfied by having the right behaviour in the product — they are satisfied by having the
right behaviour *and being able to show why it is the right behaviour*. The exemption in
§5 in particular is worth nothing undocumented: it is a decision to rely on a carve-out,
and a decision nobody wrote down looks identical to an oversight.

## 2. Role and classification

| Question | Answer |
|---|---|
| Role | **Provider** of an AI system (VoxTranslate is placed on the market under our own name) and **deployer** of third-party models (Qwen, Groq, Deepgram, Cartesia, OpenAI, Gemini) |
| GPAI model provider? | **No.** We integrate general-purpose models; we do not train, fine-tune or rebrand one. Chapter V obligations do not attach. |
| Prohibited practices (Art. 5)? | **No** — see §4 |
| High-risk (Annex III)? | **No.** Real-time speech translation for general communication is not an Annex III use case. This holds only while the intended purpose stays as declared; marketing the product *for* recruitment, migration/asylum/border procedures, education assessment or access to essential services would move it into Annex III (obligations from 2 December 2027 under the Omnibus timeline). |
| Applicable | **Art. 50** (transparency), **Art. 4** (AI literacy) |

## 3. Where each obligation is met

| Obligation | Surface | Implementation |
|---|---|---|
| Art. 50(1) — tell people they are interacting with AI | Dashboard Help Assistant | `dashboard/src/layouts/BaseLayout.astro` — `#ha-ai-notice`, populated from `helpAssistant.aiDisclosure`, above the fold in the panel, never gated on session state |
| Art. 50(1) | Dashboard Insights + Voice Assistant | `dashboard/src/pages/[lang]/insights.astro`, `…/projects/detail.astro` — server-rendered `voiceAssistant.aiDisclosure`, present before the first interaction |
| Art. 50(1)/(4) — exposure to AI output | In-call UI | `client/src/pages/index.astro` — `.ai-notice` / `aiGeneratedNotice` (all 84 locales) |
| Art. 50(1)/(4) | Webinar viewer | `client/src/pages/w/[code].astro` — `.wv-ai-notice`, static markup so it is on screen from first paint, reusing `aiGeneratedNotice` |
| Art. 50(1)/(4) | Chrome extension | `voxtranslate-chrome-extension/src/sidepanel/App.vue` — shown above the Start button, i.e. before any exposure |
| Art. 50(2) — machine-readable marking | Transcript PDF | `server/src/pdf.rs` + `server/src/templates/transcript.typ` — `keywords` in the PDF metadata (machine-readable) plus a visible italic note and a footer label |
| Art. 50(2) | WebVTT export | `server/src/subtitles.rs` — a standard `NOTE` block before the first cue |
| Art. 50(2) | Live translated audio | **Not marked — exemption relied on, see §5** |
| Art. 4 — AI literacy | Organisational | §7 |

### SRT is deliberately unmarked

SubRip has no comment syntax. Anything we inserted would either render as a subtitle or
break strict parsers. The content is a transcription plus a translation, and translation
is named as *standard editing* in the Code of Practice on marking and labelling — so the
marking duty is doubtful to begin with. Marking WebVTT (where the format supports it) and
skipping SRT (where it does not) is the proportionate reading of "technically feasible".

## 4. Emotion recognition — why we are outside Art. 5(1)(f)

`server/src/ai/sentiment.rs` scores sentiment **from the text of the transcript**, via
Groq, per time window and per speaker. It never touches audio features, voiceprints, or
any other biometric signal.

Art. 5(1)(f) prohibits inferring emotions in the workplace, but the prohibition — through
the Art. 3(39) definition of an *emotion recognition system* — reaches only systems that
infer emotions **from biometric data**. The Commission's guidelines on prohibited
practices state that text-based sentiment analysis falls outside it. Our meetings are
overwhelmingly workplace meetings, so this distinction is the entire basis on which the
feature is lawful.

> **Hard constraint — do not "improve" this feature by adding voice.** Inferring emotion
> from vocal tone, prosody, speech rate, or any audio characteristic converts sentiment
> analysis into a prohibited practice in a workplace context. Penalty ceiling: €35M or 7%
> of worldwide annual turnover (Art. 99(3)) — the highest band in the regulation. If the
> feature ever needs to get better, it gets better on text.

Separately, per-speaker sentiment scoring of participants remains a GDPR profiling
question (transparency, legal basis, and a DPIA if it is ever applied to employees by
their employer). That is a live issue independent of the AI Act.

## 5. Live synthetic audio — exemption relied on

**What we do not do:** we do not watermark or otherwise mark the translated speech
synthesized during a live call, including the voice-cloned output on the Enhanced tier.

**Why we consider that compliant:** the Code of Practice on marking and labelling AI
generated content exempts real-time content that is ephemeral and consumed immediately —
not recorded, not stored, not further disseminated — where marking is not technically
feasible and users are made aware of the AI origin through in-experience or session-level
disclosure. Live translated audio in a WebRTC call is exactly that: generated, played, and
gone. Watermarking it would mean injecting a signal into a stream tuned to ~100 ms
end-to-end latency (100 ms Opus chunks, spec 0043).

**The conditions this depends on.** The exemption evaporates if any of these stop being
true, so they are load-bearing, not background:

1. **Session-level disclosure is present and visible on every surface that plays
   synthesized audio.** This is why the webinar viewer and the extension were fixed —
   before 2026-08-04 they played AI voice with no notice at all, which would have removed
   the basis for the exemption on those surfaces.
2. **The synthesized audio is not recorded or stored by us.** If call recording of
   translated audio is ever shipped, the recording is a stored artifact and must be
   marked — the live exemption does not travel with it.
3. **Voice cloning stays live-only.** A stored clip of a cloned voice is synthetic audio
   resembling a real person: a deepfake under Art. 3(60), with a marking duty on us and a
   disclosure duty on the deployer under Art. 50(4).

**Deadline if the reliance ever fails:** systems on the market before 2 August 2026 — ours
— had until **2 December 2026** to comply with Art. 50(2) marking under the Omnibus
transition. Anything placed on the market after 2 August 2026 gets no transition at all.

## 6. Deployer duties belong to our customers

Under Art. 50(4) the duty to disclose a deepfake sits with the **deployer** — the
organisation running the meeting — not with us. What we owe them is the means to comply,
which the in-call and viewer notices provide. Business customers who enable voice cloning
should be told in onboarding that the disclosure duty is theirs.

## 7. Art. 4 — AI literacy

In force since 2 February 2025. It requires a sufficient level of AI literacy among staff
operating AI systems on our behalf, proportionate to the size of the organisation. For a
team of this size the proportionate measure is that anyone touching the translation,
assistant or `ai/` code paths has read this document and `docs/pricing-standard-qwen.md`,
and understands the §4 constraint. Record the date each person did so; that record is the
evidence.

## 8. Open items

- [ ] Lawyer review of the §5 exemption reliance and the §4 classification
- [ ] Onboarding note for Business customers about their Art. 50(4) deployer duty (§6)
- [ ] Re-check §5 conditions before shipping call recording or stored voice messages
- [ ] Watch for the final Code of Practice on Transparency of AI-Generated Content —
      signing it is voluntary but is the cheapest available evidence of good faith
- [ ] GDPR: DPIA for per-speaker sentiment analysis if it is ever sold as an
      employee-facing feature

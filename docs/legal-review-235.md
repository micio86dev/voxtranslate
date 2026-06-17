# Legal review — Privacy / Terms / Acceptable Use (issue #235)

**Status: structural gaps CLOSED and published (v2026-06-17). Lawyer sign-off
still recommended, not yet done.** This remains a compliance *gap analysis*, not
legal advice.

**Owner decision (2026-06-17):** proceed and publish at own risk now — no budget
for counsel yet; a qualified Spain/EU lawyer review will be commissioned "once the
first revenue comes in". On that explicit instruction the "⚠️ Draft template"
disclaimer was **removed** from all 24 documents and the controller identity was
supplied (see §A). Everything an author can responsibly do without a lawyer is
done; the residual risk is the absence of formal legal sign-off, which only a
lawyer can clear.

Source of truth: `directus/legal/{privacy,terms,acceptable-use}.<lang>.md`
(8 languages each), regenerated into `directus/seed-content.sql` via
`gen-content-seed.mjs` and served via `/api/content/legal/...`. Jurisdiction:
**Spain / EU** — controller **Alessandro Micelli**, Puerto del Rosario, Spain.

## Resolution log (what was applied in all 8 languages)

- **Removed** the draft-template disclaimer from every document.
- **Controller identity** (Privacy §1): Alessandro Micelli, Puerto del Rosario,
  Spain; privacy contact privacy@voxtranslate.app.
- **Corrected factual accuracy** (the previous text mis-described processing):
  transcripts of calls with a signed-in participant **are stored** (history /
  PDF-JSON export / AI-correction; deleted on account deletion) — Privacy §2/§5;
  product **analytics** disclosed (§2/§3); **chat file attachments** disclosed.
- **Completed the processor list** (Privacy §4): added **OpenAI** (Premium
  real-time translation), **Cloudflare** (edge + TURN relay), **Resend** (email),
  **Better Stack** (operational logs) alongside the existing providers.
- **Cookies / local-storage section** added (Privacy §6): only the session token,
  cookie-consent flag, and minor UI flags — no advertising / cross-site tracking.
- **Supervisory authority** named: **AEPD** as lead, plus the user's own-country
  authority (Privacy §7).
- **Terms**: new **IP & Your-Content** licence section (§5), **Governing law &
  dispute resolution** = Spain + EU-consumer carve-out + ODR link (§10), and an
  AI-output "as is / not for high-stakes" limitation (§9).
- **AUP**: explicit **AI-misuse** prohibition added (§2).

The original gap analysis is preserved below for the lawyer's eventual review.

---

## A. Information the owner MUST provide (hard blockers)

Until these are supplied, the documents cannot be finalized or the disclaimer
removed:

1. **Registered legal entity name** (company *or* sole trader / autónomo name).
2. **Registered address** in Spain.
3. **VAT / NIF / CIF number** (if VAT-registered).
4. **Privacy contact** — confirm `privacy@voxtranslate.app` is monitored; name a
   **DPO** if one is appointed (a DPO is generally not mandatory for a small SaaS,
   but the contact must be real).
5. **Support/legal contact** — confirm `support@voxtranslate.app` is monitored.
6. Confirmation that an **EU representative** is **not** required (correct if the
   controller is established in Spain/EU — then no Art. 27 rep is needed).
7. Sign-off that the **processor list** (§4 Privacy) is current, with each
   provider's region + transfer safeguard confirmed.

## B. Concrete drafting gaps a lawyer should close

### Privacy Policy
- **§1 Controller** still has the placeholder *"Replace with your registered legal
  entity name, address…"* → fill from A1–A4.
- **No dedicated Cookies / local-storage section.** The app shows a cookie banner
  and stores a JWT + consent in `localStorage`. Add a short section disclosing:
  the auth/session token, the cookie-consent flag, and that no third-party
  advertising/tracking cookies are used (verify this is true). Required for
  ePrivacy/GDPR transparency.
- **Supervisory authority**: §6 says "your local data protection authority" — for a
  Spain-based controller, name the **AEPD** (Agencia Española de Protección de
  Datos) as the lead authority while preserving the user's right to their own.
- **AI processing transparency**: §3 covers audio→captions; consider an explicit
  line that translation/transcription is automated (AI) and may be inaccurate, and
  that transcripts/AI outputs are processed transiently (consistent with §5).

### Terms of Service
- **Missing Governing Law & Jurisdiction clause** (the issue explicitly requires
  this for Spain/EU). Add a clause naming **Spanish law** and the competent courts,
  **without overriding mandatory EU consumer protections** (a consumer can still
  sue/be sued in their country of residence under the Brussels I bis Regulation).
- **Missing IP / ownership clauses**: state (a) VoxTranslate owns the platform/IP,
  and (b) users retain ownership of **their** content (audio, transcripts, uploads)
  and grant only the limited licence needed to operate the Service.
- **AI-generated content**: §8 mentions "translation inaccuracies" — make explicit
  that AI translations/reports are provided "as is" and must not be relied on for
  high-stakes (legal/medical) use.
- Insert the **registered entity name** wherever the Terms say "we"/"VoxTranslate"
  for the binding party.

### Acceptable Use Policy
- Reasonably complete (respect, illegal/harmful, spam, recording-consent,
  enforcement). Lawyer should confirm explicit coverage of **impersonation**,
  **unauthorized access / security abuse**, and **AI misuse** (e.g. generating
  illegal content via translation) — currently implied, not itemized.

## C. What is already in good shape (no change expected)

- GDPR legal bases per processing purpose (§3 Privacy).
- International transfers via EU **Standard Contractual Clauses** (§4).
- Retention model (real-time media not stored; billing kept per tax law) (§5).
- Data-subject rights wired to **in-app "Download my data" / "Delete my account"**
  (§6) — a strong, verifiable implementation.
- Security posture incl. P2P media not transiting servers (§7).
- 18+ / children clause (§8); change-notification clause (§9).
- Consumer-rights carve-outs in the liability clause (Terms §8).

## D. Process to ship

1. Owner supplies §A. 2. Apply §B edits to **all 8 language files** per document.
3. Qualified lawyer (Spain/EU consumer + data-protection) reviews. 4. On sign-off,
remove the draft disclaimer line from each `*.md` and re-seed via the Directus
legal flow. 5. Pin the reviewed version/date.

> Re-stating the obligation: do **not** remove the disclaimer or represent these as
> final before steps 1–4. Marking a draft "reviewed" without a lawyer would itself
> be a compliance risk.

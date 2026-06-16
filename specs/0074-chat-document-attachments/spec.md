# 0074 — Chat document attachments: documents-only, 5 MB, docx + pay-to-translate

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-16 |
| **Shipped** | 2026-06-16 |
| **Version** | — |
| **Commits** | `aa4fe27` (PR #171), `1d5afe7` (PR #172) |
| **Depends on** | [0018](../0018-chat-file-upload/spec.md) (chat file upload), [0005](../0005-accounts-credits-billing/spec.md) (credits/billing), [0073](../0073-connection-status-banner/spec.md) (—, sequencing only) |

## 1. Context & Problem

Chat file upload (spec 0018) accepted **mp3/wav/txt/pdf** up to **25 MB** and, for every
upload, extracted text and ran the **Groq** translation fan-out — **without charging any
credits**. Three problems surfaced in use:

1. **Wrong scope.** Attachments should be *documents*, not audio clips (the audio path
   also pulled in a Deepgram prerecorded-STT call).
2. **Too large.** 25 MB is excessive for chat documents.
3. **Money leak.** Uploaded-document translation hit Groq for free — a real cost with no
   revenue, against a thin-margin product. (The 0018 doc-comment even noted "guests too,
   no JWT", i.e. anyone could trigger paid AI for free.)

This spec narrows the feature to documents, caps it at 5 MB, broadens the *document*
formats (incl. docx with real text extraction), and makes AI translation a **paid**
action that can never run at a loss.

## 2. Goals / Non-Goals

**Goals**
- Restrict uploads to **documents** (no audio) with a **5 MB** cap, client + server.
- Support a broad set of known document formats; **extract + translate** the ones we can
  (txt-family, pdf, docx), **store-only** the rest (doc/odt/rtf/xlsx/pptx).
- **Charge credits** for the AI translation of an upload, with a configurable minimal
  margin — and **never lose money**: if it isn't paid, Groq is simply not called.

**Non-Goals**
- No OCR (image/scanned PDFs), no audio transcription from uploads (removed).
- No text extraction for the store-only formats (they post as plain attachments).
- No pre-upload cost quote / paywall UI (v1 nudges *after* the upload).
- No change to the chat render / transcript / signed-URL plumbing (reused from 0018).

## 3. Requirements

- **R1 — Documents only, 5 MB.** *Given* the chat attach picker, *then* it accepts only
  the document allowlist and rejects anything else (415) and anything > 5 MB (413), on
  **both** client (pre-check) and server (authoritative).
- **R2 — Broad formats, translate-where-possible.** *Given* an allowed upload, *when* it
  is `txt/md/csv/log/pdf/docx`, *then* its text is extracted and (if paid) translated;
  *when* it is `doc/odt/rtf/xlsx/pptx`, *then* it is stored + shared as an attachment with
  **no extraction and no Groq call**.
- **R3 — docx extraction.** *Given* a `.docx`, *then* the server pulls the body text from
  the zipped `word/document.xml` (`<w:t>` runs), bounded against zip bombs and run
  off-thread with a timeout; failure degrades to an empty body (file still posts).
- **R4 — Pay-to-translate.** *Given* a monetized server and an extractable upload with
  target languages, *then* Groq runs **only** for a signed-in user with sufficient
  credits, who is charged `base + per_lang × targets`; *given* a guest **or** an
  out-of-credits user **or** a store-only format, *then* the file still posts as an
  attachment but is **not** translated and **no Groq call** is made.
- **R5 — Never lose money / tunable.** *Given* any unpaid path, *then* there is zero AI
  cost. *Given* the pricing needs tuning, *then* it is changeable via env/Secrets without
  a code change.
- **R6 — Feedback.** *Given* an upload that wasn't translated for a payment reason, *then*
  the uploader's client shows a localized nudge ("file shared — sign in / add credits to
  translate"), in all 8 UI languages.

## 4. Design & Architecture

- **Allowlist + kinds** (`server/src/files.rs`): `classify_ext` →
  `FileKind::{Text, Pdf, Docx, Other}`. `content_type_for(ext)` gives the canonical MIME
  per extension (client labels ignored). Client mirror: `UPLOAD_EXTS` /
  `UPLOAD_MAX_BYTES = 5 MiB` in `client/src/scripts/api.ts`; `<input accept>` + the
  `checkUploadFile` pre-check; server `SUPABASE_MAX_UPLOAD_BYTES` default 5 MiB; route body
  limit 32→8 MiB.
- **docx extraction** (`docx_extract`): `zip` opens the archive, `word/document.xml` is
  read with a 64 MiB decompressed cap, then scanned for `<w:t>…</w:t>` runs (each
  unescaped via `quick_xml::escape::unescape`, `</w:p>` → newline). Runs in
  `spawn_blocking` + 15 s timeout, exactly like the existing PDF path.
- **Billing** (R4): after extraction, Groq runs only if `do_translate`:
  - unmonetized server (`state.billing` / `state.config.billing` `None`) → free;
  - monetized → resolve the signed-in uploader from an **optional** `Bearer` JWT
    (`auth::verify_jwt`); `None` → `blocked = "signin"`; else
    `billing::deduct_feature(uid, session?, "upload_translate", cost, …)` →
    `Ok` translate, `InsufficientFunds` → `blocked = "credits"`.
  - `upload_translate_cost(ai, n) = base + per_lang × n` (rounded 6 dp; pure + unit-tested).
- **Pricing** (`AiConfig`): `upload_translate_base` (`CREDITS_UPLOAD_TRANSLATE_BASE`, 0.01)
  + `upload_translate_per_lang` (`CREDITS_UPLOAD_TRANSLATE_PER_LANG`, 0.005), env-tunable.
- **Response + client**: the upload response carries `translated` + `translate_blocked`
  (`"signin"|"credits"|null`); `uploadChatFile` parses it and `app.ts` shows the localized
  nudge. The translated message itself still arrives over the WS as a normal `chat_message`.
- **Key decisions:**
  - *No Groq call when unpaid* (rather than translate-then-bill) → the "never lose money"
    guarantee is structural, not a reconciliation.
  - *Store-only formats* reuse the same "empty text → skip fan-out" path → broad format
    support with zero new parsers and zero translation cost.
  - *Optional bearer auth* (not a hard gate) → guests keep the file-sharing utility (0018)
    while only the *AI* part is paywalled.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 (PR #171) | 5 MB cap, documents-only allowlist + kinds, docx extraction, store-only skips fan-out, client allowlist/accept/tests | `server/src/{files,config}.rs`, `server/.env.example`, `client/src/scripts/api.ts`, `client/src/pages/index.astro`, `client/src/scripts/upload.test.ts`, `server/Cargo.toml` (+`zip`,`quick-xml`) |
| S1 (PR #172) | `upload_translate` pricing + `upload_translate_cost`, pay-to-translate gate in `upload_file`, `translated`/`translate_blocked` response, client nudge + i18n ×8 | `server/src/{files,config}.rs`, `server/.env.example`, `client/src/scripts/{api,app,i18n}.ts` |

## 6. Testing & Verification

- **Server unit:** `classify_ext` (extractable vs store-only vs rejected, incl. audio→None),
  `content_type_for`, `docx_extract` (in-memory docx incl. an `&amp;` entity; non-zip →
  empty), `upload_translate_cost` (base + per-lang), `config_env` default 5 MiB. clippy
  `--all-targets` clean.
- **Server integration** (`tests/integration.rs`): the upload happy-path + 415/413
  rejections still pass under the new rules.
- **Client:** `checkUploadFile` allow/reject/size tests updated (audio now rejected);
  `astro check` 0 errors; build; 195 unit tests.
- **Prod:** auto-deployed via CI `deploy-server` (Railway) + Vercel; `/health` 200.

## 7. Deployment & Operations

- Server change → **auto-deploys on merge to `main`** via the CI `deploy-server` job (the
  `RAILWAY_TOKEN` secret is now set; see [[deploy-prod-gotchas]]). Client auto-deploys
  (Vercel).
- **Tuning prices** without a deploy-from-code: set `CREDITS_UPLOAD_TRANSLATE_BASE` /
  `CREDITS_UPLOAD_TRANSLATE_PER_LANG` on Railway → `railway redeploy -y`. Absent = defaults
  (0.01 / 0.005).
- No DB migration. New deps: `zip`, `quick-xml` (server).

## 8. Risks / Open Items

- Store-only formats (doc/odt/rtf/xlsx/pptx) aren't translated; a future iteration could
  add odt/xlsx/pptx extraction (also zip+XML) to widen the paid path.
- docx extraction is `<w:t>`-run based (no tables/headers/footnotes structure) — fine for
  translation, not a faithful render.
- Pricing margin is set blind (Groq 8B cost ≈ $0.0002/doc, charge ≈ $0.025) — revisit with
  real usage; env-tunable by design.
- Guests get no translation; if that proves too restrictive, a per-room free quota could be
  layered on later.

## 9. References

- Commits: `aa4fe27` (PR #171), `1d5afe7` (PR #172).
- Files: `server/src/files.rs`, `server/src/config.rs`, `client/src/scripts/{api,app,i18n}.ts`.
- Related: [0018](../0018-chat-file-upload/spec.md) (base upload), [0005](../0005-accounts-credits-billing/spec.md) (`deduct_feature`).

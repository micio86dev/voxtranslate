# 0018 — Chat File Upload (Supabase Storage)

| | |
|---|---|
| **Status** | ✅ Shipped & live (Supabase `chat-files` bucket provisioned) |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | `d04604a` (PR #10, closes #2) |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0005](../0005-accounts-credits-billing/spec.md), [0009](../0009-session-transcripts/spec.md), [0012](../0012-auto-language-detection/spec.md) |

## 1. Context & Problem

The in-call chat (spec 0002) is text-only: a participant types, the server fans
out a translation into every language in the room, and each peer reads it in
their own language. Issue #2 asks to turn that text chat into a **multimodal**
surface — let a participant drop an audio clip or a document into the chat and
have everyone read its translated content, just like a typed message.

The two ingredients already exist on the server: Deepgram has a prerecorded REST
endpoint (`/v1/listen`, already used for language auto-detect in spec 0012) and
the Groq translation fan-out (spec 0002). What is missing is (a) somewhere to
**store** the uploaded bytes and (b) a **pipeline** that turns a file into the
text the existing chat already knows how to translate, persist, and render.

Issue #2 specifies **Supabase Storage** as the storage layer. The project
already uses Supabase as its Postgres provider (`DATABASE_URL`); this adds the
Storage product (a `chat-files` bucket) alongside it.

## 2. Goals / Non-Goals

**Goals**
- Upload a single file into the room chat: audio (`.mp3`, `.wav`), text (`.txt`),
  or document (`.pdf`).
- Store the bytes in a Supabase Storage bucket (`chat-files`); persist file
  metadata (`file_url`, `file_name`, `file_type`, `size`, room/session link).
- Process by type — audio → Deepgram STT, txt → UTF-8 read, pdf → text
  extraction — then run the **existing** translation fan-out so the extracted
  text reaches every language in the room.
- Render the result as a normal chat bubble carrying a file **attachment** chip
  (name, size, link; inline `<audio>` player for audio) plus the translated
  transcription/text.
- Drag-and-drop **and** a file-picker button, with an upload progress state.
- Works for guests too (chat has no auth wall): authorization is room
  membership, verified against the live room registry — not a JWT.

**Non-Goals (future)**
- Multi-file upload per message; file history browser per chat.
- Long-term retention / lifecycle policies (issue says none initially).
- Async/background job processing (MVP processes inline on the request).
- OCR for scanned PDFs (text-layer PDFs only).
- Image/video files, previews beyond an audio player, role-based file ACLs.
- Client-side direct-to-Supabase signed-URL upload (we proxy through the server,
  which needs the bytes anyway to process them — see Key decisions).

## 3. Requirements

- **R1 — Upload a file into chat.** As a participant, I want to attach a file to
  the chat, so that I can share audio/documents with the room.
  - *Given* I am in a call, *when* I pick or drop a supported file, *then* it
    uploads, and a chat message appears for everyone with a file chip and (when
    extractable) the file's translated text.
- **R2 — Audio is transcribed + translated.** *Given* I upload an `.mp3`/`.wav`,
  *when* the server processes it, *then* the speech is transcribed (Deepgram,
  language auto-detected) and the transcript is fanned out to every language in
  the room; each peer reads it in their own language.
- **R3 — Text/PDF is extracted + translated.** *Given* I upload a `.txt`/`.pdf`,
  *when* the server processes it, *then* its text is extracted and translated to
  every room language.
- **R4 — Metadata persisted.** *Given* the database is configured, *when* a file
  is uploaded, *then* a `chat_files` row records its url, name, type, size and
  session/room, and the chat event is persisted to the transcript (spec 0009).
- **R5 — Validation & limits.** *Given* I pick an unsupported type or an
  oversized file, *when* I try to upload, *then* it is rejected (client-side
  pre-check + server 400/413) and the call is unaffected.
- **R6 — Graceful degradation.** *Given* `SUPABASE_*` is not configured, *when*
  I open the chat, *then* the attach button is hidden and the upload endpoint
  returns `503`; typed chat is unaffected.
- **R7 — Membership gate.** *Given* I am not an active peer of the room, *when* I
  POST to the upload endpoint, *then* it returns `403` (room membership is the
  access control, mirroring typed chat).

## 4. Design & Architecture

- **Components / files:**
  - `server/src/storage.rs` *(new)* — `SupabaseStorage`: upload bytes to the
    (private) `chat-files` bucket via the Storage REST API, then mint a
    time-limited **signed** download URL (`create_signed_url`); pure URL builders
    for tests.
  - `server/src/files.rs` *(new)* — multipart upload handler + the
    type-dispatched processing pipeline; reuses `Translator`, `Deepgram`,
    `pdf`-extract, and broadcasts a `ChatMessage`.
  - `server/src/deepgram.rs` — `transcribe_file` (prerecorded `/v1/listen`) +
    pure `parse_prerecorded_response`.
  - `server/src/config.rs` — `StorageConfig` (`SUPABASE_URL`,
    `SUPABASE_SERVICE_KEY`, `SUPABASE_BUCKET` default `chat-files`).
  - `server/src/db.rs` — `ChatFile` row + `insert_chat_file`.
  - `server/src/protocol.rs` — `Attachment` + optional `attachment` on
    `ChatMessage`.
  - `server/src/rooms.rs` — `peer_snapshot(room, id)` for the membership gate.
  - `server/migrations/006_chat_files.sql` — the `chat_files` table.
  - `client/src/scripts/chat.ts` — render the attachment chip / audio player.
  - `client/src/scripts/api.ts` — `uploadChatFile` (XHR, progress).
  - `client/src/scripts/app.ts`, `pages/index.astro`, `icons.ts`, `i18n.ts` —
    attach button, hidden input, drop zone, progress, strings (8 locales).
- **Data model:** `chat_files(id, session_id → call_sessions, room, sender_peer_id,
  sender_name, file_url, file_name, file_type, size_bytes, created_at)`.
- **Protocol / API:**
  - `POST /api/rooms/{room}/files` — `multipart/form-data` with `peer_id` +
    `file`. 200 `{ ok, url, name, type, size }`; errors 400/403/413/503.
  - `ChatMessage` gains `attachment?: { url, name, content_type, size }`.
- **Sequence (happy path):**
  1. Client pre-validates type+size, POSTs multipart (`peer_id`, `file`) with an
     XHR progress bar.
  2. Server verifies `peer_id` is a live member of `room`; rejects otherwise.
  3. Uploads bytes to `chat-files/{session}/{uuid}.{ext}` (private bucket), then
     mints a signed download URL (TTL `SUPABASE_SIGNED_URL_TTL_SECS`, default 7d).
  4. Inserts `chat_files` (best-effort when DB present).
  5. Extracts text: audio → Deepgram (`detect_language`), txt → UTF-8 (lossy),
     pdf → `pdf_extract` (in `spawn_blocking`). Truncates to a sane cap.
  6. `translate_fanout(text, src, room_langs, glossary)`; records a `Chat`
     transcript event.
  7. Broadcasts `ChatMessage` with the `attachment` + original/translations.
  8. Every client renders the chip + the text in its own language.
- **Key decisions:**
  - *Server-proxied upload, not client signed-URL* — the server must read the
    bytes anyway (to transcribe/extract), the service key never reaches the
    browser, auth/validation/processing stay in one place, and the existing
    server-centric architecture is preserved. Cost: upload bandwidth crosses the
    server; acceptable at MVP file sizes (25 MB cap).
  - *Reuse the chat pipeline* — a file becomes a `ChatMessage` with an
    `attachment`, so translation, transcript persistence, moderation-free
    rendering, and unread tracking all work unchanged. One optional field, no
    new client message type.
  - *Membership gate over JWT* — typed chat works for guests; gating uploads on
    a JWT would break parity. The live room registry is the authority (the peer
    must be connected), the same trust model the WS chat already uses.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `StorageConfig` + `SupabaseStorage` client (upload + URL builders) | `config.rs`, `storage.rs` |
| S1 | `chat_files` migration + `ChatFile`/`insert_chat_file` | `migrations/006_chat_files.sql`, `db.rs` |
| S2 | Deepgram prerecorded transcription | `deepgram.rs` |
| S3 | `Attachment` protocol field + `peer_snapshot` | `protocol.rs`, `rooms.rs` |
| S4 | Upload handler + processing pipeline + route + body limit + deps | `files.rs`, `lib.rs`, `Cargo.toml` |
| S5 | Client: attach button, drop zone, progress, `uploadChatFile`, chip render | `index.astro`, `app.ts`, `api.ts`, `chat.ts`, `icons.ts`, `i18n.ts` |
| S6 | Tests (Rust unit/integration + client vitest) | `*` |

## 6. Testing & Verification

- **Rust unit:** Supabase upload/sign URL builders + signed-URL assembly +
  object-path sanitizer;
  `parse_prerecorded_response` (transcript + detected lang, empty/garbled);
  file-kind routing + extension/size validation; `ChatMessage` serializes
  `attachment` when present and omits it when `None` (pins R2/R3/R5).
- **Rust integration:** `POST /api/rooms/{room}/files` returns 503 when storage
  unconfigured (R6) and 403 when the peer isn't in the room (R7), via the
  in-process `app()` router.
- **Client vitest:** `chat.ts` renders an attachment chip + audio player and the
  translated text; `uploadChatFile` builds the right URL/form and surfaces
  progress; client-side type/size pre-check rejects (R1/R5).
- **Typecheck + build:** `cargo clippy`/`fmt`/`test`, `astro check`, `vitest`.
- **Manual (preview, once Supabase keys set):** upload mp3/wav/txt/pdf in a
  2-language room; confirm transcript/extraction + per-language rendering, audio
  player, progress bar, drag-and-drop, and oversize/type rejection.

## 7. Deployment & Operations

- **Supabase provisioning (required before prod use):**
  1. In the Supabase project, create a **private** Storage bucket named
     `chat-files` (downloads use time-limited signed URLs, so only call
     participants who receive the broadcast can fetch the file).
  2. Set on the server host (Railway): `SUPABASE_URL` (e.g.
     `https://<ref>.supabase.co`), `SUPABASE_SERVICE_KEY` (service-role key), and
     optionally `SUPABASE_BUCKET` (defaults to `chat-files`).
- **Migration:** `006_chat_files.sql` runs automatically on startup (embedded).
- **Body limit:** the upload route raises Axum's body limit to 25 MB; other
  routes keep the default.
- **Feature gate:** with no `SUPABASE_*`, the feature self-disables (button
  hidden, endpoint 503) — safe to deploy before the bucket exists.

## 8. Risks / Open Items

- PDF text quality varies; scanned/image-only PDFs yield no text (no OCR) — the
  chip still posts, with an empty body.
- Service-role key on the server can write the bucket broadly; scope/rotate it
  and keep it server-only (never shipped to the client).
- Large audio transcribed inline can make one request slow; bounded by the 25 MB
  cap. Async job processing is a documented follow-up.
- No retention/cleanup yet — bucket grows unbounded (issue defers this).
- Signed download URLs expire after the TTL (default 7d): a file link in a
  long-persisted transcript eventually 404s. Acceptable for the in-call use
  case; a re-sign-on-demand endpoint gated by room membership is the follow-up
  if permanent access is ever needed.

## 9. References

- Issue: #2
- Files: `server/src/storage.rs`, `server/src/files.rs`,
  `server/migrations/006_chat_files.sql`, `client/src/scripts/chat.ts`
- External: https://supabase.com/docs/reference/api/introduction (Storage REST),
  https://developers.deepgram.com/docs/pre-recorded-audio

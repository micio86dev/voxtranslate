# 0082 — In-call invite link + branded transactional emails

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Micio Dev |
| **Created** | 2026-06-16 |
| **Shipped** | 2026-06-16 |
| **Version** | — |
| **Commits** | `3027b5a` |
| **Depends on** | [0016](../0016-follow-up-email/spec.md) (Resend client), [0022](../0022-guest-public-room-gate/spec.md) (guest join), [0055](../0055-meet-like-session-ui/spec.md) (call toolbar) |

## 1. Context & Problem

A call seats up to 4 (`rooms::MAX_PEERS`). Today the only way to get someone in
is to read them the room code — there's no first-class "invite" affordance, and
nothing surfaces the join link. We want the Google-Meet pattern: a one-tap
shareable link, plus the ability to email that link to specific people you know.

Separately, the only transactional emails we send (follow-up recap, bug-report
notification) ship the model/handler's raw inner HTML with no branding. The owner
wants **every** email to carry a modern, professional VoxTranslate layout (logo +
wordmark) that renders well on Outlook and mobile clients.

## 2. Goals / Non-Goals

**Goals**
- In a call with a free seat, let anyone **copy a join link**; let signed-in users
  **email** that link to addresses they type.
- A warm, non-technical invite email: branded, with a **button *and* the raw link**.
- A shared, Outlook-safe, responsive email shell used by **all** transactional email.
- A shareable link (`?room=CODE`) that prefills the home screen.

**Non-Goals**
- No member directory / contact autocomplete. You can't browse or see other users'
  addresses — you invite people whose email you already know.
- No reveal of whether a typed address belongs to a registered account (no
  enumeration). The invite is sent regardless.
- No calendar invites, no invite-acceptance tracking, no per-invite tokens (the
  room code is the capability, same as reading it aloud).

## 3. Requirements

- **R1 — Copy link.** As any participant (guest or signed-in), *given* the room has
  < 4 people, *when* I open **Invite**, *then* I see the join link and a **Copy**
  button that copies `https://<origin>/?room=<code>`.
- **R2 — Email invite.** As a *signed-in* participant, *when* I type one or more
  addresses and press **Send**, *then* each gets a branded email with a **Join the
  call** button and the link; the reply is a bare `{sent, failed}` count.
- **R3 — Seat gate.** *Given* the room is full (4), *then* the Invite affordance is
  hidden and any open invite panel closes.
- **R4 — Guests can't email.** As a guest, *then* the email block is replaced by a
  hint to sign in; the copy-link row still works. The server 401s a guest POST.
- **R5 — Deep link.** *Given* I open `…/?room=blue-fox`, *then* the home room field
  is prefilled with `blue-fox`.
- **R6 — Branded shell everywhere.** *Then* the invite, follow-up recap, and
  bug-report emails all render inside one branded, responsive, Outlook-safe shell
  (logo image + live-text `VoxTranslate` wordmark, preheader, footer tagline).
- **R7 — Anti-abuse & privacy.** Invite send is rate-limited per user, capped at
  `MAX_INVITE_EMAILS` (5) per call, one email per recipient (no recipient sees the
  others), and the link is built from the server's `app_base_url` — never client input.

## 4. Design & Architecture

- **Components / files:**
  - `server/src/email_template.rs` — `render_html(EmailLayout)`, `render_button`,
    `render_text`, `tagline(lang)`. Table layout, inline styles, MSO conditionals,
    VML bulletproof button, hidden preheader.
  - `server/src/invite.rs` — `sanitize_room`, `prepare_recipients`,
    `build_invite_email(lang, inviter, join_url)` (8-language copy), `MAX_INVITE_EMAILS`.
  - `server/src/api.rs::invite_send` — `POST /api/rooms/{room}/invite`.
  - `email_send` + bug-report handlers retrofitted to wrap inner HTML in the shell.
  - `client/src/scripts/invite.ts` — pure `parseRoomParam`, `buildInviteLink`,
    `validateInviteEmails`; `client/src/scripts/api.ts::sendInvites`.
  - `client/src/pages/index.astro` — Invite toolbar item (Room group) + `#invite-panel`.
  - `client/src/scripts/app.ts` — panel toggle, seat-gating, copy/send handlers,
    `?room=` prefill.
  - `client/src/scripts/i18n.ts` — `invite*` keys in all 8 languages.
- **Config:** new `APP_BASE_URL` (default `https://voxtranslate.app`) → `Config.app_base_url`.
- **API:** `POST /api/rooms/{room}/invite` (AuthUser) `{ emails: string[], lang? }`
  → `200 { sent, failed }` · `400` invalid room/emails · `429` rate-limited ·
  `503` Resend off · `502` all sends failed.
- **Key decisions:**
  - *Inner HTML stays un-wrapped in the DB; wrap only at send time* — drafts stay
    editable and never double-wrap.
  - *Link built server-side from `app_base_url`* — we never put our brand behind a
    client-supplied URL (anti-phishing).
  - *No registration-status leak* — same outcome whether or not the address is a member.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Branded email shell + shared tagline | `email_template.rs` |
| S1 | Invite module: validation + localized copy | `invite.rs` |
| S2 | `invite_send` endpoint + route + `app_base_url` config | `api.rs`, `lib.rs`, `config.rs` |
| S3 | Retrofit recap + bug-report emails to the shell | `api.rs` |
| S4 | Client invite panel, toolbar item, seat-gate, copy/send | `index.astro`, `app.ts`, `api.ts` |
| S5 | Pure helpers + `?room=` prefill + i18n (8 langs) | `invite.ts`, `app.ts`, `i18n.ts` |

## 6. Testing & Verification

- **Rust unit:** `email_template` (wordmark, logo URL, preheader, bulletproof button,
  attribute escaping); `invite` (`sanitize_room`, `prepare_recipients` dedupe/cap,
  localized build, inviter-name escaping, empty-name fallback). 9 tests.
- **Client unit (vitest):** `invite.test.ts` — `parseRoomParam`, `buildInviteLink`,
  `parseInviteEmails`, `validateInviteEmails`. Added to coverage allowlist.
- **Visual:** rendered invite (IT) + recap (EN) at 680px and 375px via headless
  Chromium — header bar, button, fallback link, footer, responsive collapse.
- **Gates:** `cargo fmt`/`clippy --all-targets`/`test`; `astro check`; `npm run build`.

## 7. Deployment & Operations

- Set `APP_BASE_URL=https://voxtranslate.app` in the server env (Railway). Without
  it the default is used. Invite send needs `RESEND_*` (already configured).
- No migration. No new secret.

## 8. Risks / Open Items

- **Spam relay:** mitigated by auth + per-user rate limit + 5-address cap + fixed
  template. The sender's display name is the only free text and is HTML-escaped;
  consider a length/however-spammy check as a follow-up.
- **Outlook fidelity** can't be verified locally (no Litmus); the shell uses the
  standard table + VML + MSO-conditional techniques that render there.

## 9. References

- Files: `server/src/email_template.rs`, `server/src/invite.rs`,
  `client/src/scripts/invite.ts`
- External: Resend `POST /emails`

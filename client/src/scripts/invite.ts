// Pure helpers for the in-call invite panel (spec 0082). Standalone so vitest
// (node environment) can import them without dragging in i18n.ts / app.ts, which
// touch `navigator` and the DOM at module load.

import { validEmail } from './email-utils';

/** Mirror of the server cap (`invite::MAX_INVITE_EMAILS`): a few extra over the
 *  4-seat room for bounces/declines, low enough to deter spam relaying. */
export const MAX_INVITE_EMAILS = 5;

/** Read and validate a `?room=` deep-link code from a URL query string. Returns
 *  the lowercased code, or null when absent/malformed. Mirrors the server's
 *  `sanitize_room` charset so a shared link round-trips losslessly. */
export function parseRoomParam(search: string): string | null {
  const v = new URLSearchParams(search).get('room');
  if (!v) return null;
  const code = v.trim().toLowerCase();
  return /^[a-z0-9_-]{1,64}$/.test(code) ? code : null;
}

/** Build the shareable join link for a room from the current origin. The room
 *  is encoded defensively even though codes are already a safe charset. */
export function buildInviteLink(origin: string, room: string): string {
  return `${origin.replace(/\/+$/, '')}/?room=${encodeURIComponent(room)}`;
}

/** Split a free-text field into individual addresses (comma / semicolon /
 *  whitespace separated) and dedupe case-insensitively, preserving order. */
export function parseInviteEmails(input: string): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const part of input.split(/[\s,;]+/)) {
    const e = part.trim();
    if (!e) continue;
    const key = e.toLowerCase();
    if (!seen.has(key)) {
      seen.add(key);
      out.push(e);
    }
  }
  return out;
}

export interface InviteParse {
  /** Well-formed addresses, ready to send. */
  emails: string[];
  /** Entries that failed the address check — surfaced so the user can fix them. */
  invalid: string[];
}

/** Parse + validate the email field, separating good addresses from typos. */
export function validateInviteEmails(input: string): InviteParse {
  const emails: string[] = [];
  const invalid: string[] = [];
  for (const e of parseInviteEmails(input)) {
    (validEmail(e) ? emails : invalid).push(e);
  }
  return { emails, invalid };
}

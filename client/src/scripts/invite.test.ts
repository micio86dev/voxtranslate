import { describe, expect, it } from 'vitest';
import {
  buildInviteLink,
  MAX_INVITE_EMAILS,
  parseInviteEmails,
  parseRoomParam,
  validateInviteEmails,
} from './invite';

describe('parseRoomParam', () => {
  it('reads and lowercases a valid code', () => {
    expect(parseRoomParam('?room=Blue-Fox_42')).toBe('blue-fox_42');
    expect(parseRoomParam('?lang=it&room=abc')).toBe('abc');
  });
  it('rejects missing or malformed codes', () => {
    expect(parseRoomParam('')).toBeNull();
    expect(parseRoomParam('?foo=bar')).toBeNull();
    expect(parseRoomParam('?room=has space')).toBeNull();
    expect(parseRoomParam('?room=a/b')).toBeNull();
    expect(parseRoomParam(`?room=${'a'.repeat(65)}`)).toBeNull();
  });
});

describe('buildInviteLink', () => {
  it('marks where the link came from, so a guest can be gated on one and not the other', () => {
    // A private invite is a link a guest is welcome to walk straight through; a room
    // browsed on /world is one they must sign in to join (spec 0022). The home screen
    // cannot tell them apart from the room code alone, so the origin travels in the URL.
    expect(buildInviteLink('https://voxtranslate.app', 'blue-fox', 'world')).toBe(
      'https://voxtranslate.app/?room=blue-fox&from=world',
    );
    // Absent by default: every existing caller keeps the plain invite link.
    expect(buildInviteLink('https://voxtranslate.app', 'blue-fox')).not.toContain('from=');
  });

  it('joins origin and room into a deep link', () => {
    expect(buildInviteLink('https://voxtranslate.app', 'blue-fox')).toBe(
      'https://voxtranslate.app/?room=blue-fox',
    );
  });
  it('trims a trailing slash on the origin', () => {
    expect(buildInviteLink('https://voxtranslate.app/', 'x')).toBe(
      'https://voxtranslate.app/?room=x',
    );
  });
});

describe('parseInviteEmails', () => {
  it('splits on comma/semicolon/whitespace and dedupes', () => {
    expect(parseInviteEmails('a@x.com, b@y.com;a@x.com  c@z.com')).toEqual([
      'a@x.com',
      'b@y.com',
      'c@z.com',
    ]);
  });
  it('is empty for blank input', () => {
    expect(parseInviteEmails('   ')).toEqual([]);
  });
});

describe('validateInviteEmails', () => {
  it('separates valid from invalid addresses', () => {
    const { emails, invalid } = validateInviteEmails('good@x.com, nope, also@y.io');
    expect(emails).toEqual(['good@x.com', 'also@y.io']);
    expect(invalid).toEqual(['nope']);
  });
  it('caps mirror the server', () => {
    expect(MAX_INVITE_EMAILS).toBe(5);
  });
});

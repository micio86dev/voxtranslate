// GA4 collect-request regression guard.
//
// Symptom this pins against: GA tracked nothing — no console error, and NO
// network request to `/collect` ever fired, even after we started emitting a
// manual page_view on consent grant.
//
// Root cause: the gtag stub pushed a plain ARRAY (`(...args) => push(args)`)
// instead of the `arguments` object. gtag.js dispatches a queued command ONLY
// when the pushed item is the `arguments` object (exactly how Google's snippet
// works); a plain array is silently dropped, so every consent/config/event —
// including page_view — no-ops and no hit is ever sent.
//
// This is a source-text guard (same approach as csp.test.ts): the behavioural
// path needs gtag.js + a real DOM, but the one thing that silently breaks every
// hit is the stub's push form, so we pin that.

import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

const src = readFileSync(new URL('./analytics.ts', import.meta.url), 'utf8');

describe('GA gtag stub (collect-request regression)', () => {
  it('pushes the `arguments` object, exactly like Google’s snippet', () => {
    expect(src).toMatch(/dataLayer!?\.push\(arguments\)/);
  });

  it('never reverts to pushing a rest-param array (the original bug)', () => {
    // `(...args) => dataLayer.push(args)` pushes an Array → gtag.js ignores it.
    expect(src).not.toMatch(/dataLayer!?\.push\(args\)/);
    expect(src).not.toMatch(/gtag\s*=\s*function\s*\(\s*\.\.\.\s*args/);
  });

  it('still gates on consent: config suppresses the auto page_view', () => {
    // We send page_view explicitly on grant, so config must NOT auto-send one
    // (it would be withheld under denied consent and never replayed).
    expect(src).toMatch(/send_page_view:\s*false/);
    expect(src).toMatch(/gtag\?\.\('event',\s*'page_view'\)/);
  });
});

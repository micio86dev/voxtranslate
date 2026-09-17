// Web-app VoIP dialer (spec: web-app-voip-dialer) — end-to-end proof for the phone entry
// path. Staging's `voip_rates` table is empty, so a real dial cannot be exercised there
// (see the design doc's "Testing Strategy" section) — mocking the quote/dial/detail/hangup
// endpoints with `page.route` is the ONLY way to prove the full flow end-to-end.
//
// This is also the actual automated proof for behavior `client/vitest.config.ts` deliberately
// excludes from unit coverage (app.ts is "exercised by the Playwright e2e suite ... not unit
// tests"): PR2's phone-peer mesh-skip in the `room_joined` handler (`isPhonePeer`), the
// 1.58.5 stale-prejoin regression guard on the phone entry path (R4/R7 — `startCall()` is
// reused unchanged, so a future regression that re-shows `#prejoin` on this path is a real
// bug this test catches), and the `startPhonePoll`/`pollPhoneCall` 1500ms timer wiring; plus
// PR3's CTA/dial-panel DOM glue (`openPhoneDialPanel`, the destination-field debounce, the
// quote render, the submit handler).
import { test, expect } from '@playwright/test';
import type { Page } from '@playwright/test';
import { openPage, closePage, trackConsoleErrors } from './helpers';

function json(body: unknown, status = 200) {
  return { status, contentType: 'application/json', body: JSON.stringify(body) };
}

const ORG_ID = 'org1';
const CALL_ID = 'call1';
// Matches voip/session.rs's `create_phone_peer` id shape: `phone-` + 32 lowercase hex
// (phone-dialer.ts's `isPhonePeer`/`PHONE_PEER_ID` regex, PR1).
const PHONE_PEER_ID = 'phone-0123456789abcdef0123456789abcdef';

const BUSINESS_USER = {
  id: 'u1',
  email: 'a@b.com',
  name: 'Alice',
  avatar_url: null,
  balance: 50,
  consent_given: true,
};

/** Mirrors `billing.spec.ts`'s one-dispatcher-per-page mocking pattern, extended with the
 *  VoIP quote/dial/detail/hangup + business-org endpoints this feature needs. Returns a
 *  counter object so the test can assert the hangup endpoint was actually posted. */
async function mockPhoneDialerApi(page: Page): Promise<{ hangupCalls: number }> {
  const state = { hangupCalls: 0 };
  await page.route('**/gsi/client', (r) => r.abort()); // block the external Google script
  await page.route('**/api/**', (route) => {
    const req = route.request();
    const p = new URL(req.url()).pathname;
    if (p === '/api/auth/config') {
      return route.fulfill(json({ google_client_id: 'test.apps.googleusercontent.com' }));
    }
    if (p === '/api/user/me') return route.fulfill(json(BUSINESS_USER));
    if (p === '/api/business/organizations') {
      return route.fulfill(
        json([{ id: ORG_ID, name: 'Acme', role: 'owner', plan: 'business', subscription_status: 'active' }]),
      );
    }
    if (p === `/api/business/organizations/${ORG_ID}/projects`) return route.fulfill(json([]));
    if (p === `/api/business/organizations/${ORG_ID}/voip/contacts`) {
      // R8: an empty address book degrades the destination field to a plain number input.
      return route.fulfill(json({ contacts: [], page: 1, limit: 8 }));
    }
    if (p === `/api/business/organizations/${ORG_ID}/voip/quote` && req.method() === 'POST') {
      return route.fulfill(
        json({
          destination: '+1***4567',
          country: 'US',
          price_per_minute: '0.02',
          currency: 'USD',
          reserve_credits: 500,
          estimated_minutes: 5,
          balance_credits: 5000,
          engine_id: 'standard',
          recording: false,
          transcription: true,
          consent_policy: 'none',
          disclosure_language: null,
        }),
      );
    }
    if (p === `/api/business/organizations/${ORG_ID}/voip/calls` && req.method() === 'POST') {
      return route.fulfill(
        json(
          {
            call_id: CALL_ID,
            session_id: 'sess1',
            room: 'ph-abc123',
            status: 'created',
            reserved_credits: 500,
            price_per_minute: '0.02',
          },
          201,
        ),
      );
    }
    if (
      p === `/api/business/organizations/${ORG_ID}/voip/calls/${CALL_ID}/hangup` &&
      req.method() === 'POST'
    ) {
      state.hangupCalls += 1;
      return route.fulfill(json({ requested: true }));
    }
    if (p === `/api/business/organizations/${ORG_ID}/voip/calls/${CALL_ID}`) {
      return route.fulfill(
        json({
          id: CALL_ID,
          session_id: 'sess1',
          room: 'ph-abc123',
          status: 'answered', // phaseFromStatus → 'connected' (PR1, ported from the dashboard)
          failure_reason: null,
          direction: 'outbound',
          recipient_e164: '+15551234567',
          recipient_country: 'US',
          source_language: 'en',
          target_language: 'es',
          engine_id: 'standard',
          started_at: '2026-09-17T00:00:00Z',
          ended_at: null,
          duration_seconds: 12,
          credits_consumed: 10,
          quoted_price_per_min: '0.02',
          cost_status: 'pending',
          recording_status: 'none',
          recording_available: false,
          transcription_status: 'none',
          ai_analysis_requested: false,
          consent_status: 'n/a',
          project_id: null,
          contact_id: null,
          contact_name: null,
        }),
      );
    }
    return route.fulfill(json({}, 404));
  });

  // Mock the signaling socket: on connect, hand back a room_joined carrying the telephone
  // leg as a real room peer (design: "the telephone IS a full room peer" — voip/session.rs's
  // create_phone_peer). It never sends a WebRTC offer, so PR2's isPhonePeer() must skip
  // mesh.addPeer for it — a regression there would leave a permanently black/dead tile,
  // not a visible test failure by itself, which is why this test also asserts the peer's
  // presentation cell renders.
  await page.routeWebSocket(/\/ws/, (ws) => {
    ws.onMessage(() => {}); // ignore client control/audio frames
    ws.send(
      JSON.stringify({
        type: 'room_joined',
        peer_id: 'caller',
        public: false,
        peers: [{ id: PHONE_PEER_ID, user_name: 'Phone', lang: 'es', avatar_url: null, user_id: null }],
      }),
    );
  });

  return state;
}

test('one-click dial reaches the call screen without ever showing prejoin, and leaving hangs up the PSTN leg', async ({
  browser,
}) => {
  const t = await openPage(browser);
  const consoleErrors = trackConsoleErrors(t.page);
  const state = await mockPhoneDialerApi(t.page);

  // Pre-seed a logged-in, active-subscription business user (mirrors billing.spec's
  // pattern) so boot() skips the login gate straight to #home.
  await t.page.addInitScript((u) => {
    localStorage.setItem('vox.token', 'fake.jwt');
    localStorage.setItem('vox.user', JSON.stringify(u));
  }, BUSINESS_USER);

  await t.page.goto('/', { waitUntil: 'networkidle' });
  await expect(t.page.locator('#home')).toBeVisible();

  // R1/R10: the CTA appears only for an active-subscription org on a WebRTC-capable
  // browser (canShowPhoneCta, wired by updateWorkspaceLink()).
  await expect(t.page.locator('#phone-cta')).toBeVisible();

  // The actual money/UX invariant this whole feature exists to deliver (R4/R7, design's
  // "1.58.5 stale-prejoin regression guard"): watch #prejoin for the ENTIRE flow below —
  // it must never lose its `hidden` class. A point-in-time check after the fact would miss
  // a transient reveal-then-hide; the MutationObserver catches that too.
  await t.page.evaluate(() => {
    const el = document.getElementById('prejoin')!;
    (window as unknown as { __prejoinRevealed: boolean }).__prejoinRevealed = false;
    new MutationObserver(() => {
      if (!el.classList.contains('hidden')) {
        (window as unknown as { __prejoinRevealed: boolean }).__prejoinRevealed = true;
      }
    }).observe(el, { attributes: true, attributeFilter: ['class'] });
  });

  await t.page.click('#phone-cta-toggle');
  await expect(t.page.locator('#phone-dial-panel')).toBeVisible();

  await t.page.fill('#phone-destination', '+15551234567');
  // Debounced quote (400ms, PR3's refreshPhoneQuote) — wait for the priced quote to render
  // before pressing Call: the design's "pressing Call IS the confirmation", no second
  // confirm step.
  await expect(t.page.locator('#phone-quote')).toBeVisible({ timeout: 5000 });
  await expect(t.page.locator('#phone-call-btn')).toBeEnabled();

  await t.page.click('#phone-call-btn');

  // One-click dial: mic (acquireMicOnly) → quote → dial → enters the SAME startCall() an
  // ordinary room join uses (R4) — no forked call machine, no prejoin.
  await expect(t.page.locator('#call')).toBeVisible({ timeout: 30_000 });
  await expect(t.page.locator('#call')).toHaveAttribute('data-entry', 'phone');

  expect(await t.page.evaluate(() => (window as unknown as { __prejoinRevealed: boolean }).__prejoinRevealed)).toBe(
    false,
  );
  await expect(t.page.locator('#prejoin')).toHaveClass(/hidden/);

  // PR2's isPhonePeer() mesh-skip: the telephone leg still gets a presentation cell
  // (subtitles/speaking/mute all target it), proving room_joined's phone-peer branch ran
  // instead of crashing or silently dropping the peer.
  await expect(t.page.locator(`[data-peer="${PHONE_PEER_ID}"]`)).toBeVisible();

  // PR2's startPhonePoll/pollPhoneCall (1500ms) + PR1's ported phaseFromStatus/announcement:
  // the mocked 'answered' status must reach the aria-live phase announcer.
  await expect(t.page.locator('#phone-status-live')).toHaveText(/./, { timeout: 5000 });

  // R5/R6: leaving posts hangup exactly once (createPhoneLegController's idempotent end()).
  await t.page.click('#btn-leave');
  await expect(t.page.locator('#home')).toBeVisible();
  expect(state.hangupCalls).toBe(1);

  expect(consoleErrors).toEqual([]);

  await closePage(t);
});

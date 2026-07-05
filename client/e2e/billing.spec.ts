// Billing UI e2e. The running backend is guest-only, so we simulate billing
// mode by intercepting every /api/* call with one dispatcher (robust to whatever
// origin PUBLIC_WS_HOST resolves to). Covers: the login gate, the guest
// fallback, the logged-in account bar/balance, and the buy-credits modal.
import { test, expect } from '@playwright/test';
import type { Page } from '@playwright/test';
import { openPage, closePage } from './helpers';

function json(body: unknown, status = 200) {
  return { status, contentType: 'application/json', body: JSON.stringify(body) };
}

interface Mocks {
  configStatus?: number;
  user?: unknown;
  packages?: unknown[];
  history?: unknown[];
}

async function mockApi(page: Page, m: Mocks = {}): Promise<void> {
  await page.route('**/gsi/client', (r) => r.abort()); // block the external Google script
  await page.route('**/api/**', (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === '/api/auth/config') {
      return m.configStatus === 503
        ? route.fulfill(json('off', 503))
        : route.fulfill(json({ google_client_id: 'test.apps.googleusercontent.com' }));
    }
    if (p === '/api/user/me') return route.fulfill(json(m.user ?? {}));
    if (p === '/api/billing/packages') return route.fulfill(json(m.packages ?? []));
    if (p === '/api/billing/history') return route.fulfill(json(m.history ?? []));
    if (p === '/api/usage/sessions') return route.fulfill(json([]));
    return route.fulfill(json({}, 404));
  });
}

test('billing mode shows the login gate; guest continues to home', async ({ browser }) => {
  const t = await openPage(browser);
  await mockApi(t.page);
  await t.page.goto('/', { waitUntil: 'networkidle' });

  await expect(t.page.locator('#login')).toBeVisible();
  await expect(t.page.locator('#home')).toBeHidden();
  await expect(t.page.locator('#guest-btn')).toBeVisible();

  await t.page.click('#guest-btn');
  await expect(t.page.locator('#home')).toBeVisible();
  await expect(t.page.locator('#login')).toBeHidden();
  await expect(t.page.locator('#account-bar')).toBeHidden(); // guests get no account bar

  await closePage(t);
});

test('a logged-in user sees their balance and can open buy-credits', async ({ browser }) => {
  const t = await openPage(browser);
  const user = { id: 'u1', email: 'a@b.com', name: 'Alice', avatar_url: null, balance: 2.5, consent_given: true };
  await mockApi(t.page, {
    user,
    packages: [{ id: 'starter', name: 'Starter', price_usd: 5, credits_usd: 5 }],
    history: [{ amount: 2, kind: 'free_credit', balance_after: 2, description: 'Welcome credits', created_at: '2026-01-01' }],
  });

  // Pre-seed a session so boot() skips the login gate.
  await t.page.addInitScript((u) => {
    localStorage.setItem('vox.token', 'fake.jwt');
    localStorage.setItem('vox.user', JSON.stringify(u));
  }, user);

  await t.page.goto('/', { waitUntil: 'networkidle' });
  await expect(t.page.locator('#home')).toBeVisible();
  await expect(t.page.locator('#account-bar')).toBeVisible();
  await expect(t.page.locator('#account-name')).toHaveText('Alice');
  await expect(t.page.locator('#account-balance')).toHaveText('$2.50');

  await t.page.click('#buy-btn');
  await expect(t.page.locator('#buy-modal')).toBeVisible();
  await expect(t.page.locator('.pkg-name')).toHaveText('Starter');
  await expect(t.page.locator('.pkg-price')).toHaveText('$5.00');
  await t.page.click('#buy-close');
  await expect(t.page.locator('#buy-modal')).toBeHidden();

  // The credit ledger lives in Account → Billing (reached from the avatar menu). The
  // history tab lists the welcome credit; the usage tab is empty (no sessions yet).
  await t.page.click('#account-trigger');
  await t.page.click('[data-acct-nav="billing"]');
  await expect(t.page.locator('#account')).toBeVisible();
  await expect(t.page.locator('#acct-billing .ledger-row')).toHaveCount(1);
  await expect(t.page.locator('#acct-billing .ledger-amount')).toContainText('+$2.00');
  await t.page.click('#tab-usage');
  await expect(t.page.locator('#acct-billing .ledger-empty')).toBeVisible();

  // Back to home, then logout returns to the login gate.
  await t.page.click('#account-back');
  await expect(t.page.locator('#home')).toBeVisible();
  await t.page.click('#account-trigger');
  await t.page.click('#logout-btn');
  await expect(t.page.locator('#login')).toBeVisible();

  await closePage(t);
});

test('a billed call updates the balance pill and surfaces the exhausted modal', async ({ browser }) => {
  const t = await openPage(browser);
  // A Google avatar exercises the <img> path (account bar + self video cell).
  const user = {
    id: 'u1', email: 'a@b.com', name: 'Alice',
    avatar_url: 'https://lh3.googleusercontent.com/a/test', balance: 1.5, consent_given: true,
  };
  await mockApi(t.page, { user, packages: [{ id: 'starter', name: 'Starter', price_usd: 5, credits_usd: 5 }] });
  await t.page.addInitScript((u) => {
    localStorage.setItem('vox.token', 'fake.jwt');
    localStorage.setItem('vox.user', JSON.stringify(u));
  }, user);

  // Mock the signaling socket: greet the peer, then drive the billing frames the
  // server would send while speaking (balance ticks down → low → exhausted).
  await t.page.routeWebSocket(/\/ws/, (ws) => {
    ws.onMessage(() => {}); // ignore client control/audio frames
    ws.send(JSON.stringify({ type: 'room_joined', peer_id: 'u1', peers: [] }));
    // A chat message with an avatar exercises the chat avatar rendering.
    setTimeout(() => ws.send(JSON.stringify({
      type: 'chat_message', sender_id: 'bob', sender_name: 'Bob', sender_lang: 'en',
      sender_avatar: 'https://lh3.googleusercontent.com/a/bob',
      original: 'hi', translations: { en: 'hi', it: 'ciao' }, timestamp: 1,
    })), 200);
    setTimeout(() => ws.send(JSON.stringify({ type: 'balance_update', balance: 1.2 })), 300);
    setTimeout(() => ws.send(JSON.stringify({ type: 'low_balance', balance: 0.3 })), 700);
    setTimeout(() => ws.send(JSON.stringify({ type: 'balance_exhausted' })), 1100);
  });

  await t.page.goto('/', { waitUntil: 'networkidle' });
  await t.page.fill('#room', 'billtest');
  await t.page.click('#enter');
  await t.page.waitForSelector('#prejoin:not(.hidden)');
  await t.page.waitForFunction(() => {
    const v = document.getElementById('preview') as HTMLVideoElement | null;
    return !!(v && v.srcObject && v.videoWidth > 0);
  });
  await t.page.click('#join-btn');
  await t.page.waitForSelector('#call:not(.hidden)');

  // The frames tick the balance down to zero (1.5 → 1.2 → 0.3 → exhausted). The
  // exact transient values race the poll, so assert the end state robustly.
  await expect(t.page.locator('#exhausted-modal')).toBeVisible();
  await expect(t.page.locator('#low-banner')).toBeVisible();
  await expect(t.page.locator('#call-balance')).toHaveClass(/low/);
  await expect(t.page.locator('#call-balance')).toHaveText('$0.00');
  await t.page.click('#exhausted-buy');
  await expect(t.page.locator('#exhausted-modal')).toBeHidden();
  await expect(t.page.locator('#buy-modal')).toBeVisible();

  await closePage(t);
});

test('buying a package redirects to Stripe checkout', async ({ browser }) => {
  const t = await openPage(browser);
  const user = { id: 'u1', email: 'a@b.com', name: 'Alice', avatar_url: null, balance: 1, consent_given: true };
  await mockApi(t.page, { user, packages: [{ id: 'starter', name: 'Starter', price_usd: 5, credits_usd: 5 }] });
  // Checkout returns a (stubbed) hosted URL; serve it so the redirect lands.
  await t.page.route('**/api/billing/checkout', (r) =>
    r.fulfill(json({ url: 'http://localhost:4321/stripe-stub' })),
  );
  await t.page.route('**/stripe-stub', (r) =>
    r.fulfill({ status: 200, contentType: 'text/html', body: '<h1 id="stub">stripe</h1>' }),
  );
  await t.page.addInitScript((u) => {
    localStorage.setItem('vox.token', 'fake.jwt');
    localStorage.setItem('vox.user', JSON.stringify(u));
  }, user);

  await t.page.goto('/', { waitUntil: 'networkidle' });
  await t.page.click('#buy-btn');
  await expect(t.page.locator('.pkg-name')).toHaveText('Starter');
  await t.page.click('.pkg');
  await t.page.waitForURL('**/stripe-stub');
  await expect(t.page.locator('#stub')).toBeVisible();

  await closePage(t);
});

test('the server rejecting a join for low balance returns home with the buy prompt', async ({ browser }) => {
  const t = await openPage(browser);
  const user = { id: 'u1', email: 'a@b.com', name: 'Alice', avatar_url: null, balance: 0.01, consent_given: true };
  await mockApi(t.page, { user, packages: [{ id: 'starter', name: 'Starter', price_usd: 5, credits_usd: 5 }] });
  await t.page.addInitScript((u) => {
    localStorage.setItem('vox.token', 'fake.jwt');
    localStorage.setItem('vox.user', JSON.stringify(u));
  }, user);
  // The signaling socket rejects the join with an insufficient-balance error.
  await t.page.routeWebSocket(/\/ws/, (ws) => {
    ws.onMessage(() => {});
    ws.send(JSON.stringify({ type: 'error', message: 'insufficient balance to join', code: 'insufficient_balance' }));
  });

  await t.page.goto('/', { waitUntil: 'networkidle' });
  await t.page.fill('#room', 'lowbal');
  await t.page.click('#enter');
  await t.page.waitForSelector('#prejoin:not(.hidden)');
  await t.page.waitForFunction(() => {
    const v = document.getElementById('preview') as HTMLVideoElement | null;
    return !!(v && v.srcObject && v.videoWidth > 0);
  });
  await t.page.click('#join-btn');

  // The error returns us to home and opens the buy-credits modal.
  await expect(t.page.locator('#home')).toBeVisible();
  await expect(t.page.locator('#buy-modal')).toBeVisible();
  await expect(t.page.locator('#home-status')).toContainText(/credit/i);

  await closePage(t);
});

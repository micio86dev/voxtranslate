// Live Connect end to end (spec 0121): real browsers, the REAL Rust server, a REAL database,
// and a mock only where a paid provider would be (the Qwen realtime upstream).
//
// Not part of `npm run test:e2e`: it needs the stack `playwright.live.config.ts` starts
// (server on :3101, mock upstream on :3102, Postgres on :55432). Run it with
// `npm run test:e2e:live`. Under the default config the whole file is skipped, so the guest-only
// backend CI uses never sees it.
//
// What is real: the client pages, `/api/live/*`, `/ws/live`, the room and the Standard engine,
// session lifecycle, billing (owner / org wallet), the desk queue, the metrics endpoint.
// What is not: the provider (the mock answers the first audio of each upstream session with a
// transcript that names its direction, e.g. `[it->es]`), the microphone (Chromium's synthetic
// tone) and the loudspeaker (headless output). What is NOT proven here: two real phones, real
// speech, echo, Bluetooth, mobile networks. See docs/live-connect-device-matrix.md.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { test, expect, type Browser, type BrowserContext, type Page } from '@playwright/test';
import { expireInvite, seedOrg, seedUser, sql, storageFor } from './support/live-seed.mjs';

const en: Record<string, string> = JSON.parse(
  readFileSync(fileURLToPath(new URL('../src/scripts/i18n/en.json', import.meta.url)), 'utf8'),
);
const SERVER = () => process.env.LIVE_E2E_SERVER_URL!;
const MOCK = () => process.env.LIVE_E2E_MOCK_URL!;

test.skip(!process.env.LIVE_E2E_SERVER_URL, 'needs the stack of playwright.live.config.ts (npm run test:e2e:live)');

// ---- fixtures -----------------------------------------------------------------------------

/** What the page has seen of the audio path, kept by an init script. See `probe()`. */
interface Probe {
  /** `translated_audio` frames received on `/ws/live`. */
  translatedAudio: number;
  /** Samples handed to the playback AudioWorklet (what would be played). */
  playedSamples: number;
  /** Times the playback worklet reported that audio was actually leaving the graph. */
  playingEdges: number;
  /** The `type` of every control frame received on the Live socket, in order (audio excluded). */
  frames: string[];
}

/**
 * A browser context for one phone. Signed in when `user` is given (the pages read the account
 * from localStorage exactly like a returning user), anonymous otherwise (a guest or a customer).
 *
 * The init script also instruments the audio path without touching the app: it counts
 * `translated_audio` frames on the Live socket, and the samples the playback worklet is given
 * and reports playing. That is how "the other person's audio was played" is asserted.
 */
async function phone(browser: Browser, user?: ReturnType<typeof seedUser>) {
  const ctx: BrowserContext = await browser.newContext({
    permissions: ['microphone'],
    viewport: { width: 420, height: 880 },
    serviceWorkers: 'block',
  });
  await ctx.addInitScript(
    ({ storage }) => {
      try {
        for (const [k, v] of Object.entries(storage)) localStorage.setItem(k, v as string);
        localStorage.setItem('vox_home_tour_seen', '1');
      } catch {
        /* storage blocked: the page then behaves as signed out, which is what a test would see */
      }
      const probe = { translatedAudio: 0, playedSamples: 0, playingEdges: 0, frames: [] as string[] };
      (window as unknown as { __live: typeof probe }).__live = probe;

      const NativeWS = window.WebSocket;
      window.WebSocket = new Proxy(NativeWS, {
        construct(target, args: [string | URL, (string | string[])?]) {
          const ws = new target(...args);
          if (String(args[0]).includes('/ws/live')) {
            ws.addEventListener('message', (ev) => {
              if (typeof ev.data !== 'string') return;
              const type = /^\{"type":"([a-z_]+)"/.exec(ev.data)?.[1];
              if (type === 'translated_audio') probe.translatedAudio += 1;
              else if (type) probe.frames.push(type === 'peer_muted' ? ev.data : type);
            });
          }
          return ws;
        },
      });

      const NativeNode = window.AudioWorkletNode;
      window.AudioWorkletNode = new Proxy(NativeNode, {
        construct(target, args: ConstructorParameters<typeof AudioWorkletNode>) {
          const node = new target(...args);
          if (args[1] === 'pcm-playback-processor') {
            const post = node.port.postMessage.bind(node.port);
            node.port.postMessage = ((msg: unknown, transfer?: Transferable[]) => {
              if (msg instanceof Float32Array) probe.playedSamples += msg.length;
              return post(msg, transfer as Transferable[]);
            }) as typeof node.port.postMessage;
            node.port.addEventListener('message', (e: MessageEvent<{ playing?: boolean }>) => {
              if (e.data?.playing === true) probe.playingEdges += 1;
            });
          }
          return node;
        },
      });
    },
    { storage: user ? storageFor(user) : {} },
  );
  const page = await ctx.newPage();
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
  return { ctx, page, errors };
}

const probe = (page: Page) => page.evaluate(() => (window as unknown as { __live: Probe }).__live);

/** Choose a language in a Live Connect picker (`lh`, `lg`, `ld`, `ls`), as a person would. */
async function pickLanguage(page: Page, prefix: string, english: string) {
  await page.locator(`#${prefix}-lang .lc-lang-trigger`).click();
  await page.locator(`#${prefix}-lang .lc-lang-search input`).fill(english);
  await page.locator(`#${prefix}-lang .lc-lang-opt`).first().click();
}

const room = (page: Page) => page.locator('#lc-room');
const live = (page: Page) => expect(room(page)).toHaveAttribute('data-phase', 'live');

/** Create a session through the real API as `user`; returns what a host's phone would hold. */
async function apiCreate(user: ReturnType<typeof seedUser>, lang = 'it') {
  const res = await fetch(`${SERVER()}/api/live/sessions`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${user.jwt}` },
    body: JSON.stringify({ lang, save_transcript: false }),
  });
  expect(res.status).toBe(200);
  return (await res.json()) as { session_id: string; invite_url: string };
}

async function apiDesk(org: ReturnType<typeof seedOrg>, name = 'Front desk') {
  const res = await fetch(`${SERVER()}/api/business/organizations/${org.orgId}/live/desks`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${org.owner.jwt}` },
    body: JSON.stringify({ name, location_label: 'Lobby', default_lang: 'it', project_id: org.projectId }),
  });
  expect(res.status).toBe(201);
  return (await res.json()) as { id: string; qr_url: string };
}

const metric = async (name: string) => {
  const text = await (await fetch(`${SERVER()}/metrics`)).text();
  const m = text.match(new RegExp(`^${name} (\\d+)`, 'm'));
  return m ? Number(m[1]) : 0;
};

const upstream = async () => {
  const stats = (await (await fetch(`${MOCK()}/__stats`)).json()) as {
    sessions: { source: string | null; target: string | null }[];
  };
  return stats.sessions.map((s) => `${s.source}->${s.target}`);
};

// ---- (a) the whole B2C journey ---------------------------------------------------------------

test('a host invites, a guest joins, each hears the other translated, mute and end work', async ({ browser }) => {
  const owner = seedUser({ name: 'Marco', balance: 20, language: 'it' });
  const h = await phone(browser, owner);
  const g = await phone(browser);
  const upstreamBefore = (await upstream()).length;
  const latencyBefore = await metric('voxtranslate_live_translation_ms_count');

  // The host creates the session: the invitation appears as a link AND a QR.
  await h.page.goto('/live');
  await expect(h.page.locator('#lh-setup')).toBeVisible();
  await h.page.locator('#lh-create').click();
  await expect(h.page.locator('#lh-invite')).toBeVisible();
  const link = await h.page.locator('#lh-link').inputValue();
  expect(link).toMatch(/^http:\/\/127\.0\.0\.1:4331\/lc\/[A-Za-z0-9_-]{20,}$/);
  await expect(h.page.locator('#lh-qr')).toHaveAttribute('src', /^data:image\/(png|svg)/);
  // Waiting, and said so.
  await expect(room(h.page)).toHaveAttribute('data-phase', 'waiting_peer');

  // The guest opens the link on another phone: no account, no credit. A language, consent, go.
  await g.page.goto(link);
  await expect(g.page.locator('#lg-join')).toBeVisible();
  await pickLanguage(g.page, 'lg', 'Spanish');
  await g.page.locator('#lg-name').fill('Lucia');
  await g.page.locator('#lg-consent').check();
  await g.page.locator('#lg-join-btn').click();

  // Both phones are connected to the same conversation, each seeing the other.
  await live(h.page);
  await live(g.page);
  await expect(h.page.locator('.lc-partner-name').first()).toHaveText('Lucia');
  await expect(g.page.locator('.lc-partner-name').first()).toHaveText('Marco');
  // The invitation is noise once the guest is here.
  await expect(h.page.locator('#lh-invite')).toBeHidden();

  // Simultaneous, no turn-taking: each side's microphone feeds the room, and each hears the
  // OTHER side's translation, in its own language, with the original underneath.
  await expect(g.page.locator('#lc-caption')).toContainText('[it->es]');
  await expect(h.page.locator('#lc-caption')).toContainText('[es->it]');
  await expect(g.page.locator('#lc-caption-original')).toContainText('[it speaks]');
  await expect(h.page.locator('#lc-caption-original')).toContainText('[es speaks]');

  // The audio was delivered AND handed to the playback graph on both phones.
  await expect.poll(async () => (await probe(g.page)).translatedAudio).toBeGreaterThan(0);
  await expect.poll(async () => (await probe(h.page)).translatedAudio).toBeGreaterThan(0);
  await expect.poll(async () => (await probe(g.page)).playedSamples).toBeGreaterThan(0);
  await expect.poll(async () => (await probe(h.page)).playedSamples).toBeGreaterThan(0);

  // Two independent upstream sessions, one per direction (not one shared translator).
  const dirs = (await upstream()).slice(upstreamBefore);
  expect(dirs).toEqual(expect.arrayContaining(['it->es', 'es->it']));

  // Mute is reflected on the other phone, and undone. (The guest muting is seen by the host; the
  // reverse is the separate test below, which pins a defect.)
  await g.page.locator('#lc-mute').click();
  await expect(g.page.locator('#lc-mute')).toHaveAttribute('aria-pressed', 'true');
  await expect(h.page.locator('.lc-muted-badge')).toBeVisible();
  await g.page.locator('#lc-mute').click();
  await expect(g.page.locator('#lc-mute')).toHaveAttribute('aria-pressed', 'false');
  await expect(h.page.locator('.lc-muted-badge')).toBeHidden();

  // The owner is the one who pays: the guest has no account at all, and the owner's balance fell.
  await expect
    .poll(() => Number(sql(`SELECT balance FROM users WHERE id = '${owner.id}'`)[0][0]), { timeout: 20_000 })
    .toBeLessThan(20);

  // The server measured speech -> translated audio for the utterances above (S18).
  await expect
    .poll(() => metric('voxtranslate_live_translation_ms_count'))
    .toBeGreaterThan(latencyBefore);

  // The host ends it: each side gets its own wording of the same fact.
  await h.page.locator('#lc-end').click();
  await h.page.locator('#lc-end-yes').click();
  await expect(h.page.locator('#lc-ended')).toBeVisible();
  await expect(h.page.locator('#lc-ended .lc-end-copy')).toHaveText(en.liveEndedByMe);
  await expect(g.page.locator('#lc-ended')).toBeVisible();
  await expect(g.page.locator('#lc-ended .lc-end-copy')).toHaveText(en.liveEndedByPartner);

  const [[status, reason]] = sql(
    `SELECT status, end_reason FROM live_sessions WHERE owner_user_id = '${owner.id}' ORDER BY created_at DESC LIMIT 1`,
  );
  expect([status, reason]).toEqual(['ended', 'ended_by_host']);

  expect(h.errors, 'host page errors').toEqual([]);
  expect(g.errors, 'guest page errors').toEqual([]);
  await h.ctx.close();
  await g.ctx.close();
});

// ---- known defect, pinned -----------------------------------------------------------------------

// The phone that joins SECOND learns about the first one from `room_joined.peers`, where the
// server names the id `id` (`PeerInfo`, server/src/protocol.rs). The client used to read `peer_id`
// there, so that phone never saw the other one's mute or `peer_left`; the unit tests faked the
// frame with the wrong key and could not see it. This pins the real behaviour end to end.
test('the phone that arrived second also sees the first phone mute', async ({ browser }) => {
  const owner = seedUser({ name: 'Marco', balance: 20, language: 'it' });
  const h = await phone(browser, owner);
  await h.page.goto('/live');
  await h.page.locator('#lh-create').click();
  await expect(room(h.page)).toHaveAttribute('data-phase', 'waiting_peer');
  const g = await phone(browser);
  await g.page.goto(await h.page.locator('#lh-link').inputValue());
  await pickLanguage(g.page, 'lg', 'Spanish');
  await g.page.locator('#lg-consent').check();
  await g.page.locator('#lg-join-btn').click();
  await live(h.page);
  await live(g.page);

  await h.page.locator('#lc-mute').click();
  await expect(g.page.locator('.lc-muted-badge')).toBeVisible({ timeout: 8000 });
  await h.ctx.close();
  await g.ctx.close();
});

// ---- (b) links that cannot be used ------------------------------------------------------------

test('a used link says session_full, an expired one invite_expired, a junk one invite_not_found', async ({ browser }) => {
  const owner = seedUser({ name: 'Marco', balance: 5 });
  const created = await apiCreate(owner);

  // First guest takes the seat (through the page, as a person would).
  const first = await phone(browser);
  await first.page.goto(created.invite_url);
  await pickLanguage(first.page, 'lg', 'Spanish');
  await first.page.locator('#lg-consent').check();
  await first.page.locator('#lg-join-btn').click();
  await expect(first.page.locator('#lg-session')).toBeVisible();

  // A second device with the same link is refused: one invitation, one guest.
  const second = await phone(browser);
  await second.page.goto(created.invite_url);
  await pickLanguage(second.page, 'lg', 'French');
  await second.page.locator('#lg-consent').check();
  await second.page.locator('#lg-join-btn').click();
  await expect(second.page.locator('#lg-status')).toHaveText(en.liveErrSessionFull);
  await expect(second.page.locator('#lg-session')).toBeHidden();

  // An invitation nobody used before it ran out.
  const stale = await apiCreate(seedUser({ name: 'Anna', balance: 5 }));
  expireInvite(stale.session_id);
  const late = await phone(browser);
  await late.page.goto(stale.invite_url);
  await pickLanguage(late.page, 'lg', 'Spanish');
  await late.page.locator('#lg-consent').check();
  await late.page.locator('#lg-join-btn').click();
  await expect(late.page.locator('#lg-status')).toHaveText(en.liveErrInviteExpired);

  // A code that was never issued answers like a malformed one: nothing to learn from it.
  const junk = await phone(browser);
  await junk.page.goto('/lc/not-a-real-invitation-code');
  await pickLanguage(junk.page, 'lg', 'Spanish');
  await junk.page.locator('#lg-consent').check();
  await junk.page.locator('#lg-join-btn').click();
  await expect(junk.page.locator('#lg-status')).toHaveText(en.liveErrInviteNotFound);

  for (const p of [first, second, late, junk]) await p.ctx.close();
});

// ---- (c) B2B: a permanent QR, a queue, a colleague answers, revoke ------------------------------

test('a customer scans a desk QR, staff answer from the queue, both talk; revoking kills the QR', async ({ browser }) => {
  const org = seedOrg({ credits: 1000 });
  const desk = await apiDesk(org);
  const segment = desk.qr_url.split('/').pop()!;
  expect(desk.qr_url).toMatch(/\/lc\/d\/[a-z0-9]+\.[A-Za-z0-9_-]+$/);

  const customer = await phone(browser);
  const staff = await phone(browser, org.staff);

  // The customer scans: a NEW session, waiting.
  await customer.page.goto(desk.qr_url);
  await pickLanguage(customer.page, 'ld', 'Spanish');
  await customer.page.locator('#ld-name').fill('Carmen');
  await customer.page.locator('#ld-consent').check();
  await customer.page.locator('#ld-join-btn').click();
  await expect(customer.page.locator('#ld-wait')).toBeVisible();
  await expect(customer.page.locator('#ld-desk-name')).toHaveText('Front desk');

  // A member of staff sees them in the queue of that desk, and answers.
  await staff.page.goto('/live/desks');
  const card = staff.page.locator(`article[data-desk="${desk.id}"]`);
  await expect(card.locator('.lc-queue-name')).toHaveText('Carmen');
  await card.locator('button[data-answer]').click();
  await expect(staff.page.locator('#ls-answer')).toBeVisible();
  await staff.page.locator('#ls-answer-go').click();

  // Both talk; each hears the other translated.
  await live(staff.page);
  await live(customer.page);
  await expect(customer.page.locator('#lc-caption')).toContainText('[it->es]');
  await expect(staff.page.locator('#lc-caption')).toContainText('[es->it]');
  await expect.poll(async () => (await probe(customer.page)).playedSamples).toBeGreaterThan(0);
  await expect.poll(async () => (await probe(staff.page)).playedSamples).toBeGreaterThan(0);

  // The business pays through its wallet, and the conversation is tied to its project.
  await expect
    .poll(() => Number(sql(`SELECT credits_balance FROM organizations WHERE id = '${org.orgId}'`)[0][0]), {
      timeout: 20_000,
    })
    .toBeLessThan(1000);
  const [[project]] = sql(`SELECT project_id FROM live_sessions WHERE desk_id = '${desk.id}' LIMIT 1`);
  expect(project).toBe(org.projectId);

  // Staff end it; the customer is told.
  await staff.page.locator('#lc-end').click();
  await staff.page.locator('#lc-end-yes').click();
  await expect(customer.page.locator('#lc-ended')).toBeVisible();

  // The same printed QR starts a brand new session (a QR is never a conversation).
  const again = await phone(browser);
  await again.page.goto(desk.qr_url);
  await pickLanguage(again.page, 'ld', 'Spanish');
  await again.page.locator('#ld-consent').check();
  await again.page.locator('#ld-join-btn').click();
  await expect(again.page.locator('#ld-wait')).toBeVisible();
  const [[sessions]] = sql(`SELECT count(*) FROM live_sessions WHERE desk_id = '${desk.id}'`);
  expect(Number(sessions)).toBe(2);

  // Revoke while that customer waits (connected, not merely scanned: a revoke that beats the
  // socket is a different, quieter path): they are told the QR is not valid any more...
  await expect(room(again.page)).toHaveAttribute('data-phase', 'waiting_peer');
  const revoked = await fetch(`${SERVER()}/api/business/organizations/${org.orgId}/live/desks/${desk.id}/revoke`, {
    method: 'POST',
    headers: { authorization: `Bearer ${org.owner.jwt}` },
  });
  expect(revoked.ok).toBe(true);
  await expect(again.page.locator('#ld-nobody-text')).toHaveText(en.liveDeskErrInvalid);

  // ...and anybody who scans it afterwards gets the invalid-QR screen, never a queue entry.
  const late = await phone(browser);
  await late.page.goto(`/lc/d/${segment}`);
  await pickLanguage(late.page, 'ld', 'Spanish');
  await late.page.locator('#ld-consent').check();
  await late.page.locator('#ld-join-btn').click();
  await expect(late.page.locator('#ld-status')).toHaveText(en.liveDeskErrInvalid);
  await expect(late.page.locator('#ld-join-btn')).toBeDisabled();

  for (const p of [customer, staff, again, late]) await p.ctx.close();
});

// ---- (d) the dashboard deep link ---------------------------------------------------------------

test('?answer=<session> opens the language choice of exactly that customer, and consumes the parameter', async ({
  browser,
}) => {
  const org = seedOrg();
  const desk = await apiDesk(org, 'Reception');
  const ana = await phone(browser);
  const bruno = await phone(browser);
  for (const [who, name] of [
    [ana, 'Ana'],
    [bruno, 'Bruno'],
  ] as const) {
    await who.page.goto(desk.qr_url);
    await pickLanguage(who.page, 'ld', 'Spanish');
    await who.page.locator('#ld-name').fill(name);
    await who.page.locator('#ld-consent').check();
    await who.page.locator('#ld-join-btn').click();
    await expect(who.page.locator('#ld-wait')).toBeVisible();
  }
  const [[brunoSession]] = sql(
    `SELECT s.id FROM live_sessions s JOIN live_participants p ON p.session_id = s.id
     WHERE s.desk_id = '${desk.id}' AND p.role = 'guest' AND p.display_name = 'Bruno'`,
  );

  const staff = await phone(browser, org.staff);
  await staff.page.goto(`/live/desks?answer=${brunoSession}`);
  await expect(staff.page.locator('#ls-answer')).toBeVisible();
  await expect(staff.page.locator('#ls-answer-title')).toContainText('Bruno');
  await expect(staff.page.locator('#ls-answer-title')).not.toContainText('Ana');
  // The parameter is spent: a reload does not repeat it and the id does not stay in the history.
  expect(new URL(staff.page.url()).searchParams.has('answer')).toBe(false);

  // A parameter that is not a session id is ignored, not trusted.
  await staff.page.goto('/live/desks?answer=../../etc/passwd');
  await expect(staff.page.locator('#ls-desks')).toBeVisible();
  await expect(staff.page.locator('#ls-answer')).toBeHidden();

  for (const p of [ana, bruno, staff]) await p.ctx.close();
});

// ---- (e) what Live Connect must not have changed ------------------------------------------------

test('Talk to Anyone still loads and the home entry points are where they were', async ({ browser }) => {
  const p = await phone(browser);
  await p.page.goto('/');
  // S1: the existing entry is untouched; Live Connect is a separate, additional one.
  await expect(p.page.locator('#hero-talk')).toHaveAttribute('href', '/talk');
  await expect(p.page.locator('#hero-live')).toHaveAttribute('href', '/live');

  await p.page.goto('/talk');
  await expect(p.page.locator('#tk-setup')).toBeVisible();
  await expect(p.page.locator('#tk-start')).toBeDisabled();
  expect(p.errors).toEqual([]);
  await p.ctx.close();
});

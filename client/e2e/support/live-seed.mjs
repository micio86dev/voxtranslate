// Seeding for the Live Connect e2e (spec 0121): users, an organisation, a project and a desk,
// straight into the LOCAL test database, plus the account JWTs the pages need.
//
// Why SQL and not the API: the only way to obtain an account on the server is Google sign-in,
// which a test cannot do (the server's `FakeVerifier` exists only inside the Rust test
// binaries). The server trusts a row in `users` plus an HS256 JWT signed with `JWT_SECRET`,
// so that is what is created here. Everything *after* login goes through the real HTTP API.
//
// SAFETY: this only ever talks to a loopback database. `assertLocalDb` is called on every
// query and there is no override; a test that writes users and organisations must not be one
// typo away from staging.

import { execFileSync } from 'node:child_process';
import { createHmac, randomUUID } from 'node:crypto';

export const LOCAL_DB = 'postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_voip_test';
export const JWT_SECRET = 'e2e-live-jwt-secret-0123456789abcdef-not-a-real-secret';

export function dbUrl() {
  return process.env.LIVE_E2E_DATABASE_URL || LOCAL_DB;
}

export function assertLocalDb(url = dbUrl()) {
  const host = new URL(url).hostname;
  if (!['127.0.0.1', 'localhost', '::1', '[::1]'].includes(host)) {
    throw new Error(`live e2e refuses a non-loopback database (${host}); it creates users and orgs`);
  }
}

const q = (v) => `'${String(v).replaceAll("'", "''")}'`;

/** Run one SQL statement, return its rows as arrays of strings. Needs `psql` on PATH (or PSQL). */
export function sql(statement) {
  assertLocalDb();
  const out = execFileSync(process.env.PSQL || 'psql', [dbUrl(), '-v', 'ON_ERROR_STOP=1', '-At', '-F', '\t', '-c', statement], {
    encoding: 'utf8',
  });
  return out
    .split('\n')
    .filter((l) => l && !/^(INSERT|UPDATE|DELETE) \d/.test(l))
    .map((l) => l.split('\t'));
}

const b64u = (b) => Buffer.from(b).toString('base64url');

/** An account session token, exactly what `issue_jwt` signs (HS256: sub, email, name, exp). */
export function mintJwt({ id, email, name }, hours = 24) {
  const head = b64u(JSON.stringify({ alg: 'HS256', typ: 'JWT' }));
  const body = b64u(JSON.stringify({ sub: id, email, name, exp: Math.floor(Date.now() / 1000) + hours * 3600 }));
  const sig = createHmac('sha256', JWT_SECRET).update(`${head}.${body}`).digest('base64url');
  return `${head}.${body}.${sig}`;
}

/**
 * A user with a balance, who has confirmed 18+ and the terms (the server refuses billed
 * features otherwise). Returns the fields the client stores plus the JWT.
 */
export function seedUser({ name = 'Marco', balance = 5, language = 'it' } = {}) {
  const tag = randomUUID();
  const email = `${tag}@e2e.invalid`;
  const [[id]] = sql(
    `INSERT INTO users (google_id, email, name, balance, age_confirmed, consent_tos_at)
     VALUES (${q(`g-${tag}`)}, ${q(email)}, ${q(name)}, ${Number(balance)}, TRUE, now()) RETURNING id`,
  );
  const user = { id, email, name, balance: Number(balance), consent_given: true, language };
  return { ...user, jwt: mintJwt(user) };
}

/**
 * An organisation with an active subscription and credit, one project, an owner (admin) and
 * one member (the receptionist). Returns ids and both users.
 */
export function seedOrg({ credits = 1000 } = {}) {
  const owner = seedUser({ name: 'Owner', language: 'it' });
  const staff = seedUser({ name: 'Reception', language: 'it' });
  const [[org]] = sql(
    `INSERT INTO organizations (name, slug, owner_id, subscription_status, current_period_end, credits_balance)
     VALUES ('E2E Hotel', ${q(`e2e-${randomUUID().replaceAll('-', '')}`)}, ${q(owner.id)}, 'active', now() + interval '30 days', ${Number(credits)})
     RETURNING id`,
  );
  sql(
    `INSERT INTO organization_members (org_id, user_id, role)
     VALUES (${q(org)}, ${q(owner.id)}, 'owner'), (${q(org)}, ${q(staff.id)}, 'member')`,
  );
  const [[project]] = sql(
    `INSERT INTO projects (org_id, name, created_by) VALUES (${q(org)}, 'Reception', ${q(owner.id)}) RETURNING id`,
  );
  return { orgId: org, projectId: project, owner, staff };
}

/** What a logged-in browser holds (`auth.ts`: `vox.token` and `vox.user`). */
export function storageFor(user) {
  const { jwt, ...profile } = user;
  return { 'vox.token': jwt, 'vox.user': JSON.stringify(profile) };
}

/** Make a session's invitation look expired, as the clock would. */
export function expireInvite(sessionId) {
  sql(`UPDATE live_sessions SET expires_at = now() - interval '1 minute' WHERE id = ${q(sessionId)}`);
}

// `node live-seed.mjs users 4 [balance]` prints a comma-separated list of account JWTs, for the
// k6 load test (loadtest/live-connect.js reads them from JWTS). Needs the local database.
if (import.meta.url === `file://${process.argv[1]}`) {
  const [cmd, count = '1', balance = '50'] = process.argv.slice(2);
  if (cmd !== 'users') {
    console.error('usage: node live-seed.mjs users <count> [balance]');
    process.exit(2);
  }
  const jwts = Array.from({ length: Number(count) }, () => seedUser({ balance: Number(balance) }).jwt);
  console.log(jwts.join(','));
}

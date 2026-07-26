// Registers VoxTranslate's GA4 **custom dimensions** via the Analytics Admin API, so the
// event parameters the apps already send become reportable instead of merely collected.
// The set lives in `custom-dimensions.json` (version-controlled), making this the
// reproducible source of truth rather than click-ops in the GA4 UI — mirroring
// infra/betterstack/setup-monitors.mjs.
//
// Idempotent: matches existing dimensions by `parameterName` and PATCHes the label /
// description when they drift, creating only what is missing. `parameterName` and `scope`
// are immutable in GA4, so a change to either is reported as a manual action instead of
// being silently ignored.
//
//   # 1. a bearer token you already have (fastest — see README)
//   GA4_ACCESS_TOKEN=ya29.… GA4_PROPERTY_ID=123456789 \
//     node infra/analytics/setup-ga4-dimensions.mjs
//
//   # 2. a service account added to the property as Editor (for CI)
//   GOOGLE_APPLICATION_CREDENTIALS=./sa.json GA4_PROPERTY_ID=123456789 \
//     node infra/analytics/setup-ga4-dimensions.mjs
//
//   # the numeric property id is not the G-XXXXXXX measurement id, but it can be
//   # looked up from it (walks accounts → properties → data streams):
//   GA4_ACCESS_TOKEN=… GA4_MEASUREMENT_ID=G-XXXXXXXX node infra/analytics/setup-ga4-dimensions.mjs
//
//   --dry-run   print the plan, touch nothing
//   --config <path>  use a different dimension set
//
// Requires the `analytics.edit` scope. Tokens are secrets: pass them via env, never
// commit them. See README.md in this folder.

import { readFileSync } from 'node:fs';
import { createSign } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const BASE = 'https://analyticsadmin.googleapis.com/v1beta';
const SCOPE = 'https://www.googleapis.com/auth/analytics.edit';
const HERE = dirname(fileURLToPath(import.meta.url));

const args = process.argv.slice(2);
const DRY_RUN = args.includes('--dry-run');
const CHECK_TOKEN = args.includes('--check-token');
const configPath = argValue('--config') || join(HERE, 'custom-dimensions.json');

function argValue(flag) {
  const i = args.indexOf(flag);
  return i >= 0 ? args[i + 1] : undefined;
}

function fail(msg) {
  console.error(`\n${msg}\n`);
  process.exit(1);
}

// ──────────────────────────────────────────────────────────────────────────
// Auth. Either a ready bearer token, or a service-account key we exchange for
// one (RS256 JWT bearer flow — no dependency needed for this).
// ──────────────────────────────────────────────────────────────────────────
function base64url(input) {
  return Buffer.from(input).toString('base64url');
}

async function tokenFromServiceAccount(keyPath) {
  let key;
  try {
    key = JSON.parse(readFileSync(keyPath, 'utf8'));
  } catch (err) {
    fail(`Could not read the service account key at ${keyPath}: ${err.message}`);
  }
  if (!key.client_email || !key.private_key) {
    fail(`${keyPath} does not look like a service account key (no client_email/private_key).`);
  }
  const now = Math.floor(Date.now() / 1000);
  const claims = {
    iss: key.client_email,
    scope: SCOPE,
    aud: 'https://oauth2.googleapis.com/token',
    iat: now,
    exp: now + 3600,
  };
  const unsigned = `${base64url(JSON.stringify({ alg: 'RS256', typ: 'JWT' }))}.${base64url(
    JSON.stringify(claims),
  )}`;
  const signature = createSign('RSA-SHA256').update(unsigned).sign(key.private_key, 'base64url');
  const res = await fetch('https://oauth2.googleapis.com/token', {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams({
      grant_type: 'urn:ietf:params:oauth:grant-type:jwt-bearer',
      assertion: `${unsigned}.${signature}`,
    }),
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok) {
    fail(
      `Token exchange failed (${res.status}): ${JSON.stringify(body)}\n` +
        `Is ${key.client_email} added to the GA4 property with Editor access, and is the\n` +
        'Google Analytics Admin API enabled on its project?',
    );
  }
  return body.access_token;
}

async function resolveToken() {
  if (process.env.GA4_ACCESS_TOKEN) {
    // A trailing newline (GA4_ACCESS_TOKEN=$(cat token.txt), a copy-paste out of an
    // editor) makes the Authorization header invalid and Google answers a bare 401 —
    // indistinguishable from an expired token unless you know to look for it.
    const token = process.env.GA4_ACCESS_TOKEN.trim();
    if (!token) fail('GA4_ACCESS_TOKEN is set but empty.');
    if (!token.startsWith('ya29.')) {
      console.warn(
        'warning: GA4_ACCESS_TOKEN does not start with "ya29." — OAuth access tokens do.\n' +
          '         An API key, an OAuth client id/secret, or a Measurement Protocol\n' +
          '         api_secret will all fail with 401. Continuing anyway.\n',
      );
    }
    return token;
  }
  const keyPath = process.env.GOOGLE_APPLICATION_CREDENTIALS;
  if (keyPath) return tokenFromServiceAccount(keyPath);
  fail(
    'No credentials. Provide ONE of:\n' +
      '  GA4_ACCESS_TOKEN=<bearer token with the analytics.edit scope>\n' +
      '  GOOGLE_APPLICATION_CREDENTIALS=<path to a service account key JSON>\n' +
      'See infra/analytics/README.md for how to get either in under a minute.',
  );
}

// ──────────────────────────────────────────────────────────────────────────
// Admin API client. Throws with the response body so failures are actionable
// (a missing scope and a missing property look nothing alike).
// ──────────────────────────────────────────────────────────────────────────
let TOKEN = '';

async function api(path, { method = 'GET', body, query } = {}) {
  const url = new URL(path.startsWith('http') ? path : BASE + path);
  for (const [k, v] of Object.entries(query ?? {})) url.searchParams.set(k, v);
  const res = await fetch(url, {
    method,
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      'Content-Type': 'application/json',
    },
    body: body == null ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  if (!res.ok) {
    // A missing scope, a property the credentials cannot see, and a typo in the id all
    // return different codes — say which one happened instead of dumping a stack trace.
    const hint =
      {
        401: 'The token is invalid or expired. Bearer tokens from the OAuth Playground last one hour.',
        403: 'Authenticated, but not authorised: the account needs Editor on this GA4 property, the token needs the analytics.edit scope, and the Google Analytics Admin API must be enabled.',
        404: 'No such property. GA4_PROPERTY_ID is the NUMERIC id (GA4 → Admin → Property details), not the G-XXXXXXX measurement id.',
        429: 'Admin API quota exhausted — retry later.',
      }[res.status] ?? 'Unexpected Admin API response.';
    fail(`${method} ${url.pathname} → ${res.status}\n\n${text}\n${hint}`);
  }
  return text ? JSON.parse(text) : null;
}

/** Find the numeric property id behind a G-XXXXXXX measurement id. */
async function propertyFromMeasurementId(measurementId) {
  const { accounts = [] } = await api('/accounts');
  if (!accounts.length) fail('The credentials can see no Analytics accounts.');
  for (const account of accounts) {
    const { properties = [] } = await api('/properties', {
      query: { filter: `parent:${account.name}` },
    });
    for (const property of properties) {
      const { dataStreams = [] } = await api(`/${property.name}/dataStreams`);
      const hit = dataStreams.find((s) => s.webStreamData?.measurementId === measurementId);
      if (hit) {
        console.log(`resolved ${measurementId} → ${property.name} (${property.displayName})`);
        return property.name.replace(/^properties\//, '');
      }
    }
  }
  fail(
    `No data stream with measurement id ${measurementId} is visible to these credentials.\n` +
      'Check the id, or pass GA4_PROPERTY_ID directly (GA4 → Admin → Property details).',
  );
}

/**
 * Ask Google what the credential actually is, independently of the Analytics API.
 * A 401 from analyticsadmin is ambiguous — expired token, wrong kind of credential,
 * missing scope, API not enabled all look alike from the outside. tokeninfo answers
 * the first three definitively, so this is the first thing to run when a token that
 * "should work" does not.
 */
async function checkToken(token) {
  const res = await fetch(
    `https://oauth2.googleapis.com/tokeninfo?access_token=${encodeURIComponent(token)}`,
  );
  const info = await res.json().catch(() => ({}));
  if (!res.ok) {
    fail(
      `Google rejected the token itself (${res.status}): ${JSON.stringify(info)}\n\n` +
        'So this is not about scopes or the Analytics API — the credential is not a\n' +
        'valid, unexpired OAuth access token. The usual causes:\n' +
        '  · the OAuth Playground id_token was copied instead of the access_token\n' +
        '    (an id_token is a JWT: three dot-separated base64 chunks starting "eyJ")\n' +
        '  · an API key or a Measurement Protocol api_secret was used\n' +
        '  · the token is older than its expiry (3600s for Playground tokens)\n' +
        '  · the token was revoked by generating a new one for the same client',
    );
  }
  const scopes = (info.scope || '').split(' ').filter(Boolean);
  console.log('Token accepted by Google:');
  console.log(`  expires in : ${info.expires_in}s`);
  console.log(`  audience   : ${info.aud ?? '(none)'}`);
  console.log(`  email      : ${info.email ?? '(not an end-user token)'}`);
  console.log(`  scopes     : ${scopes.length ? scopes.join('\n               ') : '(none)'}`);

  const canEdit = scopes.some(
    (s) => s === SCOPE || s === 'https://www.googleapis.com/auth/analytics.manage.users',
  );
  const canRead = scopes.some((s) =>
    [
      'https://www.googleapis.com/auth/analytics.readonly',
      'https://www.googleapis.com/auth/analytics',
    ].includes(s),
  );
  console.log('');
  if (canEdit) {
    console.log(`✓ ${SCOPE} present — this token can create custom dimensions.`);
    console.log('  If the Admin API still answers 401/403, the remaining causes are:');
    console.log('  · the Google Analytics Admin API is not enabled on the project that');
    console.log(
      '    issued the token → https://console.cloud.google.com/apis/library/analyticsadmin.googleapis.com',
    );
    console.log('  · the authorising account has no access to the GA4 property.');
  } else {
    console.log(`✗ ${SCOPE} is MISSING from this token.`);
    console.log('  Re-authorise with that exact scope; without it the Admin API refuses');
    console.log(
      `  every write.${canRead ? ' (A read-only analytics scope is present — that is not enough.)' : ''}`,
    );
    process.exitCode = 1;
  }
}

// ──────────────────────────────────────────────────────────────────────────
// Reconcile
// ──────────────────────────────────────────────────────────────────────────
const config = JSON.parse(readFileSync(configPath, 'utf8'));
const wanted = config.dimensions ?? [];
if (!wanted.length) fail(`${configPath} declares no dimensions.`);

TOKEN = await resolveToken();

if (CHECK_TOKEN) {
  await checkToken(TOKEN);
  process.exit(process.exitCode ?? 0);
}

const propertyId =
  process.env.GA4_PROPERTY_ID ||
  (process.env.GA4_MEASUREMENT_ID
    ? await propertyFromMeasurementId(process.env.GA4_MEASUREMENT_ID)
    : fail(
        'Set GA4_PROPERTY_ID (numeric, from GA4 → Admin → Property details) or\n' +
          'GA4_MEASUREMENT_ID (G-XXXXXXXX) to look it up.',
      ));

const parent = `properties/${propertyId}`;
const existing = [];
let pageToken;
do {
  const page = await api(`/${parent}/customDimensions`, {
    query: { pageSize: '200', ...(pageToken ? { pageToken } : {}) },
  });
  existing.push(...(page?.customDimensions ?? []));
  pageToken = page?.nextPageToken;
} while (pageToken);

console.log(`${parent}: ${existing.length} custom dimensions already registered\n`);

let created = 0;
let updated = 0;
let unchanged = 0;
const manual = [];

for (const want of wanted) {
  const found = existing.find((d) => d.parameterName === want.parameterName);

  if (!found) {
    if (DRY_RUN) {
      console.log(`+ would create ${want.parameterName} (${want.scope}) — "${want.displayName}"`);
    } else {
      await api(`/${parent}/customDimensions`, {
        method: 'POST',
        body: {
          parameterName: want.parameterName,
          displayName: want.displayName,
          description: want.description ?? '',
          scope: want.scope,
        },
      });
      console.log(`+ created ${want.parameterName}`);
    }
    created++;
    continue;
  }

  // scope is immutable — surface the conflict instead of pretending it applied.
  if (found.scope !== want.scope) {
    manual.push(
      `${want.parameterName}: scope is ${found.scope} in GA4 but ${want.scope} here. ` +
        'GA4 cannot change a scope — archive the dimension and re-run to recreate it.',
    );
    continue;
  }

  const drift =
    found.displayName !== want.displayName ||
    (found.description ?? '') !== (want.description ?? '');
  if (!drift) {
    unchanged++;
    continue;
  }
  if (DRY_RUN) {
    console.log(`~ would update ${want.parameterName} (label/description drifted)`);
  } else {
    await api(`/${found.name}`, {
      method: 'PATCH',
      query: { updateMask: 'displayName,description' },
      body: {
        displayName: want.displayName,
        description: want.description ?? '',
      },
    });
    console.log(`~ updated ${want.parameterName}`);
  }
  updated++;
}

// Registered in GA4 but no longer declared here: report, never delete. Archiving a
// dimension loses its historical reporting, so that stays a human decision.
const orphans = existing
  .filter((d) => !wanted.some((w) => w.parameterName === d.parameterName))
  .map((d) => d.parameterName);

console.log(
  `\n${DRY_RUN ? '[dry run] ' : ''}${created} created, ${updated} updated, ${unchanged} already correct`,
);
if (orphans.length) {
  console.log(`\nRegistered in GA4 but not declared in ${configPath.split('/').pop()}:`);
  for (const o of orphans) console.log(`  - ${o}`);
  console.log('Left alone — archiving one loses its history, so decide that by hand.');
}
if (manual.length) {
  console.log('\nNeeds a manual step:');
  for (const m of manual) console.log(`  ! ${m}`);
  process.exitCode = 1;
}
if (!DRY_RUN && created + updated > 0) {
  console.log('\nNote: dimensions apply to data collected from now on — GA4 does not backfill.');
}

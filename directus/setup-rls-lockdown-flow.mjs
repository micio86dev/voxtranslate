// Creates the "RLS lockdown on new collection" event Flow in Directus via the
// REST API. It fires whenever Directus creates a new collection (= a new public
// table, which always lands RLS-OFF) and calls the VoxTranslate server's
// `/api/admin/rls/enforce` endpoint, which runs `public.enforce_public_rls()` to
// lock it down within seconds — closing the up-to-24h gap before the nightly
// pg_cron job (infra/supabase/rls-lockdown-cron.sql) would otherwise catch it.
//
// Idempotent: re-running repairs the request operation in place if the flow
// already exists. Sibling of setup-gift-subscription-flow.mjs — same webhook
// shape, but an `event` (collections.create) trigger instead of a manual button.
//
//   DIRECTUS_URL=https://directus-production-ad16.up.railway.app \
//   DIRECTUS_TOKEN=<static admin token>   # or DIRECTUS_ADMIN_EMAIL + DIRECTUS_ADMIN_PASSWORD
//   node directus/setup-rls-lockdown-flow.mjs
//
// Requires:
//   - `public.enforce_public_rls()` installed (run rls-lockdown-cron.sql once).
//   - The Vox server deployed with the `/api/admin/rls/enforce` route.
//   - Directus env FLOWS_ENV_ALLOW_LIST includes ADMIN_API_SECRET and VOX_API_URL
//     so the webhook's {{$env.*}} placeholders resolve (already set in compose).

const FLOW_NAME = 'RLS lockdown on new collection';
const ENDPOINT = '/api/admin/rls/enforce';

// Event trigger: non-blocking `action` hook on collection creation. No body is
// needed — the endpoint takes no input and just re-runs the lockdown function.
const TRIGGER_OPTIONS = { type: 'action', scope: ['collections.create'] };

// Request-operation options, shared by create + repair. Content-Type is required
// because the server route is mounted on axum (415 otherwise); the body is an
// empty JSON object — the endpoint ignores it.
const REQUEST_OPTIONS = {
  method: 'POST',
  url: `{{$env.VOX_API_URL}}${ENDPOINT}`,
  headers: [
    { header: 'Content-Type', value: 'application/json' },
    { header: 'X-Admin-Secret', value: '{{$env.ADMIN_API_SECRET}}' },
  ],
  body: '{}',
};

const base = (process.env.DIRECTUS_URL || '').replace(/\/$/, '');
if (!base) fail('Set DIRECTUS_URL to your Directus base URL.');

async function api(path, { method = 'GET', body } = {}, tk) {
  const res = await fetch(base + path, {
    method,
    headers: {
      'Content-Type': 'application/json',
      ...(tk ? { Authorization: `Bearer ${tk}` } : {}),
    },
    body: body == null ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  if (!res.ok) fail(`${method} ${path} → ${res.status}: ${text}`);
  return text ? JSON.parse(text) : {};
}

async function getToken() {
  if (process.env.DIRECTUS_TOKEN) return process.env.DIRECTUS_TOKEN;
  const email = process.env.DIRECTUS_ADMIN_EMAIL;
  const password = process.env.DIRECTUS_ADMIN_PASSWORD;
  if (!email || !password)
    fail('Provide DIRECTUS_TOKEN, or DIRECTUS_ADMIN_EMAIL + DIRECTUS_ADMIN_PASSWORD.');
  const r = await api('/auth/login', { method: 'POST', body: { email, password } });
  return r.data.access_token;
}

const tk = await getToken();

const flows = (await api('/flows?limit=-1&fields=id,name,trigger,options', {}, tk)).data;
const existing = flows.find((f) => f.name === FLOW_NAME);
if (existing) {
  // Repair-in-place: re-apply the trigger scope + request options so a flow
  // created by an older version of this script is fixed without hand-editing
  // (the prod Directus UI is org-blocked).
  const ops = (
    await api(
      `/operations?limit=-1&filter[flow][_eq]=${existing.id}&fields=id,key,type`,
      {},
      tk,
    )
  ).data;
  const reqOp = ops.find((o) => o.type === 'request' || o.key === 'rls_enforce_request');
  if (!reqOp) fail(`Flow "${FLOW_NAME}" exists but has no request operation to repair.`);
  await api(`/operations/${reqOp.id}`, { method: 'PATCH', body: { options: REQUEST_OPTIONS } }, tk);
  await api(
    `/flows/${existing.id}`,
    { method: 'PATCH', body: { options: TRIGGER_OPTIONS } },
    tk,
  );
  console.log(`✓ Repaired "${FLOW_NAME}" (flow ${existing.id}, op ${reqOp.id}).`);
  process.exit(0);
}

const flow = (
  await api(
    '/flows',
    {
      method: 'POST',
      body: {
        name: FLOW_NAME,
        icon: 'shield_lock',
        color: '#00C4B4',
        status: 'active',
        trigger: 'event',
        accountability: 'all',
        options: TRIGGER_OPTIONS,
      },
    },
    tk,
  )
).data;

const op = (
  await api(
    '/operations',
    {
      method: 'POST',
      body: {
        flow: flow.id,
        name: 'Enforce RLS',
        key: 'rls_enforce_request',
        type: 'request',
        position_x: 19,
        position_y: 1,
        options: REQUEST_OPTIONS,
      },
    },
    tk,
  )
).data;

await api(`/flows/${flow.id}`, { method: 'PATCH', body: { operation: op.id } }, tk);

console.log(
  `✓ Created "${FLOW_NAME}" (flow ${flow.id}). New Directus collections now ` +
    'trigger an immediate RLS lockdown via the Vox server.',
);

function fail(msg) {
  console.error('✗ ' + msg);
  process.exit(1);
}

// Creates the "Gift bonus" manual Flow in Directus via the REST API. Idempotent:
// re-running repairs the request operation in place if the flow already exists
// (re-applies the body/header options). This is the reproducible version of
// the manual step in README §7 — a Directus Flow lives in Directus's own
// `directus_flows`/`directus_operations` tables, NOT in the app schema, so it is
// configured through the API rather than an app `.sql` migration.
//
//   DIRECTUS_URL=https://directus-production-ad16.up.railway.app \
//   DIRECTUS_TOKEN=<static admin token>   # or DIRECTUS_ADMIN_EMAIL + DIRECTUS_ADMIN_PASSWORD
//   node directus/setup-bonus-flow.mjs
//
// To make the button land in the same place as the other admin buttons, it mirrors
// the trigger options of an existing manual Flow on `users` (e.g. "Adjust credits")
// when one is present, overriding only the collection + confirmation inputs.
//
// Requires (Directus env): FLOWS_ENV_ALLOW_LIST must include ADMIN_API_SECRET and
// VOX_API_URL so the webhook's {{$env.*}} placeholders resolve.

const FLOW_NAME = 'Gift bonus';
const COLLECTION = 'users';
const ENDPOINT = '/api/admin/bonus';
// A manual flow run from the admin UI delivers its data as an HTTP request, so the
// confirmation fields + selected keys live under `$trigger.body.*` (NOT `$trigger.*`
// or bare `{{amount}}`, which render literal "undefined"). `$accountability.user` is
// a top-level context var. Verified against a real flow log on 2026-06-13.
const BODY =
  '{ "user_id": "{{$trigger.body.keys[0]}}", "amount": {{$trigger.body.amount}}, ' +
  '"message": "{{$trigger.body.message}}", "actor": "{{$accountability.user}}" }';

// Request-operation options, shared by create + repair. The server uses axum's JSON
// extractor, which 415s unless `Content-Type: application/json` is present — the
// header below is mandatory, not cosmetic.
const REQUEST_OPTIONS = {
  method: 'POST',
  url: `{{$env.VOX_API_URL}}${ENDPOINT}`,
  headers: [
    { header: 'Content-Type', value: 'application/json' },
    { header: 'X-Admin-Secret', value: '{{$env.ADMIN_API_SECRET}}' },
  ],
  body: BODY,
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

// Keep the sibling flow's placement/confirmation wiring; swap collection + inputs.
function buildTriggerOptions(template) {
  const o = template ? structuredClone(template) : { location: 'item', requireSelection: true };
  o.collections = [COLLECTION];
  o.fields = [
    {
      field: 'amount',
      type: 'float',
      name: 'Amount (USD)',
      meta: { field: 'amount', type: 'float', interface: 'input', required: true,
              options: { min: 0.01, step: 0.01 } },
    },
    {
      field: 'message',
      type: 'string',
      name: 'Message (optional)',
      meta: { field: 'message', type: 'string', interface: 'input' },
    },
  ];
  return o;
}

const tk = await getToken();

const flows = (await api('/flows?limit=-1&fields=id,name,trigger,options', {}, tk)).data;
const existing = flows.find((f) => f.name === FLOW_NAME);
if (existing) {
  // Repair-in-place: re-apply the request operation's options so a flow created by an
  // older version of this script (undefined body refs + missing Content-Type → 415)
  // is fixed without deleting it by hand (the prod Directus UI is org-blocked).
  const ops = (
    await api(
      `/operations?limit=-1&filter[flow][_eq]=${existing.id}&fields=id,key,type`,
      {},
      tk,
    )
  ).data;
  const reqOp = ops.find((o) => o.type === 'request' || o.key === 'bonus_request');
  if (!reqOp) fail(`Flow "${FLOW_NAME}" exists but has no request operation to repair.`);
  await api(`/operations/${reqOp.id}`, { method: 'PATCH', body: { options: REQUEST_OPTIONS } }, tk);
  console.log(
    `✓ Repaired "${FLOW_NAME}" request operation (flow ${existing.id}, op ${reqOp.id}). ` +
      'Run a grant again — the credit should apply and the email send.',
  );
  process.exit(0);
}

const sibling = flows.find(
  (f) => f.trigger === 'manual' && (f.options?.collections || []).includes(COLLECTION),
);
console.log(
  sibling
    ? `Cloning trigger config from existing manual flow "${sibling.name}".`
    : 'No sibling manual flow on `users` found — using default trigger options (verify the button shows).',
);

const flow = (
  await api('/flows', {
    method: 'POST',
    body: {
      name: FLOW_NAME,
      icon: 'redeem',
      color: '#6644FF',
      status: 'active',
      trigger: 'manual',
      accountability: 'all',
      options: buildTriggerOptions(sibling?.options),
    },
  }, tk)
).data;

const op = (
  await api('/operations', {
    method: 'POST',
    body: {
      flow: flow.id,
      name: 'Bonus request',
      key: 'bonus_request',
      type: 'request',
      position_x: 19,
      position_y: 1,
      options: REQUEST_OPTIONS,
    },
  }, tk)
).data;

await api(`/flows/${flow.id}`, { method: 'PATCH', body: { operation: op.id } }, tk);

console.log(`✓ Created "${FLOW_NAME}" (flow ${flow.id}). Reload a user page — the button appears in the top-right actions.`);

function fail(msg) {
  console.error('✗ ' + msg);
  process.exit(1);
}

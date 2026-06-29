// Creates the "Gift subscription" manual Flow in Directus via the REST API.
// Idempotent: re-running repairs the request operation in place if the flow
// already exists (re-applies the body/header options). Sibling of
// setup-bonus-flow.mjs — same shape, different collection + endpoint.
//
//   DIRECTUS_URL=https://directus-production-ad16.up.railway.app \
//   DIRECTUS_TOKEN=<static admin token>   # or DIRECTUS_ADMIN_EMAIL + DIRECTUS_ADMIN_PASSWORD
//   node directus/setup-gift-subscription-flow.mjs
//
// The button lands on the `organizations` collection (a B2B org IS the
// subscription holder — plan/credits live on the org row, not on a user). Run
// `setup-backoffice.mjs` first so `organizations` is a registered collection.
//
// Requires (Directus env): FLOWS_ENV_ALLOW_LIST must include ADMIN_API_SECRET and
// VOX_API_URL so the webhook's {{$env.*}} placeholders resolve.

const FLOW_NAME = 'Gift subscription';
const COLLECTION = 'organizations';
const ENDPOINT = '/api/admin/org/gift-subscription';
// A manual flow run from the admin UI delivers its data as an HTTP request, so the
// confirmation fields + selected keys live under `$trigger.body.*`.
// `$accountability.user` is a top-level context var.
//
// EVERY field is quoted in the body. A Directus confirmation field renders to an
// empty string when left blank, so `"credits": {{…}}` would emit `"credits": ,`
// (invalid JSON) for an empty optional. Quoting keeps the body valid JSON whatever
// the operator types; the server parses `months`/`credits` leniently (empty string
// → use the default).
const BODY =
  '{ "org_id": "{{$trigger.body.keys[0]}}", "plan": "{{$trigger.body.plan}}", ' +
  '"months": "{{$trigger.body.months}}", "credits": "{{$trigger.body.credits}}", ' +
  '"message": "{{$trigger.body.message}}", "actor": "{{$accountability.user}}" }';

// Request-operation options, shared by create + repair. The server uses axum's JSON
// extractor, which 415s unless `Content-Type: application/json` is present.
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

// Keep a sibling org-flow's placement/confirmation wiring (if any); swap the inputs.
function buildTriggerOptions(template) {
  const o = template ? structuredClone(template) : {};
  o.collections = [COLLECTION];
  o.location = 'item';
  // The confirmation dialog is what collects plan/months/credits. WITHOUT
  // `requireConfirmation` Directus skips the dialog and fires the webhook
  // immediately with empty fields → the server rejects it. The other admin flows
  // (Gift bonus / Ban / Adjust credits) all set this; the gift flow had no sibling
  // on `organizations` to clone it from, so set it explicitly. `requireSelection`
  // is false to match them (the item is already targeted on its detail page).
  o.requireConfirmation = true;
  o.requireSelection = false;
  o.confirmationDescription =
    'Gift this organization an active Business/Enterprise subscription.';
  o.fields = [
    {
      field: 'plan',
      type: 'string',
      name: 'Plan',
      meta: {
        field: 'plan',
        type: 'string',
        interface: 'select-dropdown',
        required: true,
        options: {
          choices: [
            { text: 'Business', value: 'business' },
            { text: 'Enterprise', value: 'enterprise' },
          ],
        },
      },
    },
    {
      field: 'months',
      type: 'integer',
      name: 'Months (blank = 1)',
      meta: {
        field: 'months',
        type: 'integer',
        interface: 'input',
        options: { min: 1, max: 36, placeholder: '1' },
      },
    },
    {
      field: 'credits',
      type: 'integer',
      name: 'Credits (blank = plan default)',
      meta: {
        field: 'credits',
        type: 'integer',
        interface: 'input',
        options: {
          min: 0,
          // The default depends on the chosen plan, so it can't be pre-filled
          // here — leave blank and the server applies it.
          placeholder: 'Blank → Business 1000 / Enterprise 5000 per month',
        },
      },
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
  // Repair-in-place: re-apply the request operation's options so a flow created by
  // an older version of this script is fixed without deleting it by hand (the prod
  // Directus UI is org-blocked).
  const ops = (
    await api(
      `/operations?limit=-1&filter[flow][_eq]=${existing.id}&fields=id,key,type`,
      {},
      tk,
    )
  ).data;
  const reqOp = ops.find((o) => o.type === 'request' || o.key === 'gift_subscription_request');
  if (!reqOp) fail(`Flow "${FLOW_NAME}" exists but has no request operation to repair.`);
  await api(`/operations/${reqOp.id}`, { method: 'PATCH', body: { options: REQUEST_OPTIONS } }, tk);
  // Also re-apply the trigger inputs in case the field set changed.
  await api(
    `/flows/${existing.id}`,
    { method: 'PATCH', body: { options: buildTriggerOptions(existing.options) } },
    tk,
  );
  console.log(
    `✓ Repaired "${FLOW_NAME}" (flow ${existing.id}, op ${reqOp.id}). ` +
      'Reload an organization page and gift again — plan + credits should apply.',
  );
  process.exit(0);
}

const sibling = flows.find(
  (f) => f.trigger === 'manual' && (f.options?.collections || []).includes(COLLECTION),
);
console.log(
  sibling
    ? `Cloning trigger config from existing manual flow "${sibling.name}".`
    : 'No sibling manual flow on `organizations` found — using default trigger options ' +
        '(verify the button shows; run setup-backoffice.mjs first to register the collection).',
);

const flow = (
  await api(
    '/flows',
    {
      method: 'POST',
      body: {
        name: FLOW_NAME,
        icon: 'workspace_premium',
        color: '#00C4B4',
        status: 'active',
        trigger: 'manual',
        accountability: 'all',
        options: buildTriggerOptions(sibling?.options),
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
        name: 'Gift subscription request',
        key: 'gift_subscription_request',
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
  `✓ Created "${FLOW_NAME}" (flow ${flow.id}). Reload an organization page — the button ` +
    'appears in the top-right actions.',
);

function fail(msg) {
  console.error('✗ ' + msg);
  process.exit(1);
}

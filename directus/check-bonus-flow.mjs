// Diagnoses why the "Gift bonus" Directus Flow runs (trigger returns 200) but the
// bonus is never applied (no credit, no email). It is READ-ONLY for your real data:
// it inspects the flow config and runs ONE non-destructive probe that replicates the
// flow's backend call with `amount: 0`, which the server rejects at validation
// (`400 "amount must be a positive number"`) AFTER the admin-secret check but BEFORE
// any credit grant or email — so no bonus is ever issued. See server/src/admin.rs.
//
//   DIRECTUS_URL=https://directus-production-ad16.up.railway.app \
//   DIRECTUS_TOKEN=<static admin token>   # or DIRECTUS_ADMIN_EMAIL + DIRECTUS_ADMIN_PASSWORD
//   node directus/check-bonus-flow.mjs
//
// What it tells you (the two failure modes that both look like "flow ran, nothing happened"):
//   • backend 403  → Directus is NOT resolving {{$env.ADMIN_API_SECRET}} (empty/mismatched
//                    secret). Fix the Directus service env: ADMIN_API_SECRET (== backend's),
//                    VOX_API_URL, and FLOWS_ENV_ALLOW_LIST=ADMIN_API_SECRET,VOX_API_URL, then restart.
//   • backend 400  → the secret resolves and auth PASSES. The real flow's request body refs
//                    are the suspect — re-run `node directus/setup-bonus-flow.mjs` to repair them.
//   • no status / network error → {{$env.VOX_API_URL}} is not resolving (same allow-list fix).

const FLOW_NAME = 'Gift bonus';
const ENDPOINT = '/api/admin/bonus';
// Expected request-operation options (must match setup-bonus-flow.mjs).
const EXPECTED = {
  method: 'POST',
  url: `{{$env.VOX_API_URL}}${ENDPOINT}`,
  contentType: 'application/json',
  secretHeader: 'X-Admin-Secret',
  secretValue: '{{$env.ADMIN_API_SECRET}}',
  bodyRefs: ['$trigger.body.keys[0]', '$trigger.body.amount'],
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

// Walk an arbitrary JSON value and return the first plausible HTTP status code found
// under a `status`/`statusCode`/`code` key — robust to Directus version response shapes.
function findStatus(node) {
  const stack = [node];
  let fallback = null;
  while (stack.length) {
    const cur = stack.pop();
    if (cur == null || typeof cur !== 'object') continue;
    for (const [k, v] of Object.entries(cur)) {
      if (/^(status|statuscode|code)$/i.test(k) && Number.isInteger(v) && v >= 100 && v <= 599) {
        if ([400, 401, 403, 404, 415, 422, 500, 503].includes(v)) return v; // prefer a real HTTP result
        fallback = fallback ?? v;
      }
      if (v && typeof v === 'object') stack.push(v);
    }
  }
  return fallback;
}

const tk = await getToken();

// ── 1. Flow + request-operation config ─────────────────────────────────────────
const flows = (await api('/flows?limit=-1&fields=id,name,status,trigger', {}, tk)).data;
const flow = flows.find((f) => f.name === FLOW_NAME);
if (!flow) fail(`No "${FLOW_NAME}" flow found on this Directus. Run setup-bonus-flow.mjs first.`);
console.log(`✓ Flow "${FLOW_NAME}" found (id ${flow.id}, status ${flow.status}, trigger ${flow.trigger}).`);
if (flow.status !== 'active') console.log(`  ⚠ flow status is "${flow.status}" — activate it.`);

const ops = (
  await api(`/operations?limit=-1&filter[flow][_eq]=${flow.id}&fields=id,key,type,options`, {}, tk)
).data;
const reqOp = ops.find((o) => o.type === 'request' || o.key === 'bonus_request');
if (!reqOp) fail('Flow exists but has no request operation.');
const o = reqOp.options || {};
const headers = Object.fromEntries((o.headers || []).map((h) => [String(h.header).toLowerCase(), h.value]));
const bodyStr = typeof o.body === 'string' ? o.body : JSON.stringify(o.body || '');

const drift = [];
if ((o.method || '').toUpperCase() !== EXPECTED.method) drift.push(`method is "${o.method}" (want ${EXPECTED.method})`);
if (o.url !== EXPECTED.url) drift.push(`url is "${o.url}" (want "${EXPECTED.url}")`);
if (headers['content-type'] !== EXPECTED.contentType) drift.push(`Content-Type header missing/wrong (415 risk): "${headers['content-type'] ?? '<none>'}"`);
if (headers[EXPECTED.secretHeader.toLowerCase()] !== EXPECTED.secretValue) drift.push(`${EXPECTED.secretHeader} header is "${headers[EXPECTED.secretHeader.toLowerCase()] ?? '<none>'}" (want "${EXPECTED.secretValue}")`);
for (const ref of EXPECTED.bodyRefs) if (!bodyStr.includes(ref)) drift.push(`body is missing ref "${ref}" — got: ${bodyStr}`);

if (drift.length) {
  console.log('✗ Request-operation config has drifted from the expected setup:');
  for (const d of drift) console.log(`    - ${d}`);
  console.log('  → Re-run `node directus/setup-bonus-flow.mjs` to repair it in place.');
} else {
  console.log('✓ Request-operation config matches setup-bonus-flow.mjs (url, headers, body refs all correct).');
}

// ── 2. Live env-resolution probe (non-destructive: amount 0 ⇒ server 400, no grant) ─
console.log('\nProbing whether Directus resolves {{$env.VOX_API_URL}} and {{$env.ADMIN_API_SECRET}}…');
let tmpFlowId = null;
try {
  tmpFlowId = (
    await api('/flows', {
      method: 'POST',
      body: {
        name: 'bonus-env-probe (temp)',
        status: 'active',
        trigger: 'webhook',
        accountability: 'all',
        options: { method: 'POST', async: false, return: '$all' },
      },
    }, tk)
  ).data.id;

  const probeOp = (
    await api('/operations', {
      method: 'POST',
      body: {
        flow: tmpFlowId,
        name: 'probe',
        key: 'probe',
        type: 'request',
        position_x: 19,
        position_y: 1,
        options: {
          method: 'POST',
          url: EXPECTED.url, // {{$env.VOX_API_URL}}/api/admin/bonus
          headers: [
            { header: 'Content-Type', value: EXPECTED.contentType },
            { header: EXPECTED.secretHeader, value: EXPECTED.secretValue },
          ],
          // amount 0 → rejected at validation AFTER the secret check, BEFORE any grant.
          body: '{ "user_id": "00000000-0000-0000-0000-000000000000", "amount": 0, "actor": "env-probe" }',
        },
      },
    }, tk)
  ).data;
  await api(`/flows/${tmpFlowId}`, { method: 'PATCH', body: { operation: probeOp.id } }, tk);

  // Trigger directly; tolerate a non-2xx (a backend 403 makes the request op reject).
  const res = await fetch(`${base}/flows/trigger/${tmpFlowId}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${tk}` },
    body: JSON.stringify({ ping: 1 }),
  });
  let payload;
  try { payload = JSON.parse(await res.text()); } catch { payload = {}; }
  const status = findStatus(payload);

  if (status === 400) {
    console.log('✓ Backend returned 400 — the admin secret RESOLVED and auth PASSED (amount 0 rejected, no bonus issued).');
    console.log('  → Env is fine. If real grants still fail, the culprit is the request body refs:');
    console.log('    re-run `node directus/setup-bonus-flow.mjs` to repair the operation in place.');
  } else if (status === 403) {
    console.log('✗ Backend returned 403 "invalid admin secret" — Directus is sending an empty/wrong secret.');
    console.log('  → On the DIRECTUS service (Railway), set and then RESTART:');
    console.log('      ADMIN_API_SECRET      = <exact same value as the backend service>');
    console.log('      VOX_API_URL           = https://api.voxtranslate.app');
    console.log('      FLOWS_ENV_ALLOW_LIST  = ADMIN_API_SECRET,VOX_API_URL');
  } else if (status === 503) {
    console.log('✗ Backend returned 503 — the backend service has no ADMIN_API_SECRET set. Fix it on the backend, not Directus.');
  } else {
    console.log(`⚠ Could not read a clear backend status from the probe (got: ${status ?? 'none'}).`);
    console.log('  This usually means {{$env.VOX_API_URL}} did not resolve (so the request never reached the backend).');
    console.log('  → Add VOX_API_URL to the Directus service and to FLOWS_ENV_ALLOW_LIST, then restart.');
    console.log('  Raw probe response:', JSON.stringify(payload).slice(0, 600));
  }
} finally {
  if (tmpFlowId) {
    try {
      await api(`/flows/${tmpFlowId}`, { method: 'DELETE' }, tk);
      console.log('\n(cleaned up temporary probe flow)');
    } catch (e) {
      console.log(`\n⚠ Could not delete temp probe flow ${tmpFlowId} — remove "bonus-env-probe (temp)" by hand: ${e.message}`);
    }
  }
}

function fail(msg) {
  console.error('✗ ' + msg);
  process.exit(1);
}

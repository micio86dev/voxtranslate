// Provisions the Live Connect (spec 0121) administration in Directus via the REST API:
//
//   1. a "Live Connect" folder with the live_* collections registered, readable
//      display templates, relations, and every sensitive column hidden;
//   2. two roles, created here (nothing else in this repo creates roles):
//        "Live Connect Support"  read-only, METADATA ONLY (no names, no transcripts, no QR)
//        "Live Connect Privacy"  Support + the one Flow button that reads a transcript;
//   3. the Flow buttons, each calling the VoxTranslate server (`/api/admin/live/*`)
//      with the shared secret: Revoke desk, Regenerate desk, End session, Reconcile
//      session, and (Privacy only) View transcript;
//   4. an Insights dashboard that reads metadata only.
//
// Directus stays a thin admin UI: it never writes the live tables. Every action goes
// through the server, which audits it (`admin_audit`). Customer-facing workflows
// (desks, QR download, session history, usage) stay in the B2B dashboard.
//
//   DIRECTUS_URL=https://<your-directus> \
//     DIRECTUS_ADMIN_EMAIL=… DIRECTUS_ADMIN_PASSWORD=…   # or DIRECTUS_TOKEN=<static admin token>
//     node directus/setup-live-connect.mjs [--dry-run] [--print-transcript-filter]
//
//   --dry-run                  print every call this script WOULD make and change nothing;
//                              needs no Directus, no URL and no credentials.
//   --print-transcript-filter  print the permission filters that keep Live Connect
//                              conversations out of EXISTING roles (see README), then exit.
//   --no-transcript-preview    the View transcript button then reports only counts, with no
//                              excerpt (a notification is stored in Directus and may be e-mailed).
//
// Idempotent: re-running updates in place (collection metadata, fields, policies,
// permissions, flows, the dashboard's panels). Requires Directus 11 (policies).
// Requires (Directus env): FLOWS_ENV_ALLOW_LIST includes ADMIN_API_SECRET and VOX_API_URL.
// Run setup-backoffice.mjs first so organizations / projects /
// organization_credits_transactions are already registered collections.

const args = new Set(process.argv.slice(2));
const DRY = args.has('--dry-run');
// How many utterances the transcript button previews in the operator's notification (0 = none).
const PREVIEW_LINES = args.has('--no-transcript-preview') ? 0 : 3;

// ──────────────────────────────────────────────────────────────────────────
// What keeps a conversation out of an EXISTING role. `transcript_events`,
// `session_participants`, `transcripts`, `call_sessions` (and everything else that
// hangs off a call session) already hold Live Connect rows, because a live
// conversation reuses the meeting tables. A role that must not read live
// conversations needs this filter on each of them. A NULL `kind` is an ordinary
// meeting, so `_neq` alone would hide it (SQL `NULL <> 'x'` is not true).
// ──────────────────────────────────────────────────────────────────────────
const NOT_LIVE = {
  _or: [{ kind: { _null: true } }, { kind: { _neq: 'live_connect' } }],
};
const TRANSCRIPT_FILTERS = {
  call_sessions: NOT_LIVE,
  ...Object.fromEntries(
    [
      'transcript_events',
      'session_participants',
      'transcripts',
      'session_reports',
      'session_sentiments',
      'session_emails',
      'transcript_bookmarks',
      'chat_files',
    ].map((c) => [c, { session_id: NOT_LIVE }]),
  ),
};

if (args.has('--print-transcript-filter')) {
  console.log(JSON.stringify(TRANSCRIPT_FILTERS, null, 2));
  process.exit(0);
}

// ──────────────────────────────────────────────────────────────────────────
// Tiny REST client. In --dry-run nothing leaves the process: reads answer "nothing
// there yet" and writes are printed.
// ──────────────────────────────────────────────────────────────────────────
const base = (process.env.DIRECTUS_URL || '').replace(/\/$/, '');
if (!DRY && !base) fail('Set DIRECTUS_URL to your Directus base URL (or pass --dry-run).');

let TOKEN = '';
let planned = 0;
async function api(path, { method = 'GET', body } = {}) {
  if (DRY) {
    if (method === 'GET') return { data: [] };
    planned++;
    console.log(`  [dry-run] ${method} ${path}${body === undefined ? '' : ' ' + compact(body)}`);
    return { data: { id: `<dry-run:${path.split('?')[0].split('/')[1] || 'id'}>` } };
  }
  const res = await fetch(base + path, {
    method,
    headers: {
      'Content-Type': 'application/json',
      ...(TOKEN ? { Authorization: `Bearer ${TOKEN}` } : {}),
    },
    body: body == null ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  if (!res.ok) throw new Error(`${method} ${path} → ${res.status}: ${text.slice(0, 300)}`);
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

const q = encodeURIComponent;
async function find(path, filterField, value) {
  const r = await api(`${path}?filter[${filterField}][_eq]=${q(value)}&limit=1&fields=*`);
  return r.data?.[0] || null;
}

// ──────────────────────────────────────────────────────────────────────────
// 1. Data model
// ──────────────────────────────────────────────────────────────────────────
const FOLDER = {
  name: 'live_connect',
  icon: 'forum',
  color: '#2ECDA7',
  note: 'Live Connect (spec 0121): metadata of two-phone conversations. Written ONLY by the server; actions are Flow buttons.',
  translations: [{ language: 'en-US', translation: 'Live Connect' }],
};

const STATUS_SESSION = [
  { text: 'Waiting', value: 'waiting', color: '#FFA439' },
  { text: 'Active', value: 'active', color: '#2ECDA7' },
  { text: 'Ended', value: 'ended', color: '#A2B5CD' },
  { text: 'Expired', value: 'expired', color: '#E35169' },
];
const STATUS_DESK = [
  { text: 'Active', value: 'active', color: '#2ECDA7' },
  { text: 'Revoked', value: 'revoked', color: '#E35169' },
];
const ROLE_PARTICIPANT = [
  { text: 'Host', value: 'host', color: '#3399FF' },
  { text: 'Staff', value: 'staff', color: '#3399FF' },
  { text: 'Guest', value: 'guest', color: '#6644FF' },
];
const PAYER = [
  { text: 'User', value: 'user', color: '#3399FF' },
  { text: 'Org', value: 'org', color: '#00C4B4' },
];
const labels = (choices) => ({
  interface: 'select-dropdown',
  options: { choices },
  display: 'labels',
  display_options: { choices, showAsDot: false },
});
const m2o = (template) => ({
  interface: 'select-dropdown-m2o',
  special: ['m2o'],
  options: { template },
  display: 'related-values',
  display_options: { template },
});
const HIDDEN_HASH =
  'Hidden on purpose: a SHA-256 fingerprint of a credential. No use to an operator; never exposed to Support.';
const PERSONAL =
  'PERSONAL DATA (a person\'s name). Admin-only: excluded from the Live Connect Support and Privacy roles; erased with the conversation.';

// Collections. `live_usage_ticks` is NOT here: its primary key is composite
// (session_id, peer_id, tick_seq) and Directus ignores tables without a single
// primary key column ("doesn't have a primary key column and will be ignored").
// Per-session billing is checked by the "Reconcile session" Flow instead.
const COLLECTIONS = {
  live_desks: {
    icon: 'storefront',
    display: '{{name}} · {{location_label}} ({{status}})',
    note: 'Permanent QR entry points of a business. The QR itself is derived from a server secret and never stored.',
    sort: 1,
    fields: {
      id: {},
      org_id: m2o('{{name}}'),
      project_id: m2o('{{name}}'),
      name: {},
      location_label: {},
      default_lang: {},
      languages: {},
      public_id: { hidden: true, note: 'Part of the printed QR URL: hidden so the QR is never reconstructable here.' },
      token_version: { note: 'Bumped by Regenerate: every earlier printed QR stops verifying.' },
      status: labels(STATUS_DESK),
      created_by: { hidden: true },
      created_at: {},
      revoked_at: {},
    },
  },
  live_sessions: {
    icon: 'forum',
    display: '{{status}} · {{end_reason}} · {{created_at}}',
    note: 'One row per conversation: lifecycle, owner, project and desk. NO content here.',
    sort: 2,
    fields: {
      id: {},
      call_session_id: { hidden: true, note: 'Points at the meeting-side row that holds what was said (conversation content). Not for Support.' },
      owner_user_id: { note: 'The account that owns (and pays for) a personal conversation. Shown as an id on purpose: user names are not exposed to Support.' },
      org_id: m2o('{{name}}'),
      project_id: m2o('{{name}}'),
      desk_id: m2o('{{name}}'),
      status: labels(STATUS_SESSION),
      invite_hash: { hidden: true, note: HIDDEN_HASH },
      host_lang: {},
      save_transcript: {},
      engine_id: {},
      created_at: {},
      expires_at: {},
      started_at: {},
      ended_at: {},
      end_reason: { note: 'ended_by_host / ended_by_guest / ended_by_staff / ended_by_admin / abandoned / expired / max_duration / desk_revoked / desk_regenerated / …' },
    },
    // Reverse (one-to-many) views so an operator reads a session in one place.
    aliases: [
      { field: 'participants', relation: ['live_participants', 'session_id'], template: '{{role}} · {{lang}}', note: 'Who was on the call (no names).' },
      { field: 'events', relation: ['live_session_events', 'session_id'], template: '{{kind}} · {{ts}}', note: 'Bounded diagnostics timeline (kinds only).' },
    ],
  },
  live_participants: {
    icon: 'groups',
    display: '{{role}} · {{lang}}',
    note: 'The two seats of a conversation. Names are personal data and are NOT visible to Support.',
    sort: 3,
    fields: {
      peer_id: {},
      session_id: m2o('{{status}} · {{created_at}}'),
      role: labels(ROLE_PARTICIPANT),
      user_id: { hidden: true },
      display_name: { note: PERSONAL },
      lang: {},
      token_hash: { hidden: true, note: HIDDEN_HASH },
      consent_at: {},
      joined_at: {},
      left_at: {},
    },
  },
  live_session_events: {
    icon: 'timeline',
    display: '{{kind}} · {{ts}}',
    note: 'Bounded diagnostics (join, leave, reconnect, admin_ended, …). Never tokens or transcript text.',
    sort: 4,
    fields: {
      id: {},
      session_id: m2o('{{status}} · {{created_at}}'),
      ts: {},
      kind: {},
      detail: { note: 'Free-form diagnostic JSON: admin-only, excluded from Support.' },
    },
  },
};

// Columns each role may read. EVERYTHING not listed is invisible to the role (an
// explicit allow-list: a column added later is hidden until someone lists it).
const SUPPORT_FIELDS = {
  live_desks: ['id', 'org_id', 'project_id', 'name', 'location_label', 'default_lang', 'languages', 'token_version', 'status', 'created_at', 'revoked_at'],
  live_sessions: ['id', 'owner_user_id', 'org_id', 'project_id', 'desk_id', 'status', 'host_lang', 'save_transcript', 'engine_id', 'created_at', 'expires_at', 'started_at', 'ended_at', 'end_reason', 'participants', 'events'],
  // No display_name, no token_hash, no user_id.
  live_participants: ['peer_id', 'session_id', 'role', 'lang', 'consent_at', 'joined_at', 'left_at'],
  // No `detail`.
  live_session_events: ['id', 'session_id', 'ts', 'kind'],
};

async function ensureFolder() {
  const meta = { icon: FOLDER.icon, color: FOLDER.color, note: FOLDER.note, collapse: 'open', translations: FOLDER.translations };
  try {
    await api('/collections', { method: 'POST', body: { collection: FOLDER.name, schema: null, meta } });
    return 'created';
  } catch {
    await api(`/collections/${FOLDER.name}`, { method: 'PATCH', body: { meta } });
    return 'updated';
  }
}

async function ensureCollection(name, cfg) {
  const meta = {
    icon: cfg.icon,
    note: cfg.note,
    display_template: cfg.display,
    group: FOLDER.name,
    sort: cfg.sort,
    hidden: false,
  };
  // PATCH first: it also registers an as-yet-unmanaged table (verified on Directus 11).
  try {
    await api(`/collections/${name}`, { method: 'PATCH', body: { meta } });
    return 'updated';
  } catch (ePatch) {
    try {
      await api('/collections', { method: 'POST', body: { collection: name, schema: null, meta } });
      return 'registered';
    } catch (ePost) {
      throw new Error(`${name}: ${trunc(ePatch)} | ${trunc(ePost)}`);
    }
  }
}

// Field metadata: read-only in the app (only the server writes these tables), hidden
// where sensitive, with a readable interface/display for statuses and relations.
async function ensureFields(collection, fields) {
  for (const [field, spec] of Object.entries(fields)) {
    const meta = { readonly: true, ...spec };
    await api(`/fields/${collection}/${field}`, { method: 'PATCH', body: { meta } });
  }
}

async function ensureAliases(collection, aliases = []) {
  for (const a of aliases) {
    const meta = {
      special: ['o2m'],
      interface: 'list-o2m',
      options: { template: a.template, enableCreate: false, enableSelect: false },
      display: 'related-values',
      display_options: { template: a.template },
      readonly: true,
      note: a.note,
    };
    const have = (await api(`/fields/${collection}/${a.field}`).catch(() => null))?.data;
    if (have && have.field) {
      await api(`/fields/${collection}/${a.field}`, { method: 'PATCH', body: { meta } });
    } else {
      await api(`/fields/${collection}`, { method: 'POST', body: { field: a.field, type: 'alias', schema: null, meta } });
    }
    // The foreign key already exists in Postgres; this only gives it the reverse name.
    // (Directus needs collection/field/related_collection at the top level here.)
    const [many, field] = a.relation;
    await api(`/relations/${many}/${field}`, {
      method: 'PATCH',
      body: { collection: many, field, related_collection: collection, meta: { one_field: a.field } },
    });
  }
}

// ──────────────────────────────────────────────────────────────────────────
// 2. Roles and policies (Directus 11: permissions hang off policies)
// ──────────────────────────────────────────────────────────────────────────
const SUPPORT = 'Live Connect Support';
const PRIVACY = 'Live Connect Privacy';
const SUPPORT_POLICY = 'Live Connect Support (metadata)';
const PRIVACY_POLICY = 'Live Connect Privacy (transcript button)';
const FLOW_PREFIX = 'Live Connect - ';
const DASHBOARD = 'Live Connect - Operations';
const TRANSCRIPT_FLOW = `${FLOW_PREFIX}View transcript`;

const readPerm = (collection, fields, permissions = {}) => ({
  collection,
  action: 'read',
  fields,
  permissions,
  validation: null,
  presets: null,
});

function supportPermissions() {
  const perms = Object.entries(SUPPORT_FIELDS).map(([c, f]) => readPerm(c, f));
  // Names only, for display of the relations above.
  perms.push(readPerm('organizations', ['id', 'name']));
  perms.push(readPerm('projects', ['id', 'name']));
  // Live Connect usage in the org ledger, amounts only (no description, no session link).
  perms.push(readPerm('organization_credits_transactions', ['id', 'org_id', 'amount', 'type', 'created_at'], { type: { _eq: 'live_connect' } }));
  // Insights + buttons: only the Live Connect ones, and never the transcript button.
  perms.push(readPerm('directus_dashboards', ['*'], { name: { _starts_with: FLOW_PREFIX } }));
  perms.push(readPerm('directus_panels', ['*'], { dashboard: { name: { _starts_with: FLOW_PREFIX } } }));
  perms.push(
    readPerm('directus_flows', ['*'], {
      _and: [
        { trigger: { _eq: 'manual' } },
        { name: { _starts_with: FLOW_PREFIX } },
        { name: { _neq: TRANSCRIPT_FLOW } },
      ],
    }),
  );
  return perms;
}

function privacyPermissions() {
  // Adds exactly one thing: seeing the transcript button.
  return [readPerm('directus_flows', ['*'], { _and: [{ trigger: { _eq: 'manual' } }, { name: { _eq: TRANSCRIPT_FLOW } }] })];
}

async function ensurePolicy(name, description, icon) {
  const body = { name, icon, description, admin_access: false, app_access: true, enforce_tfa: false };
  const have = await find('/policies', 'name', name);
  if (have) {
    await api(`/policies/${have.id}`, { method: 'PATCH', body });
    return have.id;
  }
  return (await api('/policies', { method: 'POST', body })).data.id;
}

async function ensureRole(name, description, icon) {
  const body = { name, icon, description };
  const have = await find('/roles', 'name', name);
  if (have) {
    await api(`/roles/${have.id}`, { method: 'PATCH', body });
    return have.id;
  }
  return (await api('/roles', { method: 'POST', body })).data.id;
}

async function ensureAccess(roleId, policyId) {
  const r = await api(`/access?filter[role][_eq]=${roleId}&filter[policy][_eq]=${policyId}&limit=1`);
  if (r.data?.length) return 'linked';
  await api('/access', { method: 'POST', body: { role: roleId, policy: policyId } });
  return 'linked now';
}

const norm = (p) =>
  JSON.stringify({
    collection: p.collection,
    action: p.action,
    fields: [...(p.fields || [])].sort(),
    permissions: p.permissions || {},
  });

// Replace the policy's permissions with exactly `desired` (only when they differ).
async function setPermissions(policyId, desired) {
  const have = (await api(`/permissions?filter[policy][_eq]=${policyId}&limit=-1&fields=*`)).data || [];
  const same =
    have.length === desired.length &&
    JSON.stringify(have.map(norm).sort()) === JSON.stringify(desired.map(norm).sort());
  if (same) return 'unchanged';
  for (const p of have) await api(`/permissions/${p.id}`, { method: 'DELETE' });
  await api('/permissions', { method: 'POST', body: desired.map((p) => ({ ...p, policy: policyId })) });
  return `set (${desired.length})`;
}

// ──────────────────────────────────────────────────────────────────────────
// 3. Flows: manual buttons that call the server
// ──────────────────────────────────────────────────────────────────────────
const reasonField = (label, hint) => ({
  field: 'reason',
  type: 'text',
  name: label,
  meta: { field: 'reason', type: 'text', interface: 'input-multiline', required: true, width: 'full', note: hint },
});

// Runs inside Directus's sandbox (isolated-vm). Only plain JS: no imports.
// `data.$trigger.body.keys` is the selected row(s); `data.actor_lookup` the item-read.
// One step does the checks AND builds the request, because Directus allows each
// operation to be the failure target of only ONE predecessor.
const PREPARE = (idField, withReason, privacyRoleId) => `
module.exports = async function (data) {
  const keys = (data.$trigger && data.$trigger.body && data.$trigger.body.keys) || [];
  if (keys.length !== 1) throw new Error('Select exactly one row: this action applies to a single desk/session.');
  ${privacyRoleId
    ? `const acc = data.$accountability || {};
  const roles = acc.roles || (acc.role ? [acc.role] : []);
  if (!acc.admin && !roles.includes(${JSON.stringify(privacyRoleId)})) throw new Error('Only the Live Connect Privacy role may read a conversation.');`
    : ''}
  const user = Array.isArray(data.actor_lookup) ? data.actor_lookup[0] : data.actor_lookup;
  const actor = user && user.email;
  if (!actor) throw new Error('Your Directus account has no email: the audit trail needs one.');
  const body = { ${idField}: String(keys[0]), actor: actor };
  ${withReason ? `body.reason = String((data.$trigger.body && data.$trigger.body.reason) || '').trim();` : ''}
  return { body: JSON.stringify(body) };
};`;

// Turns the server's answer (or its refusal) into a short message for the operator.
const FORMAT = (kind) => `
module.exports = async function (data) {
  const KIND = ${JSON.stringify(kind)};
  const out = (s, m) => ({ subject: s, message: m });
  // A step BEFORE the request failed (selection, role, missing e-mail): nothing was sent.
  if (data.request === undefined) {
    const e = data.$last;
    return out('Live Connect: not done', String((e && e.message) || 'The action did not run.'));
  }
  const r = data.request || {};
  if (!r.data || typeof r.data !== 'object') {
    return out('Live Connect: not done', 'The server did not answer as expected' + (r.status ? ' (HTTP ' + r.status + ')' : '') + (r.message ? ': ' + r.message : '') + '.');
  }
  const res = r.data;
  if (res.code || res.message && !res.status) {
    return out('Live Connect: refused', String(res.code || 'error') + (res.message ? ' - ' + res.message : ''));
  }
  if (KIND === 'desk-revoke') return out('Desk revoked', 'Status: ' + res.status + '. Waiting customers closed: ' + res.waiting_closed + '. Audited.');
  if (KIND === 'desk-regenerate') return out('Desk regenerated', 'Token version is now ' + res.token_version + '; waiting customers closed: ' + res.waiting_closed + '. The new QR is NOT shown here by design: the organisation downloads it from its dashboard. Audited.');
  if (KIND === 'session-end') return out('Session ended', 'End reason: ' + res.end_reason + '. Finalisation started. Audited.');
  if (KIND === 'session-reconcile') {
    const t = res.ticks || {}, l = res.ledger || {};
    const lines = [
      'Verdict: ' + String(res.status).toUpperCase() + ' (payer: ' + res.payer + ')',
      'Ticks: ' + t.count + ' (org credits ' + t.org_credits + ', user USD ' + t.user_usd + ')',
      'Ledger: org entries ' + l.org_entries + ' (credits ' + l.org_credits + '), usage USD ' + l.user_usd,
      'Ticks without a debit: ' + res.missing_ticks + ' / debits without a tick: ' + res.duplicate_ticks,
    ].concat((res.notes || []).map(function (n) { return 'Note: ' + n; }));
    return out('Reconciliation: ' + res.status, lines.join('\\n'));
  }
  if (KIND === 'session-transcript') {
    const us = res.utterances || [];
    const clip = function (s) { s = String(s || ''); return s.length > 120 ? s.slice(0, 120) + '...' : s; };
    const N = ${PREVIEW_LINES};
    const preview = us.slice(0, N).map(function (u) { return '- [' + u.original_lang + '] ' + clip(u.original); });
    return out(
      'Transcript read (' + us.length + ' utterances)',
      'This access was AUDITED with your name and your reason.' +
        (N > 0 ? '\\n' + preview.join('\\n') + (us.length > N ? '\\n... ' + (us.length - N) + ' more not shown.' : '') +
          '\\nDelete this notification when you are done: it holds a copy of this excerpt.' : '\\nNo excerpt is kept in Directus.')
    );
  }
  return out('Done', JSON.stringify(res));
};`;

function flowSpecs(privacyRoleId) {
  return [
    {
      name: `${FLOW_PREFIX}Revoke desk`,
      icon: 'block',
      color: '#E35169',
      collection: 'live_desks',
      description: 'Switch a desk\'s QR off and close everyone waiting at it (server: /api/admin/live/desk-revoke).',
      confirm: 'Revokes the desk of the selected row: its printed QR stops working immediately and customers still waiting are closed. Audited with your name and reason.',
      fields: [reasonField('Reason', 'Why is this desk being revoked?')],
      endpoint: '/api/admin/live/desk-revoke',
      idField: 'desk_id',
      kind: 'desk-revoke',
    },
    {
      name: `${FLOW_PREFIX}Regenerate desk`,
      icon: 'autorenew',
      color: '#FFA439',
      collection: 'live_desks',
      description: 'Invalidate every printed QR of a desk and put it back in service (server: /api/admin/live/desk-regenerate). The new QR is never shown here.',
      confirm: 'Replaces the desk\'s QR: every earlier print stops working. You will NOT see the new QR: the organisation downloads it from its own dashboard. Audited.',
      fields: [reasonField('Reason', 'Why is the QR being replaced?')],
      endpoint: '/api/admin/live/desk-regenerate',
      idField: 'desk_id',
      kind: 'desk-regenerate',
    },
    {
      name: `${FLOW_PREFIX}End session`,
      icon: 'call_end',
      color: '#E35169',
      collection: 'live_sessions',
      description: 'End an open conversation as the platform (server: /api/admin/live/session-end).',
      confirm: 'Ends the selected conversation now (end reason ended_by_admin). The two phones are told within seconds. Audited with your name and reason.',
      fields: [reasonField('Reason', 'Why is this conversation being ended?')],
      endpoint: '/api/admin/live/session-end',
      idField: 'session_id',
      kind: 'session-end',
    },
    {
      name: `${FLOW_PREFIX}Reconcile session`,
      icon: 'fact_check',
      color: '#3399FF',
      collection: 'live_sessions',
      description: 'Read-only billing check: metered ticks vs the ledger (server: /api/admin/live/session-reconcile). Changes nothing.',
      confirm: 'Compares what the metering recorded with what was debited for the selected conversation. Read-only.',
      fields: [],
      endpoint: '/api/admin/live/session-reconcile',
      idField: 'session_id',
      kind: 'session-reconcile',
    },
    {
      name: TRANSCRIPT_FLOW,
      icon: 'visibility',
      color: '#6644FF',
      collection: 'live_sessions',
      description: 'PRIVILEGED: read what was said in one conversation (server: /api/admin/live/session-transcript). Requires the Privacy role, a real reason, and is audited BEFORE anything is shown.',
      confirm: 'You are about to read the CONTENT of a conversation. Your name, the session and your reason are written to the audit log first. If the audit cannot be written, nothing is shown.',
      fields: [reasonField('Reason (min. 10 characters)', 'Be specific: e.g. "customer ticket #123 asked for a copy".')],
      endpoint: '/api/admin/live/session-transcript',
      idField: 'session_id',
      kind: 'session-transcript',
      privacyRoleId,
    },
  ];
}

// Operation chain for one flow. Each op carries its successors by KEY; ids are
// resolved after creation.
//   actor_lookup → prepare(check + build) → request ─ok→ format → notify
//                         └ reject → format_prep_fail → notify_prep_fail
//                                          └ reject → format_fail → notify_fail
function operationsFor(spec) {
  const withReason = spec.fields.length > 0;
  const notify = (key, from) => ({
    key,
    name: 'Tell the operator',
    type: 'notification',
    options: {
      recipient: ['{{$accountability.user}}'],
      subject: `{{${from}.subject}}`,
      message: `{{${from}.message}}`,
      permissions: '$full',
    },
  });
  return [
    {
      key: 'actor_lookup',
      name: 'Who is acting',
      type: 'item-read',
      options: { collection: 'directus_users', key: ['{{$accountability.user}}'], query: { fields: ['email'] }, permissions: '$full', emitEvents: false },
      resolve: 'prepare',
    },
    { key: 'prepare', name: 'Check and build the request', type: 'exec', options: { code: PREPARE(spec.idField, withReason, spec.privacyRoleId) }, resolve: 'request', reject: 'format_prep_fail' },
    {
      key: 'request',
      name: 'Call the VoxTranslate server',
      type: 'request',
      options: {
        method: 'POST',
        url: `{{$env.VOX_API_URL}}${spec.endpoint}`,
        headers: [
          { header: 'Content-Type', value: 'application/json' },
          { header: 'X-Admin-Secret', value: '{{$env.ADMIN_API_SECRET}}' },
        ],
        body: '{{prepare.body}}',
      },
      resolve: 'format',
      reject: 'format_fail',
    },
    { key: 'format', name: 'Format the answer', type: 'exec', options: { code: FORMAT(spec.kind) }, resolve: 'notify' },
    notify('notify', 'format'),
    { key: 'format_fail', name: 'Format the refusal', type: 'exec', options: { code: FORMAT(spec.kind) }, resolve: 'notify_fail' },
    notify('notify_fail', 'format_fail'),
    { key: 'format_prep_fail', name: 'Format the check failure', type: 'exec', options: { code: FORMAT(spec.kind) }, resolve: 'notify_prep_fail' },
    notify('notify_prep_fail', 'format_prep_fail'),
  ];
}

function triggerOptions(spec) {
  return {
    collections: [spec.collection],
    location: 'item',
    requireSelection: true,
    requireConfirmation: true,
    confirmationDescription: spec.confirm,
    async: false,
    fields: spec.fields,
  };
}

async function ensureFlow(spec) {
  const body = {
    name: spec.name,
    icon: spec.icon,
    color: spec.color,
    description: spec.description,
    status: 'active',
    trigger: 'manual',
    accountability: 'all',
    options: triggerOptions(spec),
  };
  let flow = await find('/flows', 'name', spec.name);
  let verb;
  if (flow) {
    // Detach and drop the old operations; they are rebuilt below so an edit to this
    // script always lands (no half-old chain).
    await api(`/flows/${flow.id}`, { method: 'PATCH', body: { ...body, operation: null } });
    const old = (await api(`/operations?filter[flow][_eq]=${flow.id}&limit=-1&fields=id`)).data || [];
    // Operations point at each other (resolve/reject): cut the links, then delete.
    for (const o of old) await api(`/operations/${o.id}`, { method: 'PATCH', body: { resolve: null, reject: null } });
    for (const o of old) await api(`/operations/${o.id}`, { method: 'DELETE' });
    verb = 'updated';
  } else {
    flow = (await api('/flows', { method: 'POST', body })).data;
    verb = 'created';
  }
  const ops = operationsFor(spec);
  const ids = {};
  let x = 19;
  for (const op of ops) {
    const created = (
      await api('/operations', {
        method: 'POST',
        body: { flow: flow.id, name: op.name, key: op.key, type: op.type, position_x: x, position_y: op.key.includes('fail') ? 19 : 1, options: op.options },
      })
    ).data;
    ids[op.key] = created.id;
    if (!op.key.includes('fail')) x += 18;
  }
  for (const op of ops) {
    const patch = {};
    if (op.resolve) patch.resolve = ids[op.resolve];
    if (op.reject) patch.reject = ids[op.reject];
    if (Object.keys(patch).length) await api(`/operations/${ids[op.key]}`, { method: 'PATCH', body: patch });
  }
  await api(`/flows/${flow.id}`, { method: 'PATCH', body: { operation: ids[ops[0].key] } });
  return verb;
}

// ──────────────────────────────────────────────────────────────────────────
// 4. Insights: metadata only (panels run with the VIEWER's permissions, so a
//    Support user's panels can only ever read what Support can read)
// ──────────────────────────────────────────────────────────────────────────
const panels = [];
const COL_X = [1, 7, 13, 19];
function metric(dash, idx, baseY, p) {
  panels.push({
    dashboard: dash, name: p.name, note: p.note || null, icon: p.icon || 'analytics', color: p.color || null,
    show_header: true, type: 'metric', position_x: COL_X[idx % 4], position_y: baseY + Math.floor(idx / 4) * 6, width: 6, height: 6,
    options: { collection: p.collection, field: p.field || 'id', function: p.fn || 'count', sortField: null, filter: p.filter || {}, prefix: p.prefix || '', suffix: p.suffix || '', abbreviate: true, conditionalFormatting: [] },
  });
}
function timeseries(dash, { x = 1, y, w = 24, h = 11, name, note, color, collection, dateField, fn = 'count', field = 'id', filter, precision = 'day', range = '8 weeks' }) {
  panels.push({
    dashboard: dash, name, note: note || null, icon: 'show_chart', color: color || null, show_header: true, type: 'time-series',
    position_x: x, position_y: y, width: w, height: h,
    options: { collection, dateField, valueField: field, function: fn, precision, range, filter: filter || {}, color: color || null, fillHoles: true, showXAxis: true, showYAxis: true },
  });
}
function list(dash, { x = 1, y, w = 12, h = 12, name, note, collection, sortField = 'created_at', sortDirection = 'desc', limit = 8, filter }) {
  panels.push({
    dashboard: dash, name, note: note || null, icon: 'list', color: null, show_header: true, type: 'list',
    position_x: x, position_y: y, width: w, height: h,
    options: { collection, limit, sortField, sortDirection, filter: filter || {} },
  });
}
function barchart(dash, { x = 1, y, w = 12, h = 14, name, note, color, collection, xField, fn = 'count', field = 'id', filter, horizontal = true }) {
  panels.push({
    dashboard: dash, name, note: note || null, icon: 'bar_chart', color: color || null, show_header: true, type: 'bar-chart',
    position_x: x, position_y: y, width: w, height: h,
    options: { collection, xAxis: xField, yAxis: field, function: fn, filter: filter || {}, horizontal, color: color || null, showDataLabel: true, sortDirection: 'desc' },
  });
}

async function ensureDashboard(name, icon, note) {
  const found = (await api(`/dashboards?filter[name][_eq]=${q(name)}&fields=id&limit=1`)).data;
  if (found?.length) {
    const id = found[0].id;
    const old = (await api(`/panels?filter[dashboard][_eq]=${id}&fields=id&limit=-1`)).data || [];
    for (const p of old) await api(`/panels/${p.id}`, { method: 'DELETE' });
    await api(`/dashboards/${id}`, { method: 'PATCH', body: { icon, note } });
    return id;
  }
  return (await api('/dashboards', { method: 'POST', body: { name, icon, note } })).data.id;
}

const DAYS7 = '$NOW(-7 days)';
const FAILED = { _or: [{ end_reason: { _in: ['abandoned', 'expired', 'ended_by_billing', 'max_duration'] } }, { status: { _eq: 'expired' } }] };

function buildInsights(d) {
  let i = 0;
  metric(d, i++, 1, { name: 'Open sessions now', icon: 'sensors', color: '#2ECDA7', collection: 'live_sessions', filter: { status: { _in: ['waiting', 'active'] } }, note: 'waiting + active' });
  metric(d, i++, 1, { name: 'Active conversations', icon: 'forum', color: '#2ECDA7', collection: 'live_sessions', filter: { status: { _eq: 'active' } } });
  metric(d, i++, 1, { name: 'Sessions (7d)', icon: 'history', collection: 'live_sessions', filter: { created_at: { _gte: DAYS7 } } });
  metric(d, i++, 1, { name: 'Expired while waiting (7d)', icon: 'hourglass_empty', color: '#FFA439', collection: 'live_sessions', filter: { _and: [{ created_at: { _gte: DAYS7 } }, { status: { _eq: 'expired' } }, { started_at: { _null: true } }] }, note: 'Nobody answered in time. Compare with "Sessions (7d)" for the waiting-expired rate.' });
  metric(d, i++, 1, { name: 'Failed / abandoned (7d)', icon: 'error', color: '#E35169', collection: 'live_sessions', filter: { _and: [{ created_at: { _gte: DAYS7 } }, FAILED] } });
  metric(d, i++, 1, { name: 'Ended by an operator (7d)', icon: 'admin_panel_settings', collection: 'live_sessions', filter: { _and: [{ created_at: { _gte: DAYS7 } }, { end_reason: { _eq: 'ended_by_admin' } }] } });
  metric(d, i++, 1, { name: 'Desks revoked', icon: 'block', color: '#E35169', collection: 'live_desks', filter: { status: { _eq: 'revoked' } } });
  metric(d, i++, 1, { name: 'Org credits spent (7d)', icon: 'toll', color: '#00C4B4', collection: 'organization_credits_transactions', fn: 'sum', field: 'amount', filter: { _and: [{ type: { _eq: 'live_connect' } }, { created_at: { _gte: DAYS7 } }] }, note: 'Negative: credits debited from organisation wallets for Live Connect.' });
  const tsY = 1 + Math.ceil(i / 4) * 6;
  timeseries(d, { x: 1, y: tsY, w: 12, name: 'Sessions / day', color: '#3399FF', collection: 'live_sessions', dateField: 'created_at' });
  timeseries(d, { x: 13, y: tsY, w: 12, name: 'Failed / abandoned / day', color: '#E35169', collection: 'live_sessions', dateField: 'created_at', filter: FAILED });
  const mid = tsY + 11;
  barchart(d, { x: 1, y: mid, w: 12, name: 'Sessions by end reason', color: '#6644FF', collection: 'live_sessions', xField: 'end_reason', filter: { end_reason: { _nnull: true } }, note: 'Why conversations ended' });
  barchart(d, { x: 13, y: mid, w: 12, name: 'Sessions by desk', color: '#00C4B4', collection: 'live_sessions', xField: 'desk_id', filter: { desk_id: { _nnull: true } }, note: 'Busiest desks (count). Credits per desk are not available in Directus: ticks have a composite key, see README.' });
  timeseries(d, { x: 1, y: mid + 14, w: 24, h: 10, name: 'Org credits spent / day', color: '#00C4B4', collection: 'organization_credits_transactions', dateField: 'created_at', fn: 'sum', field: 'amount', filter: { type: { _eq: 'live_connect' } } });
  const listY = mid + 24;
  list(d, { x: 1, y: listY, w: 12, name: 'Failed / abandoned sessions', collection: 'live_sessions', filter: FAILED, note: 'Newest first. Select one → Reconcile session / End session.' });
  list(d, { x: 13, y: listY, w: 12, name: 'Recent sessions', collection: 'live_sessions' });
}

// ──────────────────────────────────────────────────────────────────────────
// Run
// ──────────────────────────────────────────────────────────────────────────
async function requireDirectus11() {
  const info = (await api('/server/info')).data;
  const version = info?.version;
  if (!version) {
    console.log('  ! could not read the Directus version (needs an admin token); assuming 11.');
    return;
  }
  if (Number.parseInt(String(version).split('.')[0], 10) < 11) {
    fail(`Directus ${version} found: this script needs Directus 11 (policies).`);
  }
  console.log(`  Directus ${version}`);
}

async function main() {
  if (DRY) {
    console.log('DRY RUN: nothing is sent anywhere; reads are assumed empty, so everything shows as "create".\n');
  } else {
    TOKEN = await getToken();
    console.log('▸ Server');
    await requireDirectus11();
  }

  console.log('▸ Folder');
  console.log(`  • ${FOLDER.name}: ${await ensureFolder()}`);

  console.log('▸ Collections');
  for (const [name, cfg] of Object.entries(COLLECTIONS)) {
    console.log(`  • ${name}: ${await ensureCollection(name, cfg)}`);
  }
  console.log('  • live_usage_ticks: skipped (composite primary key; Directus cannot manage it)');

  console.log('▸ Fields (read-only, sensitive columns hidden) and relations');
  for (const [name, cfg] of Object.entries(COLLECTIONS)) {
    await ensureFields(name, cfg.fields);
    await ensureAliases(name, cfg.aliases);
    console.log(`  • ${name}: ${Object.keys(cfg.fields).length} fields${cfg.aliases ? `, ${cfg.aliases.length} reverse relations` : ''}`);
  }

  console.log('▸ Roles and policies');
  const supportPolicy = await ensurePolicy(SUPPORT_POLICY, 'Read-only metadata of Live Connect: statuses, languages, durations, end reasons, credits. No names, no transcripts, no QR.', 'support_agent');
  const privacyPolicy = await ensurePolicy(PRIVACY_POLICY, 'Adds the "View transcript" button (reason mandatory, audited by the server).', 'policy');
  const supportRole = await ensureRole(SUPPORT, 'Live Connect support: read-only metadata.', 'support_agent');
  const privacyRole = await ensureRole(PRIVACY, 'Live Connect support + the audited transcript button.', 'policy');
  console.log(`  • ${SUPPORT_POLICY}: permissions ${await setPermissions(supportPolicy, supportPermissions())}`);
  console.log(`  • ${PRIVACY_POLICY}: permissions ${await setPermissions(privacyPolicy, privacyPermissions())}`);
  console.log(`  • ${SUPPORT} → ${SUPPORT_POLICY}: ${await ensureAccess(supportRole, supportPolicy)}`);
  console.log(`  • ${PRIVACY} → ${SUPPORT_POLICY}: ${await ensureAccess(privacyRole, supportPolicy)}`);
  console.log(`  • ${PRIVACY} → ${PRIVACY_POLICY}: ${await ensureAccess(privacyRole, privacyPolicy)}`);

  console.log('▸ Flows');
  for (const spec of flowSpecs(privacyRole)) {
    console.log(`  • ${spec.name}: ${await ensureFlow(spec)}`);
  }

  console.log('▸ Insights');
  const dash = await ensureDashboard(DASHBOARD, 'forum', 'Live Connect operations. Metadata only: statuses, end reasons, counts, credits.');
  buildInsights(dash);
  let ok = 0;
  for (const p of panels) {
    try {
      await api('/panels', { method: 'POST', body: p });
      ok++;
    } catch (e) {
      console.warn(`  ! panel "${p.name}": ${trunc(e)}`);
    }
  }
  console.log(`  • ${ok}/${panels.length} panels`);

  if (DRY) {
    console.log(`\n✓ Dry run complete: ${planned} write calls planned, none sent.`);
  } else {
    console.log('\n✓ Live Connect administration provisioned.');
    console.log('  Create operators in Directus (Users) and give them ONE of the two roles;');
    console.log('  this script creates no users. Existing roles are untouched: see the README for');
    console.log('  the permission filter that keeps live conversations out of them');
    console.log('  (node directus/setup-live-connect.mjs --print-transcript-filter).');
  }
}

function compact(v) {
  const s = JSON.stringify(v);
  return s.length > 220 ? s.slice(0, 220) + `… (${s.length} chars)` : s;
}
function trunc(e) {
  return String(e && e.message ? e.message : e).replace(/\s+/g, ' ').slice(0, 200);
}
function fail(msg) {
  console.error('✗ ' + msg);
  process.exit(1);
}

main().catch((e) => fail(trunc(e)));

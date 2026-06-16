// Provisions the VoxTranslate **backoffice** in Directus via the REST API:
//
//   1. Registers every app table as a managed Directus collection, grouped into
//      folders with icons, colours and display templates — so the whole data
//      model reads like an admin product, not raw SQL tables.
//   2. Builds four **Insights dashboards** of KPI panels: Overview, Billing &
//      Stripe (revenue / promo spend / movements), Moderation, and Acquisition &
//      Features (incl. users-by-`source` campaign attribution).
//
// Idempotent: re-running PATCHes collection metadata in place and rebuilds each
// dashboard's panels (so tweaking this file and re-running just refreshes the
// layout). Like setup-bonus-flow.mjs, dashboards/panels live in Directus's own
// `directus_dashboards`/`directus_panels` tables (not the app schema), so they're
// provisioned through the API, not an app `.sql` migration.
//
//   DIRECTUS_URL=https://directus-production-ad16.up.railway.app \
//     DIRECTUS_ADMIN_EMAIL=… DIRECTUS_ADMIN_PASSWORD=…   # or DIRECTUS_TOKEN=<static admin token>
//     node directus/setup-backoffice.mjs
//
// The privileged action buttons (Ban / Credit / Bonus / Resolve report / GDPR
// delete) are a separate concern — see setup-bonus-flow.mjs and README §7.

const base = (process.env.DIRECTUS_URL || '').replace(/\/$/, '');
if (!base) fail('Set DIRECTUS_URL to your Directus base URL.');

// ──────────────────────────────────────────────────────────────────────────
// Tiny REST client. Throws on non-2xx so callers can try/catch (registration
// probes several endpoints); fatal config problems use fail() → exit.
// ──────────────────────────────────────────────────────────────────────────
let TOKEN = '';
async function api(path, { method = 'GET', body } = {}) {
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

// ──────────────────────────────────────────────────────────────────────────
// Data model: folders + collection metadata. Display templates use the same
// {{field}} syntax as the Directus UI, so list panels and relational pickers
// render a human label instead of a UUID.
// ──────────────────────────────────────────────────────────────────────────
const FOLDERS = [
  { name: 'accounts', icon: 'group', color: '#2ECDA7', note: 'Users, credits & billing ledger' },
  { name: 'sessions', icon: 'record_voice_over', color: '#3399FF', note: 'Calls, participants, transcripts & files' },
  { name: 'moderation', icon: 'shield', color: '#E35169', note: 'Reports, bans, blocklist & audit trail' },
  { name: 'ai_features', icon: 'smart_toy', color: '#6644FF', note: 'AI reports, sentiment, emails & glossaries' },
  { name: 'content', icon: 'language', color: '#FFA439', note: 'UI strings & legal pages (multilingual)' },
];

// Each: [collection, { group, icon, color, display, note, sort }]
const COLLECTIONS = [
  // accounts
  ['users', { group: 'accounts', icon: 'person', display: '{{name}} · {{email}}', note: 'Registered accounts. balance / banned_until change ONLY via Flow buttons.', sort: 1 }],
  ['credit_transactions', { group: 'accounts', icon: 'receipt_long', display: '{{kind}} {{amount}} → {{balance_after}}', note: 'Immutable money ledger (signed amounts).', sort: 2 }],
  ['usage_sessions', { group: 'accounts', icon: 'mic', display: '{{room}} · {{speaking_seconds}}s', note: 'Per-call speaking time billed to a user.', sort: 3 }],
  ['stripe_events', { group: 'accounts', icon: 'payments', display: '{{type}} · {{id}}', note: 'Stripe webhook idempotency log (one row per event).', sort: 4 }],
  // sessions
  ['call_sessions', { group: 'sessions', icon: 'videocam', display: '{{room}} · {{started_at}}', note: 'One row per room lifetime. ended_at NULL = live.', sort: 1 }],
  ['session_participants', { group: 'sessions', icon: 'groups', display: '{{name}} ({{lang}})', note: 'Who took part in each call.', sort: 2 }],
  ['transcript_events', { group: 'sessions', icon: 'subtitles', display: '{{speaker_name}}: {{original_text}}', note: 'Finalized speech + chat, with the translation fan-out.', sort: 3 }],
  ['transcript_bookmarks', { group: 'sessions', icon: 'bookmark', display: '{{label}}', note: 'Moments pinned during a call.', sort: 4 }],
  ['chat_files', { group: 'sessions', icon: 'attach_file', display: '{{file_name}} ({{file_type}})', note: 'Files shared in the room chat.', sort: 5 }],
  // moderation
  ['reports', { group: 'moderation', icon: 'flag', color: '#E35169', display: '{{reason}} · {{status}}', note: 'Abuse reports. Resolve via the Flow button.', sort: 1 }],
  ['bug_reports', { group: 'moderation', icon: 'bug_report', color: '#E35169', display: '{{message}} · {{status}}', note: 'User-submitted bug/error reports (spec 0071). Triage via status: received → cancelled | resolved, then delete.', sort: 2 }],
  ['blocklist_terms', { group: 'moderation', icon: 'block', display: '{{term}}', note: 'Moderation blocklist (loaded by the server at startup).', sort: 3 }],
  ['admin_audit', { group: 'moderation', icon: 'history', display: '{{action}} · {{actor}}', note: 'Every privileged backoffice action.', sort: 4 }],
  // ai_features
  ['session_reports', { group: 'ai_features', icon: 'description', display: '{{format}} · {{lang}}', note: 'AI-generated meeting reports.', sort: 1 }],
  ['session_sentiments', { group: 'ai_features', icon: 'mood', display: '{{model}}', note: 'Per-session sentiment analysis (cached).', sort: 2 }],
  ['session_emails', { group: 'ai_features', icon: 'mail', display: '{{subject}} · {{status}}', note: 'Follow-up email drafts and sends.', sort: 3 }],
  ['room_glossaries', { group: 'ai_features', icon: 'menu_book', display: '{{name}} · {{room}}', note: 'Per-room glossary headers.', sort: 4 }],
  ['glossary_entries', { group: 'ai_features', icon: 'translate', display: '{{source_term}} → {{target_term}}', note: 'Glossary term pairs injected into translation.', sort: 5 }],
  // content
  ['languages', { group: 'content', icon: 'flag', display: '{{name}} ({{code}})', note: 'Supported UI languages.', sort: 1 }],
  ['i18n_strings', { group: 'content', icon: 'text_fields', display: '{{key}}', note: 'UI string keys (edit values via the Translations interface).', sort: 2 }],
  ['i18n_translations', { group: 'content', icon: 'g_translate', display: '{{language}}: {{value}}', note: 'Per-language UI string values.', sort: 3 }],
  ['legal_pages', { group: 'content', icon: 'gavel', display: '{{slug}} · {{version}}', note: 'Terms / privacy / acceptable-use base rows.', sort: 4 }],
  ['legal_translations', { group: 'content', icon: 'gavel', display: '{{language}}: {{title}}', note: 'Per-language legal page bodies (markdown).', sort: 5 }],
];

async function ensureFolder({ name, icon, color, note }) {
  const meta = { icon, color, note, collapse: 'open' };
  try {
    await api('/collections', { method: 'POST', body: { collection: name, schema: null, meta } });
    return 'created';
  } catch {
    try {
      await api(`/collections/${name}`, { method: 'PATCH', body: { meta } });
      return 'updated';
    } catch (e) {
      console.warn(`  ! folder ${name}: ${trunc(e)}`);
      return 'failed';
    }
  }
}

async function ensureCollection(name, cfg) {
  const meta = {
    icon: cfg.icon,
    color: cfg.color || null,
    note: cfg.note || null,
    display_template: cfg.display || null,
    group: cfg.group || null,
    sort: cfg.sort || null,
    hidden: false,
  };
  // PATCH first — the common case is a table already managed (README §4 had the
  // operator enable several by hand). For an as-yet-unmanaged table, fall back to
  // POST with schema:null, which registers the metadata against the existing
  // table WITHOUT attempting a CREATE TABLE.
  try {
    await api(`/collections/${name}`, { method: 'PATCH', body: { meta } });
    return 'updated';
  } catch (ePatch) {
    try {
      await api('/collections', { method: 'POST', body: { collection: name, schema: null, meta } });
      return 'registered';
    } catch (ePost) {
      console.warn(`  ! ${name}: ${trunc(ePatch)} | ${trunc(ePost)}`);
      return 'failed';
    }
  }
}

// bug_reports.status drives triage — present it as a coloured dropdown (the same
// values the DB CHECK enforces) instead of a free-text input, and render it as a
// label badge in lists. Idempotent: PATCHes the auto-discovered field's metadata.
const BUG_STATUS_CHOICES = [
  { text: 'Received', value: 'received', color: '#FFA439' },
  { text: 'Resolved', value: 'resolved', color: '#2ECDA7' },
  { text: 'Cancelled', value: 'cancelled', color: '#E35169' },
];
async function ensureBugReportStatusField() {
  const meta = {
    interface: 'select-dropdown',
    options: { choices: BUG_STATUS_CHOICES },
    display: 'labels',
    display_options: { choices: BUG_STATUS_CHOICES, showAsDot: false },
    width: 'half',
  };
  try {
    await api('/fields/bug_reports/status', { method: 'PATCH', body: { meta } });
    return 'configured';
  } catch (e) {
    console.warn(`  ! bug_reports.status dropdown: ${trunc(e)}`);
    return 'failed';
  }
}

// ──────────────────────────────────────────────────────────────────────────
// Insights: dashboards + panels. Panels are pure config over the items API
// (no SQL), so they work as long as the collection is registered above.
// ──────────────────────────────────────────────────────────────────────────
async function ensureDashboard(name, icon, note) {
  const found = (
    await api(`/dashboards?filter[name][_eq]=${encodeURIComponent(name)}&fields=id&limit=1`)
  ).data;
  if (found.length) {
    const id = found[0].id;
    const panels = (await api(`/panels?filter[dashboard][_eq]=${id}&fields=id&limit=-1`)).data;
    for (const p of panels) await api(`/panels/${p.id}`, { method: 'DELETE' });
    await api(`/dashboards/${id}`, { method: 'PATCH', body: { icon, note } });
    return id;
  }
  return (await api('/dashboards', { method: 'POST', body: { name, icon, note } })).data.id;
}

const panels = []; // accumulate, create at the end

// 4-wide grid of metric tiles. row()/full() return positions so sections never overlap.
const COL_X = [1, 7, 13, 19]; // x for columns 0..3 (each metric is 6 wide)
function metric(dash, idx, baseY, p) {
  panels.push({
    dashboard: dash,
    name: p.name,
    note: p.note || null,
    icon: p.icon || 'analytics',
    color: p.color || null,
    show_header: true,
    type: 'metric',
    position_x: COL_X[idx % 4],
    position_y: baseY + Math.floor(idx / 4) * 6,
    width: 6,
    height: 6,
    options: {
      collection: p.collection,
      field: p.field || 'id',
      function: p.fn || 'count',
      sortField: null,
      filter: p.filter || {},
      prefix: p.prefix || '',
      suffix: p.suffix || '',
      abbreviate: true,
      conditionalFormatting: [],
    },
  });
}
function timeseries(dash, { x = 1, y, w = 24, h = 11, name, note, color, collection, dateField, fn = 'count', field = 'id', filter, precision = 'day', range = '12 weeks' }) {
  panels.push({
    dashboard: dash, name, note: note || null, icon: 'show_chart', color: color || null,
    show_header: true, type: 'time-series', position_x: x, position_y: y, width: w, height: h,
    options: {
      collection, dateField, valueField: field, function: fn, precision, range,
      filter: filter || {}, color: color || null, fillHoles: true, showXAxis: true, showYAxis: true,
    },
  });
}
function list(dash, { x = 1, y, w = 12, h = 12, name, note, collection, sortField = 'created_at', sortDirection = 'desc', limit = 8, filter }) {
  panels.push({
    dashboard: dash, name, note: note || null, icon: 'list', color: null,
    show_header: true, type: 'list', position_x: x, position_y: y, width: w, height: h,
    options: { collection, limit, sortField, sortDirection, filter: filter || {} },
  });
}
function barchart(dash, { x = 1, y, w = 24, h = 14, name, note, color, collection, xField, fn = 'count', field = 'id', filter, horizontal = true }) {
  panels.push({
    dashboard: dash, name, note: note || null, icon: 'bar_chart', color: color || null,
    show_header: true, type: 'bar-chart', position_x: x, position_y: y, width: w, height: h,
    options: {
      collection, xAxis: xField, yAxis: field, function: fn, filter: filter || {},
      horizontal, color: color || null, showDataLabel: true, sortDirection: 'desc',
    },
  });
}
// Interactive dashboard filter: a text input whose value is exposed as a dashboard
// variable named `field`. Other panels reference it in their filters as `{{field}}`
// (e.g. a list with `{ message: { _icontains: '{{bug_q}}' } }`), so typing here
// live-filters those panels. Empty default → `_icontains: ''` matches everything.
function globalVar(dash, { x = 1, y, w = 24, h = 4, name, field, defaultValue = '', placeholder = '' }) {
  panels.push({
    dashboard: dash, name, note: null, icon: 'search', color: null,
    show_header: true, type: 'global-variable', position_x: x, position_y: y, width: w, height: h,
    options: { field, type: 'string', defaultValue, interface: 'input', options: { placeholder, iconLeft: 'search' } },
  });
}

const NOW30 = '$NOW(-30 days)';
const PURCHASE = { kind: { _eq: 'purchase' } };

function buildOverview(d) {
  let i = 0;
  metric(d, i++, 1, { name: 'Total users', icon: 'group', color: '#2ECDA7', collection: 'users' });
  metric(d, i++, 1, { name: 'New users (30d)', icon: 'person_add', color: '#2ECDA7', collection: 'users', filter: { created_at: { _gte: NOW30 } } });
  metric(d, i++, 1, { name: 'Banned (active)', icon: 'block', color: '#E35169', collection: 'users', filter: { banned_until: { _gt: '$NOW' } } });
  metric(d, i++, 1, { name: 'Total calls', icon: 'videocam', color: '#3399FF', collection: 'call_sessions' });
  metric(d, i++, 1, { name: 'Live calls now', icon: 'sensors', color: '#3399FF', collection: 'call_sessions', filter: { ended_at: { _null: true } } });
  metric(d, i++, 1, { name: 'Translations', icon: 'translate', color: '#6644FF', collection: 'transcript_events', filter: { event_type: { _eq: 'speech' } }, note: 'Finalized speech utterances' });
  metric(d, i++, 1, { name: 'Chat messages', icon: 'chat', collection: 'transcript_events', filter: { event_type: { _eq: 'chat' } } });
  metric(d, i++, 1, { name: 'Files shared', icon: 'attach_file', collection: 'chat_files' });
  const tsY = 1 + Math.ceil(i / 4) * 6;
  timeseries(d, { x: 1, y: tsY, w: 12, name: 'New users / day', color: '#2ECDA7', collection: 'users', dateField: 'created_at' });
  timeseries(d, { x: 13, y: tsY, w: 12, name: 'Calls / day', color: '#3399FF', collection: 'call_sessions', dateField: 'started_at' });
}

function buildBilling(d) {
  let i = 0;
  // Guadagni (income) — completed Stripe purchases.
  metric(d, i++, 1, { name: 'Revenue (Stripe)', icon: 'payments', color: '#2ECDA7', prefix: '$', fn: 'sum', field: 'amount', collection: 'credit_transactions', filter: PURCHASE, note: 'Completed Stripe purchases' });
  metric(d, i++, 1, { name: 'Purchases', icon: 'shopping_cart', color: '#2ECDA7', collection: 'credit_transactions', filter: PURCHASE });
  metric(d, i++, 1, { name: 'Revenue (30d)', icon: 'trending_up', color: '#2ECDA7', prefix: '$', fn: 'sum', field: 'amount', collection: 'credit_transactions', filter: { _and: [PURCHASE, { created_at: { _gte: NOW30 } }] } });
  metric(d, i++, 1, { name: 'Avg purchase', icon: 'sell', prefix: '$', fn: 'avg', field: 'amount', collection: 'credit_transactions', filter: PURCHASE });
  // Spese (outgoing) — promotional credit we give away + outstanding liability.
  metric(d, i++, 1, { name: 'Promo credits granted', icon: 'redeem', color: '#FFA439', prefix: '$', fn: 'sum', field: 'amount', collection: 'credit_transactions', filter: { kind: { _in: ['free_credit', 'bonus'] } }, note: 'Welcome + admin bonus credits (cost of acquisition)' });
  metric(d, i++, 1, { name: 'Bonus credits', icon: 'card_giftcard', color: '#FFA439', prefix: '$', fn: 'sum', field: 'amount', collection: 'credit_transactions', filter: { kind: { _eq: 'bonus' } } });
  metric(d, i++, 1, { name: 'Credits spent', icon: 'mic', prefix: '$', fn: 'sum', field: 'amount', collection: 'credit_transactions', filter: { kind: { _eq: 'usage' } }, note: 'Speaking-time consumption (negative)' });
  metric(d, i++, 1, { name: 'Outstanding balance', icon: 'account_balance_wallet', prefix: '$', fn: 'sum', field: 'balance', collection: 'users', note: 'Unspent credits held by users (liability)' });
  // AI feature charges (the `feature` column is set for report/sentiment/email).
  metric(d, i++, 1, { name: 'AI feature charges', icon: 'smart_toy', color: '#6644FF', collection: 'credit_transactions', filter: { feature: { _nnull: true } }, note: 'Report / sentiment / email charges' });
  const tsY = 1 + Math.ceil(i / 4) * 6;
  timeseries(d, { x: 1, y: tsY, w: 24, h: 10, name: 'Revenue / day (Stripe)', color: '#2ECDA7', collection: 'credit_transactions', dateField: 'created_at', fn: 'sum', field: 'amount', filter: PURCHASE });
  const listY = tsY + 10;
  list(d, { x: 1, y: listY, w: 12, name: 'Recent Stripe events', collection: 'stripe_events', sortField: 'processed_at', note: 'Webhook movements log' });
  list(d, { x: 13, y: listY, w: 12, name: 'Recent transactions', collection: 'credit_transactions', sortField: 'created_at' });
}

function buildModeration(d) {
  let i = 0;
  metric(d, i++, 1, { name: 'Open reports', icon: 'flag', color: '#E35169', collection: 'reports', filter: { status: { _eq: 'open' } } });
  metric(d, i++, 1, { name: 'Resolved', icon: 'task_alt', color: '#2ECDA7', collection: 'reports', filter: { status: { _eq: 'resolved' } } });
  metric(d, i++, 1, { name: 'Dismissed', icon: 'do_not_disturb_on', collection: 'reports', filter: { status: { _eq: 'dismissed' } } });
  metric(d, i++, 1, { name: 'Banned (active)', icon: 'block', color: '#E35169', collection: 'users', filter: { banned_until: { _gt: '$NOW' } } });
  metric(d, i++, 1, { name: 'Blocklist terms', icon: 'dangerous', collection: 'blocklist_terms' });
  metric(d, i++, 1, { name: 'Admin actions', icon: 'history', collection: 'admin_audit' });
  const tsY = 1 + Math.ceil(i / 4) * 6;
  timeseries(d, { x: 1, y: tsY, w: 24, h: 10, name: 'Reports / day', color: '#E35169', collection: 'reports', dateField: 'created_at' });
  const listY = tsY + 10;
  list(d, { x: 1, y: listY, w: 12, name: 'Open reports', collection: 'reports', filter: { status: { _eq: 'open' } }, note: 'Oldest first → resolve via Flow' });
  list(d, { x: 13, y: listY, w: 12, name: 'Recent admin actions', collection: 'admin_audit' });

  // --- Bug reports (spec 0071): status counts + a searchable list ---
  const bugY = listY + 12;
  metric(d, 0, bugY, { name: 'Bug reports — new', icon: 'bug_report', color: '#FFA439', collection: 'bug_reports', filter: { status: { _eq: 'received' } } });
  metric(d, 1, bugY, { name: 'Bug — resolved', icon: 'task_alt', color: '#2ECDA7', collection: 'bug_reports', filter: { status: { _eq: 'resolved' } } });
  metric(d, 2, bugY, { name: 'Bug — cancelled', icon: 'do_not_disturb_on', color: '#E35169', collection: 'bug_reports', filter: { status: { _eq: 'cancelled' } } });
  metric(d, 3, bugY, { name: 'Bug reports — total', icon: 'bug_report', collection: 'bug_reports' });
  // Search box → drives the list below via the `bug_q` dashboard variable.
  const searchY = bugY + 6;
  globalVar(d, { x: 1, y: searchY, w: 24, h: 4, name: '🔎 Search bug reports', field: 'bug_q', placeholder: 'Type to filter by message…' });
  list(d, { x: 1, y: searchY + 4, w: 24, h: 12, name: 'Bug reports', collection: 'bug_reports', sortField: 'created_at', sortDirection: 'desc', limit: 12, filter: { message: { _icontains: '{{bug_q}}' } }, note: 'Newest first; filtered by the search box above' });
}

function buildAcquisition(d) {
  let i = 0;
  metric(d, i++, 1, { name: 'Attributed users', icon: 'campaign', color: '#2ECDA7', collection: 'users', filter: { source: { _nnull: true } }, note: 'Arrived via ?source / utm_source' });
  metric(d, i++, 1, { name: 'Organic users', icon: 'eco', collection: 'users', filter: { source: { _null: true } } });
  metric(d, i++, 1, { name: 'Glossaries', icon: 'menu_book', color: '#6644FF', collection: 'room_glossaries' });
  metric(d, i++, 1, { name: 'Glossary terms', icon: 'translate', color: '#6644FF', collection: 'glossary_entries' });
  metric(d, i++, 1, { name: 'Bookmarks', icon: 'bookmark', collection: 'transcript_bookmarks' });
  metric(d, i++, 1, { name: 'AI reports', icon: 'description', color: '#6644FF', collection: 'session_reports' });
  metric(d, i++, 1, { name: 'Sentiment runs', icon: 'mood', color: '#6644FF', collection: 'session_sentiments' });
  metric(d, i++, 1, { name: 'Follow-up emails', icon: 'mail', collection: 'session_emails' });
  const chartY = 1 + Math.ceil(i / 4) * 6;
  barchart(d, { x: 1, y: chartY, w: 24, h: 14, name: 'Users by source', color: '#2ECDA7', collection: 'users', xField: 'source', filter: { source: { _nnull: true } }, note: 'Where registered users came from' });
}

// ──────────────────────────────────────────────────────────────────────────
// Run.
// ──────────────────────────────────────────────────────────────────────────
async function main() {
  TOKEN = await getToken();

  console.log('▸ Data model: folders');
  for (const f of FOLDERS) console.log(`  • ${f.name}: ${await ensureFolder(f)}`);

  console.log('▸ Data model: collections');
  for (const [name, cfg] of COLLECTIONS) console.log(`  • ${name}: ${await ensureCollection(name, cfg)}`);

  console.log('▸ Field interfaces');
  console.log(`  • bug_reports.status (dropdown): ${await ensureBugReportStatusField()}`);

  console.log('▸ Insights dashboards');
  const dOverview = await ensureDashboard('📊 Overview', 'insights', 'Top-line KPIs across users, calls and translations.');
  const dBilling = await ensureDashboard('💳 Billing & Stripe', 'payments', 'Revenue (Stripe purchases), promo spend and movements.');
  const dModeration = await ensureDashboard('🛡️ Moderation', 'shield', 'Reports, bans and the admin audit trail.');
  const dAcquisition = await ensureDashboard('🚀 Acquisition & Features', 'rocket_launch', 'Where users come from (source) and feature usage.');

  buildOverview(dOverview);
  buildBilling(dBilling);
  buildModeration(dModeration);
  buildAcquisition(dAcquisition);

  let ok = 0;
  for (const p of panels) {
    try {
      await api('/panels', { method: 'POST', body: p });
      ok++;
    } catch (e) {
      console.warn(`  ! panel "${p.name}": ${trunc(e)}`);
    }
  }
  console.log(`  • ${ok}/${panels.length} panels created across 4 dashboards`);

  console.log('\n✓ Backoffice provisioned. Open Directus → Insights for the dashboards,');
  console.log('  and Content for the grouped collections. Privileged actions (ban/credit/');
  console.log('  bonus/resolve/delete) are the Flow buttons — see setup-bonus-flow.mjs + README §7.');
}

function trunc(e) {
  return String(e && e.message ? e.message : e).replace(/\s+/g, ' ').slice(0, 160);
}
function fail(msg) {
  console.error('✗ ' + msg);
  process.exit(1);
}

main().catch((e) => fail(trunc(e)));

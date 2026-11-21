// Creates the Live Connect OPERATORS in Directus and gives each exactly ONE of the two roles that
// `setup-live-connect.mjs` provisions:
//
//   "Live Connect Support"  read-only metadata (no names, no transcripts, no QR)
//   "Live Connect Privacy"  Support + the audited "View transcript" button
//
//   DIRECTUS_URL=https://<your-directus> DIRECTUS_TOKEN=<static admin token> \
//     node directus/seed-live-connect-operators.mjs \
//       --support support@example.com --privacy privacy@example.com [--dry-run]
//
// Repeat `--support` / `--privacy` for several people. Idempotent: an operator that already exists
// keeps their account; their role is set to the requested one ONLY if they have no role yet or one of
// the two Live Connect roles (a person is never moved off an unrelated role, and never gets both).
//
// No password is set: Directus can only send an invitation when mail is configured, so an
// administrator sets the password from the user's page (or configures mail first). Until then the
// account exists but cannot sign in. Nothing here ever prints a token or a password.

const argv = process.argv.slice(2);
const DRY = argv.includes('--dry-run');

function collect(flag) {
  const out = [];
  for (let i = 0; i < argv.length; i++) if (argv[i] === flag && argv[i + 1]) out.push(argv[++i].trim().toLowerCase());
  return out;
}

const SUPPORT = collect('--support');
const PRIVACY = collect('--privacy');
const EMAIL = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

function fail(msg) {
  console.error('✗ ' + msg);
  process.exit(1);
}

for (const e of [...SUPPORT, ...PRIVACY]) if (!EMAIL.test(e)) fail(`Not an e-mail address: ${e}`);
const both = SUPPORT.filter((e) => PRIVACY.includes(e));
if (both.length) fail(`One person gets ONE role, but listed twice: ${both.join(', ')}`);
if (!SUPPORT.length && !PRIVACY.length) fail('Give at least one --support <email> or --privacy <email>.');

const base = (process.env.DIRECTUS_URL || '').replace(/\/$/, '');
const token = process.env.DIRECTUS_TOKEN;
if (!DRY && (!base || !token)) fail('Set DIRECTUS_URL and DIRECTUS_TOKEN (a static admin token).');

async function api(method, path, body) {
  const res = await fetch(base + path, {
    method,
    headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    /* not JSON */
  }
  if (!res.ok) fail(`${method} ${path} → ${res.status}: ${json?.errors?.map((e) => e.message).join('; ') || text.slice(0, 200)}`);
  return json?.data;
}

const ROLE_NAMES = { support: 'Live Connect Support', privacy: 'Live Connect Privacy' };

if (DRY) {
  console.log('Dry run: nothing is sent.');
  for (const e of SUPPORT) console.log(`  would ensure ${e} → ${ROLE_NAMES.support}`);
  for (const e of PRIVACY) console.log(`  would ensure ${e} → ${ROLE_NAMES.privacy}`);
  process.exit(0);
}

const roles = await api('GET', `/roles?limit=-1&fields=id,name&filter[name][_in]=${encodeURIComponent(Object.values(ROLE_NAMES).join(','))}`);
const roleId = Object.fromEntries(roles.map((r) => [r.name, r.id]));
for (const n of Object.values(ROLE_NAMES)) if (!roleId[n]) fail(`Role "${n}" does not exist: run setup-live-connect.mjs first.`);
const liveRoleIds = new Set(Object.values(roleId));

async function ensure(email, roleName) {
  const wanted = roleId[roleName];
  const found = await api('GET', `/users?limit=1&fields=id,role,status&filter[email][_eq]=${encodeURIComponent(email)}`);
  if (!found?.length) {
    await api('POST', '/users', { email, role: wanted, status: 'active', first_name: roleName.replace('Live Connect ', 'LC '), last_name: 'operator' });
    console.log(`  + ${email} created → ${roleName}`);
    return;
  }
  const u = found[0];
  const roleOf = typeof u.role === 'object' && u.role ? u.role.id : u.role;
  if (roleOf === wanted) {
    console.log(`  = ${email} already ${roleName}`);
  } else if (!roleOf || liveRoleIds.has(roleOf)) {
    await api('PATCH', `/users/${u.id}`, { role: wanted });
    console.log(`  ~ ${email} moved to ${roleName}`);
  } else {
    console.log(`  ! ${email} has an unrelated role: left alone (change it by hand if you really mean it)`);
  }
}

console.log('Live Connect operators');
for (const e of SUPPORT) await ensure(e, ROLE_NAMES.support);
for (const e of PRIVACY) await ensure(e, ROLE_NAMES.privacy);
console.log('✓ Done. No password was set: set it from the user page in Directus, or configure mail first.');

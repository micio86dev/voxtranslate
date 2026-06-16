# Database backups (PITR + off-site encrypted dumps)

**Why.** The Postgres DB on **Supabase** holds the data that can't be regenerated:
user accounts/PII, the credits & billing ledger, call sessions and transcripts —
GDPR-relevant (spec 0006). Supabase's *default* on the Pro plan is **daily** backups
(7-day retention); the Free tier has **none**. This runbook sets up two complementary
layers so a single failure (accidental delete, bad migration, region/account loss)
never costs the data:

1. **Supabase PITR** — continuous recovery *inside* Supabase (≈2-min RPO).
2. **Off-site encrypted `pg_dump`** — independent, portable copies on a bucket *you*
   own, so losing the Supabase project/account doesn't take the backups with it.

> **Not covered here:** file blobs in **Supabase Storage** (chat uploads, spec 0018)
> live in object storage, not the DB — back those up separately (bucket replication
> or a periodic sync). This runbook is the **database**.

---

## 1. Layer 1 — enable Supabase PITR (owner, Supabase dashboard)

The single biggest upgrade: from one-per-day to **any point in time, ~2-minute
granularity**. It's a paid add-on and **requires at least the Small compute add-on**.
Enabling PITR *replaces* the daily backups (it's strictly finer).

1. Supabase → your project → **Database → Backups → Point in Time**.
2. Enable PITR and pick a **retention**: 7 / 14 / 28 days (cost scales with retention;
   check the current price in the dashboard). 14 days is a sensible default.
3. Done — recovery points now archive every ~2 min. **Restore** = Backups → Point in
   Time → pick a timestamp → restore (the project is briefly offline during restore;
   downtime scales with DB size).

That alone already beats daily/weekly. Layer 2 covers what PITR can't: **independence
from Supabase itself**.

## 2. Layer 2 — off-site encrypted dumps (this repo)

`.github/workflows/db-backup.yml` runs every 6 h (and on demand via *Run workflow*):
`pg_dump` the `public` schema → **integrity-check** (`pg_restore --list`) → **AES-256
encrypt** (GPG, symmetric) → upload to an **S3-compatible** bucket. It **skips cleanly
until the secrets below are set**, so it won't spam failures before you configure it.

### 2.1 Pick storage (S3-compatible)

Any of S3 / **Cloudflare R2** / **Backblaze B2**. **R2 is recommended**: S3-compatible,
**no egress fees**, and you can pin an **EU jurisdiction** (keeps the GDPR surface tidy).
Create a private bucket (e.g. `voxtranslate-db-backups`) and a scoped access key.

> ⚠️ **R2 jurisdiction must be consistent or you get `AccessDenied` on upload.** If the
> bucket is created with **EU** jurisdiction, then **all three** must be EU: the bucket,
> the **endpoint** (`https://<acct>.eu.r2.cloudflarestorage.com` — note the `.eu.`), and
> the **API token** (created with the EU jurisdiction). A `Default` token / non-`.eu.`
> endpoint against an EU bucket fails. Simplest if you don't need EU: use **Default**
> everywhere (dumps are already AES-256-encrypted client-side, so the bucket only ever
> holds ciphertext).

### 2.2 (Recommended) a least-privilege backup role

Instead of the superuser string, give the workflow a **read-only** role. In the Supabase
SQL editor:

```sql
CREATE ROLE vox_backup WITH LOGIN PASSWORD '<strong-random>';
GRANT CONNECT ON DATABASE postgres TO vox_backup;
GRANT USAGE ON SCHEMA public TO vox_backup;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO vox_backup;
GRANT SELECT ON ALL SEQUENCES IN SCHEMA public TO vox_backup;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO vox_backup;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON SEQUENCES TO vox_backup;
```

Build its connection string from Supabase → **Connect** (or Database → Connection
string). GitHub-hosted runners are **IPv4-only** while Supabase's *direct* connection
is IPv6-only, so use the **Session pooler** string (host `…pooler.supabase.com`, port
`5432`) — it's IPv4 **and** gives `pg_dump` the real session it needs. Do **not** use
the *Transaction pooler* (port `6543`): it can't run `pg_dump`.

### 2.3 Secrets

The workflow's job runs in the **`Production`** environment, so add these as
**environment secrets** there: Settings → **Environments → Production → Add secret**
(repository-level Actions secrets also work if you drop the `environment:` line):

| Secret | Value |
|---|---|
| `BACKUP_DATABASE_URL` | the `vox_backup` (or `postgres`) session connection string, port 5432 |
| `BACKUP_PASSPHRASE` | a long random passphrase for AES-256 — **store it somewhere safe too** |
| `BACKUP_S3_BUCKET` | bucket name, e.g. `voxtranslate-db-backups` |
| `BACKUP_S3_ENDPOINT` | R2: `https://<acct>.r2.cloudflarestorage.com` · B2: `https://s3.<region>.backblazeb2.com` · S3: leave empty |
| `BACKUP_S3_REGION` | R2: `auto` · B2/S3: the bucket region |
| `BACKUP_S3_ACCESS_KEY_ID` / `BACKUP_S3_SECRET_ACCESS_KEY` | the scoped bucket key |

> ⚠️ **If you lose `BACKUP_PASSPHRASE`, the dumps are unrecoverable.** Keep a copy in your
> password manager — it is *not* stored anywhere else.

Then trigger once: Actions → **DB backup (off-site, encrypted)** → *Run workflow*, and
confirm a `voxtranslate-<ts>.dump.gpg` object appears under `db-backups/`.

### 2.4 Retention (bucket lifecycle, not the workflow)

The workflow only *writes*; deleting via a script is risky, so retention is a **bucket
lifecycle rule** (set once): expire objects under `db-backups/` after e.g. **30 days**.
R2/B2/S3 all support this in their console. With a 6 h cadence that's ~120 live copies.
(Scope the rule to the `db-backups/` **prefix** so it never touches `storage/` below.)

### 2.5 Supabase Storage file blobs (chat uploads)

The DB backup covers *rows*; the chat-upload **file blobs** (spec 0018) live in the
Supabase Storage bucket **`chat-files`**, keyed `‹session›/‹uuid›.‹ext›`. A second
workflow — `.github/workflows/storage-backup.yml` (daily) — mirrors them to the **same
R2 bucket** under the `storage/chat-files/` prefix via `rclone` S3→S3. Uploads are
immutable UUIDs, so it's an additive `rclone copy` (incremental; never deletes).

Add these to the **`Production`** environment (the existing R2 secrets are reused as the
destination):

| Secret | Value |
|---|---|
| `BACKUP_SUPABASE_S3_ENDPOINT` | `https://<project-ref>.supabase.co/storage/v1/s3` |
| `BACKUP_SUPABASE_S3_REGION` | the project's region, e.g. `eu-central-1` (shown next to the keys) |
| `BACKUP_SUPABASE_S3_ACCESS_KEY_ID` / `BACKUP_SUPABASE_S3_SECRET_ACCESS_KEY` | Supabase → **Storage → S3 Configuration → Access keys → New access key** |
| `BACKUP_SUPABASE_BUCKET` | optional; defaults to `chat-files` |

> rclone env-var remotes register under a **lowercase** name and the reference is
> case-sensitive — the workflow uses `supa:` / `r2:` (not `SUPA:`/`R2:`).

**Restore** a blob (or the whole set) is the reverse copy — into a new bucket or back
into Supabase:

```sh
rclone copy r2:<R2_BUCKET>/storage/chat-files ./chat-files-restore   # to local
# …or straight back into a (new) Supabase bucket, with the supa: remote configured:
rclone copy r2:<R2_BUCKET>/storage/chat-files supa:chat-files
```

## 3. Restore from an off-site dump (disaster recovery / drill)

Restore into a **fresh** database (a new Supabase project, or local `postgres:17`):

```sh
# 1. download the chosen object
aws s3 cp "s3://$BUCKET/db-backups/voxtranslate-<ts>.dump.gpg" ./restore.dump.gpg \
  --endpoint-url "$S3_ENDPOINT"
# 2. decrypt
gpg --batch --decrypt --passphrase "$BACKUP_PASSPHRASE" --output restore.dump restore.dump.gpg
# 3. inspect (sanity)
pg_restore --list restore.dump | head
# 4. restore into the TARGET (fresh DB)
pg_restore --no-owner --no-privileges --schema=public \
  --dbname "$TARGET_DATABASE_URL" restore.dump
```

> Onto a **fresh** DB, omit `--clean`. To overwrite an existing DB instead, add
> `--clean --if-exists` — this **drops** the existing objects first, so only do it on a
> target you intend to replace. Prefer restoring to a new DB and repointing the app.

## 4. Verify (don't trust an untested backup)

Quarterly (and after any schema change), do a **restore drill**: run §3 against a throwaway
`postgres:17` (Docker) and spot-check row counts on `users`, `credit_transactions`,
`bug_reports`. A backup you've never restored is a hypothesis, not a backup.

## 5. Security & GDPR notes

- Dumps are **encrypted client-side** (AES-256) before they ever leave the runner; the
  bucket only ever holds ciphertext.
- Use a **private** bucket + a **scoped** key (write-only to `db-backups/` if your
  provider supports per-prefix policies).
- A new storage processor/region is a GDPR processing change — record it in the
  processor list + retention schedule (spec 0006). PITR (Layer 1) stays inside Supabase,
  so it adds no new processor.
- The dump contains PII; treat the passphrase + bucket key as production secrets.

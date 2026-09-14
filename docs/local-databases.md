# Three databases, and never the wrong one

A developer machine talks to **two** databases and must never talk to a third.

| | what runs against it | how it is chosen |
|---|---|---|
| **test** | `cargo test`, `cargo llvm-cov` | `DATABASE_URL` exported for the command |
| **local dev** | the server you run yourself | `DATABASE_URL` in `server/.env` |
| **staging / production** | only deployments | injected by Railway, never present locally |

## Why this is enforced in code and not by convention

It was convention, and the convention was one forgotten `export` away from a bad day.

`server/.env` on a developer machine points at a *deployed* database. Every binary in
`server/` loads that file through `dotenvy`, so an omitted `DATABASE_URL` is not an error —
it is silently filled in with a deployed one. Three things were reachable that way:

- `cargo test` — `tests/integration.rs` called `dotenvy::dotenv()` itself, so with no
  `DATABASE_URL` exported the suite ran against whatever `.env` pointed at. That suite
  creates users, deletes rows, and drives sweeps that claim every matching record in the
  database.
- `voip-rates` — its write opens with `DELETE FROM voip_rates WHERE provider = …`. It
  printed where it was about to write, which helps only somebody who reads it.
- the server itself — `init` runs migrations against whatever it is handed.

So the rule is now checked, in `db::guard_local_database` and `db::test_database_url`:

- **Tests refuse a remote database outright.** No override. There is no version of
  "run the suite against staging" that is a good idea.
- **Binaries and the server refuse one too**, unless `ALLOW_REMOTE_DB=1` is set — and then
  they say so in the log. Pointing at production from a laptop is occasionally the job; it
  should never be something you did without noticing.
- **Deployments are exempt**, detected by `RAILWAY_ENVIRONMENT`, which Railway injects into
  every container it runs. A remote database is the correct answer there.

"Local" means a loopback host — `localhost`, `127.0.0.1`, `::1`, or a unix socket. The host
is read after the last `@`, so a password containing `localhost` does not buy its way past
the check, and neither does a database *named* `localhost`.

## Setting the two local databases up

Both live in Docker, from the compose file at the repo root:

```sh
docker compose up -d --wait postgres
```

That is the whole setup. It creates `voxtranslate_dev` and `voxtranslate_voip_test`, and
installs `vector` in both — CI's image (`pgvector/pgvector:pg16`), because plain
`postgres:16` has no such extension and the tests that need it then **skip silently while
the run still prints ok**.

`--wait` is not decoration: the healthcheck asks `pg_isready` about `voxtranslate_dev`
specifically, so the command returns once the databases actually exist rather than when
the server first answers.

Two deliberate choices in `docker-compose.yml`:

- **Port 55432, not 5432.** A native Postgres may already hold 5432 on a developer
  machine — one does on the machine this was written on. Sharing the port would make
  "which database am I talking to" depend on which process started first, which is the
  ambiguity this whole thing exists to remove.
- **Bound to `127.0.0.1:`, not a bare port.** A bare `55432:5432` publishes on every
  interface, so on a shared network anyone on it can reach the database.

Keeping the data is the default; `docker compose down -v` throws it away and the next
`up` recreates both databases from scratch.

## Running things

```sh
# tests — the test database, passed explicitly. It is NOT read from .env.
DATABASE_URL=postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_voip_test cargo test

# the server — reads .env, which points at the dev database
cargo run

# the whole stack in containers, database included
docker compose up -d --wait

# a deliberate production operation, said out loud
ALLOW_REMOTE_DB=1 DATABASE_URL='<prod>' cargo run --bin voip-rates -- rates.csv
```

`server/.env` should hold:

```
DATABASE_URL=postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_dev
```

Inside the compose network the server reaches the database at `postgres:5432` instead, and
the compose file sets that itself — a single-label hostname counts as local precisely so
this works without an override.

### What it looks like when the guard stops you

A binary or a test refuses outright, naming the target with the password stripped. The
server is the exception: a database it cannot use makes it fall back to **guest-only
mode**, which is the long-standing behaviour for an unreachable database and is right in
production, where a transient failure should not crash-loop. So it starts — but it logs

```
ERROR billing/database init failed (refusing to start the server against a remote
database from a local process. …)
```

If billing, accounts or anything else DB-backed is mysteriously absent locally, that line
is the first place to look.

## Reading production without going near it by hand

The `railway` CLI is authenticated and returns variables in plaintext, so a read-only
question does not need anyone to paste a URL around:

```sh
railway variables --project <id> --environment production --service <name> --json
```

Note that `server/.env` and production are **different databases** — different regions,
different data. Diagnosing production from `.env` gives the right answer for the wrong
reason, or the wrong answer entirely.

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

```sh
createdb voxtranslate_voip_test   # for the suite
createdb voxtranslate_dev         # for the server you run yourself
```

The test database needs the `vector` extension, or the DB-gated tests that use embeddings
skip silently and the run still prints `ok`:

```sh
psql voxtranslate_voip_test -c 'CREATE EXTENSION IF NOT EXISTS vector;'
```

Point `server/.env` at the dev one:

```
DATABASE_URL=postgres://postgres@127.0.0.1:5432/voxtranslate_dev
```

Migrations run at boot, so the first `cargo run` creates the schema.

## Running things

```sh
# tests — pass the test database explicitly; it is not read from .env
DATABASE_URL=postgres://postgres@127.0.0.1:5432/voxtranslate_voip_test cargo test

# the server — reads .env, which points at the dev database
cargo run

# a deliberate production operation, said out loud
ALLOW_REMOTE_DB=1 DATABASE_URL='<prod>' cargo run --bin voip-rates -- rates.csv
```

## Reading production without going near it by hand

The `railway` CLI is authenticated and returns variables in plaintext, so a read-only
question does not need anyone to paste a URL around:

```sh
railway variables --project <id> --environment production --service <name> --json
```

Note that `server/.env` and production are **different databases** — different regions,
different data. Diagnosing production from `.env` gives the right answer for the wrong
reason, or the wrong answer entirely.

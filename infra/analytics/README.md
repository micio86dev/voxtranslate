# GA4 custom dimensions

GA4 **collects** every event parameter the apps send, but it can only **report** on the
ones registered as custom dimensions. Until `acquisition_source` is registered, the
`sign_up` events carry the campaign that produced each registration and no exploration,
audience or report can see it.

`custom-dimensions.json` is the version-controlled set; `setup-ga4-dimensions.mjs`
reconciles a GA4 property to it. Editing the file and re-running is the intended workflow —
the GA4 UI is the fallback, not the source of truth.

```bash
# preview — touches nothing
GA4_ACCESS_TOKEN=… GA4_PROPERTY_ID=123456789 \
  node infra/analytics/setup-ga4-dimensions.mjs --dry-run

# apply
GA4_ACCESS_TOKEN=… GA4_PROPERTY_ID=123456789 \
  node infra/analytics/setup-ga4-dimensions.mjs
```

The script creates what is missing, updates a label or description that has drifted, and
leaves everything else alone. Re-running is safe.

When a credential misbehaves, ask Google what it is before blaming the Analytics API:

```bash
GA4_ACCESS_TOKEN=… node infra/analytics/setup-ga4-dimensions.mjs --check-token
```

That hits the `tokeninfo` endpoint and prints validity, remaining lifetime, the
authorising account and the exact scope list — which separates an expired token from a
missing scope from a credential of the wrong type entirely. A 401 from the Admin API
cannot tell those apart; this can.

## Finding the property id

`GA4_PROPERTY_ID` is the **numeric** id in GA4 → Admin → Property details — not the
`G-XXXXXXXX` measurement id that `PUBLIC_GA_ID` holds. If you only have the measurement
id, the script can look it up (it walks accounts → properties → data streams):

```bash
GA4_ACCESS_TOKEN=… GA4_MEASUREMENT_ID=G-XXXXXXXX \
  node infra/analytics/setup-ga4-dimensions.mjs --dry-run
```

## Credentials

Either works; both need the `https://www.googleapis.com/auth/analytics.edit` scope.

### A. A bearer token (fastest, one-off)

1. Open [developers.google.com/oauthplayground](https://developers.google.com/oauthplayground/).
2. In *Step 1*, paste the scope `https://www.googleapis.com/auth/analytics.edit`.
3. Authorise with the Google account that administers the GA4 property, then *Exchange
   authorization code for tokens*.
4. Copy the access token into `GA4_ACCESS_TOKEN`. **It expires after one hour** — fine for
   a manual run, useless for CI.

### B. A service account (for CI)

1. In a Google Cloud project, create a service account and download a JSON key.
2. Enable the **Google Analytics Admin API** on that project.
3. In GA4 → Admin → Property access management, add the service account's
   `client_email` with the **Editor** role. Without this step the API answers 403 even
   though the token is valid.
4. Run with `GOOGLE_APPLICATION_CREDENTIALS=/path/to/key.json`; the script mints and
   exchanges the JWT itself, no dependencies.

Never commit a key or a token. Both belong in env vars, or in GitHub Actions secrets if
this is ever wired into a workflow.

## Things GA4 will not let you undo

- **`parameterName` and `scope` are immutable.** To change either you archive the
  dimension and create a new one — and archiving loses that dimension's reporting
  history. The script refuses to pretend otherwise: a scope mismatch is reported as a
  manual step.
- **No backfill.** A dimension applies to data collected after it exists. Register a
  parameter before you start caring about its numbers, not after.
- **A dimension registered in GA4 but no longer declared here is left alone**, only
  listed. Deleting reporting history should never be a side effect of running a script.

## Limits and what belongs elsewhere

GA4 enforces field limits that the API only reveals when it rejects a write, so the
script checks them locally and `--dry-run` fails on a violation:

| Field | Limit |
|---|---|
| `parameterName` | 40 chars |
| `displayName` | 82 chars |
| `description` | 150 chars |

That last one is tight. The full reasoning for each dimension therefore lives in a
`rationale` field in `custom-dimensions.json`, which stays in the repo and is never sent
to GA4 — keep `description` to what is useful inside the GA4 UI.

GA4 allows 50 event-scoped custom dimensions per property, so the declared set stays
deliberately small. Numeric parameters the apps send — `duration_seconds` on
`recording_stopped`, `count` on `invite_sent` — belong in custom **metrics**, which have
their own (10) limit and a separate Admin API collection; they are not handled here.

## Where the parameters come from

| Parameter | Sent by | Event |
|---|---|---|
| `acquisition_source` | `client/src/scripts/analytics.ts` (`trackAuthSuccess`) | `sign_up` |
| `method` | same, plus `app.ts` | `sign_up`, `login`, `room_joined` |
| `visibility` | `client/src/scripts/app.ts` | `start_call` |
| `is_returning_user` | `client/src/scripts/app.ts` | `room_joined` |
| `reason` | `client/src/scripts/app.ts` | `call_failed`, `limit_reached` |
| `button`, `location` | `website/src/pages/[lang]/index.astro` | `cta_clicked` |

Adding a tracked parameter in code means adding it here too, or it stays unreportable.

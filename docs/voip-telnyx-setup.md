# VoIP — Telnyx account setup and live verification

Everything in this document costs money or changes a production account, so **none of it
is done automatically**. It is the human half of shipping spec 0111.

The code is complete and testable without any of it: `VOIP_PROVIDER=mock` runs the entire
flow — dial, answer, media, consent, recording, billing, settlement — against the
in-process provider, with no telco and no charges. Staging can run that way indefinitely.

---

## 1. Account prerequisites

| Step | Why | Cost |
|---|---|---|
| Telnyx account, **EU billing entity** | The EU endpoint and Frankfurt anchoring are the whole residency story | free |
| Level 2 verification | Required before outbound international is enabled | free |
| Execute a **DPA** | Telnyx becomes a processor for call audio and phone numbers | free, needs legal |
| Add balance | Every dial spends it | **paid** |
| Create a **Voice App** (*Voice → Voice API/Programmable Voice → Create Voice App*) | Gives `TELNYX_CONNECTION_ID` | free |
| Create an **Outbound Voice Profile** | Where provider-side channel and spend limits live | free |
| Buy at least one number | Caller ID; unverified numbers may not be presented | **paid, recurring** |

## 2. Voice App settings

> **Naming.** Telnyx renamed *Call Control application* to **Voice App**. The API is
> unchanged — the id still goes in `TELNYX_CONNECTION_ID` and the adapter still posts to
> `/v2/calls/{id}/actions/*` — but the portal button says "Create Voice App", so looking
> for the old name wastes a quarter of an hour.

| Setting | Value | Why |
|---|---|---|
| Webhook URL | `https://<api-host>/api/voip/webhooks/telnyx` | |
| Webhook API version | v2 | The adapter parses the v2 envelope (`data.event_type`, `data.payload`) |
| Webhook failover URL | the **direct Railway origin**, same path | Telnyx retries here after two consecutive failures to the primary. The primary goes through Cloudflare, whose bot management challenges exactly this shape of traffic; the failover deliberately bypasses it. `/api/voip/webhooks/*` is exempt from the origin lock for this reason — see below. |
| **Anchorsite** | **Frankfurt, Germany** | This is what actually decides where media is handled — and where webhooks are sent *from*, so it is also what an IP allow-list has to match |
| Media encryption | SRTP | |
| DTMF type | RFC 2833 | The consent gate depends on DTMF arriving |

### Why the failover bypasses Cloudflare, and why that is safe

A carrier's webhook is a server-to-server POST from a fleet we do not control — the traffic
shape Cloudflare's bot management is built to challenge. It has already happened in this
project once, with MediaMTX, and the exemption in `origin_lock` still says so.

So the failover URL points at the direct Railway origin, and `/api/voip/webhooks/*` is
exempt from the origin lock. A failover that returns 403 is **worse than none**: Telnyx
records a failed delivery, the lifecycle event is lost, and the call rings, bills and never
settles — with nothing anywhere reporting an error.

Skipping the lock costs nothing here because the endpoint never trusted the network. It is
authenticated by an Ed25519 signature over `{timestamp}|{body}` with a 5-minute tolerance,
checked before the payload is read, and it fails closed when no public key is configured.
An unsigned request from anywhere gets exactly one outcome: 401.

Then copy the account's **public key** — Mission Control → *Account Settings* → **Keys &
Credentials** → **Public Key** tab — into `TELNYX_PUBLIC_KEY`. Without it every webhook is rejected — the adapter fails closed on
purpose, because a deployment that forgot the key would otherwise take call-control
instructions from anyone who can reach the endpoint.

## 3. Environment

```sh
# Feature
VOIP_ENABLED=true
VOIP_PROVIDER=telnyx                 # or `mock` — a legitimate staging value
VOIP_ROLLOUT_STAGE=internal          # disabled | internal | beta | business | ga
VOIP_MEDIA_WS_BASE=wss://api.voxtranslate.app   # where the carrier opens the media socket

# Residency
VOIP_REQUIRE_EU_PROCESSING=false     # see docs/voip-data-flow.md before changing
VOIP_DEFAULT_REGION="Frankfurt, Germany"

# Commercials
VOIP_MIN_GROSS_MARGIN_PERCENT=20
VOIP_COST_SAFETY_BUFFER_PERCENT=10
VOIP_MAX_DESTINATION_RATE=1.00
VOIP_DAILY_PROVIDER_SPEND_LIMIT=50
VOIP_MAX_CALL_DURATION_MINUTES=60
VOIP_MAX_CONCURRENT_CALLS_GLOBAL=50
VOIP_MAX_CONCURRENT_CALLS_PER_ORG=10
VOIP_MAX_CONCURRENT_CALLS_PER_USER=2
VOIP_RATE_MAX_AGE_SECS=86400

# Destinations
VOIP_ALLOW_INTERNATIONAL=true
VOIP_ALLOWED_COUNTRIES=              # empty = no allow-list
VOIP_BLOCKED_COUNTRIES=
VOIP_CHINA_ENABLED=false             # docs/voip-china-validation.md
VOIP_CHINA_REQUIRE_VALIDATED_ROUTE=true

# Features
VOIP_RECORDING_ENABLED=false
VOIP_TRANSCRIPTION_ENABLED=true
VOIP_VIDEO_ENABLED=false

# Privacy
VOIP_PSEUDONYM_KEY=<32+ random bytes>     # falls back to JWT_SECRET if unset
VOIP_MEDIA_TICKET_KEY=<32+ random bytes>  # signs media-socket tickets; see below
VOIP_VIDEO_INVITE_KEY=<32+ random bytes>  # signs video-upgrade invitations
VOIP_WEBHOOK_TOLERANCE_SECS=300

# Provider
TELNYX_API_KEY=<secret>
TELNYX_API_BASE=https://api.telnyx.eu    # the DEFAULT; the .com base leaves the EU
TELNYX_CONNECTION_ID=<the Voice App's "Application ID">
TELNYX_OUTBOUND_VOICE_PROFILE_ID=<ovp id>
TELNYX_PUBLIC_KEY=<base64 ed25519 public key>
TELNYX_DEFAULT_CALLER_ID=+39...
TELNYX_MEDIA_ANCHOR="Frankfurt, Germany"
```

### The three keys are deliberately not one

`VOIP_PSEUDONYM_KEY`, `VOIP_MEDIA_TICKET_KEY` and `VOIP_VIDEO_INVITE_KEY` do very different
jobs with very different exposure. A pseudonym is written into logs and rows by the
thousand and must stay stable for the life of the data. A media ticket is signed once and
lives sixty seconds, machine to machine. A video invitation is handed to a *person* and may
sit in a chat log for fifteen minutes. Reusing one key across them would make a weakness in
any one of them a weakness in all three.

None is required. When a specific key is unset it is **derived** from the shared fallback
secret through a domain separator rather than being the same bytes — so the separation
holds even in a deployment that configures none of them. Set dedicated ones in production
anyway: rotating either ticket key is harmless (in-flight tickets expire in minutes), while
rotating the pseudonym key changes every pseudonym you have already written.

### The `speak` command

The consent announcement is spoken by Telnyx, not synthesised by us: `PlayRequest::Speak`
maps to the Call Control `speak` command with a BCP-47 locale. This needs no second vendor
and no audio cache, and the announcement is a fixed sentence rather than a conversation.

The trade is Telnyx's language list, which is narrower than our 84. `telephony::telnyx::
speak_language` maps what it covers and falls back to `en-US` for the rest — and
`voip::consent::announcement` has already fallen back to English *text* in the same cases,
so the voice and the words stay in the same language. A call whose recipient language is
outside the list is disclosed in English and the row records that
(`disclosure_language = 'en'`).

If the Call Control application has speech synthesis disabled, set `speech_synthesis:
false` on the provider's capabilities. Capture will then not start at all rather than
starting without a disclosure.

### The video upgrade needs no carrier feature

`VOIP_VIDEO_ENABLED=true` is the only switch. The upgrade is an invitation into the
VoxTranslate room the call is already happening in — nothing is asked of Telnyx, and
nothing in that path can disturb the telephone call.

The link is `{api}/api/voip/video/{ticket}`, built from `VOIP_MEDIA_WS_BASE` (which already
names this API's host, so there is no second origin to keep in step). Redemption verifies
the signature and the 15-minute expiry, checks the call is still live, and only then
redirects to the room. **The room code never appears in the link the caller shares**, so a
forwarded invitation that has expired yields nothing.

There is deliberately no channel from here to the recipient's telephone. The caller is
already talking to them and passes the link on; an SMS integration would be a second
provider surface and a per-message charge, and nothing depends on one.

`TELNYX_API_BASE` and `TELNYX_MEDIA_ANCHOR` both default to the EU values, and
`TelnyxConfig::is_eu()` requires **both**. Changing either one silently moves telephony out
of the EU with no other visible symptom, which is why there is a test for it.

## 4. Provider-side limits to record before load testing

Application-side concurrency and Telnyx-side capacity are separate concerns, and the
second one is not ours to raise. Record, don't assume:

- Current account concurrent-call limit
- Outbound Voice Profile channel limit
- API rate limits
- Daily spend limit configured provider-side
- Verification level and what it gates

A load test that proves our control plane handles 1,000 calls proves nothing about whether
the account may place them.

## 5. Live smoke tests

Gated, never in CI, never automatic:

```sh
VOIP_LIVE_TESTS=true cargo test --test telnyx_live -- --ignored --nocapture
```

| Check | Spends money |
|---|---|
| Credentials accepted by the EU endpoint | no |
| Call Control application resolves | no |
| Rate deck is (still) not available over the API — see §6 | no |
| Webhook signature round-trip against the real public key | no |
| **One controlled call to a number you own** | **yes** |
| Recording start/stop and retrieval | **yes** |
| Cost appears in the provider's usage report | no |

Set `TELNYX_LIVE_TEST_TO` to a number **you own**. Never a customer's, never a real
person's who has not agreed, never one in a fixture.

## 6. The rate deck is imported, not fetched

**Verified against a live EU account**, not assumed:

| Endpoint | Result |
|---|---|
| `api.telnyx.eu/v2/public/pricing?primitive=voice` | **404** |
| `api.telnyx.com/v2/public/pricing?primitive=voice` | **404** |
| `api.telnyx.com/v2/pricing/products` | 200, but a product catalogue — no prefixes, no per-minute prices |

There is no documented public REST endpoint for per-destination voice rates. What Telnyx
offers is a rate deck you **download** from the Outbound Voice Profile. So `fetch_rate_deck`
reports `Unsupported` — the same treatment as `fetch_cdr` — rather than staying pointed at
an endpoint that does not exist, which would refuse every call with `rate_unavailable` for
a reason no log explains.

Import the downloaded file:

```sh
# Look before you write: parses, reports, touches nothing.
cargo run --bin voip-rates -- --dry-run rates.csv

DATABASE_URL=… cargo run --bin voip-rates -- rates.csv
```

**Pass `DATABASE_URL` explicitly, and read the `target:` line before you let it run.** The
importer loads `server/.env` like every other binary here, so an omitted `DATABASE_URL` is
filled in silently — and on a developer machine that file points at a *deployed* database.
The write opens with `DELETE FROM voip_rates WHERE provider = …`, so the wrong URL does not
fail, it replaces the live deck. The importer therefore prints where it is about to write,
host and database only:

```
target: aws-1-eu-central-1.pooler.supabase.com:5432/postgres
```

It reads comma, semicolon and tab exports, strips `+` and currency symbols, tolerates
blank and totals rows, and **refuses to guess**: an unrecognised column stops the import
and prints the headers it actually saw. A deck read with the wrong mapping does not fail —
it prices every call wrongly, and the first anyone hears of it is the invoice. If your
export uses a header the importer does not know, add it to the `*_KEYS` lists in
`src/bin/voip-rates.rs`.

**Put it on a schedule.** `voip_rates.fetched_at` is what `VOIP_RATE_MAX_AGE_SECS` measures
(24 h by default), and a deck past that age refuses calls rather than pricing them from
stale numbers. A rate deck imported once is a rate deck that expires. Do **not** widen the
staleness window to make the symptom go away — that trades a loud failure for a silent
mispricing.

The import replaces the whole deck for the provider in one transaction: a prefix Telnyx
*removed* must disappear, and an upsert would keep pricing it from the last deck that
mentioned it for ever.

## 7. Known gap: per-leg cost reconciliation

Telnyx rates calls asynchronously and exposes the result through batched usage reports,
not on the hangup webhook. The adapter therefore reports `fetch_cdr` as **unsupported**
rather than returning a fabricated zero, and affected calls show as **unreconciled**:
`voip_calls.actual_provider_cost_usd IS NULL`.

That is a visible gap by design — a made-up zero would show every call at 100% margin,
which is worse than admitting the number is not in yet. Closing it requires a live account
to confirm the usage-report endpoint's shape, and is the main credential-dependent item
outstanding.

## 8. Rollback

1. **Immediate, and does not touch live calls:** set `enabled = false` on the
   organisation's `voip_org_settings` row, or disable the **Outbound Voice Profile** in
   the Telnyx portal. Both take effect on the next dial with no restart.
2. `VOIP_ROLLOUT_STAGE=disabled` — refuses every new dial deployment-wide, **after a
   restart**. `VoipConfig` is read once at boot, and the restart that applies the change
   drops every media socket in flight. So this stops new calls only in the sense that it
   cuts off the current ones first.
3. `VOIP_ENABLED=false` — the routes are not registered at all. Same restart caveat.
4. None of them touches data. Reservations already open settle normally on their hangup
   webhooks; if the process is gone, the recovery sweep closes them and refunds.

Nothing here needs a migration to be reverted. `056_voip.sql` only adds tables and one
partial index; leaving it applied with the feature off is inert.

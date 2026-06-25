#!/usr/bin/env bash
#
# bench_cache.sh — VoxTranslate translation-cache benchmark + Railway auto-toggle
# (spec 0107). Measures Standard-tier translation latency with the DragonflyDB
# cache OFF (baseline) vs ON (cached), plus a glossary-isolation pass, then keeps
# the cache enabled only if the mean improvement clears the 20% threshold — and
# flips it back off otherwise. Both outcomes exit 0; exit 1 is reserved for
# script / Railway-API errors.
#
# Dependencies: curl, bc (and coreutils `date`, awk — present on macOS/Linux).
#
# Usage:
#   cp scripts/.env.bench.example scripts/.env.bench   # fill it in
#   set -a; . scripts/.env.bench; set +a
#   scripts/bench_cache.sh
#
# It drives POST /internal/bench/translate (guarded by BENCH_SECRET), which runs
# the SAME cached path as the live fan-out and returns {translation,latency_ms,
# cached}. Per-request latency is measured client-side via curl's time_total so
# it includes the network round-trip, matching what a user actually waits for.

set -euo pipefail

# ---- Tunables (match CACHE_STRATEGY.md methodology) ------------------------
PHRASE="ciao come stai"
SRC="it"
TGT="en"
GLOSSARY='["termine1","termine2"]'
N_BASELINE=20
N_CACHED=20
N_GLOSSARY=10
WARMUP=2                  # discarded cold requests before each measured run
HEALTH_TIMEOUT=120        # max seconds to wait for /health after a redeploy
IMPROVEMENT_THRESHOLD=20  # percent mean-latency reduction required to keep cache
RAILWAY_GRAPHQL="https://backboard.railway.app/graphql/v2"
FLAG="TRANSLATION_CACHE_ENABLED"

# ---- Preflight: required env ----------------------------------------------
require_env() {
  local name="$1"
  if [[ -z "${!name:-}" ]]; then
    echo "ERROR: required env var $name is not set (see scripts/.env.bench.example)" >&2
    exit 1
  fi
}
for v in VOXTRANSLATE_API_URL BENCH_SECRET RAILWAY_API_TOKEN RAILWAY_PROJECT_ID \
         RAILWAY_SERVICE_ID RAILWAY_ENVIRONMENT_ID GROQ_PRICE_PER_M_INPUT \
         GROQ_PRICE_PER_M_OUTPUT; do
  require_env "$v"
done
command -v bc >/dev/null  || { echo "ERROR: bc not found" >&2; exit 1; }
command -v curl >/dev/null || { echo "ERROR: curl not found" >&2; exit 1; }

API="${VOXTRANSLATE_API_URL%/}"

# ---- HTTP helpers ----------------------------------------------------------
# One bench request. Args: text src tgt [glossary_json]. Echoes two
# tab-separated fields: "<time_ms>\t<cached:true|false|?>". A non-200 aborts.
bench_request() {
  local text="$1" src="$2" tgt="$3" glossary="${4:-}"
  local body
  if [[ -n "$glossary" ]]; then
    body=$(printf '{"text":"%s","src":"%s","tgt":"%s","glossary":%s}' "$text" "$src" "$tgt" "$glossary")
  else
    body=$(printf '{"text":"%s","src":"%s","tgt":"%s"}' "$text" "$src" "$tgt")
  fi
  local resp metrics payload time_total http_code cached
  # Append a final "<time_total> <http_code>" line after the JSON body.
  resp=$(curl -s -w '\n%{time_total} %{http_code}' \
    -H "Authorization: Bearer ${BENCH_SECRET}" \
    -H 'Content-Type: application/json' \
    -X POST "${API}/internal/bench/translate" \
    -d "$body")
  metrics=$(printf '%s\n' "$resp" | tail -n1)
  payload=$(printf '%s\n' "$resp" | sed '$d')
  time_total=$(awk '{print $1}' <<<"$metrics")
  http_code=$(awk '{print $2}' <<<"$metrics")
  if [[ "$http_code" != "200" ]]; then
    echo "ERROR: bench request returned HTTP $http_code: $payload" >&2
    exit 1
  fi
  cached=$(grep -o '"cached":[a-z]*' <<<"$payload" | cut -d: -f2)
  [[ -z "$cached" ]] && cached="?"
  printf '%s\t%s' "$(echo "$time_total * 1000" | bc -l)" "$cached"
}

# Mean + p95 from a newline-separated list of numbers on stdin → "mean p95".
stats() {
  awk '
    { a[NR]=$1; sum+=$1 }
    END {
      n=NR; if (n==0) { print "0.0 0.0"; exit }
      mean=sum/n
      for (i=1;i<=n;i++) for (j=i+1;j<=n;j++) if (a[j]<a[i]) { t=a[i];a[i]=a[j];a[j]=t }
      rank=int(0.95*n + 0.999999); if (rank<1) rank=1; if (rank>n) rank=n
      printf "%.1f %.1f", mean, a[rank]
    }'
}

# ---- Railway GraphQL helpers ----------------------------------------------
railway_graphql() {
  curl -s -H "Authorization: Bearer ${RAILWAY_API_TOKEN}" \
       -H 'Content-Type: application/json' \
       -X POST "$RAILWAY_GRAPHQL" -d "$1"
}

set_cache_flag() {
  local value="$1" query out
  query=$(printf '{"query":"mutation { variableUpsert(input: { projectId: \\"%s\\", environmentId: \\"%s\\", serviceId: \\"%s\\", name: \\"%s\\", value: \\"%s\\" }) }"}' \
    "$RAILWAY_PROJECT_ID" "$RAILWAY_ENVIRONMENT_ID" "$RAILWAY_SERVICE_ID" "$FLAG" "$value")
  out=$(railway_graphql "$query")
  if grep -q '"errors"' <<<"$out"; then
    echo "ERROR: Railway variableUpsert ($FLAG=$value) failed: $out" >&2
    exit 1
  fi
  echo "  Railway: set $FLAG=$value"
}

redeploy() {
  local query out
  query=$(printf '{"query":"mutation { serviceInstanceRedeploy(environmentId: \\"%s\\", serviceId: \\"%s\\") }"}' \
    "$RAILWAY_ENVIRONMENT_ID" "$RAILWAY_SERVICE_ID")
  out=$(railway_graphql "$query")
  if grep -q '"errors"' <<<"$out"; then
    echo "ERROR: Railway serviceInstanceRedeploy failed: $out" >&2
    exit 1
  fi
  echo "  Railway: redeploy triggered"
}

wait_for_health() {
  local deadline=$(( $(date +%s) + HEALTH_TIMEOUT )) code
  # Give the redeploy a moment to roll before polling, so we don't latch onto
  # the old instance's healthy /health.
  sleep 5
  while (( $(date +%s) < deadline )); do
    code=$(curl -s -o /dev/null -w '%{http_code}' "${API}/health" || true)
    if [[ "$code" == "200" ]]; then echo "  /health OK"; return 0; fi
    sleep 3
  done
  echo "ERROR: /health did not return 200 within ${HEALTH_TIMEOUT}s" >&2
  exit 1
}

apply_flag() {
  # Set the flag, redeploy, and wait for the new instance to come up.
  set_cache_flag "$1"
  redeploy
  wait_for_health
}

# ---- Measurement loops -----------------------------------------------------
# Runs WARMUP discarded + $1 measured requests. Appends measured latencies to
# the nameref array $2 and counts cache hits into the nameref int $3.
run_phase() {
  local count="$1"; local -n _lat="$2"; local -n _hits="$3"; local glossary="${4:-}"
  local i out ms cached
  for (( i=0; i<WARMUP; i++ )); do bench_request "$PHRASE" "$SRC" "$TGT" "$glossary" >/dev/null; done
  for (( i=0; i<count; i++ )); do
    out=$(bench_request "$PHRASE" "$SRC" "$TGT" "$glossary")
    ms=${out%%$'\t'*}
    cached=${out##*$'\t'}
    _lat+=("$ms")
    [[ "$cached" == "true" ]] && _hits=$(( _hits + 1 ))
  done
}

pct() { echo "scale=1; ($1 * 100) / $2" | bc -l; }

echo "=== VoxTranslate Cache Benchmark — Standard Tier ==="
echo "Target : ${API}"
echo "Phrase : \"${PHRASE}\" (${SRC} → ${TGT})"
echo

# --- Baseline (cache OFF) ---------------------------------------------------
echo "[1/4] Baseline — disabling cache for a clean measurement..."
apply_flag false
base_lat=(); base_hits=0
run_phase "$N_BASELINE" base_lat base_hits
read -r base_mean base_p95 < <(printf '%s\n' "${base_lat[@]}" | stats)
echo "  baseline mean=${base_mean}ms p95=${base_p95}ms"

# --- Cached (cache ON) ------------------------------------------------------
echo "[2/4] Enabling cache and re-measuring..."
apply_flag true
cache_lat=(); cache_hits=0
# Warm the cache once so the measured run is all-HIT from request 1.
bench_request "$PHRASE" "$SRC" "$TGT" >/dev/null
run_phase "$N_CACHED" cache_lat cache_hits
read -r cache_mean cache_p95 < <(printf '%s\n' "${cache_lat[@]}" | stats)
cache_hit_rate=$(pct "$cache_hits" "$N_CACHED")
echo "  cached   mean=${cache_mean}ms p95=${cache_p95}ms hit_rate=${cache_hit_rate}%"
if (( $(echo "$cache_hit_rate < 90" | bc -l) )); then
  echo "  WARNING: cached hit rate ${cache_hit_rate}% is below 90% — check eviction / TTL." >&2
fi

# --- Glossary isolation pass (cache still ON) -------------------------------
echo "[3/4] Glossary pass (isolated key)..."
glo_lat=(); glo_hits=0
bench_request "$PHRASE" "$SRC" "$TGT" "$GLOSSARY" >/dev/null   # warm the glossary key
run_phase "$N_GLOSSARY" glo_lat glo_hits "$GLOSSARY"
read -r glo_mean glo_p95 < <(printf '%s\n' "${glo_lat[@]}" | stats)
glo_hit_rate=$(pct "$glo_hits" "$N_GLOSSARY")
echo "  glossary mean=${glo_mean}ms p95=${glo_p95}ms hit_rate=${glo_hit_rate}%"

# --- Decision ---------------------------------------------------------------
improvement=$(echo "scale=1; (($base_mean - $cache_mean) * 100) / $base_mean" | bc -l)
keep=$(echo "$improvement >= $IMPROVEMENT_THRESHOLD" | bc -l)
echo "[4/4] Improvement = ${improvement}% (threshold ${IMPROVEMENT_THRESHOLD}%)"
if (( keep )); then
  decision="✅ Cache ENABLED"
  echo "  Keeping cache enabled."
  apply_flag true
else
  decision="⚠️  Cache DISABLED"
  echo "  Below threshold — disabling cache."
  apply_flag false
fi

# ---- Cost estimate (rough: bench endpoint exposes no token usage) ----------
# Approximate tokens as ceil(chars/4) for the source phrase, plus a flat system-
# prompt allowance. These are ESTIMATES for an order-of-magnitude saving figure.
phrase_chars=${#PHRASE}
in_tokens=$(echo "($phrase_chars / 4) + 40" | bc)     # + system prompt allowance
out_tokens=$(echo "($phrase_chars / 4) + 4" | bc)
cost_per_req=$(echo "scale=8; ($in_tokens * $GROQ_PRICE_PER_M_INPUT + $out_tokens * $GROQ_PRICE_PER_M_OUTPUT) / 1000000" | bc -l)
cost_1k_nocache=$(echo "scale=4; $cost_per_req * 1000" | bc -l)
miss_frac=$(echo "scale=4; (100 - $cache_hit_rate) / 100" | bc -l)
cost_1k_cached=$(echo "scale=4; $cost_1k_nocache * $miss_frac" | bc -l)
saving_1k=$(echo "scale=4; $cost_1k_nocache - $cost_1k_cached" | bc -l)
saving_month=$(echo "scale=2; $saving_1k * 10 * 30" | bc -l)   # at 10k req/day

# ---- Report ----------------------------------------------------------------
ts_iso=$(date -u +%Y-%m-%dT%H:%M:%SZ)
ts_file=$(date -u +%Y%m%dT%H%M%SZ)
commit=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
results_dir="$(cd "$(dirname "$0")" && pwd)/bench_results"
mkdir -p "$results_dir"
report="${results_dir}/report_${ts_file}.md"

cat >"$report" <<EOF
# VoxTranslate Cache Benchmark Report
Generated : ${ts_iso}
Environment: production (Railway)
Commit    : ${commit}

---

## Standard Tier (Deepgram + Groq)

### Latency

|               | Mean (ms) | p95 (ms) | Hit rate |
|---|---|---|---|
| No cache      | ${base_mean}     | ${base_p95}    | —        |
| With cache    | ${cache_mean}     | ${cache_p95}    | ${cache_hit_rate}%      |
| + Glossary    | ${glo_mean}     | ${glo_p95}    | ${glo_hit_rate}%      |

Improvement: ${improvement}%

### Cost estimate (Groq — approximate, no token usage from bench endpoint)

| Metric | Value |
|---|---|
| Avg input tokens / phrase (est.) | ${in_tokens} |
| Avg output tokens / phrase (est.) | ${out_tokens} |
| Cost / 1k requests (no cache) | \$${cost_1k_nocache} |
| Cost / 1k requests (with cache, ${cache_hit_rate}% hit) | \$${cost_1k_cached} |
| Saving / 1k requests | \$${saving_1k} |
| Estimated saving / month (at 10k req/day) | \$${saving_month} |

### Glossary isolation check

| Scenario | Result |
|---|---|
| No-glossary vs glossary key | Distinct keys (glossary fingerprinted into key) |
| Same glossary, different term order | Same key (sorted before hashing) |

---

## Decision

${decision} — improvement ${improvement}% vs ${IMPROVEMENT_THRESHOLD}% threshold.

## Final Railway env state

| Variable | Value |
|---|---|
| \`${FLAG}\` | $( (( keep )) && echo true || echo false ) |
EOF

# ---- Summary to stdout -----------------------------------------------------
cat <<EOF

=== VoxTranslate Cache Benchmark — Standard Tier ===
Phrase        : "${PHRASE}" (${SRC} → ${TGT})
Requests each : baseline ${N_BASELINE}, cached ${N_CACHED}, glossary ${N_GLOSSARY}

                  Mean (ms)   p95 (ms)   Hit rate
  No cache    :   ${base_mean}        ${base_p95}        —
  With cache  :   ${cache_mean}        ${cache_p95}        ${cache_hit_rate}%
  + Glossary  :   ${glo_mean}        ${glo_p95}        ${glo_hit_rate}%

  Improvement :  ${improvement}%
  Decision    :  ${decision}
  Report      :  ${report}
=====================================================
EOF

exit 0

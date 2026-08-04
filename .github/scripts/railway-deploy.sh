#!/usr/bin/env bash
#
# Deploy the current directory to Railway and report the DEPLOYMENT's outcome.
#
# WHY THIS EXISTS: `railway up --ci` streams the build logs and exits 1 when that stream
# drops — which it does routinely, with "Failed to stream build logs: Failed to retrieve
# build log", after roughly 70 seconds. The build carries on server-side and reaches
# SUCCESS, so the job goes red while production ships fine. On 2026-08-04 both the
# staging and the production deploy jobs failed that way in the same release. A CI that
# is red when everything works is a CI nobody reads.
#
# So: start the deploy detached, then judge it by the deployment's own status.
#
# FAIL CLOSED. Every path that cannot prove SUCCESS exits non-zero — no new deployment,
# an unreadable status, a terminal failure, or the timeout. A deploy script that goes
# green when it lost track of the deploy is worse than the flake it replaces.
#
# Usage: railway-deploy.sh <service-name>
# Needs: RAILWAY_TOKEN in the environment, and jq (present on GitHub runners).

set -euo pipefail

SERVICE="${1:?usage: railway-deploy.sh <service-name>}"

# How long to wait for the deployment to reach a terminal state. A cold Rust build with
# no Docker layer cache took ~6 minutes on 2026-08-04; 25 minutes leaves real headroom
# without letting a wedged deploy hold a runner all day.
POLL_INTERVAL_SECS=15
MAX_POLLS=100

latest_id() {
  # `|| true` so one transient API blip does not kill the run under `set -e`; the
  # surrounding loops still fail closed when they time out.
  railway deployment list --service "$SERVICE" --limit 1 --json 2>/dev/null \
    | jq -r '.[0].id // ""' || true
}

status_of() {
  railway deployment list --service "$SERVICE" --limit 5 --json 2>/dev/null \
    | jq -r --arg id "$1" '.[] | select(.id == $id) | .status' || true
}

previous_id="$(latest_id)"
echo "Previous deployment: ${previous_id:-<none>}"

railway up --detach --service "$SERVICE"

# The new deployment does not appear instantly; wait for an id that is not the old one.
new_id=""
for _ in $(seq 1 30); do
  candidate="$(latest_id)"
  if [ -n "$candidate" ] && [ "$candidate" != "$previous_id" ]; then
    new_id="$candidate"
    break
  fi
  sleep 5
done

if [ -z "$new_id" ]; then
  echo "::error::railway up returned but no new deployment appeared for $SERVICE"
  exit 1
fi

echo "Deployment $new_id started; waiting for it to settle."

for _ in $(seq 1 "$MAX_POLLS"); do
  status="$(status_of "$new_id")"
  echo "  status: ${status:-<unreadable>}"
  case "$status" in
    SUCCESS)
      echo "Deployment $new_id succeeded."
      exit 0
      ;;
    FAILED | CRASHED | REMOVED)
      echo "::error::Deployment $new_id ended in status $status"
      echo "Build logs: railway logs --deployment $new_id"
      exit 1
      ;;
  esac
  sleep "$POLL_INTERVAL_SECS"
done

echo "::error::Timed out after $((POLL_INTERVAL_SECS * MAX_POLLS))s waiting for deployment $new_id"
exit 1

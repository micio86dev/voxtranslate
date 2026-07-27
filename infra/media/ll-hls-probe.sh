#!/usr/bin/env bash
#
# ll-hls-probe.sh — answer ONE question about a live webinar: is the media server
# actually emitting Low-Latency HLS, or has the stream silently degraded to plain HLS?
#
#   ./ll-hls-probe.sh <webinar-code>
#
# Why this exists: `hlsVariant: lowLatency` in mediamtx.yml is a REQUEST, not a
# guarantee. Two things break it in the field and neither logs an error —
#   1. no EXT-X-PART in the media playlist (LL-HLS never engaged at all), and
#   2. EXT-X-TARGETDURATION far above hlsSegmentDuration, because MediaMTX can only
#      close a segment on an IDR frame: the real segment length is the WHIP
#      publisher's keyframe interval, not the configured 1s.
# Either one puts a hard floor under end-to-end latency that no player tuning can
# lift. Run this BEFORE blaming (or tuning) the client.
#
# Env overrides: MEDIA_HLS_HOST (default ingest.voxtranslate.app),
#                GUEST_ORIGIN   (default https://voxtranslate.app)
set -euo pipefail

HOST="${MEDIA_HLS_HOST:-ingest.voxtranslate.app}"
ORIGIN="${GUEST_ORIGIN:-https://voxtranslate.app}"
CODE="${1:-}"

if [[ -z "$CODE" ]]; then
  echo "usage: $0 <webinar-code>   (the code from https://voxtranslate.app/w/<code>)" >&2
  exit 2
fi

MASTER="https://${HOST}/webinar/${CODE}/index.m3u8"
# MediaMTX gates HLS behind a session cookie handed out on the FIRST request, so every
# follow-up fetch must carry it — exactly like the browser player does.
JAR="$(mktemp)"
trap 'rm -f "$JAR"' EXIT

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }

say "1. CORS + session cookie  ($MASTER)"
# -L is REQUIRED: MediaMTX gates HLS with a cookie-check bounce — the first request 302s
# to `?cookieCheck=1` with a Set-Cookie, and only the redirected request returns the
# playlist. Without following it (carrying the jar) every probe reports a bare 302.
headers="$(curl -sS -L -D - -o /dev/null -c "$JAR" -b "$JAR" -H "Origin: ${ORIGIN}" "$MASTER" || true)"
# Read the LAST status line: with -L the header block covers every hop.
status="$(printf '%s' "$headers" | rg '^HTTP/' | tail -1 | awk '{print $2}')"
if [[ "$status" != "200" ]]; then
  bad "master playlist returned HTTP ${status:-<no response>} — is the webinar live?"
  exit 1
fi
ok "master playlist reachable (HTTP 200)"

acao="$(printf '%s' "$headers" | rg -i '^access-control-allow-origin:' | tail -1 | tr -d '\r' || true)"
acac="$(printf '%s' "$headers" | rg -i '^access-control-allow-credentials:' | tail -1 | tr -d '\r' || true)"
# The player fetches playlists with credentials (the session cookie). A wildcard ACAO is
# INVALID together with credentials, so the browser rejects every blocking reload and
# hls.js quietly abandons LL-HLS. That failure is invisible except right here.
if [[ "$acao" == *"*"* ]]; then
  bad "Access-Control-Allow-Origin is '*' — credentialed reloads will be rejected, LL-HLS dies"
  info "fix: pin hlsAllowOrigin to ${ORIGIN} in mediamtx.yml"
elif [[ -n "$acao" ]]; then
  ok "${acao}"
else
  bad "no Access-Control-Allow-Origin header — cross-origin playback will fail"
fi
[[ -n "$acac" ]] && ok "$acac" || info "(no allow-credentials header)"

say "2. Media playlist"
master_body="$(curl -sS -L -b "$JAR" -c "$JAR" -H "Origin: ${ORIGIN}" "$MASTER")"
# First non-comment line pointing at a playlist = the variant stream.
variant="$(printf '%s' "$master_body" | rg -v '^#' | rg '\.m3u8' | head -1 | tr -d '\r' || true)"
if [[ -z "$variant" ]]; then
  bad "master lists no variant playlist — nothing is being remuxed yet"
  printf '%s\n' "$master_body" | head -20
  exit 1
fi
MEDIA_URL="$(python3 - "$MASTER" "$variant" <<'PY'
import sys, urllib.parse
print(urllib.parse.urljoin(sys.argv[1], sys.argv[2]))
PY
)"
info "variant: $MEDIA_URL"
media="$(curl -sS -L -b "$JAR" -c "$JAR" -H "Origin: ${ORIGIN}" "$MEDIA_URL")"

say "3. Low-latency markers"
parts="$(printf '%s' "$media" | rg -c '^#EXT-X-PART:' || true)"
server_control="$(printf '%s' "$media" | rg '^#EXT-X-SERVER-CONTROL:' | tr -d '\r' || true)"
part_inf="$(printf '%s' "$media" | rg '^#EXT-X-PART-INF:' | tr -d '\r' || true)"

if [[ "${parts:-0}" -gt 0 ]]; then
  ok "EXT-X-PART present (${parts} parts) — the server IS emitting LL-HLS"
else
  bad "NO EXT-X-PART — this is plain HLS on the wire; the player cannot go low-latency"
fi
[[ -n "$part_inf" ]] && ok "$part_inf" || bad "no EXT-X-PART-INF (part target duration missing)"
if [[ "$server_control" == *"CAN-BLOCK-RELOAD=YES"* ]]; then
  ok "$server_control"
else
  bad "blocking playlist reload NOT advertised: ${server_control:-<absent>}"
fi

say "4. Segment length vs. configured hlsSegmentDuration"
# EXTINF is the ONLY honest measure of the publisher's keyframe interval: MediaMTX
# cannot cut a segment anywhere else.
printf '%s' "$media" | rg -o '^#EXTINF:([0-9.]+)' -r '$1' | tail -8 |
  awk '
    { d[NR]=$1; sum+=$1; if ($1>max) max=$1 }
    END {
      if (NR==0) { print "    no EXTINF yet — no complete segment published"; exit }
      printf "    last %d segments: ", NR
      for (i=1;i<=NR;i++) printf "%.2fs ", d[i]
      printf "\n    mean %.2fs   max %.2fs\n", sum/NR, max
      if (sum/NR > 1.6)
        printf "  \033[31m✗\033[0m segments are ~%.1fx the configured 1s: the WHIP publisher'\''s\n      keyframe interval is the real floor, not hlsSegmentDuration\n", sum/NR
      else
        printf "  \033[32m✓\033[0m segments track the configured 1s — keyframe interval is fine\n"
    }'

target="$(printf '%s' "$media" | rg -o '^#EXT-X-TARGETDURATION:([0-9]+)' -r '$1' | head -1 || true)"
if [[ -n "$target" ]]; then
  info "EXT-X-TARGETDURATION: ${target}s  → plain-HLS fallback would hold back ~$((target * 3))s"
fi

say "Verdict"
if [[ "${parts:-0}" -gt 0 && "$server_control" == *"CAN-BLOCK-RELOAD=YES"* ]]; then
  echo "  Server side is CLEAN. Remaining latency is in the player or the network —"
  echo "  measure it in the browser with guest-latency.js."
else
  echo "  Server side is the bottleneck. Fix the playlist before touching the player:"
  echo "  no amount of hls.js tuning can undo a missing EXT-X-PART."
fi

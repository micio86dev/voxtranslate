#!/usr/bin/env bash
# site-audit.sh — deterministic, reproducible evidence extractor for the marketing agent team.
#
# Usage:  .claude/tools/site-audit.sh <url> [outdir]
#
# Emits a plain-text report to stdout and saves the raw HTML to <outdir>/page.html
# so any claim can be re-verified. Extracts ONLY what is literally in the response:
# never infers, never estimates. Requires: curl, rg.

set -uo pipefail

URL="${1:-}"
OUTDIR="${2:-${TMPDIR:-/tmp}/site-audit}"

if [[ -z "$URL" ]]; then
  echo "usage: site-audit.sh <url> [outdir]" >&2
  exit 2
fi

command -v rg >/dev/null 2>&1 || { echo "rg (ripgrep) is required: brew install ripgrep" >&2; exit 3; }

mkdir -p "$OUTDIR"
SLUG="$(printf '%s' "$URL" | tr -c 'a-zA-Z0-9' '-' | cut -c1-80)"
HTML="$OUTDIR/${SLUG}.html"
HDRS="$OUTDIR/${SLUG}.headers"

UA='Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36'

section() { printf '\n===== %s =====\n' "$1"; }

# ---------------------------------------------------------------- transport
section "HTTP / TRANSPORT"
curl -sS -L -A "$UA" -o "$HTML" -D "$HDRS" \
  -w 'final_url=%{url_effective}\nhttp_code=%{http_code}\nredirects=%{num_redirects}\ntime_namelookup=%{time_namelookup}\ntime_connect=%{time_connect}\ntime_starttransfer=%{time_starttransfer}\ntime_total=%{time_total}\nsize_download_bytes=%{size_download}\n' \
  "$URL" 2>&1 || echo "FETCH_FAILED"

echo "--- response headers ---"
rg -iN '^(server|content-type|cache-control|content-encoding|strict-transport-security|x-powered-by|x-vercel|cf-|age|vary|content-security-policy):' "$HDRS" 2>/dev/null | head -30

echo "--- raw html saved to: $HTML ---"

# ---------------------------------------------------------------- head / meta
section "TITLE / META"
rg -oN '(?s)<title[^>]*>.*?</title>' "$HTML" | head -3
rg -oiN '<meta[^>]+name="description"[^>]*>' "$HTML" | head -3
rg -oiN '<meta[^>]+name="robots"[^>]*>' "$HTML" | head -3
rg -oiN '<link[^>]+rel="canonical"[^>]*>' "$HTML" | head -3
rg -oiN '<meta[^>]+property="og:[^"]+"[^>]*>' "$HTML" | head -12
rg -oiN '<meta[^>]+name="twitter:[^"]+"[^>]*>' "$HTML" | head -8

section "HREFLANG"
rg -oiN '<link[^>]+hreflang="[^"]*"[^>]*>' "$HTML" | head -40
printf 'hreflang_count=%s\n' "$(rg -oic 'hreflang="' "$HTML" 2>/dev/null || echo 0)"

# ---------------------------------------------------------------- content
section "HEADINGS (h1/h2/h3)"
rg -oN '(?s)<h1[^>]*>.*?</h1>' "$HTML" | head -10
rg -oN '(?s)<h2[^>]*>.*?</h2>' "$HTML" | head -25
rg -oN '(?s)<h3[^>]*>.*?</h3>' "$HTML" | head -25

section "STRUCTURED DATA (JSON-LD @type)"
rg -oN '"@type"\s*:\s*"[^"]+"' "$HTML" | sort | uniq -c | sort -rn | head -20

section "IMAGES / ALT"
printf 'img_tags=%s\n' "$(rg -oic '<img' "$HTML" 2>/dev/null || echo 0)"
printf 'img_with_alt=%s\n' "$(rg -oic 'alt="' "$HTML" 2>/dev/null || echo 0)"
printf 'lazy_loaded=%s\n' "$(rg -oic 'loading="lazy"' "$HTML" 2>/dev/null || echo 0)"

# ---------------------------------------------------------------- conversion
section "FORMS & INPUTS"
rg -oiN '<form[^>]*>' "$HTML" | head -10
rg -oiN '<input[^>]+type="[^"]+"[^>]*>' "$HTML" | head -20
printf 'form_count=%s\n' "$(rg -oic '<form' "$HTML" 2>/dev/null || echo 0)"

section "CTA-LIKE LINKS & BUTTONS (href + label)"
rg -oN '(?s)<a[^>]+href="[^"]*"[^>]*>.*?</a>' "$HTML" \
  | rg -iN '(sign|start|try|free|demo|book|buy|price|pricing|get|contact|download|trial|register|login|prova|inizia|prezzi|contatt)' \
  | head -30

section "OUTBOUND / SOCIAL LINKS"
rg -oiN 'https?://(www\.)?(instagram|linkedin|youtube|x|twitter|facebook|tiktok|github|producthunt|reddit|discord|t\.me|medium)\.com[^"'"'"' ]*' "$HTML" \
  | sort -u | head -25

section "CONTACT SURFACES"
rg -oiN 'mailto:[^"'"'"' ]+' "$HTML" | sort -u | head -10
rg -oiN 'tel:[^"'"'"' ]+' "$HTML" | sort -u | head -10
rg -oiN '(intercom|crisp|tawk|hubspot|drift|zendesk|calendly|cal\.com|tidio)' "$HTML" | sort -u | head -10

section "PRICING SIGNALS ON THIS PAGE"
rg -oiN '(€|\$|£)\s?[0-9]+([.,][0-9]{1,2})?(\s?/\s?(mo|month|mese|yr|year|anno|user|utente))?' "$HTML" | sort | uniq -c | sort -rn | head -20
rg -oiN '(free trial|no credit card|money.back|cancel anytime|prova gratuita|senza carta)' "$HTML" | sort -u | head -10

# ---------------------------------------------------------------- ads / tracking
section "TRACKING / ADS PIXELS (presence = literal string match in HTML)"
for probe in \
  "googletagmanager.com/gtm|Google Tag Manager" \
  "gtag\\(|Google gtag.js" \
  "google-analytics.com|Universal Analytics" \
  "googleads|googleadservices|AW-|Google Ads conversion" \
  "connect.facebook.net|fbq\\(|Meta Pixel" \
  "snap.licdn.com|_linkedin_partner_id|LinkedIn Insight Tag" \
  "analytics.tiktok.com|TikTok Pixel" \
  "static.ads-twitter.com|X (Twitter) Pixel" \
  "redditstatic.com/ads|Reddit Pixel" \
  "static.hotjar.com|Hotjar" \
  "clarity.ms|Microsoft Clarity" \
  "plausible.io|Plausible" \
  "posthog|PostHog" \
  "vercel/insights|_vercel/insights|Vercel Analytics" \
  "va.vercel-scripts|Vercel Speed Insights" \
  "segment.com/analytics|Segment" \
  "amplitude|Amplitude" \
  "mixpanel|Mixpanel" \
; do
  pat="${probe%%|*}"; name="${probe#*|}"
  if rg -qiN "$pat" "$HTML" 2>/dev/null; then echo "PRESENT  — $name"; else echo "absent   — $name"; fi
done

# ---------------------------------------------------------------- crawlability
ORIGIN="$(printf '%s' "$URL" | rg -oN '^https?://[^/]+' | head -1)"
section "ROBOTS.TXT  ($ORIGIN/robots.txt)"
curl -sS -A "$UA" -w '\n[http_code=%{http_code}]\n' "$ORIGIN/robots.txt" 2>&1 | head -40

section "SITEMAP DISCOVERY"
for sm in sitemap.xml sitemap-index.xml sitemap_index.xml; do
  code="$(curl -sS -o "$OUTDIR/$sm" -w '%{http_code}' -A "$UA" "$ORIGIN/$sm" 2>/dev/null)"
  echo "$ORIGIN/$sm -> HTTP $code"
  if [[ "$code" == "200" ]]; then
    printf '  <loc> count = %s\n' "$(rg -oic '<loc>' "$OUTDIR/$sm" 2>/dev/null || echo 0)"
    rg -oN '<loc>[^<]+</loc>' "$OUTDIR/$sm" 2>/dev/null | head -25
  fi
done

section "LLMS.TXT"
curl -sS -o /dev/null -w "$ORIGIN/llms.txt -> HTTP %{http_code}\n" -A "$UA" "$ORIGIN/llms.txt" 2>&1

section "DONE"
echo "raw_html=$HTML"
echo "headers=$HDRS"

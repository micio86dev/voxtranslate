# .claude/tools — support scripts for the marketing agent team

| File | Purpose |
|---|---|
| `EVIDENCE-PROTOCOL.md` | **Read first.** The no-invented-data rule, source tags, scoring rubric, evidence ledger format. Binding on every marketing agent. |
| `site-audit.sh` | Deterministic evidence extractor. `./site-audit.sh <url> [outdir]` — fetches the page and prints only literal facts: headers, title/meta, hreflang, headings, JSON-LD, forms, CTA links, contact surfaces, pricing strings, tracking pixels, robots.txt, sitemap, llms.txt. Saves raw HTML so any claim can be re-verified. |

## Adding API-backed data sources

Several metrics are `[UNVERIFIABLE]` without a paid data provider (keyword volume, domain
traffic, competitor ad spend). To upgrade them from `[UNVERIFIABLE]` to `[API]`, drop a
script here that reads a key from the environment and prints raw JSON, then reference it
from the relevant skill. Suggested slots:

| Metric | Provider that would resolve it | Env var |
|---|---|---|
| Keyword monthly volume, CPC, difficulty | DataForSEO / Semrush / Ahrefs / Google Ads API | `DATAFORSEO_LOGIN` / `SEMRUSH_API_KEY` / `AHREFS_TOKEN` |
| Domain organic traffic estimate | Ahrefs / Semrush / Similarweb | as above |
| Real SERP position for a keyword | DataForSEO SERP / SerpApi | `SERPAPI_KEY` |
| Owned-site real traffic & conversion | Google Search Console / GA4 / Vercel Analytics | OAuth / `VERCEL_TOKEN` |
| Competitor active ad creatives | Meta Ad Library API / Google Ads Transparency | `META_AD_LIBRARY_TOKEN` |

Until such a script exists, agents MUST report those metrics as `[UNVERIFIABLE]` — never estimate.

Note: the Meta Ad Library and the Google Ads Transparency Center are also browsable without
an API key and count as `[WEB]` evidence when the actual result page is fetched and quoted.

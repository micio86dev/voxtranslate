# Evidence Protocol — Marketing Agent Team

**This is the non-negotiable rule for every agent in the marketing team.**

## Rule 0 — No invented data

Every factual claim in an agent report MUST come from one of these four sources:

| Source tag | Meaning | How it is produced |
|---|---|---|
| `[SITE]` | Read from the target site itself | `curl`, `.claude/tools/site-audit.sh`, WebFetch, or browser automation |
| `[WEB]` | Read from a public third-party page | WebSearch + WebFetch of the actual result page |
| `[API]` | Returned by a tool/API call | Bash call to a real endpoint, MCP tool result |
| `[UNVERIFIABLE]` | Could not be confirmed with the tools available | — |

A claim with no tag is a defect. A reviewer must be able to re-run the same command
and get the same fact.

## Rule 1 — `[UNVERIFIABLE]` is a valid, expected answer

If a number cannot be measured with the tools actually available, the agent MUST write:

> `[UNVERIFIABLE]` — <what was needed> — <why it could not be obtained> — <what tool/access would resolve it>

Examples of things that are normally `[UNVERIFIABLE]` without a paid data source:

- Monthly keyword search volume (needs Google Keyword Planner / Ahrefs / Semrush / DataForSEO API)
- Estimated organic traffic of any domain (needs Ahrefs / Semrush / Similarweb API)
- Competitor ad spend, CPM, CTR benchmarks for a specific account
- Conversion rate, bounce rate, or funnel drop-off of a site you cannot instrument
- Follower counts on platforms that block unauthenticated reads

**Never** substitute an estimate, an industry average, or a plausible-looking number for a
missing measurement. Writing "≈ 12,000 searches/month" without an `[API]` source is a
protocol violation, even if the number happens to be close.

## Rule 2 — Distinguish measurement from judgement

- **Measurement** = a fact with a source tag. Immutable.
- **Judgement** = the agent's expert opinion (scores, priorities, rewrites). Allowed and
  expected, but must be labelled `[JUDGEMENT]` and must reference the measurements it rests on.

A score of 6/10 is a judgement. "The H1 is 'Real-time voice translation'" is a measurement.

## Rule 3 — Scoring rubric (shared by all agents)

All 1–10 scores use the same anchors, so the final report card is comparable across areas:

| Score | Meaning |
|---|---|
| 1–2 | Broken / absent. The dimension does not exist on this site. |
| 3–4 | Present but actively harmful or badly mis-executed. |
| 5–6 | Functional baseline. Nothing wrong, nothing that wins. |
| 7–8 | Competitive. Clearly deliberate work, minor gaps. |
| 9–10 | Best-in-category. Would be cited as an example. |

Every score must be accompanied by: the evidence it rests on, and the single change that
would move it up one point.

## Rule 4 — Evidence ledger

Every agent report ends with an **Evidence Ledger**: a table of every command / URL used, so
the whole analysis is reproducible.

| # | Source tag | Command or URL | What it established |
|---|---|---|---|

## Rule 5 — Confidence

Where a measurement is partial (e.g. one page sampled out of many), state the sample:
"3 of 14 pages inspected — nav, pricing, signup".

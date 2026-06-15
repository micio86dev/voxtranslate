# Pin the Railway server region (#113)

**Why.** The server does the latency-sensitive work on the subtitle path: it holds
the **Deepgram** streaming STT socket and fans translations out to **Groq**. Both
are US-based. If the Railway service runs in a region far from the users *and* from
Deepgram/Groq, every audio chunk and every translation pays a needless round-trip,
adding to subtitle latency. Pinning the region removes that variable (and stops a
silent region change on redeploy).

**No app/code change** — this is a Railway dashboard setting.

---

## 1. Choose the region

Two pulls, sometimes in tension:
- **Close to Deepgram/Groq** (both US) → a **US** region (e.g. `us-east`) minimizes
  the STT/translation hop, which is *per audio chunk* and therefore the dominant
  latency term.
- **Close to the users** → reduces the WS control-plane RTT.

Because the Deepgram/Groq hop is on the hot path and is US-bound, **default to a US
region (`us-east`)** unless your audience is overwhelmingly in one non-US area *and*
you measure a win. Re-evaluate with the k6 latency numbers (#114) if needed.

## 2. Pin it (Railway)

1. Railway → the **server** service → **Settings → Regions** (or *Deploy → Region*).
2. Select the region explicitly (don't leave it on "automatic").
3. **Redeploy** so the running instance moves: trigger a deploy from the dashboard,
   or `railway up` from `server/`.

> Single-instance, vertical-scale today (`railway.toml`, spec 0058) — one region is
> correct. Multi-region needs the shared room registry (a separate #114 follow-up),
> since rooms are in-memory per instance.

## 3. Verify

- Railway service shows the chosen region as **pinned**.
- After redeploy: open a call, speak, and confirm subtitles still flow.
- Compare subtitle latency before/after if you have the k6 / manual numbers; expect
  the STT round-trip component to drop when the server sits near Deepgram/Groq.

## Rollback

Re-select the previous region (or "automatic") in Settings and redeploy. State is
ephemeral (in-memory rooms), so there's nothing to migrate.

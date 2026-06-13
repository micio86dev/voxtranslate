# 0030 — Mobile-friendly Video Bitrate + Weak-network Warning

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0026](../0026-turn-relay/spec.md) |

## 1. Context & Problem

The call is a **WebRTC full mesh** capped at 4: each peer uploads its stream to
*every* other peer, so per-user upload ≈ `(N−1) × stream bitrate`. At 4-way that's
3 copies of your video on the uplink — fine on fibre, but rough on a mobile/4G
connection, where the uplink is the bottleneck and the result is lag/freezes. The
cap stays at **4** (mesh sweet spot — beyond that needs an SFU), but we should
(a) make the video **lighter so mobile uplinks cope**, and (b) **tell the user**
when their network can't keep up so they can drop to audio.

## 2. Goals / Non-Goals

**Goals**
- Lower the **outbound video bitrate** (and mobile capture resolution) so a 4-way
  mesh fits a typical 4G uplink; the browser's congestion control handles the rest.
- A non-intrusive **weak-network warning** ("try turning your camera off") when
  `getStats` shows a sustained bandwidth-limited / lossy uplink.

**Non-Goals**
- Raising the 4-peer cap (an SFU is a separate architecture + server-bandwidth
  cost; documented in this spec's context).
- A full connection-quality diagnostics panel (relay/srflx/RTT/loss) — tracked in
  [0026](../0026-turn-relay/spec.md) §8; this ships only the *warning*.
- Auto-disabling the camera — we *suggest*, the user decides.

## 3. Requirements

- **R1 — Bitrate cap.** Each peer connection's outbound video is capped via
  `RTCRtpSender.setParameters` (`maxBitrate`): **~1 Mbps desktop / ~450 kbps
  mobile**. (Best-effort — ignored where `setParameters` is unsupported.)
- **R2 — Mobile capture.** On mobile, `getUserMedia` captures **480p** (desktop
  keeps 720p).
- **R3 — Weak-network warning.** `getStats` is polled every 5 s across peers; when
  video `qualityLimitationReason === 'bandwidth'` **or** remote `fractionLost > 8 %`
  for **two consecutive** checks, a toast nudges the camera off — at most **once a
  minute**. Localized in all 8 languages (`weakNetwork`).

## 4. Design & Architecture

- `client/src/scripts/webrtc.ts` — `MeshManager` gains a `maxVideoBitrate`
  constructor arg + `onNetworkWeak` callback; `addPeer` caps the video sender's
  bitrate (`capVideoBitrate`) and starts a shared `getStats` poller
  (`startStatsMonitor`); `destroy` clears it.
- `client/src/scripts/app.ts` — `videoConstraints()` returns 480p on mobile;
  the mesh is constructed with `IS_MOBILE ? 450_000 : 1_000_000`;
  `mesh.onNetworkWeak` → `showWeakNetworkWarning()` (60 s cooldown toast).
- `client/src/scripts/i18n.ts` — `weakNetwork` in 8 languages.
- **Key decisions:**
  - *Cap bitrate, not just resolution* — `maxBitrate` directly bounds the uplink
    cost in the mesh, independent of resolution; congestion control still adapts
    below the cap.
  - *`qualityLimitationReason === 'bandwidth'` as the primary signal* — it's the
    browser literally saying "I'm reducing quality because of your uplink", which
    is exactly the condition to warn about; loss is a secondary trigger.
  - *Suggest, throttled* — a once-a-minute toast avoids nagging; the user keeps
    control (no auto-camera-off).
  - *Keep the 4 cap* — mesh is correct up to ~4–5; more would need an SFU.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `maxVideoBitrate` cap on each video sender | `webrtc.ts` |
| S1 | `getStats` weak-uplink monitor + `onNetworkWeak` | `webrtc.ts` |
| S2 | 480p mobile capture + wire the cap/warning | `app.ts` |
| S3 | `weakNetwork` i18n (8 langs) | `i18n.ts` |

## 6. Testing & Verification

- `astro check` clean; **101/101** client unit tests; coverage thresholds met
  (lines 91.5 % / functions 85.8 % ≥ 85 %); production build OK.
- The existing `webrtc.test.ts` still passes (the new constructor arg defaults; the
  monitor uses the global `setInterval` so it runs under the Node test env).
- The `getStats` monitor is browser-only behaviour — validated manually (throttle
  the uplink in dev tools → the warning appears).

## 7. Deployment & Operations

- Client-only — ships with the Vercel autodeploy on `main`. No server change.

## 8. Risks / Open Items

- The bitrate cap is a fixed target, not adaptive per-tier; the browser's own
  congestion control compensates. A future step is the full `getStats` diagnostics
  panel (0026 §8) to *show* the live quality, not just warn.

## 9. References

- Mesh bandwidth scaling: per-user upload ≈ `(N−1) × bitrate`.
- Files: `client/src/scripts/{webrtc,app,i18n}.ts`.

// Lazy-loaded in-call module bundle (spec 0105). The landing + lobby pages never make a
// call, so these modules — WebRTC mesh, chat, audio capture, the Soniox receive pipeline, the
// collaborative whiteboard and mini-games — are split OUT of the landing entry chunk and
// dynamically imported at PRE-JOIN entry. The download overlaps camera/device setup, so there
// is no perceived join latency, and a visitor who only browses the landing page never pays for
// any of it.
//
// These are the modules a REMOTE peer can trigger (a peer can start the whiteboard or a game),
// so they must all be present for the whole call — hence one bundle loaded up front, not per
// button. Purely local features a peer can't trigger (recording, screen-share PiP, virtual
// background, the post-call session screen) are split separately and imported on their own
// button click — see the `import('./…')` sites in app.ts.

export type CallModules = {
  webrtc: typeof import('./webrtc');
  chat: typeof import('./chat');
  soniox: typeof import('./soniox');
  audioCapture: typeof import('./audio-capture');
  pcmCapture: typeof import('./pcm-capture');
  micMeter: typeof import('./mic-meter');
  whiteboard: typeof import('./whiteboard');
  tictactoe: typeof import('./tictactoe');
  quiz: typeof import('./quiz');
};

let cached: Promise<CallModules> | null = null;

/**
 * Dynamically import the in-call modules (cached: one fetch per page). Warm it at pre-join so
 * the chunk downloads while the camera/devices initialise; `await` it again at join — the
 * cached promise resolves instantly when already loaded. On failure the cache is cleared so a
 * later attempt (e.g. after a transient offline blip) can retry.
 */
export function loadCallModules(): Promise<CallModules> {
  if (!cached) {
    cached = Promise.all([
      import('./webrtc'),
      import('./chat'),
      import('./soniox'),
      import('./audio-capture'),
      import('./pcm-capture'),
      import('./mic-meter'),
      import('./whiteboard'),
      import('./tictactoe'),
      import('./quiz'),
    ])
      .then(
        ([webrtc, chat, soniox, audioCapture, pcmCapture, micMeter, whiteboard, tictactoe, quiz]) => ({
          webrtc,
          chat,
          soniox,
          audioCapture,
          pcmCapture,
          micMeter,
          whiteboard,
          tictactoe,
          quiz,
        }),
      )
      .catch((e) => {
        cached = null; // allow a retry on the next attempt
        throw e;
      });
  }
  return cached;
}

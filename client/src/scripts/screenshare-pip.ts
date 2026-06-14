// Screen-share camera PiP (spec 0053). Draws the shared screen full-frame onto a
// hidden canvas with the sharer's camera composited as a small rounded overlay in
// the bottom-right corner, then captureStream()s a single video track. This keeps
// the mesh's one-video-sender-per-peer model — the composite is swapped onto the
// existing sender with replaceTrack, so there is NO WebRTC renegotiation.

/** Hard cap so re-encoding a 4K share doesn't pin the CPU; common 1080p shares
 *  stay pixel-exact (scale clamps to 1). */
const MAX_W = 1920;
const MAX_H = 1080;
const FPS = 30;
const FRAME_MS = 1000 / FPS;
/** PiP overlay as a fraction of the canvas width, plus its margin from the edges. */
const PIP_WIDTH_FRAC = 0.24;
const PIP_MARGIN_FRAC = 0.025;

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** Cap the composite canvas to MAX_W×MAX_H while preserving the screen's aspect
 *  ratio (never upscales). Returns a sane default while the source size is still
 *  unknown (videoWidth 0). */
export function fitCanvas(srcW: number, srcH: number, maxW = MAX_W, maxH = MAX_H): { w: number; h: number } {
  if (srcW <= 0 || srcH <= 0) return { w: 1280, h: 720 };
  const scale = Math.min(1, maxW / srcW, maxH / srcH);
  return { w: Math.round(srcW * scale), h: Math.round(srcH * scale) };
}

/** Bottom-right PiP rectangle for the camera overlay: PIP_WIDTH_FRAC of the canvas
 *  width, camera aspect preserved, inset by PIP_MARGIN_FRAC. */
export function pipRect(canvasW: number, canvasH: number, camW: number, camH: number): Rect {
  const w = Math.round(canvasW * PIP_WIDTH_FRAC);
  const aspect = camW > 0 && camH > 0 ? camH / camW : 9 / 16;
  const h = Math.round(w * aspect);
  const margin = Math.round(canvasW * PIP_MARGIN_FRAC);
  return { x: canvasW - w - margin, y: canvasH - h - margin, w, h };
}

/** Centre-crop source dims to fill `box` without distortion (object-fit: cover).
 *  Returns the source sub-rectangle to draw from. */
export function coverCrop(
  srcW: number,
  srcH: number,
  boxW: number,
  boxH: number,
): { sx: number; sy: number; sw: number; sh: number } {
  if (srcW <= 0 || srcH <= 0 || boxW <= 0 || boxH <= 0) {
    return { sx: 0, sy: 0, sw: srcW, sh: srcH };
  }
  const srcAspect = srcW / srcH;
  const boxAspect = boxW / boxH;
  if (srcAspect > boxAspect) {
    // Source is wider than the box → crop the sides.
    const sw = Math.round(srcH * boxAspect);
    return { sx: Math.round((srcW - sw) / 2), sy: 0, sw, sh: srcH };
  }
  // Source is taller than the box → crop top/bottom.
  const sh = Math.round(srcW / boxAspect);
  return { sx: 0, sy: Math.round((srcH - sh) / 2), sw: srcW, sh };
}

export class ScreenSharePip {
  private readonly canvas: HTMLCanvasElement;
  private readonly ctx: CanvasRenderingContext2D;
  /** Hidden <video> for the shared screen — drawImage needs a playing element. */
  private readonly screenVideo: HTMLVideoElement;
  /** Hidden <video> for the camera PiP; null when the camera is off. */
  private cameraVideo: HTMLVideoElement | null = null;
  private cameraTrack: MediaStreamTrack | null = null;
  private output: MediaStream | null = null;
  private rafId = 0;
  private tickId = 0;
  private lastFrame = 0;
  private running = false;

  constructor(screenStream: MediaStream) {
    this.canvas = document.createElement('canvas'); // detached — captureStream works
    // Seed the canvas from the track settings so the first captured frame is
    // already the right size (avoids a resize glitch mid-stream).
    const s = screenStream.getVideoTracks()[0]?.getSettings?.() ?? {};
    const init = fitCanvas(s.width ?? 0, s.height ?? 0);
    this.canvas.width = init.w;
    this.canvas.height = init.h;
    const ctx = this.canvas.getContext('2d');
    if (!ctx) throw new Error('2d canvas unavailable');
    this.ctx = ctx;
    this.screenVideo = document.createElement('video');
    this.screenVideo.muted = true;
    this.screenVideo.playsInline = true;
    this.screenVideo.srcObject = screenStream;
    void this.screenVideo.play().catch(() => {});
  }

  /** Begin the draw loop and return the composited stream to send to peers. */
  start(): MediaStream {
    this.running = true;
    this.lastFrame = 0;
    this.draw(); // paint one frame before capture so the track has dimensions
    this.output = this.canvas.captureStream(FPS);
    const loop = (now: number) => {
      if (!this.running) return;
      if (now - this.lastFrame >= FRAME_MS) {
        this.lastFrame = now;
        this.draw();
      }
      this.rafId = requestAnimationFrame(loop);
    };
    this.rafId = requestAnimationFrame(loop);
    // RAF pauses in background tabs — a 1s tick keeps frames flowing to peers so
    // the shared screen never freezes when the sharer tabs away (same trick as the
    // recording compositor).
    this.tickId = window.setInterval(() => this.draw(), 1000);
    return this.output;
  }

  /** The composited stream once started, else null. */
  get stream(): MediaStream | null {
    return this.output;
  }

  /** Point the PiP at a camera track (raw or blurred), or null to hide it. Used at
   *  start and whenever the camera is toggled / re-processed mid-share. */
  setCamera(track: MediaStreamTrack | null): void {
    if (track === this.cameraTrack) return;
    this.cameraTrack = track;
    if (!track) {
      if (this.cameraVideo) {
        this.cameraVideo.pause();
        this.cameraVideo.srcObject = null;
        this.cameraVideo = null;
      }
      return;
    }
    if (!this.cameraVideo) {
      this.cameraVideo = document.createElement('video');
      this.cameraVideo.muted = true;
      this.cameraVideo.playsInline = true;
    }
    this.cameraVideo.srcObject = new MediaStream([track]);
    void this.cameraVideo.play().catch(() => {});
  }

  stop(): void {
    this.running = false;
    cancelAnimationFrame(this.rafId);
    clearInterval(this.tickId);
    this.output?.getTracks().forEach((t) => t.stop());
    this.output = null;
    this.screenVideo.pause();
    this.screenVideo.srcObject = null;
    if (this.cameraVideo) {
      this.cameraVideo.pause();
      this.cameraVideo.srcObject = null;
      this.cameraVideo = null;
    }
    this.cameraTrack = null;
  }

  private draw(): void {
    const { ctx, canvas, screenVideo } = this;
    // Resize the canvas to the screen's live resolution (capped) once it's known.
    if (screenVideo.videoWidth > 0 && screenVideo.videoHeight > 0) {
      const fit = fitCanvas(screenVideo.videoWidth, screenVideo.videoHeight);
      if (canvas.width !== fit.w || canvas.height !== fit.h) {
        canvas.width = fit.w;
        canvas.height = fit.h;
      }
    }
    ctx.fillStyle = '#000';
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    if (screenVideo.videoWidth > 0) {
      ctx.drawImage(screenVideo, 0, 0, canvas.width, canvas.height);
    }
    this.drawPip();
  }

  private drawPip(): void {
    const cam = this.cameraVideo;
    if (!cam || cam.videoWidth === 0 || cam.videoHeight === 0) return;
    const { ctx, canvas } = this;
    const r = pipRect(canvas.width, canvas.height, cam.videoWidth, cam.videoHeight);
    const crop = coverCrop(cam.videoWidth, cam.videoHeight, r.w, r.h);
    const radius = Math.round(Math.min(r.w, r.h) * 0.12);
    // Backing + drop shadow so the PiP reads as a card floating over the screen.
    ctx.save();
    ctx.shadowColor = 'rgba(0, 0, 0, 0.5)';
    ctx.shadowBlur = Math.round(r.w * 0.04);
    ctx.shadowOffsetY = 2;
    ctx.fillStyle = '#000';
    this.roundRectPath(r, radius);
    ctx.fill();
    ctx.restore();
    // Clip to the rounded rect and draw the centre-cropped camera frame.
    ctx.save();
    this.roundRectPath(r, radius);
    ctx.clip();
    ctx.drawImage(cam, crop.sx, crop.sy, crop.sw, crop.sh, r.x, r.y, r.w, r.h);
    ctx.restore();
    // Hairline border.
    ctx.save();
    this.roundRectPath(r, radius);
    ctx.lineWidth = 2;
    ctx.strokeStyle = 'rgba(255, 255, 255, 0.85)';
    ctx.stroke();
    ctx.restore();
  }

  private roundRectPath(r: Rect, radius: number): void {
    const { ctx } = this;
    ctx.beginPath();
    if (typeof ctx.roundRect === 'function') {
      ctx.roundRect(r.x, r.y, r.w, r.h, radius);
    } else {
      ctx.rect(r.x, r.y, r.w, r.h);
    }
  }
}

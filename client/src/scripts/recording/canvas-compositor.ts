// Composite recording (spec 0010) — draws every participant onto a hidden
// 1280×720 canvas at ~30fps. Camera-off (or frame-less) participants get a
// placeholder tile: initials disc + name. A name pill overlays every tile.

import { COMP_W, COMP_H, computeLayout, containFit, type Tile } from './layout';
import { hueOf, initials } from './utils';
import type { ParticipantSource } from './types';

const FPS = 30;
const FRAME_MS = 1000 / FPS;
const PLACEHOLDER_BG = '#1a1b26';

export class CanvasCompositor {
  private readonly canvas: HTMLCanvasElement;
  private readonly ctx: CanvasRenderingContext2D;
  /** Hidden <video> per participant — drawImage needs a playing element. */
  private readonly videos = new Map<string, HTMLVideoElement>();
  private sources: ParticipantSource[] = [];
  private rafId = 0;
  private tickId = 0;
  private lastFrame = 0;
  private running = false;

  constructor() {
    // Never appended to the DOM — captureStream works on detached canvases.
    this.canvas = document.createElement('canvas');
    this.canvas.width = COMP_W;
    this.canvas.height = COMP_H;
    const ctx = this.canvas.getContext('2d');
    if (!ctx) throw new Error('2d canvas unavailable');
    this.ctx = ctx;
  }

  start(sources: ParticipantSource[]): void {
    this.setSources(sources);
    this.running = true;
    this.lastFrame = 0;
    const loop = (now: number) => {
      if (!this.running) return;
      if (now - this.lastFrame >= FRAME_MS) {
        this.lastFrame = now;
        this.draw();
      }
      this.rafId = requestAnimationFrame(loop);
    };
    this.rafId = requestAnimationFrame(loop);
    // rAF pauses in background tabs (documented limitation) — a 1s safety
    // tick keeps frames flowing at reduced rate so the recording never stalls.
    this.tickId = window.setInterval(() => this.draw(), 1000);
  }

  captureStream(fps = FPS): MediaStream {
    return this.canvas.captureStream(fps);
  }

  /** Swap the participant set; the next frame draws the new layout. */
  setSources(list: ParticipantSource[]): void {
    this.sources = [...list];
    const keep = new Set(list.map((s) => s.peerId));
    for (const [peerId, video] of this.videos) {
      if (!keep.has(peerId)) this.dropVideo(peerId, video);
    }
    for (const s of list) this.syncVideo(s);
  }

  updateSource(peerId: string, stream: MediaStream | null): void {
    const src = this.sources.find((s) => s.peerId === peerId);
    if (!src) return;
    src.stream = stream;
    this.syncVideo(src);
  }

  setVideoOff(peerId: string, off: boolean): void {
    const src = this.sources.find((s) => s.peerId === peerId);
    if (src) src.videoOff = off;
  }

  stop(): void {
    this.running = false;
    cancelAnimationFrame(this.rafId);
    clearInterval(this.tickId);
    for (const [peerId, video] of this.videos) this.dropVideo(peerId, video);
    this.sources = [];
  }

  private syncVideo(src: ParticipantSource): void {
    let video = this.videos.get(src.peerId);
    if (!video) {
      video = document.createElement('video');
      video.muted = true;
      video.playsInline = true;
      this.videos.set(src.peerId, video);
    }
    // Re-bind only when the underlying video TRACK changes — not when the caller
    // hands a fresh MediaStream wrapper around the same track. selfRecordingStream()
    // and whiteboard.captureStream() build a NEW MediaStream every roster tick
    // (~1×/s via syncRoster); reassigning srcObject reloads the element (videoWidth
    // drops to 0 for a frame → placeholder), which made the self tile flicker like
    // the camera was toggling off/on. Comparing tracks keeps a steady stream steady.
    const curTrack = (video.srcObject as MediaStream | null)?.getVideoTracks()[0] ?? null;
    const nextTrack = src.stream?.getVideoTracks()[0] ?? null;
    if (curTrack !== nextTrack) {
      video.srcObject = src.stream;
      if (src.stream) void video.play().catch(() => {});
    }
  }

  private dropVideo(peerId: string, video: HTMLVideoElement): void {
    video.pause();
    video.srcObject = null;
    this.videos.delete(peerId);
  }

  private draw(): void {
    const { ctx } = this;
    ctx.fillStyle = '#000';
    ctx.fillRect(0, 0, COMP_W, COMP_H);
    const layout = computeLayout(this.sources.length);
    this.sources.slice(0, layout.length).forEach((src, i) => {
      const tile = layout[i]!;
      const video = this.videos.get(src.peerId);
      const hasFrames = !!video && video.videoWidth > 0 && video.videoHeight > 0;
      if (!src.videoOff && hasFrames) {
        const fit = containFit(video!.videoWidth, video!.videoHeight, tile);
        ctx.drawImage(video!, fit.x, fit.y, fit.w, fit.h);
      } else {
        this.drawPlaceholder(src.name, tile);
      }
      this.drawNamePill(src.name, tile);
    });
  }

  private drawPlaceholder(name: string, tile: Tile): void {
    const { ctx } = this;
    ctx.fillStyle = PLACEHOLDER_BG;
    ctx.fillRect(tile.x, tile.y, tile.w, tile.h);
    const r = Math.min(tile.w, tile.h) * 0.18;
    const cx = tile.x + tile.w / 2;
    const cy = tile.y + tile.h / 2;
    ctx.fillStyle = `hsl(${hueOf(name)}, 60%, 25%)`;
    ctx.beginPath();
    ctx.arc(cx, cy, r, 0, Math.PI * 2);
    ctx.fill();
    ctx.fillStyle = '#fff';
    ctx.font = `600 ${Math.round(r * 0.8)}px system-ui, sans-serif`;
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText(initials(name) || '?', cx, cy);
  }

  private drawNamePill(name: string, tile: Tile): void {
    const { ctx } = this;
    const font = '14px system-ui, sans-serif';
    ctx.font = font;
    const padX = 8;
    const h = 22;
    const w = Math.min(ctx.measureText(name).width + padX * 2, tile.w - 12);
    const x = tile.x + 6;
    const y = tile.y + tile.h - h - 6;
    ctx.save();
    // Clip to the tile so long names never bleed into a neighbour.
    ctx.beginPath();
    ctx.rect(tile.x, tile.y, tile.w, tile.h);
    ctx.clip();
    ctx.fillStyle = 'rgba(0, 0, 0, 0.55)';
    if (typeof ctx.roundRect === 'function') {
      ctx.beginPath();
      ctx.roundRect(x, y, w, h, 6);
      ctx.fill();
    } else {
      ctx.fillRect(x, y, w, h);
    }
    ctx.fillStyle = '#fff';
    ctx.font = font;
    ctx.textAlign = 'left';
    ctx.textBaseline = 'middle';
    ctx.fillText(name, x + padX, y + h / 2 + 1, w - padX * 2);
    ctx.restore();
  }
}

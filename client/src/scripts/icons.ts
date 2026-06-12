// Inline stroke icons (Lucide/Feather style). Zero dependencies, ~1KB total —
// each icon is just SVG path data drawn in `currentColor`, so it inherits the
// button's color and stays razor-light.

const PATHS: Record<string, string> = {
  mic:
    '<path d="M12 2a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3z"/><path d="M19 10v1a7 7 0 0 1-14 0v-1"/><line x1="12" y1="19" x2="12" y2="22"/><line x1="8" y1="22" x2="16" y2="22"/>',
  'mic-off':
    '<line x1="2" y1="2" x2="22" y2="22"/><path d="M9 9v2a3 3 0 0 0 5.12 2.12"/><path d="M15 9.34V5a3 3 0 0 0-5.94-.6"/><path d="M17 16.95A7 7 0 0 1 5 11v-1m14 0v1a7 7 0 0 1-.11 1.23"/><line x1="12" y1="19" x2="12" y2="22"/><line x1="8" y1="22" x2="16" y2="22"/>',
  video:
    '<path d="m22 8-6 4 6 4V8z"/><rect x="2" y="6" width="14" height="12" rx="2"/>',
  'video-off':
    '<path d="M16 16v1a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h2m4 0h2a2 2 0 0 1 2 2v3l3-2 3 2v6"/><line x1="2" y1="2" x2="22" y2="22"/>',
  'volume-on':
    '<path d="M4 9v6h4l5 4V5L8 9H4z"/><path d="M16 8.5a4 4 0 0 1 0 7"/><path d="M18.5 6a7 7 0 0 1 0 12"/>',
  'volume-off':
    '<path d="M4 9v6h4l5 4V5L8 9H4z"/><line x1="22" y1="9" x2="16" y2="15"/><line x1="16" y1="9" x2="22" y2="15"/>',
  chat:
    '<path d="M21 14a2 2 0 0 1-2 2H8l-4 4V6a2 2 0 0 1 2-2h13a2 2 0 0 1 2 2z"/>',
  leave:
    '<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><polyline points="16 17 21 12 16 7"/><line x1="21" y1="12" x2="9" y2="12"/>',
  shuffle:
    '<polyline points="16 3 21 3 21 8"/><line x1="4" y1="20" x2="21" y2="3"/><polyline points="21 16 21 21 16 21"/><line x1="15" y1="15" x2="21" y2="21"/><line x1="4" y1="4" x2="9" y2="9"/>',
  users:
    '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>',
  send:
    '<line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/>',
  close:
    '<line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/>',
  copy:
    '<rect x="9" y="9" width="12" height="12" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>',
  shield:
    '<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/>',
  flag:
    '<path d="M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1z"/><line x1="4" y1="22" x2="4" y2="15"/>',
  block:
    '<circle cx="12" cy="12" r="10"/><line x1="4.9" y1="4.9" x2="19.1" y2="19.1"/>',
  hand:
    '<path d="M18 8.5V4a2 2 0 0 0-4 0v4.5"/><path d="M14 8.5V2a2 2 0 0 0-4 0v6.5"/><path d="M10 8.5V4a2 2 0 0 0-4 0v8.5"/><path d="M22 12.5V10a2 2 0 0 0-4 0v2.5"/><rect x="6" y="12" width="16" height="10" rx="2"/>',
  'hand-raised':
    '<path d="M18 8.5V4a2 2 0 0 0-4 0v4.5"/><path d="M14 8.5V2a2 2 0 0 0-4 0v6.5"/><path d="M10 8.5V4a2 2 0 0 0-4 0v8.5"/><path d="M22 12.5V10a2 2 0 0 0-4 0v2.5"/><rect x="6" y="12" width="16" height="10" rx="2"/><circle cx="18" cy="7" r="3" fill="currentColor" stroke="none"/>',
  fullscreen:
    '<path d="M8 3H5a2 2 0 0 0-2 2v3m18 0V5a2 2 0 0 0-2-2h-3m0 18h3a2 2 0 0 0 2-2v-3M3 16v3a2 2 0 0 0 2 2h3"/>',
  'fullscreen-off':
    '<path d="M8 3v3a2 2 0 0 1-2 2H3m18 0h-3a2 2 0 0 1-2-2V3m0 18v-3a2 2 0 0 1 2-2h3M3 16h3a2 2 0 0 1 2 2v3"/>',
  pip:
    '<path d="M2 6a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2z"/><rect x="10" y="11" width="8" height="5" rx="1"/>',
  pin:
    '<path d="M12 2v20M2 12h20"/>',
  'pin-off':
    '<path d="M12 2v20M2 12h20"/><line x1="2" y1="2" x2="22" y2="22"/>',
  grid:
    '<rect x="3" y="3" width="7" height="7"/><rect x="14" y="3" width="7" height="7"/><rect x="3" y="14" width="7" height="7"/><rect x="14" y="14" width="7" height="7"/>',
  speaker:
    '<rect x="4" y="4" width="16" height="16" rx="2"/><rect x="14" y="14" width="6" height="6" rx="1"/><rect x="4" y="14" width="8" height="6" rx="1"/>',
  monitor:
    '<rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/>',
  recording:
    '<circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="3" fill="currentColor" stroke="none"/>',
  subtitle:
    '<rect x="2" y="5" width="20" height="14" rx="2"/><line x1="6" y1="11" x2="18" y2="11"/><line x1="6" y1="15" x2="14" y2="15"/>',
  'subtitle-off':
    '<rect x="2" y="5" width="20" height="14" rx="2"/><line x1="6" y1="11" x2="18" y2="11"/><line x1="6" y1="15" x2="14" y2="15"/><line x1="2" y1="2" x2="22" y2="22"/>',
  bookmark:
    '<path d="m19 21-7-4-7 4V5a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2v16z"/>',
  pencil:
    '<path d="M17 3a2.85 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5z"/>',
  trash:
    '<polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>',
  book:
    '<path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20"/><path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z"/>',
  // Background effects (sparkles) — used by the camera blur toggle.
  sparkles:
    '<path d="M12 3l1.9 5.1L19 10l-5.1 1.9L12 17l-1.9-5.1L5 10l5.1-1.9z"/><path d="M19 15l.8 2.2L22 18l-2.2.8L19 21l-.8-2.2L16 18l2.2-.8z"/><path d="M5 4l.6 1.6L7 6l-1.4.4L5 8l-.6-1.6L3 6l1.4-.4z"/>',
  // Chat file attachment (paperclip) — the upload button (spec 0018).
  paperclip:
    '<path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48"/>',
  // Generic file (document) — fallback chip glyph for non-audio attachments.
  file:
    '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/>',
};

/** Return inline SVG markup for `name`, drawn in `currentColor`. */
export function icon(name: string, size = 22): string {
  return (
    `<svg class="ico" width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" ` +
    `stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" ` +
    `aria-hidden="true" focusable="false">${PATHS[name] ?? ''}</svg>`
  );
}

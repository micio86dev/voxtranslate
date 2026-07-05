import { describe, expect, it } from 'vitest';

import { icon } from './icons';

/** The SVG body between the opening tag and `</svg>`. */
const inner = (markup: string): string =>
  markup.replace(/^<svg [^>]*>/, '').replace(/<\/svg>$/, '');

describe('icon', () => {
  it('returns inline SVG drawn in currentColor with a11y attributes', () => {
    const svg = icon('mic');
    expect(svg.startsWith('<svg class="ico"')).toBe(true);
    expect(svg.endsWith('</svg>')).toBe(true);
    expect(svg).toContain('stroke="currentColor"');
    expect(svg).toContain('viewBox="0 0 24 24"');
    expect(svg).toContain('aria-hidden="true"');
    expect(svg).toContain('focusable="false"');
    // The mic glyph's capsule path.
    expect(svg).toContain('M12 2a3 3 0 0 0-3 3');
  });

  it('defaults to 22px and honours a custom size', () => {
    expect(icon('chat')).toContain('width="22" height="22"');
    expect(icon('chat', 16)).toContain('width="16" height="16"');
  });

  it('renders an empty (but valid) SVG shell for an unknown name', () => {
    const svg = icon('definitely-not-an-icon');
    expect(inner(svg)).toBe('');
    expect(svg.startsWith('<svg')).toBe(true);
    expect(svg.endsWith('</svg>')).toBe(true);
  });

  it('has distinct path data per glyph', () => {
    expect(inner(icon('mic'))).not.toBe(inner(icon('mic-off')));
    expect(inner(icon('video'))).not.toBe(inner(icon('video-off')));
  });

  it('renders non-empty bodies for the glyphs the call UI uses', () => {
    const names = [
      'mic',
      'mic-off',
      'video',
      'video-off',
      'volume-on',
      'volume-off',
      'chat',
      'leave',
      'shuffle',
      'users',
      'user',
      'info',
      'send',
      'close',
      'copy',
      'shield',
      'wallet',
      'bell',
      'flag',
      'block',
      'bug',
      'hand',
      'hand-raised',
      'fullscreen',
      'fullscreen-off',
      'pip',
      'pin',
      'pin-off',
      'grid',
      'speaker',
      'monitor',
      'recording',
      'subtitle',
      'subtitle-off',
      'bookmark',
      'pencil',
      'board',
      'eraser',
      'game',
      'quiz',
      'trash',
      'book',
      'more',
      'move',
      'sparkles',
      'timer',
      'paperclip',
      'file',
      'highlighter',
      'line',
      'arrow',
      'square',
      'circle',
      'plus',
      'download',
      'chevron-left',
      'chevron-right',
      'user-plus',
      'link',
      'mail',
      'headphones',
    ];
    for (const name of names) {
      expect(inner(icon(name)), `icon "${name}" should have path data`).not.toBe('');
    }
  });
});

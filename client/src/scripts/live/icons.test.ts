import { describe, expect, it } from 'vitest';
import { icon } from '../icons';

describe('live-connect icon (home entries)', () => {
  it('renders a non-empty glyph distinct from the Talk and call icons', () => {
    const body = (n: string) => icon(n).replace(/^<svg [^>]*>/, '').replace(/<\/svg>$/, '');
    expect(body('live-connect')).not.toBe('');
    expect(body('live-connect')).not.toBe(body('chat'));
    expect(body('live-connect')).not.toBe(body('users'));
    expect(body('live-connect')).not.toBe(body('phone'));
  });
});

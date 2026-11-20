// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';

const toDataURL = vi.fn();
let loadFails = false;
vi.mock('qrcode', () => ({
  get toDataURL() {
    if (loadFails) throw new Error('chunk failed');
    return toDataURL;
  },
}));
const toast = vi.fn();
vi.mock('./toast', () => ({ toast: (...a: unknown[]) => toast(...a) }));

import { downloadQr, printQr, qrDataUrl, renderQr } from './qr';

beforeEach(() => {
  toDataURL.mockReset();
  toast.mockReset();
  loadFails = false;
  document.body.innerHTML = '';
});

describe('renderQr', () => {
  it('renders a 160px data URL into the image', async () => {
    toDataURL.mockResolvedValue('data:image/png;base64,AAA');
    const img = document.createElement('img');
    await renderQr(img, 'https://x.test/lc/abc');
    expect(toDataURL).toHaveBeenCalledWith('https://x.test/lc/abc', { margin: 1, width: 160 });
    expect(img.src).toBe('data:image/png;base64,AAA');
    expect(img.classList.contains('hidden')).toBe(false);
  });

  it('hides the image when the qrcode chunk cannot load', async () => {
    loadFails = true;
    const img = document.createElement('img');
    await renderQr(img, 'x');
    expect(img.classList.contains('hidden')).toBe(true);
  });
});

describe('qrDataUrl', () => {
  it('asks for the large print-quality render', async () => {
    toDataURL.mockResolvedValue('data:big');
    await expect(qrDataUrl('u', 'fallback')).resolves.toBe('data:big');
    expect(toDataURL).toHaveBeenCalledWith('u', { margin: 2, width: 512 });
  });

  it('falls back to the already-rendered source on failure', async () => {
    loadFails = true;
    await expect(qrDataUrl('u', 'fallback')).resolves.toBe('fallback');
  });
});

describe('downloadQr', () => {
  it('clicks a temporary anchor named after the code, then removes it', () => {
    const clicked: Array<{ href: string; download: string }> = [];
    const spy = vi
      .spyOn(HTMLAnchorElement.prototype, 'click')
      .mockImplementation(function (this: HTMLAnchorElement) {
        clicked.push({ href: this.href, download: this.download });
      });
    downloadQr('data:image/png;base64,AAA', 'ab12');
    expect(clicked).toEqual([{ href: 'data:image/png;base64,AAA', download: 'webinar-ab12.png' }]);
    expect(document.querySelector('a')).toBeNull();
    spy.mockRestore();
  });
});

describe('printQr', () => {
  it('tells the user when the print window is blocked', () => {
    vi.spyOn(window, 'open').mockReturnValue(null);
    printQr('data:x', 'Title', 'https://x.test/j');
    expect(toast).toHaveBeenCalledWith('Allow pop-ups to print the QR code.', 'err');
  });

  it('builds the page with textContent only and prints once the image is ready', () => {
    const doc = document.implementation.createHTMLDocument('');
    const win = { document: doc, focus: vi.fn(), print: vi.fn() };
    vi.spyOn(window, 'open').mockReturnValue(win as unknown as Window);
    printQr('data:x', '<b>Title</b>', 'https://x.test/j');
    expect(doc.title).toBe('<b>Title</b>');
    expect(doc.querySelector('h1')?.textContent).toBe('<b>Title</b>');
    expect(doc.querySelector('b')).toBeNull();
    expect(doc.querySelector('p')?.textContent).toBe('https://x.test/j');
    const img = doc.querySelector('img') as HTMLImageElement;
    expect(img.getAttribute('src')).toBe('data:x');
    // jsdom never decodes images, so printing waits for the load/error event.
    expect(win.print).not.toHaveBeenCalled();
    img.dispatchEvent(new Event('load'));
    expect(win.print).toHaveBeenCalledTimes(1);
    expect(win.focus).toHaveBeenCalled();
  });
});

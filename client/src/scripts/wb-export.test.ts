import { describe, it, expect } from 'vitest';
import { jpegPagesToPdf, pngBlob } from './wb-export';

// jpegPagesToPdf is pure (TextEncoder/atob/Blob are Node globals): assert the PDF
// structure rather than rendering. The embedded "JPEG" bytes are arbitrary base64 —
// the assembler copies them verbatim, so any base64 exercises the same code path.
const page = (n: number) => ({ dataUrl: 'data:image/jpeg;base64,QUJD', w: 100 + n, h: 50 + n });

async function pdfText(blob: Blob): Promise<string> {
  return new TextDecoder('latin1').decode(new Uint8Array(await blob.arrayBuffer()));
}

describe('jpegPagesToPdf (spec 0062)', () => {
  it('emits a valid single-page PDF skeleton', async () => {
    const blob = jpegPagesToPdf([page(0)]);
    expect(blob.type).toBe('application/pdf');
    const t = await pdfText(blob);
    expect(t.startsWith('%PDF-1.3')).toBe(true);
    expect(t).toContain('/Type /Catalog');
    expect(t).toContain('/Count 1');
    expect(t).toContain('/Filter /DCTDecode');
    expect(t).toContain('/Length 3'); // "ABC" → 3 bytes
    expect(t).toContain('startxref');
    expect(t.trimEnd().endsWith('%%EOF')).toBe(true);
  });

  it('one page object + trio per page; /Count tracks page total', async () => {
    const t = await pdfText(jpegPagesToPdf([page(0), page(1), page(2)]));
    expect(t).toContain('/Count 3');
    // 3 pages → objects 1 Catalog + 2 Pages + 3×3 = 11; xref header "0 12".
    expect(t).toContain('xref\n0 12');
    expect((t.match(/\/Type \/Page\b/g) ?? []).length).toBe(3);
    expect((t.match(/\/DCTDecode/g) ?? []).length).toBe(3);
    // MediaBox carries each page's own dimensions.
    expect(t).toContain('/MediaBox [0 0 100 50]');
    expect(t).toContain('/MediaBox [0 0 102 52]');
  });
});

describe('pngBlob', () => {
  it('resolves with the Blob that canvas.toBlob yields', async () => {
    const expected = new Blob(['png'], { type: 'image/png' });
    // Duck-typed canvas: toBlob invokes its callback with the produced blob.
    const canvas = {
      toBlob: (cb: (b: Blob | null) => void, type?: string) => {
        expect(type).toBe('image/png');
        cb(expected);
      },
    } as unknown as HTMLCanvasElement;
    await expect(pngBlob(canvas)).resolves.toBe(expected);
  });

  it('resolves null when the canvas cannot produce a blob', async () => {
    const canvas = {
      toBlob: (cb: (b: Blob | null) => void) => cb(null),
    } as unknown as HTMLCanvasElement;
    await expect(pngBlob(canvas)).resolves.toBeNull();
  });
});

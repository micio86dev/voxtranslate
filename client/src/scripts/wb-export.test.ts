import { describe, it, expect } from 'vitest';
import { jpegPagesToPdf } from './wb-export';

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

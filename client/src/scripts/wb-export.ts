// Zero-dependency whiteboard export (spec 0062 / #96). PNG via canvas.toBlob; a
// multi-page PDF hand-assembled from per-page JPEGs embedded as /DCTDecode image
// XObjects (JPEG bytes pass straight through — no compression library needed).

/** Click an off-DOM <a download> to save a Blob, then release the object URL. */
export function triggerDownload(blob: Blob, filename: string): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** A canvas → PNG Blob (promise wrapper around toBlob). */
export function pngBlob(canvas: HTMLCanvasElement): Promise<Blob | null> {
  return new Promise((resolve) => canvas.toBlob(resolve, 'image/png'));
}

/** Decode a base64 string (no `data:` prefix) to raw bytes. */
function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export interface PdfPageImage {
  dataUrl: string; // a "data:image/jpeg;base64,..." URL
  w: number;
  h: number;
}

/** Build a multi-page PDF where each page is one full-bleed JPEG image. Objects:
 *  1 Catalog, 2 Pages, then a (image, content, page) trio per page. */
export function jpegPagesToPdf(pages: PdfPageImage[]): Blob {
  const enc = new TextEncoder();
  const chunks: Uint8Array[] = [];
  let len = 0;
  const push = (data: Uint8Array | string): void => {
    const u = typeof data === 'string' ? enc.encode(data) : data;
    chunks.push(u);
    len += u.length;
  };

  const n = Math.max(1, pages.length);
  const objCount = 2 + n * 3;
  const objOffset = new Array<number>(objCount + 1).fill(0);
  const obj = (num: number, body: () => void): void => {
    objOffset[num] = len;
    push(`${num} 0 obj\n`);
    body();
    push('endobj\n');
  };

  push('%PDF-1.3\n');
  push(new Uint8Array([0x25, 0xff, 0xff, 0xff, 0xff, 0x0a])); // binary marker

  // page object numbers are the 3rd of each trio: 5, 8, 11, …
  const pageNums = pages.map((_, i) => 5 + i * 3);
  obj(1, () => push('<< /Type /Catalog /Pages 2 0 R >>\n'));
  obj(2, () => push(`<< /Type /Pages /Count ${n} /Kids [${pageNums.map((p) => `${p} 0 R`).join(' ')}] >>\n`));

  pages.forEach((page, i) => {
    const imgNum = 3 + i * 3;
    const contentNum = 4 + i * 3;
    const pageNum = 5 + i * 3;
    const bytes = base64ToBytes(page.dataUrl.slice(page.dataUrl.indexOf(',') + 1));
    obj(imgNum, () => {
      push(
        `<< /Type /XObject /Subtype /Image /Width ${page.w} /Height ${page.h} ` +
          `/ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length ${bytes.length} >>\nstream\n`,
      );
      push(bytes);
      push('\nendstream\n');
    });
    const content = `q\n${page.w} 0 0 ${page.h} 0 0 cm\n/Im0 Do\nQ\n`;
    obj(contentNum, () => push(`<< /Length ${enc.encode(content).length} >>\nstream\n${content}endstream\n`));
    obj(pageNum, () =>
      push(
        `<< /Type /Page /Parent 2 0 R /MediaBox [0 0 ${page.w} ${page.h}] ` +
          `/Resources << /XObject << /Im0 ${imgNum} 0 R >> >> /Contents ${contentNum} 0 R >>\n`,
      ),
    );
  });

  const xref = len;
  push(`xref\n0 ${objCount + 1}\n`);
  push('0000000000 65535 f \n');
  for (let i = 1; i <= objCount; i++) push(`${String(objOffset[i]).padStart(10, '0')} 00000 n \n`);
  push(`trailer\n<< /Size ${objCount + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`);

  return new Blob(chunks as BlobPart[], { type: 'application/pdf' });
}

// Pure subtitle rendering (extracted from app.ts `showSubtitle` so it is unit-testable
// in jsdom without booting the whole call screen). Given the current display mode and a
// subtitle's parts, it decides which line(s) to show and writes them into the DOM.
//
// The mode is the 4-state display cycle from the subtitle button:
//   'translated' → only the translation | 'original' → only the source
//   'both' → translation (prominent) + source line beneath | 'off' → handled by the caller.

export type SubtitleMode = "off" | "translated" | "original" | "both";

export interface SubtitleParts {
  /** The listener's-language translation (server tiers: `translations[myLang]`; Enhanced: the
   *  flushed Groq translation). */
  translation?: string;
  /** The speaker's original words (server `subtitle_final.original` / Cartesia interim). */
  original?: string;
  /** True while the segment is still streaming (an interim frame). */
  interim: boolean;
}

/** One styled line to render. `subtitle-translation` is the prominent main line (whatever its
 *  language); `subtitle-original` is the smaller source line shown beneath it in 'both' mode. */
export interface SubtitleLine {
  cls: "subtitle-translation" | "subtitle-original";
  text: string;
}

/** The line(s) the given mode wants. Empty when there is nothing to show for this mode
 *  (e.g. an interim frame that only carries the source while showing only translations —
 *  the caller then leaves whatever is on screen rather than blanking it). Pure. */
export function subtitleLines(
  mode: SubtitleMode,
  { translation, original, interim }: SubtitleParts,
): SubtitleLine[] {
  const lines: SubtitleLine[] = [];
  if (mode === "translated") {
    if (translation)
      lines.push({ cls: "subtitle-translation", text: translation });
  } else if (mode === "original") {
    if (original) lines.push({ cls: "subtitle-translation", text: original });
  } else if (mode === "both") {
    const main = translation ?? original;
    if (main) lines.push({ cls: "subtitle-translation", text: main });
    // The smaller source line only makes sense on a FINAL frame that has both a translation
    // and a distinct original.
    if (!interim && translation && original && original !== translation) {
      lines.push({ cls: "subtitle-original", text: original });
    }
  }
  return lines;
}

/** Render the subtitle box into `area` (replacing its content). Returns `false` when there is
 *  nothing to show for this mode, so the caller can leave the previous caption on screen.
 *  This is the exact on-screen render path used by the live call — unit-tested in jsdom. */
export function renderSubtitleInto(
  area: HTMLElement,
  mode: SubtitleMode,
  parts: SubtitleParts,
): boolean {
  const lines = subtitleLines(mode, parts);
  if (!lines.length) return false;
  const doc = area.ownerDocument;
  area.innerHTML = "";
  const box = doc.createElement("div");
  box.className = `subtitle${parts.interim ? " subtitle-interim" : ""}`;
  for (const line of lines) {
    const span = doc.createElement("span");
    span.className = line.cls;
    span.textContent = line.text;
    box.appendChild(span);
  }
  area.appendChild(box);
  return true;
}

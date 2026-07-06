// @vitest-environment jsdom
// On-screen render verification for subtitles (jsdom). Proves that, given the exact
// `subtitle_final` a foreign listener receives, the translated text lands in the DOM —
// closing the loop with the server-side end-to-end test that produces that message.
import { describe, it, expect, beforeEach } from "vitest";
import { subtitleLines, renderSubtitleInto } from "./subtitle-render";

describe("subtitleLines", () => {
  it("translated mode shows only the translation", () => {
    expect(
      subtitleLines("translated", {
        translation: "Hey everyone",
        original: "Ciao",
        interim: false,
      }),
    ).toEqual([{ cls: "subtitle-translation", text: "Hey everyone" }]);
  });

  it("both mode shows translation (prominent) + source line on a final frame", () => {
    expect(
      subtitleLines("both", {
        translation: "Hey everyone",
        original: "Ciao a tutti",
        interim: false,
      }),
    ).toEqual([
      { cls: "subtitle-translation", text: "Hey everyone" },
      { cls: "subtitle-original", text: "Ciao a tutti" },
    ]);
  });

  it("original mode shows the source", () => {
    expect(
      subtitleLines("original", {
        translation: "Hey everyone",
        original: "Ciao",
        interim: false,
      }),
    ).toEqual([{ cls: "subtitle-translation", text: "Ciao" }]);
  });

  it("translated mode ignores an Enhanced interim that only carries the source", () => {
    // Cartesia interim = untranslated source; feeding it as `original` means 'translated'
    // mode shows nothing (no foreign source flashing) until the real translation arrives.
    expect(
      subtitleLines("translated", { original: "Ciao a", interim: true }),
    ).toEqual([]);
  });
});

describe("renderSubtitleInto (on-screen)", () => {
  let area: HTMLElement;
  beforeEach(() => {
    document.body.innerHTML = '<div class="subtitle-area"></div>';
    area = document.querySelector(".subtitle-area") as HTMLElement;
  });

  it("renders the Pro subtitle_final's translation into the DOM (both mode)", () => {
    // Exactly what the server end-to-end test delivered to a foreign listener:
    // original IT, translations.en = "Hey everyone, today we're trying the automatic subtitle translation".
    const rendered = renderSubtitleInto(area, "both", {
      translation:
        "Hey everyone, today we're trying the automatic subtitle translation",
      original:
        "Ciao a tutti, oggi proviamo la traduzione automatica dei sottotitoli",
      interim: false,
    });
    expect(rendered).toBe(true);
    const main = area.querySelector(".subtitle-translation");
    expect(main?.textContent).toBe(
      "Hey everyone, today we're trying the automatic subtitle translation",
    );
    expect(area.querySelector(".subtitle-original")?.textContent).toBe(
      "Ciao a tutti, oggi proviamo la traduzione automatica dei sottotitoli",
    );
  });

  it("renders the translation in translated mode and reports true", () => {
    expect(
      renderSubtitleInto(area, "translated", {
        translation: "Hey everyone",
        interim: false,
      }),
    ).toBe(true);
    expect(area.textContent).toBe("Hey everyone");
    expect(area.querySelector(".subtitle-translation")).not.toBeNull();
  });

  it("leaves the screen untouched when there is nothing to show (returns false)", () => {
    area.innerHTML = '<div class="subtitle">previous</div>';
    // Enhanced interim (source only) in translated mode → nothing new, keep previous.
    expect(
      renderSubtitleInto(area, "translated", {
        original: "Ciao",
        interim: true,
      }),
    ).toBe(false);
    expect(area.textContent).toBe("previous");
  });

  it("marks interim frames with the subtitle-interim class", () => {
    renderSubtitleInto(area, "both", { original: "Ciao", interim: true });
    expect(area.querySelector(".subtitle.subtitle-interim")).not.toBeNull();
  });
});

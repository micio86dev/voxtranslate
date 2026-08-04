// VoxTranslate call transcript (spec 0009).
//
// Injection safety (load-bearing): ALL user data arrives as ONE JSON string
// via `sys.inputs.data`. Strings decoded from JSON render literally in typst —
// room names, speaker names, and chat text can never inject markup or code.
#let data = json(bytes(sys.inputs.data))

// One color per participant (by join order); cycles past 4 speakers.
#let palette = (
  rgb("#6c5ce7"), // purple
  rgb("#00876c"), // teal
  rgb("#c0392b"), // red
  rgb("#b8860b"), // gold
)
#let speaker-color(i) = palette.at(calc.rem(i, palette.len()))

// `keywords` reaches the PDF metadata, which is what makes the AI Act Art. 50(2)
// marking machine-readable — the visible note further down is for the reader.
#set document(title: data.title + " - " + data.room, keywords: data.ai_keywords)
#set page(
  paper: "a4",
  margin: (x: 2cm, top: 2cm, bottom: 2.4cm),
  footer: context {
    set text(size: 8pt, fill: rgb("#888888"))
    data.footer
    h(1fr)
    counter(page).display("1 / 1", both: true)
  },
)
// Per-glyph fallback: Latin/Greek/Cyrillic from Noto Sans, CJK from SC/JP.
#set text(font: ("Noto Sans", "Noto Sans SC", "Noto Sans JP"), size: 10pt)

// ---- Header ----------------------------------------------------------------
#block(text(size: 18pt, weight: "bold", data.title))
#block(text(size: 12pt, fill: rgb("#555555"), data.room))
#v(0.5em)

#for m in data.meta {
  text(size: 9pt, weight: "bold", m.label + ": ")
  text(size: 9pt, m.value)
  linebreak()
}
#{
  text(size: 9pt, weight: "bold", data.participants_label + ": ")
  for (i, p) in data.participants.enumerate() {
    if i > 0 { text(size: 9pt, ", ") }
    text(size: 9pt, weight: "bold", fill: speaker-color(p.color), p.name)
    text(size: 9pt, fill: rgb("#777777"), " (" + p.lang + ")")
  }
}

#v(0.4em)
// AI Act Art. 50(2)/(5): visible, on the first page, above the content it qualifies.
#block(text(size: 8.5pt, style: "italic", fill: rgb("#777777"), data.ai_notice))
#v(0.4em)
#line(length: 100%, stroke: 0.5pt + rgb("#dddddd"))
#v(0.4em)

// ---- Chronological events --------------------------------------------------
#if data.events.len() == 0 {
  text(fill: rgb("#888888"), style: "italic", data.empty_label)
}
#for ev in data.events {
  if ev.at("marker", default: false) {
    // Bookmark marker row (spec 0013) — gold badge + owner + optional label.
    block(spacing: 1.1em, breakable: false, {
      text(size: 8.5pt, fill: rgb("#999999"), "[" + ev.time + "]")
      h(0.5em)
      box(
        fill: rgb("#fdf3d7"),
        inset: (x: 3pt, y: 1.5pt),
        radius: 2pt,
        text(size: 7pt, fill: rgb("#8a6d1a"), weight: "bold", data.bookmark_label),
      )
      h(0.4em)
      text(size: 9pt, weight: "bold", fill: rgb("#8a6d1a"), ev.by)
      if ev.label != none {
        linebreak()
        text(fill: rgb("#555555"), style: "italic", ev.label)
      }
    })
  } else {
    block(spacing: 1.1em, breakable: false, {
      text(size: 8.5pt, fill: rgb("#999999"), "[" + ev.time + "]")
      h(0.5em)
      text(weight: "bold", fill: speaker-color(ev.color), ev.speaker)
      if ev.chat {
        h(0.4em)
        box(
          fill: rgb("#eceff4"),
          inset: (x: 3pt, y: 1.5pt),
          radius: 2pt,
          text(size: 7pt, fill: rgb("#555555"), weight: "bold", "CHAT"),
        )
      }
      linebreak()
      text(fill: rgb("#555555"), ev.original)
      if ev.translation != none {
        linebreak()
        text(weight: "bold", ev.translation)
      }
    })
  }
}

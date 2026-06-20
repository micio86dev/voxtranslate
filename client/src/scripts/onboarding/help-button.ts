// Shared "?" help launcher. Both the home and call surfaces render a `.help-fab` button
// (static markup in index.astro with `data-i18n-title="onbHelpLabel"`); this just wires the
// click to the relevant guide. The button always works, regardless of the tour-seen flags.

export function wireHelpButton(id: string, launch: () => void): void {
  const btn = document.getElementById(id);
  if (!btn) return;
  btn.addEventListener('click', (e) => {
    e.preventDefault();
    launch();
  });
}

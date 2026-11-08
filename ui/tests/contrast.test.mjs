// WCAG 2.2 AA colour contrast (1.4.3 text 4.5:1, 1.4.11 non-text 3:1), measured from the design
// tokens in src/index.css for both themes. jsdom has no layout, so axe's colour-contrast rule is
// off in a11y.test.mjs; this covers every foreground/background pair the UI actually uses
// (grep `text-*` / `bg-*` in src/). Tints with alpha (`bg-warning/10`, `bg-primary/5`, …) are
// blended over the page background in sRGB, as the browser does.
// Run with SR_CONTRAST_TABLE=1 to print the measured table.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const css = readFileSync(new URL("../src/index.css", import.meta.url), "utf8");

function tokens(block) {
  const out = {};
  for (const m of block.matchAll(/--color-([a-z-]+):\s*oklch\(\s*([\d.]+)\s+([\d.]+)\s+([\d.]+)\s*\)/g)) {
    out[m[1]] = [Number(m[2]), Number(m[3]), Number(m[4])];
  }
  return out;
}
const lightBlock = css.match(/@theme\s*\{([\s\S]*?)\n\}/)?.[1] ?? "";
const darkBlock = css.match(/@media \(prefers-color-scheme: dark\)\s*\{\s*:root\s*\{([\s\S]*?)\}/)?.[1] ?? "";
const LIGHT = tokens(lightBlock);
const DARK = { ...LIGHT, ...tokens(darkBlock) };

/** OKLCH → gamma-encoded sRGB in [0, 1] (clamped). */
function oklchToSrgb([L, C, h]) {
  const a = C * Math.cos((h * Math.PI) / 180);
  const b = C * Math.sin((h * Math.PI) / 180);
  const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const m = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
  const lin = [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ];
  return lin.map((v) => {
    const c = Math.min(1, Math.max(0, v));
    return c <= 0.0031308 ? 12.92 * c : 1.055 * c ** (1 / 2.4) - 0.055;
  });
}
const toLinear = (c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
const luminance = ([r, g, b]) => 0.2126 * toLinear(r) + 0.7152 * toLinear(g) + 0.0722 * toLinear(b);
function ratio(fg, bg) {
  const [a, b] = [luminance(fg), luminance(bg)].sort((x, y) => y - x);
  return (a + 0.05) / (b + 0.05);
}

/** `name`, `name/NN` (alpha over the page background) or `white`. */
function colour(theme, spec) {
  if (spec === "white") return [1, 1, 1];
  const [name, alpha] = spec.split("/");
  const tok = theme[name];
  assert.ok(tok, `token --color-${name} exists`);
  const c = oklchToSrgb(tok);
  if (!alpha) return c;
  const bg = oklchToSrgb(theme.background);
  const t = Number(alpha) / 100;
  return c.map((v, i) => v * t + bg[i] * (1 - t));
}

// [foreground, background, minimum, where it's used]
const TEXT = 4.5;
const UI = 3;
const PAIRS = [
  ["foreground", "background", TEXT, "body text"],
  ["foreground", "muted", TEXT, "text on muted surfaces (code, chips)"],
  ["foreground", "accent", TEXT, "text on hovered / selected rows, active nav"],
  ["foreground", "warning/10", TEXT, "error panel text (ErrorPanel)"],
  ["foreground", "primary/5", TEXT, "approval card text"],
  ["foreground", "primary/15", TEXT, "search hit <mark>"],
  ["muted-foreground", "background", TEXT, "secondary text, placeholders"],
  ["muted-foreground", "muted", TEXT, "secondary text on muted surfaces"],
  ["muted-foreground", "accent", TEXT, "secondary text on hovered rows"],
  ["primary", "background", TEXT, "links"],
  ["primary-foreground", "primary", TEXT, "primary button"],
  ["white", "danger-fill", TEXT, "danger button"],
  ["primary", "background", UI, "focus ring"],
  ["warning-strong", "background", UI, "warning icons"],
  ["warning-strong", "warning/10", UI, "warning icon inside the error panel"],
  ["success", "background", UI, "success icons and the Up to date dot"],
  ["control-off", "background", UI, "control edges, switch off track"],
  ["muted-foreground", "background", UI, "Starting / No folders dot"],
];

export const measured = [];
for (const [themeName, theme] of [["light", LIGHT], ["dark", DARK]]) {
  test(`contrast (${themeName} theme): every token pair meets WCAG 2.2 AA`, () => {
    const failures = [];
    for (const [fg, bg, min, use] of PAIRS) {
      const r = ratio(colour(theme, fg), colour(theme, bg));
      measured.push({ theme: themeName, fg, bg, ratio: r.toFixed(2), min, use });
      if (r < min) failures.push(`${fg} on ${bg} (${use}): ${r.toFixed(2)}:1 < ${min}:1`);
    }
    assert.deepEqual(failures, []);
  });
}

test("contrast: the parser found both themes", () => {
  assert.ok(Object.keys(LIGHT).length >= 10, "light tokens parsed");
  assert.ok(Object.keys(tokens(darkBlock)).length >= 8, "dark overrides parsed");
  if (process.env.SR_CONTRAST_TABLE) {
    for (const m of measured) console.log(`${m.theme}\t${m.fg} on ${m.bg}\t${m.ratio}:1\t(min ${m.min})\t${m.use}`);
  }
});

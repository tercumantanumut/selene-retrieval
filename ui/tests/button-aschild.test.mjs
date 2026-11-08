// Regression (M4 owner bug, fixed by M5-A in button.tsx): `Button asChild` handed Radix Slot
// `[busy && <Loader2/>, children]`, Slot threw "Slot failed to slot onto its children" and
// TanStack showed "Something went wrong!" on Overview / Folders as soon as a folder had skipped
// files ("View skipped files" is a `Button asChild`).
//
// 1. The real Button, rendered on its own (server render of a tiny entry built by Vite with the
//    app's config and aliases) with asChild × busy × locked: never throws, always one element.
//    A control case proves the render catches Slot's error class.
// 2. Every `Button asChild` / `asChild` trigger in the real UI bundle: FolderCard "View skipped
//    files" (Overview + Folders, skipped files > 0), FolderDetail "Back to folders", Settings
//    "Manage connected apps", ScopeMenu trigger (Search): rendered with no runtime error.
import { after, afterEach, test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { build } from "vite";
import { appView, FOLDERS, uiError } from "./a11y/fixtures.mjs";
import { mount } from "./a11y/harness.mjs";

const uiRoot = fileURLToPath(new URL("..", import.meta.url));
// Under node_modules so the server bundle's bare imports (react, radix-ui) resolve; never linted.
const work = join(uiRoot, "node_modules", ".cache", `sr-button-aschild-${process.pid}`);
after(() => rmSync(work, { recursive: true, force: true }));

const ENTRY = `
import * as React from "react";
import { renderToString } from "react-dom/server";
import { Slot } from "radix-ui";
import { Button } from "@/components/ui/button";

export function renderAll() {
  const out = [];
  for (const busy of [false, true]) for (const locked of [false, true]) {
    const html = renderToString(
      <Button asChild busy={busy} locked={locked} size="sm" variant="link">
        <a href="#x">View skipped files</a>
      </Button>,
    );
    out.push({ busy, locked, html });
    const plain = renderToString(<Button busy={busy} locked={locked}>Save</Button>);
    out.push({ busy, locked, html: plain, plain: true });
  }
  return out;
}

/** The M4 shape: a leading \`false\` next to the child. Must throw (proves the check works). */
export function renderOldShape() {
  return renderToString(<Slot.Root>{[false, <a key="a" href="#x">x</a>]}</Slot.Root>);
}
`;

let mod;
async function entry() {
  if (mod) return mod;
  mkdirSync(work, { recursive: true });
  const src = join(work, "entry.tsx");
  writeFileSync(src, ENTRY);
  await build({
    root: uiRoot,
    configFile: join(uiRoot, "vite.config.ts"),
    logLevel: "error",
    build: { ssr: src, outDir: join(work, "out"), emptyOutDir: true, minify: false, rollupOptions: { output: { format: "esm", entryFileNames: "entry.mjs" } } },
  });
  mod = await import(pathToFileURL(join(work, "out", "entry.mjs")).href);
  return mod;
}

test("Button asChild × busy × locked renders exactly its one child, never throws", async () => {
  const m = await entry();
  const all = m.renderAll();
  assert.equal(all.length, 8);
  for (const r of all.filter((x) => !x.plain)) {
    assert.match(r.html, /^<a [^>]*href="#x"[^>]*>View skipped files<\/a>$/, JSON.stringify(r));
    if (r.locked) assert.match(r.html, /aria-disabled="true"/);
  }
  for (const r of all.filter((x) => x.plain)) {
    assert.match(r.html, /^<button /);
    assert.equal(/animate-spin/.test(r.html), r.busy, "busy spinner only on a plain button");
  }
});

test("control: Slot still throws for the M4 children shape", async () => {
  const m = await entry();
  assert.throws(() => m.renderOldShape(), /slot/i);
});

const open = [];
afterEach(() => {
  while (open.length) open.pop().close();
});
async function app(opts) {
  const a = await mount(opts);
  open.push(a);
  return a;
}
function noCrash(a, where) {
  const body = a.document.body.textContent;
  assert.ok(!body.includes("Something went wrong!"), `${where}: error boundary shown`);
  assert.ok(!/failed to slot/i.test(body + a.problems.join("\n")), `${where}: Slot error`);
  assert.deepEqual(a.problems, [], `${where}: runtime errors`);
}

test("Overview and Folders render a folder with skipped files ('View skipped files')", async () => {
  const skipped = FOLDERS.find((f) => f.state === "up_to_date_with_skips" && f.files_skipped > 0);
  assert.ok(skipped, "fixture has a folder with skipped files");
  const a = await app({ view: appView({ folders: FOLDERS }) });
  noCrash(a, "Overview");
  assert.ok(a.byText("View skipped files", "a").length >= 1, "Overview shows the link");
  // The card's link opens the folder's skipped list (Folder detail).
  await a.click(a.byText("View skipped files", "a")[0]);
  await a.settle(20);
  noCrash(a, "Folder detail (skipped)");
  // The Folders page (rows, no card) with the same folders.
  await a.nav("Folders");
  noCrash(a, "Folders");
  assert.ok(a.document.querySelectorAll("a[data-row]").length === FOLDERS.length, "Folders lists every folder");
});

test("Folder detail of a removed folder shows 'Back to folders'", async () => {
  const a = await app({
    view: appView({ folders: FOLDERS }),
    handlers: { get_folder_detail: { __error: uiError({ title: "Not found", details: "NotFound · trace 01JTRACE" }) } },
  });
  await a.nav("Folders");
  const row = [...a.document.querySelectorAll("a[data-row]")].find((r) => r.getAttribute("aria-label").startsWith("OneDrive"));
  await a.click(row);
  await a.settle(20);
  noCrash(a, "Folder detail not found");
  assert.ok(a.byText("Back to folders", "a").length === 1, "the asChild link is there");
});

test("Settings 'Manage connected apps' and the Search scope menu trigger", async () => {
  const a = await app({ view: appView({ folders: FOLDERS }) });
  await a.nav("Settings");
  noCrash(a, "Settings");
  assert.ok(a.byText("Manage connected apps", "a").length === 1);
  await a.nav("Search");
  noCrash(a, "Search");
  const trigger = a.document.querySelector('button[aria-label^="Search in:"]');
  assert.ok(trigger, "ScopeMenu trigger rendered");
  trigger.focus();
  await a.key(trigger, "Enter");
  noCrash(a, "Search scope menu open");
});

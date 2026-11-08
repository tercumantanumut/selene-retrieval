// Runs the REAL UI bundle (vite build of src/, as shipped) inside jsdom with a fake Tauri IPC, so
// the accessibility tests see the same DOM the app renders: real router, real Radix widgets,
// real pages. Nothing in src/ knows about the tests. The fake IPC answers commands from
// fixtures and can push the app's events (state-changed, service-push, …) like the Rust side.
//
// jsdom has no layout, so axe's colour-contrast rule is off here; contrast is measured from the
// design tokens (both themes) in tests/contrast.test.mjs.
import { mkdtempSync, readdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { JSDOM, VirtualConsole } from "jsdom";
import { build } from "vite";
import { appView, defaults } from "./fixtures.mjs";

const uiRoot = fileURLToPath(new URL("../..", import.meta.url));
const require = createRequire(import.meta.url);
const AXE_SRC = readFileSync(require.resolve("axe-core/axe.min.js"), "utf8");

let bundlePromise = null;

function walk(dir) {
  return readdirSync(dir).flatMap((n) => {
    const p = join(dir, n);
    return statSync(p).isDirectory() ? walk(p) : [p];
  });
}

/** One production build per test run, as a single classic script (jsdom can't run modules). */
export function getBundle() {
  bundlePromise ??= (async () => {
    const outDir = mkdtempSync(join(tmpdir(), "sr-ui-a11y-"));
    try {
      await build({
        root: uiRoot,
        configFile: join(uiRoot, "vite.config.ts"),
        logLevel: "error",
        build: {
          outDir,
          emptyOutDir: true,
          minify: false,
          cssCodeSplit: false,
          modulePreload: false,
          rollupOptions: {
            output: { format: "iife", entryFileNames: "app.js", assetFileNames: "[name][extname]" },
          },
        },
      });
      const files = walk(outDir);
      const js = files.filter((f) => f.endsWith(".js")).map((f) => readFileSync(f, "utf8")).join("\n");
      const css = files.filter((f) => f.endsWith(".css")).map((f) => readFileSync(f, "utf8")).join("\n");
      return { js, css };
    } finally {
      rmSync(outDir, { recursive: true, force: true });
    }
  })();
  return bundlePromise;
}

function polyfill(w) {
  w.matchMedia = (q) => ({
    matches: false,
    media: q,
    onchange: null,
    addEventListener() {},
    removeEventListener() {},
    addListener() {},
    removeListener() {},
    dispatchEvent: () => false,
  });
  w.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
  // axe probes <canvas> for icon-font glyphs; jsdom has no canvas ("Not implemented" error).
  // `null` is what a browser without a 2D context returns, and axe handles it.
  w.HTMLCanvasElement.prototype.getContext = () => null;
  w.HTMLElement.prototype.scrollIntoView = function () {};
  w.scrollTo = () => {};
  w.Element.prototype.hasPointerCapture = () => false;
  w.Element.prototype.setPointerCapture = () => {};
  w.Element.prototype.releasePointerCapture = () => {};
  if (!w.PointerEvent) {
    w.PointerEvent = class extends w.MouseEvent {
      constructor(type, init = {}) {
        super(type, init);
        this.pointerId = init.pointerId ?? 1;
        this.pointerType = init.pointerType ?? "mouse";
      }
    };
  }
  w.CSS ??= {};
  w.CSS.escape ??= (s) => String(s).replace(/[^a-zA-Z0-9_-]/g, (c) => `\\${c}`);
  Object.defineProperty(w.navigator, "clipboard", { value: { writeText: async () => {} }, configurable: true });
  if (!w.crypto?.randomUUID) {
    let n = 0;
    Object.defineProperty(w, "crypto", {
      value: { randomUUID: () => `00000000-0000-4000-8000-${String(++n).padStart(12, "0")}`, getRandomValues: (a) => a },
      configurable: true,
    });
  }
}

/**
 * Mounts the app. `handlers` override fixture answers (`cmd → value | (args) => value`; a
 * `{ __error: UiError }` value rejects like a Rust `Err`). Returns helpers to drive it.
 */
export async function mount({ view = appView(), handlers = {} } = {}) {
  const { js, css } = await getBundle();
  const problems = [];
  const vc = new VirtualConsole();
  vc.on("jsdomError", (e) => {
    // jsdom's CSS parser doesn't know Tailwind 4's newest syntax; that isn't an app error.
    if (!/Could not parse CSS stylesheet/.test(String(e?.message ?? e))) problems.push(String(e?.stack ?? e));
  });
  vc.on("error", (...a) => problems.push(a.map(String).join(" ")));
  const dom = new JSDOM(
    `<!doctype html><html lang="en"><head><title>Selene Retrieval</title><style>${css}</style></head><body><div id="root"></div></body></html>`,
    { runScripts: "outside-only", pretendToBeVisual: true, url: "http://tauri.localhost/", virtualConsole: vc },
  );
  const w = dom.window;
  polyfill(w);

  const calls = [];
  const answers = { ...defaults(view), ...handlers };
  const callbacks = new Map();
  const listeners = new Map();
  let nextCb = 1;
  w.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: "main" }, currentWebview: { label: "main", windowLabel: "main" } },
    transformCallback(cb, once) {
      const id = nextCb++;
      callbacks.set(id, (data) => {
        if (once) callbacks.delete(id);
        return cb?.(data);
      });
      return id;
    },
    unregisterCallback: (id) => callbacks.delete(id),
    convertFileSrc: (p) => p,
    async invoke(cmd, args) {
      calls.push({ cmd, args });
      if (cmd === "plugin:event|listen") {
        const list = listeners.get(args.event) ?? [];
        list.push(args.handler);
        listeners.set(args.event, list);
        return args.handler;
      }
      if (cmd.startsWith("plugin:event|")) return null;
      const a = answers[cmd];
      const v = typeof a === "function" ? a(args) : a;
      if (v && typeof v === "object" && "__error" in v) throw v.__error;
      return v === undefined ? null : structuredClone(v);
    },
  };
  w.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: (_e, id) => callbacks.delete(id) };

  w.eval(js);

  const settle = async (rounds = 12) => {
    for (let i = 0; i < rounds; i++) await new Promise((r) => w.setTimeout(r, 0));
  };
  await settle(20);

  const doc = w.document;
  const api = {
    window: w,
    document: doc,
    calls,
    problems,
    settle,
    answers,
    /** Push an app event (`state-changed`, `service-push`, …) as the Rust side would. */
    async emit(event, payload) {
      for (const id of listeners.get(event) ?? []) callbacks.get(id)?.({ event, id, payload });
      await settle();
    },
    /** Every element whose own trimmed text is `text` (buttons, links, headings…). */
    byText(text, selector = "button, a, [role=menuitem], [role=menuitemcheckbox], h1, h2, h3, label, summary") {
      return [...doc.querySelectorAll(selector)].filter((e) => e.textContent.trim() === text);
    },
    one(text, selector) {
      const all = api.byText(text, selector);
      if (all.length === 0) throw new Error(`no element with text "${text}"`);
      return all[0];
    },
    async click(el) {
      el.dispatchEvent(new w.PointerEvent("pointerdown", { bubbles: true, cancelable: true, button: 0, pointerType: "mouse" }));
      el.dispatchEvent(new w.MouseEvent("mousedown", { bubbles: true, cancelable: true, button: 0 }));
      el.dispatchEvent(new w.PointerEvent("pointerup", { bubbles: true, cancelable: true, button: 0, pointerType: "mouse" }));
      el.dispatchEvent(new w.MouseEvent("mouseup", { bubbles: true, cancelable: true, button: 0 }));
      el.click();
      await settle();
    },
    /** Types into an input/textarea the way React sees it (native value setter + input event). */
    async type(el, text) {
      const proto = el.tagName === "TEXTAREA" ? w.HTMLTextAreaElement.prototype : w.HTMLInputElement.prototype;
      Object.getOwnPropertyDescriptor(proto, "value").set.call(el, text);
      el.dispatchEvent(new w.Event("input", { bubbles: true }));
      await settle();
    },
    async key(el, key, init = {}) {
      el.dispatchEvent(new w.KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...init }));
      el.dispatchEvent(new w.KeyboardEvent("keyup", { key, bubbles: true, cancelable: true, ...init }));
      await settle();
    },
    /** A top nav link, or an item of the "More" menu (keyboard: Enter opens it). */
    async nav(label) {
      const link = [...doc.querySelectorAll('nav[aria-label="Main"] a')].find((a) => a.textContent.trim() === label);
      if (link) return api.click(link);
      const more = [...doc.querySelectorAll('nav[aria-label="Main"] button')].find((b) => b.textContent.trim() === "More");
      if (!more) throw new Error("no More menu");
      more.focus();
      await api.key(more, "Enter");
      const item = [...doc.querySelectorAll("[role=menuitem]")].find((m) => m.textContent.trim() === label);
      if (!item) throw new Error(`no menu item ${label}`);
      await api.click(item);
    },
    async openDisclosure(label = "Advanced") {
      const t = [...doc.querySelectorAll("h2 > button[aria-expanded]")].find((b) => b.textContent.trim() === label);
      if (!t) throw new Error(`no disclosure ${label}`);
      if (t.getAttribute("aria-expanded") !== "true") await api.click(t);
    },
    /** axe-core over the whole document (WCAG 2.0–2.2 A/AA + best practices). */
    async axe(context = doc) {
      if (!w.axe) w.eval(AXE_SRC);
      const r = await w.axe.run(context, {
        runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa", "best-practice"] },
        rules: { "color-contrast": { enabled: false } },
        resultTypes: ["violations"],
      });
      return r.violations;
    },
    close: () => w.close(),
  };
  return api;
}

/** One line per violating node, for readable assertion messages. */
export function describe(violations) {
  return violations
    .flatMap((v) => v.nodes.map((n) => `${v.id} (${v.impact}): ${n.target.join(" ")} — ${n.failureSummary?.split("\n")[1]?.trim() ?? v.help}`))
    .join("\n");
}

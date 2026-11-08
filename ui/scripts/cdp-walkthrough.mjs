// Dev-only: drives the REAL app window through the first-run wizard over WebView2's DevTools
// protocol and saves screenshots. Start sr-app with
//   WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222
// Nothing is faked: the real Windows folder picker is answered through UI Automation
// (answer-folder-dialog.ps1, types the path and presses "Select Folder"). Every IPC request the UI
// sends is counted (CDP Network) as evidence that status is pushed, not polled.
//
// Usage: node scripts/cdp-walkthrough.mjs wizard   <outDir> <folder>
//        node scripts/cdp-walkthrough.mjs reattach <outDir> <minReconnects>
import { writeFileSync, mkdirSync } from "node:fs";
import { join, dirname } from "node:path";
import { execFile } from "node:child_process";
import { fileURLToPath } from "node:url";

const [mode, outDir, arg] = process.argv.slice(2);
const port = process.env.CDP_PORT ?? "9222";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
mkdirSync(outDir, { recursive: true });

async function pageUrl() {
  for (let i = 0; i < 120; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
      const p = list.find((t) => t.type === "page");
      if (p) return p.webSocketDebuggerUrl;
    } catch {
      /* not up yet */
    }
    await sleep(500);
  }
  throw new Error("no DevTools page");
}

const ws = new WebSocket(await pageUrl());
await new Promise((r, j) => {
  ws.onopen = r;
  ws.onerror = j;
});
let nextId = 1;
const pending = new Map();
const ipcCounts = {};
ws.onmessage = (m) => {
  const msg = JSON.parse(m.data);
  if (msg.id && pending.has(msg.id)) {
    pending.get(msg.id)(msg);
    pending.delete(msg.id);
  } else if (msg.method === "Network.requestWillBeSent") {
    const u = msg.params.request.url;
    if (u.includes("ipc.localhost") && msg.params.request.method === "POST") {
      const cmd = decodeURIComponent(new URL(u).pathname.slice(1));
      ipcCounts[cmd] = (ipcCounts[cmd] ?? 0) + 1;
    }
  }
};
const send = (method, params = {}) =>
  new Promise((r) => {
    const id = nextId++;
    pending.set(id, r);
    ws.send(JSON.stringify({ id, method, params }));
  });
async function evaluate(expression) {
  const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (r.result?.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails));
  return r.result?.result?.value;
}
async function shot(name) {
  const r = await send("Page.captureScreenshot", { format: "png" });
  writeFileSync(join(outDir, `${name}.png`), Buffer.from(r.result.data, "base64"));
  console.log(`screenshot ${name}.png`);
}
async function waitText(text, timeoutMs = 60_000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    if (await evaluate(`document.body.innerText.includes(${JSON.stringify(text)})`)) return Date.now() - t0;
    await sleep(250);
  }
  throw new Error(`timeout waiting for "${text}"`);
}
async function click(text) {
  const ok = await evaluate(`(() => {
    const el = [...document.querySelectorAll("button, a")].find((e) => e.textContent.trim().includes(${JSON.stringify(text)}));
    if (!el) return false; el.click(); return true; })()`);
  if (!ok) throw new Error(`no button "${text}"`);
}


await send("Page.enable");
await send("Network.enable");
await send("Emulation.setDeviceMetricsOverride", { width: 920, height: 700, deviceScaleFactor: 1, mobile: false });

if (mode === "wizard") {
  const folder = arg;
  await waitText("Choose a folder to search");
  await shot("01-wizard-choose-folder");
  const helper = join(dirname(fileURLToPath(import.meta.url)), "answer-folder-dialog.ps1");
  const answered = new Promise((res, rej) =>
    execFile("powershell", ["-NoProfile", "-File", helper, "-Path", folder], (e, out) => (e ? rej(e) : res(out.trim()))),
  );
  await click("Choose a folder");
  console.log(await answered);
  const tSetup = await waitText("Setting up search", 30_000);
  await shot("02-wizard-indexing");
  const tDone = await waitText("Up to date ·", 600_000);
  await shot("03-wizard-indexed");
  console.log(`setup screen after ${tSetup} ms; folder up to date ${tDone} ms later`);
  await click("Next: use it in Claude");
  await waitText("Connect Claude Desktop");
  await shot("04-wizard-connect");
  await click("Connect Claude Desktop");
  await waitText("Claude Desktop is connected");
  await shot("05-wizard-connected");
  await click("Finish");
  await waitText("Your folders");
  await shot("06-overview");
  await click("Settings");
  await waitText("Search quality");
  await click("Advanced");
  await waitText("Index location");
  await shot("07-settings-advanced");
  await click("Overview");
  await waitText("Your folders");
  console.log(`IPC requests sent by the UI during the walk-through: ${JSON.stringify(ipcCounts)}`);
} else if (mode === "reattach") {
  const min = Number(arg ?? 1);
  const t0 = Date.now();
  for (;;) {
    const v = await evaluate(`window.__TAURI_INTERNALS__.invoke("get_state")`);
    if (v.reconnects >= min && v.overall !== "starting" && v.overall !== "problem") {
      console.log(`re-attached: reconnects=${v.reconnects} overall=${v.overall} after ${Date.now() - t0} ms`);
      break;
    }
    if (Date.now() - t0 > 60_000) throw new Error(`no re-attach: ${JSON.stringify(v)}`);
    await sleep(500);
  }
  await waitText("Your folders");
  await shot("08-after-service-restart");
}
ws.close();

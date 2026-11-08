// M5 accessibility pass (WCAG 2.2 AA, DS-6), automated part:
//  1. axe-core over every page and the key states (dialogs, locks, R34, errors), in the real UI
//     bundle under jsdom with a fake service (tests/a11y/harness.mjs);
//  2. behaviour checks axe can't see: one polite live region fed by state changes only, focus
//     after navigation / dialogs / inline confirms, switch names vs descriptions, policy locks,
//     the M4 tester's P2 items.
// Colour contrast is measured from the design tokens in contrast.test.mjs (both themes).
import { afterEach, test } from "node:test";
import assert from "node:assert/strict";
import { appView, folder, FOLDERS, ID, NO_PROVIDER_STATUS, uiError } from "./a11y/fixtures.mjs";
import { describe, mount } from "./a11y/harness.mjs";

// Each test gets its own window; close it right after the test so jsdom windows (each holds the
// whole bundle + CSS) don't pile up across the file.
const open = [];
afterEach(() => {
  while (open.length) open.pop().close();
});
async function app(opts) {
  const a = await mount(opts);
  open.push(a);
  return a;
}

async function noViolations(a, where) {
  const v = await a.axe();
  assert.equal(v.length, 0, `${where}:\n${describe(v)}`);
  assert.deepEqual(a.problems, [], `${where}: runtime errors`);
}

const MANAGED = [
  { path: "integrations.http_server_enabled", message: "Managed by your organisation", detail: null },
  { path: "integrations.mcp_add_folder_enabled", message: "Managed by your organisation", detail: null },
  { path: "integrations.mcp_sync_folder_enabled", message: "Managed by your organisation", detail: null },
  { path: "system.data_dir", message: "Managed by your organisation", detail: null },
  { path: "search.model_downloads", message: "Managed by your organisation", detail: null },
  { path: "provider.remote", message: "Managed by your organisation", detail: "Allowed: Ollama" },
  { path: "answers", message: "Managed by your organisation", detail: null },
];

// ------------------------------------------------------------------------------------- axe

test("axe: Overview with notices, a folder request and every folder state", async () => {
  const a = await app({
    view: appView({
      notices: [uiError({ title: "Some settings were reset", body: "A setting was invalid.", action: { kind: "open_settings" }, action_label: "Open settings", details: "SettingsInvalid · trace 1" })],
      approvals: [{ id: "01JAPPROVAL000000000000000", client: "Claude", path: "C:\\Users\\test\\Notes", expires_at_ms: Date.now() + 120_000 }],
    }),
  });
  await noViolations(a, "Overview");
});

test("axe: first-run wizard (no folders)", async () => {
  const a = await app({ view: appView({ overall: "no_folders", summary: "No folders yet", folders: [] }) });
  assert.ok(a.byText("Choose a folder to search", "h1").length === 1, "the step title is the page's h1");
  await noViolations(a, "Wizard");
});

test("axe: service problem panel", async () => {
  const a = await app({ view: appView({ overall: "problem", summary: "Background service didn't start", problem: uiError(), folders: [] }) });
  await noViolations(a, "Problem");
});

test("axe: Folders list (filter shown from 9 folders), Folder detail with Advanced open", async () => {
  const many = Array.from({ length: 9 }, (_, i) => folder({ id: `01JF${String(i).padStart(22, "0")}`, name: `Folder ${i + 1}` }));
  const a = await app({ view: appView({ folders: [...FOLDERS, ...many] }) });
  await a.nav("Folders");
  assert.ok(a.document.querySelector('input[aria-label="Filter folders"]'));
  await noViolations(a, "Folders");
  const row = [...a.document.querySelectorAll("a[data-row]")].find((r) => r.getAttribute("aria-label").startsWith("OneDrive"));
  await a.click(row);
  await a.openDisclosure("Advanced for this folder");
  await noViolations(a, "Folder detail");
});

test("axe: Search results and an answer with citations", async () => {
  const a = await app();
  await a.nav("Search");
  await a.type(a.document.querySelector('input[aria-label="Search your folders"]'), "lease");
  await a.click(a.one("Ask"));
  await a.settle(30);
  assert.ok(a.byText("Sources", "h3").length === 1, "the answer is shown");
  await noViolations(a, "Search + answer");
});

test("axe: Connected apps with Advanced open, and the Add a tool dialog", async () => {
  const a = await app();
  await a.nav("Connected apps");
  await a.openDisclosure();
  await noViolations(a, "Connected apps");
  await a.click(a.one("Add a tool…"));
  assert.ok(a.document.querySelector('[role="dialog"]'));
  await noViolations(a, "Add a tool dialog");
});

test("axe: Settings with Advanced open, licences dialog, Move dialog", async () => {
  const a = await app();
  await a.nav("Settings");
  await a.openDisclosure();
  await noViolations(a, "Settings");
  await a.click(a.one("View licences"));
  await noViolations(a, "Licences dialog");
  await a.key(a.document.activeElement, "Escape");
  await a.click(a.one("Move…"));
  assert.ok(a.byText("Move the search index", "h2").length === 1);
  await noViolations(a, "Move dialog");
});

test("axe: Providers & Keys, Activity, Prompt Enhancer, Folder groups", async () => {
  const a = await app();
  await a.nav("Providers & Keys");
  await noViolations(a, "Providers");
  await a.nav("Activity");
  await noViolations(a, "Activity");
  await a.nav("Prompt Enhancer");
  await noViolations(a, "Prompt Enhancer");
  await a.nav("Settings");
  await a.openDisclosure();
  await a.click(a.one("Folder groups", "a"));
  await noViolations(a, "Folder groups");
});

test("axe: consent dialog (Claude asked for an answer)", async () => {
  const a = await app({
    view: appView({ consent_request: { provider: "openai", provider_name: "OpenAI", purpose: "answer" } }),
  });
  const d = a.document.querySelector('[role="alertdialog"]');
  assert.ok(d, "consent dialog open");
  assert.equal(a.document.activeElement?.textContent.trim(), "Don't allow", "focus on the least destructive choice");
  await noViolations(a, "Consent dialog");
});

test("axe: Settings and Connected apps fully managed by policy", async () => {
  const a = await app({ view: appView({ policy: { managed: MANAGED } }) });
  await a.nav("Settings");
  await a.openDisclosure();
  await noViolations(a, "Settings (managed)");
  await a.nav("Connected apps");
  await a.openDisclosure();
  await noViolations(a, "Connected apps (managed)");
});

test("axe: R34 search index not found", async () => {
  const a = await app({ view: missingView({ old_copy: "C:\\Users\\test\\AppData\\Local\\SeleneRetrieval" }) });
  await noViolations(a, "R34");
});

// ------------------------------------------------------------------------- live region (DS-6)

test("live region: one polite region; announces state changes, never progress ticks", async () => {
  const a = await app();
  const regions = [...a.document.querySelectorAll('[aria-live="polite"][data-testid="live-region"]')];
  assert.equal(regions.length, 1);
  const region = regions[0];
  const tick = appView({
    folders: FOLDERS.map((f) => (f.id === ID.code ? { ...f, files_done: 900, line: "Indexing 900 of 2,100 files · about 2 minutes left" } : f)),
  });
  await a.emit("state-changed", tick);
  assert.equal(region.textContent, "", "a progress tick is not announced");
  const done = appView({
    folders: FOLDERS.map((f) => (f.id === ID.code ? { ...f, state: "up_to_date", line: "Up to date · 2,100 files" } : f)),
  });
  await a.emit("state-changed", done);
  assert.equal(region.textContent, "“Projects”: Up to date · 2,100 files");
  // Pushing the same view again says nothing new.
  await a.emit("state-changed", { ...done });
  assert.equal(region.textContent, "“Projects”: Up to date · 2,100 files");
});

test("live region: the index move is announced once per 25 % step", async () => {
  const a = await app();
  const region = a.document.querySelector('[data-testid="live-region"]');
  const move = (p, line) => appView({ index_move: { can_cancel: true, percent: p, line, from: "C:\\a", to: "D:\\b" } });
  await a.emit("state-changed", move(1, "Moving… 0.0 of 2.4 GB"));
  assert.equal(region.textContent, "Moving… 0.0 of 2.4 GB");
  await a.emit("state-changed", move(10, "Moving… 0.2 of 2.4 GB"));
  assert.equal(region.textContent, "Moving… 0.0 of 2.4 GB", "no announcement inside a step");
  await a.emit("state-changed", move(26, "Moving… 0.6 of 2.4 GB"));
  assert.equal(region.textContent, "Moving… 0.6 of 2.4 GB");
});

test("answers and improved prompts are never read out whole by a live region", async () => {
  const a = await app();
  await a.nav("Search");
  await a.type(a.document.querySelector('input[aria-label="Search your folders"]'), "lease");
  await a.click(a.one("Ask"));
  await a.settle(30);
  const live = [...a.document.querySelectorAll("[aria-live], [role=status], [role=alert]")].map((e) => e.textContent);
  assert.ok(live.some((t) => t.includes("Answer ready")), "the outcome is announced");
  assert.ok(!live.some((t) => t.includes("The lease starts")), "the answer text is not in a live region");
});

test("target size (2.5.8): citation chips are at least 24 × 24 px", async () => {
  const a = await app();
  await a.nav("Search");
  await a.type(a.document.querySelector('input[aria-label="Search your folders"]'), "lease");
  await a.click(a.one("Ask"));
  await a.settle(30);
  const chips = [...a.document.querySelectorAll('button[aria-label^="Source "]')];
  assert.ok(chips.length >= 1, "the answer has chips");
  for (const c of chips) {
    const cls = c.className.split(/\s+/);
    assert.ok(cls.includes("h-6") && cls.includes("min-w-6"), `chip ${c.getAttribute("aria-label")} is 24 px (h-6 min-w-6)`);
  }
});

// ------------------------------------------------------------------------------ focus (2.4.3)

test("focus: a route change moves focus to the new page's h1", async () => {
  const a = await app();
  await a.nav("Folders");
  assert.equal(a.document.activeElement?.tagName, "H1");
  assert.equal(a.document.activeElement?.textContent.trim(), "Folders");
});

test("focus: Ctrl+K lands in the search box (the page doesn't steal it back)", async () => {
  const a = await app();
  await a.key(a.document.body, "k", { ctrlKey: true });
  await a.settle(20);
  assert.equal(a.document.activeElement?.getAttribute("aria-label"), "Search your folders");
});

test("focus: a dialog takes focus inside and gives it back to its trigger on Esc", async () => {
  const a = await app();
  await a.nav("Settings");
  await a.openDisclosure();
  const trigger = a.one("Move…");
  trigger.focus();
  await a.click(trigger);
  const dialog = a.document.querySelector('[role="dialog"]');
  assert.ok(dialog?.contains(a.document.activeElement), "focus is inside the dialog");
  await a.key(a.document.activeElement, "Escape");
  assert.ok(a.document.querySelector('[role="dialog"]') === null, "Esc closes it");
  assert.ok(a.document.activeElement === trigger, "focus returns to [Move…]");
});

test("focus: wizard steps move focus to the new step's title", async () => {
  const a = await app({
    view: appView({ overall: "no_folders", summary: "No folders yet", folders: [] }),
    handlers: {
      "plugin:dialog|open": "C:\\Users\\test\\Documents",
      estimate_folder: { files: 10, capped: false, eta_s: 30, text: "About 10 files. This first pass should take a minute or two." },
      add_folder: ID.docs,
    },
  });
  await a.click(a.one("Choose a folder…"));
  await a.settle(20);
  await a.emit("state-changed", appView({ folders: [FOLDERS[0]] }));
  const next = a.one("Next");
  assert.ok(a.document.activeElement === next, "step 2 focuses [Next] (page 06)");
  await a.click(next);
  assert.equal(a.document.activeElement?.textContent.trim(), "Get written answers (optional)");
});

// ------------------------------------------------------------------- names and descriptions

test("every switch is named by its title only; the help line is its description", async () => {
  const a = await app();
  await a.nav("Settings");
  await a.openDisclosure();
  const switches = [...a.document.querySelectorAll('[role="switch"]')];
  assert.ok(switches.length >= 4);
  for (const s of switches) {
    const label = a.document.querySelector(`label[for="${s.id}"]`);
    assert.ok(label, `switch ${s.id} has a <label for>`);
    const desc = (s.getAttribute("aria-describedby") ?? "").split(" ").filter(Boolean);
    for (const id of desc) {
      const d = a.document.getElementById(id);
      assert.ok(d, `description ${id} exists`);
      assert.ok(!label.contains(d), "the help line isn't part of the name");
    }
  }
});

// ------------------------------------------------------------------------- policy (page 33)

test("policy: locked controls stay focusable, say why, and do nothing", async () => {
  const a = await app({ view: appView({ policy: { managed: MANAGED } }) });
  await a.nav("Settings");
  assert.ok(a.document.body.textContent.includes("Some settings are managed by your organisation."));
  await a.openDisclosure();
  // The folder-requests and folder-checks switches: aria-disabled (not disabled), described by
  // the managed line.
  for (const text of ["Let connected apps ask to add folders", "Let connected apps check folders for changes"]) {
    const label = a.one(text, "label");
    const sw = a.document.getElementById(label.getAttribute("for"));
    assert.equal(sw.getAttribute("aria-disabled"), "true", text);
    assert.equal(sw.hasAttribute("disabled"), false, `${text}: focusable`);
    const desc = sw.getAttribute("aria-describedby").split(" ").map((id) => a.document.getElementById(id)?.textContent).join(" ");
    assert.match(desc, /Managed by your organisation/);
    const before = a.calls.length;
    await a.click(sw);
    assert.ok(!a.calls.slice(before).some((c) => c.cmd === "set_advanced_settings"), `${text}: a locked switch sends nothing`);
  }
  // Move…: locked, no dialog.
  const move = a.one("Move…");
  assert.equal(move.getAttribute("aria-disabled"), "true");
  await a.click(move);
  assert.ok(a.document.querySelector('[role="dialog"]') === null, "a locked Move… opens nothing");
  // Model downloads managed: the Basic offer is hidden; Advanced shows the choice locked.
  assert.ok(!a.byText("Use it").length, "no download offer in Basic");
  assert.equal(a.one("Better search").getAttribute("aria-disabled"), "true");
  // A whole section managed: one line under its title, both switches locked.
  const answers = a.one("Answers", "h2").closest("[data-surface]");
  assert.equal(answers.querySelectorAll('[role="switch"][aria-disabled="true"]').length, 2);
});

test("policy: cloud providers locked leaves local providers selectable, with the reason", async () => {
  const a = await app({
    view: appView({ policy: { managed: MANAGED } }),
    handlers: { get_provider_status: NO_PROVIDER_STATUS },
  });
  await a.nav("Providers & Keys");
  assert.match(a.document.querySelector("main").textContent, /Managed by your organisation\s*Allowed: Ollama/);
  await a.click(a.one("Add a provider"));
  assert.ok(a.document.querySelector('[role="dialog"]'), "the add dialog still opens (no dead end)");
});

// -------------------------------------------------------------------------------- R34 screen

function missingView(over = {}) {
  return appView({
    overall: "problem",
    summary: "Search index not found",
    folders: [],
    index_missing: {
      path: "D:\\SeleneRetrieval Index",
      drive: "D:",
      old_copy: null,
      folders_to_restore: 3,
      error: uiError({
        title: "Search index not found",
        body: "Your search index is in \"D:\\SeleneRetrieval Index\", which can't be reached. Reconnect the drive, or move the index back.",
        action: { kind: "none" },
        action_label: "",
        details: "IndexMissing · trace 1",
      }),
      ...over,
    },
  });
}

test("R34: the screen takes focus, offers the right choices and confirms Start fresh", async () => {
  const a = await app({ view: missingView() });
  assert.equal(a.document.activeElement?.textContent.trim(), "Search index not found");
  assert.ok(a.byText("Reconnect the drive", "h2").length === 1);
  assert.equal(a.byText("Move the index back", "button").length, 0, "no earlier copy → only Start fresh");
  assert.ok(a.document.body.textContent.includes("read your 3 folders again"));
  await a.click(a.one("Start fresh…"));
  assert.equal(a.document.activeElement?.textContent.trim(), "Keep waiting", "focus on the safe choice");
  await a.click(a.one("Start fresh", "button"));
  assert.ok(a.calls.some((c) => c.cmd === "recover_index" && c.args.choice === "start_fresh"));
});

test("R34: with an earlier copy, [Move the index back] sends move_back", async () => {
  const a = await app({ view: missingView({ old_copy: "C:\\Users\\test\\AppData\\Local\\SeleneRetrieval" }) });
  await a.click(a.one("Move the index back", "button"));
  assert.ok(a.calls.some((c) => c.cmd === "recover_index" && c.args.choice === "move_back"));
});

// ---------------------------------------------------------------- M4 tester P2 low items

test("P2: the 'Deeper answers is on/off' link lands on the Answers card", async () => {
  const a = await app();
  await a.nav("Search");
  await a.type(a.document.querySelector('input[aria-label="Search your folders"]'), "lease");
  await a.click(a.one("Ask"));
  await a.settle(30);
  const link = a.one("Deeper answers is off", "a");
  assert.match(link.getAttribute("href"), /section=answers/);
  await a.click(link);
  await a.settle(20);
  assert.equal(a.document.activeElement?.textContent.trim(), "Answers");
});

test("P2: [Ask] says where its switch is; the switch help mentions Ask", async () => {
  const a = await app({ handlers: { get_answers_settings: { answers_enabled: false, deeper_answers: false, max_rounds: 2 } } });
  await a.nav("Search");
  assert.equal(a.byText("Ask").length, 0);
  assert.ok(a.byText("Turn on answers in Settings", "a").length === 1);
  await a.nav("Settings");
  assert.ok(a.document.body.textContent.includes("Also turns on Ask on the Search page."));
});

test("P2: size limit text uses KB/MB only; a re-read confirm is drawn once", async () => {
  const a = await app();
  await a.nav("Folders");
  await a.click([...a.document.querySelectorAll("a[data-row]")].find((r) => r.getAttribute("aria-label").startsWith("OneDrive")));
  await a.openDisclosure("Advanced for this folder");
  const text = a.document.querySelector("main").textContent;
  assert.ok(!/KiB|MiB/.test(text), "no KiB/MiB");
  const label = a.one("Include subfolders", "label");
  await a.click(a.document.getElementById(label.getAttribute("for")));
  await a.settle(20);
  const n = a.document.querySelector("main").textContent.split("This re-reads the folder.").length - 1;
  assert.equal(n, 1, "exactly one confirm");
  assert.equal(a.document.activeElement?.textContent.trim(), "Cancel", "focus on the safe choice");
});

test("P2: a failed restart toast offers one action", async () => {
  const a = await app({
    handlers: { restart_service: { __error: uiError({ action: { kind: "restart_app" }, action_label: "Restart Selene Retrieval" }) } },
  });
  await a.nav("Settings");
  await a.openDisclosure();
  await a.click(a.one("Restart now"));
  await a.settle(20);
  const toast = a.document.querySelector("[data-sonner-toast]");
  assert.ok(toast, "an error toast");
  assert.ok([...toast.querySelectorAll("button")].some((b) => b.textContent.trim() === "Restart Selene Retrieval"));
});

test("P2: the Move failure panel offers [Retry] and keeps the chooser", async () => {
  const a = await app({
    handlers: {
      "plugin:dialog|open": "D:\\",
      check_index_target: { ok: true, index_path: "D:\\SeleneRetrieval Index", free: "200 GB", problem: null },
    },
  });
  await a.nav("Settings");
  await a.openDisclosure();
  await a.click(a.one("Move…"));
  await a.click(a.one("Choose a new location…"));
  await a.settle(20);
  await a.click(a.one("Move"));
  await a.emit("service-push", {
    kind: "index_move_failed",
    error: uiError({ title: "Not enough disk space", body: "The drive is full.", action: { kind: "free_disk_space" }, action_label: "Free up space", details: "DiskFull · trace 1" }),
    kept_path: "C:\\Users\\test\\AppData\\Local\\SeleneRetrieval",
  });
  const dialog = a.document.querySelector('[role="dialog"]');
  assert.ok(dialog.textContent.includes("Nothing was moved."));
  assert.ok([...dialog.querySelectorAll("button")].some((b) => b.textContent.trim() === "Retry"));
  assert.ok([...dialog.querySelectorAll("button")].some((b) => b.textContent.trim() === "Choose a new location…"));
});

test("P2: a stale connected app can be removed (clients.forget); page 29 says 'password'", async () => {
  const a = await app();
  await a.nav("Connected apps");
  await a.openDisclosure();
  const main = a.document.querySelector("main");
  assert.ok(main.textContent.includes("127.0.0.1 only, password required"));
  assert.ok(!/token required/.test(main.textContent));
  const removes = a.byText("Remove…");
  assert.equal(removes.length, 1, "only the app without an open session (Cursor) can be removed");
  await a.click(removes[0]);
  assert.equal(a.document.activeElement?.textContent.trim(), "Keep");
  await a.click(a.one("Remove"));
  assert.ok(a.calls.some((c) => c.cmd === "forget_client" && c.args.clientId === "cursor"));
});

test("OneDrive: note on the card, badge on the row, [Include them] on the detail", async () => {
  const a = await app();
  assert.ok(a.document.body.textContent.includes("Some files are stored only in OneDrive and were skipped."));
  await a.nav("Folders");
  assert.ok(a.byText("OneDrive files skipped", "span").length >= 1);
  await a.click([...a.document.querySelectorAll("a[data-row]")].find((r) => r.getAttribute("aria-label").startsWith("OneDrive")));
  await a.click(a.one("Include them"));
  const save = a.calls.find((c) => c.cmd === "update_folder_settings");
  assert.equal(save?.args.settings.include_online_only, true);
});

test("Amendment 17: a folder that only listed its files asks once: search its contents, or remove it", async () => {
  const scans = folder({
    name: "Scans",
    path: "C:\\Users\\test\\Scans",
    state: "paused",
    line: "Paused until you choose what to do",
    files_done: 0,
    files_total: 3,
    was_files_only: true,
  });
  const a = await app({ view: appView({ folders: [FOLDERS[1], scans] }) });
  // Overview: listed first, with the plain question and its two answers on the card.
  const card = a.document.querySelector('ul[aria-label="Folders"] li [role="group"]');
  assert.equal(card.getAttribute("aria-label"), "Scans, Paused until you choose what to do");
  assert.ok(card.textContent.includes("This folder was set to only list its files, which is no longer possible."));
  assert.equal(a.byText("Resume").length, 0, "[Resume] reads [Search its contents] here");
  assert.equal(a.byText("Remove…").length, 2, "every card keeps [Remove…]");
  await noViolations(a, "Overview with a former Files-only folder");
  await a.click(a.one("Search its contents"));
  const resume = a.calls.find((c) => c.cmd === "set_folder_paused")?.args;
  assert.equal(resume?.folderId, ID.docs);
  assert.equal(resume?.paused, false);
  // The drive is unplugged: no question while [Search its contents] can't be offered.
  const away = { ...scans, state: "unavailable", line: "Can't reach this folder. The drive may be unplugged or the folder moved." };
  const NOTE = "which is no longer possible";
  await a.emit("state-changed", appView({ folders: [FOLDERS[1], away] }));
  assert.ok(!a.document.body.textContent.includes(NOTE), "no question on the unavailable card");
  assert.equal(a.byText("Search its contents").length, 0);
  await a.emit("state-changed", appView({ folders: [FOLDERS[1], scans] }));
  assert.ok(a.document.body.textContent.includes(NOTE), "back when the drive is back");

  // Folder detail: the same question; the mode control is gone; [Remove folder…] is the usual flow.
  await a.nav("Folders");
  await a.click([...a.document.querySelectorAll("a[data-row]")].find((r) => r.getAttribute("aria-label").startsWith("Scans")));
  assert.ok(a.document.querySelector("main").textContent.includes("Search its contents, or remove it from Selene Retrieval."));
  await a.emit("state-changed", appView({ folders: [FOLDERS[1], away] }));
  assert.ok(!a.document.querySelector("main").textContent.includes(NOTE), "not on the detail either");
  await a.emit("state-changed", appView({ folders: [FOLDERS[1], scans] }));
  await a.openDisclosure("Advanced for this folder");
  const main = a.document.querySelector("main").textContent;
  assert.ok(!main.includes("Only list the files") && !main.includes("only list the files"), "no mode choice");
  assert.equal(a.document.querySelectorAll('[role="radiogroup"]').length, 0);
  await noViolations(a, "Folder detail of a former Files-only folder");
  await a.click(a.one("Remove folder…"));
  await a.click(a.one("Remove"));
  assert.equal(a.calls.find((c) => c.cmd === "remove_folder")?.args.folderId, ID.docs);
});

test("#40 ConnectionNotTrusted: plain text, no [Retry] (even from a raw RPC error)", async () => {
  const a = await app({ handlers: { restart_service: { __error: { code: "ConnectionNotTrusted", ctx: {} } } } });
  await a.nav("Settings");
  await a.openDisclosure();
  await a.click(a.one("Restart now"));
  await a.settle(20);
  const toast = a.document.querySelector("[data-sonner-toast]");
  assert.ok(toast, "an error toast");
  assert.ok(toast.textContent.includes("Connection not safe"));
  assert.ok(toast.textContent.includes("Restart Windows to fix this."));
  assert.ok(![...toast.querySelectorAll("button")].some((b) => /Retry|Restart Selene/.test(b.textContent)), "no retry button");
});

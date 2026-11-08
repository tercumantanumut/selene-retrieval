// Amendment 13 · "Create a folder for me" (docs/07-default-folder-plan.md) in the real UI bundle:
// the wizard's first step offers two choices, one click makes and adds the folder (the page never
// sends a path and never opens the picker for it), step 2 says it's ready with [Open folder], and
// Finish lands on the folder's details. Failures fall back to choosing a folder. The Folders page
// offers it in its empty state and next to [Add a folder] only while the folder isn't added.
import { afterEach, test } from "node:test";
import assert from "node:assert/strict";
import { appView, folder, FOLDERS, folderDetail, NO_PROVIDER_STATUS, uiError } from "./a11y/fixtures.mjs";
import { describe, mount } from "./a11y/harness.mjs";

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

const NEW_ID = "01JSELENEFOLDER00000000000";
const PATH = "C:\\Users\\test\\Documents\\Selene Retrieval";
const OFFER = { available: true, folder_id: null, name: "Selene Retrieval" };
const ADDED = { available: false, folder_id: NEW_ID, name: "Selene Retrieval" };
const RESULT = {
  folder_id: NEW_ID,
  name: "Selene Retrieval",
  path: PATH,
  created: true,
  already_added: false,
  opened: true,
  in_onedrive: false,
};
const NEW_FOLDER = folder({ id: NEW_ID, name: "Selene Retrieval", path: PATH, line: "Up to date · 0 files", files_done: 0, files_total: 0 });
const FIRST_RUN = appView({ overall: "no_folders", summary: "No folders yet", folders: [] });

const called = (a, cmd) => a.calls.filter((c) => c.cmd === cmd);

test("wizard step 1 offers both choices, named and described", async () => {
  const a = await app({ view: FIRST_RUN, handlers: { get_default_folder: OFFER } });
  assert.ok(a.byText("Choose a folder to search", "h1").length === 1);
  const create = a.one("Create a folder for me");
  const choose = a.one("Choose an existing folder…");
  assert.ok(create && choose);
  const hint = a.document.getElementById(create.getAttribute("aria-describedby"));
  assert.ok(hint?.textContent.includes("“Selene Retrieval” folder in Documents"), "the button says where");
  assert.ok(hint.textContent.includes("becomes searchable"));
  // Keyboard: both are real buttons in tab order, the recommended one first.
  const buttons = [...a.document.querySelectorAll("main button")];
  assert.ok(buttons.indexOf(create) < buttons.indexOf(choose));
  assert.equal(create.tabIndex, 0);
  await noViolations(a, "Wizard with the offer");
});

test("one click: created, ready with [Open folder], Finish opens its details", async () => {
  let added = false;
  const a = await app({
    view: FIRST_RUN,
    handlers: {
      get_default_folder: () => (added ? ADDED : OFFER),
      create_default_folder: () => {
        added = true;
        return RESULT;
      },
      get_provider_status: NO_PROVIDER_STATUS,
      get_folder_detail: () => ({ ...folderDetail(), id: NEW_ID, name: "Selene Retrieval", path: PATH, skipped: [], skipped_total: 0, watch_note: null, cloud_note: null, online_only_skipped: 0 }),
    },
  });
  await a.click(a.one("Create a folder for me"));
  await a.settle(20);
  assert.equal(called(a, "create_default_folder").length, 1, "one click, one call");
  assert.equal(Object.keys(called(a, "create_default_folder")[0].args ?? {}).length, 0, "the page sends no path");
  assert.equal(called(a, "add_folder").length, 0);
  assert.equal(called(a, "plugin:dialog|open").length, 0, "no picker");
  await a.emit("state-changed", appView({ folders: [NEW_FOLDER] }));
  assert.ok(a.byText("Your folder is ready", "h1").length === 1);
  const body = a.document.querySelector("main").textContent;
  assert.ok(body.includes("Anything you put in “Selene Retrieval” becomes searchable automatically."));
  assert.ok(body.includes("We opened it in File Explorer for you."));
  assert.ok(body.includes("Step 2 of 4"));
  const next = a.one("Next");
  assert.ok(a.document.activeElement === next, "focus on [Next]");
  await noViolations(a, "Ready step");

  await a.click(a.one("Open folder"));
  assert.deepEqual(
    called(a, "open_folder").map((c) => JSON.stringify(c.args)),
    [JSON.stringify({ folderId: NEW_ID })],
    "opened by id, never by path",
  );

  await a.click(a.one("Next"));
  await a.click(a.one("Skip for now"));
  await a.click(a.one("Finish"));
  await a.settle(20);
  assert.ok(a.byText("Selene Retrieval", "h1").length === 1, "the folder's details");
  assert.ok(
    a.document.querySelector("main").textContent.includes("Anything you put here becomes searchable automatically."),
    "the details say it's ready",
  );
  assert.ok(a.byText("Open folder").length === 1);
  await noViolations(a, "Folder details");
});

test("OneDrive: the ready step says online-only files aren't searched", async () => {
  const a = await app({
    view: FIRST_RUN,
    handlers: { get_default_folder: OFFER, create_default_folder: { ...RESULT, opened: false, in_onedrive: true } },
  });
  await a.click(a.one("Create a folder for me"));
  await a.settle(20);
  const body = a.document.querySelector("main").textContent;
  assert.ok(body.includes("This folder is in OneDrive."));
  assert.ok(!body.includes("We opened it"), "only when Explorer was opened");
});

test("a failure explains itself and falls back to choosing a folder", async () => {
  const a = await app({
    view: FIRST_RUN,
    handlers: {
      get_default_folder: OFFER,
      create_default_folder: {
        __error: uiError({
          title: "Couldn't create the folder",
          body: "Windows didn't let Selene Retrieval create a folder in Documents.",
          action: { kind: "choose_folder" },
          action_label: "Choose a folder",
          details: "DefaultFolder · AccessDenied",
        }),
      },
    },
  });
  await a.click(a.one("Create a folder for me"));
  await a.settle(20);
  const alert = a.document.querySelector('[role="alert"]');
  assert.ok(alert?.textContent.includes("Couldn't create the folder"));
  assert.ok(a.byText("Choose an existing folder…").length === 1, "the other choice stays");
  await noViolations(a, "Wizard error");
  await a.click(a.one("Choose a folder"));
  await a.settle(20);
  assert.equal(called(a, "plugin:dialog|open").length, 1, "the picker opens");
});

test("policy or no Documents: the offer is hidden and step 1 is as before", async () => {
  const a = await app({ view: FIRST_RUN, handlers: { get_default_folder: { ...OFFER, available: false } } });
  assert.equal(a.byText("Create a folder for me").length, 0);
  assert.ok(a.byText("Choose a folder…").length === 1);
});

test("Folders empty state offers both; success opens the new folder", async () => {
  let added = false;
  const a = await app({
    view: appView({ overall: "synced", summary: "Up to date", folders: [] }),
    handlers: {
      get_default_folder: () => (added ? ADDED : OFFER),
      create_default_folder: () => {
        added = true;
        return RESULT;
      },
    },
  });
  await a.nav("Folders");
  assert.ok(a.byText("No folders yet", "h2").length === 1);
  assert.ok(a.byText("Choose an existing folder…").length === 1);
  await noViolations(a, "Folders empty with the offer");
  await a.click(a.one("Create a folder for me"));
  await a.emit("state-changed", appView({ folders: [NEW_FOLDER] }));
  await a.settle(20);
  assert.ok(a.byText("Selene Retrieval", "h1").length === 1, "on its details page");
  assert.equal(called(a, "add_folder").length, 0);
});

test("Folders header: [Create a folder for me] only while it isn't added", async () => {
  const a = await app({ handlers: { get_default_folder: OFFER } });
  await a.nav("Folders");
  assert.ok(a.byText("Create a folder for me").length === 1);
  assert.ok(a.byText("Add a folder").length === 1);
  await noViolations(a, "Folders header with the offer");

  const b = await app({ view: appView({ folders: [...FOLDERS, NEW_FOLDER] }), handlers: { get_default_folder: ADDED } });
  await b.nav("Folders");
  assert.equal(b.byText("Create a folder for me").length, 0, "hidden once added");
});

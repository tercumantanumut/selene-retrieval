// "Claude (sign in)" / "Codex (sign in)" (M5-C, Amendments 10.2 / 10.3) in the real UI bundle:
// the wizard step "Get written answers (optional)" (page 20) and Providers & Keys (page 21). If
// Claude Code (or Codex) is signed in on this PC, Selene Retrieval uses that sign-in: no paste
// box, no code field, no "authorization code", no token. One state, one action each. The
// provider list label stays "Claude (sign in)" (never the product name as the label).
import { afterEach, test } from "node:test";
import assert from "node:assert/strict";
import {
  appView,
  CLAUDE_SIGNED_IN,
  CLAUDE_SIGNED_OUT,
  CODEX_NOT_INSTALLED,
  FOLDERS,
  ID,
  NO_PROVIDER_STATUS,
  PROVIDER,
} from "./a11y/fixtures.mjs";
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
/** Nothing of the removed paste-the-code flow is ever shown. */
function noPasteFlow(a) {
  const text = a.document.body.textContent.toLowerCase();
  assert.ok(!text.includes("authorization code"), "no 'authorization code'");
  assert.ok(!text.includes("paste"), "no paste box");
  assert.ok(!text.includes("submit code"), "no code field");
  assert.ok(!text.includes("sign in with anthropic"), "no OAuth button");
}

/** Opens a Radix select by keyboard and returns the option starting with `label`. */
async function choose(a, label) {
  const trigger = a.document.querySelector('[role="combobox"]');
  assert.ok(trigger, "a provider select");
  trigger.focus();
  await a.key(trigger, "Enter");
  const opt = [...a.document.querySelectorAll('[role="option"]')].find((o) => o.textContent.trim().startsWith(label));
  assert.ok(opt, `option ${label}`);
  return opt;
}
async function pick(a, label) {
  const opt = await choose(a, label);
  opt.focus();
  await a.key(opt, "Enter");
  await a.settle(20);
}

/** The wizard up to step 3 (page 20). */
async function wizardStep3(handlers = {}) {
  const a = await app({
    view: appView({ overall: "no_folders", summary: "No folders yet", folders: [] }),
    handlers: {
      "plugin:dialog|open": "C:\\Users\\test\\Documents",
      estimate_folder: { files: 10, capped: false, eta_s: 30, text: "About 10 files. This first pass should take a minute or two." },
      add_folder: ID.docs,
      get_provider_status: NO_PROVIDER_STATUS,
      ...handlers,
    },
  });
  await a.click(a.one("Choose a folder…"));
  await a.settle(20);
  await a.emit("state-changed", appView({ folders: [FOLDERS[0]] }));
  await a.click(a.one("Next"));
  assert.ok(a.byText("Get written answers (optional)", "h1").length === 1);
  return a;
}

test("wizard: both sign-in providers are offered with their labels; Antigravity stays key-only", async () => {
  const a = await wizardStep3();
  await choose(a, "Claude (sign in)");
  const opts = [...a.document.querySelectorAll('[role="option"]')];
  const codex = opts.find((o) => o.textContent.includes("Codex"));
  assert.ok(codex.textContent.startsWith("Codex (sign in)"));
  assert.notEqual(codex.getAttribute("aria-disabled"), "true", "Codex is selectable (Amendment 10.3)");
  assert.ok(!opts.some((o) => o.textContent.trim().startsWith("Claude Code")), "label is never the product name");
  const anti = opts.find((o) => o.textContent.includes("Antigravity"));
  assert.ok(anti && !anti.textContent.includes("sign in"), "Antigravity: API key only");
});

test("wizard: Claude Code already signed in → 'Using your Claude Code sign-in' and [Check and save] with no key", async () => {
  const a = await wizardStep3({
    cli_signin_status: ({ provider }) => (provider === "codex" ? CODEX_NOT_INSTALLED : CLAUDE_SIGNED_IN),
    save_provider: { status: { ...PROVIDER, provider: "claudecode", provider_name: "Claude (sign in)", key_saved: false, key_last4: "" }, validated: true },
  });
  await pick(a, "Claude (sign in)");
  const main = a.document.querySelector("main");
  assert.ok(main.textContent.includes("Using your Claude Code sign-in: test@example.com (Max)"), main.textContent);
  assert.equal(a.byText("Sign in").length, 0, "nothing to sign in to");
  assert.ok(a.byText("Check again").length >= 1);
  noPasteFlow(a);
  await noViolations(a, "Wizard: signed in");
  const save = a.one("Check and save");
  assert.equal(save.disabled, false);
  await a.click(save);
  await a.settle(20);
  const call = a.calls.find((c) => c.cmd === "save_provider");
  assert.equal(call?.args.provider, "claudecode");
  assert.equal(call?.args.key, null, "no key is sent for a sign-in");
  // The status was read without forcing a new check; no start call.
  assert.ok(a.calls.some((c) => c.cmd === "cli_signin_status" && c.args.provider === "claudecode"));
  assert.ok(!a.calls.some((c) => c.cmd === "cli_signin_start"));
});

test("wizard: not signed in → one [Sign in] that starts the tool's own sign-in; focus re-checks", async () => {
  let signedIn = false;
  const a = await wizardStep3({
    cli_signin_status: ({ provider, refresh }) =>
      provider === "codex" ? CODEX_NOT_INSTALLED : refresh && signedIn ? CLAUDE_SIGNED_IN : CLAUDE_SIGNED_OUT,
  });
  await pick(a, "Claude (sign in)");
  const main = a.document.querySelector("main");
  assert.ok(main.textContent.includes("Claude Code isn't signed in"));
  assert.equal(a.byText("Check and save")[0]?.disabled, true, "nothing to save before signing in");
  noPasteFlow(a);
  await noViolations(a, "Wizard: not signed in");
  await a.click(a.one("Sign in"));
  await a.settle(20);
  const start = a.calls.find((c) => c.cmd === "cli_signin_start");
  assert.equal(start?.args.provider, "claudecode");
  assert.ok(main.textContent.includes("Finish signing in in the Claude Code window that opened."));
  assert.ok(!a.calls.some((c) => c.cmd === "open_link"), "the app opens no address itself");
  noPasteFlow(a);
  // The user signs in in the browser and comes back: the window regains focus → checked again.
  signedIn = true;
  a.window.dispatchEvent(new a.window.Event("focus"));
  await a.settle(20);
  assert.ok(a.calls.some((c) => c.cmd === "cli_signin_status" && c.args.refresh === true));
  assert.ok(main.textContent.includes("Using your Claude Code sign-in: test@example.com (Max)"));
  assert.equal(a.byText("Check and save")[0]?.disabled, false);
});

test("wizard: Codex not installed → plain line and the install link (allow-listed page)", async () => {
  const a = await wizardStep3();
  await pick(a, "Codex (sign in)");
  const main = a.document.querySelector("main");
  assert.ok(main.textContent.includes("Codex isn't installed on this computer"));
  assert.equal(a.byText("Sign in").length, 0);
  await noViolations(a, "Wizard: Codex not installed");
  await a.click(a.one("Get Codex"));
  await a.settle(20);
  const link = a.calls.find((c) => c.cmd === "open_link");
  assert.equal(link?.args.url, "https://developers.openai.com/codex/cli");
});

test("Providers & Keys: the sign-in provider row shows the account; no key to replace; [Check again]", async () => {
  const a = await app({
    handlers: {
      get_provider_status: { ...PROVIDER, provider: "claudecode", provider_name: "Claude (sign in)", key_saved: false, key_last4: "" },
      cli_signin_status: ({ provider }) => (provider === "codex" ? CODEX_NOT_INSTALLED : CLAUDE_SIGNED_IN),
    },
  });
  await a.nav("Providers & Keys");
  await a.settle(20);
  const main = a.document.querySelector("main");
  assert.ok(main.textContent.includes("Using your Claude Code sign-in: test@example.com (Max)"));
  assert.equal(a.byText("Replace key").length, 0, "no key to replace");
  assert.equal(a.byText("Sign out").length, 0, "never signs the user out of Claude Code");
  // Codex isn't installed: no extra card for it.
  assert.equal(a.byText("Codex (sign in)", "h2").length, 0);
  noPasteFlow(a);
  await noViolations(a, "Providers (Claude sign in)");
});

test("Providers & Keys: with another provider, the Claude (sign in) card switches after a confirm", async () => {
  const a = await app({
    handlers: {
      cli_signin_status: ({ provider }) => (provider === "codex" ? CODEX_NOT_INSTALLED : CLAUDE_SIGNED_IN),
      save_provider: { status: { ...PROVIDER, provider: "claudecode", provider_name: "Claude (sign in)" }, validated: true },
    },
  });
  await a.nav("Providers & Keys");
  await a.settle(20);
  assert.ok(a.byText("Claude (sign in)", "h2").length === 1);
  await a.click(a.one("Use for answers…"));
  assert.equal(a.document.activeElement?.textContent.trim(), "Keep OpenAI", "focus on the safe choice");
  await noViolations(a, "Providers (switch confirm)");
  await a.click(a.one("Use Claude (sign in)"));
  await a.settle(20);
  const call = a.calls.find((c) => c.cmd === "save_provider");
  assert.equal(call?.args.provider, "claudecode");
  assert.equal(call?.args.key, null);
});

test("a pushed cli_signin_changed updates the card without any polling", async () => {
  const a = await app({
    handlers: {
      cli_signin_status: ({ provider }) => (provider === "codex" ? CODEX_NOT_INSTALLED : CLAUDE_SIGNED_OUT),
    },
  });
  await a.nav("Providers & Keys");
  await a.settle(20);
  const main = a.document.querySelector("main");
  assert.ok(main.textContent.includes("Claude Code isn't signed in"));
  await a.emit("service-push", { kind: "cli_signin_changed", status: CLAUDE_SIGNED_IN });
  await a.settle(20);
  assert.ok(main.textContent.includes("Using your Claude Code sign-in: test@example.com (Max)"));
});

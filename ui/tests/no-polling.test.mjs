// Lint test (task 7, P1.5 "progress is pushed, never polled"):
//  1. the whole UI source passes the no-polling rules, and
//  2. the rules really fire on polling code (fixtures), so the check can't silently rot.
import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { ESLint } from "eslint";

const uiRoot = fileURLToPath(new URL("..", import.meta.url));
const eslint = new ESLint({ cwd: uiRoot });

const POLLING_SAMPLES = {
  "setInterval status poll": `import { commands } from "@/bindings";
export function poll() { setInterval(() => { void commands.getState(); }, 1000); }`,
  "window.setInterval": `export function poll(f: () => void) { window.setInterval(f, 500); }`,
  "setTimeout re-arm loop": `export function loop(f: () => void) { const t = () => { f(); setTimeout(t, 1000); }; t(); }`,
  "TanStack refetchInterval": `import { useQuery } from "@tanstack/react-query";
export function useStatus() { return useQuery({ queryKey: ["s"], queryFn: () => 1, refetchInterval: 2000 }); }`,
};

for (const [name, code] of Object.entries(POLLING_SAMPLES)) {
  test(`no-polling rule rejects: ${name}`, async () => {
    const [res] = await eslint.lintText(code, { filePath: `${uiRoot}/src/__polling_sample__.tsx` });
    const hits = res.messages.filter((m) =>
      ["no-restricted-globals", "no-restricted-properties", "no-restricted-syntax"].includes(m.ruleId ?? ""),
    );
    assert.ok(hits.length > 0, `expected a no-polling error, got: ${JSON.stringify(res.messages)}`);
  });
}

test("UI source has no polling and no lint errors", async () => {
  const results = await eslint.lintFiles(["src/**/*.{ts,tsx}"]);
  const errors = results.flatMap((r) =>
    r.messages.filter((m) => m.severity === 2).map((m) => `${r.filePath}:${m.line} ${m.ruleId}: ${m.message}`),
  );
  assert.deepEqual(errors, []);
});

// The TS error-code list (src/lib/errors.ts) must mirror sr-common's `ErrorCode` (#1–#41), and the
// plain text the UI keeps for raw codes must be sr-common's text, verbatim.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const read = (rel) => readFileSync(new URL(rel, import.meta.url), "utf8");
const ts = read("../src/lib/errors.ts");
const rustMod = read("../../crates/common/src/error/mod.rs");
const rustMsg = read("../../crates/common/src/error/messages.rs");

test("ERROR_CODES mirrors sr_common::error::ErrorCode, in order", () => {
  const body = rustMod.match(/error_codes!\s*\{([\s\S]*?)\n\}/)?.[1] ?? rustMod;
  const rust = [...body.matchAll(/^\s*([A-Z][A-Za-z]+)\s*=>\s*(No|Yes|Auto|NotApplicable)\s*,/gm)].map((m) => m[1]);
  const list = ts.match(/ERROR_CODES = \[([\s\S]*?)\] as const/)?.[1] ?? "";
  const mine = [...list.matchAll(/"([A-Za-z]+)"/g)].map((m) => m[1]);
  assert.ok(rust.length >= 40, `parsed ${rust.length} Rust codes`);
  assert.deepEqual(mine, rust);
});

test("#40 ConnectionNotTrusted: the UI's text is sr-common's, verbatim", () => {
  const block = rustMsg.match(/C::ConnectionNotTrusted => \(([\s\S]*?)A::None/)?.[1] ?? "";
  const strings = [...block.matchAll(/"((?:[^"\\]|\\.)*)"/g)].map((m) => m[1].replace(/\\'/g, "'"));
  assert.equal(strings.length, 2, "title + body found in messages.rs");
  assert.ok(ts.includes(`title: "${strings[0]}"`), `title "${strings[0]}"`);
  assert.ok(ts.includes(`body: "${strings[1]}"`), `body "${strings[1]}"`);
});
